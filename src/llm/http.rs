use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, ClientBuilder, StatusCode};
use serde_json::Value;

use super::{ErrorKind, LlmConfig, LlmError, LlmResponse, Protocol, ToolSpec, build_request, parse_response};
use crate::protocol::composer::Message;

const BASE_DELAY: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Call the provider and retry transient failures as part of the same
/// logical model call. See [build_request] for the write-ahead record.
pub async fn call(
    config: &LlmConfig,
    messages: &[Message],
    tools: &[ToolSpec],
    thinking: bool,
) -> Result<LlmResponse, LlmError> {
    let body = build_request(config, messages, tools, thinking)?;
    let client = client_builder().build().map_err(transport_error)?;
    call_with_client(config, &body, &client, BASE_DELAY).await
}

/// Send a body already built with [super::build_request] (and recorded), with
/// the same retries as [call]. The valet uses this so the recorded request is
/// exactly the one sent.
pub async fn call_body(config: &LlmConfig, body: &str) -> Result<LlmResponse, LlmError> {
    let client = client_builder().build().map_err(transport_error)?;
    call_with_client(config, body, &client, BASE_DELAY).await
}

pub(super) fn client_builder() -> ClientBuilder {
    Client::builder().redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never())
}

pub(super) async fn call_with_client(
    config: &LlmConfig,
    body: &str,
    client: &Client,
    base_delay: Duration,
) -> Result<LlmResponse, LlmError> {
    let auth = auth_header(config).await?;
    let (path, header) = match config.protocol {
        Protocol::Anthropic => ("/v1/messages", "x-api-key"),
        Protocol::Openai => ("/chat/completions", AUTHORIZATION.as_str()),
    };
    let url = format!("{}{path}", config.base_url.trim_end_matches('/'));
    let mut request = client
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .timeout(Duration::from_secs(config.timeout_s))
        .body(body.to_owned());
    if let Some(auth) = auth {
        request = request.header(header, auth);
    }
    if config.protocol == Protocol::Anthropic {
        request = request.header("anthropic-version", "2023-06-01");
    }
    let request = request.build().map_err(transport_error)?;

    let mut attempts = 1;
    let mut backoff = base_delay.min(MAX_BACKOFF);
    loop {
        // The request has an in-memory body, so every attempt sends the same bytes.
        let result =
            attempt(client, request.try_clone().expect("buffered request is cloneable"), config.protocol).await;
        let (mut error, retry_after) = match result {
            Ok(response) => return Ok(response),
            Err(failure) => failure,
        };
        error.attempts = attempts;
        if !error.retryable() || attempts > u64::from(config.max_retries) {
            return Err(error);
        }

        let jittered = backoff.mul_f64(rand::random_range(0.5..=1.0));
        tokio::time::sleep(jittered.max(retry_after)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
        attempts += 1;
    }
}

async fn auth_header(config: &LlmConfig) -> Result<Option<HeaderValue>, LlmError> {
    let Some(path) = &config.api_key_file else {
        return Ok(None);
    };
    let key = tokio::fs::read_to_string(path).await.map_err(|error| {
        LlmError::new(ErrorKind::Auth, format!("cannot read API key file {}: {error}", path.display()))
    })?;
    let key = key.trim_end_matches(['\r', '\n']);
    if key.is_empty() {
        return Err(LlmError::new(ErrorKind::Auth, "API key file is empty"));
    }
    let mut header = match config.protocol {
        Protocol::Anthropic => HeaderValue::from_str(key),
        Protocol::Openai => HeaderValue::from_str(&format!("Bearer {key}")),
    }
    .map_err(|_| LlmError::new(ErrorKind::Auth, "API key contains invalid header characters"))?;
    header.set_sensitive(true);
    Ok(Some(header))
}

async fn attempt(
    client: &Client,
    request: reqwest::Request,
    protocol: Protocol,
) -> Result<LlmResponse, (LlmError, Duration)> {
    let response = client.execute(request).await.map_err(|error| (transport_error(error), Duration::ZERO))?;
    let status = response.status();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|seconds| seconds.trim().parse().ok())
        .map(Duration::from_secs)
        .unwrap_or_default();

    let body = response.bytes().await;
    let result = if status.is_success() {
        body.map_err(transport_error).and_then(|body| {
            let text = std::str::from_utf8(&body)
                .map_err(|error| LlmError::new(ErrorKind::Other, format!("provider response is not UTF-8: {error}")))?;
            parse_response(protocol, text)
        })
    } else {
        let text = body.as_deref().ok().map(String::from_utf8_lossy).unwrap_or_default();
        Err(status_error(status, &text))
    };
    result.map_err(|mut error| {
        error.status = Some(status.as_u16());
        (error, retry_after)
    })
}

fn transport_error(error: reqwest::Error) -> LlmError {
    let kind = if error.is_timeout() {
        ErrorKind::Timeout
    } else if error.is_connect() || error.is_body() || error.is_request() {
        ErrorKind::Connection
    } else if error.is_builder() {
        ErrorKind::BadRequest
    } else {
        ErrorKind::Other
    };
    LlmError::new(kind, error.without_url().to_string())
}

fn status_error(status: StatusCode, body: &str) -> LlmError {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let provider_message = json.pointer("/error/message").and_then(Value::as_str).unwrap_or(body.trim());
    let kind = match status {
        StatusCode::TOO_MANY_REQUESTS => ErrorKind::RateLimit,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ErrorKind::Auth,
        StatusCode::BAD_REQUEST if context_too_long(&json, provider_message) => ErrorKind::ContextTooLong,
        _ if status.is_server_error() => ErrorKind::Server,
        _ if status.is_client_error() => ErrorKind::BadRequest,
        _ => ErrorKind::Other,
    };
    let message = if provider_message.is_empty() { status.to_string() } else { provider_message.to_owned() };
    LlmError::new(kind, message)
}

fn context_too_long(body: &Value, message: &str) -> bool {
    if ["/error/code", "/error/type"].iter().any(|path| {
        matches!(body.pointer(path).and_then(Value::as_str), Some("context_length_exceeded" | "context_too_long"))
    }) {
        return true;
    }

    let message = message.to_ascii_lowercase();
    [
        "prompt is too long",
        "prompt too long",
        "input is too long",
        "request too large for model context",
        "maximum context length",
        "exceed context limit",
        "exceeds the context window",
        "exceeds the model's context window",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

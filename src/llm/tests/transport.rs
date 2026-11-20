use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::{ANTHROPIC_RESPONSE, OPENAI_RESPONSE, config};
use crate::llm::http::{call_with_client, client_builder};
use crate::llm::{ErrorKind, LlmConfig, LlmError, LlmResponse, Protocol, build_request, call};
use crate::protocol::composer::{Message, Role};

struct Reply {
    status: u16,
    body: &'static str,
    retry_after: Option<u64>,
    stall_body: bool,
}

impl Reply {
    fn new(status: u16, body: &'static str) -> Self {
        Self { status, body, retry_after: None, stall_body: false }
    }
}

#[derive(Debug)]
struct Received {
    at: Instant,
    headers: String,
    body: String,
}

async fn serve(replies: Vec<Reply>) -> (String, JoinHandle<Vec<Received>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut headers = String::new();
            loop {
                let offset = headers.len();
                assert_ne!(reader.read_line(&mut headers).await.unwrap(), 0);
                if &headers[offset..] == "\r\n" {
                    break;
                }
            }
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse().unwrap())
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            requests.push(Received { at: Instant::now(), headers, body: String::from_utf8(body).unwrap() });

            let retry_after =
                reply.retry_after.map(|seconds| format!("Retry-After: {seconds}\r\n")).unwrap_or_default();
            let headers = format!(
                "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{retry_after}\r\n",
                reply.status,
                reply.body.len(),
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            if reply.stall_body {
                // Leave the body unfinished until the client times out and closes.
                let mut unexpected = Vec::new();
                stream.read_to_end(&mut unexpected).await.unwrap();
                assert!(unexpected.is_empty());
            } else {
                stream.write_all(reply.body.as_bytes()).await.unwrap();
            }
        }
        requests
    });
    (base_url, server)
}

async fn test_call(config: &LlmConfig, body: &str) -> Result<LlmResponse, LlmError> {
    let client = client_builder().no_proxy().build().unwrap();
    call_with_client(config, body, &client, Duration::from_millis(1)).await
}

#[tokio::test]
async fn retries_overload_and_sends_recorded_body_and_anthropic_headers() {
    let (base_url, server) = serve(vec![Reply::new(529, "overloaded"), Reply::new(200, ANTHROPIC_RESPONSE)]).await;
    let mut config = config(Protocol::Anthropic);
    config.base_url = format!("{base_url}///");
    config.max_retries = 1;
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("key");
    std::fs::write(&key_file, "test-key\r\n").unwrap();
    config.api_key_file = Some(key_file);
    let messages = [Message::text(Role::User, "Find a term.")];
    let body = build_request(&config, &messages, &[], true).unwrap();

    let response = test_call(&config, &body).await.unwrap();
    assert_eq!(response.raw, ANTHROPIC_RESPONSE);
    let received = server.await.unwrap();
    assert_eq!(received.len(), 2);
    for request in received {
        assert_eq!(request.body, body);
        assert!(request.headers.starts_with("POST /v1/messages HTTP/1.1\r\n"));
        assert!(request.headers.contains("x-api-key: test-key\r\n"));
        assert!(request.headers.contains("anthropic-version: 2023-06-01\r\n"));
    }
}

#[tokio::test]
async fn honors_retry_after_seconds_and_sends_openai_headers() {
    let (base_url, server) = serve(vec![
        Reply { retry_after: Some(1), ..Reply::new(429, r#"{"error":{"message":"slow down"}}"#) },
        Reply::new(200, OPENAI_RESPONSE),
    ])
    .await;
    let mut config = config(Protocol::Openai);
    config.base_url = format!("{base_url}/v1/");
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("key");
    std::fs::write(&key_file, "another-key\n").unwrap();
    config.api_key_file = Some(key_file);
    let body = build_request(&config, &[Message::text(Role::User, "Find a term.")], &[], false).unwrap();

    assert_eq!(test_call(&config, &body).await.unwrap().raw, OPENAI_RESPONSE);
    let received = server.await.unwrap();
    assert_eq!(received.len(), 2);
    assert!(received[1].at.duration_since(received[0].at) >= Duration::from_secs(1));
    for request in received {
        assert_eq!(request.body, body);
        assert!(request.headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(request.headers.contains("authorization: Bearer another-key\r\n"));
    }
}

#[tokio::test]
async fn rejects_nonretryable_statuses_and_context_errors() {
    let cases = [
        (Protocol::Openai, 400, r#"{"error":{"message":"invalid model"}}"#, ErrorKind::BadRequest),
        (Protocol::Openai, 401, r#"{"error":{"message":"invalid key"}}"#, ErrorKind::Auth),
        (Protocol::Anthropic, 403, r#"{"error":{"message":"forbidden"}}"#, ErrorKind::Auth),
        (
            Protocol::Openai,
            400,
            r#"{"error":{"code":"context_length_exceeded","message":"reduce your messages"}}"#,
            ErrorKind::ContextTooLong,
        ),
        (
            Protocol::Anthropic,
            400,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 200000 tokens > 100000 maximum"}}"#,
            ErrorKind::ContextTooLong,
        ),
    ];
    for (protocol, status, response, kind) in cases {
        let (base_url, server) = serve(vec![Reply::new(status, response)]).await;
        let mut config = config(protocol);
        config.base_url = base_url;
        let body = build_request(&config, &[], &[], false).unwrap();

        let error = test_call(&config, &body).await.unwrap_err();
        assert_eq!(error.kind, kind);
        assert_eq!(error.status, Some(status));
        assert_eq!(error.attempts, 1);
        assert!(!error.retryable());
        let received = server.await.unwrap();
        assert_eq!(received.len(), 1);
        assert!(!received[0].headers.contains("authorization:"));
        assert!(!received[0].headers.contains("x-api-key:"));
    }
}

#[tokio::test]
async fn returns_last_status_when_retries_are_exhausted() {
    let (base_url, server) = serve(vec![
        Reply::new(500, "server error"),
        Reply::new(503, "temporarily unavailable"),
        Reply::new(429, r#"{"error":{"message":"rate limited"}}"#),
    ])
    .await;
    let mut config = config(Protocol::Openai);
    config.base_url = base_url;
    config.max_retries = 2;
    let body = build_request(&config, &[], &[], false).unwrap();

    let error = test_call(&config, &body).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::RateLimit);
    assert_eq!(error.status, Some(429));
    assert_eq!(error.attempts, 3);
    assert_eq!(error.message, "rate limited");
    assert!(error.retryable());
    assert_eq!(server.await.unwrap().len(), 3);
}

#[tokio::test]
async fn times_out_while_reading_body_and_retries() {
    let (base_url, server) = serve(vec![
        Reply { stall_body: true, ..Reply::new(200, OPENAI_RESPONSE) },
        Reply { stall_body: true, ..Reply::new(200, OPENAI_RESPONSE) },
    ])
    .await;
    let mut config = config(Protocol::Openai);
    config.base_url = base_url;
    config.timeout_s = 1;
    config.max_retries = 1;
    let body = build_request(&config, &[], &[], false).unwrap();

    let error = test_call(&config, &body).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.status, Some(200));
    assert_eq!(error.attempts, 2);
    assert!(error.retryable());
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn connection_failure_is_retryable() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let mut config = config(Protocol::Openai);
    config.base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    config.max_retries = 1;
    let body = build_request(&config, &[], &[], false).unwrap();

    let error = test_call(&config, &body).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Connection);
    assert_eq!(error.status, None);
    assert_eq!(error.attempts, 2);
    assert!(error.retryable());
}

#[tokio::test]
async fn configured_missing_key_fails_before_sending() {
    let mut config = config(Protocol::Anthropic);
    let dir = tempfile::tempdir().unwrap();
    config.api_key_file = Some(dir.path().join("missing"));

    let error = call(&config, &[], &[], false).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Auth);
    assert_eq!(error.status, None);
    assert_eq!(error.attempts, 0);
    assert!(!error.retryable());
}

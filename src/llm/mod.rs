//! Provider translation and calls (spec sections 2, 6.2 and 7).
//!
//! The valet builds and records the request before calling the provider.
//! Retries of that same request belong to the same recorded action.

mod anthropic;
mod http;
mod openai;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use crate::protocol::composer::{ContentBlock, Message};

pub use http::{call, call_body};

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    pub protocol: Protocol,
    pub model_id: String,
    pub base_url: String,
    pub api_key_file: Option<PathBuf>,
    pub thinking: ThinkingConfig,
    pub context_window: u64,
    pub max_output: u64,
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_timeout_s() -> u64 {
    300
}

fn default_max_retries() -> u32 {
    3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Anthropic,
    Openai,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThinkingConfig {
    pub native: ThinkingNative,
    pub on: bool,
    pub returned: ThinkingReturned,
    pub replay: ThinkingReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingNative {
    Never,
    Optional,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingReturned {
    Full,
    Summary,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingReplay {
    Required,
    Optional,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug)]
pub struct LlmResponse {
    pub content: Vec<ContentBlock>,
    /// Provider-native stop reason, e.g. `tool_use` or `tool_calls`.
    pub stop_reason: Option<String>,
    /// Some OpenAI-compatible endpoints omit usage.
    pub usage: Option<Usage>,
    /// Complete provider JSON body as received.
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    RateLimit,
    Server,
    Timeout,
    Connection,
    Auth,
    BadRequest,
    ContextTooLong,
    Other,
}

#[derive(Debug, thiserror::Error, Serialize)]
#[error("{kind:?}: {message}")]
pub struct LlmError {
    pub kind: ErrorKind,
    pub status: Option<u16>,
    pub message: String,
    /// Number of HTTP attempts, including the initial call. Zero for local errors.
    pub attempts: u64,
}

impl LlmError {
    pub fn retryable(&self) -> bool {
        matches!(self.kind, ErrorKind::RateLimit | ErrorKind::Server | ErrorKind::Timeout | ErrorKind::Connection)
    }

    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, status: None, message: message.into(), attempts: 0 }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, message)
    }
}

/// Build the exact JSON body sent by [call], without reading credentials or
/// making an HTTP request. Record this before starting the call. Keep it as
/// a string if the record would otherwise parse and reserialize JSON values.
///
/// `thinking` is the resolved flag (a compose override or the profile
/// default). The composer selects thinking blocks to replay.
pub fn build_request(
    config: &LlmConfig,
    messages: &[Message],
    tools: &[ToolSpec],
    thinking: bool,
) -> Result<String, LlmError> {
    match config.protocol {
        Protocol::Anthropic => anthropic::build_request(config, messages, tools, thinking),
        Protocol::Openai => openai::build_request(config, messages, tools, thinking),
    }
}

/// Parse a successful provider response, retaining the original JSON and the
/// exact spelling of thinking fields. HTTP failures are classified by [call].
pub fn parse_response(protocol: Protocol, body: &str) -> Result<LlmResponse, LlmError> {
    match protocol {
        Protocol::Anthropic => anthropic::parse_response(body),
        Protocol::Openai => openai::parse_response(body),
    }
    .map_err(|error| LlmError::new(ErrorKind::Other, format!("invalid provider response: {error}")))
}

fn encode_request(request: impl Serialize) -> Result<String, LlmError> {
    serde_json::to_string(&request).map_err(|error| LlmError::bad_request(error.to_string()))
}

fn required<'a, T: ?Sized>(value: Option<&'a T>, field: &str) -> Result<&'a T, LlmError> {
    value.ok_or_else(|| LlmError::bad_request(format!("content block is missing {field}")))
}

fn thinking_block(raw: &RawValue) -> serde_json::Result<ContentBlock> {
    let mut block = ContentBlock::thinking(serde_json::from_str(raw.get())?);
    block.raw_json = Some(raw.get().to_owned());
    Ok(block)
}

fn replay_json(block: &ContentBlock, value: &Value) -> Result<Box<RawValue>, LlmError> {
    if let Some(original) = &block.raw_json {
        let decoded: Value = serde_json::from_str(original)
            .map_err(|error| LlmError::bad_request(format!("invalid thinking raw_json: {error}")))?;
        if &decoded != value {
            return Err(LlmError::bad_request("thinking raw_json does not match raw"));
        }
        return RawValue::from_string(original.clone()).map_err(|error| LlmError::bad_request(error.to_string()));
    }
    serde_json::value::to_raw_value(value).map_err(|error| LlmError::bad_request(error.to_string()))
}

#[cfg(test)]
mod tests;

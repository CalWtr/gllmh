//! Shapes of `action.input` / `action.output` per action kind.
//! See docs/action-records.md: the valet writes these, a composer reads them.

use serde::{Deserialize, Serialize};

use super::broker::SendOutput;
use super::composer::{ContentBlock, Message};
use crate::frame::ErrorBody;
use crate::types::{Action, ActionState};

/// `llm_call.input`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCallInput {
    pub llm_profile: String,
    pub messages: Vec<Message>,
    pub tools: Vec<String>,
    pub thinking: bool,
    /// The provider request body as sent.
    #[serde(default)]
    pub request: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// `llm_call.output` when it succeeded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCallOutput {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
    #[serde(default)]
    pub usage: Usage,
    /// The provider response body as received.
    #[serde(default)]
    pub response: serde_json::Value,
}

/// `llm_call.output` when it failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCallFailure {
    pub error: LlmCallError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCallError {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub message: String,
    pub retryable: bool,
    pub attempts: u32,
}

/// What the model sees for a tool_call or a send from a send tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

/// `tool_call.input`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallInput {
    pub tool: String,
    pub arguments: serde_json::Value,
}

/// `tool_call.output`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallOutput {
    pub tool_result: ToolResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

/// `send.output` when it failed. On success the output is a bare `SendOutput`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendFailure {
    pub error: ErrorBody,
}

/// What the model sees for a send made by a send tool, derived from its row.
/// `None` while the send is unfinished.
pub fn send_tool_result(action: &Action) -> Option<ToolResult> {
    let output = action.output.as_ref();
    match action.state {
        ActionState::Succeeded => {
            let reply: SendOutput = serde_json::from_value(output?.clone()).ok()?;
            let kind = action.input.get("kind").and_then(|v| v.as_str()).unwrap_or("interaction");
            let to = action.input.get("to").and_then(|v| v.as_str()).unwrap_or("?");
            Some(ToolResult { text: format!("sent {kind} {} to {to}", reply.id), is_error: false })
        }
        ActionState::Failed => {
            let text = output
                .and_then(|o| serde_json::from_value::<SendFailure>(o.clone()).ok())
                .map(|f| format!("{}: {}", f.error.code, f.error.message))
                .unwrap_or_else(|| "failed".into());
            Some(ToolResult { text, is_error: true })
        }
        ActionState::Denied | ActionState::Interrupted => {
            Some(ToolResult { text: action.state.as_str().into(), is_error: true })
        }
        ActionState::Requested | ActionState::Running => None,
    }
}

/// Pull the `tool_result` out of any action output, if it has one.
pub fn tool_result_of(output: &serde_json::Value) -> Option<ToolResult> {
    serde_json::from_value(output.get("tool_result")?.clone()).ok()
}

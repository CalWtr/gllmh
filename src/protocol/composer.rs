//! Section 6.2: valet ⇔ composer, on the composer's stdin and stdout.
//! One `init`, then only `compose`, possibly several at once.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::broker::Edges;
use crate::types::{InteractionId, Sender};

pub const INIT: &str = "init";
pub const COMPOSE: &str = "compose";

pub mod codes {
    /// Not parseable, or names a profile or tool the agent does not have.
    pub const INVALID_REPLY: &str = "invalid_reply";
    /// No reply within compose_timeout_s.
    pub const TIMEOUT: &str = "timeout";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitInput {
    pub user: String,
    /// Resolved, including `builtin` and `custom`.
    pub composer_profile: serde_json::Value,
    /// Without api_key_file.
    pub llm_profiles: Vec<serde_json::Value>,
    /// Names only; the valet owns the schemas, send tools included.
    pub tools: Vec<String>,
    /// Read-only; the composer reads the v1_ views only.
    pub store: PathBuf,
    /// EXTENSION: hello's edges, so a composer can tell the model whom it may
    /// message and direct.
    #[serde(default)]
    pub edges: BTreeMap<Sender, Edges>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitOutput {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComposeInput {
    pub turn: InteractionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    Done,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkWrite {
    pub target: Ulid,
    pub key: String,
    /// Null removes the mark.
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComposeOutput {
    /// Which of the agent's llm_profiles answers this call.
    pub llm_profile: String,
    /// The whole model input; the valet translates it to that profile's protocol.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Which tools to offer; the valet attaches their schemas.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Overrides llm_profile.thinking.on for this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<MarkWrite>,
    /// End the turn instead of calling the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Stop>,
}

/// GLLMH's own message format, the same for every provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    ToolResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockType {
    Text,
    ToolUse,
    ToolResult,
    Thinking,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: BlockType,
    /// text, and a tool_result's content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// tool_use and tool_result: pairs a result with its request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// tool_use: the tool's name as offered to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// tool_use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    /// thinking: the provider's block, replayed byte-exact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
    /// EXTENSION: tool_result only. True when the tool failed, was denied, or
    /// the broker refused the send; providers mark the result as an error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: Some(text.into()), ..Self::empty(BlockType::Text) }
    }

    pub fn tool_use(call_id: impl Into<String>, tool: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            call_id: Some(call_id.into()),
            tool: Some(tool.into()),
            arguments: Some(arguments),
            ..Self::empty(BlockType::ToolUse)
        }
    }

    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>, is_error: bool) -> Self {
        Self { call_id: Some(call_id.into()), text: Some(text.into()), is_error, ..Self::empty(BlockType::ToolResult) }
    }

    pub fn thinking(raw: serde_json::Value) -> Self {
        Self { raw: Some(raw), ..Self::empty(BlockType::Thinking) }
    }

    fn empty(kind: BlockType) -> Self {
        Self { kind, text: None, call_id: None, tool: None, arguments: None, raw: None, is_error: false }
    }
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self { role, content: vec![ContentBlock::text(text)] }
    }
}

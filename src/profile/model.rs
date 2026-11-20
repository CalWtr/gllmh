//! Configuration and resolved profile data from section 7.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::{DEFAULT_BROKER_DB, DEFAULT_USER_DATA_STORE};
use crate::types::Hazardness;

/// A workflow's participants, permissions and broker settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProfile {
    pub name: String,
    pub users: Vec<String>,
    /// Sender spellings are validated together with their recipients.
    #[serde(default)]
    pub allow_messages: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub allow_directives: BTreeMap<String, Vec<String>>,
    pub restart: Restart,
    #[serde(default = "shutdown_grace")]
    pub child_process_shutdown_grace_s: u64,
    #[serde(default = "max_hops")]
    pub max_hops: u32,
    #[serde(default = "interaction_ttl")]
    pub interaction_ttl_s: u64,
    #[serde(default = "max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "broker_db_path")]
    pub broker_db_path: PathBuf,
}

/// Process restart policy for unit generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    OnFailure,
    Never,
}

/// One participant, before resolving its profile references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserProfile {
    pub name: String,
    pub system_user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default = "user_data_store")]
    pub user_data_store: PathBuf,
    #[serde(default)]
    pub services: BTreeMap<String, Service>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composer_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_profiles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_profiles: Option<NameSelection>,
}

/// A selection of named tools, also used for the tools exposed by an MCP profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NameSelection {
    Names(Vec<String>),
    All(Wildcard),
}

/// The only string accepted in place of a tool name list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wildcard {
    #[serde(rename = "*")]
    All,
}

/// A service belonging to a user's valet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Service {
    pub command: String,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub restart: Restart,
    pub required: bool,
    #[serde(default, deserialize_with = "present_json", skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Provider settings for calls on behalf of an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmProfile {
    pub name: String,
    pub protocol: LlmProtocol,
    pub model_id: String,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<PathBuf>,
    pub thinking: Thinking,
    pub context_window: u64,
    pub max_output: u64,
    pub tuning: Vec<PathBuf>,
    #[serde(default = "llm_timeout")]
    pub timeout_s: u64,
    #[serde(default = "max_retries")]
    pub max_retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProtocol {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
}

/// How a provider generates and accepts thinking blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thinking {
    pub native: NativeThinking,
    pub on: bool,
    pub returned: ReturnedThinking,
    pub replay: ThinkingReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeThinking {
    Never,
    Optional,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnedThinking {
    Full,
    Summary,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingReplay {
    Required,
    Optional,
    Never,
}

/// How the valet starts a composer and limits its turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComposerProfile {
    pub name: String,
    pub engine: ComposerEngine,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_composer_bin: Option<PathBuf>,
    pub work_mode: WorkMode,
    #[serde(default = "max_steps")]
    pub max_steps_per_turn: u32,
    #[serde(default = "max_parallel_turns")]
    pub max_parallel_turns: u32,
    #[serde(default = "compose_timeout")]
    pub compose_timeout_s: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin: Option<BuiltinComposer>,
    #[serde(default, deserialize_with = "present_json", skip_serializing_if = "Option::is_none")]
    pub custom: Option<serde_json::Value>,
}

impl ComposerProfile {
    /// Effective context settings for the builtin engine.
    pub fn builtin_settings(&self) -> Option<BuiltinComposer> {
        (self.engine == ComposerEngine::Builtin).then(|| self.builtin.unwrap_or_default())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerEngine {
    Builtin,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    Inst,
    Reason,
    Act,
    React,
}

/// Context selection and budget for a builtin composer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BuiltinComposer {
    #[serde(default)]
    pub mode: BuiltinMode,
    #[serde(default = "budget_fraction")]
    pub budget_fraction: f64,
}

impl Default for BuiltinComposer {
    fn default() -> Self {
        Self { mode: BuiltinMode::default(), budget_fraction: budget_fraction() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinMode {
    #[default]
    Message,
    Directive,
}

/// One builtin or executable tool, or a selection of tools from one MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProfile {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin: Option<BuiltinTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_bin: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_schema: Option<PathBuf>,
    pub hazardness: Hazardness,
    #[serde(default = "tool_timeout")]
    pub timeout_s: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinTool {
    SendMessage,
    SendDirective,
}

/// An MCP server locator and the tools it exposes to an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTool {
    /// Socket path, URL or stdio command.
    pub server: String,
    pub tools: NameSelection,
}

/// The profile carried in section 6.1's hello response. The valet can deserialize
/// it without filesystem access to the config directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedUser {
    pub name: String,
    pub system_user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// As configured, including a leading `~` for the system user's home if present.
    pub user_data_store: PathBuf,
    pub services: BTreeMap<String, Service>,
    /// Its presence makes this user an agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composer_profile: Option<ComposerProfile>,
    /// In configured order. The builtin composer selects the first entry.
    pub llm_profiles: Vec<LlmProfile>,
    /// In configured order for a list, or sorted by profile name for `"*"`.
    pub tool_profiles: Vec<ToolProfile>,
}

// A present JSON value can be null. Only an omitted field is None.
fn present_json<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

fn shutdown_grace() -> u64 {
    10
}

fn max_hops() -> u32 {
    16
}

fn interaction_ttl() -> u64 {
    86_400
}

fn max_body_bytes() -> usize {
    1_048_576
}

fn broker_db_path() -> PathBuf {
    DEFAULT_BROKER_DB.into()
}

fn user_data_store() -> PathBuf {
    DEFAULT_USER_DATA_STORE.into()
}

fn llm_timeout() -> u64 {
    300
}

fn max_retries() -> u32 {
    3
}

fn max_steps() -> u32 {
    20
}

fn max_parallel_turns() -> u32 {
    1
}

fn compose_timeout() -> u64 {
    60
}

fn budget_fraction() -> f64 {
    0.6
}

fn tool_timeout() -> u64 {
    120
}

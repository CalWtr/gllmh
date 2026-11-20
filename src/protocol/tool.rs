//! Section 6.3 `run`: the tool contract. Not a connection: the valet starts
//! `tool_bin` as a child process per tool_call. Arguments go to its stdin as
//! JSON, its stdout becomes the tool_result's text.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Env var carrying the current turn's id, for a tool to hand to a service as caused_by.
pub const ENV_TURN: &str = "GLLMH_TURN";
/// Env var a service reads for its socket path.
pub const ENV_SERVICE_SOCKET: &str = "GLLMH_SERVICE_SOCKET";
/// Env var a service reads for its config file path.
pub const ENV_SERVICE_CONFIG: &str = "GLLMH_SERVICE_CONFIG";

pub mod codes {
    /// Killed after timeout_s.
    pub const TIMEOUT: &str = "timeout";
    /// The system user cannot run tool_bin.
    pub const NOT_EXECUTABLE: &str = "not_executable";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInput {
    /// Written to the tool's stdin.
    pub arguments: serde_json::Value,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub timeout_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

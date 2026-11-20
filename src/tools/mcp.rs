//! Discover and run the MCP tools behind section 7's tool profiles.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::llm::ToolSpec;
use crate::mcp::ToolInfo;
use crate::profile::{McpTool, NameSelection, ToolProfile};
use crate::protocol::tool::{RunInput, RunOutput};

use super::{RunError, run::run_with_args};

#[derive(Debug, thiserror::Error)]
enum ListError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("MCP list exited with code {code}: {stderr}")]
    Exit { code: i32, stderr: String },
    #[error("invalid MCP list output: {0}")]
    Json(#[from] serde_json::Error),
}

pub(crate) struct DiscoveredTool {
    pub profile: String,
    pub original_name: String,
    pub spec: ToolSpec,
}

/// Cache the selected tools in profile and server order. EXTENSION: skip names
/// outside the provider's alphabet or length limit and aliases with ambiguous
/// routing, including any alias that equals a configured profile name.
pub(crate) async fn discover(profiles: &[ToolProfile], cwd: &Path) -> Vec<DiscoveredTool> {
    let mut discovered = Vec::new();
    let mut listed = BTreeSet::new();
    for profile in profiles {
        let Some(mcp) = &profile.mcp else { continue };
        if !listed.insert(&profile.name) {
            continue;
        }
        let tools = match list(mcp, cwd, profile.timeout_s).await {
            Ok(tools) => tools,
            Err(e) => {
                tracing::error!(profile = %profile.name, "could not list MCP tools: {e:#}");
                continue;
            }
        };
        for tool in tools {
            if let NameSelection::Names(names) = &mcp.tools
                && !names.contains(&tool.name)
            {
                continue;
            }
            let name = format!("{}__{}", profile.name, tool.name);
            if tool.name.is_empty()
                || name.len() > 64
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                tracing::warn!(profile = %profile.name, tool = %tool.name, "skipping invalid MCP tool name");
                continue;
            }
            discovered.push(DiscoveredTool {
                profile: profile.name.clone(),
                original_name: tool.name,
                spec: ToolSpec {
                    name,
                    description: tool.description.unwrap_or_default(),
                    input_schema: tool.input_schema,
                },
            });
        }
    }

    let mut counts = HashMap::new();
    for name in profiles.iter().map(|p| &p.name).chain(discovered.iter().map(|t| &t.spec.name)) {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    discovered.retain(|tool| {
        let collides = counts[&tool.spec.name] > 1;
        if collides {
            tracing::warn!(profile = %tool.profile, tool = %tool.spec.name, "skipping ambiguous MCP tool name");
        }
        !collides
    });
    discovered
}

async fn list(mcp: &McpTool, cwd: &Path, timeout_s: u64) -> Result<Vec<ToolInfo>, ListError> {
    let input = RunInput { arguments: Value::Null, cwd: cwd.into(), env: Default::default(), timeout_s };
    let output = run_client("list", &mcp.server, input).await?;
    if output.exit_code != 0 {
        return Err(ListError::Exit { code: output.exit_code, stderr: output.stderr.trim().into() });
    }
    Ok(serde_json::from_str(&output.stdout)?)
}

pub(crate) async fn call(server: &str, input: RunInput) -> Result<RunOutput, RunError> {
    run_client("call", server, input).await
}

async fn run_client(command: &str, server: &str, input: RunInput) -> Result<RunOutput, RunError> {
    let bin =
        std::env::current_exe().map_err(|source| RunError::NotExecutable { bin: PathBuf::from("gllmh"), source })?;
    run_with_args(&bin, &["mcp", command, &format!("--server={server}")], input).await
}

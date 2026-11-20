//! The valet's side of tools: which names an agent has, their schemas, and
//! running one tool request from a model response (sections 2, 6.3, 7).

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::llm::ToolSpec;
use crate::paths;
use crate::profile::ResolvedUser;
use crate::protocol::records::{ToolCallInput, ToolCallOutput, ToolResult};
use crate::protocol::tool::{ENV_TURN, RunInput};
use crate::store::ActionRequest;
use crate::tools::{self, SendPayload, mcp};
use crate::types::{ActionId, ActionKind, InteractionId, Sender};

use super::{SendRequest, Valet, home_dir};

/// Where a tool request sits: its turn, step, and the llm_call that asked for it.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub turn: InteractionId,
    pub root: InteractionId,
    pub step: u32,
    pub requested_by: ActionId,
}

/// One tool request from a model response.
#[derive(Debug, Clone)]
pub struct ToolUse {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

/// An executable tool's schema file (section 7, docs/tool-schemas.md).
#[derive(Deserialize)]
pub(super) struct ExecutableSchema {
    name: String,
    description: String,
    input: Value,
}

pub(super) async fn load_mcp_tools(profile: &ResolvedUser, data_dir: &Path) -> Vec<mcp::DiscoveredTool> {
    mcp::discover(&profile.tool_profiles, &tool_cwd(profile, data_dir)).await
}

fn tool_cwd(profile: &ResolvedUser, data_dir: &Path) -> PathBuf {
    profile.cwd.as_deref().map_or_else(|| data_dir.to_owned(), |path| paths::expand_home(path, &home_dir()))
}

enum ToolProcess<'a> {
    Executable(&'a Path),
    Mcp { server: &'a str, tool: &'a str },
}

pub(super) async fn load_executable_schemas(
    profile: &ResolvedUser,
) -> anyhow::Result<BTreeMap<PathBuf, ExecutableSchema>> {
    let home = home_dir();
    let mut schemas = BTreeMap::new();
    for tool in &profile.tool_profiles {
        let Some(path) = &tool.tool_schema else { continue };
        if let Entry::Vacant(entry) = schemas.entry(paths::expand_home(path, &home)) {
            let path = entry.key();
            let label = || format!("tool {:?} schema {}", tool.name, path.display());
            let bytes = tokio::fs::read(path).await.with_context(label)?;
            let envelope: Value = serde_json::from_slice(&bytes).with_context(label)?;
            ensure!(envelope.is_object(), "{}: expected a JSON object", label());
            let schema: ExecutableSchema = serde_json::from_value(envelope).with_context(label)?;
            ensure!(
                !schema.name.is_empty() && schema.input.is_object(),
                "{}: expected a name and input schema object",
                label()
            );
            entry.insert(schema);
        }
    }
    Ok(schemas)
}

impl Valet {
    /// The agent's tool_profile names, as handed to the composer in `init`.
    pub fn tool_names(&self) -> Vec<String> {
        self.profile.tool_profiles.iter().map(|t| t.name.clone()).collect()
    }

    /// Schemas for the tools a compose reply offers, by tool_profile name.
    pub fn tool_specs(&self, names: &[String]) -> Vec<ToolSpec> {
        let edges = self.edges.get(&Sender::user(&self.user));
        let home = home_dir();
        let mut specs = Vec::new();
        for name in names {
            let Some(profile) = self.profile.tool_profiles.iter().find(|p| p.name == *name) else { continue };
            if profile.mcp.is_some() {
                specs.extend(self.mcp_tools.iter().filter(|tool| tool.profile == *name).map(|tool| tool.spec.clone()));
            } else if let Some(builtin) = profile.builtin {
                specs.extend(edges.and_then(|edges| tools::send_spec(name, builtin, edges)));
            } else if let Some(path) = &profile.tool_schema
                && let Some(schema) = self.executable_schemas.get(&paths::expand_home(path, &home))
            {
                specs.push(ToolSpec {
                    name: name.clone(),
                    description: schema.description.clone(),
                    input_schema: schema.input.clone(),
                });
            }
        }
        specs
    }

    /// Record and run one tool request. Its row is written before it runs;
    /// the composer reads the result back from the store.
    /// EXTENSION: malformed builtin send arguments are recorded as a failed
    /// tool_call because they cannot be represented as a broker SendInput.
    pub async fn run_tool(&self, ctx: &ToolCtx, call: ToolUse) {
        if let Err(e) = self.record_and_run_tool(ctx, call).await {
            tracing::error!(requested_by = %ctx.requested_by, "could not record tool call: {e}");
        }
    }

    async fn record_and_run_tool(&self, ctx: &ToolCtx, call: ToolUse) -> crate::store::Result<()> {
        let mcp_tool = self.mcp_tools.iter().find(|tool| tool.spec.name == call.tool);
        let profile_name = mcp_tool.map_or(call.tool.as_str(), |tool| tool.profile.as_str());
        let profile = self.profile.tool_profiles.iter().find(|profile| profile.name == profile_name);
        let invalid_send = if let Some(builtin) = profile.and_then(|p| p.builtin) {
            match tools::send_arguments(builtin, &call.arguments) {
                Ok((to, payload)) => {
                    self.run_send_tool(ctx, call.call_id, to, payload).await;
                    return Ok(());
                }
                Err(e) => Some(format!("{}: {}", e.code, e.message)),
            }
        } else {
            None
        };

        let input = ToolCallInput { tool: call.tool, arguments: call.arguments };
        let mut req = ActionRequest::new(ActionKind::ToolCall, serde_json::to_value(&input).expect("tool input"));
        req.turn = Some(ctx.turn);
        req.root = Some(ctx.root);
        req.step = Some(ctx.step);
        req.requested_by = Some(ctx.requested_by);
        req.call_id = Some(call.call_id);
        req.hazardness = profile.map(|p| p.hazardness);

        let process = profile.and_then(|p| {
            let process = match (mcp_tool, &p.mcp) {
                (Some(tool), Some(mcp)) => ToolProcess::Mcp { server: &mcp.server, tool: &tool.original_name },
                _ => ToolProcess::Executable(p.tool_bin.as_deref()?),
            };
            Some((process, p.timeout_s))
        });
        let runnable = process.is_some() || invalid_send.is_some();
        let id = self
            .store
            .with(move |s| {
                let id = s.request_action(req)?;
                if runnable {
                    s.start_action(id)?;
                } else {
                    s.deny_action(id)?;
                }
                Ok::<_, crate::store::Error>(id)
            })
            .await?;
        let output = if let Some((process, timeout_s)) = process {
            self.run_process(ctx, process, timeout_s, input.arguments).await
        } else if let Some(text) = invalid_send {
            // No typed broker input exists. Record the malformed call for the composer.
            failed_tool(text)
        } else {
            return Ok(());
        };
        self.store
            .with(move |s| {
                let value = serde_json::to_value(&output).expect("tool output");
                if output.tool_result.is_error { s.fail_action(id, &value) } else { s.succeed_action(id, &value, None) }
            })
            .await
    }

    async fn run_send_tool(&self, ctx: &ToolCtx, call_id: String, to: &str, payload: SendPayload) {
        let mut req = SendRequest::new(payload.kind(), to);
        match payload {
            SendPayload::Message(body) => req.body = Some(Value::String(body)),
            SendPayload::Directive(blocks) => req.context = Some(blocks),
        }
        req.caused_by = Some(ctx.turn);
        req.turn = Some(ctx.turn);
        req.root = Some(ctx.root);
        req.step = Some(ctx.step);
        req.requested_by = Some(ctx.requested_by);
        req.call_id = Some(call_id);
        let done = self.send(req).await;
        if let Err(e) = done.result
            && done.action == ActionId::nil()
        {
            tracing::error!(requested_by = %ctx.requested_by, "could not record send tool: {}", e.message);
        }
    }

    async fn run_process(
        &self,
        ctx: &ToolCtx,
        process: ToolProcess<'_>,
        timeout_s: u64,
        arguments: Value,
    ) -> ToolCallOutput {
        let cwd = tool_cwd(&self.profile, &self.data_dir);
        let env = BTreeMap::from([(ENV_TURN.into(), ctx.turn.to_string())]);
        let mut input = RunInput { arguments, cwd, env, timeout_s };
        let result = match process {
            ToolProcess::Executable(bin) => tools::run(&paths::expand_home(bin, &home_dir()), input).await,
            ToolProcess::Mcp { server, tool } => {
                let call = ToolCallInput { tool: tool.into(), arguments: input.arguments };
                input.arguments = serde_json::to_value(call).expect("MCP call input");
                mcp::call(server, input).await
            }
        };
        match result {
            Ok(output) => {
                let is_error = output.exit_code != 0;
                let text = if is_error && matches!(process, ToolProcess::Executable(_)) {
                    let summary = format!("tool exited with code {}", output.exit_code);
                    if output.stdout.is_empty() { summary } else { format!("{summary}\n{}", output.stdout) }
                } else {
                    output.stdout
                };
                ToolCallOutput {
                    tool_result: ToolResult { text, is_error },
                    exit_code: Some(output.exit_code),
                    stderr: Some(output.stderr),
                }
            }
            Err(e) => failed_tool(e.to_string()),
        }
    }
}

fn failed_tool(text: String) -> ToolCallOutput {
    ToolCallOutput { tool_result: ToolResult { text, is_error: true }, exit_code: None, stderr: None }
}

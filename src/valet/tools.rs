//! The valet's side of tools: which names an agent has, their schemas, and
//! running one tool request from a model response (sections 2, 6.3, 7).
//!
//! M4.3 leaves the seam: a tool request is recorded, then denied, because no
//! runner exists yet. M5.1 fills in executables and send tools, M5.3 MCP tools.

use crate::llm::ToolSpec;
use crate::protocol::records::ToolCallInput;
use crate::store::ActionRequest;
use crate::types::{ActionId, ActionKind, InteractionId};

use super::Valet;

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
    pub arguments: serde_json::Value,
}

impl Valet {
    /// The agent's tool_profile names, as handed to the composer in `init`.
    pub fn tool_names(&self) -> Vec<String> {
        self.profile.tool_profiles.iter().map(|t| t.name.clone()).collect()
    }

    /// Schemas for the tools a compose reply offers, by tool_profile name.
    pub fn tool_specs(&self, names: &[String]) -> Vec<ToolSpec> {
        let _ = names;
        Vec::new()
    }

    /// Record and run one tool request. Its row is written before it runs;
    /// the composer reads the result back from the store.
    pub async fn run_tool(&self, ctx: &ToolCtx, call: ToolUse) {
        let mut req = ActionRequest::new(
            ActionKind::ToolCall,
            serde_json::to_value(ToolCallInput { tool: call.tool.clone(), arguments: call.arguments }).expect("input"),
        );
        req.turn = Some(ctx.turn);
        req.root = Some(ctx.root);
        req.step = Some(ctx.step);
        req.requested_by = Some(ctx.requested_by);
        req.call_id = Some(call.call_id);
        // No runner yet (M5.1): the request is refused before it runs, and the
        // composer shows the model a `denied` tool result.
        tracing::warn!(tool = %call.tool, "no tool runner; denying");
        let res = self
            .store
            .with(move |s| {
                let id = s.request_action(req)?;
                s.deny_action(id)
            })
            .await;
        if let Err(e) = res {
            tracing::error!("could not record tool call: {e}");
        }
    }
}

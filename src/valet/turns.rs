//! The turn engine (section 2: turn states, One turn step by step, When
//! something fails; section 5: Recovery steps 5–6; section 4: invariants 7, 8).
//!
//! The valet runs the loop; the composer only builds model input. Each step:
//! compose → write the llm_call row → call the provider → record the response
//! → write and run each tool request → back to compose. A turn ends `done`
//! when a response asks for no tools, `cut_off` at max_steps_per_turn,
//! `failed` on `stop: fail` or two compose failures in a row, and
//! `interrupted` when recovery finds a tool call that was in flight.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use tokio::task::JoinSet;

use crate::frame::ErrorBody;
use crate::llm::{self, LlmConfig};
use crate::profile::ComposerProfile;
use crate::protocol::broker::{self, CloseInput, CloseOutput};
use crate::protocol::composer::{BlockType, ComposeOutput, Stop, codes as compose_codes};
use crate::protocol::records::{LlmCallError, LlmCallFailure, LlmCallInput, LlmCallOutput, Usage};
use crate::store::ActionRequest;
use crate::types::{ActionKind, Interaction, InteractionId, Outcome, TurnOutcome};

use super::Valet;
use super::composer::Composer;
use super::tools::{ToolCtx, ToolUse};

/// A running turn engine; resolves when it stops (the composer exited).
pub struct Engine {
    exited: Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>,
}

impl Engine {
    pub async fn exited(self) -> anyhow::Result<()> {
        self.exited.await
    }
}

/// How a turn ended, and the final response text if it ended `done`.
#[derive(Debug, Clone)]
pub struct TurnEnd {
    pub outcome: TurnOutcome,
    pub text: Option<String>,
}

/// Start the engine. Users without a composer get an engine that never ends.
/// Runs recovery for turns first (section 5, steps 5–6).
pub async fn start(valet: &Arc<Valet>) -> anyhow::Result<Engine> {
    let Some(profile) = valet.profile.composer_profile.clone() else {
        return Ok(Engine { exited: Box::pin(std::future::pending()) });
    };
    let (composer, mut child) = Composer::start(valet, &profile).await?;
    let ctx = Arc::new(TurnRunner { valet: valet.clone(), composer, profile });

    // Step 5: a running turn whose tool call was in flight cannot resume.
    let mut resume = Vec::new();
    for rt in valet.store.with(|s| s.running_turns()).await? {
        if rt.has_unfinished_tool_call || rt.has_interrupted_tool_call {
            tracing::warn!(turn = %rt.turn.trigger, "recovery: tool call was in flight; turn interrupted");
            ctx.end(rt.turn.trigger, TurnEnd { outcome: TurnOutcome::Interrupted, text: None }).await;
        } else {
            tracing::info!(turn = %rt.turn.trigger, "recovery: resuming turn");
            resume.push(rt.turn.trigger);
        }
    }

    let exited = async move {
        let mut running: JoinSet<()> = JoinSet::new();
        let mut started = std::collections::HashSet::new();
        for trigger in resume {
            started.insert(trigger);
            let ctx = ctx.clone();
            running.spawn(async move { ctx.run(trigger).await });
        }
        let max = ctx.profile.max_parallel_turns.max(1) as usize;
        loop {
            // Start queued turns, oldest first, while slots are free (invariant 7).
            if running.len() < max {
                let queued = ctx.valet.store.with(|s| s.queued_turns()).await?;
                for turn in queued {
                    if running.len() >= max {
                        break;
                    }
                    let trigger = turn.trigger;
                    if started.contains(&trigger) {
                        continue;
                    }
                    ctx.valet.store.with(move |s| s.start_turn(trigger)).await?;
                    started.insert(trigger);
                    let ctx = ctx.clone();
                    running.spawn(async move { ctx.run(trigger).await });
                }
            }
            tokio::select! {
                _ = ctx.valet.wake.notified() => {}
                Some(r) = running.join_next(), if !running.is_empty() => {
                    if let Err(e) = r {
                        tracing::error!("turn task panicked: {e}");
                    }
                }
                status = child.wait() => {
                    let status = status.context("wait for the composer")?;
                    anyhow::bail!("composer exited ({status}); the valet exits with it");
                }
            }
        }
    };
    Ok(Engine { exited: Box::pin(exited) })
}

impl Valet {
    /// A turn was queued or ended: let the engine fill free slots.
    pub fn turns_wake(&self) {
        self.wake.notify_one();
    }
}

struct TurnRunner {
    valet: Arc<Valet>,
    composer: Composer,
    profile: ComposerProfile,
}

impl TurnRunner {
    /// Run one turn from wherever its rows say it is, to its end.
    async fn run(&self, trigger: InteractionId) {
        let end = match self.steps(trigger).await {
            Ok(end) => end,
            Err(e) => {
                tracing::error!(turn = %trigger, "turn failed: {e:#}");
                TurnEnd { outcome: TurnOutcome::Failed, text: None }
            }
        };
        self.end(trigger, end).await;
        self.valet.turns_wake();
    }

    async fn steps(&self, trigger: InteractionId) -> anyhow::Result<TurnEnd> {
        let turn = self.valet.store.with(move |s| s.get_turn(trigger)).await?.context("turn row missing")?;
        let max_steps = self.profile.max_steps_per_turn;
        let mut compose_failures = 0;
        loop {
            // Invariant 8: no model call beyond max_steps_per_turn.
            let steps = self.valet.store.with(move |s| s.get_turn(trigger)).await?.map_or(0, |t| t.steps);
            if steps >= max_steps {
                return Ok(TurnEnd { outcome: TurnOutcome::CutOff, text: None });
            }

            let out = match self.compose(trigger).await {
                Ok(out) => {
                    compose_failures = 0;
                    out
                }
                Err(e) => {
                    compose_failures += 1;
                    tracing::warn!(turn = %trigger, code = %e.code, "compose failed ({compose_failures}): {}", e.message);
                    if compose_failures >= 2 {
                        return Ok(TurnEnd { outcome: TurnOutcome::Failed, text: None });
                    }
                    continue;
                }
            };

            for mark in &out.marks {
                let (target, key, value) = (mark.target, mark.key.clone(), mark.value.clone());
                self.valet.store.with(move |s| s.set_mark(target, &key, &value, trigger)).await?;
            }
            match out.stop {
                Some(Stop::Done) => return Ok(TurnEnd { outcome: TurnOutcome::Done, text: None }),
                Some(Stop::Fail) => return Ok(TurnEnd { outcome: TurnOutcome::Failed, text: None }),
                None => {}
            }

            let step = self.valet.store.with(move |s| s.increment_turn_step(trigger, max_steps)).await?;
            let Some(response) = self.llm_call(trigger, turn.root, step, &out).await? else {
                // Failed after retries: the composer sees the failed row and decides.
                continue;
            };
            let (llm_action, content) = response;

            let uses: Vec<ToolUse> = content
                .iter()
                .filter(|b| b.kind == BlockType::ToolUse)
                .map(|b| ToolUse {
                    call_id: b.call_id.clone().unwrap_or_default(),
                    tool: b.tool.clone().unwrap_or_default(),
                    arguments: b.arguments.clone().unwrap_or(serde_json::Value::Null),
                })
                .collect();
            if uses.is_empty() {
                let text: String =
                    content.iter().filter(|b| b.kind == BlockType::Text).filter_map(|b| b.text.as_deref()).collect();
                return Ok(TurnEnd { outcome: TurnOutcome::Done, text: Some(text) });
            }
            let ctx = ToolCtx { turn: trigger, root: turn.root, step, requested_by: llm_action };
            for call in uses {
                self.valet.run_tool(&ctx, call).await;
            }
        }
    }

    /// One compose, validated against what the agent has (6.2 invalid_reply).
    async fn compose(&self, trigger: InteractionId) -> Result<ComposeOutput, ErrorBody> {
        let out = self.composer.compose(trigger).await?;
        if out.stop.is_none() {
            if !self.valet.profile.llm_profiles.iter().any(|p| p.name == out.llm_profile) {
                return Err(ErrorBody::new(
                    compose_codes::INVALID_REPLY,
                    format!("unknown llm_profile {:?}", out.llm_profile),
                ));
            }
            let names = self.valet.tool_names();
            if let Some(bad) = out.tools.iter().find(|t| !names.contains(t)) {
                return Err(ErrorBody::new(compose_codes::INVALID_REPLY, format!("unknown tool {bad:?}")));
            }
        }
        Ok(out)
    }

    /// Write the llm_call row, call the provider, record the outcome.
    /// Returns the action id and response content, or None if it failed.
    async fn llm_call(
        &self,
        trigger: InteractionId,
        root: InteractionId,
        step: u32,
        out: &ComposeOutput,
    ) -> anyhow::Result<Option<(crate::types::ActionId, Vec<crate::protocol::composer::ContentBlock>)>> {
        let profile =
            self.valet.profile.llm_profiles.iter().find(|p| p.name == out.llm_profile).context("llm_profile")?.clone();
        let config: LlmConfig = serde_json::from_value(serde_json::to_value(&profile)?)?;
        // A compose reply may ask for thinking; a model that never thinks
        // (thinking.native = never) is not asked to (docs/decisions.md).
        let thinking = out.thinking.unwrap_or(profile.thinking.on)
            && profile.thinking.native != crate::profile::NativeThinking::Never;
        let specs = self.valet.tool_specs(&out.tools);
        let offered: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();

        let built = llm::build_request(&config, &out.messages, &specs, thinking);
        let request_value = match &built {
            Ok(body) => serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::Value::Null,
        };
        let input = LlmCallInput {
            llm_profile: out.llm_profile.clone(),
            messages: out.messages.clone(),
            tools: offered,
            thinking,
            request: request_value,
        };
        let mut req = ActionRequest::new(ActionKind::LlmCall, serde_json::to_value(input)?);
        req.turn = Some(trigger);
        req.root = Some(root);
        req.step = Some(step);
        let id = self
            .valet
            .store
            .with(move |s| {
                let id = s.request_action(req)?;
                s.start_action(id)?;
                Ok::<_, crate::store::Error>(id)
            })
            .await?;

        let result = match built {
            Ok(body) => llm::call_body(&config, &body).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(resp) => {
                let thinking_raw: Vec<&str> = resp
                    .content
                    .iter()
                    .filter(|b| b.kind == BlockType::Thinking)
                    .filter_map(|b| b.raw_json.as_deref())
                    .collect();
                let reasoning = (!thinking_raw.is_empty()).then(|| format!("[{}]", thinking_raw.join(",")));
                let output = LlmCallOutput {
                    content: resp.content.clone(),
                    stop_reason: resp.stop_reason.clone().unwrap_or_default(),
                    usage: resp
                        .usage
                        .map(|u| Usage { input_tokens: u.input_tokens, output_tokens: u.output_tokens })
                        .unwrap_or_default(),
                    response: serde_json::from_str(&resp.raw).unwrap_or(serde_json::Value::Null),
                };
                let output = serde_json::to_value(output)?;
                self.valet.store.with(move |s| s.succeed_action(id, &output, reasoning.as_deref())).await?;
                Ok(Some((id, resp.content)))
            }
            Err(e) => {
                tracing::warn!(turn = %trigger, "llm call failed: {e}");
                let failure = LlmCallFailure {
                    error: LlmCallError {
                        kind: serde_json::to_value(e.kind)?.as_str().unwrap_or("other").to_string(),
                        status: e.status,
                        message: e.message.clone(),
                        retryable: e.retryable(),
                        attempts: e.attempts as u32,
                    },
                };
                let output = serde_json::to_value(failure)?;
                self.valet.store.with(move |s| s.fail_action(id, &output)).await?;
                Ok(None)
            }
        }
    }

    /// End a turn: record the end state, let results go out (M6.1), close the
    /// interaction with the turn's outcome.
    async fn end(&self, trigger: InteractionId, end: TurnEnd) {
        let outcome = end.outcome;
        if let Err(e) = self.valet.store.with(move |s| s.end_turn(trigger, outcome)).await {
            tracing::error!(turn = %trigger, "could not record turn end: {e}");
        }
        tracing::info!(turn = %trigger, outcome = %outcome, "turn ended");
        let interaction = self
            .valet
            .store
            .with(move |s| s.receive_action(trigger))
            .await
            .ok()
            .flatten()
            .and_then(|a| serde_json::from_value::<Interaction>(a.input).ok());
        if let Some(interaction) = &interaction {
            self.valet.on_turn_end(interaction, &end).await;
        }
        let close = CloseInput { id: trigger, outcome: Outcome::from(outcome) };
        if let Err(e) = self.valet.broker.request::<_, CloseOutput>(broker::CLOSE, &close).await {
            tracing::warn!(turn = %trigger, "close failed: {e}");
        }
    }
}

impl Valet {
    /// Hook for M6.1: a turn opened by a directive sends its result here.
    pub async fn on_turn_end(&self, _trigger: &Interaction, _end: &TurnEnd) {}
}

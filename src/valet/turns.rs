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
use std::time::Duration;

use anyhow::Context;
use tokio::task::JoinSet;

use crate::frame::ErrorBody;
use crate::llm::{self, LlmConfig};
use crate::profile::ComposerProfile;
use crate::protocol::broker::{self, CloseInput, CloseOutput, LookupInput, LookupOutput};
use crate::protocol::composer::{BlockType, ComposeOutput, Stop, codes as compose_codes};
use crate::protocol::records::{LlmCallError, LlmCallFailure, LlmCallInput, LlmCallOutput, Usage};
use crate::store::ActionRequest;
use crate::types::{ActionKind, Interaction, InteractionId, InteractionKind, Outcome, TurnOutcome};

use super::composer::Composer;
use super::tools::{ToolCtx, ToolUse};
use super::{SendRequest, Valet};

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

    // Send recovery has already resolved any ambiguously accepted results.
    for (directive, turn, text) in valet.store.with(|s| s.unclosed_directives()).await? {
        if let Some(outcome) = turn.state.turn_outcome() {
            valet.on_turn_end(&directive, &TurnEnd { outcome, text }).await;
        }
    }

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

    /// Record the outcome before sending any owed result, then close.
    async fn end(&self, trigger: InteractionId, end: TurnEnd) {
        if let Err(e) = self.record_end(trigger, end).await {
            tracing::error!(turn = %trigger, "could not finish turn: {e:#}");
        }
    }

    async fn record_end(&self, trigger: InteractionId, end: TurnEnd) -> anyhow::Result<()> {
        let outcome = end.outcome;
        let result_text = end.text.clone();
        let interaction = self
            .valet
            .store
            .with(move |s| -> anyhow::Result<_> {
                let receive = s.receive_action(trigger)?.context("turn's receive action missing")?;
                let interaction: Interaction = serde_json::from_value(receive.input)?;
                if interaction.kind == InteractionKind::Directive {
                    s.end_directive(trigger, outcome, result_text.as_deref())?;
                } else {
                    s.end_turn(trigger, outcome)?;
                }
                Ok(interaction)
            })
            .await?;
        tracing::info!(turn = %trigger, outcome = %outcome, "turn ended");
        if interaction.kind == InteractionKind::Directive {
            self.valet.on_turn_end(&interaction, &end).await;
        } else {
            super::deliver::close(&self.valet, &interaction, outcome.into()).await;
        }
        Ok(())
    }
}

impl Valet {
    /// Finish a directive's broker obligations, in order (sections 2 and 4).
    async fn on_turn_end(&self, directive: &Interaction, end: &TurnEnd) {
        if let Err(e) = self.send_result(directive, end).await {
            tracing::error!(turn = %directive.id, outcome = %end.outcome, "directive result was not confirmed: {e:#}");
            return;
        }
        self.close_directive(directive.id, end.outcome).await;
    }

    async fn send_result(&self, directive: &Interaction, end: &TurnEnd) -> anyhow::Result<()> {
        let trigger = directive.id;
        if self.store.with(move |s| s.has_sent_result(trigger)).await? {
            return Ok(());
        }

        let mut req = SendRequest::new(InteractionKind::Result, &directive.from.user);
        req.caused_by = Some(trigger);
        req.turn_outcome = Some(end.outcome);
        req.turn = Some(trigger);
        req.root = Some(directive.root);
        if end.outcome == TurnOutcome::Done {
            req.body = end.text.clone().map(serde_json::Value::String);
        }

        // Allow about eight seconds for explicit retryable errors. An
        // ambiguous send must be settled before creating a new send action.
        let mut delays = [250, 500, 1000, 2000, 4000].into_iter();
        loop {
            let sent = self.send(req.clone()).await;
            let Err(error) = sent.result else {
                return Ok(());
            };
            if error.code == crate::protocol::local::codes::BROKER_UNAVAILABLE {
                let id = sent.action;
                let reply: LookupOutput = self.broker.request(broker::LOOKUP, &LookupInput { client_ref: id }).await?;
                if reply.found {
                    let accepted = reply.interaction.context("broker lookup omitted the accepted result")?;
                    self.store.with(move |s| s.recover_send(id, &accepted)).await?;
                    return Ok(());
                }
                self.store.with(move |s| s.interrupt_action(id)).await?;
            }
            let Some(ms) = delays.next().filter(|_| error.retry) else {
                return Err(error.into());
            };
            tracing::warn!(turn = %trigger, "retrying directive result in {ms} ms: {error}");
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }

    /// Confirm the close only after the broker acknowledges it. On startup
    /// a delivery whose ack was lost may still be waiting for redelivery.
    pub(super) async fn close_directive(&self, trigger: InteractionId, outcome: TurnOutcome) {
        let close = CloseInput { id: trigger, outcome: Outcome::from(outcome) };
        match self.broker.request::<_, CloseOutput>(broker::CLOSE, &close).await {
            Ok(_) => {
                if let Err(e) = self.store.with(move |s| s.confirm_directive_close(trigger)).await {
                    tracing::error!(turn = %trigger, "could not record directive close: {e}");
                }
            }
            Err(e) => tracing::warn!(turn = %trigger, "directive close failed: {e}"),
        }
    }
}

//! Select and render the recorded context for one compose request (spec section 7).

use std::{collections::HashMap, fmt::Write, fs, path::PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    frame::{ErrorBody, codes},
    paths,
    profile::{BuiltinComposer, BuiltinMode, ComposerProfile, LlmProfile, ThinkingReplay, WorkMode},
    protocol::{
        broker::Edges,
        composer::{BlockType, ComposeOutput, ContentBlock, InitInput, Message, Role, Stop},
        records::{LlmCallOutput, ToolResult, send_tool_result, tool_result_of},
    },
    store::ReadOnlyStore,
    types::{Action, ActionId, ActionKind, ActionState, Block, Interaction, InteractionId, InteractionKind, Sender},
};

#[derive(Debug, thiserror::Error)]
pub(super) enum InitError {
    #[error("invalid composer profile: {0}")]
    ComposerProfile(serde_json::Error),
    #[error("gllmh composer requires a builtin profile")]
    CustomEngine,
    #[error("at least one llm profile is required")]
    NoLlmProfile,
    #[error("invalid first llm profile: {0}")]
    LlmProfile(serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ComposeError {
    #[error("unknown turn {0}")]
    UnknownTurn(InteractionId),
    #[error("cannot read composer store: {0}")]
    Store(#[from] crate::store::Error),
    #[error("missing receive action for trigger {0}")]
    MissingTrigger(InteractionId),
    #[error("missing output for succeeded llm_call {0}")]
    MissingOutput(ActionId),
    #[error("tool_use is missing a call_id in llm_call {0}")]
    MissingCallId(ActionId),
    #[error("cannot decode {kind} action {id}: {source}")]
    Decode { kind: ActionKind, id: ActionId, source: serde_json::Error },
}

impl ComposeError {
    pub(super) fn into_reply(self) -> ErrorBody {
        let code = match self {
            Self::UnknownTurn(_) => codes::BAD_INPUT,
            _ => codes::INTERNAL,
        };
        ErrorBody::new(code, self.to_string())
    }
}

pub(super) struct Composer {
    store: PathBuf,
    user: String,
    edges: Edges,
    tools: Vec<String>,
    work_mode: WorkMode,
    builtin: BuiltinComposer,
    llm: LlmProfile,
}

impl Composer {
    pub(super) fn new(mut input: InitInput) -> Result<Self, InitError> {
        let profile: ComposerProfile =
            serde_json::from_value(input.composer_profile).map_err(InitError::ComposerProfile)?;
        let builtin = profile.builtin_settings().ok_or(InitError::CustomEngine)?;
        let first = input.llm_profiles.into_iter().next().ok_or(InitError::NoLlmProfile)?;
        let llm = serde_json::from_value(first).map_err(InitError::LlmProfile)?;
        let edges = input.edges.remove(&Sender::user(&input.user)).unwrap_or_default();
        Ok(Self {
            store: input.store,
            user: input.user,
            edges,
            tools: input.tools,
            work_mode: profile.work_mode,
            builtin,
            llm,
        })
    }

    pub(super) fn compose(&self, trigger: InteractionId) -> Result<ComposeOutput, ComposeError> {
        let store = ReadOnlyStore::open(&self.store)?;
        if store.turn_by_trigger(trigger)?.is_none() {
            return Err(ComposeError::UnknownTurn(trigger));
        }

        let actions = match self.builtin.mode {
            BuiltinMode::Message => store.actions()?,
            BuiltinMode::Directive => store.actions_of_turn(trigger)?,
        };
        let failed = actions
            .iter()
            .rev()
            .find(|a| a.kind == ActionKind::LlmCall && a.turn == Some(trigger))
            .is_some_and(|a| a.state == ActionState::Failed);
        let mut output = ComposeOutput {
            llm_profile: self.llm.name.clone(),
            messages: Vec::new(),
            tools: match self.work_mode {
                WorkMode::Inst | WorkMode::Reason => Vec::new(),
                WorkMode::Act | WorkMode::React => self.tools.clone(),
            },
            thinking: Some(matches!(self.work_mode, WorkMode::Reason | WorkMode::React)),
            marks: Vec::new(),
            stop: failed.then_some(Stop::Fail),
        };
        if !failed {
            output.messages = self.messages(&actions, trigger)?;
        }
        Ok(output)
    }

    fn messages(&self, actions: &[Action], trigger: InteractionId) -> Result<Vec<Message>, ComposeError> {
        let mut groups = Vec::new();
        let mut prefix = vec![self.system_message()];

        let tool_rows = index_tool_rows(actions);
        let mut found_trigger = false;
        for action in actions {
            match action.kind {
                ActionKind::Receive => {
                    let interaction =
                        Interaction::deserialize(&action.input).map_err(|source| decode_error(action, source))?;
                    let pinned = action.interaction == Some(trigger);
                    found_trigger |= pinned;
                    if pinned && self.builtin.mode == BuiltinMode::Directive {
                        let text = if interaction.kind == InteractionKind::Directive {
                            context_sections(interaction.context.as_deref().unwrap_or_default())
                        } else {
                            receive_text(&interaction)
                        };
                        prefix.push(text_message(Role::User, text));
                    } else {
                        groups.push((pinned, vec![text_message(Role::User, receive_text(&interaction))]));
                    }
                }
                ActionKind::LlmCall if action.state == ActionState::Succeeded => {
                    groups.push((false, self.model_step(action, tool_rows.get(&action.id))?));
                }
                _ => {}
            }
        }
        if !found_trigger {
            return Err(ComposeError::MissingTrigger(trigger));
        }

        let budget = self.llm.context_window as f64 * self.builtin.budget_fraction;
        Ok(trim(prefix, groups, budget))
    }

    fn system_message(&self) -> Message {
        let mut sections = Vec::new();
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        for path in &self.llm.tuning {
            let path = paths::expand_home(path, &home);
            match fs::read_to_string(&path) {
                Ok(text) => sections.push(text),
                Err(error) => tracing::warn!(path = %path.display(), %error, "skipping tuning file"),
            }
        }
        let mut header = format!("You are {}.", self.user);
        for (tool, verb, recipients) in
            [("send_message", "message", &self.edges.messages), ("send_directive", "direct", &self.edges.directives)]
        {
            if self.tools.iter().any(|name| name == tool) {
                if recipients.is_empty() {
                    write!(header, "\nYou may {verb} no users.").expect("writing to String");
                } else {
                    write!(header, "\nYou may {verb} {}.", recipients.join(", ")).expect("writing to String");
                }
            }
        }
        sections.push(header);
        text_message(Role::System, sections.join("\n\n"))
    }

    fn model_step(
        &self,
        action: &Action,
        tool_rows: Option<&HashMap<&str, &Action>>,
    ) -> Result<Vec<Message>, ComposeError> {
        let output = action.output.as_ref().ok_or(ComposeError::MissingOutput(action.id))?;
        let mut output = LlmCallOutput::deserialize(output).map_err(|source| decode_error(action, source))?;
        output.content.retain(|block| match block.kind {
            BlockType::Text | BlockType::ToolUse => true,
            BlockType::Thinking => self.llm.thinking.replay != ThinkingReplay::Never,
            BlockType::ToolResult => false,
        });
        if output.content.is_empty() {
            return Ok(Vec::new());
        }
        let results: Vec<_> = output
            .content
            .iter()
            .filter(|block| block.kind == BlockType::ToolUse)
            .map(|block| {
                let call_id = block.call_id.as_deref().ok_or(ComposeError::MissingCallId(action.id))?;
                let row = tool_rows.and_then(|rows| rows.get(call_id).copied());
                let result = row.map_or_else(|| error_result("not run"), render_tool_result);
                Ok(ContentBlock::tool_result(call_id, result.text, result.is_error))
            })
            .collect::<Result<_, ComposeError>>()?;
        let mut messages = vec![Message { role: Role::Assistant, content: output.content }];
        if !results.is_empty() {
            messages.push(Message { role: Role::ToolResult, content: results });
        }
        Ok(messages)
    }
}

fn decode_error(action: &Action, source: serde_json::Error) -> ComposeError {
    ComposeError::Decode { kind: action.kind, id: action.id, source }
}

fn index_tool_rows(actions: &[Action]) -> HashMap<ActionId, HashMap<&str, &Action>> {
    let mut index = HashMap::<_, HashMap<_, _>>::new();
    for action in actions.iter().filter(|a| matches!(a.kind, ActionKind::ToolCall | ActionKind::Send)) {
        if let (Some(parent), Some(call_id)) = (action.requested_by, action.call_id.as_deref()) {
            index.entry(parent).or_default().entry(call_id).or_insert(action);
        }
    }
    index
}

fn render_tool_result(action: &Action) -> ToolResult {
    match action.state {
        ActionState::Succeeded | ActionState::Failed => {
            let result = match action.kind {
                ActionKind::Send => send_tool_result(action),
                _ => action.output.as_ref().and_then(tool_result_of),
            };
            let missing = if action.state == ActionState::Failed { "failed" } else { "missing tool result" };
            result.unwrap_or_else(|| error_result(missing))
        }
        state => error_result(state.as_str()),
    }
}

fn error_result(text: &str) -> ToolResult {
    ToolResult { text: text.into(), is_error: true }
}

fn receive_text(interaction: &Interaction) -> String {
    let mut prefix = format!("[{} from {}", interaction.kind.as_str(), interaction.from);
    if let Some(outcome) = interaction.turn_outcome.filter(|_| interaction.kind == InteractionKind::Result) {
        write!(prefix, ": {}", outcome.as_str()).expect("writing to String");
    }
    prefix.push(']');
    let body = match interaction.kind {
        InteractionKind::Directive => interaction.context.as_deref().map(context_sections),
        _ => interaction.body.as_ref().map(|body| match body {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }),
    };
    if let Some(body) = body {
        prefix.push('\n');
        prefix.push_str(&body);
    }
    prefix
}

fn context_sections(blocks: &[Block]) -> String {
    blocks.iter().map(|block| format!("## {}\n{}", block.name, block.content)).collect::<Vec<_>>().join("\n\n")
}

fn text_message(role: Role, text: String) -> Message {
    Message { role, content: vec![ContentBlock::text(text)] }
}

// Estimate one token per four Unicode characters of the serialized messages.
// Evict whole model steps, including all their tool results.
fn trim(mut prefix: Vec<Message>, groups: Vec<(bool, Vec<Message>)>, budget: f64) -> Vec<Message> {
    fn chars(messages: &[Message]) -> usize {
        messages.iter().map(|message| serde_json::to_string(message).expect("message serializes").chars().count()).sum()
    }
    let groups: Vec<_> = groups.into_iter().map(|(pinned, messages)| (pinned, chars(&messages), messages)).collect();
    let mut total = chars(&prefix) + groups.iter().map(|(_, cost, _)| cost).sum::<usize>();
    for (pinned, cost, messages) in groups {
        if !pinned && total as f64 / 4.0 > budget {
            total -= cost;
        } else {
            prefix.extend(messages);
        }
    }
    prefix
}

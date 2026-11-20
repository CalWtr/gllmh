use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use super::{LlmConfig, LlmError, LlmResponse, ToolSpec, Usage, encode_request, replay_json, required, thinking_block};
use crate::protocol::composer::{BlockType, ContentBlock, Message, Role};

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<RequestBlock<'a>>,
    messages: Vec<RequestMessage<'a>>,
    #[serde(skip_serializing_if = "<[ToolSpec]>::is_empty")]
    tools: &'a [ToolSpec],
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
}

#[derive(Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u64,
}

#[derive(Serialize)]
struct RequestMessage<'a> {
    role: &'static str,
    content: Vec<RequestBlock<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum RequestBlock<'a> {
    Standard(StandardBlock<'a>),
    Thinking(Box<RawValue>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StandardBlock<'a> {
    Text { text: &'a str },
    ToolUse { id: &'a str, name: &'a str, input: &'a Value },
    ToolResult { tool_use_id: &'a str, content: &'a str, is_error: bool },
}

pub(super) fn build_request(
    config: &LlmConfig,
    messages: &[Message],
    tools: &[ToolSpec],
    thinking: bool,
) -> Result<String, LlmError> {
    let thinking = if thinking {
        if config.max_output <= 1_024 {
            return Err(LlmError::bad_request("Anthropic thinking requires max_output greater than 1024"));
        }
        Some(Thinking { kind: "enabled", budget_tokens: (config.max_output / 2).max(1_024) })
    } else {
        None
    };
    let mut request = Request {
        model: &config.model_id,
        max_tokens: config.max_output,
        system: Vec::new(),
        messages: Vec::new(),
        tools,
        thinking,
    };

    for message in messages {
        let mut content = message
            .content
            .iter()
            .filter_map(|block| translate_block(message.role, block).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        if message.role == Role::System {
            request.system.append(&mut content);
            continue;
        }

        let role = if message.role == Role::Assistant { "assistant" } else { "user" };
        // Adjacent user inputs share a message, including parallel tool
        // results produced as separate composer messages.
        if let Some(previous) = request.messages.last_mut().filter(|previous| previous.role == role) {
            previous.content.append(&mut content);
        } else if !content.is_empty() {
            request.messages.push(RequestMessage { role, content });
        }
    }
    encode_request(request)
}

fn translate_block(role: Role, block: &ContentBlock) -> Result<Option<RequestBlock<'_>>, LlmError> {
    let standard = match (role, block.kind) {
        (Role::System | Role::User | Role::Assistant, BlockType::Text) => {
            StandardBlock::Text { text: required(block.text.as_deref(), "text")? }
        }
        (Role::Assistant, BlockType::ToolUse) => StandardBlock::ToolUse {
            id: required(block.call_id.as_deref(), "call_id")?,
            name: required(block.tool.as_deref(), "tool")?,
            input: required(block.arguments.as_ref(), "arguments")?,
        },
        (Role::ToolResult, BlockType::ToolResult) => StandardBlock::ToolResult {
            tool_use_id: required(block.call_id.as_deref(), "call_id")?,
            content: required(block.text.as_deref(), "text")?,
            is_error: block.is_error,
        },
        (Role::Assistant, BlockType::Thinking) => {
            let raw = required(block.raw.as_ref(), "raw")?;
            if raw.is_string() {
                // A Chat Completions reasoning_content field.
                return Ok(None);
            }
            if !matches!(raw.get("type").and_then(Value::as_str), Some("thinking" | "redacted_thinking")) {
                return Err(LlmError::bad_request("invalid Anthropic thinking block"));
            }
            return Ok(Some(RequestBlock::Thinking(replay_json(block, raw)?)));
        }
        _ => return Err(LlmError::bad_request(format!("unexpected {:?} block in {role:?} message", block.kind))),
    };
    Ok(Some(RequestBlock::Standard(standard)))
}

#[derive(Deserialize)]
struct Response<'a> {
    #[serde(borrow)]
    content: Vec<&'a RawValue>,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking,
    RedactedThinking,
    #[serde(other)]
    Other,
}

pub(super) fn parse_response(body: &str) -> serde_json::Result<LlmResponse> {
    let response: Response<'_> = serde_json::from_str(body)?;
    let mut content = Vec::with_capacity(response.content.len());
    for raw in response.content {
        let block = match serde_json::from_str(raw.get())? {
            ResponseBlock::Text { text } => ContentBlock::text(text),
            ResponseBlock::ToolUse { id, name, input } => ContentBlock::tool_use(id, name, input),
            ResponseBlock::Thinking | ResponseBlock::RedactedThinking => thinking_block(raw)?,
            ResponseBlock::Other => continue,
        };
        content.push(block);
    }
    let usage = response.usage;
    Ok(LlmResponse {
        content,
        stop_reason: response.stop_reason,
        usage: Some(Usage {
            input_tokens: usage
                .input_tokens
                .saturating_add(usage.cache_creation_input_tokens.unwrap_or_default())
                .saturating_add(usage.cache_read_input_tokens.unwrap_or_default()),
            output_tokens: usage.output_tokens,
        }),
        raw: body.to_owned(),
    })
}

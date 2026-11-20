use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use super::{LlmConfig, LlmError, LlmResponse, ToolSpec, Usage, encode_request, replay_json, required, thinking_block};
use crate::protocol::composer::{BlockType, ContentBlock, Message, Role};

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    max_completion_tokens: u64,
    messages: Vec<RequestMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<RequestTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum RequestMessage<'a> {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<Box<RawValue>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: String,
    },
}

#[derive(Serialize)]
struct RequestTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolDefinition<'a>,
}

#[derive(Serialize)]
struct ToolDefinition<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Serialize, Deserialize)]
struct ToolCall<'a> {
    #[serde(borrow)]
    id: Cow<'a, str>,
    #[serde(rename = "type")]
    kind: ToolKind,
    #[serde(borrow)]
    function: FunctionCall<'a>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolKind {
    Function,
}

#[derive(Serialize, Deserialize)]
struct FunctionCall<'a> {
    #[serde(borrow)]
    name: Cow<'a, str>,
    arguments: String,
}

pub(super) fn build_request(
    config: &LlmConfig,
    messages: &[Message],
    tools: &[ToolSpec],
    thinking: bool,
) -> Result<String, LlmError> {
    let mut translated = Vec::new();
    for message in messages {
        match message.role {
            Role::ToolResult => {
                for block in &message.content {
                    if block.kind != BlockType::ToolResult {
                        return Err(unexpected_block(message.role, block.kind));
                    }
                    let text = required(block.text.as_deref(), "text")?;
                    let content = if block.is_error { format!("[tool error]\n{text}") } else { text.to_owned() };
                    translated.push(RequestMessage::Tool {
                        tool_call_id: required(block.call_id.as_deref(), "call_id")?,
                        content,
                    });
                }
            }
            Role::System => translated.push(RequestMessage::System { content: text_only(message)? }),
            Role::User => translated.push(RequestMessage::User { content: text_only(message)? }),
            Role::Assistant => translated.push(assistant_message(message)?),
        }
    }
    encode_request(Request {
        model: &config.model_id,
        max_completion_tokens: config.max_output,
        messages: translated,
        tools: tools
            .iter()
            .map(|tool| RequestTool {
                kind: "function",
                function: ToolDefinition {
                    name: &tool.name,
                    description: &tool.description,
                    parameters: &tool.input_schema,
                },
            })
            .collect(),
        reasoning_effort: thinking.then_some("medium"),
    })
}

fn text_only(message: &Message) -> Result<String, LlmError> {
    let mut content = String::new();
    for block in &message.content {
        if block.kind != BlockType::Text {
            return Err(unexpected_block(message.role, block.kind));
        }
        content.push_str(required(block.text.as_deref(), "text")?);
    }
    Ok(content)
}

fn assistant_message(message: &Message) -> Result<RequestMessage<'_>, LlmError> {
    let mut content: Option<String> = None;
    let mut tool_calls = Vec::new();
    let mut reasoning_content = None;
    for block in &message.content {
        match block.kind {
            BlockType::Text => content.get_or_insert_default().push_str(required(block.text.as_deref(), "text")?),
            BlockType::ToolUse => tool_calls.push(ToolCall {
                id: required(block.call_id.as_deref(), "call_id")?.into(),
                kind: ToolKind::Function,
                function: FunctionCall {
                    name: required(block.tool.as_deref(), "tool")?.into(),
                    arguments: encode_request(required(block.arguments.as_ref(), "arguments")?)?,
                },
            }),
            BlockType::Thinking => {
                let raw = required(block.raw.as_ref(), "raw")?;
                if matches!(raw.get("type").and_then(Value::as_str), Some("thinking" | "redacted_thinking")) {
                    continue;
                }
                if !raw.is_string() || reasoning_content.is_some() {
                    return Err(LlmError::bad_request("expected one reasoning_content string per assistant message"));
                }
                reasoning_content = Some(replay_json(block, raw)?);
            }
            BlockType::ToolResult => return Err(unexpected_block(message.role, block.kind)),
        }
    }
    Ok(RequestMessage::Assistant { content, tool_calls, reasoning_content })
}

fn unexpected_block(role: Role, kind: BlockType) -> LlmError {
    LlmError::bad_request(format!("unexpected {kind:?} block in {role:?} message"))
}

#[derive(Deserialize)]
struct Response<'a> {
    #[serde(borrow)]
    choices: Vec<Choice<'a>>,
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct Choice<'a> {
    #[serde(borrow)]
    message: ResponseMessage<'a>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage<'a> {
    content: Option<String>,
    refusal: Option<String>,
    #[serde(borrow)]
    reasoning_content: Option<&'a RawValue>,
    #[serde(borrow)]
    tool_calls: Option<Vec<ToolCall<'a>>>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

pub(super) fn parse_response(body: &str) -> serde_json::Result<LlmResponse> {
    let response: Response<'_> = serde_json::from_str(body)?;
    let first = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| serde::de::Error::custom("no choices in Chat Completions response"))?;
    let mut content = Vec::new();
    if let Some(raw) = first.message.reasoning_content {
        let block = thinking_block(raw)?;
        if !block.raw.as_ref().is_some_and(Value::is_string) {
            return Err(serde::de::Error::custom("reasoning_content must be a string"));
        }
        content.push(block);
    }
    if let Some(text) = first.message.content.or(first.message.refusal) {
        content.push(ContentBlock::text(text));
    }
    for call in first.message.tool_calls.into_iter().flatten() {
        let arguments = serde_json::from_str(&call.function.arguments)?;
        content.push(ContentBlock::tool_use(call.id, call.function.name, arguments));
    }
    Ok(LlmResponse {
        content,
        stop_reason: first.finish_reason,
        usage: response
            .usage
            .map(|usage| Usage { input_tokens: usage.prompt_tokens, output_tokens: usage.completion_tokens }),
        raw: body.to_owned(),
    })
}

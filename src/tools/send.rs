//! Builtin send tools (section 7). The valet's send path checks recipient edges.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::frame::{ErrorBody, codes};
use crate::llm::ToolSpec;
use crate::profile::BuiltinTool;
use crate::protocol::broker::Edges;
use crate::types::{Block, InteractionKind};

pub(crate) fn send_spec(name: &str, builtin: BuiltinTool, edges: &Edges) -> Option<ToolSpec> {
    let (recipients, description, field, schema) = match builtin {
        BuiltinTool::SendMessage => (&edges.messages, "Send a message", "body", json!({ "type": "string" })),
        BuiltinTool::SendDirective => (
            &edges.directives,
            "Send a directive",
            "context",
            json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "name": { "type": "string" }, "content": { "type": "string" } },
                    "required": ["name", "content"]
                }
            }),
        ),
    };
    if recipients.is_empty() {
        return None;
    }

    Some(ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "enum": recipients },
                field: schema
            },
            "required": ["to", field]
        }),
    })
}

pub(crate) enum SendPayload {
    Message(String),
    Directive(Vec<Block>),
}

impl SendPayload {
    pub(crate) fn kind(&self) -> InteractionKind {
        match self {
            Self::Message(_) => InteractionKind::Message,
            Self::Directive(_) => InteractionKind::Directive,
        }
    }
}

pub(crate) fn send_arguments(builtin: BuiltinTool, args: &Value) -> Result<(&str, SendPayload), ErrorBody> {
    let bad_input = |message| ErrorBody::new(codes::BAD_INPUT, message);
    let to = args.get("to").and_then(Value::as_str).ok_or_else(|| bad_input("expected a string 'to'"))?;
    let payload = match builtin {
        BuiltinTool::SendMessage => {
            let body = args.get("body").and_then(Value::as_str).ok_or_else(|| bad_input("expected a string 'body'"))?;
            SendPayload::Message(body.into())
        }
        BuiltinTool::SendDirective => {
            let blocks = args
                .get("context")
                .and_then(Value::as_array)
                .filter(|items| items.iter().all(Value::is_object))
                .ok_or_else(|| bad_input("expected 'context' to be an array of blocks"))?;
            let context = blocks
                .iter()
                .map(Block::deserialize)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| ErrorBody::new(codes::BAD_INPUT, format!("invalid 'context' block: {e}")))?;
            SendPayload::Directive(context)
        }
    };

    Ok((to, payload))
}

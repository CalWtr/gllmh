//! Loopback, non-streaming LLM fixture for valet tests (spec §§ 2, 6.2, 9).
//!
//! A script is a JSON array. For example:
//! ```json
//! [{"match":{"model_id":"fake","contains":"hello"},
//!   "repeat":true,"response":{"type":"message","text":"hi"}}]
//! ```

use std::io;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use ulid::Ulid;

pub type Script = Vec<Rule>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Rule {
    #[serde(default, rename = "match")]
    pub matcher: Matcher,
    /// A rule is consumed on selection unless this is true.
    #[serde(default)]
    pub repeat: bool,
    #[serde(default)]
    pub delay_ms: u64,
    pub response: Reply,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Matcher {
    pub model_id: Option<String>,
    /// Case-sensitive substring of the latest user or tool result message.
    pub contains: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Message {
        text: Option<String>,
        thinking: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
    },
    Error {
        status: u16,
        message: String,
        /// Retry-After header in seconds.
        retry_after: Option<u64>,
    },
}

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

#[derive(Default)]
struct ServerState {
    script: Script,
    requests: Vec<RecordedRequest>,
}

pub struct FakeLlm {
    base_url: String,
    state: Arc<Mutex<ServerState>>,
    // Dropping the sender stops accepting connections.
    _shutdown: oneshot::Sender<()>,
}

impl FakeLlm {
    pub async fn start(script: Script) -> io::Result<Self> {
        Self::start_on_port(script, 0).await
    }

    pub async fn start_on_port(script: Script, port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let state = Arc::new(Mutex::new(ServerState { script, ..ServerState::default() }));
        let app = Router::new()
            .route("/v1/messages", post(handle))
            .route("/chat/completions", post(handle))
            .route("/v1/chat/completions", post(handle))
            .with_state(Arc::clone(&state));
        let (shutdown, stopped) = oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve fake LLM");
        });
        Ok(Self { base_url, state, _shutdown: shutdown })
    }

    /// Loopback origin, with no API path suffix.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Snapshot in rule-selection order, independent of response delays.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn replace_script(&self, script: Script) {
        self.state.lock().unwrap().script = script;
    }

    pub fn extend_script(&self, rules: impl IntoIterator<Item = Rule>) {
        self.state.lock().unwrap().script.extend(rules);
    }
}

fn content_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }

    let pieces: Vec<_> = content
        .as_array()?
        .iter()
        .filter_map(|block| match block["type"].as_str()? {
            "text" => block["text"].as_str().map(str::to_owned),
            "tool_result" => content_text(&block["content"]),
            _ => None,
        })
        .collect();
    (!pieces.is_empty()).then(|| pieces.join("\n"))
}

fn last_input_text(body: &Value) -> Option<String> {
    let last_input = body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| matches!(message["role"].as_str(), Some("user" | "tool")))?;
    content_text(&last_input["content"])
}

async fn handle(
    State(state): State<Arc<Mutex<ServerState>>>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let path = uri.path();
    let provider = if path == "/v1/messages" { Provider::Anthropic } else { Provider::OpenAi };
    let model = body["model"].as_str();
    let input = last_input_text(&body);
    let streaming = body["stream"].as_bool() == Some(true);
    let rule = {
        let mut state = state.lock().unwrap();
        state.requests.push(RecordedRequest { path: path.to_owned(), headers, body: body.clone() });
        if streaming {
            return provider.error(StatusCode::BAD_REQUEST, "fake LLM only supports non-streaming requests", None);
        }
        state
            .script
            .iter()
            .position(|rule| {
                rule.matcher.model_id.as_deref().is_none_or(|id| Some(id) == model)
                    && rule
                        .matcher
                        .contains
                        .as_deref()
                        .is_none_or(|needle| input.as_deref().is_some_and(|text| text.contains(needle)))
            })
            .map(
                |index| {
                    if state.script[index].repeat { state.script[index].clone() } else { state.script.remove(index) }
                },
            )
    };

    let Some(rule) = rule else {
        return provider.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("no fake LLM rule matched POST {path}: {body}"),
            None,
        );
    };

    if rule.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(rule.delay_ms)).await;
    }
    match rule.response {
        Reply::Message { text, thinking, tool_calls } => {
            provider.message(model.unwrap_or("fake"), text, thinking, tool_calls)
        }
        Reply::Error { status, message, retry_after } => {
            let status =
                StatusCode::from_u16(status).ok().filter(|status| status.is_client_error() || status.is_server_error());
            match status {
                Some(status) => provider.error(status, &message, retry_after),
                None => provider.error(StatusCode::INTERNAL_SERVER_ERROR, "invalid fake LLM error status", None),
            }
        }
    }
}

enum Provider {
    Anthropic,
    OpenAi,
}

impl Provider {
    fn message(
        self,
        model: &str,
        text: Option<String>,
        thinking: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Response {
        let id = Ulid::new();
        let has_tools = !tool_calls.is_empty();
        let body = match self {
            Self::Anthropic => {
                let mut content = Vec::new();
                if let Some(thinking) = thinking {
                    content.push(json!({"type": "thinking", "thinking": thinking, "signature": "fake-signature"}));
                }
                if let Some(text) = text {
                    content.push(json!({"type": "text", "text": text}));
                }
                content.extend(tool_calls.into_iter().map(|call| {
                    json!({
                        "type": "tool_use", "id": format!("toolu_{}", Ulid::new()),
                        "name": call.name, "input": call.arguments
                    })
                }));
                json!({
                    "id": format!("msg_{id}"), "model": model, "type": "message", "role": "assistant",
                    "content": content, "stop_reason": if has_tools { "tool_use" } else { "end_turn" },
                    "stop_sequence": null, "usage": {"input_tokens": 1, "output_tokens": 1}
                })
            }
            Self::OpenAi => {
                let mut message = json!({"role": "assistant", "content": text, "refusal": null});
                if let Some(thinking) = thinking {
                    message["reasoning_content"] = json!(thinking);
                }
                if has_tools {
                    message["tool_calls"] = Value::Array(
                        tool_calls
                            .into_iter()
                            .map(|call| {
                                json!({
                                    "id": format!("call_{}", Ulid::new()), "type": "function",
                                    "function": {"name": call.name, "arguments": call.arguments.to_string()}
                                })
                            })
                            .collect(),
                    );
                }
                json!({
                    "id": format!("chatcmpl-{id}"), "model": model, "object": "chat.completion",
                    "created": chrono::Utc::now().timestamp(),
                    "choices": [{"index": 0, "message": message, "logprobs": null,
                                 "finish_reason": if has_tools { "tool_calls" } else { "stop" }}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
            }
        };
        Json(body).into_response()
    }

    fn error(self, status: StatusCode, message: &str, retry_after: Option<u64>) -> Response {
        let body = match self {
            Self::Anthropic => {
                let kind = match status.as_u16() {
                    401 => "authentication_error",
                    403 => "permission_error",
                    404 => "not_found_error",
                    413 => "request_too_large",
                    429 => "rate_limit_error",
                    504 => "timeout_error",
                    529 => "overloaded_error",
                    _ if status.is_client_error() => "invalid_request_error",
                    _ => "api_error",
                };
                json!({"type": "error", "error": {"type": kind, "message": message}})
            }
            Self::OpenAi => {
                let kind = match status.as_u16() {
                    429 => "rate_limit_error",
                    _ if status.is_client_error() => "invalid_request_error",
                    _ => "server_error",
                };
                json!({"error": {"type": kind, "message": message, "param": null, "code": null}})
            }
        };
        let mut response = (status, Json(body)).into_response();
        if let Some(seconds) = retry_after {
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

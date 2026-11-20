mod transport;

use serde_json::{Value, json};

use super::*;
use crate::protocol::composer::{BlockType, ComposeOutput, Role};

const ANTHROPIC_RESPONSE: &str = include_str!("anthropic_response.json");
const OPENAI_RESPONSE: &str = include_str!("openai_response.json");

fn config(protocol: Protocol) -> LlmConfig {
    serde_json::from_value(json!({
        "name": "test",
        "protocol": match protocol {
            Protocol::Anthropic => "anthropic",
            Protocol::Openai => "openai",
        },
        "model_id": "test-model",
        "base_url": "http://127.0.0.1:0",
        "thinking": {"native": "optional", "on": true, "returned": "full", "replay": "required"},
        "context_window": 16_384,
        "max_output": 4_096,
        "tuning": [],
    }))
    .unwrap()
}

fn tools() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "lookup".into(),
        description: "Find a term".into(),
        input_schema: json!({"type":"object","properties":{"term":{"type":"string"}}}),
    }]
}

fn conversation(assistant: Vec<ContentBlock>) -> Vec<Message> {
    vec![
        Message::text(Role::System, "Guide."),
        Message::text(Role::System, "Be concise."),
        Message::text(Role::User, "Find snow and ice."),
        Message::new(Role::Assistant, assistant),
        Message::new(Role::ToolResult, vec![ContentBlock::tool_result("call_1", "snow found", false)]),
        Message::new(Role::ToolResult, vec![ContentBlock::tool_result("call_2", "lookup denied", true)]),
    ]
}

fn expected_tool_calls() -> Vec<ContentBlock> {
    vec![
        ContentBlock::text("Looking up the answer"),
        ContentBlock::tool_use("call_1", "lookup", json!({"term":"snow"})),
        ContentBlock::tool_use("call_2", "lookup", json!({"term":"ice"})),
    ]
}

#[tokio::test]
async fn anthropic_fixture_round_trips_thinking_through_composer_peer() {
    use crate::frame::Peer;
    use crate::protocol::composer::{COMPOSE, ComposeInput};

    let response = parse_response(Protocol::Anthropic, ANTHROPIC_RESPONSE).unwrap();
    assert_eq!(response.raw, ANTHROPIC_RESPONSE);
    assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(response.usage, Some(Usage { input_tokens: 25, output_tokens: 4 }));
    assert_eq!(&response.content[2..], expected_tool_calls());
    assert_eq!(response.content[0].kind, BlockType::Thinking);
    assert_eq!(response.content[0].raw.as_ref().unwrap()["thinking"], "Check ☃");
    assert_eq!(response.content[1].kind, BlockType::Thinking);
    assert_eq!(response.content[1].raw.as_ref().unwrap()["data"], "encrypted");

    let (a, b) = tokio::io::duplex(4096);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let (valet, _incoming) = Peer::spawn(ar, aw);
    let (_composer, mut incoming) = Peer::spawn(br, bw);
    let compose = tokio::spawn(async move {
        let request = incoming.next().await.unwrap();
        assert_eq!(request.op, COMPOSE);
        let (_, reply) = request.parse::<ComposeInput>().unwrap();
        reply.ok(ComposeOutput {
            llm_profile: "test".into(),
            messages: conversation(response.content),
            tools: vec!["lookup".into()],
            thinking: Some(true),
            marks: Vec::new(),
            stop: None,
        });
    });
    let reply: ComposeOutput = valet.request(COMPOSE, &ComposeInput { turn: ulid::Ulid::new() }).await.unwrap();
    compose.await.unwrap();

    let body = build_request(&config(Protocol::Anthropic), &reply.messages, &tools(), true).unwrap();
    let sent: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        sent,
        json!({
            "model":"test-model",
            "max_tokens":4096,
            "system":[{"type":"text","text":"Guide."},{"type":"text","text":"Be concise."}],
            "thinking":{"type":"enabled","budget_tokens":2048},
            "messages":[
                {"role":"user","content":[{"type":"text","text":"Find snow and ice."}]},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"Check ☃","signature":"sig"},
                    {"type":"redacted_thinking","data":"encrypted"},
                    {"type":"text","text":"Looking up the answer"},
                    {"type":"tool_use","id":"call_1","name":"lookup","input":{"term":"snow"}},
                    {"type":"tool_use","id":"call_2","name":"lookup","input":{"term":"ice"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"snow found","is_error":false},
                    {"type":"tool_result","tool_use_id":"call_2","content":"lookup denied","is_error":true}
                ]}
            ],
            "tools": tools(),
        })
    );
    let thinking = r#"{ "signature" : "sig", "thinking": "Check \u2603",  "type": "thinking"
    }"#;
    let redacted = r#"{"data": "encrypted",   "type" : "redacted_thinking"}"#;
    assert!(body.contains(&format!("[{thinking},{redacted},")));
}

#[test]
fn openai_fixture_round_trips_messages_and_reasoning() {
    let response = parse_response(Protocol::Openai, OPENAI_RESPONSE).unwrap();
    assert_eq!(response.raw, OPENAI_RESPONSE);
    assert_eq!(response.stop_reason.as_deref(), Some("tool_calls"));
    assert_eq!(response.usage, Some(Usage { input_tokens: 17, output_tokens: 4 }));
    assert_eq!(response.content[0].kind, BlockType::Thinking);
    assert_eq!(response.content[0].raw, Some(json!("Check ☃")));
    assert_eq!(&response.content[1..], expected_tool_calls());

    let config = config(Protocol::Openai);
    let messages = conversation(response.content);
    let enabled = build_request(&config, &messages, &tools(), true).unwrap();
    let sent: Value = serde_json::from_str(&enabled).unwrap();
    assert_eq!(
        sent,
        json!({
            "model":"test-model",
            "max_completion_tokens":4096,
            "reasoning_effort":"medium",
            "messages":[
                {"role":"system","content":"Guide."},
                {"role":"system","content":"Be concise."},
                {"role":"user","content":"Find snow and ice."},
                {"role":"assistant","content":"Looking up the answer","reasoning_content":"Check ☃","tool_calls":[
                    {"type":"function","id":"call_1","function":{"name":"lookup","arguments":"{\"term\":\"snow\"}"}},
                    {"type":"function","id":"call_2","function":{"name":"lookup","arguments":"{\"term\":\"ice\"}"}}
                ]},
                {"role":"tool","tool_call_id":"call_1","content":"snow found"},
                {"role":"tool","tool_call_id":"call_2","content":"[tool error]\nlookup denied"}
            ],
            "tools":[{"type":"function","function":{
                "name":"lookup","description":"Find a term",
                "parameters":{"type":"object","properties":{"term":{"type":"string"}}}
            }}]
        })
    );
    assert!(enabled.contains(r#""reasoning_content":"Check \u2603""#));

    let disabled = build_request(&config, &messages, &[], false).unwrap();
    let disabled: Value = serde_json::from_str(&disabled).unwrap();
    assert!(disabled.get("reasoning_effort").is_none());
    assert!(disabled.get("tools").is_none());
    assert_eq!(disabled["messages"][3]["reasoning_content"], "Check ☃");
}

#[test]
fn anthropic_thinking_respects_output_limit_and_default_flag() {
    let mut config = config(Protocol::Anthropic);
    assert_eq!(config.timeout_s, 300);
    assert_eq!(config.max_retries, 3);
    assert_eq!(config.thinking.native, ThinkingNative::Optional);
    assert_eq!(config.thinking.returned, ThinkingReturned::Full);
    assert_eq!(config.thinking.replay, ThinkingReplay::Required);
    assert!(config.thinking.on);

    config.max_output = 1025;
    let sent: Value = serde_json::from_str(&build_request(&config, &[], &[], true).unwrap()).unwrap();
    assert_eq!(sent["thinking"]["budget_tokens"], 1024);

    config.max_output = 1024;
    assert_eq!(build_request(&config, &[], &[], true).unwrap_err().kind, ErrorKind::BadRequest);
    let disabled: Value = serde_json::from_str(&build_request(&config, &[], &[], false).unwrap()).unwrap();
    assert!(disabled.get("thinking").is_none());
}

#[test]
fn thinking_replay_supports_legacy_raw_and_detects_mismatched_bytes() {
    let config = config(Protocol::Anthropic);
    let mut block = ContentBlock::thinking(json!({"type":"thinking","thinking":"test","signature":"s"}));
    let messages = [Message::new(Role::Assistant, vec![block.clone()])];
    let request: Value = serde_json::from_str(&build_request(&config, &messages, &[], true).unwrap()).unwrap();
    assert_eq!(request["messages"][0]["content"][0], block.raw.clone().unwrap());

    block.raw_json = Some(r#"{"type":"thinking","thinking":"replaced","signature":"s"}"#.into());
    let error = build_request(&config, &[Message::new(Role::Assistant, vec![block])], &[], true).unwrap_err();
    assert_eq!(error.kind, ErrorKind::BadRequest);
    assert_eq!(error.attempts, 0);
}

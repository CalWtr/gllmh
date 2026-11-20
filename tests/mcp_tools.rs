//! MCP tool profiles through a dev-mode valet and the fake LLM.

mod support;

use std::path::Path;
use std::time::Duration;

use gllmh::protocol::broker::SendOutput;
use gllmh::protocol::local::{self, SubmitInput};
use gllmh::protocol::records::{LlmCallInput, ToolCallInput, ToolCallOutput};
use gllmh::types::{Action, ActionKind, ActionState, Hazardness, InteractionKind, TurnState};
use serde_json::{Value, json};
use support::dev::{Dev, eventually, write_config};
use support::fake_llm::{FakeLlm, Matcher, Reply, Rule, ToolCall};

fn server(mode: &str) -> String {
    let script = support::fixture("mcp_stdio.sh");
    format!("sh {} {}", shell_words::quote(script.to_str().unwrap()), shell_words::quote(mode))
}

async fn start(dev: &mut Dev, fake: &FakeLlm, cwd: Option<&Path>, mcp_profiles: &[(&str, &str, Value, u64)]) {
    dev.config = dev.dir.path().join("config");
    let mut user = json!({
        "name": "agent", "system_user": "agent", "composer_profile": "main", "llm_profiles": ["fake"],
        "tool_profiles": mcp_profiles.iter().map(|(name, ..)| name).collect::<Vec<_>>()
    });
    if let Some(cwd) = cwd {
        user["cwd"] = json!(cwd);
    }
    let mut profiles = vec![
        (
            "runtime",
            "mcp",
            json!({
                "name": "mcp", "users": ["alice", "agent"],
                "allow_messages": { "alice": ["agent"] }, "restart": "never"
            }),
        ),
        ("user", "alice", json!({ "name": "alice", "system_user": "alice" })),
        ("user", "agent", user),
        (
            "composer",
            "main",
            json!({
                "name": "main", "engine": "builtin", "work_mode": "react",
                "max_steps_per_turn": 4, "builtin": { "mode": "message" }
            }),
        ),
        (
            "llm",
            "fake",
            json!({
                "name": "fake", "protocol": "anthropic", "model_id": "fixture", "base_url": fake.base_url(),
                "thinking": { "native": "never", "on": false, "returned": "none", "replay": "never" },
                "context_window": 100000, "max_output": 1024, "tuning": [], "timeout_s": 10, "max_retries": 0
            }),
        ),
    ];
    profiles.extend(mcp_profiles.iter().map(|(name, mode, tools, timeout_s)| {
        (
            "tool",
            *name,
            json!({
                "name": name, "mcp": { "server": server(mode), "tools": tools },
                "hazardness": "caution", "timeout_s": timeout_s
            }),
        )
    }));
    write_config(&dev.config, &profiles);
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    dev.start_valet("agent").await;
}

async fn fake(calls: &[(&str, Value)]) -> FakeLlm {
    let reply = |text, tool_calls| Rule {
        matcher: Matcher::default(),
        repeat: false,
        delay_ms: 0,
        response: Reply::Message { text, thinking: None, tool_calls },
    };
    let calls = calls.iter().map(|(name, arguments)| ToolCall { name: (*name).into(), arguments: arguments.clone() });
    FakeLlm::start(vec![reply(None, calls.collect()), reply(Some("done".into()), vec![])]).await.unwrap()
}

async fn run_turn(dev: &Dev) -> (SendOutput, Vec<Action>) {
    let (ui, _incoming) = dev.client("alice").await;
    let sent: SendOutput = ui
        .request(
            local::SUBMIT,
            &SubmitInput {
                kind: InteractionKind::Message,
                to: "agent".into(),
                body: Some(json!("Use the MCP tools")),
                context: None,
                caused_by: None,
            },
        )
        .await
        .unwrap();
    let store = dev.store("agent");
    let state = eventually(Duration::from_secs(20), || {
        store.turn_by_trigger(sent.id).unwrap().map(|turn| turn.state).filter(|state| state.is_end())
    })
    .await;
    assert_eq!(state, TurnState::Done);
    let actions = store.actions_of_turn(sent.id).unwrap();
    (sent, actions)
}

fn assert_result(request: &Value, action: &Action, text: &str, is_error: bool) {
    let result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"].as_str() == action.call_id.as_deref())
        .expect("model sees a result for the recorded call ID");
    assert_eq!(result["content"], text);
    assert_eq!(result["is_error"].as_bool().unwrap_or_default(), is_error);
}

#[tokio::test]
async fn startup_lists_once_and_calls_replay_success_and_is_error() {
    let args = json!({ "text": "hello world" });
    let fake = fake(&[("fs__echo", args.clone()), ("fs__fail", json!({})), ("fs__dup", json!({}))]).await;
    let mut dev = Dev::new("/nonexistent", "mcp");
    let cwd = dev.dir.path().join("tools cwd");
    std::fs::create_dir(&cwd).unwrap();
    let trace = dev.dir.path().join("mcp.trace");
    dev.valet_env.push(("GLLMH_TEST_MCP_TRACE".into(), trace.display().to_string()));
    start(&mut dev, &fake, Some(&cwd), &[("fs", "valet fixture", json!("*"), 10)]).await;

    let startup = format!("list\t{}\t\n", cwd.display());
    assert_eq!(std::fs::read_to_string(&trace).unwrap(), startup);
    let (sent, actions) = run_turn(&dev).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let schema = json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] });
    assert_eq!(
        requests[0].body["tools"],
        json!([
            { "name": "fs__echo", "description": "Echo text", "input_schema": schema },
            { "name": "fs__fail", "description": "", "input_schema": { "type": "object" } }
        ])
    );
    let llm = actions.iter().find(|a| a.kind == ActionKind::LlmCall).unwrap();
    let llm_input: LlmCallInput = serde_json::from_value(llm.input.clone()).unwrap();
    assert_eq!(llm_input.tools, ["fs__echo", "fs__fail"]);
    let calls: Vec<_> = actions.iter().filter(|a| a.kind == ActionKind::ToolCall).collect();
    assert_eq!(calls.len(), 3);
    let echo_output = format!("hello world{}", json!({ "type": "image", "mimeType": "image/png", "data": "AA==" }));
    for (action, name, arguments, text, exit_code, state) in [
        (calls[0], "fs__echo", args, echo_output.as_str(), 0, ActionState::Succeeded),
        (calls[1], "fs__fail", json!({}), "fixture failure", 1, ActionState::Failed),
    ] {
        assert_eq!(action.state, state);
        assert_eq!(action.requested_by, Some(llm.id));
        assert_eq!(action.turn, Some(sent.id));
        assert_eq!(action.root, Some(sent.root));
        assert_eq!(action.step, Some(1));
        assert_eq!(action.hazardness, Some(Hazardness::Caution));
        let input: ToolCallInput = serde_json::from_value(action.input.clone()).unwrap();
        assert_eq!(input.tool, name);
        assert_eq!(input.arguments, arguments);
        let output: ToolCallOutput = serde_json::from_value(action.output.clone().unwrap()).unwrap();
        assert_eq!(output.exit_code, Some(exit_code));
        assert_eq!(output.tool_result.text, text);
        assert_eq!(output.tool_result.is_error, exit_code != 0);
        if exit_code != 0 {
            assert!(output.stderr.unwrap().contains("isError"));
        }
        assert_result(&requests[1].body, action, text, exit_code != 0);
    }
    assert_eq!(calls[2].state, ActionState::Denied);
    assert_result(&requests[1].body, calls[2], "denied", true);
    assert_eq!(
        std::fs::read_to_string(trace).unwrap(),
        format!("{startup}echo\t{}\t{}\nfail\t{}\t{}\n", cwd.display(), sent.id, cwd.display(), sent.id)
    );
}

#[tokio::test]
async fn filtering_and_failed_discovery_leave_unknown_calls_denied() {
    let fake =
        fake(&[("selected__fail", json!({})), ("offline__echo", json!({})), ("unresponsive__echo", json!({}))]).await;
    let mut dev = Dev::new("/nonexistent", "mcp");
    start(
        &mut dev,
        &fake,
        None,
        &[
            ("offline", "exit early", json!("*"), 10),
            ("unresponsive", "hang", json!("*"), 1),
            ("selected", "stdio fixture", json!(["echo", "missing"]), 10),
        ],
    )
    .await;
    let (_, actions) = run_turn(&dev).await;
    assert!(dev.valet_running("agent"));

    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].body["tools"],
        json!([{ "name": "selected__echo", "description": "Echo text", "input_schema": { "type": "object" } }])
    );
    let calls: Vec<_> = actions.iter().filter(|a| a.kind == ActionKind::ToolCall).collect();
    assert_eq!(calls.len(), 3);
    for (action, name) in calls.into_iter().zip(["selected__fail", "offline__echo", "unresponsive__echo"]) {
        let input: ToolCallInput = serde_json::from_value(action.input.clone()).unwrap();
        assert_eq!(input.tool, name);
        assert_eq!(action.state, ActionState::Denied);
        assert_eq!(action.hazardness, None);
        assert_eq!(action.output, None);
        assert_result(&requests[1].body, action, "denied", true);
    }
}

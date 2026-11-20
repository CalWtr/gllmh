//! Executable and send tools through a real dev-mode valet and the fake LLM.

mod support;

use std::path::Path;
use std::time::Duration;

use gllmh::frame;
use gllmh::protocol::broker::{self, SendInput, SendOutput};
use gllmh::protocol::local::{self, StatusInput, StatusOutput, SubmitInput};
use gllmh::protocol::records::{LlmCallOutput, SendFailure, ToolCallInput, ToolCallOutput};
use gllmh::protocol::tool;
use gllmh::types::{Action, ActionKind, ActionState, Hazardness, Interaction, InteractionKind, Sender, TurnState};
use serde_json::{Value, json};
use support::dev::{Dev, eventually, write_config, write_script};
use support::fake_llm::{FakeLlm, Matcher, Reply, Rule, ToolCall};

const T: Duration = Duration::from_secs(20);

fn config(dev: &Dev, base_url: &str, executables: &[(&str, &Path, &Path, u64)], directive: bool) {
    let mut tool_names = vec!["send_message", "send_directive"];
    tool_names.extend(executables.iter().map(|(name, ..)| *name));
    let agent = |name: &str, tool_names: &[&str]| {
        json!({
            "name": name, "system_user": name,
            "composer_profile": "main", "llm_profiles": ["fake"], "tool_profiles": tool_names
        })
    };
    let mut profiles = vec![
        (
            "runtime",
            "solo",
            json!({
                "name": "solo", "users": ["alice", "helper", "worker"],
                "allow_messages": { "alice": ["helper"], "helper": ["alice"] },
                "allow_directives": if directive { json!({ "helper": ["worker"] }) } else { json!({}) },
                "restart": "never"
            }),
        ),
        ("user", "alice", json!({ "name": "alice", "system_user": "alice" })),
        ("user", "helper", agent("helper", &tool_names)),
        ("user", "worker", agent("worker", &[])),
        (
            "composer",
            "main",
            json!({
                "name": "main", "engine": "builtin", "work_mode": "react",
                "max_steps_per_turn": 5, "builtin": { "mode": "message" }
            }),
        ),
        (
            "llm",
            "fake",
            json!({
                "name": "fake", "protocol": "anthropic", "model_id": "fake-model", "base_url": base_url,
                "thinking": { "native": "never", "on": false, "returned": "none", "replay": "never" },
                "context_window": 100000, "max_output": 1024, "tuning": [], "timeout_s": 10, "max_retries": 0
            }),
        ),
        ("tool", "send_message", json!({ "name": "send_message", "builtin": "send_message", "hazardness": "safe" })),
        (
            "tool",
            "send_directive",
            json!({ "name": "send_directive", "builtin": "send_directive", "hazardness": "caution" }),
        ),
    ];
    profiles.extend(executables.iter().map(|(name, bin, schema, timeout_s)| {
        (
            "tool",
            *name,
            json!({ "name": name, "tool_bin": bin, "tool_schema": schema, "timeout_s": timeout_s, "hazardness": "caution" }),
        )
    }));
    write_config(&dev.config, &profiles);
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall { name: name.into(), arguments }
}

async fn fake(calls: Vec<ToolCall>) -> FakeLlm {
    let reply = |text, tool_calls| Rule {
        matcher: Matcher::default(),
        repeat: false,
        delay_ms: 0,
        response: Reply::Message { text, thinking: None, tool_calls },
    };
    FakeLlm::start(vec![reply(None, calls), reply(Some("done".into()), vec![])]).await.unwrap()
}

async fn start(dev: &mut Dev) {
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    dev.start_valet("helper").await;
}

async fn trigger(dev: &Dev) -> SendOutput {
    let (ui, _incoming) = dev.client("alice").await;
    ui.request(
        local::SUBMIT,
        &SubmitInput {
            kind: InteractionKind::Message,
            to: "helper".into(),
            body: Some(json!("Use the offered tools")),
            context: None,
            caused_by: None,
        },
    )
    .await
    .unwrap()
}

async fn finished_calls(dev: &Dev, trigger: SendOutput) -> Vec<Action> {
    let store = dev.store("helper");
    let state =
        eventually(T, || store.turn_by_trigger(trigger.id).unwrap().map(|t| t.state).filter(|s| s.is_end())).await;
    assert_eq!(state, TurnState::Done);

    let actions = store.actions_of_turn(trigger.id).unwrap();
    let llm = actions.iter().find(|row| row.kind == ActionKind::LlmCall).unwrap();
    let response: LlmCallOutput = serde_json::from_value(llm.output.clone().unwrap()).unwrap();
    let calls: Vec<_> = actions.iter().filter(|row| row.call_id.is_some()).cloned().collect();
    let expected_ids: Vec<_> = response.content.iter().filter_map(|block| block.call_id.as_deref()).collect();
    assert_eq!(calls.iter().filter_map(|row| row.call_id.as_deref()).collect::<Vec<_>>(), expected_ids);
    for row in &calls {
        assert_eq!(row.requested_by, Some(llm.id));
        assert_eq!(row.turn, Some(trigger.id));
        assert_eq!(row.root, Some(trigger.root));
        assert_eq!(row.step, Some(1));
    }
    calls
}

fn result_text<'a>(request: &'a Value, row: &Action, is_error: bool) -> &'a str {
    let result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"].as_str() == row.call_id.as_deref())
        .expect("model sees a tool result for the call ID");
    assert_eq!(result["is_error"].as_bool().unwrap_or_default(), is_error);
    result["content"].as_str().unwrap()
}

#[tokio::test]
async fn executable_results_and_denials_are_recorded_and_replayed() {
    let args = json!({ "key": "two words" });
    let fake = fake(vec![
        call("lookup", args.clone()),
        call("fails", json!({})),
        call("slow", json!({})),
        call("missing_bin", json!({})),
        call("schema_name", json!({})),
    ])
    .await;
    let mut dev = Dev::new("/nonexistent", "solo");
    dev.config = dev.dir.path().join("config");
    let schema = dev.dir.path().join("schema.json");
    let description = "A schema loaded at valet startup";
    let input_schema = json!({ "type": "object", "properties": { "key": { "type": "string" } } });
    std::fs::write(
        &schema,
        json!({ "name": "schema_name", "description": description, "input": input_schema }).to_string(),
    )
    .unwrap();

    let cwd = dev.data_dir("helper");
    let lookup =
        write_script(dev.dir.path(), "echo.sh", "#!/bin/sh\nprintf '%s\n%s\n' \"$GLLMH_TURN\" \"$PWD\"\ncat\n");
    let fails =
        write_script(dev.dir.path(), "fails.sh", "#!/bin/sh\nprintf 'partial'\nprintf 'diagnostic' >&2\nexit 7\n");
    let slow = write_script(dev.dir.path(), "slow.sh", "#!/bin/sh\nexec sleep 30\n");
    let missing_bin = dev.dir.path().join("missing");
    config(
        &dev,
        fake.base_url(),
        &[
            ("lookup", &lookup, &schema, 5),
            ("fails", &fails, &schema, 5),
            ("slow", &slow, &schema, 1),
            ("missing_bin", &missing_bin, &schema, 5),
        ],
        false,
    );
    start(&mut dev).await;
    std::fs::remove_file(&schema).unwrap();

    let sent = trigger(&dev).await;
    let calls = finished_calls(&dev, sent).await;
    let [success, failed, timeout, missing, denied] = calls.as_slice() else { panic!("expected five calls") };
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let specs = requests[0].body["tools"].as_array().unwrap();
    assert_eq!(
        specs.iter().map(|s| s["name"].as_str().unwrap()).collect::<Vec<_>>(),
        ["send_message", "lookup", "fails", "slow", "missing_bin"],
    );
    assert_eq!(specs[1]["description"], description);
    assert_eq!(specs[1]["input_schema"], input_schema);
    for (row, name) in calls.iter().zip(["lookup", "fails", "slow", "missing_bin", "schema_name"]) {
        assert_eq!(row.kind, ActionKind::ToolCall);
        let input: ToolCallInput = serde_json::from_value(row.input.clone()).unwrap();
        assert_eq!(input.tool, name);
    }
    for row in [success, failed, timeout, missing] {
        assert_eq!(row.hazardness, Some(Hazardness::Caution));
    }

    let followup = &requests[1].body;
    let expected = format!("{}\n{}\n{args}", sent.id, cwd.display());
    assert_eq!(success.state, ActionState::Succeeded);
    assert_eq!(result_text(followup, success, false), expected);
    let output: ToolCallOutput = serde_json::from_value(success.output.clone().unwrap()).unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.tool_result.text, expected);
    assert_eq!(failed.state, ActionState::Failed);
    let output: ToolCallOutput = serde_json::from_value(failed.output.clone().unwrap()).unwrap();
    assert_eq!(output.exit_code, Some(7));
    assert_eq!(output.stderr.as_deref(), Some("diagnostic"));
    assert_eq!(result_text(followup, failed, true), "tool exited with code 7\npartial");
    assert_eq!(timeout.state, ActionState::Failed);
    assert!(result_text(followup, timeout, true).starts_with(tool::codes::TIMEOUT));
    assert_eq!(missing.state, ActionState::Failed);
    assert!(result_text(followup, missing, true).starts_with(tool::codes::NOT_EXECUTABLE));
    assert_eq!(denied.state, ActionState::Denied);
    assert_eq!(denied.hazardness, None);
    assert_eq!(denied.output, None);
    assert_eq!(result_text(followup, denied, true), "denied");
}

#[tokio::test]
async fn send_tools_use_hello_edges_and_the_recorded_send_path() {
    let context = json!([{ "name": "task", "content": "Check the fixture" }]);
    let fake = fake(vec![
        call("send_message", json!({ "to": "alice", "body": "tool delivery" })),
        call("send_message", json!({ "to": "worker", "body": "refused message" })),
        call("send_directive", json!({ "to": "worker", "context": context })),
        call("send_directive", json!({ "to": "worker", "context": [["task", "not an object"]] })),
    ])
    .await;
    let mut dev = Dev::new("/nonexistent", "solo");
    dev.config = dev.dir.path().join("config");
    config(&dev, fake.base_url(), &[], true);
    start(&mut dev).await;

    let sent = trigger(&dev).await;
    let calls = finished_calls(&dev, sent).await;
    let [message, refused, directive, malformed] = calls.as_slice() else { panic!("expected four calls") };
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let specs = requests[0].body["tools"].as_array().unwrap();
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0]["name"], "send_message");
    assert_eq!(specs[0]["input_schema"]["properties"]["to"]["enum"], json!(["alice"]));
    assert_eq!(specs[0]["input_schema"]["properties"]["body"]["type"], "string");
    assert_eq!(specs[0]["input_schema"]["required"], json!(["to", "body"]));
    assert_eq!(specs[1]["name"], "send_directive");
    assert_eq!(specs[1]["input_schema"]["properties"]["to"]["enum"], json!(["worker"]));
    let context_schema = &specs[1]["input_schema"]["properties"]["context"];
    assert_eq!(context_schema["type"], "array");
    assert_eq!(context_schema["items"]["properties"]["name"]["type"], "string");
    assert_eq!(context_schema["items"]["properties"]["content"]["type"], "string");
    assert_eq!(context_schema["items"]["required"], json!(["name", "content"]));
    assert_eq!(specs[1]["input_schema"]["required"], json!(["to", "context"]));

    for row in [message, refused, directive] {
        assert_eq!(row.kind, ActionKind::Send);
        assert_eq!(row.hazardness, None);
        let input: SendInput = serde_json::from_value(row.input.clone()).unwrap();
        assert_eq!(input.client_ref, row.id);
        assert_eq!(input.caused_by, Some(sent.id));
        assert!(!input.local);
    }
    assert_eq!(message.state, ActionState::Succeeded);
    let store = dev.store("alice");
    let received: Interaction = eventually(T, || {
        store.actions().unwrap().into_iter().find_map(|row| {
            (row.kind == ActionKind::Receive && row.interaction == message.interaction)
                .then(|| serde_json::from_value(row.input).unwrap())
        })
    })
    .await;
    assert_eq!(received.kind, InteractionKind::Message);
    assert_eq!(received.from, Sender::user("helper"));
    assert_eq!(received.to, "alice");
    assert_eq!(received.body, Some(json!("tool delivery")));
    assert_eq!(received.caused_by, Some(sent.id));
    assert_eq!(received.root, sent.root);
    assert_eq!(received.hops, sent.hops + 1);
    let followup = &requests[1].body;
    assert_eq!(result_text(followup, message, false), format!("sent message {} to alice", received.id));

    assert_eq!(refused.state, ActionState::Failed);
    assert!(refused.interaction.is_none());
    let failure: SendFailure = serde_json::from_value(refused.output.clone().unwrap()).unwrap();
    assert_eq!(failure.error.code, broker::codes::NOT_ALLOWED);
    assert!(result_text(followup, refused, true).starts_with(broker::codes::NOT_ALLOWED));

    assert_eq!(directive.state, ActionState::Succeeded);
    let input: SendInput = serde_json::from_value(directive.input.clone()).unwrap();
    assert_eq!(input.kind, InteractionKind::Directive);
    assert_eq!(serde_json::to_value(input.context.unwrap()).unwrap(), context);
    assert_eq!(
        result_text(followup, directive, false),
        format!("sent directive {} to worker", directive.interaction.unwrap())
    );

    assert_eq!(malformed.kind, ActionKind::ToolCall);
    assert_eq!(malformed.state, ActionState::Failed);
    assert_eq!(malformed.hazardness, Some(Hazardness::Caution));
    assert!(result_text(followup, malformed, true).starts_with(frame::codes::BAD_INPUT));

    let (ui, _incoming) = dev.client("alice").await;
    let status: StatusOutput = ui.request(local::STATUS, &StatusInput { root: sent.root }).await.unwrap();
    assert_eq!(status.interactions.len(), 3, "only the trigger and two successful sends reached the broker log");
}

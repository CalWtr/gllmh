//! M4.3: an agent's turns, end to end in dev mode, against the fake LLM.

mod support;

use std::path::Path;
use std::time::Duration;

use gllmh::protocol::broker::SendOutput;
use gllmh::protocol::local::{self, StatusInput, StatusOutput, SubmitInput};
use gllmh::types::{ActionKind, ActionState, InteractionKind, InteractionState, Outcome, TurnState};
use serde_json::{Value, json};
use support::dev::{Dev, eventually, eventually_async, write_config, write_script};
use support::fake_llm::{FakeLlm, Matcher, Reply, Rule, ToolCall};

const T: Duration = Duration::from_secs(20);

/// alice (no composer) and helper (builtin message-mode agent) in `dir`.
fn config(dir: &Path, base_url: &str, composer: Value) {
    write_config(
        dir,
        &[
            (
                "runtime",
                "solo",
                json!({
                    "name": "solo", "users": ["alice", "helper"],
                    "allow_messages": { "alice": ["helper"], "helper": ["alice"] },
                    "allow_directives": {}, "restart": "never"
                }),
            ),
            ("user", "alice", json!({ "name": "alice", "system_user": "alice" })),
            (
                "user",
                "helper",
                json!({
                    "name": "helper", "system_user": "helper",
                    "composer_profile": "main", "llm_profiles": ["fake"], "tool_profiles": []
                }),
            ),
            ("composer", "main", composer),
            (
                "llm",
                "fake",
                json!({
                    "name": "fake", "protocol": "anthropic", "model_id": "fake-model", "base_url": base_url,
                    "thinking": { "native": "never", "on": false, "returned": "none", "replay": "never" },
                    "context_window": 100000, "max_output": 1024, "tuning": [], "timeout_s": 10, "max_retries": 0
                }),
            ),
        ],
    );
}

fn builtin(max_steps: u32) -> Value {
    json!({
        "name": "main", "engine": "builtin", "work_mode": "react",
        "max_steps_per_turn": max_steps, "builtin": { "mode": "message" }
    })
}

fn text(t: &str) -> Rule {
    Rule {
        matcher: Matcher::default(),
        repeat: false,
        delay_ms: 0,
        response: Reply::Message { text: Some(t.into()), thinking: None, tool_calls: vec![] },
    }
}

async fn setup(script: Vec<Rule>, composer: Value) -> (Dev, FakeLlm) {
    let fake = FakeLlm::start(script).await.unwrap();
    let mut dev = Dev::new("/nonexistent", "solo");
    dev.config = dev.dir.path().join("config");
    config(&dev.config, fake.base_url(), composer);
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    dev.start_valet("helper").await;
    (dev, fake)
}

async fn say(dev: &Dev, text: &str) -> SendOutput {
    let (ui, _n) = dev.client("alice").await;
    let input = SubmitInput {
        kind: InteractionKind::Message,
        to: "helper".into(),
        body: Some(json!(text)),
        context: None,
        caused_by: None,
    };
    ui.request(local::SUBMIT, &input).await.unwrap()
}

async fn turn_state(dev: &Dev, trigger: ulid::Ulid) -> TurnState {
    let store = dev.store("helper");
    eventually(T, || store.turn_by_trigger(trigger).ok().flatten().map(|t| t.state).filter(|s| s.is_end())).await
}

async fn closed_outcome(dev: &Dev, root: ulid::Ulid) -> Outcome {
    let (ui, _n) = dev.client("alice").await;
    eventually_async(T, || async {
        let out: StatusOutput = ui.request(local::STATUS, &StatusInput { root }).await.ok()?;
        let row = out.interactions.into_iter().find(|r| r.id == root)?;
        (row.state == InteractionState::Closed).then_some(row.outcome?)
    })
    .await
}

#[tokio::test]
async fn an_agent_answers_through_the_model() {
    let (dev, fake) = setup(vec![text("hello alice")], builtin(5)).await;
    let sent = say(&dev, "hi helper").await;

    assert_eq!(turn_state(&dev, sent.id).await, TurnState::Done);
    assert_eq!(closed_outcome(&dev, sent.id).await, Outcome::Done);

    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.to_string().contains("hi helper"), "the model saw the message");
    let llm = dev.store("helper").actions().unwrap().into_iter().find(|a| a.kind == ActionKind::LlmCall).unwrap();
    assert_eq!(llm.state, ActionState::Succeeded);
    assert_eq!(llm.turn, Some(sent.id));
    assert_eq!(llm.step, Some(1));
    assert!(llm.output.unwrap().to_string().contains("hello alice"));
}

#[tokio::test]
async fn the_step_limit_cuts_a_turn_off() {
    let looping = Rule {
        matcher: Matcher::default(),
        repeat: true,
        delay_ms: 0,
        response: Reply::Message {
            text: None,
            thinking: None,
            tool_calls: vec![ToolCall { name: "anything".into(), arguments: json!({}) }],
        },
    };
    let (dev, fake) = setup(vec![looping], builtin(3)).await;
    let sent = say(&dev, "loop forever").await;

    assert_eq!(turn_state(&dev, sent.id).await, TurnState::CutOff);
    assert_eq!(closed_outcome(&dev, sent.id).await, Outcome::CutOff);
    assert_eq!(fake.requests().len(), 3, "exactly max_steps_per_turn model calls");
    let denied = dev.store("helper").actions().unwrap().into_iter().filter(|a| a.state == ActionState::Denied).count();
    assert_eq!(denied, 3, "each tool request is recorded before it is refused");
}

#[tokio::test]
async fn a_non_retryable_provider_error_fails_the_turn() {
    let bad = Rule {
        matcher: Matcher::default(),
        repeat: true,
        delay_ms: 0,
        response: Reply::Error { status: 400, message: "bad request".into(), retry_after: None },
    };
    let (dev, fake) = setup(vec![bad], builtin(5)).await;
    let sent = say(&dev, "anything").await;

    assert_eq!(turn_state(&dev, sent.id).await, TurnState::Failed);
    assert_eq!(fake.requests().len(), 1);
    let llm = dev.store("helper").actions().unwrap().into_iter().find(|a| a.kind == ActionKind::LlmCall).unwrap();
    assert_eq!(llm.state, ActionState::Failed);
}

/// A custom composer that answers init, then fails every compose.
const FAILING_COMPOSER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/^{"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *'"op":"init"'*) printf '{"id":"r%s","re":"%s","output":{}}\n' "$id" "$id" ;;
    *) printf '{"id":"r%s","re":"%s","error":{"code":"invalid_reply","message":"no","retry":false}}\n' "$id" "$id" ;;
  esac
done
"#;

/// A custom composer that answers init, then exits.
const DYING_COMPOSER: &str = r#"#!/bin/sh
IFS= read -r line
id=$(printf '%s' "$line" | sed 's/^{"id":"\([^"]*\)".*/\1/')
printf '{"id":"r%s","re":"%s","output":{}}\n' "$id" "$id"
sleep 0.5
exit 3
"#;

fn custom(bin: &Path) -> Value {
    json!({
        "name": "main", "engine": "custom", "work_mode": "react", "custom_composer_bin": bin,
        "max_steps_per_turn": 5
    })
}

#[tokio::test]
async fn two_compose_failures_in_a_row_fail_the_turn() {
    let scripts = tempfile::tempdir().unwrap();
    let bin = write_script(scripts.path(), "failing.sh", FAILING_COMPOSER);
    let (dev, fake) = setup(vec![], custom(&bin)).await;
    let sent = say(&dev, "anything").await;

    assert_eq!(turn_state(&dev, sent.id).await, TurnState::Failed);
    assert_eq!(closed_outcome(&dev, sent.id).await, Outcome::Failed);
    assert!(fake.requests().is_empty(), "no model call without a composed input");
}

#[tokio::test]
async fn the_valet_exits_with_its_composer() {
    let scripts = tempfile::tempdir().unwrap();
    let bin = write_script(scripts.path(), "dying.sh", DYING_COMPOSER);
    let (mut dev, _fake) = setup(vec![], custom(&bin)).await;
    eventually(T, || (!dev.valet_running("helper")).then_some(())).await;
}

#[tokio::test]
async fn a_restart_mid_turn_resumes_it() {
    let slow = Rule {
        matcher: Matcher::default(),
        repeat: false,
        delay_ms: 3000,
        response: Reply::Message { text: Some("too late".into()), thinking: None, tool_calls: vec![] },
    };
    let (mut dev, fake) = setup(vec![slow, text("after restart")], builtin(5)).await;
    let sent = say(&dev, "take your time").await;

    // Kill the valet while its model call is in flight.
    eventually(T, || (!fake.requests().is_empty()).then_some(())).await;
    dev.kill_valet("helper").await;
    dev.start_valet("helper").await;

    assert_eq!(turn_state(&dev, sent.id).await, TurnState::Done);
    let actions = dev.store("helper").actions().unwrap();
    let llm: Vec<_> = actions.iter().filter(|a| a.kind == ActionKind::LlmCall).collect();
    assert_eq!(llm.len(), 2);
    assert_eq!(llm[0].state, ActionState::Interrupted);
    assert_eq!(llm[1].state, ActionState::Succeeded);
    assert!(llm[1].output.as_ref().unwrap().to_string().contains("after restart"));
}

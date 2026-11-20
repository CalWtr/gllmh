//! M6.1 directive turns and results in dev mode.

mod support;

use std::time::Duration;

use gllmh::broker::log::InteractionLog;
use gllmh::frame::{Incoming, Peer};
use gllmh::protocol::broker::{SendInput, SendOutput, StatusRow};
use gllmh::protocol::local::{self, NotifyInput, NotifyOutput, StatusInput, StatusOutput, SubmitInput};
use gllmh::store::{ActionRequest, Store};
use gllmh::types::{
    Action, ActionId, ActionKind, ActionState, Block, Interaction, InteractionId, InteractionKind, InteractionState,
    Outcome, Sender, TurnOutcome, TurnState,
};
use serde_json::{Value, json};
use support::dev::{Dev, eventually, eventually_async, write_config, write_script};
use support::fake_llm::{FakeLlm, Matcher, Reply, Rule, ToolCall};

const T: Duration = Duration::from_secs(20);

async fn configured(script: Vec<Rule>, worker: Value) -> (Dev, FakeLlm) {
    let fake = FakeLlm::start(script).await.unwrap();
    let mut dev = Dev::new("/nonexistent", "directives");
    dev.config = dev.dir.path().join("config");
    write_config(
        &dev.config,
        &[
            (
                "runtime",
                "directives",
                json!({
                    "name": "directives", "users": ["alice", "worker", "director"],
                    "allow_directives": {"alice": ["worker"], "director": ["worker"]},
                    "max_hops": 0, "restart": "never"
                }),
            ),
            ("user", "alice", json!({"name": "alice", "system_user": "alice"})),
            (
                "user",
                "worker",
                json!({
                    "name": "worker", "system_user": "worker",
                    "composer_profile": "worker", "llm_profiles": ["fake"], "tool_profiles": []
                }),
            ),
            (
                "user",
                "director",
                json!({
                    "name": "director", "system_user": "director",
                    "composer_profile": "director", "llm_profiles": ["fake"], "tool_profiles": []
                }),
            ),
            ("composer", "worker", worker),
            (
                "composer",
                "director",
                json!({"name": "director", "engine": "builtin", "work_mode": "react", "builtin": {"mode": "message"}}),
            ),
            (
                "llm",
                "fake",
                json!({
                    "name": "fake", "protocol": "anthropic", "model_id": "fake-model", "base_url": fake.base_url(),
                    "thinking": {"native": "never", "on": false, "returned": "none", "replay": "never"},
                    "context_window": 100000, "max_output": 1024, "tuning": [], "timeout_s": 10, "max_retries": 0
                }),
            ),
        ],
    );
    (dev, fake)
}

fn worker(max_steps: u32) -> Value {
    json!({
        "name": "worker", "engine": "builtin", "work_mode": "react", "builtin": {"mode": "directive"},
        "max_steps_per_turn": max_steps, "max_parallel_turns": 2
    })
}

fn text(reply: &str, delay_ms: u64) -> Rule {
    Rule {
        matcher: Matcher::default(),
        repeat: false,
        delay_ms,
        response: Reply::Message { text: Some(reply.into()), thinking: None, tool_calls: vec![] },
    }
}

async fn direct(ui: &Peer, task: &str) -> SendOutput {
    ui.request(
        local::SUBMIT,
        &SubmitInput {
            kind: InteractionKind::Directive,
            to: "worker".into(),
            body: None,
            context: Some(vec![Block { name: "task".into(), content: task.into() }]),
            caused_by: None,
        },
    )
    .await
    .unwrap()
}

async fn notified(notes: &mut Incoming) -> Interaction {
    let note = tokio::time::timeout(T, notes.next()).await.unwrap().unwrap();
    assert_eq!(note.op, local::NOTIFY);
    let (NotifyInput { interaction }, responder) = note.parse::<NotifyInput>().unwrap();
    responder.ok(NotifyOutput {});
    interaction
}

async fn closed(ui: &Peer, directive: &SendOutput, outcome: TurnOutcome, result_outcome: Outcome) -> StatusRow {
    let rows = eventually_async(T, || async {
        let status: StatusOutput = ui.request(local::STATUS, &StatusInput { root: directive.root }).await.unwrap();
        let rows = status.interactions;
        (rows.len() >= 2 && rows.iter().all(|row| row.state == InteractionState::Closed)).then_some(rows)
    })
    .await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, directive.id);
    assert_eq!(rows[0].outcome, Some(outcome.into()));
    assert_eq!(rows[1].kind, InteractionKind::Result);
    assert_eq!(rows[1].hops, directive.hops + 1);
    assert_eq!(rows[1].outcome, Some(result_outcome));
    // Broker timestamps also establish result acceptance before directive close.
    assert!(rows[1].accepted_at <= rows[0].closed_at.unwrap());
    rows.into_iter().nth(1).unwrap()
}

fn check_result(result: &Interaction, directive: &SendOutput, owner: &str, outcome: TurnOutcome, body: Option<&str>) {
    assert_eq!(result.kind, InteractionKind::Result);
    assert_eq!(result.caused_by, Some(directive.id));
    assert_eq!(result.turn_outcome, Some(outcome));
    assert_eq!(result.body, body.map(|text| json!(text)));
    assert_eq!(result.from, Sender::user("worker"));
    assert_eq!(result.to, owner);
    assert_eq!(result.root, directive.root);
    assert_eq!(result.hops, directive.hops + 1);
}

fn result_sends(dev: &Dev, trigger: InteractionId) -> Vec<Action> {
    dev.store("worker")
        .actions_of_turn(trigger)
        .unwrap()
        .into_iter()
        .filter(|action| action.kind == ActionKind::Send)
        .collect()
}

async fn start_pair(dev: &mut Dev) {
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    dev.start_valet("worker").await;
}

#[tokio::test]
async fn user_director_is_notified_and_both_interactions_close() {
    let (mut dev, fake) = configured(vec![text("finished the draft", 0)], worker(5)).await;
    start_pair(&mut dev).await;
    let (ui, mut notes) = dev.client("alice").await;
    let directive = direct(&ui, "draft a letter").await;
    let received = notified(&mut notes).await;
    check_result(&received, &directive, "alice", TurnOutcome::Done, Some("finished the draft"));
    let result = closed(&ui, &directive, TurnOutcome::Done, Outcome::Received).await;
    assert_eq!(result.id, received.id);
    assert!(dev.store("alice").turn_by_trigger(result.id).unwrap().is_none());
    let sends = result_sends(&dev, directive.id);
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].interaction, Some(result.id));
    assert_eq!(sends[0].root, Some(directive.root));
    assert_eq!(sends[0].state, ActionState::Succeeded);
    assert_eq!(fake.requests().len(), 1);
    assert!(fake.requests()[0].body.to_string().contains("draft a letter"));
}

#[tokio::test]
async fn agent_director_opens_a_turn_for_the_result() {
    let (mut dev, fake) = configured(vec![text("work complete", 0), text("result seen", 0)], worker(5)).await;
    dev.start_broker("director").await;
    dev.start_valet("director").await;
    dev.start_valet("worker").await;
    let (ui, mut notes) = dev.client("director").await;
    let directive = direct(&ui, "finish work").await;
    let received = notified(&mut notes).await;
    check_result(&received, &directive, "director", TurnOutcome::Done, Some("work complete"));
    let result = closed(&ui, &directive, TurnOutcome::Done, Outcome::Done).await;
    assert_eq!(result.id, received.id);
    let turn = dev.store("director").turn_by_trigger(result.id).unwrap().unwrap();
    assert_eq!(turn.state, TurnState::Done);
    assert_eq!(turn.root, directive.root);
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.to_string().contains("[result from worker: done]"));
    assert!(requests[1].body.to_string().contains("work complete"));
}

#[tokio::test]
async fn two_directives_overlap_and_the_third_waits_for_a_slot() {
    let slow = |reply: &str| {
        let mut rule = text(reply, 1500);
        rule.matcher.contains = Some(reply.into());
        rule
    };
    let (mut dev, fake) = configured(vec![slow("first"), slow("second"), text("third", 0)], worker(5)).await;
    start_pair(&mut dev).await;
    let (ui, _notes) = dev.client("alice").await;
    let first = direct(&ui, "first").await;
    let second = direct(&ui, "second").await;
    let third = direct(&ui, "third").await;
    let store = dev.store("worker");

    eventually(T, || {
        let state = |id| store.turn_by_trigger(id).unwrap().map(|turn| turn.state);
        (fake.requests().len() == 2
            && state(first.id) == Some(TurnState::Running)
            && state(second.id) == Some(TurnState::Running)
            && state(third.id) == Some(TurnState::Queued))
        .then_some(())
    })
    .await;

    let mut calls = Vec::new();
    for directive in [&first, &second, &third] {
        closed(&ui, directive, TurnOutcome::Done, Outcome::Received).await;
        let call = store
            .actions_of_turn(directive.id)
            .unwrap()
            .into_iter()
            .find(|action| action.kind == ActionKind::LlmCall)
            .unwrap();
        calls.push(call);
    }
    let earliest_end = calls[0].ended_at.unwrap().min(calls[1].ended_at.unwrap());
    assert!(calls[0].requested_at.max(calls[1].requested_at) < earliest_end);
    assert!(calls[2].requested_at >= earliest_end);
    assert_eq!(fake.requests().len(), 3);
}

/// Answers init and records each compose before returning invalid_reply.
const FAILING_COMPOSER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/^{"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *'"op":"init"'*) printf '{"id":"r%s","re":"%s","output":{}}\n' "$id" "$id" ;;
    *) printf 'compose\n' >> "$0.calls"
       printf '{"id":"r%s","re":"%s","error":{"code":"invalid_reply","message":"no","retry":false}}\n' "$id" "$id" ;;
  esac
done
"#;

#[tokio::test]
async fn two_composer_failures_send_a_failed_result() {
    let scripts = tempfile::tempdir().unwrap();
    let bin = write_script(scripts.path(), "failing.sh", FAILING_COMPOSER);
    let custom = json!({"name": "worker", "engine": "custom", "work_mode": "react", "custom_composer_bin": bin});
    let (mut dev, fake) = configured(vec![], custom).await;
    start_pair(&mut dev).await;
    let (ui, mut notes) = dev.client("alice").await;
    let directive = direct(&ui, "compose this").await;
    check_result(&notified(&mut notes).await, &directive, "alice", TurnOutcome::Failed, None);
    closed(&ui, &directive, TurnOutcome::Failed, Outcome::Received).await;
    assert_eq!(std::fs::read_to_string(scripts.path().join("failing.sh.calls")).unwrap().lines().count(), 2);
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn the_step_limit_sends_a_cut_off_result() {
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
    let (mut dev, fake) = configured(vec![looping], worker(3)).await;
    start_pair(&mut dev).await;
    let (ui, mut notes) = dev.client("alice").await;
    let directive = direct(&ui, "keep going").await;
    check_result(&notified(&mut notes).await, &directive, "alice", TurnOutcome::CutOff, None);
    closed(&ui, &directive, TurnOutcome::CutOff, Outcome::Received).await;
    assert_eq!(fake.requests().len(), 3);
}

fn seed_running(dev: &Dev, acked: bool) -> (InteractionLog, Store, SendOutput) {
    let mut log = InteractionLog::open(dev.db()).unwrap();
    let input = SendInput {
        client_ref: ActionId::new(),
        kind: InteractionKind::Directive,
        to: "worker".into(),
        service: None,
        caused_by: None,
        body: None,
        context: Some(vec![Block { name: "task".into(), content: "recover me".into() }]),
        turn_outcome: None,
        local: true,
    };
    let accepted = log.accept(&Sender::user("alice"), &input).unwrap().interaction;
    log.mark_delivered(accepted.id).unwrap();
    let delivery = log.get(accepted.id).unwrap().unwrap();
    let mut store = Store::open(&dev.data_dir("worker")).unwrap();
    store.record_receive(&delivery, true).unwrap();
    store.start_turn(delivery.id).unwrap();
    if acked {
        log.ack(delivery.id).unwrap();
    }
    (log, store, SendOutput { id: delivery.id, root: delivery.root, hops: delivery.hops })
}

#[tokio::test]
async fn recovery_with_a_tool_call_in_flight_sends_an_interrupted_result() {
    let (mut dev, fake) = configured(vec![], worker(5)).await;
    let (log, mut store, directive) = seed_running(&dev, true);
    let mut request = ActionRequest::new(ActionKind::ToolCall, json!({"tool": "in-flight", "arguments": {}}));
    request.turn = Some(directive.id);
    request.root = Some(directive.root);
    let tool = store.request_action(request).unwrap();
    store.start_action(tool).unwrap();
    drop((log, store));

    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    let (ui, mut notes) = dev.client("alice").await;
    dev.start_valet("worker").await;

    check_result(&notified(&mut notes).await, &directive, "alice", TurnOutcome::Interrupted, None);
    closed(&ui, &directive, TurnOutcome::Interrupted, Outcome::Received).await;
    let actions = dev.store("worker").actions_of_turn(directive.id).unwrap();
    assert_eq!(actions.iter().find(|action| action.id == tool).unwrap().state, ActionState::Interrupted);
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn recovery_finishes_an_ended_directive_even_if_its_ack_was_lost() {
    let (mut dev, fake) = configured(vec![], worker(5)).await;
    let (log, mut store, directive) = seed_running(&dev, false);
    store.end_directive(directive.id, TurnOutcome::Done, Some("saved final text")).unwrap();
    drop((log, store));

    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    let (ui, mut notes) = dev.client("alice").await;
    dev.start_valet("worker").await;

    check_result(&notified(&mut notes).await, &directive, "alice", TurnOutcome::Done, Some("saved final text"));
    closed(&ui, &directive, TurnOutcome::Done, Outcome::Received).await;
    assert_eq!(result_sends(&dev, directive.id).len(), 1);
    assert!(fake.requests().is_empty());
}

async fn recover_accepted_result(recorded: bool) {
    let (mut dev, fake) = configured(vec![], worker(5)).await;
    let (mut log, mut store, directive) = seed_running(&dev, true);
    // Terminal row without a close checkpoint, as in an older store.
    store.end_turn(directive.id, TurnOutcome::Done).unwrap();
    let mut request = ActionRequest::new(ActionKind::Send, Value::Null);
    request.turn = Some(directive.id);
    request.root = Some(directive.root);
    let mut input = SendInput {
        client_ref: ActionId::nil(),
        kind: InteractionKind::Result,
        to: "alice".into(),
        service: None,
        caused_by: Some(directive.id),
        body: Some(json!("already sent")),
        context: None,
        turn_outcome: Some(TurnOutcome::Done),
        local: false,
    };
    let id = store
        .request_action_with(request, |id| {
            input.client_ref = id;
            Some(serde_json::to_value(&input).unwrap())
        })
        .unwrap();
    store.start_action(id).unwrap();
    let result = log.accept(&Sender::user("worker"), &input).unwrap().interaction;
    if recorded {
        store.succeed_send(id, &SendOutput { id: result.id, root: result.root, hops: result.hops }).unwrap();
    }
    drop((log, store));

    start_pair(&mut dev).await;
    let (ui, _notes) = dev.client("alice").await;
    assert_eq!(closed(&ui, &directive, TurnOutcome::Done, Outcome::Received).await.id, result.id);
    let sends = result_sends(&dev, directive.id);
    assert_eq!(sends.len(), 1, "recovery should not attempt to send the result again");
    assert_eq!(sends[0].state, ActionState::Succeeded);
    assert_eq!(sends[0].interaction, Some(result.id));
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn recovery_closes_a_directive_whose_result_send_already_succeeded() {
    recover_accepted_result(true).await;
}

#[tokio::test]
async fn recovery_settles_an_accepted_result_whose_send_was_still_running() {
    recover_accepted_result(false).await;
}

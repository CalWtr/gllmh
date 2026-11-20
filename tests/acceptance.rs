//! A2: spec section 9, both workflows, in dev mode with the fake LLM.
//!
//! The acceptance profile (tests/fixtures/acceptance) runs with its LLM
//! pointed at the fake provider and the real A1 services as service
//! processes. `user` watches through the real `gllmh attach`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gllmh::paths::{service_config, service_socket};
use gllmh::protocol::broker::StatusRow;
use gllmh::protocol::local::{self, StatusInput, StatusOutput};
use gllmh::protocol::tool::{ENV_SERVICE_CONFIG, ENV_SERVICE_SOCKET};
use gllmh::types::{Interaction, InteractionKind, InteractionState, Outcome, TurnOutcome, TurnState};
use serde_json::{Value, json};
use support::dev::{Dev, eventually, eventually_async};
use support::examples::example;
use support::fake_llm::{FakeLlm, Matcher, Reply, Rule, ToolCall};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use ulid::Ulid;

const T: Duration = Duration::from_secs(30);
const USERS: [&str; 4] = ["user", "agent", "worker_agent", "dispatch"];

fn rule(contains: &str, delay_ms: u64, response: Reply) -> Rule {
    Rule { matcher: Matcher { model_id: None, contains: Some(contains.into()) }, repeat: true, delay_ms, response }
}

fn say(text: &str) -> Reply {
    Reply::Message { text: Some(text.into()), thinking: None, tool_calls: vec![] }
}

fn call(name: &str, arguments: Value) -> Reply {
    Reply::Message { text: None, thinking: None, tool_calls: vec![ToolCall { name: name.into(), arguments }] }
}

/// The model's side of both workflows. Rules match the latest user or
/// tool_result message, so each step of each turn has its own trigger text.
fn script(worker_delay_ms: u64) -> Vec<Rule> {
    vec![
        // agent, turn 1: the timer's message → direct the worker.
        rule(
            "wake up",
            0,
            call(
                "send_directive",
                json!({ "to": "worker_agent", "context": [{ "name": "instructions", "content": "write the draft" }] }),
            ),
        ),
        rule("sent directive", 0, say("directed the worker")),
        // worker_agent: any directive carrying the instructions block.
        rule("write the draft", worker_delay_ms, say("the draft")),
        // agent, turn 2: the worker's result → tell the user.
        rule("[result from worker_agent", 0, call("send_message", json!({ "to": "user", "body": "draft ready" }))),
        rule("sent message", 0, say("told the user")),
    ]
}

struct Acceptance {
    dev: Dev,
    fake: FakeLlm,
    /// Kept alive for the test: dropping it kills `gllmh attach`.
    _attach: Child,
    attach_out: Arc<Mutex<String>>,
}

impl Acceptance {
    async fn start(worker_delay_ms: u64) -> Self {
        let fake = FakeLlm::start(script(worker_delay_ms)).await.unwrap();
        let mut dev = Dev::new("/nonexistent", "acceptance");
        dev.config = dev.dir.path().join("config");
        copy_dir(&support::fixture("acceptance"), &dev.config);
        patch(&dev.config.join("llm/anthropic.json"), |llm| {
            llm["base_url"] = json!(fake.base_url());
            llm.as_object_mut().unwrap().remove("api_key_file");
            llm["timeout_s"] = json!(20);
            llm["max_retries"] = json!(0);
        });
        patch(&dev.config.join("user/agent.json"), |u| {
            u["services"]["timer"]["config"] = json!({ "to": "agent", "body": "wake up", "delay_s": 0 });
        });
        let result_file = dev.dir.path().join("dispatcher-result.json");
        patch(&dev.config.join("user/dispatch.json"), |u| {
            u["services"]["draft_dispatcher"]["config"] = json!({
                "to": "worker_agent",
                "context": [{ "name": "instructions", "content": "write the draft" }],
                "result_file": result_file,
            });
        });

        dev.start_broker("user").await;
        for user in USERS {
            dev.start_valet(user).await;
        }

        let mut attach = Command::new(support::gllmh_bin())
            .arg("attach")
            .arg("--socket")
            .arg(dev.local_socket("user"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let attach_out = Arc::new(Mutex::new(String::new()));
        let mut lines = BufReader::new(attach.stdout.take().unwrap()).lines();
        let sink = attach_out.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let mut out = sink.lock().unwrap();
                out.push_str(&line);
                out.push('\n');
            }
        });
        Self { dev, fake, _attach: attach, attach_out }
    }

    fn service(&self, owner: &str, name: &str, bin: &str) -> Child {
        let data = self.dev.data_dir(owner);
        Command::new(example(bin))
            .env(ENV_SERVICE_SOCKET, service_socket(&data, name))
            .env(ENV_SERVICE_CONFIG, service_config(&data, name))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    /// Workflow 1 starts when the timer fires. Returns the run's root.
    async fn fire_timer(&self) -> Ulid {
        let mut timer = self.service("agent", "timer", "timer_service");
        let mut out = BufReader::new(timer.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(T, out.next_line()).await.unwrap().unwrap().expect("timer reply");
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["hops"], 0, "the timer's message starts the run");
        reply["root"].as_str().unwrap().parse().unwrap()
    }

    /// Workflow 2 starts when the dispatcher starts; it keeps running.
    fn start_dispatcher(&self) -> Child {
        self.service("dispatch", "draft_dispatcher", "draft_dispatcher")
    }

    async fn dispatcher_result(&self) -> Interaction {
        let path = self.dev.dir.path().join("dispatcher-result.json");
        eventually(T, || serde_json::from_slice(&std::fs::read(&path).ok()?).ok()).await
    }

    /// `gllmh status <root>`, through `user`'s or another participant's valet.
    async fn status(&self, via: &str, root: Ulid) -> Vec<StatusRow> {
        let (ui, _n) = self.dev.client(via).await;
        let out: StatusOutput = ui.request(local::STATUS, &StatusInput { root }).await.unwrap();
        out.interactions
    }

    /// Wait until the run has `n` interactions, all closed.
    async fn closed_run(&self, via: &str, root: Ulid, n: usize) -> Vec<StatusRow> {
        let (ui, _n) = self.dev.client(via).await;
        eventually_async(T, || async {
            let out: StatusOutput = ui.request(local::STATUS, &StatusInput { root }).await.ok()?;
            let rows = out.interactions;
            (rows.len() == n && rows.iter().all(|r| r.state == InteractionState::Closed)).then_some(rows)
        })
        .await
    }

    fn attach_saw(&self, text: &str) -> bool {
        self.attach_out.lock().unwrap().contains(text)
    }
}

/// Workflow 1 as section 9 states its pass condition.
fn check_workflow_1(rows: &[StatusRow], root: Ulid) {
    let shape: Vec<(InteractionKind, String, String, u32)> =
        rows.iter().map(|r| (r.kind, r.from.to_string(), r.to.clone(), r.hops)).collect();
    assert_eq!(
        shape,
        vec![
            (InteractionKind::Message, "agent::timer".into(), "agent".into(), 0),
            (InteractionKind::Directive, "agent".into(), "worker_agent".into(), 1),
            (InteractionKind::Result, "worker_agent".into(), "agent".into(), 2),
            (InteractionKind::Message, "agent".into(), "user".into(), 3),
        ]
    );
    assert_eq!(rows[0].id, root);
    assert!(rows.iter().all(|r| r.state == InteractionState::Closed));
    assert_eq!(rows[2].outcome, Some(Outcome::Done), "the result opened a turn on agent that ended done");
    assert_eq!(rows[3].outcome, Some(Outcome::Received));
}

/// Workflow 2: both interactions closed, and the service got its result.
fn check_workflow_2(rows: &[StatusRow], result: &Interaction) {
    let shape: Vec<(InteractionKind, String, String)> =
        rows.iter().map(|r| (r.kind, r.from.to_string(), r.to.clone())).collect();
    assert_eq!(
        shape,
        vec![
            (InteractionKind::Directive, "dispatch::draft_dispatcher".into(), "worker_agent".into()),
            (InteractionKind::Result, "worker_agent".into(), "dispatch".into()),
        ]
    );
    assert!(rows.iter().all(|r| r.state == InteractionState::Closed));
    assert_eq!(result.kind, InteractionKind::Result);
    assert_eq!(result.caused_by, Some(rows[0].id));
    assert_eq!(result.turn_outcome, Some(TurnOutcome::Done));
    assert_eq!(result.body, Some(json!("the draft")));
}

#[tokio::test]
async fn workflow_1_timer_to_agent_to_worker_and_back_to_user() {
    let a = Acceptance::start(0).await;
    let root = a.fire_timer().await;
    let rows = a.closed_run("user", root, 4).await;
    check_workflow_1(&rows, root);
    eventually(T, || a.attach_saw("draft ready").then_some(())).await;
    assert!(a.attach_out.lock().unwrap().contains("agent message"), "attach shows sender and kind");
}

#[tokio::test]
async fn workflow_2_service_directs_worker_and_gets_its_result() {
    let a = Acceptance::start(0).await;
    let _dispatcher = a.start_dispatcher();
    let result = a.dispatcher_result().await;
    let root = result.root;
    let rows = a.closed_run("dispatch", root, 2).await;
    check_workflow_2(&rows, &result);
    let turns = a.dev.store("dispatch").actions().unwrap();
    assert!(turns.iter().all(|a| a.turn.is_none()), "the result opened no turn on dispatch");
    assert_eq!(a.fake.requests().len(), 1, "one model call: the worker's");
}

#[tokio::test]
async fn both_workflows_together_run_the_worker_in_parallel() {
    // The worker's model call takes long enough for both directives to overlap.
    let a = Acceptance::start(1500).await;
    let (root_1, _dispatcher) = tokio::join!(a.fire_timer(), async { a.start_dispatcher() });
    let result = a.dispatcher_result().await;

    check_workflow_1(&a.closed_run("user", root_1, 4).await, root_1);
    check_workflow_2(&a.closed_run("dispatch", result.root, 2).await, &result);

    let turns = worker_turns(&a.dev);
    assert_eq!(turns.len(), 2);
    let (x, y) = (&turns[0], &turns[1]);
    assert!(x.0 < y.1 && y.0 < x.1, "worker_agent's two turns overlap in time: {turns:?}");
    check_invariants(&a.dev);
}

#[tokio::test]
async fn directors_get_their_results_across_a_worker_restart() {
    let mut a = Acceptance::start(2000).await;
    let timer = a.fire_timer();
    let (root_1, _dispatcher) = tokio::join!(timer, async { a.start_dispatcher() });

    // Kill worker_agent while both of its turns wait on the model.
    let store = a.dev.store("worker_agent");
    eventually(T, || {
        let running = store.actions().ok()?.iter().filter(|x| x.kind == gllmh::types::ActionKind::LlmCall).count();
        (running == 2).then_some(())
    })
    .await;
    a.dev.kill_valet("worker_agent").await;
    a.dev.start_valet("worker_agent").await;

    let result = a.dispatcher_result().await;
    check_workflow_2(&a.closed_run("dispatch", result.root, 2).await, &result);
    check_workflow_1(&a.closed_run("user", root_1, 4).await, root_1);
    eventually(T, || a.attach_saw("draft ready").then_some(())).await;

    // Both turns were mid model call: the calls were interrupted and asked
    // again after the restart (resume), and both turns still ended done.
    let llm: Vec<_> = a
        .dev
        .store("worker_agent")
        .actions()
        .unwrap()
        .into_iter()
        .filter(|x| x.kind == gllmh::types::ActionKind::LlmCall)
        .map(|x| x.state)
        .collect();
    let interrupted = llm.iter().filter(|s| **s == gllmh::types::ActionState::Interrupted).count();
    let succeeded = llm.iter().filter(|s| **s == gllmh::types::ActionState::Succeeded).count();
    assert_eq!((interrupted, succeeded), (2, 2), "llm_call states: {llm:?}");
    assert_eq!(worker_turns(&a.dev).len(), 2);
    let status = a.status("dispatch", result.root).await;
    assert_eq!(status[1].kind, InteractionKind::Result);
    check_invariants(&a.dev);
}

/// worker_agent's turns as (opened_at, ended_at), all ended done.
fn worker_turns(dev: &Dev) -> Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    let store = dev.store("worker_agent");
    let actions = store.actions().unwrap();
    let mut triggers: Vec<Ulid> =
        actions.iter().filter(|a| a.kind == gllmh::types::ActionKind::Receive).filter_map(|a| a.interaction).collect();
    triggers.sort();
    triggers
        .into_iter()
        .map(|t| {
            let turn = store.turn_by_trigger(t).unwrap().expect("turn");
            assert_eq!(turn.state, TurnState::Done);
            (turn.opened_at, turn.ended_at.expect("ended"))
        })
        .collect()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest: PathBuf = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn patch(path: &Path, f: impl FnOnce(&mut Value)) {
    let mut v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    f(&mut v);
    std::fs::write(path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
}

/// id, kind, from, to, hops, state, outcome, client_ref, caused_by
type LogRow = (String, String, String, String, u32, String, Option<String>, String, Option<String>);

/// Section 4 invariants that are queries over the log or a store, checked
/// after a full run. The rest are enforced and tested where they live
/// (broker: identity, edges, hops; valet: write-ahead; turns: step limit).
fn check_invariants(dev: &Dev) {
    use rusqlite::{Connection, OpenFlags};
    let log = Connection::open_with_flags(dev.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let rows: Vec<LogRow> = log
        .prepare(r#"SELECT id, kind, "from", "to", hops, state, outcome, client_ref, caused_by FROM interaction"#)
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!rows.is_empty());
    let agents = ["agent", "worker_agent"];
    let edges_msg = [("agent::timer", "agent"), ("agent", "user")];
    let edges_dir = [("agent", "worker_agent"), ("dispatch::draft_dispatcher", "worker_agent")];
    for (id, kind, from, to, hops, state, outcome, client_ref, _) in &rows {
        // 3: every recipient is a user.
        assert!(USERS.contains(&to.as_str()), "{id}: recipient {to} is not a user");
        // 5: messages and directives follow an edge, within max_hops (16).
        match kind.as_str() {
            "message" => assert!(edges_msg.contains(&(from.as_str(), to.as_str())), "{id}: no message edge"),
            "directive" => {
                assert!(edges_dir.contains(&(from.as_str(), to.as_str())), "{id}: no directive edge");
                // 9: a directive's recipient has a composer.
                assert!(agents.contains(&to.as_str()), "{id}: directive to a user without composer");
            }
            _ => {}
        }
        assert!(*hops <= 16, "{id}: hops {hops}");
        // 4: closed rows carry an outcome.
        assert_eq!(state == "closed", outcome.is_some(), "{id}: state {state} outcome {outcome:?}");
        // 6: the sender's store holds the send row whose id is the client_ref.
        let sender = from.split("::").next().unwrap();
        let store = dev.store(sender);
        let send = store.actions().unwrap().into_iter().find(|a| a.id.to_string() == *client_ref);
        let send = send.unwrap_or_else(|| panic!("{id}: no send row {client_ref} in {sender}'s store"));
        assert_eq!(send.kind, gllmh::types::ActionKind::Send);
    }
    // 10: every directive has exactly one result.
    for (id, kind, ..) in rows.iter().filter(|r| r.1 == "directive") {
        let results = rows.iter().filter(|r| r.1 == "result" && r.8.as_deref() == Some(id.as_str())).count();
        assert_eq!(results, 1, "directive {id} has {results} results ({kind})");
    }
    // 7 and 8: per agent, never more turns running than allowed; steps within the limit.
    for (agent, max_parallel, max_steps) in [("agent", 1usize, 20u32), ("worker_agent", 2, 20)] {
        let store = dev.store(agent);
        let mut spans = Vec::new();
        for trigger in store.actions().unwrap().iter().filter(|a| a.kind == gllmh::types::ActionKind::Receive) {
            let Some(turn) = store.turn_by_trigger(trigger.interaction.unwrap()).unwrap() else { continue };
            assert!(turn.steps <= max_steps, "{agent}: turn {} has {} steps", turn.trigger, turn.steps);
            if let Some(end) = turn.ended_at {
                spans.push((turn.opened_at, end));
            }
        }
        for (start, _) in &spans {
            let at_once = spans.iter().filter(|(s, e)| s <= start && start < e).count();
            assert!(at_once <= max_parallel, "{agent}: {at_once} turns at once");
        }
    }
}

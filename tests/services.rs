//! M7.1 service submits and result forwarding in dev mode.

mod support;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use gllmh::frame::{CallError, Incoming, Peer, Responder};
use gllmh::paths::{service_config, service_socket};
use gllmh::protocol::broker::{SendInput, codes::NOT_ALLOWED};
use gllmh::protocol::local::{self, NotifyInput, NotifyOutput, StatusInput, StatusOutput, SubmitInput, SubmitOutput};
use gllmh::protocol::tool::{ENV_SERVICE_CONFIG, ENV_SERVICE_SOCKET};
use gllmh::types::{ActionKind, ActionState, Block, Interaction, InteractionKind, InteractionState, Outcome, Sender};
use serde_json::{Value, json};
use support::dev::{Dev, eventually, eventually_async, write_config};
use support::fake_llm::{FakeLlm, Matcher, Reply, Rule};
use tokio::net::{UnixDatagram, UnixStream};
use tokio::process::Command;
use tokio::time::timeout;
use ulid::Ulid;

const DISPATCHER: &str = "draft_dispatcher";
const T: Duration = Duration::from_secs(20);

async fn configured(dispatch_is_agent: bool) -> (Dev, FakeLlm, PathBuf) {
    let fake = FakeLlm::start(vec![Rule {
        matcher: Matcher::default(),
        repeat: true,
        delay_ms: 0,
        response: Reply::Message { text: Some("Draft complete".into()), thinking: None, tool_calls: vec![] },
    }])
    .await
    .unwrap();
    let mut dev = Dev::new("/nonexistent", "services");
    dev.config = dev.dir.path().join("config");
    let result_file = dev.dir.path().join("result.jsonl");
    let mut dispatch = json!({
        "name": "dispatch", "system_user": "dispatch",
        "services": {
            (DISPATCHER): {
                "command": "/bin/false", "args": [], "restart": "never", "required": false,
                "config": {"to": "worker", "context": [{"name": "draft", "content": "Write a letter"}],
                           "result_file": result_file}
            },
            "other": {"command": "/bin/false", "args": [], "restart": "never", "required": false}
        }
    });
    if dispatch_is_agent {
        dispatch["composer_profile"] = json!("inbox");
        dispatch["llm_profiles"] = json!(["fake"]);
    }
    write_config(
        &dev.config,
        &[
            (
                "runtime",
                "services",
                json!({
                    "name": "services", "users": ["dispatch", "worker", "inbox"], "restart": "never",
                    "allow_messages": {"dispatch": ["inbox"], "dispatch::draft_dispatcher": ["inbox"],
                                       "inbox": ["dispatch"]},
                    "allow_directives": {"dispatch": ["worker"], "dispatch::draft_dispatcher": ["worker"]}
                }),
            ),
            ("user", "dispatch", dispatch),
            (
                "user",
                "worker",
                json!({"name": "worker", "system_user": "worker", "composer_profile": "worker", "llm_profiles": ["fake"]}),
            ),
            (
                "user",
                "inbox",
                json!({"name": "inbox", "system_user": "inbox", "composer_profile": "inbox", "llm_profiles": ["fake"]}),
            ),
            (
                "composer",
                "worker",
                json!({"name": "worker", "engine": "builtin", "work_mode": "react", "builtin": {"mode": "directive"}}),
            ),
            (
                "composer",
                "inbox",
                json!({"name": "inbox", "engine": "builtin", "work_mode": "react", "builtin": {"mode": "message"}}),
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
    dev.start_broker("dispatch").await;
    (dev, fake, result_file)
}

async fn service(dev: &Dev, name: &str) -> (Peer, Incoming) {
    Peer::spawn_unix(UnixStream::connect(service_socket(&dev.data_dir("dispatch"), name)).await.unwrap())
}

async fn status(ui: &Peer, root: Ulid) -> StatusOutput {
    ui.request(local::STATUS, &StatusInput { root }).await.unwrap()
}

async fn run_in_state(ui: &Peer, root: Ulid, result_state: InteractionState) -> StatusOutput {
    eventually_async(T, || async {
        let status = status(ui, root).await;
        (status.interactions.len() == 2
            && status.interactions[0].state == InteractionState::Closed
            && status.interactions[1].state == result_state)
            .then_some(status)
    })
    .await
}

async fn direct(peer: &Peer) -> SubmitOutput {
    timeout(
        T,
        peer.request(
            local::SUBMIT,
            &SubmitInput {
                kind: InteractionKind::Directive,
                to: "worker".into(),
                body: None,
                context: Some(vec![Block { name: "draft".into(), content: "Write a letter".into() }]),
                caused_by: None,
            },
        ),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn result_notice(notes: &mut Incoming) -> (Interaction, Responder) {
    let request = timeout(T, notes.next()).await.unwrap().unwrap();
    assert_eq!(request.op, local::NOTIFY);
    let (input, responder) = request.parse::<NotifyInput>().unwrap();
    assert_eq!(input.interaction.kind, InteractionKind::Result);
    (input.interaction, responder)
}

fn check_service_run(dev: &Dev, directive: Ulid, result: &Interaction) {
    assert_eq!(result.caused_by, Some(directive));
    assert_eq!(result.root, directive);
    assert_eq!(result.hops, 1);
    assert_eq!(result.body, Some(json!("Draft complete")));
    assert_eq!(result.from, Sender::user("worker"));
    assert_eq!(result.to, "dispatch");
    let store = dev.store("dispatch");
    assert!(store.turn_by_trigger(result.id).unwrap().is_none());
    let actions = store.actions().unwrap();
    let send = actions.iter().find(|a| a.kind == ActionKind::Send && a.interaction == Some(directive)).unwrap();
    assert_eq!(send.service.as_deref(), Some(DISPATCHER));
    assert_eq!(send.state, ActionState::Succeeded);
    assert!(send.turn.is_none());
    let receives = actions.iter().filter(|a| a.kind == ActionKind::Receive && a.interaction == Some(result.id));
    let rows = receives.collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].turn.is_none());
}

async fn dispatcher_example() -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["build", "--locked", "--example", "draft_dispatcher", "--message-format=json"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "example build failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find_map(|message| {
            if message["reason"] == "compiler-artifact" && message["target"]["kind"] == json!(["example"]) {
                message["executable"].as_str().map(PathBuf::from)
            } else {
                None
            }
        })
        .expect("draft_dispatcher executable")
}

#[tokio::test]
async fn real_dispatcher_gets_its_result_and_both_interactions_close() {
    let (mut dev, fake, result_file) = configured(false).await;
    dev.start_valet("dispatch").await;
    dev.start_valet("worker").await;
    let (ui, mut ui_notes) = dev.client("dispatch").await;
    let (_other, mut other_notes) = service(&dev, "other").await;
    let data = dev.data_dir("dispatch");
    let _dispatcher = Command::new(dispatcher_example().await)
        .env(ENV_SERVICE_SOCKET, service_socket(&data, DISPATCHER))
        .env(ENV_SERVICE_CONFIG, service_config(&data, DISPATCHER))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result: Interaction =
        eventually(T, || std::fs::read(&result_file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok())).await;
    check_service_run(&dev, result.root, &result);
    let status = run_in_state(&ui, result.root, InteractionState::Closed).await;
    assert_eq!(status.interactions[0].from, Sender::service("dispatch", DISPATCHER));
    assert_eq!(status.interactions[0].outcome, Some(Outcome::Done));
    assert_eq!(status.interactions[1].outcome, Some(Outcome::Received));
    assert_eq!(fake.requests().len(), 1);
    assert!(fake.requests()[0].body.to_string().contains("Write a letter"));
    assert!(timeout(Duration::from_millis(100), ui_notes.next()).await.is_err());
    assert!(timeout(Duration::from_millis(100), other_notes.next()).await.is_err());
}

#[tokio::test]
async fn submit_uses_service_edges_and_preserves_causal_hops() {
    let (mut dev, _fake, _) = configured(false).await;
    dev.start_valet("dispatch").await;
    dev.start_valet("worker").await;
    dev.start_valet("inbox").await;
    let (ui, mut ui_notes) = dev.client("dispatch").await;
    let (inbox, mut inbox_notes) = dev.client("inbox").await;
    let (peer, mut service_notes) = service(&dev, DISPATCHER).await;
    let mut input = SubmitInput {
        kind: InteractionKind::Message,
        to: "inbox".into(),
        body: Some(json!("wake up")),
        context: None,
        caused_by: None,
    };
    let parent: SubmitOutput = ui.request(local::SUBMIT, &input).await.unwrap();
    let note = timeout(T, inbox_notes.next()).await.unwrap().unwrap();
    assert_eq!(note.parse::<NotifyInput>().unwrap().0.interaction.id, parent.id);

    input.caused_by = Some(parent.id);
    let output: SubmitOutput = peer.request(local::SUBMIT, &input).await.unwrap();
    assert_eq!(output.root, parent.id);
    assert_eq!(output.hops, parent.hops + 1);
    let note = timeout(T, inbox_notes.next()).await.unwrap().unwrap();
    assert_eq!(note.op, local::NOTIFY);
    let (received, responder) = note.parse::<NotifyInput>().unwrap();
    responder.ok(NotifyOutput {});
    assert_eq!(received.interaction.id, output.id);
    assert_eq!(received.interaction.from, Sender::service("dispatch", DISPATCHER));
    assert_eq!(received.interaction.caused_by, Some(parent.id));

    let (other, mut other_notes) = service(&dev, "other").await;
    let refused = other.request::<_, SubmitOutput>(local::SUBMIT, &input).await;
    assert!(matches!(refused, Err(CallError::Remote(e)) if e.code == NOT_ALLOWED));
    let actions = dev.store("dispatch").actions().unwrap();
    let send = actions.iter().find(|a| a.interaction == Some(output.id)).unwrap();
    let stored: SendInput = serde_json::from_value(send.input.clone()).unwrap();
    assert_eq!(send.service.as_deref(), Some(DISPATCHER));
    assert!(send.turn.is_none());
    assert!(!stored.local);
    assert!(actions.iter().any(|a| a.service.as_deref() == Some("other") && a.state == ActionState::Failed));

    // A result addressed to the owner as an ordinary director stays on local.sock.
    let directive = direct(&ui).await;
    let (ordinary_result, response) = result_notice(&mut ui_notes).await;
    assert_eq!(ordinary_result.caused_by, Some(directive.id));
    response.ok(NotifyOutput {});
    run_in_state(&ui, directive.root, InteractionState::Closed).await;
    input.to = "dispatch".into();
    input.caused_by = None;
    let message: SubmitOutput = inbox.request(local::SUBMIT, &input).await.unwrap();
    let note = timeout(T, ui_notes.next()).await.unwrap().unwrap();
    let (received, responder) = note.parse::<NotifyInput>().unwrap();
    assert_eq!(received.interaction.id, message.id);
    responder.ok(NotifyOutput {});
    assert!(timeout(Duration::from_millis(100), service_notes.next()).await.is_err());
    assert!(timeout(Duration::from_millis(100), other_notes.next()).await.is_err());
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn sockets_and_private_config_exist_when_ready_is_emitted() {
    let (mut dev, _fake, _) = configured(false).await;
    let notify_path = dev.dir.path().join("notify");
    let socket = UnixDatagram::bind(&notify_path).unwrap();
    dev.valet_env.push(("NOTIFY_SOCKET".into(), notify_path.to_str().unwrap().into()));
    dev.start_valet("dispatch").await;
    let mut buf = [0; 64];
    let n = timeout(T, socket.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"READY=1");
    let data = dev.data_dir("dispatch");
    for name in [DISPATCHER, "other"] {
        let socket = service_socket(&data, name);
        assert!(std::fs::metadata(&socket).unwrap().file_type().is_socket());
        assert_eq!(mode(&socket), 0o600);
        assert_eq!(mode(&service_config(&data, name)), 0o600);
    }
    let config: Value = serde_json::from_slice(&std::fs::read(service_config(&data, "other")).unwrap()).unwrap();
    assert_eq!(config, json!({}));
}

#[tokio::test]
async fn queued_results_reach_reconnecting_service_with_and_without_valet_restart() {
    let (mut dev, fake, _) = configured(true).await;
    dev.start_valet("dispatch").await;
    let (peer, notes) = service(&dev, DISPATCHER).await;
    let first = direct(&peer).await;
    let second = direct(&peer).await;
    drop((peer, notes));
    dev.start_valet("worker").await;

    let (ui, mut ui_notes) = dev.client("dispatch").await;
    run_in_state(&ui, first.root, InteractionState::Acked).await;
    run_in_state(&ui, second.root, InteractionState::Acked).await;
    let (peer, mut notes) = service(&dev, DISPATCHER).await;
    let (result, reply) = result_notice(&mut notes).await;
    let (first, second) = if result.root == first.root { (first, second) } else { (second, first) };
    check_service_run(&dev, first.id, &result);
    reply.ok(NotifyOutput {});
    run_in_state(&ui, first.root, InteractionState::Closed).await;

    // The next notify is in flight when the valet dies. The broker has
    // already acked delivery, but the service has yet to ack forwarding.
    let (result, unacked) = result_notice(&mut notes).await;
    check_service_run(&dev, second.id, &result);
    run_in_state(&ui, second.root, InteractionState::Acked).await;
    assert!(timeout(Duration::from_millis(100), ui_notes.next()).await.is_err());
    dev.kill_valet("dispatch").await;
    drop((peer, notes, unacked, ui, ui_notes));
    dev.start_valet("dispatch").await;
    let (ui, _ui_notes) = dev.client("dispatch").await;
    let (_peer, mut notes) = service(&dev, DISPATCHER).await;
    let (replayed, reply) = result_notice(&mut notes).await;
    assert_eq!(replayed, result);
    reply.ok(NotifyOutput {});
    run_in_state(&ui, second.root, InteractionState::Closed).await;
    check_service_run(&dev, second.id, &replayed);
    assert!(timeout(Duration::from_millis(100), notes.next()).await.is_err());
    assert_eq!(fake.requests().len(), 2, "only the worker ran turns");
}

//! Run the actual example executables against a local fake valet.

use std::collections::HashMap;
use std::future::Future;
use std::net::Shutdown;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gllmh::frame::{ErrorBody, Incoming, Peer, Responder};
use gllmh::protocol::broker::codes::NOT_ALLOWED;
use gllmh::protocol::local::{NOTIFY, NotifyInput, SUBMIT, SubmitInput, SubmitOutput, codes::BROKER_UNAVAILABLE};
use gllmh::protocol::tool::{ENV_SERVICE_CONFIG, ENV_SERVICE_SOCKET};
use gllmh::types::{Block, Interaction, InteractionKind, InteractionState, Sender, TurnOutcome, now};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdout, Command};
use tokio::time::timeout;
use ulid::Ulid;

fn examples() -> &'static HashMap<String, PathBuf> {
    static EXAMPLES: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();
    EXAMPLES.get_or_init(|| {
        let build = std::process::Command::new(env!("CARGO"))
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args([
                "build",
                "--locked",
                "--example",
                "timer_service",
                "--example",
                "draft_dispatcher",
                "--message-format=json",
            ])
            .output()
            .expect("run Cargo to build the examples");
        assert!(
            build.status.success(),
            "example build failed:\n{}\n{}",
            String::from_utf8_lossy(&build.stderr),
            String::from_utf8_lossy(&build.stdout)
        );
        String::from_utf8(build.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|message| {
                message["reason"] == "compiler-artifact" && message["target"]["kind"] == json!(["example"])
            })
            .filter_map(|message| {
                Some((message["target"]["name"].as_str()?.to_owned(), PathBuf::from(message["executable"].as_str()?)))
            })
            .collect()
    })
}

async fn within<T>(operation: impl Future<Output = T>) -> T {
    timeout(Duration::from_secs(10), operation).await.expect("service exchange timed out")
}

struct Valet {
    dir: TempDir,
    listener: UnixListener,
}

impl Valet {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("service.sock")).unwrap();
        Self { dir, listener }
    }

    fn spawn(&self, example: &str, config: Value) -> (Child, Lines<BufReader<ChildStdout>>) {
        let path = self.dir.path().join("config.json");
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut child = Command::new(&examples()[example])
            .env(ENV_SERVICE_SOCKET, self.dir.path().join("service.sock"))
            .env(ENV_SERVICE_CONFIG, path)
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        (child, stdout)
    }

    async fn accept(&self) -> (Peer, Incoming, std::os::unix::net::UnixStream) {
        let (stream, _) = within(self.listener.accept()).await.unwrap();
        let stream = stream.into_std().unwrap();
        let disconnect = stream.try_clone().unwrap();
        let (reader, writer) = UnixStream::from_std(stream).unwrap().into_split();
        let (peer, incoming) = Peer::spawn(reader, writer);
        (peer, incoming, disconnect)
    }
}

async fn receive_submit(incoming: &mut Incoming) -> (SubmitInput, Responder) {
    let request = within(incoming.next()).await.expect("expected a submit request");
    assert_eq!(request.op, SUBMIT);
    request.parse().expect("invalid submit input")
}

async fn logged(stdout: &mut Lines<BufReader<ChildStdout>>) -> Value {
    serde_json::from_str(&within(stdout.next_line()).await.unwrap().expect("expected a JSON line")).unwrap()
}

fn accepted() -> SubmitOutput {
    let id = Ulid::new();
    SubmitOutput { id, root: id, hops: 0 }
}

fn result_for(directive: Ulid) -> Interaction {
    Interaction {
        id: Ulid::new(),
        kind: InteractionKind::Result,
        client_ref: Ulid::new(),
        from: Sender::user("worker_agent"),
        to: "dispatch".into(),
        caused_by: Some(directive),
        root: directive,
        hops: 1,
        body: Some(json!("Draft complete")),
        context: None,
        turn_outcome: Some(TurnOutcome::Done),
        state: InteractionState::Closed,
        outcome: None,
        accepted_at: now(),
        closed_at: Some(now()),
    }
}

async fn notify(peer: &Peer, interaction: Interaction) {
    let reply: Value = within(peer.request(NOTIFY, &NotifyInput { interaction })).await.unwrap();
    assert_eq!(reply, json!({}));
}

#[tokio::test]
async fn one_shot_timer_preserves_cause() {
    let valet = Valet::new();
    let cause = Ulid::new();
    examples();
    let start = Instant::now();
    let (mut child, mut stdout) =
        valet.spawn("timer_service", json!({"to": "agent", "body": {"wake": true}, "delay_s": 1, "caused_by": cause}));
    let (_, mut incoming, _) = valet.accept().await;
    let (input, response) = receive_submit(&mut incoming).await;
    assert!(start.elapsed() >= Duration::from_secs(1), "submitted before the delay");
    assert_eq!(
        input,
        SubmitInput {
            kind: InteractionKind::Message,
            to: "agent".into(),
            body: Some(json!({"wake": true})),
            context: None,
            caused_by: Some(cause),
        }
    );
    let output = SubmitOutput { root: cause, hops: 1, ..accepted() };
    response.ok(output);
    assert_eq!(logged(&mut stdout).await, json!(output));
    assert!(within(child.wait()).await.unwrap().success());
    assert!(stdout.next_line().await.unwrap().is_none());
}

#[tokio::test]
async fn recurring_timer_reconnects_and_starts_fresh_runs() {
    let valet = Valet::new();
    let (_child, mut stdout) = valet.spawn(
        "timer_service",
        json!({"to": "agent", "body": "tick", "delay_s": 0, "repeat_s": 1, "caused_by": Ulid::new()}),
    );
    let (_, mut incoming, disconnect) = valet.accept().await;
    let (input, response) = receive_submit(&mut incoming).await;
    assert_eq!(input.caused_by, None);
    let retryable = ErrorBody::retryable(BROKER_UNAVAILABLE, "broker is restarting");
    response.err(retryable.clone());
    assert_eq!(logged(&mut stdout).await, json!(retryable));
    let (retried, response) = receive_submit(&mut incoming).await;
    assert_eq!(retried, input);
    let output = accepted();
    response.ok(output);
    assert_eq!(logged(&mut stdout).await, json!(output));

    let Valet { dir, listener } = valet;
    drop(listener);
    let socket = dir.path().join("service.sock");
    std::fs::remove_file(&socket).unwrap();
    disconnect.shutdown(Shutdown::Both).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let valet = Valet { listener: UnixListener::bind(socket).unwrap(), dir };
    let (_, mut incoming, _) = valet.accept().await;
    let (next, response) = receive_submit(&mut incoming).await;
    assert_eq!(next, input);
    let output = accepted();
    response.ok(output);
    assert_eq!(logged(&mut stdout).await, json!(output));
}

#[tokio::test]
async fn dispatcher_reconnects_and_records_only_the_matching_result() {
    let valet = Valet::new();
    let path = valet.dir.path().join("result.jsonl");
    let context = vec![Block { name: "assignment".into(), content: "Draft an outline".into() }];
    let (mut child, mut stdout) =
        valet.spawn("draft_dispatcher", json!({"to": "worker_agent", "context": context, "result_file": path}));
    let (peer, mut incoming, disconnect) = valet.accept().await;
    let (input, response) = receive_submit(&mut incoming).await;
    assert_eq!(
        input,
        SubmitInput {
            kind: InteractionKind::Directive,
            to: "worker_agent".into(),
            body: None,
            context: Some(context),
            caused_by: None,
        }
    );
    let directive = accepted();
    response.ok(directive);
    // This ack confirms the dispatcher has processed the submit reply before the outage.
    notify(&peer, result_for(Ulid::new())).await;
    assert!(!path.exists());
    disconnect.shutdown(Shutdown::Both).unwrap();

    let (peer, mut incoming, _) = valet.accept().await;
    let mut wrong_kind = result_for(directive.id);
    wrong_kind.kind = InteractionKind::Message;
    notify(&peer, wrong_kind).await;
    assert!(!path.exists());
    let result = result_for(directive.id);
    notify(&peer, result.clone()).await;
    assert_eq!(logged(&mut stdout).await, json!(result));
    let mut expected = serde_json::to_vec(&result).unwrap();
    expected.push(b'\n');
    assert_eq!(std::fs::read(&path).unwrap(), expected);

    notify(&peer, result).await;
    assert!(timeout(Duration::from_millis(100), stdout.next_line()).await.is_err());
    assert!(timeout(Duration::from_millis(100), incoming.next()).await.is_err(), "dispatcher resubmitted");
    assert!(child.try_wait().unwrap().is_none(), "dispatcher should stay alive");
}

#[tokio::test]
async fn services_exit_on_permanent_refusal() {
    for (example, config) in [
        ("timer_service", json!({"to": "agent", "body": "wake", "delay_s": 0})),
        ("draft_dispatcher", json!({"to": "worker_agent", "context": []})),
    ] {
        let valet = Valet::new();
        let (mut child, mut stdout) = valet.spawn(example, config);
        let (_, mut incoming, _) = valet.accept().await;
        let (_, response) = receive_submit(&mut incoming).await;
        let refusal = ErrorBody::new(NOT_ALLOWED, "no edge");
        response.err(refusal.clone());
        assert_eq!(logged(&mut stdout).await, json!(refusal));
        assert!(!within(child.wait()).await.unwrap().success(), "{example} should fail");
    }
}

#[tokio::test]
async fn dispatcher_does_not_replay_an_unacknowledged_submit() {
    let valet = Valet::new();
    let (mut child, _) = valet.spawn("draft_dispatcher", json!({"to": "worker_agent", "context": []}));
    let (_, mut incoming, disconnect) = valet.accept().await;
    let (_, response) = receive_submit(&mut incoming).await;
    disconnect.shutdown(Shutdown::Both).unwrap();
    drop(response);
    assert!(!within(child.wait()).await.unwrap().success());
}

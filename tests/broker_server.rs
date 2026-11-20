//! Exercise section 6.1 using the real broker and raw Peer connections.

mod support;

use std::fs;
use std::future::Future;
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use gllmh::frame::{self, CallError, ErrorBody, Incoming, Peer, Responder};
use gllmh::paths::user_socket;
use gllmh::profile::RuntimeProfile;
use gllmh::protocol::{PROTOCOL_VERSION, broker::*};
use gllmh::types::{
    ActionId, Block, Interaction, InteractionId, InteractionKind, InteractionState, Outcome, Sender, TurnOutcome,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::{UnixDatagram, UnixStream};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(8);

struct TestBroker {
    dir: TempDir,
    config: PathBuf,
}

impl TestBroker {
    fn acceptance() -> Self {
        Self { dir: tempfile::tempdir().unwrap(), config: support::fixture("acceptance") }
    }

    fn with_runtime(update: impl FnOnce(&mut RuntimeProfile)) -> Self {
        let mut broker = Self::acceptance();
        let config = broker.dir.path().join("config");
        copy_tree(&broker.config, &config);
        let runtime_file = config.join("runtime/acceptance.json");
        let mut profile: RuntimeProfile = serde_json::from_slice(&fs::read(&runtime_file).unwrap()).unwrap();
        update(&mut profile);
        fs::write(runtime_file, serde_json::to_vec(&profile).unwrap()).unwrap();
        broker.config = config;
        broker
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("broker.db")
    }

    fn run_dir(&self) -> PathBuf {
        self.dir.path().join("users")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(support::gllmh_bin());
        command
            .args(["serve", "--profile", "acceptance", "--dev", "--config-dir"])
            .arg(&self.config)
            .arg("--run-dir")
            .arg(self.run_dir())
            .arg("--db")
            .arg(self.db())
            .args(["--expire-interval-s", "1"])
            .env_remove("NOTIFY_SOCKET")
            .kill_on_drop(true);
        command
    }

    async fn start(&self) -> Child {
        let notify = self.dir.path().join("ready");
        let socket = UnixDatagram::bind(&notify).unwrap();
        let mut child = self.command().env("NOTIFY_SOCKET", &notify).spawn().unwrap();
        let mut buf = [0; 32];
        let bytes = within(async {
            tokio::select! {
                bytes = socket.recv(&mut buf) => bytes.unwrap(),
                status = child.wait() => panic!("broker exited before READY: {:?}", status.unwrap()),
            }
        })
        .await;
        assert_eq!(&buf[..bytes], b"READY=1");
        fs::remove_file(notify).unwrap();
        child
    }

    async fn connect(&self, user: &str) -> Link {
        let stream = within(UnixStream::connect(user_socket(&self.run_dir(), user))).await.unwrap();
        let std_stream = stream.into_std().unwrap();
        let control = std_stream.try_clone().unwrap();
        let (peer, incoming) = Peer::spawn_unix(UnixStream::from_std(std_stream).unwrap());
        Link { peer, incoming, control }
    }

    async fn online(&self, user: &str) -> Link {
        let link = self.connect(user).await;
        assert_eq!(link.hello().await.user, user);
        link
    }
}

struct Link {
    peer: Peer,
    incoming: Incoming,
    control: std::os::unix::net::UnixStream,
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.control.shutdown(Shutdown::Both);
    }
}

impl Link {
    async fn call<I: Serialize, O: DeserializeOwned>(&self, op: &str, input: &I) -> O {
        within(self.peer.request(op, input)).await.unwrap()
    }

    async fn error(&self, op: &str, input: &impl Serialize, code: &str) {
        let error = within(self.peer.request::<_, Value>(op, input)).await.unwrap_err();
        assert_eq!(error.code(), Some(code), "{error}");
    }

    async fn hello(&self) -> HelloOutput {
        self.call(HELLO, &hello()).await
    }

    async fn send(&self, input: &SendInput) -> SendOutput {
        self.call(SEND, input).await
    }

    async fn receive(&mut self, id: InteractionId) -> (Interaction, Responder) {
        let request = within(self.incoming.next()).await.unwrap();
        assert_eq!(request.op, DELIVER);
        let (input, reply) = request.parse::<DeliverInput>().unwrap();
        assert_eq!(input.interaction.id, id);
        assert_eq!(input.interaction.state, InteractionState::Delivered);
        (input.interaction, reply)
    }

    async fn close(&self, id: InteractionId, outcome: Outcome) {
        let _: CloseOutput = self.call(CLOSE, &CloseInput { id, outcome }).await;
    }

    async fn record(&mut self, id: InteractionId, outcome: Outcome) -> Interaction {
        let (interaction, reply) = self.receive(id).await;
        reply.ok(DeliverOutput {});
        self.close(id, outcome).await;
        interaction
    }

    async fn lookup(&self, client_ref: ActionId) -> LookupOutput {
        self.call(LOOKUP, &LookupInput { client_ref }).await
    }
}

async fn within<F: Future>(future: F) -> F::Output {
    timeout(DEADLINE, future).await.expect("broker test timed out")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn hello() -> HelloInput {
    HelloInput { protocol: PROTOCOL_VERSION }
}

fn message(to: &str) -> SendInput {
    SendInput {
        client_ref: ActionId::new(),
        kind: InteractionKind::Message,
        to: to.into(),
        service: None,
        caused_by: None,
        body: Some(json!("body")),
        context: None,
        turn_outcome: None,
        local: false,
    }
}

fn timer() -> SendInput {
    SendInput { service: Some("timer".into()), ..message("agent") }
}

fn directive(to: &str, caused_by: Option<InteractionId>) -> SendInput {
    SendInput {
        kind: InteractionKind::Directive,
        caused_by,
        body: None,
        context: Some(vec![Block { name: "instructions".into(), content: "prepare a draft".into() }]),
        ..message(to)
    }
}

fn result(to: &str, caused_by: InteractionId) -> SendInput {
    SendInput {
        kind: InteractionKind::Result,
        caused_by: Some(caused_by),
        turn_outcome: Some(TurnOutcome::Done),
        ..message(to)
    }
}

#[tokio::test]
async fn validates_profile_and_prints_all_violations_before_listening() {
    let broker = TestBroker::with_runtime(|profile| {
        profile.users.push("missing_one".into());
        profile.users.push("missing_two".into());
    });
    let output = within(broker.command().stderr(Stdio::piped()).output()).await.unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("missing_one.json"), "{stderr}");
    assert!(stderr.contains("missing_two.json"), "{stderr}");
    assert!(!broker.run_dir().exists());
}

#[tokio::test]
async fn hello_uses_listener_identity_and_excludes_a_second_connection() {
    let broker = TestBroker::acceptance();
    let _child = broker.start().await;
    for path in [user_socket(&broker.run_dir(), "agent"), broker.db()] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let agent = broker.connect("agent").await;
    agent.error(SEND, &message("user"), codes::NO_HELLO).await;
    agent.error(HELLO, &HelloInput { protocol: PROTOCOL_VERSION + 1 }, codes::PROTOCOL_MISMATCH).await;
    let hello = agent.hello().await;
    assert_eq!(hello.user, "agent");
    assert_eq!(hello.edges[&Sender::user("agent")].directives, ["worker_agent"]);
    assert_eq!(hello.edges[&Sender::service("agent", "timer")].messages, ["agent"]);
    let second = broker.connect("agent").await;
    second.error(HELLO, &HelloInput { protocol: PROTOCOL_VERSION }, codes::ALREADY_CONNECTED).await;
    second.error(SEND, &message("user"), codes::NO_HELLO).await;
    agent.error("other", &HelloInput { protocol: PROTOCOL_VERSION }, frame::codes::UNKNOWN_OP).await;

    let duplicate_broker = within(broker.command().stderr(Stdio::piped()).output()).await.unwrap();
    assert!(!duplicate_broker.status.success());
    let stderr = String::from_utf8(duplicate_broker.stderr).unwrap();
    assert!(stderr.contains("listening socket"), "{stderr}");
    assert_eq!(broker.connect("user").await.hello().await.user, "user");
}

#[tokio::test]
async fn routes_acceptance_workflow_and_exposes_only_status_metadata() {
    let broker = TestBroker::acceptance();
    let _child = broker.start().await;
    let mut agent = broker.online("agent").await;
    let mut worker = broker.online("worker_agent").await;
    let mut user = broker.online("user").await;
    let dispatch = broker.online("dispatch").await;

    let tick = timer();
    let a = agent.send(&tick).await;
    let received = agent.record(a.id, Outcome::Done).await;
    assert_eq!(received.from, Sender::service("agent", "timer"));
    let b = agent.send(&directive("worker_agent", Some(a.id))).await;
    let received = worker.record(b.id, Outcome::Done).await;
    assert_eq!(received.context.unwrap()[0].content, "prepare a draft");
    let c = worker.send(&result("agent", b.id)).await;
    assert_eq!(agent.record(c.id, Outcome::Done).await.turn_outcome, Some(TurnOutcome::Done));
    let d = agent.send(&SendInput { caused_by: Some(c.id), body: Some(Value::Null), ..message("user") }).await;
    assert_eq!(user.record(d.id, Outcome::Received).await.body, Some(Value::Null));

    let outputs = [a, b, c, d];
    assert_eq!(outputs.map(|row| row.hops), [0, 1, 2, 3]);
    assert!(outputs.iter().all(|row| row.root == a.id));
    let status_input = StatusInput { root: a.root };
    let status: Value = user.call(STATUS, &status_input).await;
    for row in status["interactions"].as_array().unwrap() {
        assert!(row.get("body").is_none());
        assert!(row.get("context").is_none());
    }
    let status: StatusOutput = serde_json::from_value(status).unwrap();
    assert_eq!(status.interactions.iter().map(|row| row.id).collect::<Vec<_>>(), outputs.map(|row| row.id));
    assert!(status.interactions.iter().all(|row| row.state == InteractionState::Closed));
    dispatch.error(STATUS, &status_input, codes::NOT_PARTICIPANT).await;
    dispatch.error(STATUS, &StatusInput { root: d.id }, codes::UNKNOWN_ROOT).await;

    // Retrying the same sender/ref bypasses recipient, size, and causal checks.
    let retry = SendInput {
        to: "missing".into(),
        caused_by: Some(InteractionId::new()),
        body: Some(json!("x".repeat(1_048_577))),
        ..tick
    };
    assert_eq!(agent.send(&retry).await, a);
    assert_eq!(agent.lookup(retry.client_ref).await.interaction, Some(a));
    assert!(!user.lookup(retry.client_ref).await.found);
    user.close(d.id, Outcome::Received).await;
    user.error(CLOSE, &CloseInput { id: d.id, outcome: Outcome::Failed }, codes::NOT_ACKED).await;
}

#[tokio::test]
async fn enforces_edges_recipients_size_and_hops_with_result_exemption() {
    let broker = TestBroker::with_runtime(|profile| {
        profile.max_body_bytes = 55;
        profile.max_hops = 1;
    });
    let _child = broker.start().await;
    let agent = broker.online("agent").await;
    let worker = broker.online("worker_agent").await;
    let dispatch = broker.online("dispatch").await;

    agent.error(SEND, &message("agent::timer"), codes::UNKNOWN_USER).await;
    agent.error(SEND, &message("agent"), codes::NOT_ALLOWED).await;
    agent.error(SEND, &directive("agent", None), codes::NOT_ALLOWED).await;
    agent.error(SEND, &directive("user", None), codes::NOT_AN_AGENT).await;
    dispatch.error(SEND, &timer(), codes::NOT_ALLOWED).await;
    agent
        .error(SEND, &SendInput { caused_by: Some(InteractionId::new()), ..message("user") }, frame::codes::BAD_INPUT)
        .await;
    agent.error(SEND, &SendInput { body: Some(json!("x".repeat(54))), ..message("user") }, codes::TOO_LARGE).await;

    // The directive's context is 53 bytes. Body and context count together.
    let directive_input = directive("worker_agent", None);
    let context_size = serde_json::to_vec(directive_input.context.as_ref().unwrap()).unwrap().len();
    assert!(context_size <= 55 && context_size + 4 > 55);
    agent.error(SEND, &SendInput { body: Some(Value::Null), ..directive_input }, codes::TOO_LARGE).await;

    let a = agent.send(&timer()).await;
    let b = agent.send(&directive("worker_agent", Some(a.id))).await;
    worker.error(SEND, &result("agent", a.id), codes::NOT_OWED).await;
    worker.error(SEND, &result("dispatch", b.id), codes::NOT_OWED).await;
    let reply = result("agent", b.id);
    let c = worker.send(&reply).await;
    assert_eq!(worker.send(&reply).await, c);
    worker.error(SEND, &result("agent", b.id), codes::NOT_OWED).await;
    assert_eq!([a.hops, b.hops, c.hops], [0, 1, 2]);
    let followup = SendInput { caused_by: Some(c.id), ..message("user") };
    agent.error(SEND, &followup, codes::HOP_LIMIT).await;
    let d = agent.send(&SendInput { local: true, ..followup }).await;
    assert_eq!(d.hops, 0);
    assert_eq!(d.root, a.id);

    let indirect = SendInput { service: Some("draft_dispatcher".into()), ..directive("worker_agent", None) };
    let accepted = dispatch.send(&indirect).await;
    let routed = worker.send(&result("dispatch", accepted.id)).await;
    assert_eq!(routed.root, accepted.id);
    let status: StatusOutput = dispatch.call(STATUS, &StatusInput { root: accepted.id }).await;
    assert_eq!(status.interactions[1].to, "dispatch");
    assert_eq!(dispatch.lookup(indirect.client_ref).await.interaction, Some(accepted));
}

#[tokio::test]
async fn delivers_oldest_first_waits_for_ack_and_retries_errors() {
    let broker = TestBroker::acceptance();
    let _child = broker.start().await;
    let agent = broker.online("agent").await;
    let a = agent.send(&message("user")).await;
    let b = agent.send(&message("user")).await;
    agent.error(CLOSE, &CloseInput { id: a.id, outcome: Outcome::Received }, codes::NOT_YOURS).await;

    let mut user = broker.online("user").await;
    let (first, reply) = user.receive(a.id).await;
    user.error(CLOSE, &CloseInput { id: a.id, outcome: Outcome::Received }, codes::NOT_ACKED).await;
    assert!(timeout(Duration::from_millis(60), user.incoming.next()).await.is_err());
    reply.err(ErrorBody::retryable(codes::STORE_FAILED, "test write failure"));

    let (redelivery, reply) = user.receive(a.id).await;
    assert_eq!(redelivery, first);
    reply.err(ErrorBody::new(frame::codes::INTERNAL, "test alternate failure"));
    user.record(a.id, Outcome::Received).await;
    user.record(b.id, Outcome::Received).await;
    let c = agent.send(&message("user")).await;
    user.record(c.id, Outcome::Received).await;
}

#[tokio::test]
async fn requeues_unacknowledged_delivery_on_disconnect_and_restart() {
    let broker = TestBroker::acceptance();
    let mut child = broker.start().await;
    let agent = broker.online("agent").await;
    let mut user = broker.online("user").await;
    let input = message("user");
    let a = agent.send(&input).await;
    let b = agent.send(&message("user")).await;
    let (first, reply) = user.receive(a.id).await;
    drop(user);
    drop(reply);

    let mut user = broker.connect("user").await;
    within(async {
        loop {
            match user.peer.request::<_, HelloOutput>(HELLO, &hello()).await {
                Ok(output) => {
                    assert_eq!(output.user, "user");
                    break;
                }
                Err(CallError::Remote(error)) if error.code == codes::ALREADY_CONNECTED => {
                    sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("reconnect: {error}"),
            }
        }
    })
    .await;
    let (redelivered, reply) = user.receive(a.id).await;
    assert_eq!(first, redelivered);
    within(child.kill()).await.unwrap();
    drop(reply);
    drop(user);
    drop(agent);

    let _child = broker.start().await;
    let mut user = broker.online("user").await;
    assert_eq!(user.record(a.id, Outcome::Received).await, first);
    user.record(b.id, Outcome::Received).await;
    assert_eq!(broker.online("agent").await.send(&input).await, a);
}

#[tokio::test]
async fn expires_only_accepted_interactions_on_a_timer() {
    let broker = TestBroker::with_runtime(|profile| profile.interaction_ttl_s = 1);
    let _child = broker.start().await;
    let agent = broker.online("agent").await;
    let mut worker = broker.online("worker_agent").await;
    let a = agent.send(&directive("worker_agent", None)).await;
    let (_delivered, ack) = worker.receive(a.id).await;
    let b = agent.send(&message("user")).await;
    within(async {
        loop {
            let status: StatusOutput = agent.call(STATUS, &StatusInput { root: b.root }).await;
            if status.interactions[0].state == InteractionState::Dead {
                break;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await;
    let status: StatusOutput = agent.call(STATUS, &StatusInput { root: a.root }).await;
    assert_eq!(status.interactions[0].state, InteractionState::Delivered);
    ack.ok(DeliverOutput {});
    worker.close(a.id, Outcome::Done).await;

    let mut user = broker.online("user").await;
    assert!(timeout(Duration::from_millis(60), user.incoming.next()).await.is_err());
}

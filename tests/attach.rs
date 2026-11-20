use std::{process::Stdio, time::Duration};

use gllmh::{
    attach::session,
    frame::{ErrorBody, Incoming, Peer},
    protocol::{
        broker::{StatusRow, codes::NOT_ALLOWED},
        local::{self, NotifyInput, NotifyOutput, StatusInput, StatusOutput, SubmitInput, SubmitOutput},
    },
    types::{Block, Interaction, InteractionKind, InteractionState, Outcome, Sender, Timestamp, TurnOutcome},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    time::timeout,
};
use ulid::Ulid;

const DEADLINE: Duration = Duration::from_secs(10);

fn peer(stream: UnixStream) -> (Peer, Incoming) {
    let (reader, writer) = stream.into_split();
    Peer::spawn(reader, writer)
}

fn interaction(kind: InteractionKind, root: Ulid) -> Interaction {
    Interaction {
        id: Ulid::new(),
        client_ref: Ulid::new(),
        kind,
        from: Sender::parse("agent::worker").unwrap(),
        to: "operator".into(),
        caused_by: None,
        root,
        hops: 1,
        body: None,
        context: None,
        turn_outcome: None,
        state: InteractionState::Closed,
        outcome: Some(Outcome::Received),
        accepted_at: Timestamp::UNIX_EPOCH,
        closed_at: Some(Timestamp::UNIX_EPOCH),
    }
}

#[tokio::test]
async fn session_handles_notifications_and_commands() {
    timeout(DEADLINE, async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (client, incoming) = peer(UnixStream::connect(&path).await.unwrap());
        let (server, mut requests) = peer(listener.accept().await.unwrap().0);
        let root = Ulid::new();
        let directive_id = Ulid::new();
        let blocks = vec![
            Block { name: "instructions".into(), content: "review the draft".into() },
            Block { name: "reference".into(), content: "part=two".into() },
        ];

        let commands = format!(
            "bare line\n/msg agent hello  world\n/dir agent broken=\"quote\n\
             /dir agent instructions=\"review the draft\" reference='part=two'\n\
             /status {root}\n/msg denied nope\n/quit\n/msg agent ignored\n"
        );
        let mut lines = BufReader::new(commands.as_bytes()).lines();
        let mut output = Vec::new();

        let serve = async {
            let request = requests.next().await.unwrap();
            assert_eq!(request.op, local::SUBMIT);
            assert_eq!(
                request.input,
                serde_json::to_value(SubmitInput {
                    kind: InteractionKind::Message,
                    to: "agent".into(),
                    body: Some("hello  world".into()),
                    context: None,
                    caused_by: None,
                })
                .unwrap()
            );

            let mut message = interaction(InteractionKind::Message, root);
            message.body = Some("a message".into());
            let mut directive = interaction(InteractionKind::Directive, root);
            directive.context = Some(blocks.clone());
            let mut result = interaction(InteractionKind::Result, root);
            result.body = Some("reviewed".into());
            result.turn_outcome = Some(TurnOutcome::Done);
            for interaction in [message, directive, result.clone()] {
                let ack: serde_json::Value = server.request(local::NOTIFY, &NotifyInput { interaction }).await.unwrap();
                assert_eq!(ack, serde_json::to_value(NotifyOutput {}).unwrap());
            }
            request.responder.ok(SubmitOutput { id: root, root, hops: 0 });

            let request = requests.next().await.unwrap();
            assert_eq!(request.op, local::SUBMIT);
            assert_eq!(
                request.input,
                serde_json::to_value(SubmitInput {
                    kind: InteractionKind::Directive,
                    to: "agent".into(),
                    body: None,
                    context: Some(blocks),
                    caused_by: None,
                })
                .unwrap()
            );
            request.responder.ok(SubmitOutput { id: directive_id, root: directive_id, hops: 0 });

            let request = requests.next().await.unwrap();
            assert_eq!(request.op, local::STATUS);
            let (input, reply) = request.parse::<StatusInput>().unwrap();
            assert_eq!(input, StatusInput { root });
            reply.ok(StatusOutput { interactions: vec![StatusRow::from(&result)] });

            let request = requests.next().await.unwrap();
            assert_eq!(request.op, local::SUBMIT);
            let (input, reply) = request.parse::<SubmitInput>().unwrap();
            assert_eq!(input.to, "denied");
            reply.err(ErrorBody::new(NOT_ALLOWED, "no edge"));
        };

        let (exit, ()) = tokio::join!(session(async || lines.next_line().await, &mut output, client, incoming), serve);
        exit.unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("/status <root> | /quit"));
        assert!(output.contains("agent::worker message\n\"a message\"\n"));
        assert!(
            output.contains("agent::worker directive\ninstructions=\"review the draft\"\nreference=\"part=two\"\n")
        );
        assert!(output.contains("agent::worker result turn_outcome=done\n\"reviewed\"\n"));
        assert!(output.contains(&format!("id={root} root={root} hops=0")));
        assert!(output.contains(&format!("id={directive_id} root={directive_id} hops=0")));
        assert!(output.contains(&format!("root={root} interactions=1")));
        assert!(output.contains("hops=1 result agent::worker -> operator state=closed outcome=received"));
        assert!(output.contains("not_allowed: no edge"));
        assert_eq!(output.matches("Usage:").count(), 1);
    })
    .await
    .expect("attach session timed out");
}

fn binary(path: &std::path::Path) -> tokio::process::Child {
    Command::new(env!("CARGO_BIN_EXE_gllmh"))
        .args(["attach", "--socket"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn binary_accepts_piped_input_and_exits_on_eof() {
    timeout(DEADLINE, async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut child = binary(&path);
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"/msg agent script input\n").await.unwrap();
        drop(stdin);

        let (_server, mut requests) = peer(listener.accept().await.unwrap().0);
        let request = requests.next().await.unwrap();
        assert_eq!(request.op, local::SUBMIT);
        let (input, reply) = request.parse::<SubmitInput>().unwrap();
        assert_eq!(input.body, Some("script input".into()));
        let id = Ulid::new();
        reply.ok(SubmitOutput { id, root: id, hops: 0 });

        let output = child.wait_with_output().await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8(output.stdout).unwrap(), format!("id={id} root={id} hops=0\n"));
    })
    .await
    .expect("attach binary timed out");
}

#[tokio::test]
async fn binary_reports_socket_close_while_stdin_is_open() {
    timeout(DEADLINE, async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut child = binary(&path);
        let _open_stdin = child.stdin.take().unwrap();
        drop(listener.accept().await.unwrap().0);
        let output = child.wait_with_output().await.unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("valet connection closed"));
    })
    .await
    .expect("attach binary hung after socket close");
}

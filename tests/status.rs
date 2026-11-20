//! Exercise the status CLI through real local sockets in dev mode.

mod support;

use std::{path::Path, process::Output, time::Duration};

use gllmh::protocol::broker::codes::{NOT_PARTICIPANT, UNKNOWN_ROOT};
use gllmh::protocol::local::{self, StatusInput, StatusOutput, SubmitInput, SubmitOutput};
use gllmh::types::{InteractionKind, InteractionState, Timestamp};
use serde_json::json;
use support::{
    dev::{Dev, eventually_async, write_config},
    gllmh_bin,
};
use ulid::Ulid;

async fn submit(dev: &Dev, from: &str, to: &str, caused_by: Option<Ulid>) -> SubmitOutput {
    let (peer, _incoming) = dev.client(from).await;
    peer.request(
        local::SUBMIT,
        &SubmitInput {
            kind: InteractionKind::Message,
            to: to.into(),
            body: Some(json!("private message body")),
            context: None,
            caused_by,
        },
    )
    .await
    .unwrap()
}

async fn status(socket: &Path, root: Ulid) -> Output {
    let mut cmd = tokio::process::Command::new(gllmh_bin());
    cmd.args(["status", &root.to_string(), "--socket"]).arg(socket).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(10), cmd.output()).await.expect("status timed out").unwrap()
}

fn assert_error(output: Output, code: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&format!("{code}:")), "{stderr}");
}

#[tokio::test]
async fn status_lists_a_run_only_for_its_participants() {
    let mut dev = Dev::new("/nonexistent", "solo");
    dev.config = dev.dir.path().join("config");
    write_config(
        &dev.config,
        &[
            (
                "runtime",
                "solo",
                json!({
                    "name": "solo", "users": ["alice", "bob", "carol"],
                    "allow_messages": {"alice": ["bob"], "bob": ["alice"]},
                    "allow_directives": {}, "restart": "never"
                }),
            ),
            ("user", "alice", json!({"name": "alice", "system_user": "alice"})),
            ("user", "bob", json!({"name": "bob", "system_user": "bob"})),
            ("user", "carol", json!({"name": "carol", "system_user": "carol"})),
        ],
    );
    dev.start_broker("alice").await;
    for user in ["alice", "bob", "carol"] {
        dev.start_valet(user).await;
    }

    let first = submit(&dev, "alice", "bob", None).await;
    let second = submit(&dev, "bob", "alice", Some(first.id)).await;
    assert_eq!(second.root, first.root);
    let root = first.root;
    let (peer, _incoming) = dev.client("alice").await;
    eventually_async(Duration::from_secs(20), || async {
        let out: StatusOutput = peer.request(local::STATUS, &StatusInput { root }).await.unwrap();
        (out.interactions.len() == 2 && out.interactions.iter().all(|row| row.state == InteractionState::Closed))
            .then_some(())
    })
    .await;

    let alice = dev.local_socket("alice");
    let output = status(&alice, root).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut lines = stdout.lines();
    assert_eq!(lines.next().unwrap(), "id\tkind\tfrom\tto\tstate\toutcome\thops\taccepted_at\tclosed_at");
    for (sent, from, to) in [(first, "alice", "bob"), (second, "bob", "alice")] {
        let fields: Vec<_> = lines.next().unwrap().split('\t').collect();
        assert_eq!(fields.len(), 9);
        assert_eq!(fields[..7], [sent.id.to_string().as_str(), "message", from, to, "closed", "received", "0"]);
        let accepted: Timestamp = fields[7].parse().unwrap();
        let closed: Timestamp = fields[8].parse().unwrap();
        assert!(accepted <= closed);
    }
    assert!(lines.next().is_none());

    assert_error(status(&dev.local_socket("carol"), root).await, NOT_PARTICIPANT);
    assert_error(status(&alice, Ulid::new()).await, UNKNOWN_ROOT);
    assert_error(status(&dev.dir.path().join("absent.sock"), root).await, "connection_failed");
}

//! Run the standalone composer against the valet and a fake provider.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use gllmh::protocol::local::{self, SubmitInput, SubmitOutput};
use gllmh::types::{InteractionKind, TurnState};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use support::dev::{Dev, eventually_async, write_config};
use support::fake_llm::{FakeLlm, Matcher, Reply, Rule};

async fn build_example() -> PathBuf {
    let build = tokio::process::Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["build", "--locked", "--example", "echo_composer", "--message-format=json"])
        .output()
        .await
        .expect("run Cargo to build echo_composer");
    assert!(build.status.success(), "example build failed: {}", String::from_utf8_lossy(&build.stderr));
    String::from_utf8(build.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|msg| msg["reason"] == "compiler-artifact" && msg["target"]["kind"] == json!(["example"]))
        .find_map(|msg| msg["executable"].as_str().map(PathBuf::from))
        .expect("Cargo reports the example executable")
}

#[tokio::test]
async fn custom_composer_reads_the_trigger_and_marks_it() {
    let example = build_example().await;
    let fake = FakeLlm::start(vec![Rule {
        matcher: Matcher::default(),
        repeat: false,
        delay_ms: 0,
        response: Reply::Message { text: Some("echoed".into()), thinking: None, tool_calls: vec![] },
    }])
    .await
    .unwrap();
    let mut dev = Dev::new("/nonexistent", "solo");
    dev.config = dev.dir.path().join("config");
    write_config(
        &dev.config,
        &[
            (
                "runtime",
                "solo",
                json!({
                    "name": "solo", "users": ["alice", "helper"],
                    "allow_messages": {"alice": ["helper"]}, "allow_directives": {}, "restart": "never"
                }),
            ),
            ("user", "alice", json!({"name": "alice", "system_user": "alice"})),
            (
                "user",
                "helper",
                json!({
                    "name": "helper", "system_user": "helper", "composer_profile": "echo",
                    "llm_profiles": ["fake"], "tool_profiles": []
                }),
            ),
            (
                "composer",
                "echo",
                json!({
                    "name": "echo", "engine": "custom", "work_mode": "inst", "custom_composer_bin": example,
                    "custom": {"options": [null, true, "echo"]}
                }),
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
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    dev.start_valet("helper").await;

    let trigger_text = "A unique message from Alice";
    let (alice, _incoming) = dev.client("alice").await;
    let sent: SubmitOutput = alice
        .request(
            local::SUBMIT,
            &SubmitInput {
                kind: InteractionKind::Message,
                to: "helper".into(),
                body: Some(json!(trigger_text)),
                context: None,
                caused_by: None,
            },
        )
        .await
        .unwrap();
    let store_path = gllmh::paths::store_db(&dev.data_dir("helper"));
    let trigger = sent.id.to_string();
    let state = eventually_async(Duration::from_secs(20), || async {
        let path = store_path.clone();
        let id = sent.id;
        tokio::task::spawn_blocking(move || {
            let store = gllmh::store::ReadOnlyStore::open(&path).unwrap();
            store.turn_by_trigger(id).unwrap().map(|turn| turn.state).filter(|state| state.is_end())
        })
        .await
        .unwrap()
    })
    .await;
    assert_eq!(state, TurnState::Done);

    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let user_message = &requests[0].body["messages"][0];
    assert_eq!(user_message["role"], "user");
    assert_eq!(user_message["content"][0]["text"], trigger_text);

    tokio::task::spawn_blocking(move || {
        let store = Connection::open_with_flags(store_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (value, set_by): (String, String) = store
            .query_row("SELECT value, set_by FROM v1_mark WHERE target = ?1 AND key = 'seen_by'", [&trigger], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&value).unwrap(), "echo_composer");
        assert_eq!(set_by, trigger);
    })
    .await
    .unwrap();
}

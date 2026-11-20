mod support;

use std::collections::BTreeMap;
use std::process::Command;

use gllmh::profile::{BuiltinMode, LoadedRuntime, ResolvedUser, WorkMode, validate};
use gllmh::protocol::broker::{Edges, HelloOutput};
use gllmh::types::Sender;

#[test]
fn acceptance_profile_passes_dev_check() {
    let output = Command::new(support::gllmh_bin())
        .args(["check", "--profile", "acceptance", "--dev", "--config-dir"])
        .arg(support::fixture("acceptance"))
        .env("GLLMH_CONFIG_DIR", "unused")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn check_uses_environment_config_directory_and_reports_all_violations() {
    let output = Command::new(support::gllmh_bin())
        .args(["check", "--profile", "acceptance", "--dev"])
        .env("GLLMH_CONFIG_DIR", support::fixture("acceptance"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("runtime")).unwrap();
    std::fs::write(
        dir.path().join("runtime/invalid.json"),
        r#"{"name":"invalid","users":[],"restart":"never","allow_messages":{"nobody":["elsewhere"]}}"#,
    )
    .unwrap();
    let output = Command::new(support::gllmh_bin())
        .args(["check", "--profile", "invalid", "--dev"])
        .env("GLLMH_CONFIG_DIR", dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown or invalid sender \"nobody\""), "{stderr}");
    assert!(stderr.contains("unknown recipient \"elsewhere\""), "{stderr}");
}

#[test]
fn acceptance_profiles_and_edges_round_trip_through_hello() {
    let runtime = validate(&support::fixture("acceptance"), "acceptance", true).unwrap();
    let agent = hello_user(&runtime, "agent");
    let composer = agent.composer_profile.unwrap();
    assert_eq!(composer.work_mode, WorkMode::React);
    assert_eq!(composer.builtin_settings().unwrap().mode, BuiltinMode::Message);
    assert_eq!(agent.llm_profiles[0].name, "anthropic");
    assert_eq!(
        agent.tool_profiles.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        ["send_message", "send_directive",]
    );
    assert!(agent.services.contains_key("timer"));

    let worker = hello_user(&runtime, "worker_agent");
    let composer = worker.composer_profile.unwrap();
    assert_eq!(composer.builtin_settings().unwrap().mode, BuiltinMode::Directive);
    assert_eq!(composer.max_parallel_turns, 2);
    assert!(worker.tool_profiles.is_empty());
    assert!(hello_user(&runtime, "user").composer_profile.is_none());
    let dispatch = hello_user(&runtime, "dispatch");
    assert!(dispatch.composer_profile.is_none());
    assert!(dispatch.services.contains_key("draft_dispatcher"));

    assert_eq!(
        runtime.edges,
        BTreeMap::from([
            (Sender::user("user"), Edges::default()),
            (Sender::user("agent"), Edges { messages: vec!["user".into()], directives: vec!["worker_agent".into()] }),
            (Sender::service("agent", "timer"), Edges { messages: vec!["agent".into()], directives: vec![] }),
            (Sender::user("worker_agent"), Edges::default()),
            (Sender::user("dispatch"), Edges::default()),
            (
                Sender::service("dispatch", "draft_dispatcher"),
                Edges { messages: vec![], directives: vec!["worker_agent".into()] },
            ),
        ])
    );
    assert!(runtime.hello("unknown").unwrap().is_none());
}

fn hello_user(runtime: &LoadedRuntime, name: &str) -> ResolvedUser {
    let hello = runtime.hello(name).unwrap().unwrap();
    let hello: HelloOutput = serde_json::from_slice(&serde_json::to_vec(&hello).unwrap()).unwrap();
    assert_eq!(hello.user, name);
    let sender_edges: BTreeMap<_, _> = runtime
        .edges
        .iter()
        .filter(|(sender, _)| sender.user == name)
        .map(|(sender, edges)| (sender.clone(), edges.clone()))
        .collect();
    assert_eq!(hello.edges, sender_edges);
    serde_json::from_value(hello.profile).unwrap()
}

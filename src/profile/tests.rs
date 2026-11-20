use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::{Value, json};

use super::system::Accounts;
use super::validate::validate_with_accounts;
use super::*;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Self { dir };
        fixture.write("runtime/test.json", runtime(&["alice"]));
        fixture.write("user/alice.json", json!({"name": "alice", "system_user": "alice-os"}));
        fixture
    }

    fn write(&self, path: &str, profile: Value) {
        let path = self.dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&profile).unwrap()).unwrap();
    }

    fn check(&self) -> Result<LoadedRuntime, Vec<Violation>> {
        validate(self.dir.path(), "test", true)
    }

    fn check_with(&self, accounts: Accounts) -> Result<LoadedRuntime, Vec<Violation>> {
        validate_with_accounts(self.dir.path(), "test", Some(accounts))
    }

    fn make_agent(&self, composer: Value) {
        self.write("composer/work.json", composer);
        self.write("user/alice.json", json!({"name": "alice", "system_user": "alice-os", "composer_profile": "work"}));
    }
}

fn runtime(users: &[&str]) -> Value {
    json!({"name": "test", "users": users, "restart": "never"})
}

fn composer(engine: &str) -> Value {
    json!({"name": "work", "engine": engine, "work_mode": "inst"})
}

fn tool(name: &str) -> Value {
    json!({"name": name, "hazardness": "safe"})
}

fn llm(name: &str) -> Value {
    json!({
        "name": name, "protocol": "openai", "model_id": "model", "base_url": "http://localhost",
        "thinking": {"native": "never", "on": false, "returned": "none", "replay": "never"},
        "context_window": 2000, "max_output": 1000, "tuning": [],
    })
}

fn assert_errors(result: Result<LoadedRuntime, Vec<Violation>>, expected: &[&str]) {
    let actual: Vec<_> = result.unwrap_err().iter().map(ToString::to_string).collect();
    for text in expected {
        assert!(actual.iter().any(|error| error.contains(text)), "missing {text:?} in {actual:#?}");
    }
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn unknown_account(_: &str) -> io::Result<Option<u32>> {
    Ok(None)
}

fn arbitrary_id(_: &str) -> io::Result<Option<u32>> {
    Ok(Some(u32::MAX))
}

#[test]
fn reports_missing_system_user_and_skips_lookup_in_dev_mode() {
    let fixture = Fixture::new();
    assert_errors(
        fixture.check_with(Accounts { user: unknown_account, group: unknown_account }),
        &["system_user \"alice-os\" is missing from passwd", "system group gllmh does not exist"],
    );
    assert!(fixture.check().is_ok());
}

#[test]
fn rejects_shared_system_user_even_in_dev_mode() {
    let fixture = Fixture::new();
    fixture.write("runtime/test.json", runtime(&["alice", "bob"]));
    fixture.write("user/bob.json", json!({"name": "bob", "system_user": "alice-os"}));
    assert_errors(fixture.check(), &["users \"alice\" and \"bob\" share system_user \"alice-os\""]);
}

#[test]
fn rejects_distinct_account_names_with_same_uid() {
    let fixture = Fixture::new();
    fixture.write("runtime/test.json", runtime(&["alice", "bob"]));
    fixture.write("user/bob.json", json!({"name": "bob", "system_user": "bob-os"}));
    assert_errors(
        fixture.check_with(Accounts { user: arbitrary_id, group: unknown_account }),
        &["share UID", "system_user aliases \"alice-os\" and \"bob-os\""],
    );
    assert!(fixture.check().is_ok());
}

#[test]
fn rejects_colons_in_user_names() {
    let fixture = Fixture::new();
    fixture.write("runtime/test.json", runtime(&["bad:user"]));
    fixture.write("user/bad:user.json", json!({"name": "bad:user", "system_user": "alice-os"}));
    assert_errors(fixture.check(), &["user profile: invalid name \"bad:user\""]);
}

#[test]
fn rejects_colons_in_service_names() {
    let fixture = Fixture::new();
    fixture.write(
        "user/alice.json",
        json!({
            "name": "alice", "system_user": "alice-os",
            "services": {"bad:service": {"command": "/bin/true", "args": [], "restart": "never", "required": false}},
        }),
    );
    assert_errors(fixture.check(), &["service of user \"alice\": invalid name \"bad:service\""]);
}

#[test]
fn rejects_unknown_and_malformed_senders_in_both_edge_maps() {
    let fixture = Fixture::new();
    let mut profile = runtime(&["alice"]);
    profile["allow_messages"] = json!({"nobody": [], "alice::unknown": [], "alice::bad::name": []});
    profile["allow_directives"] = json!({"nobody": []});
    fixture.write("runtime/test.json", profile);
    assert_errors(
        fixture.check(),
        &[
            "allow_messages: unknown or invalid sender \"nobody\"",
            "allow_messages: unknown or invalid sender \"alice::unknown\"",
            "allow_messages: unknown or invalid sender \"alice::bad::name\"",
            "allow_directives: unknown or invalid sender \"nobody\"",
        ],
    );
}

#[test]
fn rejects_non_user_recipients_in_both_edge_maps() {
    let fixture = Fixture::new();
    fixture.write(
        "user/alice.json",
        json!({
            "name": "alice", "system_user": "alice-os",
            "services": {"task": {"command": "/bin/true", "args": [], "restart": "never", "required": false}},
        }),
    );
    let mut profile = runtime(&["alice"]);
    profile["allow_messages"] = json!({"alice": ["alice::task"]});
    profile["allow_directives"] = json!({"alice": ["other"]});
    fixture.write("runtime/test.json", profile);
    assert_errors(
        fixture.check(),
        &[
            "allow_messages: sender \"alice\" has unknown recipient \"alice::task\"",
            "allow_directives: sender \"alice\" has unknown recipient \"other\"",
        ],
    );
}

#[test]
fn rejects_directive_edges_to_users_without_composers() {
    let fixture = Fixture::new();
    let mut profile = runtime(&["alice"]);
    profile["allow_directives"] = json!({"alice": ["alice"]});
    fixture.write("runtime/test.json", profile);
    assert_errors(fixture.check(), &["allow_directives: recipient \"alice\" of \"alice\" needs a composer_profile"]);
}

#[test]
fn rejects_message_edges_to_builtin_directive_agents() {
    let fixture = Fixture::new();
    let mut settings = composer("builtin");
    settings["builtin"] = json!({"mode": "directive"});
    fixture.make_agent(settings);
    let mut profile = runtime(&["alice"]);
    profile["allow_messages"] = json!({"alice": ["alice"]});
    fixture.write("runtime/test.json", profile);
    assert_errors(
        fixture.check(),
        &["allow_messages: recipient \"alice\" of \"alice\" is a builtin directive-mode agent"],
    );
}

#[test]
fn rejects_model_and_tool_selections_without_a_composer() {
    let fixture = Fixture::new();
    fixture.write(
        "user/alice.json",
        json!({
            "name": "alice", "system_user": "alice-os", "llm_profiles": [], "tool_profiles": "*",
        }),
    );
    assert_errors(
        fixture.check(),
        &["llm_profiles requires composer_profile", "tool_profiles requires composer_profile"],
    );
}

#[test]
fn reports_all_unknown_profile_references() {
    let fixture = Fixture::new();
    fixture.write("runtime/test.json", runtime(&["alice", "missing-user"]));
    fixture.write(
        "user/alice.json",
        json!({
            "name": "alice", "system_user": "alice-os", "composer_profile": "missing-composer",
            "llm_profiles": ["missing-model"], "tool_profiles": ["missing-tool"],
        }),
    );
    assert_errors(
        fixture.check(),
        &[
            "user/missing-user.json",
            "composer/missing-composer.json",
            "llm/missing-model.json",
            "tool/missing-tool.json",
        ],
    );
    assert_errors(validate(fixture.dir.path(), "missing-runtime", true), &["runtime/missing-runtime.json"]);
}

#[test]
fn rejects_mismatched_profile_names_duplicate_membership_and_path_references() {
    let fixture = Fixture::new();
    fixture.write("runtime/test.json", runtime(&["alice", "alice", "../bob"]));
    fixture.write("user/alice.json", json!({"name": "other", "system_user": "alice-os"}));
    assert_errors(
        fixture.check(),
        &[
            "runtime lists user \"alice\" more than once",
            "profile name \"other\" must match file name \"alice\"",
            "user profile reference \"../bob\" is not a single file name",
        ],
    );
}

#[test]
fn allows_parallel_turns_only_for_builtin_directive_or_custom() {
    let fixture = Fixture::new();
    let mut profile = composer("builtin");
    profile["max_parallel_turns"] = json!(2);
    fixture.make_agent(profile.clone());
    assert_errors(fixture.check(), &["max_parallel_turns > 1 requires builtin directive mode or a custom engine"]);

    profile["builtin"] = json!({"mode": "directive"});
    fixture.make_agent(profile.clone());
    assert!(fixture.check().is_ok());

    profile["engine"] = json!("custom");
    profile["custom_composer_bin"] = json!("/bin/composer");
    fixture.make_agent(profile);
    assert!(fixture.check().is_ok());
}

#[test]
fn rejects_custom_composer_without_binary() {
    let fixture = Fixture::new();
    fixture.make_agent(composer("custom"));
    assert_errors(fixture.check(), &["custom engine requires custom_composer_bin"]);
}

#[test]
fn validates_all_tool_field_combinations() {
    let fixture = Fixture::new();
    fixture.make_agent(composer("builtin"));
    fixture.write(
        "user/alice.json",
        json!({
            "name": "alice", "system_user": "alice-os", "composer_profile": "work", "tool_profiles": "*",
        }),
    );

    for combination in 0..16 {
        let mut profile = tool("tool");
        for (bit, field, value) in [
            (1, "builtin", json!("send_message")),
            (2, "mcp", json!({"server": "/srv/mcp.sock", "tools": "*"})),
            (4, "tool_bin", json!("/bin/tool")),
            (8, "tool_schema", json!("/srv/tool.json")),
        ] {
            if combination & bit != 0 {
                profile[field] = value;
            }
        }
        fixture.write("tool/tool.json", profile);
        if [1, 2, 12].contains(&combination) {
            assert!(fixture.check().is_ok(), "valid combination {combination}");
        } else {
            assert_errors(fixture.check(), &["expected exactly one of builtin, mcp, or tool_bin with tool_schema"]);
        }
    }
}

#[test]
fn checks_config_file_and_directory_modes_and_group_outside_dev() {
    let fixture = Fixture::new();
    let dir = fixture.dir.path();
    set_mode(dir, 0o700);
    set_mode(&dir.join("runtime"), 0o755);
    set_mode(&dir.join("runtime/test.json"), 0o644);
    let accounts = Accounts { user: arbitrary_id, group: arbitrary_id };
    assert_errors(
        fixture.check_with(accounts),
        &[
            "expected mode 0750, found 0700",
            "expected mode 0750, found 0755",
            "expected mode 0640, found 0644",
            "expected group gllmh",
        ],
    );
    assert!(fixture.check().is_ok());
}

#[test]
fn checks_api_key_mode_and_owner_outside_dev() {
    let fixture = Fixture::new();
    fixture.make_agent(composer("builtin"));
    fixture.write(
        "user/alice.json",
        json!({
            "name": "alice", "system_user": "alice-os", "composer_profile": "work", "llm_profiles": ["model"],
        }),
    );
    let mut profile = llm("model");
    let path = fixture.dir.path().join("key");
    profile["api_key_file"] = json!(path);
    fixture.write("llm/model.json", profile);
    fs::write(&path, "test key").unwrap();
    set_mode(&path, 0o600);
    assert_errors(
        fixture.check_with(Accounts { user: arbitrary_id, group: arbitrary_id }),
        &["expected mode 0400, found 0600", "expected api_key_file owner \"alice-os\""],
    );
    fs::remove_file(path).unwrap();
    assert_errors(fixture.check_with(Accounts { user: arbitrary_id, group: arbitrary_id }), &["key\": No such file"]);
    assert!(fixture.check().is_ok());
}

#[test]
fn loads_section_seven_defaults_and_resolves_wildcards_and_ordered_llms() {
    let fixture = Fixture::new();
    fixture.make_agent(composer("builtin"));
    fixture.write(
        "user/alice.json",
        json!({
            "name": "alice", "system_user": "alice-os", "composer_profile": "work",
            "llm_profiles": ["z", "a"], "tool_profiles": "*",
        }),
    );
    fixture.write("llm/a.json", llm("a"));
    fixture.write("llm/z.json", llm("z"));
    assert!(fixture.check().unwrap().users["alice"].tool_profiles.is_empty());
    fixture.write("tool/a.json", json!({"name": "a", "hazardness": "safe", "builtin": "send_message"}));
    fixture.write(
        "tool/z.json",
        json!({"name": "z", "hazardness": "safe", "mcp": {
            "server": "/srv/mcp.sock", "tools": ["read"],
        }}),
    );
    fs::write(fixture.dir.path().join("tool/README"), "ignored").unwrap();

    let loaded = fixture.check().unwrap();
    let runtime = loaded.runtime;
    assert_eq!(runtime.child_process_shutdown_grace_s, 10);
    assert_eq!(runtime.max_hops, 16);
    assert_eq!(runtime.interaction_ttl_s, 86_400);
    assert_eq!(runtime.max_body_bytes, 1_048_576);
    assert_eq!(runtime.broker_db_path, Path::new("/var/lib/gllmh/broker.db"));

    let user = &loaded.users["alice"];
    assert_eq!(user.user_data_store, Path::new("~/.gllmh"));
    let composer = user.composer_profile.as_ref().unwrap();
    assert_eq!(composer.max_steps_per_turn, 20);
    assert_eq!(composer.max_parallel_turns, 1);
    assert_eq!(composer.compose_timeout_s, 60);
    let builtin = composer.builtin_settings().unwrap();
    assert_eq!(builtin.mode, BuiltinMode::Message);
    assert_eq!(builtin.budget_fraction, 0.6);

    assert_eq!(user.llm_profiles.iter().map(|llm| llm.name.as_str()).collect::<Vec<_>>(), ["z", "a"]);
    assert_eq!(user.llm_profiles[0].protocol, LlmProtocol::OpenAi);
    assert_eq!(user.llm_profiles[0].timeout_s, 300);
    assert_eq!(user.llm_profiles[0].max_retries, 3);
    assert_eq!(user.tool_profiles.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["a", "z"]);
    assert_eq!(user.tool_profiles[0].timeout_s, 120);
}

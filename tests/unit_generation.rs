use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gllmh::profile::{LoadedRuntime, Restart, validate};
use gllmh::units::generate;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn build() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gllmh"));
    command.args(["build", "--profile", "acceptance", "--dev", "--config-dir"]).arg(fixture("acceptance"));
    command.env("HOME", "/home/gllmh-test");
    command
}

fn success(output: Output) -> String {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn read_units(dir: &Path) -> BTreeMap<String, String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name().into_string().unwrap(), fs::read_to_string(entry.path()).unwrap())
        })
        .collect()
}

#[test]
fn acceptance_build_matches_golden_files_in_both_output_modes() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("units");
    let golden = read_units(&fixture("units"));
    assert!(success(build().arg("--out").arg(&out).output().unwrap()).is_empty());
    assert_eq!(read_units(&out), golden);

    let stdout = success(build().output().unwrap());
    let printed = stdout
        .trim_start_matches("# ")
        .split("\n\n# ")
        .map(|block| {
            let (name, contents) = block.split_once('\n').unwrap();
            (name.to_owned(), format!("{}\n", contents.trim_end_matches('\n')))
        })
        .collect::<BTreeMap<_, _>>();
    assert!(stdout.starts_with("# gllmh.target\n"));
    assert_eq!(printed, golden);
}

#[test]
fn invalid_profile_reports_all_violations_before_writing_units() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("runtime")).unwrap();
    fs::write(
        dir.path().join("runtime/invalid.json"),
        r#"{"name":"invalid","users":[],"restart":"never","allow_messages":{"nobody":["elsewhere"]}}"#,
    )
    .unwrap();

    let out = dir.path().join("units");
    let output = Command::new(env!("CARGO_BIN_EXE_gllmh"))
        .args(["build", "--profile", "invalid", "--dev", "--config-dir"])
        .arg(dir.path())
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown or invalid sender \"nobody\""), "{stderr}");
    assert!(stderr.contains("unknown recipient \"elsewhere\""), "{stderr}");
    assert!(!out.exists());
}

fn acceptance_with_absolute_paths() -> LoadedRuntime {
    let mut loaded = validate(&fixture("acceptance"), "acceptance", true).unwrap();
    for user in loaded.users.values_mut() {
        user.user_data_store = PathBuf::from("/home/gllmh-test/.gllmh");
    }
    loaded
}

#[test]
fn services_select_required_by_or_wanted_by_and_preserve_arguments() {
    let mut loaded = acceptance_with_absolute_paths();
    let agent = loaded.users.get_mut("agent").unwrap();
    agent.cwd = Some("/work/valet directory".into());
    agent.user_data_store = "/data/agent state".into();
    let timer = agent.services.get_mut("timer").unwrap();
    timer.cwd = Some("/work/service directory".into());
    timer.restart = Restart::Never;
    timer.args = ["", "two words", "a\"b", "a'b", r"a\b", ";", "${HOME}", "%n", "a\nb"].map(String::from).to_vec();
    loaded.users.get_mut("dispatch").unwrap().services.get_mut("draft_dispatcher").unwrap().required = false;
    let units = generate(&loaded, Path::new("/bin with spaces/gllmh")).into_iter().collect::<BTreeMap<_, _>>();
    let timer = &units["gllmh-service-agent::timer.service"];
    assert!(timer.ends_with("[Install]\nRequiredBy=gllmh.target\n"));
    assert!(timer.contains("WorkingDirectory=/work/service directory\n"));
    assert!(timer.contains("Restart=no\n"));
    assert!(timer.contains("Environment=\"GLLMH_SERVICE_SOCKET=/data/agent state/services/timer.sock\"\n"));
    assert!(
        timer
            .contains(r#"ExecStart=/usr/bin/gllmh-timer "" "two words" "a\"b" "a'b" "a\\b" ";" $${HOME} %%n "a\x0ab""#)
    );
    let dispatch = &units["gllmh-service-dispatch::draft_dispatcher.service"];
    assert!(dispatch.ends_with("[Install]\nWantedBy=gllmh.target\n"));
    assert!(!dispatch.contains("WorkingDirectory="));
    let valet = &units["gllmh-valet-agent.service"];
    assert!(valet.contains("ExecStart=\"/bin with spaces/gllmh\" valet --user agent\n"));
    assert!(valet.contains("WorkingDirectory=/work/valet directory\n"));
}

#[test]
fn systemd_accepts_generated_units_including_double_colons() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = acceptance_with_absolute_paths();
    let bin = Path::new(env!("CARGO_BIN_EXE_gllmh"));
    for service in loaded.users.values_mut().flat_map(|user| user.services.values_mut()) {
        service.command = bin.to_str().unwrap().into();
        service.args = vec![";".into(), "two words".into(), "$HOME".into(), "%n".into()];
    }

    let mut verify = Command::new("systemd-analyze");
    verify.args(["--man=no", "--recursive-errors=yes", "verify"]);
    verify.env("SYSTEMD_UNIT_PATH", dir.path());
    // Provide the verifier's implicit dependencies without relying on the host unit graph.
    for name in ["basic.target", "sysinit.target", "shutdown.target", "sockets.target"] {
        fs::write(dir.path().join(name), "[Unit]\nDefaultDependencies=no\n").unwrap();
    }
    for (name, contents) in generate(&loaded, bin) {
        let path = dir.path().join(&name);
        fs::write(&path, &contents).unwrap();
        // Load installed services through the target. The verify CLI interprets colons
        // in a positional path as alias syntax, while the unit loader accepts them.
        for (install, dependency) in
            [("RequiredBy=gllmh.target", "gllmh.target.requires"), ("WantedBy=gllmh.target", "gllmh.target.wants")]
        {
            if contents.lines().any(|line| line == install) {
                let dependency_dir = dir.path().join(dependency);
                fs::create_dir_all(&dependency_dir).unwrap();
                symlink(Path::new("..").join(&name), dependency_dir.join(&name)).unwrap();
            }
        }
        if !name.contains(':') {
            verify.arg(path);
        }
    }
    match verify.output() {
        Err(err) if err.kind() == ErrorKind::NotFound => eprintln!("skipped: systemd-analyze is not installed"),
        result => {
            let output = result.unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success() && stderr.is_empty(), "{stderr}");
        }
    }
}

//! Build example binaries (the acceptance services, example composers) once
//! per test binary and return their paths.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

static BUILT: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

/// Path of the built example `name`, building it on first use.
pub fn example(name: &str) -> PathBuf {
    let built = BUILT.get_or_init(Default::default);
    let mut built = built.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(path) = built.get(name) {
        return path.clone();
    }
    let output = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["build", "--locked", "--example", name, "--message-format=json"])
        .output()
        .expect("run cargo build --example");
    assert!(output.status.success(), "example {name} failed to build: {}", String::from_utf8_lossy(&output.stderr));
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|m| {
            (m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
                .then(|| m["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .unwrap_or_else(|| panic!("no executable reported for example {name}"));
    built.insert(name.to_string(), path.clone());
    path
}

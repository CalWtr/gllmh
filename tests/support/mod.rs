//! Shared helpers for integration tests. Each test binary includes this with
//! `mod support;`. Add helpers in their own file here (e.g. `fake_llm.rs`,
//! `dev.rs`) and declare them below; keep additions additive.

#![allow(dead_code)]

pub mod dev;

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Path of the built `gllmh` binary.
pub fn gllmh_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gllmh"))
}

/// Path of a checked-in fixture under tests/fixtures/.
pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(rel)
}

/// Wait until `path` exists, or panic after `timeout`.
pub async fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub mod fake_llm;

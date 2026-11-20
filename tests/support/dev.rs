//! A dev-mode GLLMH in a tempdir: one broker and any number of valets, all
//! as the calling user (spec section 2, `gllmh serve --dev`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use gllmh::frame::{Incoming, Peer};
use gllmh::store::ReadOnlyStore;
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use super::gllmh_bin;

const READY_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Dev {
    pub dir: TempDir,
    pub config: PathBuf,
    pub profile: String,
    broker: Option<Child>,
    valets: BTreeMap<String, Child>,
    /// Extra environment for every valet, e.g. to point an llm_profile somewhere.
    pub valet_env: Vec<(String, String)>,
}

impl Dev {
    /// A dev runtime for `profile` under `config`; nothing is started yet.
    pub fn new(config: impl AsRef<Path>, profile: &str) -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            config: config.as_ref().to_path_buf(),
            profile: profile.to_string(),
            broker: None,
            valets: BTreeMap::new(),
            valet_env: Vec::new(),
        }
    }

    pub fn run_dir(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    pub fn db(&self) -> PathBuf {
        self.dir.path().join("broker.db")
    }

    pub fn data_root(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn data_dir(&self, user: &str) -> PathBuf {
        self.data_root().join(user)
    }

    pub fn user_socket(&self, user: &str) -> PathBuf {
        gllmh::paths::user_socket(&self.run_dir(), user)
    }

    pub fn local_socket(&self, user: &str) -> PathBuf {
        gllmh::paths::local_socket(&self.data_dir(user))
    }

    /// Start (or restart) the broker and wait until `user`'s socket accepts.
    pub async fn start_broker(&mut self, any_user: &str) {
        self.stop_broker().await;
        let child = Command::new(gllmh_bin())
            .args(["serve", "--dev", "--profile", &self.profile])
            .arg("--config-dir")
            .arg(&self.config)
            .arg("--run-dir")
            .arg(self.run_dir())
            .arg("--db")
            .arg(self.db())
            .env("GLLMH_LOG", log_level())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn broker");
        self.broker = Some(child);
        wait_connectable(&self.user_socket(any_user)).await;
    }

    pub async fn stop_broker(&mut self) {
        if let Some(mut child) = self.broker.take() {
            let _ = child.kill().await;
        }
    }

    /// Start (or restart) `user`'s valet and wait until its local.sock accepts.
    pub async fn start_valet(&mut self, user: &str) {
        self.kill_valet(user).await;
        let mut cmd = Command::new(gllmh_bin());
        cmd.args(["valet", "--dev", "--user", user])
            .arg("--run-dir")
            .arg(self.run_dir())
            .arg("--data-root")
            .arg(self.data_root())
            .env("GLLMH_LOG", log_level())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for (k, v) in &self.valet_env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn valet");
        self.valets.insert(user.to_string(), child);
        wait_connectable(&self.local_socket(user)).await;
    }

    /// SIGKILL a valet, as a crash would.
    pub async fn kill_valet(&mut self, user: &str) {
        if let Some(mut child) = self.valets.remove(user) {
            let _ = child.kill().await;
        }
    }

    /// True while `user`'s valet process is still running.
    pub fn valet_running(&mut self, user: &str) -> bool {
        self.valets.get_mut(user).is_some_and(|c| c.try_wait().ok().flatten().is_none())
    }

    /// A controller connected to `user`'s local.sock.
    pub async fn client(&self, user: &str) -> (Peer, Incoming) {
        let stream = UnixStream::connect(self.local_socket(user)).await.expect("connect local.sock");
        Peer::spawn_unix(stream)
    }

    /// A raw connection on `user`'s broker socket, as a valet would hold.
    pub async fn broker_line(&self, user: &str) -> (Peer, Incoming) {
        let stream = UnixStream::connect(self.user_socket(user)).await.expect("connect broker socket");
        Peer::spawn_unix(stream)
    }

    /// Open `user`'s store read-only.
    pub fn store(&self, user: &str) -> ReadOnlyStore {
        ReadOnlyStore::open(&gllmh::paths::store_db(&self.data_dir(user))).expect("open store")
    }
}

fn log_level() -> String {
    std::env::var("GLLMH_TEST_LOG").unwrap_or_else(|_| "warn".into())
}

/// Wait until a unix socket accepts a connection (a stale file does not count).
pub async fn wait_connectable(path: &Path) {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if UnixStream::connect(path).await.is_ok() {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {}", path.display());
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll `f` until it returns Some, or panic after `timeout`.
pub async fn eventually<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "condition not met within {timeout:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll an async `f` until it returns Some, or panic after `timeout`.
pub async fn eventually_async<T, Fut>(timeout: Duration, mut f: impl FnMut() -> Fut) -> T
where
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "condition not met within {timeout:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

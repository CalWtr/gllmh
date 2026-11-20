//! The `valet`: one user's long-running process (spec sections 2, 5, 6).
//!
//! It holds the user's one connection to the broker, records every action in
//! the user's store before running it, serves `local.sock` to UIs and
//! controllers, and (for an agent) runs turns.
//!
//! Layout, and which task fills what:
//!
//! - `mod.rs` (M3.2): startup, the [`Valet`] shared state, the main loop.
//! - `store.rs` (M3.2): async access to the synchronous store.
//! - `send.rs` (M3.2): the one path every send takes (write-ahead, edge pre-check, broker).
//! - `deliver.rs` (M3.2): recording deliveries; hands agent receives to the turn engine.
//! - `local.rs` (M3.2): local.sock; M7.1 adds the service sockets beside it.
//! - `recovery.rs` (M3.2): sends and actions. Turn recovery is in `turns.rs`.
//! - `composer.rs` (M4.3): the composer child process.
//! - `turns.rs` (M4.3): the turn engine. M6.1 sends results from its `on_turn_end` hook.
//! - `tools.rs` (M4.3 seam): tool names, schemas and running a tool request; M5.1, M5.3 fill it.
//!
//! The valet's lifetime is bound to its broker connection: when the broker
//! goes away, the valet exits non-zero and systemd restarts it, which runs
//! recovery (docs/decisions.md).

mod composer;
mod deliver;
mod local;
mod recovery;
mod send;
mod store;
mod tools;
mod turns;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use tokio::net::UnixStream;
use tokio::sync::watch;

use crate::frame::{ErrorBody, Incoming, Peer, codes};
use crate::paths::{self, RunDirArg};
use crate::profile::ResolvedUser;
use crate::protocol::{self, broker};
use crate::types::Sender;

pub use local::Clients;
pub use send::SendRequest;
pub use store::StoreHandle;
pub use tools::{ToolCtx, ToolUse};
pub use turns::TurnEnd;

/// `gllmh valet --user <name>`.
#[derive(Debug, Clone, clap::Args)]
pub struct ValetArgs {
    /// The user this valet runs for.
    #[arg(long)]
    pub user: String,
    /// Dev mode: everyone runs as the caller; service cgroup checks are skipped.
    #[arg(long)]
    pub dev: bool,
    /// Use `<data-root>/<user>` as the user_data_store instead of the profile's.
    /// Needed in dev mode, where every user shares one home directory.
    #[arg(long, env = "GLLMH_DATA_ROOT")]
    pub data_root: Option<PathBuf>,
    #[command(flatten)]
    pub run: RunDirArg,
}

/// State shared by every part of one valet.
pub struct Valet {
    pub user: String,
    pub profile: ResolvedUser,
    pub edges: BTreeMap<Sender, broker::Edges>,
    pub data_dir: PathBuf,
    pub dev: bool,
    pub store: StoreHandle,
    pub broker: Peer,
    pub clients: Clients,
    /// Flips to true once recovery is done; deliveries wait for it.
    recovered: watch::Sender<bool>,
    /// Wakes the turn engine when a turn is queued or ends.
    wake: tokio::sync::Notify,
}

impl Valet {
    /// True when this user has a composer_profile.
    pub fn is_agent(&self) -> bool {
        self.profile.composer_profile.is_some()
    }

    /// Wait until recovery has finished.
    pub async fn wait_recovered(&self) {
        let mut rx = self.recovered.subscribe();
        let _ = rx.wait_for(|done| *done).await;
    }
}

pub async fn run(args: ValetArgs) -> anyhow::Result<()> {
    let socket = paths::user_socket(&args.run.run_dir, &args.user);
    let stream =
        UnixStream::connect(&socket).await.with_context(|| format!("connect to broker at {}", socket.display()))?;
    let (peer, incoming) = Peer::spawn_unix(stream);

    let hello: broker::HelloOutput = peer
        .request(broker::HELLO, &broker::HelloInput { protocol: protocol::PROTOCOL_VERSION })
        .await
        .context("hello")?;
    if hello.user != args.user {
        bail!("broker says this socket belongs to {:?}, not {:?}", hello.user, args.user);
    }
    let profile: ResolvedUser = serde_json::from_value(hello.profile).context("resolved profile from hello")?;
    let data_dir = match &args.data_root {
        Some(root) => root.join(&args.user),
        None => paths::expand_home(&profile.user_data_store, &home_dir()),
    };
    let store = StoreHandle::open(&data_dir).await.with_context(|| format!("open store in {}", data_dir.display()))?;

    let (recovered, _) = watch::channel(false);
    let valet = Arc::new(Valet {
        user: args.user.clone(),
        profile,
        edges: hello.edges,
        data_dir: data_dir.clone(),
        dev: args.dev,
        store,
        broker: peer,
        clients: Clients::default(),
        recovered,
        wake: tokio::sync::Notify::new(),
    });

    // Deliveries are accepted from here on but wait for recovery (section 5, step 2).
    let broker_loop = tokio::spawn(serve_broker(valet.clone(), incoming));

    recovery::recover(&valet).await.context("recovery")?;
    let turns = turns::start(&valet).await.context("start turn engine")?;
    let _local = local::listen(valet.clone(), &paths::local_socket(&data_dir)).await?;
    valet.recovered.send_replace(true);

    if let Err(e) = crate::sdnotify::ready() {
        tracing::warn!("sd_notify failed: {e}");
    }
    tracing::info!(user = %valet.user, data_dir = %data_dir.display(), "valet ready");

    tokio::select! {
        r = broker_loop => {
            r.context("broker loop panicked")?;
            bail!("broker connection closed")
        }
        r = turns.exited() => r,
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

/// Answer the broker's requests until the connection closes.
async fn serve_broker(valet: Arc<Valet>, mut incoming: Incoming) {
    while let Some(req) = incoming.next().await {
        match req.op.as_str() {
            broker::DELIVER => {
                let valet = valet.clone();
                tokio::spawn(async move {
                    if let Some((input, responder)) = req.parse::<broker::DeliverInput>() {
                        deliver::handle(&valet, input.interaction, responder).await;
                    }
                });
            }
            other => {
                let msg = format!("the valet does not serve {other:?} on the broker line");
                req.responder.err(ErrorBody::new(codes::UNKNOWN_OP, msg));
            }
        }
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// The calling user's home directory, for expanding `~` in user_data_store.
/// systemd sets $HOME for `User=` units.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| Path::new("/").to_path_buf())
}

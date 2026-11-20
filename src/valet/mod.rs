//! The `valet`: one user's long-running process (spec sections 2, 5, 6).
//!
//! Owner: M3.2 (core), M4.3 (turn engine), M5.x (tools), M6.1 (results),
//! M7.1 (services). M3.2 lays out the submodules the later tasks fill.

use crate::paths::RunDirArg;

/// `gllmh valet --user <name>`.
#[derive(Debug, Clone, clap::Args)]
pub struct ValetArgs {
    /// The user this valet runs for.
    #[arg(long)]
    pub user: String,
    /// Dev mode: everyone runs as the caller; service cgroup checks are skipped.
    #[arg(long)]
    pub dev: bool,
    #[command(flatten)]
    pub run: RunDirArg,
}

pub async fn run(_args: ValetArgs) -> anyhow::Result<()> {
    anyhow::bail!("gllmh valet: not implemented (M3.2)")
}

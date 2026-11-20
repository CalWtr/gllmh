//! `gllmh serve`: sockets and the operations of spec section 6.1.
//!
//! Owner: M2.2.

use crate::paths::{ConfigDirArg, RunDirArg};

/// `gllmh serve --profile <name> [--dev]`.
#[derive(Debug, Clone, clap::Args)]
pub struct ServeArgs {
    /// The runtime_profile to serve.
    #[arg(long)]
    pub profile: String,
    /// Create the user sockets itself, run everyone as the caller, skip identity checks.
    #[arg(long)]
    pub dev: bool,
    #[command(flatten)]
    pub config: ConfigDirArg,
    #[command(flatten)]
    pub run: RunDirArg,
}

pub async fn serve(_args: ServeArgs) -> anyhow::Result<()> {
    anyhow::bail!("gllmh serve: not implemented (M2.2)")
}

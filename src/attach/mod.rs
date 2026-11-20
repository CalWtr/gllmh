//! `gllmh attach`: a line-oriented terminal UI on the caller's local.sock
//! (spec section 6.3).
//!
//! Owner: M3.3.

use std::path::PathBuf;

/// `gllmh attach`.
#[derive(Debug, Clone, clap::Args)]
pub struct AttachArgs {
    /// The valet's local.sock. Default: ~/.gllmh/local.sock.
    #[arg(long)]
    pub socket: Option<PathBuf>,
}

pub async fn run(_args: AttachArgs) -> anyhow::Result<()> {
    anyhow::bail!("gllmh attach: not implemented (M3.3)")
}

/// `gllmh status <root>`: one run's interactions, through the caller's valet.
///
/// Owner: M6.2.
#[derive(Debug, Clone, clap::Args)]
pub struct StatusArgs {
    /// The run's root interaction id.
    pub root: ulid::Ulid,
    /// The valet's local.sock. Default: ~/.gllmh/local.sock.
    #[arg(long)]
    pub socket: Option<PathBuf>,
}

pub async fn status(_args: StatusArgs) -> anyhow::Result<()> {
    anyhow::bail!("gllmh status: not implemented (M6.2)")
}

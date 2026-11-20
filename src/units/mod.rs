//! systemd unit generation of spec section 7 (Unit files): `gllmh build`.
//!
//! Owner: M1.2.

use std::path::PathBuf;

use crate::paths::{ConfigDirArg, DEFAULT_BIN};

/// `gllmh build --profile <name>`.
#[derive(Debug, Clone, clap::Args)]
pub struct BuildArgs {
    /// The runtime_profile to turn into units.
    #[arg(long)]
    pub profile: String,
    /// Write one file per unit into this directory instead of printing them.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Path of the gllmh binary in ExecStart= lines.
    #[arg(long, default_value = DEFAULT_BIN)]
    pub bin: PathBuf,
    #[command(flatten)]
    pub config: ConfigDirArg,
}

pub async fn build(_args: BuildArgs) -> anyhow::Result<()> {
    anyhow::bail!("gllmh build: not implemented (M1.2)")
}

//! Profiles of spec section 7: types, loading, resolution and `gllmh check`.
//!
//! Owner: M1.1. Unit generation (`gllmh build`) is `crate::units` (M1.2).

use crate::paths::ConfigDirArg;

/// `gllmh check --profile <name>`.
#[derive(Debug, Clone, clap::Args)]
pub struct CheckArgs {
    /// The runtime_profile to validate.
    #[arg(long)]
    pub profile: String,
    /// Skip the checks that need real system users and file modes.
    #[arg(long)]
    pub dev: bool,
    #[command(flatten)]
    pub config: ConfigDirArg,
}

pub async fn check(_args: CheckArgs) -> anyhow::Result<()> {
    anyhow::bail!("gllmh check: not implemented (M1.1)")
}

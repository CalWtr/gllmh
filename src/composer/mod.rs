//! The builtin `composer` (spec section 7, Builtin composer), run as the
//! hidden subcommand `gllmh composer`, speaking section 6.2 on stdin/stdout.
//!
//! Owner: M4.2.

/// `gllmh composer` (hidden; started by the valet).
#[derive(Debug, Clone, clap::Args)]
pub struct ComposerArgs {}

pub async fn run(_args: ComposerArgs) -> anyhow::Result<()> {
    anyhow::bail!("gllmh composer: not implemented (M4.2)")
}

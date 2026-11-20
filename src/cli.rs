//! The `gllmh` command line (spec section 6, Commands). Each subcommand's
//! arguments live in the module that implements it.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "gllmh", version, about = "General Large Language Model Harness")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// The broker.
    Serve(crate::broker::ServeArgs),
    /// One user's valet, started by its unit.
    Valet(crate::valet::ValetArgs),
    /// Validate a runtime_profile, its system users, names and file modes.
    Check(crate::profile::CheckArgs),
    /// Run check, then print the unit files for the profile.
    Build(crate::units::BuildArgs),
    /// A terminal UI on the caller's local.sock.
    Attach(crate::attach::AttachArgs),
    /// List the interactions of one run, without bodies.
    Status(crate::attach::StatusArgs),
    /// The MCP client behind MCP tools.
    Mcp(crate::mcp::McpArgs),
    /// The builtin composer; started by the valet.
    #[command(hide = true)]
    Composer(crate::composer::ComposerArgs),
}

/// Run the parsed command.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Serve(a) => crate::broker::serve(a).await,
        Command::Valet(a) => crate::valet::run(a).await,
        Command::Check(a) => crate::profile::check(a).await,
        Command::Build(a) => crate::units::build(a).await,
        Command::Attach(a) => crate::attach::run(a).await,
        Command::Status(a) => crate::attach::status(a).await,
        Command::Mcp(a) => crate::mcp::run(a).await,
        Command::Composer(a) => crate::composer::run(a).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }
}

//! The MCP client behind MCP tools: `gllmh mcp list` and `gllmh mcp call`
//! (spec section 7, MCP tools).
//!
//! Owner: M5.2.

/// `gllmh mcp <list|call>`.
#[derive(Debug, Clone, clap::Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum McpCommand {
    /// Print the server's tools as JSON: [{name, description, input_schema}].
    List {
        /// An http(s) URL, a unix socket path, or a command line for a stdio server.
        #[arg(long)]
        server: String,
    },
    /// Read {tool, arguments} on stdin, call it, print the result on stdout.
    Call {
        #[arg(long)]
        server: String,
    },
}

pub async fn run(_args: McpArgs) -> anyhow::Result<()> {
    anyhow::bail!("gllmh mcp: not implemented (M5.2)")
}

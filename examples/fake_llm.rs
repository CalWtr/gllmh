//! Run with `cargo run --example fake_llm -- script.json --port 8080`.
//! The script format is documented in tests/support/fake_llm.rs.

#[allow(dead_code)]
#[path = "../tests/support/fake_llm.rs"]
mod fake_llm;

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use fake_llm::{FakeLlm, Script};

#[derive(Parser)]
struct Args {
    /// JSON file containing the ordered response rules.
    script: PathBuf,
    /// Loopback port. Use 0 to let the OS select a free port.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let data = tokio::fs::read(&args.script).await.with_context(|| format!("read {}", args.script.display()))?;
    let script: Script = serde_json::from_slice(&data).with_context(|| format!("parse {}", args.script.display()))?;
    let server = FakeLlm::start_on_port(script, args.port).await.context("bind fake LLM")?;
    println!("{}", server.base_url());
    tokio::signal::ctrl_c().await.context("listen for Ctrl-C")
}

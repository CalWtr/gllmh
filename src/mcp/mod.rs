//! The MCP client behind MCP tools: `gllmh mcp list` and `gllmh mcp call`
//! (spec section 7, MCP tools).
//!
//! Owner: M5.2.

mod http;
mod transport;

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use transport::Transport;

const PROTOCOL_VERSION: &str = "2025-06-18";
const STEP_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Input(String),
    #[error("MCP {0} timed out")]
    Timeout(&'static str),
    #[error("MCP server closed the stream before replying")]
    EndOfStream,
    #[error("MCP I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not start MCP server {program:?}: {source}")]
    Spawn { program: String, source: std::io::Error },
    #[error("MCP JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("MCP HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MCP HTTP returned {0}")]
    HttpStatus(reqwest::StatusCode),
    #[error("MCP HTTP event stream: {0}")]
    EventStream(#[from] eventsource_stream::EventStreamError<reqwest::Error>),
    #[error("MCP protocol: {0}")]
    Protocol(String),
    #[error("MCP {method} failed ({code}): {message}")]
    Rpc { method: &'static str, code: i64, message: String },
}

/// A tool description in the format used by `gllmh mcp list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    #[serde(alias = "inputSchema")]
    pub input_schema: Value,
}

/// The MCP result, including tool-level failures (`is_error`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallResult {
    pub content: Vec<Value>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl CallResult {
    /// Concatenate content in order for the section 6.3 `run` stdout contract.
    pub fn output(&self) -> String {
        let mut output = String::new();
        for item in &self.content {
            if item["type"] == "text"
                && let Some(text) = item["text"].as_str()
            {
                output.push_str(text);
            } else {
                output.push_str(&item.to_string());
            }
        }
        output
    }
}

/// Open an MCP session and list all tools, in page order.
pub async fn list(server: &str) -> Result<Vec<ToolInfo>, Error> {
    let mut session = Session::connect(server).await?;
    let result = session.list_tools().await;
    session.transport.close().await;
    result
}

/// Open an MCP session and call one tool with a JSON object as its arguments.
pub async fn call(server: &str, tool: &str, args: Value) -> Result<CallResult, Error> {
    if !args.is_object() {
        return Err(Error::Input("MCP tool arguments must be a JSON object".into()));
    }
    let mut session = Session::connect(server).await?;
    let result = session.request("tools/call", json!({ "name": tool, "arguments": args })).await;
    session.transport.close().await;
    result
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPage {
    tools: Vec<ToolInfo>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
    protocol_version: String,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

struct Session {
    transport: Transport,
    next_id: u64,
}

impl Session {
    async fn connect(server: &str) -> Result<Self, Error> {
        let transport = step("connect", Transport::connect(server)).await?;
        let mut session = Self { transport, next_id: 1 };
        if let Err(error) = session.initialize().await {
            session.transport.close().await;
            return Err(error);
        }
        Ok(session)
    }

    async fn initialize(&mut self) -> Result<(), Error> {
        let result: InitializeResult = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "gllmh", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        if result.protocol_version != PROTOCOL_VERSION {
            return Err(Error::Protocol(format!("unsupported version {:?}", result.protocol_version)));
        }

        step(
            "notifications/initialized",
            self.transport.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
        )
        .await
    }

    async fn list_tools(&mut self) -> Result<Vec<ToolInfo>, Error> {
        let mut tools = Vec::new();
        let mut cursors = HashSet::new();
        let mut params = json!({});
        loop {
            let page: ListPage = self.request("tools/list", params).await?;
            tools.extend(page.tools);
            let Some(cursor) = page.next_cursor else {
                return Ok(tools);
            };
            params = json!({ "cursor": cursor });
            if !cursors.insert(cursor) {
                return Err(Error::Protocol("tools/list repeated a pagination cursor".into()));
            }
        }
    }

    async fn request<T: DeserializeOwned>(&mut self, method: &'static str, params: Value) -> Result<T, Error> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        step(method, async {
            let mut exchange = self.transport.request(request, method == "initialize").await?;
            loop {
                let mut message = exchange.next().await?;
                if message["jsonrpc"] != "2.0" {
                    return Err(Error::Protocol("expected a JSON-RPC 2.0 message".into()));
                }
                if let Some(server_method) = message["method"].as_str() {
                    if let Some(server_id) = message.get("id") {
                        let response = if server_method == "ping" {
                            json!({ "jsonrpc": "2.0", "id": server_id, "result": {} })
                        } else {
                            json!({
                                "jsonrpc": "2.0",
                                "id": server_id,
                                "error": { "code": -32601, "message": "Method not found" }
                            })
                        };
                        exchange.answer(response).await?;
                    }
                    continue;
                }
                if message["id"] != id {
                    continue;
                }
                if let Some(error) = message.get_mut("error") {
                    let RpcError { code, message } = serde_json::from_value(error.take())?;
                    return Err(Error::Rpc { method, code, message });
                }
                let result = message
                    .get_mut("result")
                    .ok_or_else(|| Error::Protocol(format!("{method} response has no result")))?;
                return Ok(serde_json::from_value(result.take())?);
            }
        })
        .await
    }
}

async fn step<T>(name: &'static str, work: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    timeout(STEP_TIMEOUT, work).await.map_err(|_| Error::Timeout(name))?
}

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

#[derive(Deserialize)]
struct CallInput {
    tool: String,
    arguments: Value,
}

pub async fn run(args: McpArgs) -> anyhow::Result<()> {
    match args.command {
        McpCommand::List { server } => {
            let tools = list(&server).await?;
            println!("{}", serde_json::to_string(&tools)?);
        }
        McpCommand::Call { server } => {
            let mut input = Vec::new();
            tokio::io::stdin().read_to_end(&mut input).await.context("reading MCP call stdin")?;
            let CallInput { tool, arguments } = serde_json::from_slice(&input).context("parsing MCP call stdin")?;
            let result = call(&server, &tool, arguments).await?;

            let mut stdout = tokio::io::stdout();
            stdout.write_all(result.output().as_bytes()).await.context("writing MCP call stdout")?;
            stdout.flush().await.context("flushing MCP call stdout")?;
            anyhow::ensure!(!result.is_error, "MCP tool {tool:?} returned isError");
        }
    }
    Ok(())
}

//! `gllmh attach`: a line-oriented terminal UI on the caller's local.sock
//! (spec section 6.3).
//!
//! Owner: M3.3.

use std::{io, path::PathBuf};

use anyhow::Context;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::UnixStream,
    sync::mpsc,
};
use ulid::Ulid;

use crate::{
    frame::{CallError, ErrorBody, Incoming, Peer, Request, codes},
    paths,
    protocol::local::{self, NotifyInput, NotifyOutput, StatusInput, StatusOutput, SubmitInput, SubmitOutput},
    types::{Block, InteractionKind},
};

/// `gllmh attach`.
#[derive(Debug, Clone, clap::Args)]
pub struct AttachArgs {
    /// The valet's local.sock. Default: ~/.gllmh/local.sock.
    #[arg(long)]
    pub socket: Option<PathBuf>,
}

pub async fn run(args: AttachArgs) -> anyhow::Result<()> {
    let socket = args.socket.unwrap_or_else(paths::default_local_socket);
    let stream = UnixStream::connect(&socket).await.with_context(|| format!("connect to {}", socket.display()))?;
    let (reader, writer) = stream.into_split();
    let (peer, incoming) = Peer::spawn(reader, writer);

    // A blocking terminal read can outlive the socket. Keep it outside Tokio's
    // blocking pool so runtime shutdown never waits for the next line of stdin.
    let (tx, mut rx) = mpsc::channel(1);
    std::thread::Builder::new().name("attach-stdin".into()).spawn(move || {
        for line in io::stdin().lines() {
            let failed = line.is_err();
            if tx.blocking_send(line).is_err() || failed {
                break;
            }
        }
    })?;

    session(async || rx.recv().await.transpose(), tokio::io::stdout(), peer, incoming).await
}

/// Runs attach over a line reader, an output writer, and a connected peer.
pub async fn session(
    mut read_line: impl AsyncFnMut() -> io::Result<Option<String>>,
    mut writer: impl AsyncWrite + Unpin,
    peer: Peer,
    mut incoming: Incoming,
) -> anyhow::Result<()> {
    loop {
        // Keep serving notifications even if the valet is waiting for a
        // notify acknowledgement before it answers the current command.
        let command = async {
            match read_line().await.context("read attach input")? {
                Some(line) => execute(&peer, &line).await,
                None => Ok(None),
            }
        };
        tokio::pin!(command);
        let output = loop {
            tokio::select! {
                biased;
                request = incoming.next() => {
                    let request = request.context("valet connection closed")?;
                    if let Some(output) = notify(request) {
                        print(&mut writer, &output).await?;
                    }
                }
                output = &mut command => break output?,
            }
        };
        let Some(output) = output else { return Ok(()) };
        print(&mut writer, &output).await?;
    }
}

async fn print(writer: &mut (impl AsyncWrite + Unpin), output: &str) -> io::Result<()> {
    writer.write_all(output.as_bytes()).await?;
    writer.flush().await
}

fn notify(request: Request) -> Option<String> {
    use std::fmt::Write;

    if request.op != local::NOTIFY {
        request.responder.err(ErrorBody::new(codes::UNKNOWN_OP, request.op));
        return None;
    }
    let (NotifyInput { interaction }, responder) = request.parse()?;
    let mut output = format!("{} {}", interaction.from, interaction.kind.as_str());
    if let Some(outcome) = interaction.turn_outcome {
        write!(output, " turn_outcome={}", outcome.as_str()).unwrap();
    }
    output.push('\n');
    if let Some(body) = interaction.body {
        writeln!(output, "{body}").unwrap();
    }
    for block in interaction.context.into_iter().flatten() {
        writeln!(output, "{}={:?}", block.name.escape_debug(), block.content).unwrap();
    }
    responder.ok(NotifyOutput {});
    Some(output)
}

const HELP: &str = "/msg <to> <text> | /dir <to> <name>=<content> … | /status <root> | /quit";

fn head_and_tail(line: &str) -> (&str, &str) {
    let (head, tail) = line.trim_start().split_once(char::is_whitespace).unwrap_or((line.trim_start(), ""));
    (head, tail.trim_start())
}

/// EXTENSION: terminal commands use shell-style quoting for directive blocks.
/// Submits have no causal parent.
async fn execute(peer: &Peer, line: &str) -> anyhow::Result<Option<String>> {
    let (command, args) = head_and_tail(line);
    let result = match command {
        "/msg" => {
            let (to, text) = head_and_tail(args);
            if text.is_empty() {
                return Ok(Some("Usage: /msg <to> <text>\n".into()));
            }
            submit(peer, InteractionKind::Message, to, Some(text.into()), None).await
        }
        "/dir" => {
            let (to, blocks) = head_and_tail(args);
            let Some(context) = parse_blocks(blocks) else {
                return Ok(Some("Usage: /dir <to> <name>=<content> … (quote spaces)\n".into()));
            };
            submit(peer, InteractionKind::Directive, to, None, Some(context)).await
        }
        "/status" => {
            let Ok(root) = args.trim_end().parse::<Ulid>() else {
                return Ok(Some("Usage: /status <root> (a ULID)\n".into()));
            };
            let status = peer.request(local::STATUS, &StatusInput { root }).await;
            status.map(|status| format_status(root, status))
        }
        "/quit" if args.is_empty() => return Ok(None),
        _ => return Ok(Some(format!("{HELP}\n"))),
    };
    match result {
        Ok(output) => Ok(Some(output)),
        Err(CallError::Remote(error)) => Ok(Some(format!("{error}\n"))),
        Err(error) => Err(error).context("valet request failed"),
    }
}

fn parse_blocks(blocks: &str) -> Option<Vec<Block>> {
    let words = shlex::split(blocks)?;
    if words.is_empty() {
        return None;
    }
    words
        .into_iter()
        .map(|word| {
            let (name, content) = word.split_once('=')?;
            if name.is_empty() {
                return None;
            }
            Some(Block { name: name.into(), content: content.into() })
        })
        .collect()
}

async fn submit(
    peer: &Peer,
    kind: InteractionKind,
    to: &str,
    body: Option<serde_json::Value>,
    context: Option<Vec<Block>>,
) -> Result<String, CallError> {
    let input = SubmitInput { kind, to: to.into(), body, context, caused_by: None };
    let SubmitOutput { id, root, hops } = peer.request(local::SUBMIT, &input).await?;
    Ok(format!("id={id} root={root} hops={hops}\n"))
}

fn format_status(root: Ulid, output: StatusOutput) -> String {
    use std::fmt::Write;

    let mut text = format!("root={root} interactions={}\n", output.interactions.len());
    for row in output.interactions {
        write!(
            text,
            "id={} hops={} {} {} -> {} state={}",
            row.id,
            row.hops,
            row.kind.as_str(),
            row.from,
            row.to,
            row.state.as_str()
        )
        .unwrap();
        if let Some(outcome) = row.outcome {
            write!(text, " outcome={}", outcome.as_str()).unwrap();
        }
        text.push('\n');
    }
    text
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

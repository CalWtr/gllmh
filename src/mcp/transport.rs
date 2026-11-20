//! Transport selection and newline-delimited JSON-RPC for local MCP servers.

use std::process::Stdio;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::timeout;

use super::http::{Http, HttpReply};
use super::{Error, SHUTDOWN_GRACE};

pub(super) enum Transport {
    Lines(Lines),
    Http(Http),
}

impl Transport {
    pub(super) async fn connect(server: &str) -> Result<Self, Error> {
        if server
            .split_once("://")
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        {
            return Ok(Self::Http(Http::new(server)?));
        }

        let path = match server.strip_prefix("unix:") {
            Some(path) => Some(path),
            None if tokio::fs::try_exists(server).await.unwrap_or(false) => Some(server),
            None => None,
        };
        if let Some(path) = path {
            let (reader, writer) = UnixStream::connect(path).await?.into_split();
            return Ok(Self::Lines(Lines::new(reader, writer, None)));
        }

        let argv = shell_words::split(server).map_err(|e| Error::Input(format!("invalid MCP server command: {e}")))?;
        let (program, args) = argv.split_first().ok_or_else(|| Error::Input("MCP server command is empty".into()))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| Error::Spawn { program: program.clone(), source })?;
        let reader = child.stdout.take().expect("spawned MCP child with piped stdout");
        let writer = child.stdin.take().expect("spawned MCP child with piped stdin");
        Ok(Self::Lines(Lines::new(reader, writer, Some(child))))
    }

    pub(super) async fn request(&mut self, request: Value, initializing: bool) -> Result<Exchange<'_>, Error> {
        match self {
            Self::Lines(lines) => {
                lines.write(request).await?;
                Ok(Exchange::Lines(lines))
            }
            Self::Http(http) => {
                let reply = http.request(request, initializing).await?;
                Ok(Exchange::Http(http, reply))
            }
        }
    }

    pub(super) async fn send(&mut self, message: Value) -> Result<(), Error> {
        match self {
            Self::Lines(lines) => lines.write(message).await,
            Self::Http(http) => http.send(message).await,
        }
    }

    pub(super) async fn close(self) {
        match self {
            Self::Lines(lines) => lines.close().await,
            Self::Http(http) => http.close().await,
        }
    }
}

/// Replies to server requests use the same connection or a separate HTTP POST
/// while the original HTTP response stream is still open.
pub(super) enum Exchange<'a> {
    Lines(&'a mut Lines),
    Http(&'a Http, HttpReply),
}

impl Exchange<'_> {
    pub(super) async fn next(&mut self) -> Result<Value, Error> {
        match self {
            Self::Lines(lines) => lines.read().await,
            Self::Http(_, reply) => reply.next().await.ok_or(Error::EndOfStream)?,
        }
    }

    pub(super) async fn answer(&mut self, response: Value) -> Result<(), Error> {
        match self {
            Self::Lines(lines) => lines.write(response).await,
            Self::Http(http, _) => http.send(response).await,
        }
    }
}

pub(super) struct Lines {
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    child: Option<Child>,
}

impl Lines {
    fn new(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        child: Option<Child>,
    ) -> Self {
        Self { reader: BufReader::new(Box::new(reader)), writer: Box::new(writer), child }
    }

    async fn write(&mut self, message: Value) -> Result<(), Error> {
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<Value, Error> {
        let mut line = Vec::new();
        if self.reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(Error::EndOfStream);
        }
        Ok(serde_json::from_slice(&line)?)
    }

    async fn close(self) {
        let Self { writer, child, .. } = self;
        drop(writer);
        if let Some(mut child) = child
            && timeout(SHUTDOWN_GRACE, child.wait()).await.is_err()
        {
            let _ = timeout(SHUTDOWN_GRACE, child.kill()).await;
        }
    }
}

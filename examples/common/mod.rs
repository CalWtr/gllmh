//! Client helpers for the acceptance services (sections 6.3 and 7).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use gllmh::frame::{CallError, Incoming, Peer};
use gllmh::protocol::local::{SUBMIT, SubmitInput, SubmitOutput};
use gllmh::protocol::tool::{ENV_SERVICE_CONFIG, ENV_SERVICE_SOCKET};
use serde::{Serialize, de::DeserializeOwned};
use tokio::net::UnixStream;
use tokio::time::sleep;

pub fn load_config<T: DeserializeOwned>() -> Result<(PathBuf, T)> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("GLLMH_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(io::stderr)
        .init();
    let socket = env_path(ENV_SERVICE_SOCKET)?;
    let path = env_path(ENV_SERVICE_CONFIG)?;
    let data = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let config = serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))?;
    Ok((socket, config))
}

fn env_path(name: &str) -> Result<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .with_context(|| format!("{name} must name a path"))
}

pub struct Backoff(Duration);

impl Default for Backoff {
    fn default() -> Self {
        Self(Duration::from_millis(50))
    }
}

impl Backoff {
    pub async fn wait(&mut self) {
        sleep(self.0).await;
        self.0 = (self.0 * 2).min(Duration::from_secs(2));
    }
}

pub async fn connect(path: &Path, backoff: &mut Backoff) -> Result<(Peer, Incoming)> {
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => {
                let (read, write) = stream.into_split();
                return Ok(Peer::spawn(read, write));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::Interrupted
                ) =>
            {
                backoff.wait().await;
            }
            Err(error) => return Err(error).with_context(|| format!("connect to {}", path.display())),
        }
    }
}

/// Retry explicit retryable refusals. A lost reply leaves acceptance unknown.
pub async fn submit(path: &Path, connection: &mut (Peer, Incoming), input: &SubmitInput) -> Result<SubmitOutput> {
    let mut backoff = Backoff::default();
    loop {
        if connection.0.is_closed() {
            backoff.wait().await;
            *connection = connect(path, &mut backoff).await?;
        }
        match connection.0.request(SUBMIT, input).await {
            Ok(output) => return Ok(output),
            Err(CallError::Remote(error)) => {
                stdout_line(&line(&error)?)?;
                if !error.retry {
                    return Err(error.into());
                }
                backoff.wait().await;
            }
            Err(error) => return Err(error).context("submit acceptance unknown"),
        }
    }
}

pub fn line(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn stdout_line(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

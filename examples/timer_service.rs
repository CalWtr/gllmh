//! Send a delayed message on a service socket (sections 2 and 9).
mod common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use gllmh::frame::{CallError, ErrorBody, codes};
use gllmh::protocol::local::SubmitInput;
use gllmh::types::{InteractionId, InteractionKind};
use serde::Deserialize;
use tokio::time::{Instant, sleep_until};

use common::{Backoff, connect, line, load_config, stdout_line, submit};

#[derive(Deserialize)]
struct Config {
    to: String,
    body: serde_json::Value,
    delay_s: u64,
    repeat_s: Option<u64>,
    caused_by: Option<InteractionId>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let (socket, config) = load_config::<Config>()?;
    ensure!(config.repeat_s != Some(0), "repeat_s must be positive");
    let repeat = config.repeat_s.map(Duration::from_secs);
    let input = SubmitInput {
        kind: InteractionKind::Message,
        to: config.to,
        body: Some(config.body),
        context: None,
        caused_by: if repeat.is_none() { config.caused_by } else { None },
    };

    let mut due = Instant::now().checked_add(Duration::from_secs(config.delay_s)).context("delay_s is too large")?;
    let mut backoff = Backoff::default();
    let mut connection = connect(&socket, &mut backoff).await?;
    loop {
        tokio::select! {
            biased;
            request = connection.1.next() => {
                if let Some(request) = request {
                    request.responder.err(ErrorBody::new(codes::UNKNOWN_OP, request.op));
                } else {
                    backoff.wait().await;
                    connection = connect(&socket, &mut backoff).await?;
                }
            }
            _ = sleep_until(due) => {
                match submit(&socket, &mut connection, &input).await {
                    Ok(output) => {
                        stdout_line(&line(&output)?)?;
                        backoff = Backoff::default();
                    }
                    Err(error) if repeat.is_some() && matches!(error.downcast_ref(), Some(CallError::Closed)) => {
                        tracing::warn!("{error:#}");
                        backoff.wait().await;
                        connection = connect(&socket, &mut backoff).await?;
                    }
                    Err(error) => return Err(error),
                }
                let Some(interval) = repeat else { return Ok(()) };
                due = due.checked_add(interval).context("repeat_s is too large")?;
                if due <= Instant::now() {
                    due = Instant::now().checked_add(interval).context("repeat_s is too large")?;
                }
            }
        }
    }
}

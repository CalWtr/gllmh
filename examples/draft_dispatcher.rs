//! Submit a directive and record its result (sections 2 and 9).
mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use gllmh::frame::{ErrorBody, codes};
use gllmh::protocol::local::{NOTIFY, NotifyInput, SubmitInput};
use gllmh::types::{Block, InteractionKind};
use serde::Deserialize;

use common::{Backoff, connect, line, load_config, stdout_line, submit};

#[derive(Deserialize)]
struct Config {
    to: String,
    context: Vec<Block>,
    result_file: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let (socket, config) = load_config::<Config>()?;
    let mut backoff = Backoff::default();
    let mut connection = connect(&socket, &mut backoff).await?;
    let directive = submit(
        &socket,
        &mut connection,
        &SubmitInput {
            kind: InteractionKind::Directive,
            to: config.to,
            body: None,
            context: Some(config.context),
            caused_by: None,
        },
    )
    .await?
    .id;

    let mut recorded = false;
    backoff = Backoff::default();
    loop {
        while let Some(request) = connection.1.next().await {
            if request.op != NOTIFY {
                request.responder.err(ErrorBody::new(codes::UNKNOWN_OP, request.op));
                continue;
            }
            let Some((notice, responder)) = request.parse::<NotifyInput>() else {
                continue;
            };
            let result = notice.interaction;
            if !recorded && result.kind == InteractionKind::Result && result.caused_by == Some(directive) {
                let bytes = line(&result)?;
                if let Some(path) = &config.result_file {
                    tokio::fs::write(path, &bytes).await.with_context(|| format!("write {}", path.display()))?;
                }
                stdout_line(&bytes)?;
                recorded = true;
            }
            responder.ok(BTreeMap::<String, serde_json::Value>::new());
            backoff = Backoff::default();
        }
        backoff.wait().await;
        connection = connect(&socket, &mut backoff).await?;
    }
}

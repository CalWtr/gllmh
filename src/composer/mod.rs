//! The builtin `composer` (spec section 7, Builtin composer), run as the
//! hidden subcommand `gllmh composer`, speaking section 6.2 on stdin/stdout.
//!
//! Owner: M4.2.

mod context;

use std::sync::Arc;

use tokio::{io, task::JoinSet};

use crate::frame::{ErrorBody, Peer, Request, codes};
use crate::protocol::composer::{COMPOSE, ComposeInput, INIT, InitInput, InitOutput};

use self::context::Composer;

/// `gllmh composer` (hidden; started by the valet).
#[derive(Debug, Clone, clap::Args)]
pub struct ComposerArgs {}

pub async fn run(_args: ComposerArgs) -> anyhow::Result<()> {
    let (peer, mut incoming, writer) = Peer::spawn_with_writer(io::stdin(), io::stdout());
    let mut composer = None;
    let mut jobs = JoinSet::new();

    loop {
        tokio::select! {
            request = incoming.next() => match request {
                Some(request) => dispatch(request, &mut composer, &mut jobs),
                None => break,
            },
            Some(result) = jobs.join_next() => log_job_error(result),
        }
    }

    // Stdin can close after the last request and before its reply is ready.
    while let Some(result) = jobs.join_next().await {
        log_job_error(result);
    }
    drop(peer);
    writer.await?;
    Ok(())
}

fn dispatch(request: Request, composer: &mut Option<Arc<Composer>>, jobs: &mut JoinSet<()>) {
    match request.op.as_str() {
        INIT if composer.is_none() => {
            let Some((input, responder)) = request.parse::<InitInput>() else { return };
            match Composer::new(input) {
                Ok(config) => {
                    *composer = Some(Arc::new(config));
                    responder.ok(InitOutput {});
                }
                Err(error) => responder.err(ErrorBody::new(codes::BAD_INPUT, error.to_string())),
            }
        }
        INIT => request.responder.err(ErrorBody::new(codes::BAD_INPUT, "already initialized")),
        COMPOSE => {
            let Some(composer) = composer.clone() else {
                request.responder.err(ErrorBody::new(codes::BAD_INPUT, "init required before compose"));
                return;
            };
            let Some((input, responder)) = request.parse::<ComposeInput>() else { return };
            jobs.spawn_blocking(move || {
                let result = composer.compose(input.turn).map_err(|error| error.into_reply());
                responder.reply(result);
            });
        }
        op => request.responder.err(ErrorBody::new(codes::UNKNOWN_OP, format!("unknown composer operation {op}"))),
    }
}

fn log_job_error(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!(%error, "compose task failed");
    }
}

//! The agent's composer as a child process (section 6.2). Builtin composers
//! run as `gllmh composer`; custom ones as `custom_composer_bin`. Both speak
//! frames on stdin/stdout and get one `init`, then `compose` requests.

use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::process::{Child, Command};

use crate::frame::{CallError, ErrorBody, Peer};
use crate::profile::{ComposerEngine, ComposerProfile};
use crate::protocol::composer::{self as proto, ComposeInput, ComposeOutput, InitInput, InitOutput};
use crate::types::InteractionId;

use super::Valet;

pub struct Composer {
    peer: Peer,
    timeout: Duration,
}

impl Composer {
    /// Start the composer and send `init`. Returns the link and the child,
    /// whose exit ends the valet.
    pub async fn start(valet: &Valet, profile: &ComposerProfile) -> anyhow::Result<(Self, Child)> {
        let mut cmd = match profile.engine {
            ComposerEngine::Builtin => {
                let mut c = Command::new(std::env::current_exe().context("locate the gllmh binary")?);
                c.arg("composer");
                c
            }
            ComposerEngine::Custom => {
                let bin = profile.custom_composer_bin.as_ref().context("custom engine without custom_composer_bin")?;
                Command::new(bin)
            }
        };
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
        if let Some(cwd) = &valet.profile.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().context("start the composer")?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (peer, mut incoming) = Peer::spawn(stdout, stdin);
        // A composer only answers; refuse anything it asks.
        tokio::spawn(async move {
            while let Some(req) = incoming.next().await {
                req.responder
                    .err(ErrorBody::new(crate::frame::codes::UNKNOWN_OP, "the valet serves no ops to a composer"));
            }
        });

        let init = InitInput {
            user: valet.user.clone(),
            composer_profile: serde_json::to_value(profile)?,
            llm_profiles: valet
                .profile
                .llm_profiles
                .iter()
                .map(|p| {
                    let mut p = p.clone();
                    p.api_key_file = None;
                    serde_json::to_value(p)
                })
                .collect::<Result<_, _>>()?,
            tools: valet.tool_names(),
            store: crate::paths::store_db(&valet.data_dir),
            edges: valet.edges.clone(),
        };
        let timeout = Duration::from_secs(profile.compose_timeout_s);
        let _: InitOutput = tokio::time::timeout(timeout, peer.request(proto::INIT, &init))
            .await
            .context("composer init timed out")?
            .context("composer init")?;
        Ok((Self { peer, timeout }, child))
    }

    /// Ask for the next model input of a turn. Errors use the codes of 6.2.
    pub async fn compose(&self, turn: InteractionId) -> Result<ComposeOutput, ErrorBody> {
        match tokio::time::timeout(self.timeout, self.peer.request(proto::COMPOSE, &ComposeInput { turn })).await {
            Err(_) => Err(ErrorBody::new(proto::codes::TIMEOUT, format!("no reply within {:?}", self.timeout))),
            Ok(Ok(out)) => Ok(out),
            Ok(Err(CallError::Remote(e))) => Err(e),
            Ok(Err(CallError::BadReply(e))) => Err(ErrorBody::new(proto::codes::INVALID_REPLY, e.to_string())),
            Ok(Err(CallError::Closed)) => Err(ErrorBody::new(proto::codes::TIMEOUT, "composer connection closed")),
        }
    }
}

//! `local.sock` (section 6.3): UIs and controllers connect here, receive
//! `notify`, and may `submit` and ask `status`. M7.1 adds the service sockets,
//! which share [`serve_client`] with a service name.

use std::collections::BTreeMap;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use crate::frame::{CallError, ErrorBody, Peer, Request, codes};
use crate::protocol::broker::{self, StatusOutput};
use crate::protocol::local::{self, NotifyInput, StatusInput, SubmitInput};
use crate::types::{Interaction, InteractionKind};

use super::{SendRequest, Valet};

/// Who is on the other end of a client connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientKind {
    /// A UI or controller on local.sock.
    Local,
    /// One of the user's services, named by the socket it connected on.
    Service(String),
}

/// Connected clients, for `notify` fan-out.
#[derive(Default)]
pub struct Clients {
    next: AtomicU64,
    peers: Mutex<BTreeMap<u64, (ClientKind, Peer)>>,
}

impl Clients {
    fn add(&self, kind: ClientKind, peer: Peer) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.peers.lock().unwrap().insert(id, (kind, peer));
        id
    }

    fn remove(&self, id: u64) {
        self.peers.lock().unwrap().remove(&id);
    }

    /// Send `notify` to every UI and controller. Replies are ignored.
    pub fn notify(&self, interaction: &Interaction) {
        self.notify_where(interaction, |k| *k == ClientKind::Local);
    }

    /// Send `notify` to the clients `pick` selects. Returns how many got it.
    pub fn notify_where(&self, interaction: &Interaction, pick: impl Fn(&ClientKind) -> bool) -> usize {
        let peers: Vec<Peer> =
            self.peers.lock().unwrap().values().filter(|(k, _)| pick(k)).map(|(_, p)| p.clone()).collect();
        for peer in &peers {
            let peer = peer.clone();
            let input = NotifyInput { interaction: interaction.clone() };
            tokio::spawn(async move {
                let _ = peer.request_raw(local::NOTIFY, serde_json::to_value(input).expect("notify")).await;
            });
        }
        peers.len()
    }
}

/// Bind `local.sock` (0600) and serve it until the valet exits.
pub async fn listen(valet: Arc<Valet>, path: &Path) -> anyhow::Result<JoinHandle<()>> {
    let listener = bind(path)?;
    Ok(tokio::spawn(accept_loop(valet, listener, ClientKind::Local)))
}

/// Bind a unix socket with mode 0600, replacing a stale file.
pub fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove stale {}", path.display())),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", path.display()))?;
    Ok(listener)
}

pub async fn accept_loop(valet: Arc<Valet>, listener: UnixListener, kind: ClientKind) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let valet = valet.clone();
                let kind = kind.clone();
                tokio::spawn(async move {
                    let (peer, incoming) = Peer::spawn_unix(stream);
                    serve_client(valet, kind, peer, incoming).await;
                });
            }
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Serve one client connection until it closes.
pub async fn serve_client(valet: Arc<Valet>, kind: ClientKind, peer: Peer, mut incoming: crate::frame::Incoming) {
    let id = valet.clients.add(kind.clone(), peer);
    while let Some(req) = incoming.next().await {
        let valet = valet.clone();
        let kind = kind.clone();
        tokio::spawn(async move { handle(&valet, &kind, req).await });
    }
    valet.clients.remove(id);
}

async fn handle(valet: &Valet, kind: &ClientKind, req: Request) {
    match req.op.as_str() {
        local::SUBMIT => {
            if let Some((input, responder)) = req.parse::<SubmitInput>() {
                responder.reply(submit(valet, kind, input).await);
            }
        }
        local::STATUS if *kind == ClientKind::Local => {
            if let Some((input, responder)) = req.parse::<StatusInput>() {
                let result = valet.broker.request::<_, StatusOutput>(broker::STATUS, &input).await;
                responder.reply(result.map_err(broker_error));
            }
        }
        other => {
            let msg = format!("{other:?} is not served on this socket");
            req.responder.err(ErrorBody::new(codes::UNKNOWN_OP, msg));
        }
    }
}

async fn submit(valet: &Valet, kind: &ClientKind, input: SubmitInput) -> Result<broker::SendOutput, ErrorBody> {
    if input.kind == InteractionKind::Result {
        return Err(ErrorBody::new(local::codes::BAD_KIND, "only messages and directives can be submitted"));
    }
    let mut req = SendRequest::new(input.kind, input.to);
    req.body = input.body;
    req.context = input.context;
    req.caused_by = input.caused_by;
    match kind {
        ClientKind::Local => req.local = true,
        // A service's name comes from its socket, never from the request.
        ClientKind::Service(name) => req.service = Some(name.clone()),
    }
    valet.send(req).await.result
}

fn broker_error(e: CallError) -> ErrorBody {
    match e {
        CallError::Remote(err) => err,
        other => ErrorBody::retryable(local::codes::BROKER_UNAVAILABLE, other.to_string()),
    }
}

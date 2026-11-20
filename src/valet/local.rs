//! `local.sock` (section 6.3): UIs and controllers connect here, receive
//! `notify`, and may `submit` and ask `status`. Service sockets share
//! [`serve_client`] with a service name.

use std::collections::BTreeMap;
use std::fs::Permissions;
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use crate::frame::{CallError, ErrorBody, Peer, Request, codes};
use crate::protocol::broker::{self, StatusOutput};
use crate::protocol::local::{self, NotifyInput, NotifyOutput, StatusInput, SubmitInput};
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
    peers: Mutex<BTreeMap<u64, Client>>,
}

#[derive(Clone)]
struct Client {
    kind: ClientKind,
    peer: Peer,
    /// Lets a service notify deadline close the connection and clear pending requests.
    service_stream: Option<Arc<StdUnixStream>>,
}

impl Clients {
    fn add(&self, client: Client) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.peers.lock().unwrap().insert(id, client);
        id
    }

    fn remove(&self, id: u64) {
        self.peers.lock().unwrap().remove(&id);
    }

    /// Send `notify` to every UI and controller. Replies are ignored.
    pub fn notify(&self, interaction: &Interaction) {
        self.notify_where(interaction, |k| *k == ClientKind::Local);
    }

    fn clients_where(&self, pick: impl Fn(&ClientKind) -> bool) -> Vec<Client> {
        self.peers.lock().unwrap().values().filter(|c| pick(&c.kind) && !c.peer.is_closed()).cloned().collect()
    }

    pub(super) fn service_connected(&self, service: &str) -> bool {
        self.peers.lock().unwrap().values().any(|client| {
            matches!(&client.kind, ClientKind::Service(name) if name == service) && !client.peer.is_closed()
        })
    }

    /// Start notifications to the selected clients and return the fan-out count.
    pub fn notify_where(&self, interaction: &Interaction, pick: impl Fn(&ClientKind) -> bool) -> usize {
        let clients = self.clients_where(pick);
        let count = clients.len();
        for client in clients {
            let input = NotifyInput { interaction: interaction.clone() };
            tokio::spawn(async move {
                let _ = client.peer.request::<_, NotifyOutput>(local::NOTIFY, &input).await;
            });
        }
        count
    }

    /// EXTENSION: await service acknowledgments, disconnecting timed-out clients.
    /// None means no client was connected. Otherwise report whether any
    /// client acknowledged the notify.
    pub async fn notify_service(&self, interaction: Interaction, service: &str) -> Option<bool> {
        let kind = ClientKind::Service(service.to_owned());
        let clients = self.clients_where(|k| *k == kind);
        if clients.is_empty() {
            return None;
        }
        let input = &NotifyInput { interaction };
        let replies = futures_util::future::join_all(clients.into_iter().map(|client| async move {
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                client.peer.request::<_, NotifyOutput>(local::NOTIFY, input),
            )
            .await
            {
                Ok(Ok(_)) => true,
                Ok(Err(e)) => {
                    tracing::debug!(service, id = %input.interaction.id, "service notify failed: {e}");
                    false
                }
                Err(_) => {
                    tracing::warn!(service, id = %input.interaction.id, "service notify timed out");
                    if let Some(stream) = client.service_stream {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                    false
                }
            }
        }))
        .await;
        Some(replies.into_iter().any(|acked| acked))
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
                    if let Err(e) = serve_client(valet, kind.clone(), stream).await {
                        tracing::warn!(?kind, "client connection rejected: {e:#}");
                    }
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
async fn serve_client(valet: Arc<Valet>, kind: ClientKind, stream: UnixStream) -> anyhow::Result<()> {
    let (stream, service_stream) = if let ClientKind::Service(name) = &kind {
        if !valet.dev {
            super::services::check_peer(&stream, &valet.user, name).await?;
        }
        let stream = stream.into_std()?;
        let service_stream = Arc::new(stream.try_clone()?);
        (UnixStream::from_std(stream)?, Some(service_stream))
    } else {
        (stream, None)
    };
    let (peer, mut incoming) = Peer::spawn_unix(stream);
    let id = valet.clients.add(Client { kind: kind.clone(), peer, service_stream });
    if let ClientKind::Service(name) = &kind {
        valet.services.wake(name);
    }
    while let Some(req) = incoming.next().await {
        let valet = valet.clone();
        let kind = kind.clone();
        tokio::spawn(async move { handle(&valet, &kind, req).await });
    }
    valet.clients.remove(id);
    Ok(())
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
    // A fast worker can return a result before the service submit records
    // the directive's broker ID. Result routing waits for that write.
    let _directive = if req.service.is_some() && req.kind == InteractionKind::Directive {
        Some(valet.services.directive_sends.read().await)
    } else {
        None
    };
    valet.send(req).await.result
}

fn broker_error(e: CallError) -> ErrorBody {
    match e {
        CallError::Remote(err) => err,
        other => ErrorBody::retryable(local::codes::BROKER_UNAVAILABLE, other.to_string()),
    }
}

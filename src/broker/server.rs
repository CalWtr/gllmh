//! `gllmh serve`: sockets and the operations of spec section 6.1.
//!
//! Owner: M2.2.

use std::collections::BTreeMap;
use std::fs;
use std::future::{Future, poll_fn};
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use anyhow::{Context, ensure};
use serde::{Serialize, de::DeserializeOwned};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify};
use tokio::task::{JoinSet, spawn_blocking};

use super::identity;
use super::log::{InteractionLog, LogError};
use crate::frame::{self, CallError, ErrorBody, Incoming, Peer, Request};
use crate::paths::{self, ConfigDirArg};
use crate::profile::{self, LoadedRuntime};
use crate::protocol::{PROTOCOL_VERSION, broker::*};
use crate::types::{self, Interaction, InteractionKind, InteractionState, Sender};

const DELIVERY_BACKOFF: Duration = Duration::from_millis(100);

/// `gllmh serve --profile <name> [--dev]`.
#[derive(Debug, Clone, clap::Args)]
pub struct ServeArgs {
    /// The runtime_profile to serve.
    #[arg(long)]
    pub profile: String,
    /// Create the user sockets itself, run everyone as the caller, skip identity checks.
    #[arg(long)]
    pub dev: bool,
    #[command(flatten)]
    pub config: ConfigDirArg,
    /// Directory of the per-user broker sockets in dev mode.
    #[arg(long, env = paths::ENV_RUN_DIR)]
    pub run_dir: Option<PathBuf>,
    /// EXTENSION: override the runtime profile's broker database path.
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// EXTENSION: seconds between interaction expiry scans.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub expire_interval_s: u64,
}

pub async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let interval = Duration::from_secs(args.expire_interval_s);
    let (broker, listeners) = spawn_blocking(move || initialize(args)).await??;
    let broker = Arc::new(broker);
    let mut tasks = JoinSet::new();
    for (user, listener) in listeners {
        tasks.spawn(broker.clone().listen(user, UnixListener::from_std(listener)?));
    }
    tasks.spawn(broker.expire(interval));
    crate::sdnotify::ready().context("notify systemd that the broker is ready")?;
    tasks.join_next().await.context("broker has no running tasks")??
}

type Listeners = Vec<(String, std::os::unix::net::UnixListener)>;

fn initialize(args: ServeArgs) -> anyhow::Result<(Broker, Listeners)> {
    let profile = profile::validate(&args.config.config_dir, &args.profile, args.dev).map_err(|violations| {
        for violation in &violations {
            eprintln!("{violation}");
        }
        anyhow::anyhow!("profile validation failed ({} violation(s))", violations.len())
    })?;
    let listeners = if args.dev {
        let run_dir = args
            .run_dir
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("gllmh/users")))
            .unwrap_or_else(|| PathBuf::from(paths::DEFAULT_RUN_DIR));
        dev_listeners(&run_dir, &profile)?
    } else {
        identity::inherited_listeners(&profile)?
    };
    let db = args.db.as_ref().unwrap_or(&profile.runtime.broker_db_path);
    let log = InteractionLog::open(db)?;
    log.requeue_all_delivered()?;
    log.expire(profile.runtime.interaction_ttl_s, types::now())?;
    let users = profile.users.keys().map(|name| (name.clone(), UserConnection::default())).collect();
    Ok((Broker { profile, users, log: Arc::new(Mutex::new(log)) }, listeners))
}

fn dev_listeners(run_dir: &Path, profile: &LoadedRuntime) -> anyhow::Result<Listeners> {
    fs::create_dir_all(run_dir).with_context(|| format!("create socket directory {run_dir:?}"))?;
    profile
        .users
        .keys()
        .map(|user| {
            let path = paths::user_socket(run_dir, user);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    ensure!(metadata.file_type().is_socket(), "refusing to replace {path:?}: expected a socket");
                    match std::os::unix::net::UnixStream::connect(&path) {
                        Ok(_) => anyhow::bail!("refusing to replace a listening socket at {path:?}"),
                        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
                        Err(error) => return Err(error).with_context(|| format!("check stale socket {path:?}")),
                    }
                    fs::remove_file(&path).with_context(|| format!("remove stale socket {path:?}"))?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("stat {path:?}")),
            }
            let listener = std::os::unix::net::UnixListener::bind(&path).with_context(|| format!("bind {path:?}"))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            Ok((user.clone(), listener))
        })
        .collect()
}

#[derive(Default)]
struct UserConnection {
    online: Mutex<()>,
    wake: Notify,
}

struct SessionRequests {
    incoming: Incoming,
    deferred: Option<Request>,
}

impl SessionRequests {
    async fn next(&mut self) -> Option<Request> {
        match self.deferred.take() {
            Some(request) => Some(request),
            None => self.incoming.next().await,
        }
    }
}

struct Broker {
    profile: LoadedRuntime,
    users: BTreeMap<String, UserConnection>,
    log: Arc<Mutex<InteractionLog>>,
}

impl Broker {
    async fn with_log<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut InteractionLog) -> T + Send + 'static,
    ) -> anyhow::Result<T> {
        let mut log = self.log.clone().lock_owned().await;
        Ok(spawn_blocking(move || f(&mut log)).await?)
    }

    async fn expire(self: Arc<Self>, interval: Duration) -> anyhow::Result<()> {
        let ttl = self.profile.runtime.interaction_ttl_s;
        loop {
            tokio::time::sleep(interval).await;
            self.with_log(move |log| log.expire(ttl, types::now())).await??;
        }
    }

    async fn listen(self: Arc<Self>, listener_user: String, listener: UnixListener) -> anyhow::Result<()> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                result = connections.join_next(), if !connections.is_empty() => {
                    if let Some(result) = result {
                        result??;
                    }
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted.with_context(|| format!("accept on {listener_user:?} socket"))?;
                    let user = identity::connection_user(&listener_user).to_owned();
                    let (peer, incoming) = Peer::spawn_unix(stream);
                    connections.spawn(self.clone().connection(user, peer, incoming));
                }
            }
        }
    }

    async fn connection(self: Arc<Self>, user: String, peer: Peer, mut incoming: Incoming) -> anyhow::Result<()> {
        let online = &self.users[&user].online;
        let _session = loop {
            let Some(request) = incoming.next().await else {
                return Ok(());
            };
            if request.op != HELLO {
                request.responder.err(ErrorBody::new(codes::NO_HELLO, "hello required"));
                continue;
            }
            let Some((input, responder)) = request.parse::<HelloInput>() else {
                continue;
            };
            if input.protocol != PROTOCOL_VERSION {
                responder.err(ErrorBody::new(codes::PROTOCOL_MISMATCH, "unsupported protocol version"));
                continue;
            }
            let Ok(session) = online.try_lock() else {
                responder.err(ErrorBody::new(codes::ALREADY_CONNECTED, "user is already connected"));
                continue;
            };
            let hello = self.profile.hello(&user)?.context("missing hello profile for listener")?;
            responder.ok(hello);
            break session;
        };
        let result = self.deliver(&user, &peer, incoming).await;
        // Release the online slot only after cleanup, so the next holder can deliver immediately.
        self.with_log(move |log| log.requeue_delivered(&user)).await??;
        result
    }

    /// Run requests while a delivery is idle, in flight or backing off. The reply
    /// takes priority over the next request so a pipelined ack then close works.
    async fn serve_until<F: Future>(
        self: &Arc<Self>,
        user: &str,
        requests: &mut SessionRequests,
        ready: F,
    ) -> Option<F::Output> {
        let ready = tokio::task::unconstrained(ready);
        tokio::pin!(ready);
        loop {
            let request = tokio::select! {
                biased;
                result = &mut ready => return Some(result),
                request = requests.next() => request?,
            };
            // The reader may have received the reply between polling the two
            // branches above. Poll again before acting on the later request.
            if let Poll::Ready(result) = poll_fn(|cx| Poll::Ready(ready.as_mut().poll(cx))).await {
                requests.deferred = Some(request);
                return Some(result);
            }
            self.request(user, request).await;
        }
    }

    async fn deliver(self: &Arc<Self>, user: &str, peer: &Peer, incoming: Incoming) -> anyhow::Result<()> {
        let mut requests = SessionRequests { incoming, deferred: None };
        let wake = &self.users[user].wake;
        while !peer.is_closed() {
            let to = user.to_owned();
            let next = self
                .with_log(move |log| {
                    let next = log.next_deliverable(&to)?;
                    if let Some(interaction) = &next {
                        log.mark_delivered(interaction.id)?;
                    }
                    Ok::<_, LogError>(next)
                })
                .await??;
            let Some(mut interaction) = next else {
                if self.serve_until(user, &mut requests, wake.notified()).await.is_none() {
                    break;
                }
                continue;
            };
            let id = interaction.id;
            interaction.state = InteractionState::Delivered;
            let input = DeliverInput { interaction };
            let reply = peer.request::<_, DeliverOutput>(DELIVER, &input);
            match self.serve_until(user, &mut requests, reply).await {
                Some(Ok(_)) => self.with_log(move |log| log.ack(id)).await??,
                None | Some(Err(CallError::Closed)) => break,
                Some(Err(error)) => {
                    tracing::warn!(%user, %id, %error, "delivery failed, requeuing");
                    let to = user.to_owned();
                    self.with_log(move |log| log.requeue_delivered(&to)).await??;
                    if self.serve_until(user, &mut requests, tokio::time::sleep(DELIVERY_BACKOFF)).await.is_none() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn request(self: &Arc<Self>, user: &str, request: Request) {
        match request.op.as_str() {
            SEND => self.handle(user, request, Self::send).await,
            CLOSE => self.handle(user, request, Self::close).await,
            LOOKUP => self.handle(user, request, Self::lookup).await,
            STATUS => self.handle(user, request, Self::status).await,
            HELLO => request.responder.err(ErrorBody::new(codes::ALREADY_CONNECTED, "hello already completed")),
            _ => request.responder.err(ErrorBody::new(frame::codes::UNKNOWN_OP, "unknown broker operation")),
        }
    }

    async fn handle<I, O>(
        self: &Arc<Self>,
        user: &str,
        request: Request,
        op: fn(&Self, &mut InteractionLog, &str, I) -> OpResult<O>,
    ) where
        I: DeserializeOwned + Send + 'static,
        O: Serialize + Send + 'static,
    {
        let Some((input, responder)) = request.parse::<I>() else {
            return;
        };
        let user = user.to_owned();
        let broker = self.clone();
        let result = self.with_log(move |log| op(&broker, log, &user, input)).await;
        responder.reply(match result {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(OpError::Rejected(error))) => Err(error),
            Ok(Err(OpError::Log(error))) => Err(internal(&error)),
            Err(error) => Err(internal(&error)),
        });
    }

    fn send(&self, log: &mut InteractionLog, user: &str, input: SendInput) -> OpResult<SendOutput> {
        let from = match input.service.as_deref() {
            Some(service) if self.profile.users[user].services.contains_key(service) => Sender::service(user, service),
            Some(_) => return Err(reject(codes::NOT_ALLOWED, "service does not belong to this user")),
            None => Sender::user(user),
        };
        if let Some(original) = log.lookup(&from, input.client_ref)? {
            return Ok(send_output(original));
        }
        let Some(recipient) = self.profile.users.get(&input.to) else {
            return Err(reject(codes::UNKNOWN_USER, "recipient is not a user in this runtime"));
        };
        let edges = &self.profile.edges[&from];
        let allowed = match input.kind {
            InteractionKind::Message => edges.messages.contains(&input.to),
            InteractionKind::Directive => {
                if recipient.composer_profile.is_none() {
                    return Err(reject(codes::NOT_AN_AGENT, "directive recipient has no composer"));
                }
                edges.directives.contains(&input.to)
            }
            InteractionKind::Result => true,
        };
        if !allowed {
            return Err(reject(codes::NOT_ALLOWED, "no edge for this kind of interaction"));
        }
        if input.kind != InteractionKind::Result
            && !input.local
            && let Some(id) = input.caused_by
        {
            let parent = log.get(id)?.ok_or(LogError::UnknownParent(id)).map_err(send_error)?;
            if parent.hops >= self.profile.runtime.max_hops {
                return Err(reject(codes::HOP_LIMIT, "interaction exceeds max_hops"));
            }
        }
        let size = json_size(input.body.as_ref())?.saturating_add(json_size(input.context.as_ref())?);
        if size > self.profile.runtime.max_body_bytes {
            return Err(reject(codes::TOO_LARGE, "interaction exceeds max_body_bytes"));
        }
        if (input.kind != InteractionKind::Directive && input.context.is_some())
            || (input.kind == InteractionKind::Directive && input.body.is_some())
            || (input.kind != InteractionKind::Result && input.turn_outcome.is_some())
        {
            return Err(reject(frame::codes::BAD_INPUT, "payload fields do not match the interaction kind"));
        }
        let accepted = log.accept(&from, &input).map_err(send_error)?;
        self.users[&input.to].wake.notify_one();
        Ok(send_output(accepted.interaction))
    }

    fn close(&self, log: &mut InteractionLog, user: &str, input: CloseInput) -> OpResult<CloseOutput> {
        let Some(row) = log.get(input.id)?.filter(|row| row.to == user) else {
            return Err(reject(codes::NOT_YOURS, "interaction does not belong to this recipient"));
        };
        match row.state {
            InteractionState::Acked => log.close(input.id, input.outcome)?,
            InteractionState::Closed if row.outcome == Some(input.outcome) => {}
            InteractionState::Closed => {
                return Err(reject(codes::NOT_ACKED, "interaction already closed with a different outcome"));
            }
            _ => return Err(reject(codes::NOT_ACKED, "interaction has not been acknowledged")),
        }
        Ok(CloseOutput {})
    }

    fn lookup(&self, log: &mut InteractionLog, user: &str, input: LookupInput) -> OpResult<LookupOutput> {
        let senders = std::iter::once(Sender::user(user))
            .chain(self.profile.users[user].services.keys().map(|service| Sender::service(user, service)));
        for sender in senders {
            if let Some(row) = log.lookup(&sender, input.client_ref)? {
                return Ok(LookupOutput { found: true, interaction: Some(send_output(row)) });
            }
        }
        Ok(LookupOutput { found: false, interaction: None })
    }

    fn status(&self, log: &mut InteractionLog, user: &str, input: StatusInput) -> OpResult<StatusOutput> {
        let participants = log.participants(input.root)?;
        if participants.is_empty() {
            return Err(reject(codes::UNKNOWN_ROOT, "no run with that root"));
        }
        if !participants.contains(user) {
            return Err(reject(codes::NOT_PARTICIPANT, "user has no interactions in this run"));
        }
        Ok(StatusOutput { interactions: log.by_root(input.root)? })
    }
}

#[derive(Debug, thiserror::Error)]
enum OpError {
    #[error(transparent)]
    Rejected(ErrorBody),
    #[error(transparent)]
    Log(#[from] LogError),
}

type OpResult<T> = Result<T, OpError>;

fn reject(code: &str, message: impl Into<String>) -> OpError {
    OpError::Rejected(ErrorBody::new(code, message))
}

fn internal(error: &dyn std::fmt::Display) -> ErrorBody {
    tracing::error!(%error, "broker operation failed");
    ErrorBody::new(frame::codes::INTERNAL, "broker operation failed")
}

fn send_error(error: LogError) -> OpError {
    match error {
        LogError::NotOwed => reject(codes::NOT_OWED, error.to_string()),
        LogError::UnknownParent(_) => reject(frame::codes::BAD_INPUT, error.to_string()),
        error => error.into(),
    }
}

fn send_output(row: Interaction) -> SendOutput {
    SendOutput { id: row.id, root: row.root, hops: row.hops }
}

fn json_size<T: Serialize>(value: Option<&T>) -> Result<usize, LogError> {
    Ok(match value {
        Some(value) => serde_json::to_vec(value)?.len(),
        None => 0,
    })
}

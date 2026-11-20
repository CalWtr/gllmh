//! Framing of spec section 6: one JSON object per text line, on every line
//! (broker ⇔ valet, valet ⇔ composer, valet ⇔ local clients and services).
//!
//! Either side may send a request; replies come back in any order, matched
//! by `re`. [`Peer::spawn`] runs one connection: [`Peer`] sends requests and
//! awaits their replies, and the returned [`Incoming`] yields the other side's
//! requests, each with a [`Responder`] to answer it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

/// One line on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// The `error` of a failed reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ErrorBody {
    /// One of the codes listed under the operation (see `protocol`).
    pub code: String,
    pub message: String,
    /// True when the same request may succeed later.
    pub retry: bool,
}

impl ErrorBody {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), retry: false }
    }

    pub fn retryable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), retry: true }
    }
}

/// Codes any operation may return, besides its own.
pub mod codes {
    /// The frame was not valid JSON or not a valid frame.
    pub const BAD_FRAME: &str = "bad_frame";
    /// The op is unknown on this line.
    pub const UNKNOWN_OP: &str = "unknown_op";
    /// The input did not match the op's schema.
    pub const BAD_INPUT: &str = "bad_input";
    /// Something failed on the answering side that is not the caller's fault.
    pub const INTERNAL: &str = "internal";
}

/// Why a request got no successful reply.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The other side replied with an error.
    #[error("remote error {0}")]
    Remote(ErrorBody),
    /// The connection closed before the reply came.
    #[error("connection closed")]
    Closed,
    /// The reply's output did not deserialize into the expected type.
    #[error("bad reply: {0}")]
    BadReply(serde_json::Error),
}

impl CallError {
    /// The remote error code, if the other side answered with one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Remote(e) => Some(&e.code),
            _ => None,
        }
    }
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, ErrorBody>>>>>;

/// The sending side of one connection. Cheap to clone.
#[derive(Clone)]
pub struct Peer {
    out: mpsc::UnboundedSender<Frame>,
    pending: Pending,
    closed: Arc<AtomicBool>,
    next_id: Arc<AtomicU64>,
}

/// A request from the other side.
#[derive(Debug)]
pub struct Request {
    pub op: String,
    pub input: Value,
    pub responder: Responder,
}

impl Request {
    /// Deserialize the input, answering `bad_input` and returning `None` if it does not fit.
    pub fn parse<T: DeserializeOwned>(self) -> Option<(T, Responder)> {
        match serde_json::from_value(self.input) {
            Ok(v) => Some((v, self.responder)),
            Err(e) => {
                self.responder.err(ErrorBody::new(codes::BAD_INPUT, e.to_string()));
                None
            }
        }
    }
}

/// Answers one incoming request. Dropping it unanswered sends `internal`.
#[derive(Debug)]
pub struct Responder {
    id: String,
    out: Option<mpsc::UnboundedSender<Frame>>,
}

impl Responder {
    /// Reply with a serializable output.
    pub fn ok<T: Serialize>(mut self, output: T) {
        let output = serde_json::to_value(output).expect("reply output serializes");
        self.send(Ok(output));
    }

    /// Reply with an error.
    pub fn err(mut self, error: ErrorBody) {
        self.send(Err(error));
    }

    /// Reply with either.
    pub fn reply<T: Serialize>(self, result: Result<T, ErrorBody>) {
        match result {
            Ok(v) => self.ok(v),
            Err(e) => self.err(e),
        }
    }

    fn send(&mut self, result: Result<Value, ErrorBody>) {
        if let Some(out) = self.out.take() {
            let (output, error) = match result {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e)),
            };
            let _ = out.send(Frame {
                id: format!("r{}", self.id),
                op: None,
                input: None,
                re: Some(self.id.clone()),
                output,
                error,
            });
        }
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if self.out.is_some() {
            self.send(Err(ErrorBody::new(codes::INTERNAL, "request dropped without a reply")));
        }
    }
}

/// The other side's requests. Ends when the connection closes.
pub struct Incoming {
    rx: mpsc::UnboundedReceiver<Request>,
    /// Keeps the connection open while the other side's requests may still be served.
    _keep: mpsc::UnboundedSender<Frame>,
}

impl Incoming {
    pub async fn next(&mut self) -> Option<Request> {
        self.rx.recv().await
    }
}

impl Peer {
    /// Run one connection: a reader task and a writer task on the tokio runtime.
    ///
    /// When the reader hits EOF or an I/O error, every pending request fails
    /// with [`CallError::Closed`] and `Incoming` ends.
    pub fn spawn<R, W>(reader: R, writer: W) -> (Peer, Incoming)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (peer, incoming, _) = Self::spawn_with_writer(reader, writer);
        (peer, incoming)
    }

    /// Like `spawn`, with a handle to wait for queued replies to flush after
    /// input EOF. Drop the peer, the `Incoming` and every responder before awaiting it.
    pub fn spawn_with_writer<R, W>(reader: R, writer: W) -> (Peer, Incoming, tokio::task::JoinHandle<()>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Frame>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Request>();
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));

        let writer = tokio::spawn(write_loop(writer, out_rx));
        tokio::spawn(read_loop(reader, pending.clone(), closed.clone(), out_tx.downgrade(), in_tx));

        let incoming = Incoming { rx: in_rx, _keep: out_tx.clone() };
        let peer = Peer { out: out_tx, pending, closed, next_id: Arc::new(AtomicU64::new(1)) };
        (peer, incoming, writer)
    }

    /// Split a unix stream and run it.
    pub fn spawn_unix(stream: tokio::net::UnixStream) -> (Peer, Incoming) {
        let (r, w) = stream.into_split();
        Self::spawn(r, w)
    }

    /// Send a request and await its raw reply.
    pub async fn request_raw(&self, op: &str, input: Value) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        // The reader sets `closed` before it clears `pending`, so a request
        // inserted after the clear still sees the flag here.
        if self.closed.load(Ordering::SeqCst) {
            self.pending.lock().await.remove(&id);
            return Err(CallError::Closed);
        }
        let frame =
            Frame { id: id.clone(), op: Some(op.to_string()), input: Some(input), re: None, output: None, error: None };
        if self.out.send(frame).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(CallError::Closed);
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(CallError::Remote(e)),
            Err(_) => Err(CallError::Closed),
        }
    }

    /// Send a typed request and deserialize its reply.
    pub async fn request<I: Serialize, O: DeserializeOwned>(&self, op: &str, input: &I) -> Result<O, CallError> {
        let input = serde_json::to_value(input).expect("request input serializes");
        let output = self.request_raw(op, input).await?;
        serde_json::from_value(output).map_err(CallError::BadReply)
    }

    /// True once the other side has closed the connection.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.out.is_closed()
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Frame>) {
    while let Some(frame) = rx.recv().await {
        let mut line = serde_json::to_vec(&frame).expect("frame serializes");
        line.push(b'\n');
        if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

async fn read_loop<R: AsyncRead + Unpin>(
    reader: R,
    pending: Pending,
    closed: Arc<AtomicBool>,
    // Weak: the connection closes once every Peer, Incoming and Responder is gone.
    out: mpsc::WeakUnboundedSender<Frame>,
    incoming: mpsc::UnboundedSender<Request>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let frame: Frame = match serde_json::from_str(&line) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("bad frame: {e}");
                // No id to answer to when the frame did not parse; try to recover one.
                if let Some(id) = serde_json::from_str::<Value>(&line)
                    .ok()
                    .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
                {
                    let mut r = Responder { id, out: out.upgrade() };
                    r.send(Err(ErrorBody::new(codes::BAD_FRAME, e.to_string())));
                }
                continue;
            }
        };
        if let Some(re) = frame.re {
            let result = match frame.error {
                Some(e) => Err(e),
                None => Ok(frame.output.unwrap_or(Value::Null)),
            };
            if let Some(tx) = pending.lock().await.remove(&re) {
                let _ = tx.send(result);
            } else {
                tracing::warn!("reply to unknown request {re}");
            }
        } else if let Some(op) = frame.op {
            let request = Request {
                op,
                input: frame.input.unwrap_or(Value::Null),
                responder: Responder { id: frame.id, out: out.upgrade() },
            };
            if incoming.send(request).is_err() {
                // Nobody listens for requests on this side; the Request's
                // responder answers `internal` as it drops.
            }
        } else {
            let mut r = Responder { id: frame.id, out: out.upgrade() };
            r.send(Err(ErrorBody::new(codes::BAD_FRAME, "frame has neither op nor re")));
        }
    }
    // Connection gone: fail everything still waiting, and anything sent later.
    closed.store(true, Ordering::SeqCst);
    pending.lock().await.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn pair() -> ((Peer, Incoming), (Peer, Incoming)) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        (Peer::spawn(ar, aw), Peer::spawn(br, bw))
    }

    #[tokio::test]
    async fn replies_out_of_order_are_matched() {
        let ((a, _a_in), (_b, mut b_in)) = pair();
        let first = tokio::spawn({
            let a = a.clone();
            async move { a.request_raw("slow", json!(1)).await }
        });
        let second = tokio::spawn({
            let a = a.clone();
            async move { a.request_raw("fast", json!(2)).await }
        });
        let r1 = b_in.next().await.unwrap();
        let r2 = b_in.next().await.unwrap();
        // Answer whichever arrived second first.
        let (slow, fast) = if r1.op == "slow" { (r1, r2) } else { (r2, r1) };
        fast.responder.ok(json!("fast-done"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        slow.responder.ok(json!("slow-done"));
        assert_eq!(first.await.unwrap().unwrap(), json!("slow-done"));
        assert_eq!(second.await.unwrap().unwrap(), json!("fast-done"));
    }

    #[tokio::test]
    async fn both_sides_send_requests_on_one_connection() {
        let ((a, mut a_in), (b, mut b_in)) = pair();
        let a_serves = tokio::spawn(async move {
            let r = a_in.next().await.unwrap();
            assert_eq!(r.op, "deliver");
            r.responder.ok(json!({}));
        });
        let b_serves = tokio::spawn(async move {
            let r = b_in.next().await.unwrap();
            assert_eq!(r.op, "send");
            let n = r.input["n"].as_i64().unwrap();
            r.responder.ok(json!({ "n": n + 1 }));
        });
        let from_a: Value = a.request("send", &json!({ "n": 41 })).await.unwrap();
        let from_b: Value = b.request("deliver", &json!({})).await.unwrap();
        assert_eq!(from_a, json!({ "n": 42 }));
        assert_eq!(from_b, json!({}));
        a_serves.await.unwrap();
        b_serves.await.unwrap();
    }

    #[tokio::test]
    async fn error_replies_and_dropped_responders() {
        let ((a, _a_in), (_b, mut b_in)) = pair();
        let call = tokio::spawn({
            let a = a.clone();
            async move { a.request_raw("send", json!({})).await }
        });
        let r = b_in.next().await.unwrap();
        r.responder.err(ErrorBody::new("not_allowed", "no edge"));
        let err = call.await.unwrap().unwrap_err();
        assert_eq!(err.code(), Some("not_allowed"));

        let call = tokio::spawn({
            let a = a.clone();
            async move { a.request_raw("send", json!({})).await }
        });
        drop(b_in.next().await.unwrap());
        assert_eq!(call.await.unwrap().unwrap_err().code(), Some(codes::INTERNAL));
    }

    #[tokio::test]
    async fn pending_and_later_requests_fail_when_connection_closes() {
        let (a, b) = tokio::io::duplex(1024);
        let (ar, aw) = tokio::io::split(a);
        let (a, _a_in) = Peer::spawn(ar, aw);
        let call = tokio::spawn({
            let a = a.clone();
            async move { a.request_raw("send", json!({})).await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(b);
        assert!(matches!(call.await.unwrap(), Err(CallError::Closed)));
        assert!(a.is_closed());
        assert!(matches!(a.request_raw("send", json!({})).await, Err(CallError::Closed)));
    }

    #[tokio::test]
    async fn dropping_every_handle_closes_the_connection() {
        let ((a, a_in), (b, _b_in)) = pair();
        drop((a, a_in));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !b.is_closed() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the other side sees the close");
    }

    #[tokio::test]
    async fn bad_input_is_answered() {
        #[derive(Deserialize)]
        struct Needs {
            #[allow(dead_code)]
            n: i64,
        }
        let ((a, _a_in), (_b, mut b_in)) = pair();
        let call = tokio::spawn({
            let a = a.clone();
            async move { a.request_raw("x", json!({ "m": 1 })).await }
        });
        let r = b_in.next().await.unwrap();
        assert!(r.parse::<Needs>().is_none());
        assert_eq!(call.await.unwrap().unwrap_err().code(), Some(codes::BAD_INPUT));
    }
}

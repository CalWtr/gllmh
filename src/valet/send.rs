//! The one path every send takes: local submits, service submits, send
//! tools and results. Write the action first, pre-check the edges from hello,
//! then hand it to the broker (sections 2, 5, 6.1).

use crate::frame::{CallError, ErrorBody, codes};
use crate::protocol::broker::{self, SendInput, SendOutput};
use crate::protocol::local;
use crate::protocol::records::SendFailure;
use crate::store::ActionRequest;
use crate::types::{ActionId, ActionKind, Block, InteractionId, InteractionKind, Sender, TurnOutcome};

use super::Valet;

/// Everything about a send except its client_ref, which is the action's id.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub kind: InteractionKind,
    pub to: String,
    pub service: Option<String>,
    pub caused_by: Option<InteractionId>,
    pub body: Option<serde_json::Value>,
    pub context: Option<Vec<Block>>,
    pub turn_outcome: Option<TurnOutcome>,
    /// Submitted by a UI or controller on local.sock.
    pub local: bool,
    /// Recording context, for sends made inside a turn.
    pub turn: Option<InteractionId>,
    pub step: Option<u32>,
    pub requested_by: Option<ActionId>,
    pub call_id: Option<String>,
    pub root: Option<InteractionId>,
}

impl SendRequest {
    pub fn new(kind: InteractionKind, to: impl Into<String>) -> Self {
        Self {
            kind,
            to: to.into(),
            service: None,
            caused_by: None,
            body: None,
            context: None,
            turn_outcome: None,
            local: false,
            turn: None,
            step: None,
            requested_by: None,
            call_id: None,
            root: None,
        }
    }
}

/// How a send ended, with the id of its action row.
pub struct SendDone {
    pub action: ActionId,
    pub result: Result<SendOutput, ErrorBody>,
}

impl Valet {
    /// The sender name this send goes out under.
    fn sender_of(&self, service: Option<&str>) -> Sender {
        match service {
            Some(s) => Sender::service(&self.user, s),
            None => Sender::user(&self.user),
        }
    }

    /// Refuse a send the edges from hello do not allow, before the broker sees it.
    /// Results need no edge (section 2, Directives and results, step 6).
    fn check_edges(&self, req: &SendRequest) -> Result<(), ErrorBody> {
        let edges = self.edges.get(&self.sender_of(req.service.as_deref()));
        let allowed = match req.kind {
            InteractionKind::Result => return Ok(()),
            InteractionKind::Message => edges.is_some_and(|e| e.messages.contains(&req.to)),
            InteractionKind::Directive => edges.is_some_and(|e| e.directives.contains(&req.to)),
        };
        if allowed {
            Ok(())
        } else {
            let sender = self.sender_of(req.service.as_deref());
            Err(ErrorBody::new(
                broker::codes::NOT_ALLOWED,
                format!("{sender} may not send a {} to {}", req.kind, req.to),
            ))
        }
    }

    /// Record and perform one send. The action row is written before anything
    /// else; a send the edges refuse is recorded as failed without reaching the broker.
    pub async fn send(&self, req: SendRequest) -> SendDone {
        let input = SendInput {
            client_ref: ActionId::nil(),
            kind: req.kind,
            to: req.to.clone(),
            service: req.service.clone(),
            caused_by: req.caused_by,
            body: req.body.clone(),
            context: req.context.clone(),
            turn_outcome: req.turn_outcome,
            local: req.local,
        };
        let mut request = ActionRequest::new(ActionKind::Send, serde_json::Value::Null);
        request.turn = req.turn;
        request.step = req.step;
        request.requested_by = req.requested_by;
        request.call_id = req.call_id.clone();
        request.service = req.service.clone();
        request.root = req.root;

        // The client_ref is the action's own id, assigned as the row is written.
        let stored = input.clone();
        let written = self
            .store
            .with(move |s| {
                s.request_action_with(request, |id| {
                    Some(serde_json::to_value(SendInput { client_ref: id, ..stored }).expect("send input"))
                })
            })
            .await;
        let id = match written {
            Ok(id) => id,
            Err(e) => {
                let err = ErrorBody::retryable(codes::INTERNAL, format!("could not record the send: {e}"));
                return SendDone { action: ActionId::nil(), result: Err(err) };
            }
        };
        let input = SendInput { client_ref: id, ..input };

        if let Err(err) = self.check_edges(&req) {
            self.finish_failed(id, &err).await;
            return SendDone { action: id, result: Err(err) };
        }
        if let Err(e) = self.store.with(move |s| s.start_action(id)).await {
            let err = ErrorBody::retryable(codes::INTERNAL, format!("could not start the send: {e}"));
            return SendDone { action: id, result: Err(err) };
        }

        match self.broker.request::<_, SendOutput>(broker::SEND, &input).await {
            Ok(reply) => {
                if let Err(e) = self.store.with(move |s| s.succeed_send(id, &reply)).await {
                    tracing::error!(action = %id, "send accepted but not recorded: {e}");
                }
                SendDone { action: id, result: Ok(reply) }
            }
            Err(CallError::Remote(err)) => {
                self.finish_failed(id, &err).await;
                SendDone { action: id, result: Err(err) }
            }
            // The broker may or may not have accepted it. Leave the row running:
            // recovery settles it by lookup when the valet restarts.
            Err(e) => {
                let err = ErrorBody::retryable(local::codes::BROKER_UNAVAILABLE, e.to_string());
                SendDone { action: id, result: Err(err) }
            }
        }
    }

    async fn finish_failed(&self, id: ActionId, err: &ErrorBody) {
        let output = serde_json::to_value(SendFailure { error: err.clone() }).expect("send failure");
        let res = self
            .store
            .with(move |s| {
                // A refused send never started; move it through running so the
                // store's forward-only transitions hold.
                s.start_action(id).ok();
                s.fail_action(id, &output)
            })
            .await;
        if let Err(e) = res {
            tracing::error!(action = %id, "could not record failed send: {e}");
        }
    }
}

//! Section 6.1: broker ⇔ valet. One connection per valet, on its own socket.
//! Nothing on this line names the sender: the broker takes it from the socket.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{
    ActionId, Block, Interaction, InteractionId, InteractionKind, InteractionState, Outcome, Sender, Timestamp,
    TurnOutcome,
};

pub const HELLO: &str = "hello";
pub const SEND: &str = "send";
pub const DELIVER: &str = "deliver";
pub const CLOSE: &str = "close";
pub const LOOKUP: &str = "lookup";
pub const STATUS: &str = "status";

/// Error codes of this line, grouped by op.
pub mod codes {
    // hello
    pub const PROTOCOL_MISMATCH: &str = "protocol_mismatch";
    pub const ALREADY_CONNECTED: &str = "already_connected";
    // send
    pub const NOT_ALLOWED: &str = "not_allowed";
    pub const UNKNOWN_USER: &str = "unknown_user";
    pub const NOT_AN_AGENT: &str = "not_an_agent";
    pub const NOT_OWED: &str = "not_owed";
    pub const HOP_LIMIT: &str = "hop_limit";
    pub const TOO_LARGE: &str = "too_large";
    // deliver
    pub const STORE_FAILED: &str = "store_failed";
    // close
    pub const NOT_YOURS: &str = "not_yours";
    pub const NOT_ACKED: &str = "not_acked";
    // status
    pub const UNKNOWN_ROOT: &str = "unknown_root";
    pub const NOT_PARTICIPANT: &str = "not_participant";
    /// EXTENSION: any op other than `hello` before `hello` succeeded.
    pub const NO_HELLO: &str = "no_hello";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloInput {
    pub protocol: u32,
}

/// Who a sender may reach. One entry for the user and each of its services.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edges {
    pub messages: Vec<String>,
    pub directives: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloOutput {
    /// Taken from the socket, never from the request.
    pub user: String,
    /// The resolved user_profile with its composer, llm and tool profiles,
    /// as serialized by `profile` (M1.1). The valet cannot read /etc/gllmh.
    pub profile: serde_json::Value,
    /// Keyed by sender: the user itself and each `user::service`.
    pub edges: BTreeMap<Sender, Edges>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendInput {
    pub client_ref: ActionId,
    pub kind: InteractionKind,
    pub to: String,
    /// Set when one of the sender's services submitted it; `from` becomes `user::service`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Required for a result: the directive it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<InteractionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<Block>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_outcome: Option<TurnOutcome>,
    /// EXTENSION: true when a UI or controller submitted it on local.sock.
    /// Resets `hops` to 0 while `root` still follows `caused_by` (section 3, hops).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendOutput {
    pub id: InteractionId,
    pub root: InteractionId,
    pub hops: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliverInput {
    pub interaction: Interaction,
}

/// Sent only after the valet has recorded the interaction: this reply is the ack.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverOutput {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseInput {
    pub id: InteractionId,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseOutput {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookupInput {
    pub client_ref: ActionId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookupOutput {
    pub found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction: Option<SendOutput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusInput {
    pub root: InteractionId,
}

/// One interaction of a run, without its body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusRow {
    pub id: InteractionId,
    pub kind: InteractionKind,
    pub from: Sender,
    pub to: String,
    pub state: InteractionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    pub hops: u32,
    pub accepted_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<Timestamp>,
}

impl From<&Interaction> for StatusRow {
    fn from(i: &Interaction) -> Self {
        Self {
            id: i.id,
            kind: i.kind,
            from: i.from.clone(),
            to: i.to.clone(),
            state: i.state,
            outcome: i.outcome,
            hops: i.hops,
            accepted_at: i.accepted_at,
            closed_at: i.closed_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusOutput {
    pub interactions: Vec<StatusRow>,
}

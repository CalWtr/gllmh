//! Section 6.3: valet ⇔ UI, controllers and services.
//!
//! UIs and controllers connect to `local.sock`; each service to its own
//! `services/<name>.sock`. The same ops run on both; a service's name comes
//! from the socket it connected on, never from the request.

use serde::{Deserialize, Serialize};

use crate::types::{Block, Interaction, InteractionId, InteractionKind};

pub use super::broker::{SendOutput as SubmitOutput, StatusInput, StatusOutput};

/// valet → client: a request whose reply is ignored (the client answers `{}`).
pub const NOTIFY: &str = "notify";
pub const SUBMIT: &str = "submit";
/// Forwarded to the broker as its `status`.
pub const STATUS: &str = "status";

/// `submit` errors are the broker's `send` codes (`protocol::broker::codes`),
/// plus these.
pub mod codes {
    /// EXTENSION: the broker connection is down; retry later.
    pub const BROKER_UNAVAILABLE: &str = "broker_unavailable";
    /// EXTENSION: a submit with a kind a client may not send (only message and directive).
    pub const BAD_KIND: &str = "bad_kind";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyInput {
    /// The whole object, as received.
    pub interaction: Interaction,
}

/// Acknowledges a notification received on this connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyOutput {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitInput {
    /// message or directive; a result is never submitted.
    pub kind: InteractionKind,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<Block>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<InteractionId>,
}

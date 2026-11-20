//! Stored objects of spec section 3, and the enums every module shares.
//!
//! Field names and serde shapes match the spec exactly: these are what goes
//! over the wire (section 6) and what the `v1_` views expose (section 5).
//! Optional spec fields (`name?:`) are `Option` and skipped when absent.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Every stored timestamp: UTC, serialized as RFC 3339.
pub type Timestamp = DateTime<Utc>;

/// Id of an `interaction`; also the id of the `turn` it opens.
pub type InteractionId = Ulid;
/// Id of an `action`; also the `client_ref` of a send.
pub type ActionId = Ulid;

/// Current time as stored in every row.
pub fn now() -> Timestamp {
    Utc::now()
}

/// Separator between a user and its service in a sender name.
pub const SERVICE_SEP: &str = "::";

/// A sender: a user, or a user's service (`owner::service`).
///
/// User and service names never contain `:` (enforced by `gllmh check`),
/// so the string always splits one way.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sender {
    pub user: String,
    pub service: Option<String>,
}

impl Sender {
    pub fn user(user: impl Into<String>) -> Self {
        Self { user: user.into(), service: None }
    }

    pub fn service(user: impl Into<String>, service: impl Into<String>) -> Self {
        Self { user: user.into(), service: Some(service.into()) }
    }

    pub fn parse(s: &str) -> Result<Self, InvalidName> {
        match s.split_once(SERVICE_SEP) {
            None => {
                check_name(s)?;
                Ok(Self::user(s))
            }
            Some((u, svc)) => {
                check_name(u)?;
                check_name(svc)?;
                Ok(Self::service(u, svc))
            }
        }
    }
}

impl std::fmt::Display for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.service {
            None => f.write_str(&self.user),
            Some(svc) => write!(f, "{}{SERVICE_SEP}{svc}", self.user),
        }
    }
}

impl TryFrom<String> for Sender {
    type Error = InvalidName;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Sender> for String {
    fn from(s: Sender) -> Self {
        s.to_string()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid name {0:?}: names are non-empty and may not contain ':'")]
pub struct InvalidName(pub String);

/// A user or service name: non-empty, no `:`.
pub fn check_name(s: &str) -> Result<(), InvalidName> {
    if s.is_empty() || s.contains(':') { Err(InvalidName(s.to_string())) } else { Ok(()) }
}

macro_rules! snake_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident),+ $(,)? }) => {
        $(#[$m])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
            strum::Display, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
        )]
        #[serde(rename_all = "snake_case")]
        #[strum(serialize_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            /// The snake_case name used on the wire and in SQLite.
            pub fn as_str(self) -> &'static str {
                self.into()
            }
        }
    };
}

snake_enum!(
    /// `interaction.kind`.
    InteractionKind { Message, Directive, Result }
);

snake_enum!(
    /// `interaction.state`. Moves forward only, except requeue
    /// (`delivered` → `accepted`), per section 2.
    InteractionState { Accepted, Delivered, Acked, Closed, Dead }
);

snake_enum!(
    /// `interaction.outcome`, written by `close`.
    Outcome { Received, Done, CutOff, Failed, Interrupted }
);

snake_enum!(
    /// `interaction.turn_outcome` on a `result`: how the directive's turn ended.
    TurnOutcome { Done, Failed, CutOff, Interrupted }
);

snake_enum!(
    /// `turn.state`.
    TurnState { Queued, Running, Done, CutOff, Failed, Interrupted }
);

snake_enum!(
    /// `action.kind`.
    ActionKind { Receive, Send, LlmCall, ToolCall }
);

snake_enum!(
    /// `action.state`.
    ActionState { Requested, Running, Succeeded, Failed, Denied, Interrupted }
);

snake_enum!(
    /// `hazardness`: a label for the worst effect a tool can have. Not enforced in 0.1.
    Hazardness { Safe, Caution, Destructive, Critical }
);

impl TurnState {
    /// The turn has ended, in success or failure.
    pub fn is_end(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }

    /// The outcome its interaction closes with, and its result's `turn_outcome`.
    pub fn turn_outcome(self) -> Option<TurnOutcome> {
        match self {
            Self::Queued | Self::Running => None,
            Self::Done => Some(TurnOutcome::Done),
            Self::CutOff => Some(TurnOutcome::CutOff),
            Self::Failed => Some(TurnOutcome::Failed),
            Self::Interrupted => Some(TurnOutcome::Interrupted),
        }
    }
}

impl From<TurnOutcome> for Outcome {
    fn from(o: TurnOutcome) -> Self {
        match o {
            TurnOutcome::Done => Self::Done,
            TurnOutcome::Failed => Self::Failed,
            TurnOutcome::CutOff => Self::CutOff,
            TurnOutcome::Interrupted => Self::Interrupted,
        }
    }
}

impl ActionState {
    /// Unfinished: recovery settles or interrupts it.
    pub fn is_unfinished(self) -> bool {
        matches!(self, Self::Requested | Self::Running)
    }
}

/// A named piece of context carried by a `directive`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub name: String,
    pub content: String,
}

/// An `interaction`, as logged by the broker and as delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    pub id: InteractionId,
    pub kind: InteractionKind,
    pub client_ref: ActionId,
    pub from: Sender,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<InteractionId>,
    pub root: InteractionId,
    pub hops: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<Block>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_outcome: Option<TurnOutcome>,
    pub state: InteractionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    pub accepted_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<Timestamp>,
}

/// An `action`: one operation a valet executes and records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: ActionId,
    pub kind: ActionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<InteractionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<ActionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction: Option<InteractionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<InteractionId>,
    pub input: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hazardness: Option<Hazardness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,
    pub state: ActionState,
    pub requested_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
}

/// A `turn`: the actions an agent runs in response to one interaction.
/// Its id is its `trigger`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub trigger: InteractionId,
    pub root: InteractionId,
    pub state: TurnState,
    pub steps: u32,
    pub opened_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
}

/// A `mark`: a composer's own label on any id in its store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub target: Ulid,
    pub key: String,
    pub value: serde_json::Value,
    pub set_by: InteractionId,
    pub set_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_round_trip() {
        let s: Sender = serde_json::from_str("\"agent::timer\"").unwrap();
        assert_eq!(s, Sender::service("agent", "timer"));
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"agent::timer\"");
        assert_eq!(Sender::parse("user").unwrap(), Sender::user("user"));
        assert!(Sender::parse("a:b").is_err());
        assert!(Sender::parse("a::b::c").is_err());
        assert!(Sender::parse("").is_err());
    }

    #[test]
    fn enum_names_match_serde() {
        use strum::VariantArray;
        for v in ActionState::VARIANTS {
            assert_eq!(serde_json::to_value(v).unwrap(), v.as_str());
        }
        for v in Outcome::VARIANTS {
            assert_eq!(serde_json::to_value(v).unwrap(), v.as_str());
        }
        assert_eq!(Outcome::CutOff.as_str(), "cut_off");
        assert_eq!(ActionKind::LlmCall.as_str(), "llm_call");
        assert_eq!("tool_call".parse::<ActionKind>().unwrap(), ActionKind::ToolCall);
        assert_eq!(serde_json::to_string(&TurnState::CutOff).unwrap(), "\"cut_off\"");
        assert_eq!(TurnState::Interrupted.turn_outcome(), Some(TurnOutcome::Interrupted));
        assert_eq!(Outcome::from(TurnOutcome::Done), Outcome::Done);
    }
}

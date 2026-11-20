//! One user's SQLite store (spec sections 2, 3 and 5).
//!
//! The valet owns a single [`Store`] writer. The composer opens a [`ReadOnlyStore`]
//! from the database path in `init`. Both APIs are synchronous. Async callers
//! should do their SQLite work on a dedicated thread or in `spawn_blocking`.
//!
//! The composer contract is the section-3 columns in these SQLite views:
//!
//! - `v1_action`: `id`, `kind`, `turn`, `step`, `requested_by`, `call_id`,
//!   `service`, `interaction`, `root`, `input`, `output`, `hazardness`,
//!   `reasoning`, `state`, `requested_at`, `ended_at`.
//! - `v1_turn`: `trigger`, `root`, `state`, `steps`, `opened_at`, `ended_at`.
//! - `v1_mark`: `target`, `key`, `value`, `set_by`, `set_at`.
//!
//! IDs are ULID text. JSON fields are JSON text, timestamps are UTC RFC 3339
//! text, and absent optional fields are SQL NULL. Actions sort oldest first by
//! `id`.

mod read;
#[cfg(test)]
mod tests;
mod write;

pub use read::ReadOnlyStore;
pub use write::{ActionRequest, ReceiveOutcome, RunningTurn, Store};

use crate::types::{ActionId, ActionState, InteractionId, TurnState};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not create the user store directory")]
    CreateDir(#[source] std::io::Error),
    #[error("could not enable WAL mode, got {0}")]
    JournalMode(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    ActionId(#[from] ulid::MonotonicError),
    #[error("cannot move action {id} to {to}: missing row, invalid state or incompatible action kind")]
    ActionTransition { id: ActionId, to: ActionState },
    #[error("cannot move turn {trigger} to {to}: missing row or invalid state")]
    TurnTransition { trigger: InteractionId, to: TurnState },
    #[error("cannot increment steps for turn {trigger}: missing row, not running or at step limit {limit}")]
    TurnStep { trigger: InteractionId, limit: u32 },
}

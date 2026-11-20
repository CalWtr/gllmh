//! The broker's interaction log (spec sections 2–5).
//!
//! This API is synchronous. The broker should run it on a dedicated thread or
//! through `spawn_blocking`, and serialize delivery and connection lifecycle
//! operations for each recipient.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Datelike, SecondsFormat, TimeDelta};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params, types::Type};

use crate::protocol::broker::{SendInput, StatusRow};
use crate::types::{
    self, ActionId, Interaction, InteractionId, InteractionKind, InteractionState, Outcome, Sender, Timestamp,
};

const SCHEMA_V1: &str = r#"
    CREATE TABLE interaction (
        id TEXT NOT NULL PRIMARY KEY,
        kind TEXT NOT NULL,
        client_ref TEXT NOT NULL,
        "from" TEXT NOT NULL,
        "to" TEXT NOT NULL,
        caused_by TEXT REFERENCES interaction(id),
        root TEXT NOT NULL REFERENCES interaction(id),
        hops INTEGER NOT NULL,
        body TEXT,
        context TEXT,
        turn_outcome TEXT,
        state TEXT NOT NULL,
        outcome TEXT,
        accepted_at TEXT NOT NULL,
        closed_at TEXT,
        UNIQUE ("from", client_ref)
    );
    CREATE INDEX interaction_recipient_state ON interaction ("to", state);
    CREATE INDEX interaction_root ON interaction (root);
    CREATE UNIQUE INDEX interaction_result_cause ON interaction (caused_by) WHERE kind = 'result';
    PRAGMA user_version = 1;
"#;

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("could not open interaction log at {path:?}")]
    OpenFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("interaction log could not enable WAL (journal mode is {0})")]
    JournalMode(String),
    #[error("unsupported interaction log schema version {0}")]
    SchemaVersion(i64),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not encode interaction payload")]
    Payload(#[from] serde_json::Error),
    #[error("unknown parent interaction {0}")]
    UnknownParent(InteractionId),
    #[error("result is not owed to the specified director")]
    NotOwed,
    #[error("hop count overflows for a child of {0}")]
    HopOverflow(InteractionId),
    #[error("interaction {id} must exist in state {expected} to move to {next}")]
    InvalidTransition { id: InteractionId, expected: InteractionState, next: InteractionState },
}

/// The stored interaction and whether this send reused an existing client reference.
#[derive(Debug)]
pub struct AcceptResult {
    pub interaction: Interaction,
    pub duplicate: bool,
}

#[derive(Debug)]
pub struct InteractionLog {
    conn: Connection,
}

impl InteractionLog {
    /// Open or create a file-backed log in WAL mode and apply pending migrations.
    /// New database files are created with owner-only permissions.
    /// At broker startup, call [Self::requeue_all_delivered] before serving users.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        let path = path.as_ref();
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|source| LogError::OpenFile { path: path.to_owned(), source })?;
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        let journal: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if journal != "wal" {
            return Err(LogError::JournalMode(journal));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        Self::migrate(conn)
    }

    /// Create an isolated, in-memory log with the same schema.
    pub fn open_in_memory() -> Result<Self, LogError> {
        Self::migrate(Connection::open_in_memory()?)
    }

    fn migrate(mut conn: Connection) -> Result<Self, LogError> {
        conn.pragma_update(None, "foreign_keys", true)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match tx.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? {
            0 => tx.execute_batch(SCHEMA_V1)?,
            1 => {}
            version => return Err(LogError::SchemaVersion(version)),
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    /// Atomically accept a send or return the existing row for `(from, client_ref)`.
    ///
    /// The caller supplies a socket-derived sender (including any validated
    /// service) and checks recipient, payload shape, edge, size and hop policy.
    /// Check [Self::lookup] before those checks so retries recover the original
    /// acceptance. For a new non-local, non-result send with a parent, use
    /// [Self::get] and check `parent.hops + 1` against the configured limit.
    /// Root and hop fields are immutable, and the log re-reads the parent in its
    /// write transaction. A result's obligation and routing are checked here.
    pub fn accept(&mut self, from: &Sender, input: &SendInput) -> Result<AcceptResult, LogError> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(interaction) = Self::lookup_on(&tx, from, input.client_ref)? {
            tx.commit()?;
            return Ok(AcceptResult { interaction, duplicate: true });
        }

        let parent = match input.caused_by {
            Some(id) => Self::get_on(&tx, id)?,
            None => None,
        };
        if input.kind == InteractionKind::Result {
            let directive = parent.as_ref().ok_or(LogError::NotOwed)?;
            if from.service.is_some()
                || input.to != directive.from.user
                || !Self::result_owed_on(&tx, directive.id, &from.user)?
            {
                return Err(LogError::NotOwed);
            }
        } else if let (Some(id), None) = (input.caused_by, &parent) {
            return Err(LogError::UnknownParent(id));
        }

        let id = InteractionId::new();
        let root = parent.as_ref().map_or(id, |p| p.root);
        let hops = match parent {
            Some(p) if !input.local => p.hops.checked_add(1).ok_or(LogError::HopOverflow(p.id))?,
            _ => 0,
        };
        let accepted_at = types::now();
        let body = input.body.as_ref().map(serde_json::to_string).transpose()?;
        let context = input.context.as_ref().map(serde_json::to_string).transpose()?;
        tx.execute(
            r#"INSERT INTO interaction
               (id, kind, client_ref, "from", "to", caused_by, root, hops, body, context, turn_outcome, state, accepted_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'accepted', ?12)"#,
            params![
                id.to_string(),
                input.kind.as_str(),
                input.client_ref.to_string(),
                from.to_string(),
                input.to,
                input.caused_by.map(|id| id.to_string()),
                root.to_string(),
                hops,
                body,
                context,
                input.turn_outcome.map(|outcome| outcome.as_str()),
                timestamp_text(accepted_at),
            ],
        )?;
        tx.commit()?;

        let interaction = Interaction {
            id,
            kind: input.kind,
            client_ref: input.client_ref,
            from: from.clone(),
            to: input.to.clone(),
            caused_by: input.caused_by,
            root,
            hops,
            body: input.body.clone(),
            context: input.context.clone(),
            turn_outcome: input.turn_outcome,
            state: InteractionState::Accepted,
            outcome: None,
            accepted_at,
            closed_at: None,
        };
        Ok(AcceptResult { interaction, duplicate: false })
    }

    /// Read one interaction by ID, including its payload.
    pub fn get(&self, id: InteractionId) -> Result<Option<Interaction>, LogError> {
        Self::get_on(&self.conn, id)
    }

    fn get_on(conn: &Connection, id: InteractionId) -> Result<Option<Interaction>, LogError> {
        Ok(conn.query_row("SELECT * FROM interaction WHERE id = ?1", [id.to_string()], interaction_row).optional()?)
    }

    /// Find the interaction accepted for a sender's client reference.
    pub fn lookup(&self, from: &Sender, client_ref: ActionId) -> Result<Option<Interaction>, LogError> {
        Self::lookup_on(&self.conn, from, client_ref)
    }

    fn lookup_on(conn: &Connection, from: &Sender, client_ref: ActionId) -> Result<Option<Interaction>, LogError> {
        Ok(conn
            .query_row(
                r#"SELECT * FROM interaction WHERE "from" = ?1 AND client_ref = ?2"#,
                [from.to_string(), client_ref.to_string()],
                interaction_row,
            )
            .optional()?)
    }

    /// Peek at the recipient's oldest accepted interaction, in log append order.
    /// The caller must mark it delivered before pushing it over the wire.
    pub fn next_deliverable(&self, to: &str) -> Result<Option<Interaction>, LogError> {
        Ok(self
            .conn
            .query_row(
                r#"SELECT * FROM interaction WHERE "to" = ?1 AND state = 'accepted' ORDER BY rowid LIMIT 1"#,
                [to],
                interaction_row,
            )
            .optional()?)
    }

    /// Transition accepted → delivered.
    pub fn mark_delivered(&self, id: InteractionId) -> Result<(), LogError> {
        self.transition(id, InteractionState::Accepted, InteractionState::Delivered, None)
    }

    /// Transition delivered → acked.
    pub fn ack(&self, id: InteractionId) -> Result<(), LogError> {
        self.transition(id, InteractionState::Delivered, InteractionState::Acked, None)
    }

    /// Transition acked → closed and record the recipient's outcome and time.
    pub fn close(&self, id: InteractionId, outcome: Outcome) -> Result<(), LogError> {
        self.transition(id, InteractionState::Acked, InteractionState::Closed, Some(outcome))
    }

    fn transition(
        &self,
        id: InteractionId,
        expected: InteractionState,
        next: InteractionState,
        outcome: Option<Outcome>,
    ) -> Result<(), LogError> {
        let changed = self.conn.execute(
            "UPDATE interaction SET state = ?1, outcome = ?2, closed_at = ?3 WHERE id = ?4 AND state = ?5",
            params![
                next.as_str(),
                outcome.map(Outcome::as_str),
                outcome.map(|_| timestamp_text(types::now())),
                id.to_string(),
                expected.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(LogError::InvalidTransition { id, expected, next });
        }
        Ok(())
    }

    /// On disconnect, requeue the recipient's delivered but unacknowledged rows.
    pub fn requeue_delivered(&self, to: &str) -> Result<usize, LogError> {
        Ok(self
            .conn
            .execute(r#"UPDATE interaction SET state = 'accepted' WHERE "to" = ?1 AND state = 'delivered'"#, [to])?)
    }

    /// At startup, requeue all delivered but unacknowledged rows.
    pub fn requeue_all_delivered(&self) -> Result<usize, LogError> {
        Ok(self.conn.execute("UPDATE interaction SET state = 'accepted' WHERE state = 'delivered'", [])?)
    }

    /// Mark accepted rows dead once their age reaches the TTL. Returns the count.
    pub fn expire(&self, ttl_s: u64, now: Timestamp) -> Result<usize, LogError> {
        let Some(cutoff) =
            TimeDelta::from_std(Duration::from_secs(ttl_s)).ok().and_then(|ttl| now.checked_sub_signed(ttl))
        else {
            return Ok(0);
        };
        // A cutoff earlier than the RFC 3339 year range matches nothing.
        // A later cutoff matches every stored timestamp.
        if cutoff.year() < 0 {
            return Ok(0);
        }
        let cutoff = (cutoff.year() <= 9999).then(|| timestamp_text(cutoff));
        Ok(self.conn.execute(
            "UPDATE interaction SET state = 'dead' WHERE state = 'accepted' AND (?1 IS NULL OR accepted_at <= ?1)",
            [cutoff],
        )?)
    }

    /// List the run in log append order, without reading bodies or context.
    /// An empty list means the root is unknown.
    pub fn by_root(&self, root: InteractionId) -> Result<Vec<StatusRow>, LogError> {
        let mut stmt = self.conn.prepare(
            r#"SELECT id, kind, "from", "to", state, outcome, hops, accepted_at, closed_at
               FROM interaction WHERE root = ?1 ORDER BY rowid"#,
        )?;
        Ok(stmt.query_map([root.to_string()], status_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Return the users in the run, counting each service under its owner.
    pub fn participants(&self, root: InteractionId) -> Result<BTreeSet<String>, LogError> {
        let mut stmt = self.conn.prepare(
            r#"SELECT "from" AS participant FROM interaction WHERE root = ?1
               UNION SELECT "to" FROM interaction WHERE root = ?1"#,
        )?;
        Ok(stmt
            .query_map([root.to_string()], |row| Ok(parse_text(row, "participant", Sender::parse)?.user))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Check whether the directive was sent to this user and no result has been accepted for it.
    /// Once accepted, a result counts as the answer in every delivery state.
    pub fn result_owed(&self, directive_id: InteractionId, user: &str) -> Result<bool, LogError> {
        Self::result_owed_on(&self.conn, directive_id, user)
    }

    fn result_owed_on(conn: &Connection, directive_id: InteractionId, user: &str) -> Result<bool, LogError> {
        Ok(conn.query_row(
            r#"SELECT EXISTS (
                   SELECT 1 FROM interaction
                   WHERE id = ?1 AND kind = 'directive' AND "to" = ?2
                       AND NOT EXISTS (SELECT 1 FROM interaction WHERE caused_by = ?1 AND kind = 'result')
               )"#,
            params![directive_id.to_string(), user],
            |row| row.get(0),
        )?)
    }
}

fn timestamp_text(value: Timestamp) -> String {
    // Fixed precision and UTC make text comparisons exact, including fractional seconds.
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_text<T, E>(row: &Row<'_>, column: &str, parse: impl FnOnce(&str) -> Result<T, E>) -> rusqlite::Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let index = row.as_ref().column_index(column)?;
    let value: String = row.get(index)?;
    parse(&value).map_err(|err| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(err)))
}

fn parse_optional_text<T, E>(
    row: &Row<'_>,
    column: &str,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> rusqlite::Result<Option<T>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let index = row.as_ref().column_index(column)?;
    let value: Option<String> = row.get(index)?;
    value
        .map(|value| {
            parse(&value).map_err(|err| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(err)))
        })
        .transpose()
}

fn status_row(row: &Row<'_>) -> rusqlite::Result<StatusRow> {
    Ok(StatusRow {
        id: parse_text(row, "id", str::parse)?,
        kind: parse_text(row, "kind", str::parse)?,
        from: parse_text(row, "from", Sender::parse)?,
        to: row.get("to")?,
        state: parse_text(row, "state", str::parse)?,
        outcome: parse_optional_text(row, "outcome", str::parse)?,
        hops: row.get("hops")?,
        accepted_at: parse_text(row, "accepted_at", str::parse)?,
        closed_at: parse_optional_text(row, "closed_at", str::parse)?,
    })
}

fn interaction_row(row: &Row<'_>) -> rusqlite::Result<Interaction> {
    let status = status_row(row)?;
    Ok(Interaction {
        id: status.id,
        kind: status.kind,
        client_ref: parse_text(row, "client_ref", str::parse)?,
        from: status.from,
        to: status.to,
        caused_by: parse_optional_text(row, "caused_by", str::parse)?,
        root: parse_text(row, "root", str::parse)?,
        hops: status.hops,
        body: parse_optional_text(row, "body", |value| serde_json::from_str(value))?,
        context: parse_optional_text(row, "context", |value| serde_json::from_str(value))?,
        turn_outcome: parse_optional_text(row, "turn_outcome", str::parse)?,
        state: status.state,
        outcome: status.outcome,
        accepted_at: status.accepted_at,
        closed_at: status.closed_at,
    })
}

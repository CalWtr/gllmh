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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::{Value, json};

    use super::*;
    use crate::types::{Block, TurnOutcome};

    fn message(to: &str) -> SendInput {
        SendInput {
            client_ref: ActionId::new(),
            kind: InteractionKind::Message,
            to: to.into(),
            service: None,
            caused_by: None,
            body: Some(json!({"message": "hello"})),
            context: None,
            turn_outcome: None,
            local: false,
        }
    }

    fn assert_state(log: &InteractionLog, id: InteractionId, state: InteractionState) {
        assert_eq!(log.get(id).unwrap().unwrap().state, state);
    }

    fn assert_rejected(result: Result<(), LogError>) {
        assert!(matches!(result, Err(LogError::InvalidTransition { .. })), "{result:?}");
    }

    #[test]
    fn delivery_ack_and_close_require_the_expected_state() {
        use InteractionState::{Accepted, Acked, Closed, Delivered};
        let mut log = InteractionLog::open_in_memory().unwrap();
        let row = log.accept(&Sender::user("alice"), &message("bob")).unwrap().interaction;
        let id = row.id;

        assert_rejected(log.mark_delivered(InteractionId::new()));
        assert_rejected(log.ack(id));
        assert_rejected(log.close(id, Outcome::Received));
        assert_state(&log, id, Accepted);

        log.mark_delivered(id).unwrap();
        assert_rejected(log.mark_delivered(id));
        assert_rejected(log.close(id, Outcome::Received));
        assert_state(&log, id, Delivered);

        log.ack(id).unwrap();
        assert_rejected(log.mark_delivered(id));
        assert_rejected(log.ack(id));
        assert_state(&log, id, Acked);
        assert_eq!(log.requeue_all_delivered().unwrap(), 0);

        log.close(id, Outcome::Done).unwrap();
        assert_rejected(log.mark_delivered(id));
        assert_rejected(log.ack(id));
        assert_rejected(log.close(id, Outcome::Failed));
        assert_state(&log, id, Closed);
        let closed = log.get(id).unwrap().unwrap();
        assert_eq!(closed.outcome, Some(Outcome::Done));
        assert!(closed.closed_at.is_some());
        assert_eq!(
            closed,
            Interaction { state: Closed, outcome: Some(Outcome::Done), closed_at: closed.closed_at, ..row }
        );
    }

    #[test]
    fn deliver_oldest_first_and_requeue_only_the_disconnected_recipient() {
        let mut log = InteractionLog::open_in_memory().unwrap();
        let from = Sender::user("alice");
        let first = log.accept(&from, &message("bob")).unwrap().interaction;
        let other = log.accept(&from, &message("carol")).unwrap().interaction;
        let second = log.accept(&from, &message("bob")).unwrap().interaction;

        assert_eq!(log.next_deliverable("bob").unwrap(), Some(first.clone()));
        log.mark_delivered(first.id).unwrap();
        log.mark_delivered(other.id).unwrap();
        assert_eq!(log.next_deliverable("bob").unwrap(), Some(second));
        assert_eq!(log.requeue_delivered("bob").unwrap(), 1);
        assert_eq!(log.requeue_delivered("bob").unwrap(), 0);
        assert_eq!(log.next_deliverable("bob").unwrap(), Some(first));
        assert!(log.next_deliverable("carol").unwrap().is_none());
        assert_state(&log, other.id, InteractionState::Delivered);
    }

    #[test]
    fn duplicate_returns_the_original_row_and_is_scoped_to_the_full_sender() {
        let mut log = InteractionLog::open_in_memory().unwrap();
        let from = Sender::user("alice");
        let mut input = message("bob");
        assert!(log.lookup(&from, input.client_ref).unwrap().is_none());
        let first = log.accept(&from, &input).unwrap();
        assert!(!first.duplicate);
        log.mark_delivered(first.interaction.id).unwrap();
        let stored = Interaction { state: InteractionState::Delivered, ..first.interaction };

        input.to = "carol".into();
        input.body = Some(json!("changed"));
        input.caused_by = Some(InteractionId::new());
        let replay = log.accept(&from, &input).unwrap();
        assert!(replay.duplicate);
        assert_eq!(replay.interaction, stored);
        assert_eq!(log.lookup(&from, input.client_ref).unwrap(), Some(stored.clone()));

        input.caused_by = None;
        let service = Sender::service("alice", "timer");
        input.service = service.service.clone();
        let separate = log.accept(&service, &input).unwrap();
        assert!(!separate.duplicate);
        assert_ne!(separate.interaction.id, stored.id);
    }

    #[test]
    fn causality_retains_root_and_local_resets_hops() {
        let mut log = InteractionLog::open_in_memory().unwrap();
        let root = log.accept(&Sender::service("alice", "timer"), &message("bob")).unwrap().interaction;
        assert_eq!((root.root, root.hops), (root.id, 0));

        let mut input = message("carol");
        input.caused_by = Some(root.id);
        let child = log.accept(&Sender::user("bob"), &input).unwrap().interaction;
        assert_eq!((child.root, child.hops), (root.id, 1));

        let mut input = message("dana");
        input.caused_by = Some(child.id);
        input.local = true;
        let local = log.accept(&Sender::user("carol"), &input).unwrap().interaction;
        assert_eq!((local.root, local.hops), (root.id, 0));

        let mut input = message("alice");
        input.caused_by = Some(local.id);
        let next = log.accept(&Sender::user("dana"), &input).unwrap().interaction;
        assert_eq!((next.root, next.hops), (root.id, 1));

        assert_eq!(
            log.by_root(root.id).unwrap(),
            [&root, &child, &local, &next].into_iter().map(StatusRow::from).collect::<Vec<_>>()
        );
        assert_eq!(
            log.participants(root.id).unwrap(),
            ["alice", "bob", "carol", "dana"].map(String::from).into_iter().collect::<BTreeSet<_>>()
        );
        let missing = InteractionId::new();
        assert!(log.get(missing).unwrap().is_none());
        assert!(log.by_root(missing).unwrap().is_empty());
        assert!(log.participants(missing).unwrap().is_empty());
        input.client_ref = ActionId::new();
        input.caused_by = Some(missing);
        assert!(matches!(log.accept(&Sender::user("dana"), &input), Err(LogError::UnknownParent(id)) if id == missing));
        assert!(log.lookup(&Sender::user("dana"), input.client_ref).unwrap().is_none());
    }

    #[test]
    fn expiry_moves_only_accepted_rows_to_dead_at_the_ttl_boundary() {
        let mut log = InteractionLog::open_in_memory().unwrap();
        let from = Sender::user("alice");
        let accepted = log.accept(&from, &message("bob")).unwrap().interaction;
        let delivered = log.accept(&from, &message("bob")).unwrap().interaction;
        let acked = log.accept(&from, &message("bob")).unwrap().interaction;
        let closed = log.accept(&from, &message("bob")).unwrap().interaction;
        for id in [delivered.id, acked.id, closed.id] {
            log.mark_delivered(id).unwrap();
        }
        log.ack(acked.id).unwrap();
        log.ack(closed.id).unwrap();
        log.close(closed.id, Outcome::Received).unwrap();

        let boundary = accepted.accepted_at + TimeDelta::seconds(10);
        assert_eq!(log.expire(10, boundary - TimeDelta::nanoseconds(1)).unwrap(), 0);
        assert_eq!(log.expire(u64::MAX, boundary).unwrap(), 0);
        assert_eq!(log.expire(10, boundary).unwrap(), 1);
        assert_eq!(log.expire(10, boundary).unwrap(), 0);
        let dead = log.get(accepted.id).unwrap().unwrap();
        assert_eq!(dead, Interaction { state: InteractionState::Dead, ..accepted });
        assert_rejected(log.mark_delivered(dead.id));
        assert_rejected(log.ack(dead.id));
        assert_rejected(log.close(dead.id, Outcome::Failed));
        assert_state(&log, delivered.id, InteractionState::Delivered);
        assert_state(&log, acked.id, InteractionState::Acked);
        assert_state(&log, closed.id, InteractionState::Closed);
        assert!(log.next_deliverable("bob").unwrap().is_none());
    }

    #[test]
    fn result_owed_requires_the_directive_recipient_and_one_answer() {
        let mut log = InteractionLog::open_in_memory().unwrap();
        let director = Sender::service("alice", "timer");
        let worker = Sender::user("bob");
        let ordinary = log.accept(&director, &message("bob")).unwrap().interaction;
        let mut input = message("bob");
        input.kind = InteractionKind::Directive;
        input.body = None;
        input.context = Some(vec![Block { name: "instructions".into(), content: "work".into() }]);
        let directive = log.accept(&director, &input).unwrap().interaction;
        assert_eq!(log.get(directive.id).unwrap(), Some(directive.clone()));
        assert!(!log.result_owed(InteractionId::new(), "bob").unwrap());
        assert!(!log.result_owed(ordinary.id, "bob").unwrap());
        assert!(!log.result_owed(directive.id, "carol").unwrap());
        assert!(log.result_owed(directive.id, "bob").unwrap());

        let mut result = message("alice");
        result.kind = InteractionKind::Result;
        result.turn_outcome = Some(TurnOutcome::Failed);
        result.body = Some(Value::Null);
        assert!(matches!(log.accept(&worker, &result), Err(LogError::NotOwed)));
        result.caused_by = Some(ordinary.id);
        assert!(matches!(log.accept(&worker, &result), Err(LogError::NotOwed)));
        result.caused_by = Some(directive.id);
        assert!(matches!(log.accept(&Sender::user("carol"), &result), Err(LogError::NotOwed)));
        assert!(matches!(log.accept(&Sender::service("bob", "timer"), &result), Err(LogError::NotOwed)));
        result.to = "carol".into();
        assert!(matches!(log.accept(&worker, &result), Err(LogError::NotOwed)));
        assert!(log.result_owed(directive.id, "bob").unwrap());
        assert!(log.lookup(&worker, result.client_ref).unwrap().is_none());

        result.to = "alice".into();
        let answer = log.accept(&worker, &result).unwrap().interaction;
        assert_eq!((answer.root, answer.hops), (directive.id, 1));
        assert_eq!(log.get(answer.id).unwrap(), Some(answer.clone()));
        assert!(!log.result_owed(directive.id, "bob").unwrap());
        assert!(log.accept(&worker, &result).unwrap().duplicate);
        log.expire(0, answer.accepted_at).unwrap();
        assert_state(&log, answer.id, InteractionState::Dead);
        assert!(!log.result_owed(directive.id, "bob").unwrap());
        result.client_ref = ActionId::new();
        assert!(matches!(log.accept(&worker, &result), Err(LogError::NotOwed)));
    }

    #[test]
    fn file_reopens_in_wal_mode_and_recovers_only_delivered_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broker.db");
        let from = Sender::user("alice");
        let (first, second, acked) = {
            let mut log = InteractionLog::open(&path).unwrap();
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(
                log.conn.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0)).unwrap(),
                "wal"
            );
            let first = log.accept(&from, &message("bob")).unwrap().interaction;
            let second = log.accept(&from, &message("carol")).unwrap().interaction;
            let acked = log.accept(&from, &message("bob")).unwrap().interaction;
            for id in [first.id, second.id, acked.id] {
                log.mark_delivered(id).unwrap();
            }
            log.ack(acked.id).unwrap();
            (first, second, acked)
        };

        let log = InteractionLog::open(&path).unwrap();
        assert_state(&log, first.id, InteractionState::Delivered);
        assert_state(&log, second.id, InteractionState::Delivered);
        assert_eq!(log.requeue_all_delivered().unwrap(), 2);
        assert_eq!(log.requeue_all_delivered().unwrap(), 0);
        assert_eq!(log.lookup(&from, first.client_ref).unwrap(), Some(first.clone()));
        assert_eq!(log.next_deliverable("bob").unwrap(), Some(first));
        assert_eq!(log.next_deliverable("carol").unwrap(), Some(second));
        assert_state(&log, acked.id, InteractionState::Acked);
    }
}

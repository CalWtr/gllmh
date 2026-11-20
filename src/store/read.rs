use std::{error::Error as StdError, path::Path, str::FromStr};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Params, Row, types::Type};
use serde_json::Value;
use ulid::Ulid;

use crate::types::{Action, InteractionId, Mark, Turn};

use super::Result;

/// The composer's read-only connection to the section-5 views.
pub struct ReadOnlyStore {
    conn: Connection,
}

impl ReadOnlyStore {
    /// Open the database file named in the composer's `init.store`.
    pub fn open(db_path: &Path) -> Result<Self> {
        Ok(Self { conn: Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)? })
    }

    /// All actions, oldest first.
    pub fn actions(&self) -> Result<Vec<Action>> {
        Ok(read_all(&self.conn, "SELECT * FROM v1_action ORDER BY id", [], action)?)
    }

    /// Actions for a turn, oldest first.
    pub fn actions_of_turn(&self, trigger: InteractionId) -> Result<Vec<Action>> {
        Ok(read_all(&self.conn, "SELECT * FROM v1_action WHERE turn = ?1 ORDER BY id", [trigger.to_string()], action)?)
    }

    /// A turn by its trigger interaction.
    pub fn turn_by_trigger(&self, trigger: InteractionId) -> Result<Option<Turn>> {
        Ok(self.conn.query_row("SELECT * FROM v1_turn WHERE trigger = ?1", [trigger.to_string()], turn).optional()?)
    }

    /// All current marks, ordered by target and key.
    pub fn marks(&self) -> Result<Vec<Mark>> {
        Ok(read_all(&self.conn, "SELECT * FROM v1_mark ORDER BY target, key", [], mark)?)
    }

    /// The original reasoning JSON text of a successful model call, if present.
    /// Use this when replay requires the original bytes (section 3).
    pub fn action_reasoning_json(&self, id: Ulid) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT reasoning FROM v1_action WHERE id = ?1", [id.to_string()], |row| row.get(0))
            .optional()?
            .flatten())
    }

    #[cfg(test)]
    pub(super) fn connection(&self) -> &Connection {
        &self.conn
    }
}

pub(super) fn read_all<T>(
    conn: &Connection,
    sql: &str,
    params: impl Params,
    decode: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    conn.prepare_cached(sql)?.query_map(params, decode)?.collect()
}

pub(super) fn action(row: &Row<'_>) -> rusqlite::Result<Action> {
    Ok(Action {
        id: text(row, "id")?,
        kind: text(row, "kind")?,
        turn: optional_text(row, "turn")?,
        step: row.get("step")?,
        requested_by: optional_text(row, "requested_by")?,
        call_id: row.get("call_id")?,
        service: row.get("service")?,
        interaction: optional_text(row, "interaction")?,
        root: optional_text(row, "root")?,
        input: text(row, "input")?,
        output: optional_text(row, "output")?,
        hazardness: optional_text(row, "hazardness")?,
        reasoning: optional_text(row, "reasoning")?,
        state: text(row, "state")?,
        requested_at: text(row, "requested_at")?,
        ended_at: optional_text(row, "ended_at")?,
    })
}

pub(super) fn turn(row: &Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        trigger: text(row, "trigger")?,
        root: text(row, "root")?,
        state: text(row, "state")?,
        steps: row.get("steps")?,
        opened_at: text(row, "opened_at")?,
        ended_at: optional_text(row, "ended_at")?,
    })
}

fn mark(row: &Row<'_>) -> rusqlite::Result<Mark> {
    Ok(Mark {
        target: text(row, "target")?,
        key: row.get("key")?,
        value: text::<Value>(row, "value")?,
        set_by: text(row, "set_by")?,
        set_at: text(row, "set_at")?,
    })
}

pub(super) fn text<T: FromStr>(row: &Row<'_>, column: &str) -> rusqlite::Result<T>
where
    T::Err: StdError + Send + Sync + 'static,
{
    let index = row.as_ref().column_index(column)?;
    parse(&row.get::<_, String>(index)?, index)
}

fn optional_text<T: FromStr>(row: &Row<'_>, column: &str) -> rusqlite::Result<Option<T>>
where
    T::Err: StdError + Send + Sync + 'static,
{
    let index = row.as_ref().column_index(column)?;
    row.get::<_, Option<String>>(index)?.map(|s| parse(&s, index)).transpose()
}

fn parse<T: FromStr>(s: &str, index: usize) -> rusqlite::Result<T>
where
    T::Err: StdError + Send + Sync + 'static,
{
    s.parse().map_err(|err| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(err)))
}

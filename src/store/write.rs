use std::{fs::DirBuilder, os::unix::fs::DirBuilderExt, path::Path};

use chrono::SecondsFormat;
use rusqlite::{Connection, OptionalExtension, named_params, params};
use serde_json::Value;
use ulid::{Generator, MonotonicError, Ulid};

use crate::{
    paths,
    protocol::broker::SendOutput,
    types::{
        Action, ActionId, ActionKind, ActionState, Hazardness, Interaction, InteractionId, Turn, TurnOutcome,
        TurnState, now,
    },
};

use super::{
    Error, Result,
    read::{action, read_all, text, turn},
};

/// The initial fields of a write-ahead action.
#[derive(Debug, Clone)]
pub struct ActionRequest {
    pub kind: ActionKind,
    pub turn: Option<InteractionId>,
    pub step: Option<u32>,
    pub requested_by: Option<ActionId>,
    pub call_id: Option<String>,
    pub service: Option<String>,
    pub interaction: Option<InteractionId>,
    pub root: Option<InteractionId>,
    pub input: Value,
    pub hazardness: Option<Hazardness>,
}

impl ActionRequest {
    /// Create a request with its optional fields unset.
    pub fn new(kind: ActionKind, input: Value) -> Self {
        Self {
            kind,
            turn: None,
            step: None,
            requested_by: None,
            call_id: None,
            service: None,
            interaction: None,
            root: None,
            input,
            hazardness: None,
        }
    }
}

/// Result of recording a broker delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveOutcome {
    Recorded { action_id: ActionId },
    Duplicate,
}

/// A running turn and the tool-call evidence used during recovery.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningTurn {
    pub turn: Turn,
    pub has_unfinished_tool_call: bool,
    /// EXTENSION: true if recovery already interrupted a tool call for this turn.
    /// This evidence survives a crash between interrupting the action and ending the turn.
    pub has_interrupted_tool_call: bool,
}

/// The valet's synchronous writer connection. Open one writer per user store.
pub struct Store {
    conn: Connection,
    ids: Generator,
    last_id: Option<ActionId>,
}

impl Store {
    /// Open or create `<user_data_store>/store.db` in WAL mode.
    pub fn open(user_data_store: &Path) -> Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(user_data_store).map_err(Error::CreateDir)?;
        let conn = Connection::open(paths::store_db(user_data_store))?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if mode != "wal" {
            return Err(Error::JournalMode(mode));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(include_str!("schema.sql"))?;
        let last_id =
            conn.query_row("SELECT id FROM action ORDER BY id DESC LIMIT 1", [], |row| text(row, "id")).optional()?;
        Ok(Self { conn, ids: Generator::new(), last_id })
    }

    fn next_id(&mut self) -> Result<ActionId> {
        let mut id = self.ids.generate()?;
        // The generator tracks order in memory. Carry the order across writer restarts too.
        if let Some(last) = self.last_id.filter(|last| *last >= id) {
            id = last.increment().ok_or(MonotonicError::Overflow)?;
        }
        self.last_id = Some(id);
        Ok(id)
    }

    /// Commit the write-ahead row before running the action. Use
    /// [`Self::record_receive`] for broker deliveries.
    pub fn request_action(&mut self, request: ActionRequest) -> Result<ActionId> {
        self.request_action_with(request, |_| None)
    }

    /// Like [`Self::request_action`], but `input` may build the stored input
    /// from the new action's id (a send's `client_ref` is its own action id).
    pub fn request_action_with(
        &mut self,
        mut request: ActionRequest,
        input: impl FnOnce(ActionId) -> Option<Value>,
    ) -> Result<ActionId> {
        let id = self.next_id()?;
        if let Some(value) = input(id) {
            request.input = value;
        }
        self.conn.execute(
            "INSERT INTO action (id, kind, turn, step, requested_by, call_id, service, interaction, root, input,
                                 hazardness, state, requested_at)
             VALUES (:id, :kind, :turn, :step, :requested_by, :call_id, :service, :interaction, :root, :input,
                     :hazardness, 'requested', :requested_at)",
            named_params! {
                ":id": id.to_string(),
                ":kind": request.kind.as_str(),
                ":turn": request.turn.map(|id| id.to_string()),
                ":step": request.step,
                ":requested_by": request.requested_by.map(|id| id.to_string()),
                ":call_id": request.call_id,
                ":service": request.service,
                ":interaction": request.interaction.map(|id| id.to_string()),
                ":root": request.root.map(|id| id.to_string()),
                ":input": serde_json::to_string(&request.input)?,
                ":hazardness": request.hazardness.map(Hazardness::as_str),
                ":requested_at": timestamp(),
            },
        )?;
        Ok(id)
    }

    /// Move a requested action to running.
    pub fn start_action(&self, id: ActionId) -> Result<()> {
        self.change_action(
            id,
            ActionState::Running,
            "UPDATE action SET state = 'running' WHERE id = ?1 AND state = 'requested'",
            [id.to_string()],
        )
    }

    /// Complete a running non-send action. Reasoning, if present, must be JSON
    /// text from an llm_call. It is stored byte for byte for provider replay.
    pub fn succeed_action(&self, id: ActionId, output: &Value, reasoning_json: Option<&str>) -> Result<()> {
        if let Some(json) = reasoning_json {
            serde_json::from_str::<Value>(json)?;
        }
        self.change_action(
            id,
            ActionState::Succeeded,
            "UPDATE action SET state = 'succeeded', output = ?2, reasoning = ?3, ended_at = ?4
             WHERE id = ?1 AND state = 'running' AND kind != 'send' AND (?3 IS NULL OR kind = 'llm_call')",
            params![id.to_string(), serde_json::to_string(output)?, reasoning_json, timestamp()],
        )
    }

    /// Complete a running send with the broker's acceptance reply.
    pub fn succeed_send(&self, id: ActionId, reply: &SendOutput) -> Result<()> {
        self.write_send_success(id, reply, false)
    }

    /// Complete an unfinished send with the broker's acceptance found by lookup
    /// during recovery (section 5, step 3). The send is never executed here.
    pub fn recover_send(&self, id: ActionId, reply: &SendOutput) -> Result<()> {
        self.write_send_success(id, reply, true)
    }

    fn write_send_success(&self, id: ActionId, reply: &SendOutput, recovery: bool) -> Result<()> {
        self.change_action(
            id,
            ActionState::Succeeded,
            "UPDATE action
             SET state = 'succeeded', output = ?2, interaction = ?3, root = coalesce(root, ?4), ended_at = ?5
             WHERE id = ?1 AND kind = 'send' AND (state = 'running' OR (?6 AND state = 'requested'))",
            params![
                id.to_string(),
                serde_json::to_string(reply)?,
                reply.id.to_string(),
                reply.root.to_string(),
                timestamp(),
                recovery,
            ],
        )
    }

    /// Fail a running action and record the error as its output.
    pub fn fail_action(&self, id: ActionId, output: &Value) -> Result<()> {
        self.change_action(
            id,
            ActionState::Failed,
            "UPDATE action SET state = 'failed', output = ?2, ended_at = ?3 WHERE id = ?1 AND state = 'running'",
            params![id.to_string(), serde_json::to_string(output)?, timestamp()],
        )
    }

    /// Deny a requested action before it runs.
    pub fn deny_action(&self, id: ActionId) -> Result<()> {
        self.change_action(
            id,
            ActionState::Denied,
            "UPDATE action SET state = 'denied', ended_at = ?2 WHERE id = ?1 AND state = 'requested'",
            params![id.to_string(), timestamp()],
        )
    }

    /// Interrupt a requested or running action during recovery.
    pub fn interrupt_action(&self, id: ActionId) -> Result<()> {
        self.change_action(
            id,
            ActionState::Interrupted,
            "UPDATE action SET state = 'interrupted', ended_at = ?2
             WHERE id = ?1 AND state IN ('requested', 'running')",
            params![id.to_string(), timestamp()],
        )
    }

    fn change_action(&self, id: ActionId, to: ActionState, sql: &str, params: impl rusqlite::Params) -> Result<()> {
        if self.conn.execute(sql, params)? == 0 {
            return Err(Error::ActionTransition { id, to });
        }
        Ok(())
    }

    /// Record a succeeded receive and optionally open its queued turn in the
    /// same transaction. The valet can acknowledge delivery after this returns.
    pub fn record_receive(&mut self, interaction: &Interaction, open_turn: bool) -> Result<ReceiveOutcome> {
        let id = self.next_id()?;
        let trigger = interaction.id.to_string();
        let root = interaction.root.to_string();
        let at = timestamp();
        let tx = self.conn.transaction()?;
        let inserted = tx.execute(
            "INSERT INTO action (id, kind, turn, interaction, root, input, state, requested_at, ended_at)
             VALUES (?1, 'receive', ?2, ?3, ?4, ?5, 'succeeded', ?6, ?6)
             ON CONFLICT(interaction) WHERE kind = 'receive' DO NOTHING",
            params![
                id.to_string(),
                open_turn.then_some(&trigger),
                trigger,
                root,
                serde_json::to_string(interaction)?,
                at,
            ],
        )?;
        if inserted == 0 {
            return Ok(ReceiveOutcome::Duplicate);
        }
        if open_turn {
            tx.execute(
                "INSERT INTO turn (trigger, root, state, steps, opened_at) VALUES (?1, ?2, 'queued', 0, ?3)",
                params![trigger, root, at],
            )?;
        }
        tx.commit()?;
        Ok(ReceiveOutcome::Recorded { action_id: id })
    }

    /// Start a queued turn at zero model calls. The valet schedules turns in
    /// queue order and enforces its parallel-turn limit.
    pub fn start_turn(&self, trigger: InteractionId) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE turn SET state = 'running', steps = 0 WHERE trigger = ?1 AND state = 'queued'",
            [trigger.to_string()],
        )?;
        if changed == 0 {
            return Err(Error::TurnTransition { trigger, to: TurnState::Running });
        }
        Ok(())
    }

    /// Count a model call for a running turn. Returns the new step number;
    /// the first model call is step 1. A call beyond the valet's limit is refused.
    pub fn increment_turn_step(&self, trigger: InteractionId, max_steps: u32) -> Result<u32> {
        let mut stmt = self.conn.prepare_cached(
            "UPDATE turn SET steps = steps + 1 WHERE trigger = ?1 AND state = 'running' AND steps < ?2
             RETURNING steps",
        )?;
        let mut rows = stmt.query(params![trigger.to_string(), max_steps])?;
        let step = rows.next()?.ok_or(Error::TurnStep { trigger, limit: max_steps })?.get(0)?;
        // Step the statement to completion so a commit failure is returned to the caller.
        rows.next()?;
        Ok(step)
    }

    /// End a running turn. The valet sends any owed result and closes the
    /// interaction. Resuming a running turn needs no state update.
    pub fn end_turn(&self, trigger: InteractionId, outcome: TurnOutcome) -> Result<()> {
        Self::write_turn_end(&self.conn, trigger, outcome)
    }

    /// EXTENSION: checkpoint a directive's result text atomically with the end state.
    pub fn end_directive(&mut self, trigger: InteractionId, outcome: TurnOutcome, text: Option<&str>) -> Result<()> {
        let tx = self.conn.transaction()?;
        Self::write_turn_end(&tx, trigger, outcome)?;
        tx.execute(
            "INSERT INTO directive_end (trigger, result_text) VALUES (?1, ?2)",
            params![trigger.to_string(), text],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn write_turn_end(conn: &Connection, trigger: InteractionId, outcome: TurnOutcome) -> Result<()> {
        let to = match outcome {
            TurnOutcome::Done => TurnState::Done,
            TurnOutcome::CutOff => TurnState::CutOff,
            TurnOutcome::Failed => TurnState::Failed,
            TurnOutcome::Interrupted => TurnState::Interrupted,
        };
        let changed = conn.execute(
            "UPDATE turn SET state = ?2, ended_at = ?3 WHERE trigger = ?1 AND state = 'running'",
            params![trigger.to_string(), to.as_str(), timestamp()],
        )?;
        if changed == 0 {
            return Err(Error::TurnTransition { trigger, to });
        }
        Ok(())
    }

    /// EXTENSION: recover ended directives until the broker has confirmed close.
    /// A missing checkpoint is possible in a store written by an older valet.
    pub fn unclosed_directives(&self) -> Result<Vec<(Interaction, Turn, Option<String>)>> {
        let rows = read_all(
            &self.conn,
            "SELECT t.*, a.input, d.result_text
             FROM turn t
             JOIN action a ON a.interaction = t.trigger AND a.kind = 'receive'
             LEFT JOIN directive_end d ON d.trigger = t.trigger
             WHERE t.ended_at IS NOT NULL AND coalesce(d.closed, 0) = 0
               AND json_extract(a.input, '$.kind') = 'directive'
             ORDER BY t.rowid",
            [],
            |row| Ok((row.get::<_, String>("input")?, turn(row)?, row.get::<_, Option<String>>("result_text")?)),
        )?;
        rows.into_iter()
            .map(|(input, turn, result_text)| Ok((serde_json::from_str(&input)?, turn, result_text)))
            .collect()
    }

    /// EXTENSION: checkpoint the broker's idempotent close.
    pub fn confirm_directive_close(&self, trigger: InteractionId) -> Result<()> {
        self.conn.execute(
            "INSERT INTO directive_end (trigger, closed) VALUES (?1, 1)
             ON CONFLICT(trigger) DO UPDATE SET closed = 1",
            [trigger.to_string()],
        )?;
        Ok(())
    }

    pub fn has_sent_result(&self, trigger: InteractionId) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM action
                 WHERE turn = ?1 AND kind = 'send' AND state = 'succeeded'
                   AND json_extract(input, '$.kind') = 'result' AND json_extract(input, '$.caused_by') = ?1
             )",
            [trigger.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Replace a mark by target and key, or remove it when the value is JSON null.
    pub fn set_mark(&self, target: Ulid, key: &str, value: &Value, set_by: InteractionId) -> Result<()> {
        if value.is_null() {
            self.conn.execute("DELETE FROM mark WHERE target = ?1 AND key = ?2", params![target.to_string(), key])?;
        } else {
            self.conn.execute(
                "INSERT INTO mark (target, key, value, set_by, set_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(target, key) DO UPDATE
                 SET value = excluded.value, set_by = excluded.set_by, set_at = excluded.set_at",
                params![target.to_string(), key, serde_json::to_string(value)?, set_by.to_string(), timestamp()],
            )?;
        }
        Ok(())
    }

    pub fn get_action(&self, id: ActionId) -> Result<Option<Action>> {
        Ok(self.conn.query_row("SELECT * FROM action WHERE id = ?1", [id.to_string()], action).optional()?)
    }

    /// Actions for a turn, oldest first.
    pub fn actions_of_turn(&self, trigger: InteractionId) -> Result<Vec<Action>> {
        Ok(read_all(&self.conn, "SELECT * FROM action WHERE turn = ?1 ORDER BY id", [trigger.to_string()], action)?)
    }

    pub fn get_turn(&self, trigger: InteractionId) -> Result<Option<Turn>> {
        Ok(self.conn.query_row("SELECT * FROM turn WHERE trigger = ?1", [trigger.to_string()], turn).optional()?)
    }

    pub fn receive_action(&self, interaction: InteractionId) -> Result<Option<Action>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM action WHERE kind = 'receive' AND interaction = ?1",
                [interaction.to_string()],
                action,
            )
            .optional()?)
    }

    /// Requested and running actions, oldest first.
    pub fn unfinished_actions(&self) -> Result<Vec<Action>> {
        Ok(read_all(
            &self.conn,
            "SELECT * FROM action WHERE state IN ('requested', 'running') ORDER BY id",
            [],
            action,
        )?)
    }

    /// Requested and running sends, oldest first. The action id is the
    /// client_ref to use for the broker lookup.
    pub fn unfinished_sends(&self) -> Result<Vec<Action>> {
        Ok(read_all(
            &self.conn,
            "SELECT * FROM action WHERE kind = 'send' AND state IN ('requested', 'running') ORDER BY id",
            [],
            action,
        )?)
    }

    /// Running turns in opening order. During recovery, either tool-call
    /// flag means the turn must end as interrupted. Read after interrupting
    /// unfinished actions, or use the unfinished flag when planning recovery.
    pub fn running_turns(&self) -> Result<Vec<RunningTurn>> {
        Ok(read_all(
            &self.conn,
            "SELECT t.*,
                    EXISTS (SELECT 1 FROM action a WHERE a.turn = t.trigger AND a.kind = 'tool_call'
                            AND a.state IN ('requested', 'running')) AS has_unfinished_tool_call,
                    EXISTS (SELECT 1 FROM action a WHERE a.turn = t.trigger AND a.kind = 'tool_call'
                            AND a.state = 'interrupted') AS has_interrupted_tool_call
             FROM turn t WHERE t.state = 'running' ORDER BY t.rowid",
            [],
            |row| {
                Ok(RunningTurn {
                    turn: turn(row)?,
                    has_unfinished_tool_call: row.get("has_unfinished_tool_call")?,
                    has_interrupted_tool_call: row.get("has_interrupted_tool_call")?,
                })
            },
        )?)
    }

    /// Queued turns in local receive order, regardless of broker ULID or clock order.
    pub fn queued_turns(&self) -> Result<Vec<Turn>> {
        Ok(read_all(&self.conn, "SELECT * FROM turn WHERE state = 'queued' ORDER BY rowid", [], turn)?)
    }

    #[cfg(test)]
    pub(super) fn connection(&self) -> &Connection {
        &self.conn
    }
}

fn timestamp() -> String {
    now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

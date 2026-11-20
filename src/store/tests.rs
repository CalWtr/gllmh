use std::{os::unix::fs::PermissionsExt, path::PathBuf, str::FromStr};

use rusqlite::ErrorCode;
use serde_json::{Value, json};
use strum::VariantArray;
use tempfile::TempDir;
use ulid::Ulid;

use crate::{
    paths::store_db,
    protocol::broker::SendOutput,
    types::{
        ActionId, ActionKind, ActionState, Hazardness, Interaction, InteractionKind, InteractionState, Sender,
        Timestamp, TurnOutcome, TurnState, now,
    },
};

use super::*;

fn fixture() -> (TempDir, PathBuf, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("user").join("store");
    let store = Store::open(&path).unwrap();
    (tmp, path, store)
}

fn interaction() -> Interaction {
    Interaction {
        id: Ulid::new(),
        kind: InteractionKind::Message,
        client_ref: Ulid::new(),
        from: Sender::user("sender"),
        to: "agent".to_owned(),
        caused_by: Some(Ulid::new()),
        root: Ulid::new(),
        hops: 1,
        body: Some(json!({"text": "hello"})),
        context: None,
        turn_outcome: None,
        state: InteractionState::Delivered,
        outcome: None,
        accepted_at: now(),
        closed_at: None,
    }
}

fn request(store: &mut Store, kind: ActionKind) -> ActionId {
    store.request_action(ActionRequest::new(kind, Value::Null)).unwrap()
}

fn transition(store: &Store, id: ActionId, to: ActionState) -> Result<()> {
    match to {
        ActionState::Running => store.start_action(id),
        ActionState::Succeeded => store.succeed_action(id, &json!({ "result": 42 }), None),
        ActionState::Failed => store.fail_action(id, &json!("error")),
        ActionState::Denied => store.deny_action(id),
        ActionState::Interrupted => store.interrupt_action(id),
        ActionState::Requested => unreachable!("requested is the initial state"),
    }
}

#[test]
fn action_transition_matrix() {
    use ActionState::*;
    let (_tmp, _, mut store) = fixture();
    for &from in ActionState::VARIANTS {
        for &to in ActionState::VARIANTS.iter().filter(|&&state| state != Requested) {
            let id = request(&mut store, ActionKind::ToolCall);
            let initial = store.get_action(id).unwrap().unwrap();
            assert_eq!(initial.state, Requested);
            assert_eq!(initial.input, Value::Null);
            assert_eq!(initial.ended_at, None);

            if matches!(from, Succeeded | Failed) {
                store.start_action(id).unwrap();
            }
            if from != Requested {
                transition(&store, id, from).unwrap();
            }
            let before = store.get_action(id).unwrap().unwrap();

            match (from, to) {
                (Requested, Running | Denied | Interrupted) | (Running, Succeeded | Failed | Interrupted) => {
                    transition(&store, id, to).unwrap();
                    let after = store.get_action(id).unwrap().unwrap();
                    assert_eq!(after.state, to);
                    assert_eq!(after.requested_at, initial.requested_at);
                    assert_eq!(after.ended_at.is_none(), to == Running);
                    match to {
                        Succeeded => assert_eq!(after.output, Some(json!({"result": 42}))),
                        Failed => assert_eq!(after.output, Some(json!("error"))),
                        _ => assert_eq!(after.output, None),
                    }
                }
                _ => {
                    assert!(
                        matches!(transition(&store, id, to), Err(Error::ActionTransition { .. })),
                        "{from} -> {to}"
                    );
                    assert_eq!(store.get_action(id).unwrap(), Some(before));
                }
            }
        }
    }

    assert!(matches!(store.start_action(Ulid::nil()), Err(Error::ActionTransition { .. })));
}

#[test]
fn send_success_and_lookup_settle_the_right_actions() {
    let (_tmp, _, mut store) = fixture();
    let reply = SendOutput { id: Ulid::new(), root: Ulid::new(), hops: 1 };
    let id = request(&mut store, ActionKind::Send);

    assert!(matches!(store.succeed_send(id, &reply), Err(Error::ActionTransition { .. })));
    store.start_action(id).unwrap();
    assert!(matches!(store.succeed_action(id, &Value::Null, None), Err(Error::ActionTransition { .. })));
    store.succeed_send(id, &reply).unwrap();
    let send = store.get_action(id).unwrap().unwrap();
    assert_eq!(send.state, ActionState::Succeeded);
    assert_eq!(send.interaction, Some(reply.id));
    assert_eq!(send.root, Some(reply.root));
    assert_eq!(send.output, Some(serde_json::to_value(reply).unwrap()));
    assert!(send.ended_at.is_some());
    assert!(store.receive_action(reply.id).unwrap().is_none());
    assert!(matches!(store.recover_send(id, &reply), Err(Error::ActionTransition { .. })));

    for start in [false, true] {
        let id = request(&mut store, ActionKind::Send);
        if start {
            store.start_action(id).unwrap();
        }
        store.recover_send(id, &reply).unwrap();
        assert_eq!(store.get_action(id).unwrap().unwrap().interaction, Some(reply.id));
    }
    let llm = request(&mut store, ActionKind::LlmCall);
    assert!(matches!(store.recover_send(llm, &reply), Err(Error::ActionTransition { .. })));

    // The receive index is partial: multiple sends above can name the same interaction.
    let received = Interaction { id: reply.id, root: reply.root, ..interaction() };
    assert!(matches!(store.record_receive(&received, false).unwrap(), ReceiveOutcome::Recorded { .. }));
}

#[test]
fn turn_transitions_and_step_limit() {
    let (_tmp, _, mut store) = fixture();
    for &outcome in TurnOutcome::VARIANTS {
        let incoming = interaction();
        let id = incoming.id;
        store.record_receive(&incoming, true).unwrap();
        let queued = store.get_turn(id).unwrap().unwrap();
        assert_eq!(queued.state, TurnState::Queued);
        assert_eq!(queued.steps, 0);
        assert_eq!(queued.ended_at, None);
        assert!(matches!(store.end_turn(id, outcome), Err(Error::TurnTransition { .. })));
        assert!(matches!(store.increment_turn_step(id, 2), Err(Error::TurnStep { .. })));
        assert_eq!(store.get_turn(id).unwrap(), Some(queued.clone()));

        store.start_turn(id).unwrap();
        let running = store.get_turn(id).unwrap().unwrap();
        assert_eq!(running.state, TurnState::Running);
        assert_eq!(running.steps, 0);
        assert_eq!(running.ended_at, None);
        assert!(matches!(store.start_turn(id), Err(Error::TurnTransition { .. })));
        assert!(matches!(store.increment_turn_step(id, 0), Err(Error::TurnStep { .. })));
        assert_eq!(store.increment_turn_step(id, 2).unwrap(), 1);
        assert_eq!(store.increment_turn_step(id, 2).unwrap(), 2);
        assert!(matches!(store.increment_turn_step(id, 2), Err(Error::TurnStep { .. })));

        store.end_turn(id, outcome).unwrap();
        let ended = store.get_turn(id).unwrap().unwrap();
        assert_eq!(ended.state.turn_outcome(), Some(outcome));
        assert_eq!(ended.steps, 2);
        assert_eq!(ended.opened_at, queued.opened_at);
        assert!(ended.ended_at.is_some());
        assert!(matches!(store.start_turn(id), Err(Error::TurnTransition { .. })));
        assert!(matches!(store.increment_turn_step(id, 3), Err(Error::TurnStep { .. })));
        for &other_outcome in TurnOutcome::VARIANTS {
            assert!(matches!(store.end_turn(id, other_outcome), Err(Error::TurnTransition { .. })));
        }
        assert_eq!(store.get_turn(id).unwrap(), Some(ended));
    }
}

#[test]
fn receives_drop_duplicates_and_open_turns_atomically() {
    let (_tmp, path, mut store) = fixture();
    let reader = ReadOnlyStore::open(&store_db(&path)).unwrap();
    let incoming = interaction();
    store
        .connection()
        .execute_batch(
            "CREATE TEMP TRIGGER reject_turn BEFORE INSERT ON turn
         BEGIN SELECT RAISE(ABORT, 'injected turn insert failure'); END;",
        )
        .unwrap();
    assert!(matches!(store.record_receive(&incoming, true), Err(Error::Sqlite(_))));
    assert!(store.get_turn(incoming.id).unwrap().is_none());
    assert!(store.receive_action(incoming.id).unwrap().is_none());
    assert!(reader.actions().unwrap().is_empty());
    assert!(reader.turn_by_trigger(incoming.id).unwrap().is_none());

    store.connection().execute_batch("DROP TRIGGER reject_turn").unwrap();
    let ReceiveOutcome::Recorded { action_id } = store.record_receive(&incoming, true).unwrap() else {
        panic!("a rolled-back receive must be recordable");
    };
    let receive = store.receive_action(incoming.id).unwrap().unwrap();
    let turn = reader.turn_by_trigger(incoming.id).unwrap().unwrap();
    assert_eq!(turn.trigger, incoming.id);
    assert_eq!(turn.root, incoming.root);
    assert_eq!(turn.state, TurnState::Queued);
    assert_eq!(receive.id, action_id);
    assert_eq!(receive.turn, Some(turn.trigger));
    assert_eq!(receive.root, Some(turn.root));
    assert_eq!(receive.kind, ActionKind::Receive);
    assert_eq!(receive.state, ActionState::Succeeded);
    assert_eq!(receive.input, serde_json::to_value(&incoming).unwrap());
    assert_eq!(receive.ended_at, Some(receive.requested_at));
    assert_eq!(reader.actions_of_turn(turn.trigger).unwrap(), vec![receive.clone()]);
    assert_eq!(store.actions_of_turn(turn.trigger).unwrap(), vec![receive.clone()]);

    let redelivery = Interaction { body: Some(json!("different")), ..incoming };
    assert_eq!(store.record_receive(&redelivery, true).unwrap(), ReceiveOutcome::Duplicate);
    assert_eq!(reader.actions().unwrap(), vec![receive]);
    assert_eq!(reader.turn_by_trigger(redelivery.id).unwrap(), Some(turn));

    let human_receive = interaction();
    store.record_receive(&human_receive, false).unwrap();
    assert_eq!(store.record_receive(&human_receive, true).unwrap(), ReceiveOutcome::Duplicate);
    assert!(store.get_turn(human_receive.id).unwrap().is_none());
    assert_eq!(store.receive_action(human_receive.id).unwrap().unwrap().turn, None);
    assert_eq!(reader.actions().unwrap().len(), 2);
}

#[test]
fn marks_replace_and_remove_by_target_and_key() {
    let (_tmp, path, store) = fixture();
    let reader = ReadOnlyStore::open(&store_db(&path)).unwrap();
    let target = Ulid::new();
    let other = Ulid::new();
    let setter = Ulid::new();
    store.set_mark(target, "session", &json!(["v1"]), setter).unwrap();
    let first = reader.marks().unwrap().pop().unwrap();
    assert_eq!(first.target, target);
    assert_eq!(first.key, "session");
    assert_eq!(first.value, json!(["v1"]));
    assert_eq!(first.set_by, setter);

    store.set_mark(target, "session", &json!(false), other).unwrap();
    store.set_mark(target, "other", &json!(1), setter).unwrap();
    store.set_mark(other, "session", &json!("separate"), setter).unwrap();
    let marks = reader.marks().unwrap();
    assert_eq!(marks.len(), 3);
    let updated = marks.iter().find(|mark| mark.target == target && mark.key == "session").unwrap();
    assert_eq!(updated.value, json!(false));
    assert_eq!(updated.set_by, other);

    store.set_mark(target, "session", &Value::Null, other).unwrap();
    store.set_mark(target, "session", &Value::Null, other).unwrap();
    let marks = reader.marks().unwrap();
    assert_eq!(marks.len(), 2);
    assert!(marks.iter().all(|mark| mark.target != target || mark.key != "session"));
}

#[test]
fn recovery_queries_filter_order_and_retain_interrupted_tool_evidence() {
    let (_tmp, path, mut store) = fixture();
    // Broker IDs are deliberately in reverse of the local receive order.
    let triggers = [4u128, 3, 2, 1].map(Ulid::from);
    for id in triggers {
        store.record_receive(&Interaction { id, ..interaction() }, true).unwrap();
    }
    for trigger in &triggers[..2] {
        store.start_turn(*trigger).unwrap();
    }
    assert_eq!(store.queued_turns().unwrap().iter().map(|turn| turn.trigger).collect::<Vec<_>>(), &triggers[2..]);

    let send_requested = request(&mut store, ActionKind::Send);
    let tool = store
        .request_action(ActionRequest {
            turn: Some(triggers[0]),
            ..ActionRequest::new(ActionKind::ToolCall, Value::Null)
        })
        .unwrap();
    store.start_action(tool).unwrap();
    let send_running = request(&mut store, ActionKind::Send);
    store.start_action(send_running).unwrap();
    let completed_send = request(&mut store, ActionKind::Send);
    store.interrupt_action(completed_send).unwrap();
    let llm = store
        .request_action(ActionRequest {
            turn: Some(triggers[1]),
            ..ActionRequest::new(ActionKind::LlmCall, Value::Null)
        })
        .unwrap();
    let completed_tool = store
        .request_action(ActionRequest {
            turn: Some(triggers[1]),
            ..ActionRequest::new(ActionKind::ToolCall, Value::Null)
        })
        .unwrap();
    store.deny_action(completed_tool).unwrap();

    let action_ids = |actions: Vec<crate::types::Action>| actions.iter().map(|action| action.id).collect::<Vec<_>>();
    assert_eq!(action_ids(store.unfinished_actions().unwrap()), [send_requested, tool, send_running, llm]);
    assert_eq!(action_ids(store.unfinished_sends().unwrap()), [send_requested, send_running]);

    let running = store.running_turns().unwrap();
    assert_eq!(running.iter().map(|row| row.turn.trigger).collect::<Vec<_>>(), &triggers[..2]);
    assert!(running[0].has_unfinished_tool_call);
    assert!(!running[0].has_interrupted_tool_call);
    assert!(!running[1].has_unfinished_tool_call);
    assert!(!running[1].has_interrupted_tool_call);

    store.interrupt_action(tool).unwrap();
    store.interrupt_action(llm).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    let running = store.running_turns().unwrap();
    assert!(!running[0].has_unfinished_tool_call);
    assert!(running[0].has_interrupted_tool_call);
    assert!(!running[1].has_unfinished_tool_call);
    assert!(!running[1].has_interrupted_tool_call);
    store.end_turn(triggers[0], TurnOutcome::Interrupted).unwrap();
    assert_eq!(store.running_turns().unwrap(), [running[1].clone()]);
}

#[test]
fn action_ids_sort_across_rapid_inserts_and_reopen_with_clock_rollback() {
    let (_tmp, path, mut store) = fixture();
    let mut ids: Vec<_> = (0..64).map(|_| request(&mut store, ActionKind::LlmCall)).collect();
    let ReceiveOutcome::Recorded { action_id } = store.record_receive(&interaction(), false).unwrap() else {
        panic!("expected a new receive");
    };
    ids.push(action_id);
    assert!(ids.is_sorted_by(|a, b| a < b));

    // Put the previously recorded ID ahead of the clock before reopening.
    let future_id = Ulid::from_parts(action_id.timestamp_ms() + 60_000, 123);
    store
        .connection()
        .execute("UPDATE action SET id = ?1 WHERE id = ?2", [future_id.to_string(), action_id.to_string()])
        .unwrap();
    *ids.last_mut().unwrap() = future_id;
    drop(store);
    let mut store = Store::open(&path).unwrap();
    ids.push(request(&mut store, ActionKind::ToolCall));
    assert!(ids.is_sorted_by(|a, b| a < b));

    let reader = ReadOnlyStore::open(&store_db(&path)).unwrap();
    assert_eq!(reader.actions().unwrap().iter().map(|action| action.id).collect::<Vec<_>>(), ids);
}

#[test]
fn composer_readers_match_the_v1_contract_and_cannot_write() {
    let (_tmp, path, mut store) = fixture();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(store.connection().query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0)).unwrap(), "wal");

    let incoming = interaction();
    store.record_receive(&incoming, true).unwrap();
    let llm = store
        .request_action(ActionRequest {
            turn: Some(incoming.id),
            step: Some(1),
            root: Some(incoming.root),
            ..ActionRequest::new(ActionKind::LlmCall, json!({"messages": []}))
        })
        .unwrap();
    store.start_action(llm).unwrap();
    let reasoning = r#" [ { "text": "keep\\u0020escapes" } ] "#;
    assert!(matches!(store.succeed_action(llm, &Value::Null, Some("{")), Err(Error::Json(_))));
    store.succeed_action(llm, &json!("ok"), Some(reasoning)).unwrap();

    let tool = store
        .request_action(ActionRequest {
            turn: Some(incoming.id),
            step: Some(1),
            requested_by: Some(llm),
            call_id: Some("call_1".to_owned()),
            root: Some(incoming.root),
            hazardness: Some(Hazardness::Caution),
            ..ActionRequest::new(ActionKind::ToolCall, json!({"arg": 42}))
        })
        .unwrap();
    store.start_action(tool).unwrap();
    assert!(matches!(store.succeed_action(tool, &Value::Null, Some(reasoning)), Err(Error::ActionTransition { .. })));
    store.succeed_action(tool, &Value::Null, None).unwrap();

    let reader = ReadOnlyStore::open(&store_db(&path)).unwrap();
    let conn = reader.connection();
    assert!(conn.is_readonly("main").unwrap());
    let err = conn.execute("DELETE FROM action", []).unwrap_err();
    assert_eq!(err.sqlite_error_code(), Some(ErrorCode::ReadOnly));
    for (view, expected) in [
        (
            "v1_action",
            "id kind turn step requested_by call_id service interaction root input output hazardness reasoning state requested_at ended_at",
        ),
        ("v1_turn", "trigger root state steps opened_at ended_at"),
        ("v1_mark", "target key value set_by set_at"),
    ] {
        let columns: Vec<String> =
            read::read_all(conn, "SELECT name FROM pragma_table_info(?1) ORDER BY cid", [view], |row| row.get(0))
                .unwrap();
        assert_eq!(columns, expected.split_whitespace().collect::<Vec<_>>());
    }

    let actions = reader.actions_of_turn(incoming.id).unwrap();
    assert_eq!(actions, store.actions_of_turn(incoming.id).unwrap());
    assert_eq!(actions[1].reasoning, Some(serde_json::from_str(reasoning).unwrap()));
    assert_eq!(reader.action_reasoning_json(llm).unwrap().as_deref(), Some(reasoning));
    assert_eq!(reader.action_reasoning_json(tool).unwrap(), None);
    assert_eq!(actions[2].requested_by, Some(llm));
    assert_eq!(actions[2].call_id.as_deref(), Some("call_1"));
    assert_eq!(actions[2].step, Some(1));
    assert_eq!(actions[2].hazardness, Some(Hazardness::Caution));
    assert_eq!(actions[2].output, Some(Value::Null));

    let (output, at): (String, String) = conn
        .query_row("SELECT output, requested_at FROM v1_action WHERE id = ?1", [tool.to_string()], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(output, "null");
    assert!(at.ends_with('Z'));
    assert_eq!(Timestamp::from_str(&at).unwrap(), actions[2].requested_at);
}

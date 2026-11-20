//! M3.2: a valet without a composer, end to end in dev mode.

mod support;

use std::time::Duration;

use gllmh::protocol::broker::{self, HelloInput, HelloOutput, SendInput, SendOutput};
use gllmh::protocol::local::{self, NotifyInput, StatusInput, StatusOutput, SubmitInput};
use gllmh::store::{ActionRequest, Store};
use gllmh::types::{ActionKind, ActionState, InteractionKind, InteractionState, Outcome};
use serde_json::json;
use support::dev::{Dev, eventually, eventually_async};

const T: Duration = Duration::from_secs(10);

fn submit_message(to: &str, text: &str) -> SubmitInput {
    SubmitInput {
        kind: InteractionKind::Message,
        to: to.into(),
        body: Some(json!(text)),
        context: None,
        caused_by: None,
    }
}

#[tokio::test]
async fn two_users_exchange_a_message() {
    let mut dev = Dev::new(support::fixture("pair"), "pair");
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    dev.start_valet("bob").await;

    let (_bob_ui, mut bob_notes) = dev.client("bob").await;
    let (alice_ui, _alice_notes) = dev.client("alice").await;

    let sent: SendOutput = alice_ui.request(local::SUBMIT, &submit_message("bob", "hi bob")).await.unwrap();
    assert_eq!(sent.hops, 0);
    assert_eq!(sent.root, sent.id);

    let note = tokio::time::timeout(T, bob_notes.next()).await.unwrap().unwrap();
    assert_eq!(note.op, local::NOTIFY);
    let (NotifyInput { interaction }, responder) = note.parse::<NotifyInput>().unwrap();
    responder.ok(json!({}));
    assert_eq!(interaction.id, sent.id);
    assert_eq!(interaction.from.to_string(), "alice");
    assert_eq!(interaction.body, Some(json!("hi bob")));

    // Both stores hold the rows: alice's send, bob's receive.
    let alice_actions = dev.store("alice").actions().unwrap();
    let send = alice_actions.iter().find(|a| a.kind == ActionKind::Send).expect("alice's send");
    assert_eq!(send.state, ActionState::Succeeded);
    assert_eq!(send.interaction, Some(sent.id));
    assert_eq!(send.input["client_ref"], json!(send.id.to_string()));
    let bob_actions = dev.store("bob").actions().unwrap();
    let recv = bob_actions.iter().find(|a| a.kind == ActionKind::Receive).expect("bob's receive");
    assert_eq!(recv.interaction, Some(sent.id));

    // The interaction is closed `received` (a user with no composer closes on record).
    let rows = eventually_async(T, || async {
        let out = alice_ui.request::<_, StatusOutput>(local::STATUS, &StatusInput { root: sent.root }).await;
        let row = out.ok()?.interactions.into_iter().next()?;
        (row.state == InteractionState::Closed).then_some(row)
    })
    .await;
    assert_eq!(rows.outcome, Some(Outcome::Received));
}

#[tokio::test]
async fn edges_are_checked_before_the_broker() {
    let mut dev = Dev::new(support::fixture("pair"), "pair");
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    let (ui, _n) = dev.client("alice").await;

    let err = ui.request::<_, SendOutput>(local::SUBMIT, &submit_message("carol", "hi")).await.unwrap_err();
    assert_eq!(err.code(), Some(broker::codes::NOT_ALLOWED));
    let mut directive = submit_message("bob", "do it");
    directive.kind = InteractionKind::Directive;
    let err = ui.request::<_, SendOutput>(local::SUBMIT, &directive).await.unwrap_err();
    assert_eq!(err.code(), Some(broker::codes::NOT_ALLOWED));
    let mut result = submit_message("bob", "x");
    result.kind = InteractionKind::Result;
    let err = ui.request::<_, SendOutput>(local::SUBMIT, &result).await.unwrap_err();
    assert_eq!(err.code(), Some(local::codes::BAD_KIND));

    // Refused sends are still recorded, as failed (write-ahead, invariant 6).
    let failed = dev.store("alice").actions().unwrap().into_iter().filter(|a| a.state == ActionState::Failed).count();
    assert_eq!(failed, 2);
}

#[tokio::test]
async fn messages_wait_for_an_offline_recipient() {
    let mut dev = Dev::new(support::fixture("pair"), "pair");
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    let (ui, _n) = dev.client("alice").await;
    let first: SendOutput = ui.request(local::SUBMIT, &submit_message("bob", "one")).await.unwrap();
    let second: SendOutput = ui.request(local::SUBMIT, &submit_message("bob", "two")).await.unwrap();

    dev.start_valet("bob").await;
    let store = dev.store("bob");
    let received = eventually(T, || {
        let ids: Vec<_> = store
            .actions()
            .ok()?
            .into_iter()
            .filter(|a| a.kind == ActionKind::Receive)
            .filter_map(|a| a.interaction)
            .collect();
        (ids.len() == 2).then_some(ids)
    })
    .await;
    assert_eq!(received, vec![first.id, second.id], "delivered oldest first");
}

#[tokio::test]
async fn recovery_settles_sends_by_lookup() {
    let mut dev = Dev::new(support::fixture("pair"), "pair");
    dev.start_broker("alice").await;

    // Two sends were written ahead and started before a crash. The broker
    // accepted the first; the second never reached it.
    let (accepted_id, lost_id) = {
        let mut store = Store::open(&dev.data_dir("alice")).unwrap();
        let mut ids = Vec::new();
        for text in ["accepted", "lost"] {
            let id = store
                .request_action_with(ActionRequest::new(ActionKind::Send, json!(null)), |id| {
                    Some(
                        serde_json::to_value(SendInput {
                            client_ref: id,
                            kind: InteractionKind::Message,
                            to: "bob".into(),
                            service: None,
                            caused_by: None,
                            body: Some(json!(text)),
                            context: None,
                            turn_outcome: None,
                            local: true,
                        })
                        .unwrap(),
                    )
                })
                .unwrap();
            store.start_action(id).unwrap();
            ids.push(id);
        }
        (ids[0], ids[1])
    };
    let accepted: SendOutput = {
        let (line, _in) = dev.broker_line("alice").await;
        let _: HelloOutput = line.request(broker::HELLO, &HelloInput { protocol: 1 }).await.unwrap();
        let input = SendInput {
            client_ref: accepted_id,
            kind: InteractionKind::Message,
            to: "bob".into(),
            service: None,
            caused_by: None,
            body: Some(json!("accepted")),
            context: None,
            turn_outcome: None,
            local: true,
        };
        line.request(broker::SEND, &input).await.unwrap()
    };

    // Give the broker a moment to notice the raw line closed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    dev.start_valet("alice").await;

    let store = dev.store("alice");
    let a = store.actions().unwrap();
    let settled = a.iter().find(|x| x.id == accepted_id).unwrap();
    assert_eq!(settled.state, ActionState::Succeeded);
    assert_eq!(settled.interaction, Some(accepted.id));
    let lost = a.iter().find(|x| x.id == lost_id).unwrap();
    assert_eq!(lost.state, ActionState::Interrupted);
}

#[tokio::test]
async fn a_restarted_valet_does_not_record_twice() {
    let mut dev = Dev::new(support::fixture("pair"), "pair");
    dev.start_broker("alice").await;
    dev.start_valet("alice").await;
    dev.start_valet("bob").await;
    let (ui, _n) = dev.client("alice").await;
    let sent: SendOutput = ui.request(local::SUBMIT, &submit_message("bob", "once")).await.unwrap();
    let store = dev.store("bob");
    eventually(T, || store.actions().ok()?.iter().any(|a| a.interaction == Some(sent.id)).then_some(())).await;

    dev.kill_valet("bob").await;
    dev.start_valet("bob").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let receives = dev.store("bob").actions().unwrap().into_iter().filter(|a| a.kind == ActionKind::Receive).count();
    assert_eq!(receives, 1);
}

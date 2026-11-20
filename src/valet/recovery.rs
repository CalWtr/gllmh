//! Recovery at valet start (section 5, Recovery steps 2–4). Deliveries wait
//! until it finishes (step 2). Turns (steps 5–6) belong to the turn engine.

use crate::protocol::broker::{self, LookupInput, LookupOutput};
use crate::types::ActionKind;

use super::Valet;

pub async fn recover(valet: &Valet) -> anyhow::Result<()> {
    // Step 3: settle each unfinished send by asking the broker about its client_ref.
    let sends = valet.store.with(|s| s.unfinished_sends()).await?;
    for send in sends {
        let found: LookupOutput = valet.broker.request(broker::LOOKUP, &LookupInput { client_ref: send.id }).await?;
        match found.interaction.filter(|_| found.found) {
            Some(reply) => {
                tracing::info!(action = %send.id, interaction = %reply.id, "recovery: send was accepted");
                valet.store.with(move |s| s.recover_send(send.id, &reply)).await?;
            }
            None => {
                tracing::info!(action = %send.id, "recovery: send never reached the broker");
                valet.store.with(move |s| s.interrupt_action(send.id)).await?;
            }
        }
    }

    // Step 4: every other unfinished action is interrupted.
    let unfinished = valet.store.with(|s| s.unfinished_actions()).await?;
    for action in unfinished {
        if action.kind == ActionKind::ToolCall {
            tracing::warn!(action = %action.id, "recovery: tool call interrupted; its effect is unknown");
        }
        valet.store.with(move |s| s.interrupt_action(action.id)).await?;
    }
    Ok(())
}

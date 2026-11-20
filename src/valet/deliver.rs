//! Recording what the broker delivers (sections 2 and 5).
//!
//! The receive action, and for an agent its queued turn, commit in one
//! transaction before the reply that acks the delivery. A redelivery after a
//! crash between commit and ack hits the store's unique key and is acked again
//! without new rows.

use crate::frame::{ErrorBody, Responder};
use crate::protocol::broker::{self, CloseInput, CloseOutput, DeliverOutput};
use crate::store::ReceiveOutcome;
use crate::types::{Interaction, Outcome};

use super::Valet;

pub async fn handle(valet: &Valet, interaction: Interaction, responder: Responder) {
    valet.wait_recovered().await;

    let open_turn = valet.opens_turn(&interaction);
    let recorded = {
        let interaction = interaction.clone();
        valet.store.with(move |s| s.record_receive(&interaction, open_turn)).await
    };
    let outcome = match recorded {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::error!(id = %interaction.id, "could not record delivery: {e}");
            responder.err(ErrorBody::retryable(broker::codes::STORE_FAILED, e.to_string()));
            return;
        }
    };
    responder.ok(DeliverOutput {});

    if outcome == ReceiveOutcome::Duplicate {
        // Already recorded before a crash or reconnect. For an agent its turn
        // exists and recovery queued or resumed it; for a user without a
        // composer, close again in case the first close never arrived.
        if !open_turn {
            close(valet, &interaction, Outcome::Received).await;
        }
        return;
    }

    valet.clients.notify(&interaction);
    if open_turn {
        valet.turns_wake();
    } else {
        // A user with no composer closes an interaction as soon as it is recorded.
        close(valet, &interaction, Outcome::Received).await;
    }
}

/// Close an interaction on the broker. Closing twice with the same outcome is harmless.
pub async fn close(valet: &Valet, interaction: &Interaction, outcome: Outcome) {
    let input = CloseInput { id: interaction.id, outcome };
    if let Err(e) = valet.broker.request::<_, CloseOutput>(broker::CLOSE, &input).await {
        tracing::warn!(id = %interaction.id, "close failed: {e}");
    }
}

impl Valet {
    /// Whether a received interaction opens a turn here: on an agent, always,
    /// except a result for a directive one of this user's services sent (M7.1).
    pub fn opens_turn(&self, _interaction: &Interaction) -> bool {
        self.is_agent()
    }
}

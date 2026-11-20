//! The turn engine (M4.3). M3.2 leaves only the seam: users without a
//! composer never start it, and an agent's receives are recorded and queued
//! until M4.3 runs them.

use std::future::Future;
use std::pin::Pin;

use super::Valet;

/// A running turn engine; resolves when it stops (e.g. the composer exited).
pub struct Engine {
    exited: Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>,
}

impl Engine {
    pub async fn exited(self) -> anyhow::Result<()> {
        self.exited.await
    }
}

pub async fn start(valet: &Valet) -> anyhow::Result<Engine> {
    if valet.is_agent() {
        tracing::warn!("this user is an agent, but the turn engine is not built yet (M4.3); turns stay queued");
    }
    Ok(Engine { exited: Box::pin(std::future::pending()) })
}

impl Valet {
    /// A turn was queued: let the engine start it if a slot is free.
    pub fn turns_wake(&self) {}
}

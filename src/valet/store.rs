//! Async access to the user's synchronous store: one writer connection behind
//! a mutex, used from `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::store::{self, Store};

#[derive(Clone)]
pub struct StoreHandle {
    inner: Arc<Mutex<Store>>,
}

impl StoreHandle {
    pub async fn open(user_data_store: &Path) -> store::Result<Self> {
        let path = user_data_store.to_path_buf();
        let store = tokio::task::spawn_blocking(move || Store::open(&path)).await.expect("store open panicked")?;
        Ok(Self { inner: Arc::new(Mutex::new(store)) })
    }

    /// Run `f` on the writer connection without blocking the runtime.
    pub async fn with<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&mut Store) -> R + Send + 'static,
        R: Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut store)
        })
        .await
        .expect("store task panicked")
    }
}

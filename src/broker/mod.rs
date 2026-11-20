//! The `broker`, `gllmh serve`: relays every interaction and owns the log.
//!
//! - `log`: the SQLite interaction log (M2.1). Storage only.
//! - `identity`: the one function that maps a connection to a user (M2.2).
//! - `server`: sockets, hello/send/deliver/close/lookup/status (M2.2).

pub mod identity;
pub mod log;
pub mod server;

pub use server::{ServeArgs, serve};

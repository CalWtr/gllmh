//! Operation shapes of spec section 6, one module per line.
//!
//! Every op has its name as a constant, typed `…Input` / `…Output` structs,
//! and its error codes as constants. Implementations on both sides of a line
//! use these types; nobody hand-writes the JSON.
//!
//! Additions beyond the spec are marked `EXTENSION` and recorded in
//! docs/decisions.md.

pub mod broker;
pub mod composer;
pub mod local;
pub mod tool;

/// The protocol version `hello` carries.
pub const PROTOCOL_VERSION: u32 = 1;

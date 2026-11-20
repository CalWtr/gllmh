//! Tools an agent's model may call (spec section 6.3 `run`, section 7
//! tool_profile): executables, send tools, MCP tools.
//!
//! Owner: M5.1 (executables, send tools), M5.3 (MCP tools).

mod run;

pub use run::{RunError, run};

pub(crate) mod mcp;

mod send;

pub(crate) use send::{SendPayload, send_arguments, send_spec};

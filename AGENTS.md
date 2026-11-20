# GLLMH — notes for agents working in this repo

GLLMH runs humans, agents and services as Linux system users and relays
messages between them through one broker. **The spec is
`/home/caleb/plans/gllmh/0.1.0/final_draft.md`.** Read the sections your task
names before writing code; section numbers in doc comments refer to it. Terms
in backticks (`valet`, `turn`, `directive`, …) are defined once in its
section 1 glossary and mean exactly that here.

Work is tracked in GHH, project `GLLMH`, under the task `GLLMH_0.1.0`. Each
subtask names its spec sections, files and "Done when" tests.

## Build and test

- `cargo build`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt`.
- All four must be clean before you hand work back.
- Integration tests run in dev mode (`--dev`): one system user, no root, no systemd.
  Put temp dirs under `tempfile`, never in `$HOME` or `/run`.
- No live network calls in tests. LLM calls go to the fake provider in `tests/support/fake_llm.rs` (T1).

## Module map

One crate, one binary `gllmh`. Each directory has one owning task; edit
another task's module only for a minimal additive change, and say so in your
report.

| Path | Role | Owner |
| --- | --- | --- |
| `src/types.rs` | stored objects of section 3, shared enums, `Sender` | S0 (shared) |
| `src/frame.rs` | JSON-lines framing and `Peer` (section 6, Framing) | S0 (shared) |
| `src/protocol/` | every op's input/output/error codes, per line (section 6) | S0 (shared) |
| `src/paths.rs` | fixed paths and dev overrides (section 5) | S0 (shared) |
| `src/sdnotify.rs` | `READY=1` to systemd | S0 (shared) |
| `src/cli.rs` | subcommand dispatch; args live in each module | S0 (shared) |
| `src/profile/` | profile types, loading, resolution, `gllmh check` | M1.1 |
| `src/units/` | `gllmh build` unit files | M1.2 |
| `src/broker/log.rs` | SQLite interaction log | M2.1 |
| `src/broker/{server,identity}.rs` | `gllmh serve` | M2.2 |
| `src/store/` | per-user SQLite store, v1_ views | M3.1 |
| `src/valet/` | `gllmh valet`: core, turn engine, results, services | M3.2, M4.3, M6.1, M7.1 |
| `src/attach/` | `gllmh attach`, `gllmh status` | M3.3, M6.2 |
| `src/llm/` | Anthropic and OpenAI clients | M4.1 |
| `src/composer/` | builtin composer, `gllmh composer` | M4.2 |
| `src/tools/` | executable, send and MCP tools | M5.1, M5.3 |
| `src/mcp/` | `gllmh mcp list/call` | M5.2 |
| `tests/support/` | shared integration-test helpers | everyone, additive |
| `examples/` | acceptance services, example custom composer | A1, M6.2 |

## Conventions

- Wire shapes come from `src/protocol/`; never hand-write op JSON. Error
  codes are the constants there. Anything beyond the spec is marked
  `EXTENSION` in a doc comment and gets a line in `docs/decisions.md`.
- A spec gap you have to decide: pick the reading that keeps the section-4
  invariants, write it in `docs/decisions.md` (date, task, decision, why),
  and mention it in your report.
- Ids are ULIDs (`ulid::Ulid`); timestamps are `types::Timestamp` (UTC, RFC 3339).
- SQLite (rusqlite) is synchronous: keep a connection on a dedicated thread
  or use `tokio::task::spawn_blocking`; never block the runtime.
- Write-ahead: an action's row is written before the action runs (invariant 6).
- Errors: `anyhow` at command boundaries, `thiserror` types inside modules.
- Logging: `tracing`; level from `GLLMH_LOG` (default `info`), to stderr.
- Formatting: `rustfmt.toml` (width 120).

## Git

- Each task works on branch `gllmh/<task-code>` (e.g. `gllmh/m2.1`) from current `main`.
- Commit in small logical steps, imperative subject lines (`broker: add interaction log`).
- Do not push, merge or rebase `main`; Claude reviews and merges.

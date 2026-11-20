# Decisions

Spec gaps and the readings chosen for them, newest last. Spec:
`/home/caleb/plans/gllmh/0.1.0/final_draft.md`.

| Date | Task | Decision | Why |
| --- | --- | --- | --- |
| 2026-11-20 | root | `protocol: openai` means the Chat Completions API. | Works with any OpenAI-compatible `base_url`. Chosen with Cal. |
| 2026-11-20 | root | The builtin composer is the hidden subcommand `gllmh composer`, a child process. | The valet then treats builtin and custom composers the same way. |
| 2026-11-20 | S0 | `send` gains `local: bool`, true when a UI or controller submitted on local.sock. | Section 3 resets `hops` to 0 for UI/controller submits, but the broker cannot otherwise tell them from service or turn sends. `root` still follows `caused_by`. |
| 2026-11-20 | S0 | `init` gains `edges` (hello's edges). | The builtin composer's header names whom the agent may message and direct (section 7), and init carried no edges. |
| 2026-11-20 | S0 | `content_block` gains `is_error` on `tool_result`. | Both providers flag failed tool results; a failed, denied or refused call should reach the model as an error. |
| 2026-11-20 | S0 | Error codes `bad_frame`, `unknown_op`, `bad_input`, `internal` on every line; `no_hello` on the broker line; `broker_unavailable`, `bad_kind` on submit. | The spec lists only op-specific codes; these cover malformed traffic. |
| 2026-11-20 | S0 | `notify` is a request whose reply the valet ignores. | Section 6 frames are request/reply; a fire-and-forget op would be a fourth frame kind. |
| 2026-11-20 | root | MCP over a unix socket is newline-delimited JSON-RPC, the stdio framing. | The spec names "a socket" without a transport. |
| 2026-11-20 | A1 | Retry explicit retryable submit errors. Treat loss of a submit reply as ambiguous: a dispatcher or one-shot timer exits with an error, and a recurring timer reconnects for the next tick. | Section 6.3 has no submit idempotency key or lookup for an unknown interaction ID. Replaying an ambiguous submit can create a second interaction. |
| 2026-11-20 | A1 | Recurring timers skip missed intervals and omit `caused_by`. A dispatcher's `result_file` holds one JSON line for the matched result and is replaced on success. | Skipping missed intervals avoids a burst after an outage. Section 2 gives each recurring send a fresh run. The file mirrors the single stdout result. |
| 2026-11-20 | A1 | The acceptance test builds the two examples once with Cargo and reads their executable paths from Cargo's JSON build output. | The tests exercise the actual child processes and work with Cargo target-directory overrides without adding dependencies. |

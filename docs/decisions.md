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
| 2026-11-20 | M3.1 | Start a turn with zero steps and increment before each model call, beginning at step 1. The valet supplies the configured limit to the increment. | Counts model calls and refuses to exceed the section-4 bound even on a resumed turn. |
| 2026-11-20 | M3.1 | A broker delivery commits its receive directly as succeeded. Its turn field is the trigger when the delivery opens a turn and absent otherwise. Use local insertion order for the turn queue. | The transaction is the complete receive operation, and redelivery must leave its first record and turn intact. FIFO survives a clock change. |
| 2026-11-20 | M3.1 | Broker lookup can settle a requested or running send as succeeded. The normal success path requires running. | Section 5 requires settling every unfinished send known to the broker without executing it again. |
| 2026-11-20 | M3.1 | Running-turn recovery reports already interrupted tool actions as well as unfinished tool actions. | A second crash between recovery steps 4 and 5 must retain the tool-call evidence so the valet interrupts the turn instead of resuming it. |

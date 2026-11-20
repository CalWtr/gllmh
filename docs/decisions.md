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
| 2026-11-20 | M3.3 | Attach treats message text and directive block contents as strings. Directive arguments use shell-style quoting, and attach leaves `caused_by` unset. | The terminal syntax provides no causal parent or identity. The valet and broker remain responsible for identity, routing and causal metadata. |
| 2026-11-20 | M2.1 | A result must come from the directive's recipient as a user and be addressed to the director's user, including the owner of a service director. The first accepted result settles the obligation in every delivery state. | Section 6.1 leaves the destination check implicit. Section 2 requires one result to reach the director, and section 4 keeps accepted rows immutable when retried or expired. The log checks the obligation and inserts the result in one transaction. |
| 2026-11-20 | M2.1 | Reject a fresh send with an unknown `caused_by`. Deliver and list a run in log append order, keeping a requeued row in its original position. | The log cannot assign the specified root without a parent. Append order remains stable when acceptance times tie or the clock moves backward, and requeue changes only state. |

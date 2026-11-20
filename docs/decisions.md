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
| 2026-11-20 | T1 | Fake LLM rules are one-shot unless `repeat: true`. Record requests and select rules together before applying an optional delay. | Scripts can express successive steps, and concurrent requests cannot consume the same rule or reorder the log by finishing early. |
| 2026-11-20 | T1 | Substring matching reads the latest user or tool message, joining its text blocks with newlines and reading nested Anthropic tool-result text. | Both provider encodings can match tool output without accidentally matching earlier turns or metadata. |

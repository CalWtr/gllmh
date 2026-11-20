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
| 2026-11-20 | M4.1 | `content_block.raw_json` optionally carries the exact JSON spelling of a thinking block alongside the parsed `raw`. Only blocks for the chosen protocol are replayed. | JSON-lines frames and stored JSON values can escape line breaks in a string, while parsing `raw` alone loses whitespace, key order and escape spelling. Custom composers that only supply `raw` remain compatible. Switching protocols leaves text and tool history usable. |
| 2026-11-20 | M4.1 | Anthropic manual thinking uses half of `max_output` rounded down, with a minimum budget of 1,024. An enabled request needs `max_output > 1024`. | The provider requires a minimum budget below its output cap. Reserving the rest leaves space for the answer. |
| 2026-11-20 | M4.1 | Chat Completions uses `max_completion_tokens`; when thinking is on, it sends `reasoning_effort: medium`. Returned `reasoning_content` can be replayed when the composer supplies it. | The cap includes reasoning and visible output. Medium is a supported effort for reasoning models, and compatible endpoints may require their reasoning field during tool use. |
| 2026-11-20 | M4.1 | OpenAI tool results with `is_error` use a `[tool error]` line before the tool output. Anthropic uses its native flag. | Chat Completions has no tool-result error flag. Keeping the marker inside tool content preserves both the failure signal and the call ID. |
| 2026-11-20 | M4.1 | Retry the recorded request bytes within one `llm_call`. Use jittered exponential backoff starting from 250 ms and capped at 30 s before jitter, with integer `Retry-After` seconds as a minimum delay. Disable redirects and transport-level retries. | The valet can record the exact model input before the first attempt, and the call retains control of the endpoint and attempt limit. |
| 2026-11-20 | M4.1 | An omitted `api_key_file` allows an unauthenticated endpoint. A configured key file must be readable and contain a usable key. | Local compatible endpoints may omit authentication. A misconfigured credential fails before any provider request. |
| 2026-11-20 | M4.1 | Usage counts Anthropic's cache reads and writes as input tokens. Usage may be absent for compatible Chat Completions endpoints. | The normalized input count covers the full prompt, and absent provider measurements stay distinguishable from zero. |

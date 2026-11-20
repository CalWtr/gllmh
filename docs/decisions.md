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
| 2026-11-20 | M1.1 | Require config directories used by the runtime to be `0750` and its profile files to be `0640`, all `root:gllmh`. | Section 5 lists `0640` under `/etc/gllmh/`. Directories need search permission to let the broker reach the files. |
| 2026-11-20 | M1.1 | Absent edge maps grant no permissions. An omitted builtin mode uses message mode. | The section-7 service rule defaults to no edges. The builtin block is optional, so the default uses the conversation timeline and preserves its single-turn limit. |
| 2026-11-20 | M1.1 | Require profile names to match the requested file stem and runtime membership to list each user once. In production reject distinct system account names that resolve to the same UID. | Socket identity must map to exactly one participant under section 4. Dev mode checks duplicate literal account names without OS lookups. |
| 2026-11-20 | M1.1 | Resolve and validate profiles reachable from the selected runtime. Expand `"*"` to all tool profiles in filename order, or an empty list if the tool directory is absent. Keep configured paths in the resolved profile. | A config tree can contain independent workflows. Stable tool ordering makes hello deterministic, and the recipient can resolve paths against the appropriate system user's home when needed. |

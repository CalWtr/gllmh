# Action records

What the `valet` writes into `action.input`, `action.output` and
`action.reasoning` for each `action` kind. The spec (section 3) types these
as `json`; this file fixes their shape, because a `composer` reads them back
through `v1_action` to rebuild the model's context. Rust shapes live in
`src/protocol/records.rs`.

Only the `valet` writes these rows. A composer must treat unknown fields as
absent and must not fail on them.

## receive

```yaml
input:   interaction        # the whole interaction, as delivered (types::Interaction)
output:  null
state:   succeeded          # written together with its queued turn
```

## llm_call

```yaml
input:
  llm_profile:  string           # the profile the composer chose
  messages:     message[]        # GLLMH format, exactly as compose returned them
  tools:        string[]         # tool names offered to the model (after MCP expansion)
  thinking:     boolean          # effective thinking flag for this call
  request:      json             # the provider request body as sent
output:                          # on succeeded
  content:      content_block[]  # the response in GLLMH format: text, tool_use, thinking (raw byte-exact)
  stop_reason:  string
  usage:        { input_tokens: integer, output_tokens: integer }
  response:     json             # the provider response body as received
output:                          # on failed
  error:        { kind: string, status?: integer, message: string, retryable: boolean, attempts: integer }
reasoning:      json             # on succeeded: the thinking blocks' raw values, in order, or absent
```

## tool_call and send from a send tool

Both carry `call_id` and `requested_by` (the `llm_call` whose response asked
for them). What the model sees is a `tool_result { text, is_error }`:
stored in a `tool_call`'s output, and derived from the record for a `send`
(`protocol::records::send_tool_result`).

```yaml
# tool_call
input:
  tool:       string        # the name the model called, e.g. "grep" or "fs__read_file"
  arguments:  json
output:
  tool_result: { text: string, is_error: boolean }
  exit_code?:  integer      # executables
  stderr?:     string       # executables

# send (from send_message or send_directive)
input:        SendInput      # protocol::broker::SendInput as sent to the broker
output:       SendOutput     # on succeeded: { id, root, hops }, also when settled by recovery
output:                      # on failed
  error:      ErrorBody
# derived tool_result: succeeded -> "sent <kind> <id> to <to>"; failed -> "<code>: <message>" (is_error)
```

A `denied` or `interrupted` row may have no output. A composer renders it as
`tool_result { text: "<state>", is_error: true }`.

## send not from a tool

Submitted by a UI, controller or service, or the `result` a directive's turn
sends. No `call_id`. Same input and output as above. The builtin composer
does not render these.

## Rules a composer relies on

- Every `tool_use` in an `llm_call`'s output gets exactly one `tool_call` or
  `send` row with the same `call_id` before the next `llm_call` of that turn,
  unless the valet crashed in between. A composer must still answer a
  `tool_use` with no matching row with `tool_result { text: "not run",
  is_error: true }`, so the provider never sees an unanswered tool_use.
- Tool results answering one `llm_call` go in a single `tool_result`
  message, in the order of the `tool_use` blocks.
- A failed `llm_call` adds nothing to the model's context; the builtin
  composer only uses it to decide `stop: fail`.

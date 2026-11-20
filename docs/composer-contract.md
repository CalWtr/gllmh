# Custom composer contract

A custom composer builds the full context for each model call. The valet owns
provider credentials, translates messages to the provider's format, makes
model calls, runs tools, enforces the step limit, and records the results.
The composer can select history dynamically by querying the user's SQLite
store.

## Process and frames

The valet launches `custom_composer_bin` as a child process, with the user's
configured working directory if one is set. Read requests from stdin, write
replies to stdout, and send logs to stderr. Each line is one JSON object.
Terminate each reply with a newline and flush stdout. If the composer exits,
the valet exits.

Requests carry a unique string `id`, an `op`, and an `input` object.
Replies carry their own unique string `id`, the request's `id` in `re`,
and either an `output` object or an `error` object. An error has a string
`code`, a string `message`, and a boolean `retry` indicating whether
another attempt might succeed. The composer only replies to requests.

The valet sends one `init` and waits for its reply before sending any
`compose`. If `max_parallel_turns` exceeds 1, the composer must handle
overlapping requests for different turns. Replies may arrive out of order.
Use `re` to match them, and keep any context or caches scoped by turn.
The echo example processes requests serially and uses `max_parallel_turns: 1`.

## Initialization

```json
{"id":"1","op":"init","input":{"user":"helper","composer_profile":{"name":"echo","engine":"custom","custom_composer_bin":"/opt/gllmh/echo_composer","work_mode":"inst","max_steps_per_turn":20,"max_parallel_turns":1,"compose_timeout_s":60,"custom":{"locale":"en"}},"llm_profiles":[{"name":"primary","protocol":"anthropic","model_id":"example-model","base_url":"https://provider.example","thinking":{"native":"never","on":false,"returned":"none","replay":"never"},"context_window":100000,"max_output":1024,"tuning":[],"timeout_s":60,"max_retries":0}],"tools":[],"store":"/var/lib/gllmh/helper/store.db","edges":{"helper":{"messages":[],"directives":[]}}}}
{"id":"composer-1","re":"1","output":{}}
```

- `user` is the agent's configured name.
- `composer_profile` is the entire resolved composer profile. Read custom
  settings from `composer_profile.custom`. It is the configured JSON value
  as-is, including explicit null, and is absent if omitted from the profile.
- `llm_profiles` contains the agent's resolved model profiles with
  `api_key_file` omitted. The profile's `name` selects it in a reply.
- `tools` lists allowed tool names, including send tools. The valet owns
  their schemas.
- `store` is the SQLite database path. Open it in read-only mode.
- `edges` is a GLLMH extension. The map is keyed by sender name, and each
  entry gives the allowed `messages` and `directives` recipient lists.

## Composing model input

```json
{"id":"2","op":"compose","input":{"turn":"01HF7YAT000000000000000001"}}
{"id":"composer-2","re":"2","output":{"llm_profile":"primary","messages":[{"role":"system","content":[{"type":"text","text":"Echo the user's input."}]},{"role":"user","content":[{"type":"text","text":"Hello!"}]}],"tools":[],"marks":[{"target":"01HF7YAT000000000000000001","key":"seen_by","value":"echo_composer"}]}}
```

`turn` is the ULID of the triggering interaction. The valet asks for input
before each model call. Query the store afresh so the next composition sees
newly recorded actions. Return the entire `messages` array, including any
history to replay, a configured `llm_profile` name, and the `tools` to
offer from init. Set optional `thinking` to override the profile's default
where the model supports thinking.

A message has a `role` of `system`, `user`, `assistant`, or `tool_result`
and a `content` array. Content blocks have these shapes.

| `type` | Fields |
| --- | --- |
| `text` | `text` |
| `tool_use` | `call_id`, `tool`, `arguments` (JSON) |
| `tool_result` | `call_id`, `text`, optional `is_error` (defaults to false) |
| `thinking` | `raw` (provider block as JSON), optional `raw_json` (original JSON spelling) |

When replaying thinking, preserve the stored provider payload and include
`raw_json` where available. `raw_json` and `is_error` are GLLMH extensions.

Include **every context block of a directive trigger** in the returned
context. Whenever you include an assistant `tool_use`, follow it with a
`tool_result` using the same `call_id`. Put the results for one model call
in a single `tool_result` message in the order of the calls. Find result
rows by both `requested_by` (the model call's action ID) and `call_id`.
If a crash left no matching row, return a result with `text: "not run"`
and `is_error: true`. Keep a model call and its results together when
trimming history.

Return `stop: "done"` to finish the turn successfully, or `stop: "fail"`
to fail the turn without another model call. The current reply schema
requires a string `llm_profile` even with `stop`. An empty string is
accepted for a stop reply. The valet applies `marks` before honoring
`stop`.

```json
{"id":"composer-2","re":"2","output":{"llm_profile":"","stop":"done"}}
```

An invalid reply (for example a malformed output or an unknown profile or
tool) produces `invalid_reply`. If no reply arrives within
`compose_timeout_s`, the valet treats it as `timeout`. A compose error or
timeout permits one more compose attempt for that step, with a fresh request
ID. A second consecutive failure ends the turn as failed. The retry rule
also applies when the composer returns an error with `retry: false`.
For example, a composer can report that it could not read the store with
this reply.

```json
{"id":"composer-2","re":"2","error":{"code":"invalid_reply","message":"could not read the trigger","retry":true}}
```

## SQLite read interface

Use the versioned views. ULIDs, enum labels, and UTC RFC 3339 timestamps
are SQLite text. JSON fields are serialized JSON text; absent optional
columns are SQL NULL. Use explicit column names and an `ORDER BY` when
reconstructing a timeline. Keep read transactions short so later requests
can see newly committed actions.

| View | Columns |
| --- | --- |
| `v1_action` | `id`, `kind`, `turn`, `step`, `requested_by`, `call_id`, `service`, `interaction`, `root`, `input`, `output`, `hazardness`, `reasoning`, `state`, `requested_at`, `ended_at` |
| `v1_turn` | `trigger`, `root`, `state`, `steps`, `opened_at`, `ended_at` |
| `v1_mark` | `target`, `key`, `value`, `set_by`, `set_at` |

The turn's `trigger` doubles as its ID, `root` groups a run, and `steps`
counts model calls. In `v1_action`, `turn` names the trigger of the action's
turn, while `interaction` references the specific interaction recorded in
a receive or successful send. `step` is the model call number when
applicable. `service` names the service behind a service-originated send, and
`hazardness` is the tool's configured label. These columns may be NULL.

Turn states are `queued`, `running`, `done`, `cut_off`, `failed`, and
`interrupted`. Action kinds are `receive`, `send`, `llm_call`, and
`tool_call`. Action states are `requested`, `running`, `succeeded`,
`failed`, `denied`, and `interrupted`. An action in `requested` or
`running` has been recorded ahead of execution or completion.

Read the trigger with a bound parameter.

```sql
SELECT input FROM v1_action
WHERE turn = ?1 AND interaction = ?1 AND kind = 'receive';
```

The receive's `input` contains the whole interaction, including `body`
for messages and results, or `context` for directives. To reconstruct
model responses, tool results, and sends, use the per-kind schemas in
[action records](action-records.md).

### Marks

Return a `marks` array of objects with `target` (a ULID in the store),
`key` (a string), and `value` (any JSON value). The valet writes one mark
per target and key. A subsequent value replaces it, and JSON null removes
it. The valet sets `set_by` to the current turn's trigger ID and `set_at`
to the write time. For example, mark a run's root with a `session` key
and read `v1_mark` to find related runs on later requests.

## Running the echo example

Build it with `cargo build --example echo_composer`. Put the absolute path
to the resulting executable in `composer/echo.json` under the configuration
directory. Set the agent's `composer_profile` to `echo` and give the agent
at least one LLM profile.

```json
{
  "name": "echo",
  "engine": "custom",
  "custom_composer_bin": "/absolute/path/to/target/debug/examples/echo_composer",
  "work_mode": "inst",
  "max_parallel_turns": 1
}
```

The example reads the receive from `v1_action`, sends a system message and
the trigger's content as a user message, offers no tools, chooses the first
LLM profile, and marks the trigger with `seen_by: "echo_composer"`.

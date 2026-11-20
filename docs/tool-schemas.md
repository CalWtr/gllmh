# Executable tool schemas

A tool profile with `tool_bin` points `tool_schema` at a UTF-8 JSON file
with three required fields.

```json
{
  "name": "lookup",
  "description": "Look up a record by key",
  "input": {
    "type": "object",
    "properties": { "key": { "type": "string" } },
    "required": ["key"],
    "additionalProperties": false
  }
}
```

`name` is a nonempty string identifying the schema. The provider sees the
**tool profile's name** as the callable name. `description` is a string and
`input` is the JSON Schema object sent to the provider unchanged. The
valet passes the model's arguments to the executable as JSON on stdin.

The valet reads and checks its configured schema files at startup. Changes
take effect on restart. Unreadable files and invalid schema envelopes stop
startup with the tool's name and file path in the error. A leading `~/` in
`tool_schema`, `tool_bin`, or the user's `cwd` expands against the valet's
home directory. Relative paths resolve from the valet's startup directory.
When `cwd` is omitted, the tool starts in the valet's effective data
directory, including any dev override.

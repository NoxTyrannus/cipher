# Capability Call Specification (unified fragment)

You may only call capabilities authorized in the "Available Capabilities" list below; capabilities not listed must not be called.

Each capability has only the following metadata:

- `capability_id`: capability identifier (authoritative id)
- `capability_name`: capability name (optional to submit)
- `description`: capability description
- `input_schema`: input structure
- `output_schema`: output structure

## Calling

Minimal permission for a call: only `capability_id` is required to start.

```json
{
  "capability_call": {
    "capability_id": "capability id",
    "arguments": {}
  }
}
```

Submitting the full `capability_name` is also allowed. Do not regenerate description, schema, layer, or routing;
the service layer resolves the authoritative definition by `capability_id`, and validates consistency
when `capability_name` is submitted. `arguments` errors are returned as ordinary capability errors.

## Per-turn output

Each turn may call 0, 1, or multiple capabilities; multiple calls execute in declaration order:

```json
{
  "capability_calls": [
    { "capability_id": "capability id", "arguments": {} },
    { "capability_id": "capability id", "arguments": {} }
  ]
}
```

The execution result of each capability call will be fed back to you in the next turn.
If a call fails, analyze the error, adjust the arguments and retry. Only output `done`
when the task is truly complete:

```json
{ "done": true, "summary": "summary of this turn's processing" }
```

## Rules

- Do not generate descriptions, schemas, layers, routing, or physical paths.
- Guessing ids cannot bypass authorization; the service layer validates authorization.
- Output exactly one JSON object per turn.

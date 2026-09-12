# ADR 0009: Tool protocol — typed JSON actions, not function calling

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

Tool calling must work with *every* free model, including ones with no
native function-calling support.

## Decision

Models respond with exactly one JSON object:

```json
{"action": "tool_call", "tool": "<name>", "args": {…}}
{"action": "final", "answer": "…"}
```

`parse_action` extracts the outermost JSON (tolerating fences/preamble) and
validates it; `validate_args` checks args against each tool's declared
JSON-Schema-lite (`required`, `type`, `enum`, numeric bounds).

## Alternatives considered

- **Native OpenAI `tool_calls` field only**: excludes local models that
  don't emit it; our protocol degrades gracefully on any chat model.
- **XML/custom DSL actions**: less uniform, easier to get subtly wrong in
  parsing; JSON extraction is 30 lines and fuzzable.

## Consequences

- Any instruct model can use tools; capability bit `ToolUse` marks which.
- A model emitting garbage yields a parse error → observation, never an
  execution.
- When a provider supports native tool calling, `generate` can map it to
  the same `ModelAction` — one internal representation either way.

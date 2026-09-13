# ADR 0013: FFI live events — callback-based streaming and cross-ABI approvals

- **Status**: Accepted
- **Date**: 2026-09-13

## Context

V0's FFI was strictly blocking: `pai_send` ran the whole agent loop and
returned one JSON blob. V1 needs (a) token-level streaming so the UI shows
the answer as it is generated, and (b) interactive approval — the model may
request a tool the policy says needs a human decision, and that decision
arrives on a different thread than the blocked `pai_send` call.

## Decision

**Events out**: `pai_set_event_callback(handle, cb, user_data)` registers a
`void cb(const char* json, void* ud)` invoked *on the `pai_send` thread* for
every `AgentEvent`. Dart registers a `NativeCallable.isolateLocal`, which
forwards each event into the bridge's reply port — legal because the
callback fires on the same isolate thread that called `pai_send`.

**Approvals in**: `ApprovalNeeded` events carry an `ApprovalRequest` (tool
call id, summary, permissions, risk). The Rust `UiApproval` handler parks
the run on a `tokio::sync::oneshot`; `pai_approve(handle, call_id, granted)`
resolves it from any thread. Timeouts and closed UIs resolve to **deny** —
fail closed.

**Streaming in the core**: providers implement `InferenceProvider::stream`
returning an `EventStream` of `Delta | Done | Error`. `LlamaServerProvider`
decodes OpenAI SSE (`stream:true`, `include_usage`). The agent feeds deltas
through `FinalStream`, a small incremental parser that emits only the
decoded `content` of `{"type":"final", ...}` — tool-call JSON is never
tokenized to the UI; plain-text output streams raw.

## Alternatives considered

- **Polling FFI** (`pai_next_event` per tick): simple but adds UI latency
  and a busy loop; callbacks are the minimal honest primitive.
- **Dart `SendPort` as a native pointer**: `NativeCallable` is the supported
  mechanism; `SendPort.send` from a non-owning thread is not.
- **Streaming raw deltas to the UI**: leaks the wire protocol and flashes
  JSON at users; the `FinalStream` extractor keeps `TextDelta` semantic.

## Consequences

- `pai_send` remains the single blocking call; the callback adds liveness
  without changing the ABI's memory rules (JSON strings, `pai_free_string`).
- `UiApproval`'s 300s timeout means a run can outlive a dismissed sheet but
  never hangs forever.
- `EchoProvider` streams deterministic JSON chunks — the full pipeline is
  testable offline.

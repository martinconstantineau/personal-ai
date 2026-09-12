# Agent runtime

`pai-agent` turns a model + tools + memory into a **bounded, auditable
action loop**. It is deliberately small: the intelligence lives in the model,
the *authority* lives in the runtime.

## The loop

```rust
pub struct AgentRuntime {
    providers: ProviderRegistry,   // inference
    tools: ToolRegistry,           // what may be done
    permissions: PolicyEngine,     // whether it's allowed
    memory: Arc<dyn MemoryBackend>,
    audit: AuditLog,
    max_steps: usize,              // fail-closed bound
    step_timeout: Duration,
    device: Device,                // identity for audit attribution
}
```

`run(RunRequest)`:

1. **Recall** relevant memories for the current user message.
2. **Build** the `AIRequest`: system + protocol prompt + `[memory]` lines +
   conversation history (trust-tagged).
3. **Generate** (with `step_timeout`) → `ModelAction`.
4. `Final` → persist assistant message, `Done` audit, return.
5. `ToolCall` → `ToolInvocation`: validate args → `decide` → `RequireApproval`
   → `ApprovalHandler` → execute → `ToolExecuted` audit → append observation
   (Untrusted) → next step.
6. Exceed `max_steps` or cancel → `Failed`/`Cancelled` — never runaway.

## Approval flow

`ApprovalHandler` is injected per frontend:

- CLI: `AutoApprove` (demo) or an interactive y/n prompt.
- Flutter: `ApprovalRequest` surfaces as a sheet; the user sees tool name,
  args, risk level, and reason. The decision is audited either way.

## Agent definitions

`AgentDefinition` = prompt preamble + allowed tool subset + model
preferences. Specialization happens by *narrowing* a general agent (fewer
tools, tighter policy) — never by giving a prompt more raw power.

## Cancellation

`CancelToken` (atomic flag) is polled between steps and passed to providers;
the FFI exposes it per-handle so a UI "stop" button maps to one call.

## What the runtime deliberately doesn't do

- No planning framework, no multi-agent swarm, no self-modification loop —
  those are opt-in `AgentDefinition`s on top, not baked-in complexity.
- No direct shell/file/network access — all effects are typed `Tool`s.
- No hidden retries: a provider failure ends the step with an audited error;
  the model can decide to retry as an explicit action.

## Observability

Every run emits: `RunStarted`, `ModelRequested`/`ModelResponded`,
`ToolRequested`, `ToolAllowed`/`ToolDenied`/`ApprovalRequested`,
`ToolExecuted`, `MemoryWritten`, `Done`/`Failed`/`Cancelled`. `pai audit`
(and the FFI `pai_audit`) render them — a run is fully reconstructible from
its audit trail alone.

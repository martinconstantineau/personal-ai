# Data flow

## One chat turn (the vertical slice)

```
Flutter/CLI         pai-ffi            pai-agent                providers/tools
    │                  │                   │                         │
    │ pai_send(msg)    │                   │                         │
    │─────────────────>│ send_lock.lock()  │                         │
    │                  │ AgentRuntime::run │                         │
    │                  │──────────────────>│                         │
    │                  │                   │ memory.recall(msg)      │
    │                  │                   │──────────────┐          │
    │                  │                   │<─────────────┘ [memories]
    │                  │                   │ build_request:          │
    │                  │                   │  system+protocol+       │
    │                  │                   │  [memory] lines+history │
    │                  │                   │ provider.generate(req)  │
    │                  │                   │────────────────────────>│
    │                  │                   │<────────────────────────│
    │                  │                   │ ModelAction::ToolCall   │
    │                  │                   │ validate_args           │
    │                  │                   │ permissions.decide(...) │
    │                  │                   │ approval? (handler)     │
    │                  │                   │ tool.execute(args, ctx) │
    │                  │                   │────────────────────────>│
    │                  │                   │<────────────────────────│
    │                  │                   │ audit(ToolExecuted)     │
    │                  │                   │ append Untrusted obs    │
    │                  │                   │ ── next step ──         │
    │                  │                   │ ModelAction::Final      │
    │                  │<──────────────────│ persist + audit(Done)   │
    │  JSON{run_id,    │                   │                         │
    │   state,answer,  │                   │                         │
    │   events}        │                   │                         │
    │<─────────────────│                   │                         │
```

Key invariants:

- **Memory recall happens before every model call**, not once per run — a
  tool observation in step *n* can be recalled in step *n+1*.
- **Observations enter the transcript as `TrustLevel::Untrusted`** messages,
  prefixed by the provider. The model sees them as evidence, not orders.
- **Audit events are emitted synchronously at each transition** — if the
  process dies mid-run, the log shows exactly where.
- **`send_lock`** serializes runs per runtime handle; the Flutter isolate
  bridge additionally serializes FFI calls.

## Data at rest

```
<data_dir>/
  config.toml            Config (TOML)
  store.db               SQLite: all structured state + FTS indexes
  blobs/ab/cd/abcdef…    content-addressed binary payloads
  keys/<device>.key      ed25519 secret key (0600)  → keystore later
  models/<id>/           installed model weights + manifest
  sync/                  FolderTransport drop dir (if enabled)
```

## Config → behavior

`ComputePolicy` is the single switch that decides where work may run:

| Policy | Local | Trusted device | Remote provider |
|---|---|---|---|
| `LocalOnly` (default) | ✓ | — | — |
| `LocalPreferred` | ✓ | ✓ | — |
| `CloudAllowed` | ✓ | ✓ | ✓ |
| `CloudPreferred` | ✓ | ✓ | ✓ (first) |

The broker returns `Unavailable` rather than silently degrading — a workload
that can't run under policy is surfaced to the user, not rerouted behind
their back.

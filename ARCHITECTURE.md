# Architecture

Personal AI is a **modular monolith**: one Rust core, one C ABI boundary, one
Flutter UI. The core owns all state, policy, and I/O; the UI is a thin client
that renders JSON.

```
┌─────────────────────────────────────────────────────────────┐
│ Experience Layer                                            │
│  apps/desktop (Flutter, dart:ffi)      apps/cli (`pai`)     │
└───────────────────────────┬─────────────────────────────────┘
                            │ C ABI: pai_init / pai_send / pai_audit
                            │ JSON in, JSON out
┌───────────────────────────▼─────────────────────────────────┐
│ Core Layer (Rust workspace)                                 │
│                                                             │
│  pai-agent      bounded agent loop: prompt → model action   │
│       │         → permission check → tool → observation     │
│       │                                                     │
│  pai-inference  InferenceProvider trait + LlamaServer + Echo│
│  pai-tools      Tool trait, registry, arg validation        │
│  pai-permissions  least-privilege policy engine + approvals │
│  pai-memory     SqliteMemory (FTS5 + cosine), working set   │
│  pai-audit      append-only event log, secret redaction     │
│  pai-broker     workload placement (local → trusted → cloud)│
│  pai-models     manifest registry, sha256-verified installs │
│  pai-identity   ed25519 device keys, user/device records    │
│  pai-storage    SQLite (WAL, migrations, content-addressed  │
│                 blobs, FTS5), single `Store` handle         │
│  pai-documents  ingest → chunk → FTS; `search` → snippets   │
│  pai-sync       SyncObject + transport trait + folder impl  │
│  pai-tasks      @every scheduler, durable task table        │
│  pai-voice / pai-vision / pai-media  pipeline traits        │
│  pai-config     TOML config + ComputePolicy                 │
│  pai-core       domain types, ids, TrustLevel, Error        │
└─────────────────────────────────────────────────────────────┘
```

## Data flow (one user turn)

1. **UI** calls `pai_send(handle, "what is 2 + 3?")` (JSON).
2. **pai-ffi** hands the text to `AgentRuntime::run` on a Tokio runtime,
   serialized through a mutex so a run is never interleaved.
3. **Agent runtime** recalls relevant memories (`pai-memory`), builds an
   `AIRequest` with the tool manifest + trust-tagged context.
4. **Provider** (`pai-inference`) produces a `ModelAction`: either
   `ToolCall{name,args}` or `Final{answer}`.
5. **Tool dispatch** (`pai-tools` + `pai-permissions`): validate args →
   `PolicyEngine::decide` → approval if required → execute → audit.
6. The **observation** is appended to the conversation as an *Untrusted*
   message and the loop repeats until `Final` or the step cap.
7. Everything is written to **audit** and **storage** as it happens.

See `docs/architecture/data-flow.md` for the full sequence diagram.

## Layers and rules

- **Dependency direction**: `pai-core` has no intra-workspace deps; everything
  else may depend downward only (`agent` may use `memory`, never the reverse).
- **No async in interfaces that don't need it.** Only inference, tools, voice,
  and the FFI runtime are async; storage is synchronous behind a `Mutex`.
- **Explicit stubs, no fakes.** Crates that aren't implemented yet expose
  traits and types that compile — they never return canned responses.
- **JSON at the boundary.** The FFI surface is `String -> String`; the UI
  never sees Rust types. Versioning happens inside the JSON payloads.

## Why these choices

Each significant decision has an ADR in `docs/adr/`. The short version:

| Decision | Choice | Why |
|---|---|---|
| Core language | Rust | native perf on every device, memory safety, long-lived maintainability |
| UI | Flutter | one codebase for mobile + desktop; dart:ffi needs no codegen |
| Inference | llama.cpp HTTP | free, runs everywhere, OpenAI-compatible; swap for MLX/ExecuTorch later |
| Storage | SQLite (bundled) | zero admin, FTS5 built in, encrypted-later via SQLCipher |
| Memory | hybrid FTS + vectors | works today without an embedding model, upgrades in place |
| Sync | E2EE objects + transports | transport never sees plaintext; folder impl proves the contract |

## Crate map

| Crate | Role | Status |
|---|---|---|
| pai-core | domain model, ids, trust levels | **implemented** |
| pai-config | TOML config, `ComputePolicy` | **implemented** |
| pai-storage | SQLite store, migrations, blob store | **implemented** |
| pai-identity | users/devices, ed25519 signing | **implemented** |
| pai-inference | provider traits, llama-server + echo | **implemented** |
| pai-models | model manifest registry + installer | **implemented** |
| pai-memory | trait + SQLite backend + working set | **implemented** |
| pai-tools | tool trait, registry, JSON-schema-lite | **implemented** |
| pai-permissions | policy engine, approvals, risk levels | **implemented** |
| pai-agent | bounded run loop, cancel token | **implemented** |
| pai-audit | event log + redaction | **implemented** |
| pai-broker | placement policy | **implemented** |
| pai-tasks | durable scheduler | **implemented** |
| pai-sync | object model + folder transport | **implemented (folder)** |
| pai-documents | text ingest + chunking + FTS search | **implemented (text)** |
| pai-voice | VAD/STT/TTS pipeline traits | interface only |
| pai-vision | vision request/result types | interface only |
| pai-media | job model for gen/transcode | interface only |
| pai-ffi | C ABI + runtime handle | **implemented** |
| connectors/email | `EmailProvider` trait | interface only |

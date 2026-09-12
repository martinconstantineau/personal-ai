# System overview

Personal AI is an **operating layer**, not an app: a portable Rust core that
owns identity, memory, policy, and execution, wrapped by replaceable
experience layers (Flutter desktop today; mobile, watch, and headless later).

## Design principles

1. **Local-first, free models only.** The default path must run a capable
   assistant with zero network and zero spend. Remote capacity is opt-in.
2. **The model is replaceable; the platform is not.** Models are plugins
   behind `InferenceProvider`. Everything durable — memory, permissions,
   audit, sync — lives below the model.
3. **Capability, not trust.** Tools declare the permissions they need; the
   policy engine decides per call. The model can't escalate.
4. **Plain interfaces.** JSON at the FFI boundary, JSON in the tool protocol,
   SQL under the hood. No codegen pipeline is load-bearing.
5. **Small enough to audit.** The whole core is ~20 focused crates; each one
   can be read in an afternoon.

## Process & threading model

- One process per app; the Rust core runs inside it via `libpai_ffi`.
- A single Tokio `Runtime` lives in `PaiRuntime`; `send` calls are serialized
  by a mutex (one agent run at a time per runtime — per-conversation
  concurrency lands in V1.1).
- Flutter runs FFI calls on a worker isolate (`pai_bridge.dart`) so inference
  never blocks the UI thread.

## Subsystem map

| Concern | Crate | One-line contract |
|---|---|---|
| Types | `pai-core` | Shared domain model; zero deps |
| Config | `pai-config` | TOML `Config` + `ComputePolicy` |
| Persistence | `pai-storage` | `Store`: migrations, WAL, blob store |
| Identity | `pai-identity` | users/devices, ed25519 sign/verify |
| Inference | `pai-inference` | `InferenceProvider` + registry |
| Models | `pai-models` | manifests, sha256-verified install |
| Memory | `pai-memory` | `MemoryBackend` + SQLite impl |
| Tools | `pai-tools` | `Tool` trait + registry + validation |
| Policy | `pai-permissions` | `PolicyEngine::decide` + approvals |
| Agency | `pai-agent` | bounded run loop over the above |
| Audit | `pai-audit` | append-only event log + redaction |
| Placement | `pai-broker` | `route(Workload)` → `Placement` |
| Scheduling | `pai-tasks` | durable `@every` tasks |
| Sync | `pai-sync` | `SyncObject` + `SyncTransport` |
| Documents | `pai-documents` | ingest → chunk → FTS search |
| Voice | `pai-voice` | VAD/STT/TTS pipeline traits |
| Vision | `pai-vision` | vision request/result types |
| Media | `pai-media` | generation/transcode job model |
| FFI | `pai-ffi` | C ABI: JSON in/out, opaque handle |

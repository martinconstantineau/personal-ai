# Components

## pai-core — domain model

All shared types: `User`, `Device`, `Session`, `Conversation`, `Message`
(with multimodal `Content`), `Model`/`ModelCapability`, `Agent`/`AgentRun`,
`Task`, `ToolCall`, `Memory`, `Document`, `MediaAsset`, `Connector`,
`Workflow`, `SyncObject`, `AuditEvent`.

Two load-bearing details:

- **`id_type!`** generates `UserId`, `DeviceId`, … — newtype UUIDs so an
  `AgentRunId` can never be passed where a `SessionId` belongs.
- **`TrustLevel`** (`System > User > Verified > Untrusted`) orders content by
  provenance; `Content` variants carry it so the prompt builder never mixes
  instructions and evidence.

## pai-storage — one `Store`

`rusqlite` with the `bundled` feature (no system SQLite needed). WAL mode,
foreign keys on, `SCHEMA_VERSION` migrations in `meta`. The schema covers
identity, sessions, messages, models, memories (+FTS5 + sync triggers),
audit, tasks, sync objects, and documents (+FTS5). Binary payloads go to a
content-addressed blob dir (sha256 hex → fanout dirs) referenced from tables.

## pai-inference — providers

```rust
trait InferenceProvider {
    fn name(&self); fn capabilities(&self) -> ModelCapabilitySet;
    fn generate(&self, req: &AIRequest) -> Result<ModelAction>;
    fn stream<'a>(&'a self, req: &'a AIRequest) -> EventStream<'a>;
}
```

- **`LlamaServerProvider`** speaks the OpenAI-compatible
  `/v1/chat/completions` API — works with llama.cpp `llama-server`, Ollama,
  LM Studio, vLLM, etc. Free software, runs anywhere.
- **`EchoProvider`** is a deterministic offline responder used by the demo,
  tests, and CI — it parses the same JSON protocol a real model would emit.

`protocol_prompt()` renders the tool manifest + trust rules into the system
prompt; `parse_action()` extracts the model's action tolerating markdown
fences.

## pai-agent — the loop

`AgentRuntime::run(RunRequest)`:

```
loop (≤ max_steps, each step ≤ step_timeout):
    request = build_request(history + recalled memories + tool manifest)
    action  = provider.generate(request)?
    match action:
        Final(answer)      → persist + audit Done, return
        ToolCall{..}       → validate args → policy.decide → maybe approve
                             → tool.execute → audit → append observation
```

Fail-closed: hitting the step cap ends the run with an error rather than a
hanging tool call. A `CancelToken` aborts between steps. Every transition is
an audit event.

## pai-permissions — policy engine

18 typed `Permission`s (`MemoryWrite`, `ShellExec`, `FileWriteOutsideData`,
`NetworkEgress`, `SendEmail`, …). `PolicyTable` maps (permission, optional
target glob) → `Allow | Deny | RequireApproval`; `decide` picks the
**most restrictive** matching rule. `with_defaults()` is deny-by-default for
anything unlisted — new permissions must be consciously placed.

## pai-memory — hybrid recall

`MemoryBackend` trait + `SqliteMemory`: FTS5 over content for keyword recall
(tokenized OR-query so natural questions match), brute-force cosine over
stored embeddings when an `EmbeddingProvider` is present, importance/
confidence/decay scoring, and entity links via `memory_relationships`.
`WorkingMemory` is the per-run scratch pad the agent writes to.

## Supporting crates

- **pai-broker**: `route(Workload)` → Local / TrustedDevice / Remote /
  Unavailable, driven by `ComputePolicy`.
- **pai-tasks**: durable `@every Ns` scheduler; handlers are closures
  registered at boot, schedule state in the store.
- **pai-sync**: `SyncObject { id, kind, ciphertext, … }`, `SyncTransport`
  trait, `FolderTransport` implementation, last-writer-wins merge.
- **pai-documents**: `Extractor` trait (text impl), fixed-size chunking with
  overlap, per-section FTS + snippet extraction.
- **pai-voice / pai-vision / pai-media**: traits and job models — the
  interfaces are the deliverable at this stage.
- **connectors/email**: `EmailProvider` trait; first concrete connector
  lands with the V1 connector framework.

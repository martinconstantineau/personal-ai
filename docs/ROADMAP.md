# Roadmap

Phases are *capability* milestones, not dates. Each keeps the platform
green: `scripts/test.sh` passes on every merge.

## ✅ V0 — Groundwork

Monorepo, Rust core crates, FFI boundary, Flutter shell, vertical slice
(remember → recall → calculator → permission → audit), CLI, docs + ADRs.

## ✅ V1 — Usable local assistant

- Interactive approval UI in Flutter (`ApprovalRequest` sheets → `pai_approve`,
  policy editor backed by the persisted `policies` table)
- Streaming token events through FFI (`EventStream` + SSE for llama.cpp/
  OpenAI-compatible servers → `pai_set_event_callback` → Dart stream;
  `FinalStream` decodes only the `final` action's content)
- Local inference auto-detect (`pai models detect` / `provider: "auto"`;
  llama-server / Ollama / LM Studio `/v1/models` probing + PATH binaries)
- Hugging Face as a first-class model source: `hf://owner/repo/file.gguf`
  refs, `pai models search` / `pai models files`, LFS sha256 verification,
  `pai models serve` via a local `llama-server`
- Conversation management (persisted sessions/conversations/messages,
  list/rename/delete/select, per-conversation shared|isolated memory scope)
- `memory.forget` tool (approval-gated via `Permission::MemoryDelete`) +
  memory browser screen
- Crash-safe run resume (`agent_runs` checkpoints per step, `pai_runs` /
  `pai_resume`, `pai runs interrupted|resume|abandon`)
- `cargo audit` + `cargo deny` in CI (`deny.toml`); signed-commit policy in
  docs/SECURITY.md
- Schema v2 migration (ADR 0012); FFI event/approval design (ADR 0013)

## ✅ V1.1 — Hardening & device features

- At-rest encryption: SQLCipher (vendored, `rusqlite` `bundled-sqlcipher`)
  for `personal-ai.db`; raw 256-bit key via OS keystore (Windows Credential
  Manager / macOS Keychain / Secret Service) with 0600 file fallback;
  plaintext DBs auto-migrate via `sqlcipher_export` (ADR 0014)
- Device keys (Ed25519 + store key) in `pai_identity::keystore`; schema v3
  records `devices.key_storage`
- Interactive CLI approvals: `pai chat`/`pai runs resume` prompt on
  AskUser policies (tool, action, permissions, risk); deny fails closed
- Ollama embeddings: `Embedder` trait + `OllamaEmbedder` (`/api/tags`
  auto-detect, `nomic-embed-text` et al.); auto-embed on `memory.put`
  and `recall`, brute-force cosine merged with FTS
- Documents: `DocumentStore` (txt/md/html → chunks → FTS + section
  embeddings), `documents.search`/`documents.ingest` tools with `[Dn]`
  citations, `pai docs` CLI + FFI + Flutter Documents screen
- Filesystem jail for model-driven file reads (`ToolContext::allowed_roots`
  = `<data_dir>/inbox`); user-initiated ingest bypasses

## V2 — Connected devices

- E2EE sync: X25519 key agreement, per-object sealing, `FolderTransport`
  encrypted end-to-end; relay transport (ciphertext-only server)
- Trusted-device placement via `pai-broker` (phone asks desktop to run STT)
- Voice pipeline MVP: whisper.cpp STT + piper TTS + VAD
- Vision: llama.cpp multimodal describe/OCR
- Connector framework v1 + first real `EmailProvider` (IMAP/JMAP, drafts-first)

## V3 — The personal OS layer

- Multi-step `Workflow`s (declarative, permission-bounded)
- Proactive surface: `pai-tasks` schedules + notification channels
- Mobile builds shipping; on-device inference backends (ExecuTorch/MLC)
- Federation story for opt-in shared memories (family/team scopes)

## Known technical debt (tracked, not hidden)

| Item | Why it's deferred | Exit |
|---|---|---|
| LWW merge only | CRDT per-kind is V2 design | V2 |
| Brute-force cosine recall | fine <100k memories | `sqlite-vec` in V2 |
| `pai_send` blocking per handle | run-per-runtime model | V1 concurrency |
| Resume re-asks approvals (no double-charge guarantee) | approval is ephemeral by design | revisit in V2 |
| Approval has no rate-limit/audit-escalation | needs UX spec | V2 |
| No provider health/failover | single provider config | V2 broker |
| `hf://` resolve isn't pinned by default | `@rev` supported; pin for reproducibility | V2 |
| Tool sandbox is in-process (path jail + capability ctx), not OS-level | seccomp/AppArmor needs subprocess isolation | V2 |
| OS-keystore key loss = data loss | same trust domain as OS login | passphrase wrap option, V2 |
| PDF/EPUB/DOCX extractors declared not implemented | adapters needed | V2 |

## Cross-cutting risks

- **Small-model tool reliability** — mitigated by protocol tolerance +
  `ToolUse` capability gating + observation-on-error loop.
- **Consent fatigue** — approvals batch by session policy; defaults keep
  risky classes approval-gated until the user relaxes them.
- **Mobile llama.cpp footprint** — process-per-model is heavy on phones;
  ExecuTorch/MLC path is planned before V3 mobile ship.

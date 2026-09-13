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

## V1.1 — Hardening & device features

- At-rest encryption for `store.db` (SQLCipher or age-wrapped key)
- OS-keystore-backed device keys (iOS Keychain / Android Keystore / libsecret)
- Sandboxed tool execution profiles (`ExecutionMode` → seccomp/AppArmor)
- Interactive approvals in CLI (replace `AutoApprove` in `chat`)
- Ollama provider polish + embedding endpoint for vector recall
- Document ingest UI + RAG answers with citations

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
| Store not encrypted at rest | needs key mgmt decision | V1.1 |
| Device keys file-backed | keystore is per-OS work | V1.1 |
| LWW merge only | CRDT per-kind is V2 design | V2 |
| Brute-force cosine recall | fine <100k memories | `sqlite-vec` in V2 |
| Auto-approve in CLI (`chat` still uses it) | interactive CLI prompts | V1.1 |
| `pai_send` blocking per handle | run-per-runtime model | V1 concurrency |
| Resume re-asks approvals (no double-charge guarantee) | approval is ephemeral by design | revisit in V2 |
| Approval has no rate-limit/audit-escalation | needs UX spec | V1.1 |
| No provider health/failover | single provider config | V2 broker |
| `hf://` resolve isn't pinned by default | `@rev` supported; pin for reproducibility | V1.1 |

## Cross-cutting risks

- **Small-model tool reliability** — mitigated by protocol tolerance +
  `ToolUse` capability gating + observation-on-error loop.
- **Consent fatigue** — approvals batch by session policy; defaults keep
  risky classes approval-gated until the user relaxes them.
- **Mobile llama.cpp footprint** — process-per-model is heavy on phones;
  ExecuTorch/MLC path is planned before V3 mobile ship.

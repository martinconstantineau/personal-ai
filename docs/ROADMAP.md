# Roadmap

Phases are *capability* milestones, not dates. Each keeps the platform
green: `scripts/test.sh` passes on every merge.

## ✅ V0 — Groundwork (this PR)

Monorepo, Rust core crates, FFI boundary, Flutter shell, vertical slice
(remember → recall → calculator → permission → audit), CLI, docs + ADRs.

## V1 — Usable local assistant

- Interactive approval UI in Flutter (`ApprovalRequest` sheets, policy editor)
- Streaming token events through FFI (`EventStream` → Dart stream)
- llama.cpp auto-detect: find `llama-server`/Ollama, suggest installable
  models from the catalog, one-command `pai models install` + serve
- Conversation management (list/rename/delete, per-conversation memory scope)
- `memory.forget` tool + memory browser screen
- Crash-safe run resume (persist run state per step)
- `cargo audit` + `cargo deny` in CI; signed commits policy

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
| Auto-approve in CLI demo | approval UX is UI work | V1.1 |
| `pai_send` blocking per handle | run-per-runtime model | V1 concurrency |
| Approval has no rate-limit/audit-escalation | needs UX spec | V1.1 |
| No provider health/failover | single provider config | V2 broker |

## Cross-cutting risks

- **Small-model tool reliability** — mitigated by protocol tolerance +
  `ToolUse` capability gating + observation-on-error loop.
- **Consent fatigue** — approvals batch by session policy; defaults keep
  risky classes approval-gated until the user relaxes them.
- **Mobile llama.cpp footprint** — process-per-model is heavy on phones;
  ExecuTorch/MLC path is planned before V3 mobile ship.

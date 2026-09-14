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

**V2a done (2026-09-14):** E2EE sync MVP — signed offer/accept pairing
(`pai pair`), shared vault key wrapped via X25519+HKDF, per-object
XChaCha20-Poly1305 sealing with object-key AAD, `SyncEngine` over
`FolderTransport` pushing/pulling sealed memory objects + tombstones
(LWW), `sync_peers` trust table (schema v4), `pai sync push|pull|run|status`.
See ADR 0015.

**V2b done (2026-09-14):** email connector — `EmailProvider` trait +
`ImapProvider` (search/read/draft/label/archive/delete over IMAP+TLS via
rustls+webpki-roots), `email.*` tools gated on `EMAIL_*` permissions
(send/delete high-risk, approval-gated), `pai email` CLI with keystore
passwords (`email.json` holds no secrets), FFI ops + Flutter mailbox
screen. Drafts-first: IMAP can't send; SMTP/lettre is the send path.

**V2c done (2026-09-14):** voice pipeline MVP — `WhisperServerStt`
(whisper.cpp `whisper-server` HTTP, multipart `/inference`), `PiperTts`
(piper binary, `--output-raw` PCM → WAV wrap), `EnergyVad` (RMS +
hangover, dependency-free), `voice.json` config with `PAI_WHISPER_URL`/
`PAI_PIPER_MODEL` env overrides, `pai voice
status|configure|transcribe|say|turn`. File-based I/O for now — live mic
capture needs OS audio permissions (cpal) and is the next step.

**V2e done (2026-09-14):** sync relay transport — `pai sync serve`
(tiny_http) stores opaque `.syncobj` blobs through `FolderTransport`;
`RelayTransport` implements `SyncTransport` over HTTP so E2EE sync works
across networks. Bearer-token auth (`--token`/`PAI_SYNC_TOKEN`), mutually
exclusive `--dir`/`--relay` on `sync push|pull|run|status`, default bind
127.0.0.1. The relay still only ever sees sealed objects.

**V2f done (2026-09-14):** sync scope beyond memories — synchronized
conversations (+messages) and documents now travel through the same
sealed-object engine. `conv/`/`msg/`/`doc/` object kinds with LWW
tombstones, blob bytes inside the sealed payload, session stubs for the
FK chain, `pai conv sync`/`pai docs sync` scope toggles + `docs ingest
--sync`. Schema v5 adds `updated_at`/`deleted` to conversations and
`sync_scope`/`updated_at`/`deleted` to documents.

- E2EE sync: relay-behind-TLS deployment notes shipped at
  `docs/deployment/relay-tls.md` (Caddy + nginx configs, token
  handling, mTLS/allowlist hardening, and the honest metadata floor).
**V2l done (2026-09-15):** synced tasks with claim/lease — schema v6
persists `trigger_json`/`payload_json`/`result_json` plus `claimed_by`/
`lease_expires_at`/`updated_at`/`deleted`. `task/<id>` objects ride the
sealed engine (rank 4, no FK deps); `pai task add|list|remove|tick|sync`
manages them. `tick` claims due tasks via a single-UPDATE CAS
(unclaimed or expired-lease only), pushes the claim over the transport
*before* running, then executes — `{"kind":"prompt"}` payloads go
through the agent with `DenyApprovals` (background runs refuse
interactive permissions, never silently grant them). `@every Ns`
triggers requeue via `next_fire`; expired leases make crashed runners'
tasks claimable again. Conflicting claims converge via the engine's
LWW — newest `updated_at` wins on every replica. Verified live: task
created on A → synced to B → B claimed, ran, pushed the result → A's
tick ran nothing. Honest limit: claims suppress *accidental* double-run,
not a malicious peer racing inside the sync window — every vault member
can execute any synchronized task.
**V2k done (2026-09-14):** vault rotation + peer revocation —
`pai sync rotate` mints a fresh vault key and pushes it to every paired
peer as `vrot/<to>/<from>` objects (peer-ECDH sealed — delivery doesn't
depend on vault state — plus an ed25519 signature verified against the
sender's `sync_peers` row). Adoption is epoch-gated and **gossiped**:
an adopter re-signs and republishes to *its* peers, so a rotation
reaches the whole vault even when the pairing mesh isn't fully
connected. `sync pull|run` adopts pending rotations before pulling;
`pair remove` now points at rotate for actual revocation. Verified
live: rotate → adopt → post-rotation push/pull between two devices.
**V2g done (2026-09-14):** live mic capture + playback — cpal 0.16
(windows 0.54 chain → prebuilt windows-sys 0.52, no binutils needed on
windows-gnu), VAD-endpointed `capture_utterance` (pre-roll, ~750 ms
trailing silence, max cap) → mono 16 kHz i16 for whisper, `play` for
spoken replies, `wav_to_pcm16` decode. `pai voice listen` (mic→STT→text)
and `pai voice turn --mic` (full spoken turn, reply through speakers);
`voice status` probes devices. Hardware smoke test behind `--ignored`.

**V2h done (2026-09-14):** trusted-device compute brokerage — broker RPC
over the sealed sync transport. `breq/<to>/<id>` / `bres/<to>/<id>`
objects (XChaCha20-sealed like every `SyncObject`; the sync engine skips
the `b*` prefixes) carry requests to a specific paired device and
responses back. `pai broker devices|serve|call` — `serve` answers ops
this device has providers for (`stt` whisper-server, `tts` piper,
`infer`/`describe` llama-server), `call` targets a peer and waits.
Verified live: request → peer executes on Ollama → sealed reply.

**V2m done (2026-09-15):** broker scheduling + GC + streaming —
`SyncTransport::delete` (folder unlink, relay `DELETE` endpoint, default
no-op for transports that can't) makes object GC real. `pai broker
serve` announces `bcap/<device>` capability objects (sealed,
timestamped, refreshed every 60s, 5-min TTL); `pai broker call any <op>`
routes to the lowest-id fresh announcer instead of a named peer.
Requests carry `expires_at_ms` — workers skip *and delete* work the
caller already gave up on; served `breq`s, consumed `bres`es, and stream
chunks are deleted too, so transports stop accumulating broker objects.
`OpHandler::handle_stream` (default: one chunk via `handle`) drives
`bres/<to>/<id>/<seq>` chunk objects + the usual final marker; `call
--stream` prints infer deltas as they arrive — llama-server `Delta`s
become chunks on the worker. Verified live: `call any --stream infer`
routed by capability, streamed qwen's tokens, and left zero broker
objects on the transport.
**V2i done (2026-09-14):** Flutter voice UI + FFI ops — `pai_voice_status`
({stt,tts,mic,speaker,whisper_url}), `pai_voice_listen` (blocking
mic→whisper, {heard,text?,wav_b64}), `pai_voice_transcribe` (WAV path),
`pai_voice_say` (piper→speaker, {ok,played}|{ok,played:false,wav_b64}).
VoiceSetup is probed once at `pai_init`. Chat screen: mic button in the
input row (dictation lands in the field for review), speaker toggle in
the appbar speaks each reply aloud.

**V2n done (2026-09-16):** streaming STT — `SegmentGate` splits the
VAD stream at ~400 ms pauses (utterance still ends at ~750 ms trailing
silence); `capture_segmented` hands each pause-finalized segment to
whisper while the cpal channel keeps buffering, so partial transcripts
land mid-utterance instead of after the final pause. `stream_transcribe`
wraps capture + per-segment STT (dedicated current-thread runtime —
mic capture is synchronous; silence-only segments are skipped via
`is_quiet`; a mid-capture STT failure doesn't lose later segments).
`pai voice listen --stream` prints partials as they close; the FFI op
`pai_voice_listen_stream` + Dart `voiceListenStream` return
{heard, text, partials[]} (FFI can't push live events, so partials
arrive collected with segmentation preserved). Deadline/disconnect
mid-utterance flushes the buffered tail instead of dropping it.

**V2d done (2026-09-14):** vision MVP — `ImageUnderstandingProvider` +
`LlamaVisionProvider` (llama.cpp `--mmproj` models via OpenAI `image_url`
data-URIs), `pai describe <image> [--prompt]`, and the `vision.describe`
tool so the agent can inspect images inside the filesystem jail
(`FilesRead`, Low risk, untrusted output).

**V2p done (2026-09-16):** process-based vision adapter —
`ProcessVisionProvider` spawns a user-configured VLM runner per
describe (same external-boundary philosophy as whisper-server/piper/
llama-server — no linked ONNX/C++ runtime on windows-gnu). `<data_dir>/
vision.json` `process` block: `{command, args[], timeout_secs}` with
`{image}`/`{prompt}` argv placeholders — covers ONNX Runtime CLIs,
MLX-VLM on Apple silicon, `llama-mtmd-cli`, any local runner. Image
bytes land in a temp file (cleaned up after); stderr surfaces on
non-zero exit; a wall-clock kill bounds hung/slow first-run loads.
Selection: `vision.json` wins when its command resolves on PATH, else
llama-server — `pai describe`, the `vision.describe` tool, and the FFI
surface all pick it up transparently.

- Vision: grounding/detections when models expose them; image ingest
  into documents (OCR text into the document store)
**V2j done (2026-09-14):** SMTP send — `email.json` gains an optional
`smtp` block ({host, port, tls: tls|starttls|none, user?}); a
hand-rolled submission client (EHLO→STARTTLS→AUTH PLAIN→DATA, dot-
stuffing) over the same rustls+webpki stack as IMAP — no new deps.
`ImapProvider::send` delegates when configured (drafts-only otherwise),
so `email.send`, `pai email send`, and `pai_email_send` all light up at
once. Password shared via the `email:<user>` keystore entry.

**V2o done (2026-09-16):** OAuth2 for email — RFC 8628 device-
authorization flow (works headless; Google `oauth2.googleapis.com` +
Microsoft `login.microsoftonline.com` presets, custom-IdP overrides for
device/token URLs and scopes). `pai email configure --oauth
google|microsoft` prints the user code + verification URL, polls until
authorized, and stores the refresh token at `email-oauth:<user>` —
`email.json` gains only the public client_id. With an `oauth` block,
IMAP switches `LOGIN`→`AUTHENTICATE XOAUTH2` and SMTP `AUTH PLAIN`→`AUTH
XOAUTH2`; access tokens resolve per session via the refresh grant and
rotated refresh tokens re-store transparently. App-password auth stays
the default when no `oauth` block exists.

- Connectors: registry abstraction beyond email

## V3 — The personal OS layer

- ~~Multi-step `Workflow`s (declarative, permission-bounded)~~ — shipped:
  `workflow add|run|resume|runs|remove|sync` on `pai`, `prompt`/`tool`
  steps with `{{input}}`/`{{steps.<id>.output}}` templates, per-workflow
  tool allowlist enforced at dispatch (empty = no tools), crash-safe
  run cursor + resume, `wf/` sealed sync objects with tombstones.
- ~~Proactive surface: `pai-tasks` schedules + notification channels~~ —
  shipped: `notifications` inbox (schema v8) synced as `ntf/` objects with
  roaming read state; `notify.send` tool (permission-gated,
  `NotificationSend`); `--notify` on `pai task add` publishes the result;
  external delivery via notify.json `email_to`/`webhook_url` (opt-in only);
  `pai notify list|open|send|clear|remove|configure|test` + FFI inbox ops.
- ~~Mobile builds shipping~~ — shipped: `libpai_ffi.so` cross-compiled
  for arm64-v8a / armeabi-v7a / x86_64 via `cargo-ndk` at API 26
  (cpal/AAudio), packaged into `flutter build apk`/`appbundle`
  (`minSdk = 26`, INTERNET + RECORD_AUDIO, cleartext for LAN relays).
  Windows runner builds via VS BuildTools. `scripts/build_android_ffi.sh`
  reproduces the native libs; OpenSSL is consumed as prebuilt per-ABI
  static archives (vendored openssl-src can't cross-compile on Windows
  hosts). iOS remains unbuildable off macOS.
- ~~On-device inference backends (ExecuTorch/MLC)~~ — shipped as the
  process-boundary adapter (`ProcessInferenceProvider`): any local
  runner — ExecuTorch runner, MLC-LLM CLI, `llama-cli`, a custom script —
  plugs in via `<data_dir>/inference.json` `process` block `{command,
  args[], model?, timeout_secs}`. `{prompt}` in args is substituted as a
  single argv element (never shell-interpreted); with no placeholder the
  rendered prompt pipes to stdin. stdout becomes the completion —
  `{"type":"final",...}` action JSON is parsed, so tool dispatch still
  flows through the permission gate. Streaming forwards stdout lines as
  deltas; a wall-clock kill bounds hung loads. `--provider process`
  selects it explicitly; `--provider auto` prefers it over HTTP probing
  when `inference.json` resolves. No native runtime linkage — the same
  boundary philosophy as whisper-server/piper and the V2p vision adapter.
- ~~Federation story for opt-in shared memories (family/team scopes)~~ —
  shipped: **circles**. A circle is a named symmetric key held by a
  subset of vault members; `memories.share_circle` rows seal under it so
  non-member vault peers get ciphertext they can't open (federation is
  cryptographic, not advisory). `pai circle create|grant|list|leave`;
  grants ride `ckg/<circle>/<device>` objects sealed to the member's
  pairwise X25519 key (same wrap as vault bootstrap). `memory.remember`
  gains a `circle` arg; `memory.share` retargets existing rows (Medium
  risk — moving data to peers). Leave is forward-only: the key drops,
  received rows fence to `device_local`, already-shared copies stay
  shared.

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

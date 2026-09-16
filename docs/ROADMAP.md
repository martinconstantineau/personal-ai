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

## V4 — Personal App Cloud (see docs/PRD-personal-app-cloud.md)

- ~~Signed app packages (`pai-apps`)~~ — shipped: `manifest.toml` schema,
  Ed25519 signature over manifest + content digest, `AppRegistry`
  install/list/remove, `pai deploy` + `pai apps sign|verify|list`.
- ~~Sandboxed WASM runner~~ — shipped: wasmi 2.0 + WASI preview1,
  deny-by-default (no env/network; sockets never preopened), `files`
  preopens per manifest, fuel + memory limits, `pai apps run` audited.
- ~~Per-app storage provisioning~~ — shipped: `data/` + `data.db` created
  on install; `schema.sql` applied when `migration.auto_migrate`;
  upgrades preserve live `data/` (merged back over package seeds).
- ~~App package sync~~ — shipped: installed apps roam as sealed `app/<id>`
  objects carrying the whole package + signature; the receiver re-verifies
  against `sync_peers.ed_pubkey`/own device keys before `install_trusted`,
  removals ship tombstones, and live `data/` never syncs.
- ~~LAN device mesh (`pai-mesh`)~~ — shipped: signed multicast
  announcements (`pai sync serve --announce`), `pai mesh discover`, and
  zero-config `pai sync run --lan`; relay auth is the pairing-derived
  `hex(peer_key)` bearer — only paired devices can authenticate.
- `pai_apps_list` FFI + Dart `appsList` for the dashboard.

**V4g done (2026-09-15):** portable model packs — `pai models install
<slug> --to <dir>` writes weights plus a self-describing `index.json`
(manifest + sha256 per file) into any directory. `pack_roots()` probes
`pai-models/` under every mounted volume; `pai models scan` (and
`locate` lazily inside `serve`/`runnable`) verifies the sha and
registers+repoints entries — plug a drive into a fresh device and its
models are usable on sight. `list` marks unplugged packs `offline`.
ADR 0017.

**V4h done (2026-09-15):** app backups — `pai apps backup` snapshots
package + live `data/` into a sealed `bkp/<app>/<writer>` object that
roams to paired devices; apply only stores the pak (never touches the
live install), and `pai apps restore` re-verifies the embedded package
signature before reinstalling and swapping `data/` with rollback — the
PRD's versioned-backup + rescue path. `apps backups` lists,
`apps backup-delete` tombstones, restore `--from <writer>` picks a
device. ADR 0018.

**V4i done (2026-09-15):** app authoring loop — `pai apps init <name>`
scaffolds a source project (editable `manifest.toml` + Rust wasm
skeleton); `pai apps build <dir> [--sign]` compiles `wasm32-wasip1`
(or validates an existing package dir) into a deployable package.
init → build → deploy → run works end-to-end.

**V4j done (2026-09-15):** app placement + migration — schema v12
`apps.active_device` names the one device running an app's live
`data/` (`NULL` = legacy everywhere). `pai apps migrate <id> --to
<peer>` snapshots into a migrate-flagged `bkp/` object, hands over
placement via the `app/` object, and parks local `data/`; the target
restores inline on pull (rank order: package, then restore), everyone
else sheds stale state. `run`/`backup`/`restore` refuse on inactive
devices — no divergent instances. ADR 0019.

**V4k done (2026-09-15):** remote app execution — `pai apps run --on
<peer|any>` sends the run over the sealed broker transport (`app-run`
op → `pai_apps::app_run_op`); the peer runs it in the same wasmi
sandbox and returns base64 stdout/stderr/exit/fuel. Apps already sync
to every paired device, so `any` routes to whichever peer is serving.
Verified live (A→B over a folder transport) and in
`tests/v4k_remote_run.rs`. Any paired vault member may invoke runs —
same trust class as the existing stt/tts/infer ops. (Lettered V4k;
code predates V4j's landing — ordering is commit order.)

**V4l done (2026-09-15):** on-device app runtime — `pai_apps_run` FFI
export + Dart binding + Apps screen. The Flutter app (desktop and
Android FFI) lists synced packages and runs them in the same wasmi
sandbox as the CLI, showing stdout/stderr/exit. Full path: packages
sync → list → run, no shell needed.

**V4m done (2026-09-15):** `pai-share` capability tokens — signed
Ed25519 grants ({app_id, actions, grantee_key, device, expires}) with
a file-backed ShareStore; verify() checks signature, revocation,
expiry, action coverage. Consumed by V4o's guest path.

**V4n done (2026-09-15):** mobile app polish — `pai_apps_list` FFI
now carries `active_device` + own device id (the Apps screen badges
here / →peer / everywhere and greys out runs that aren't active
here), `pai_apps_run` enforces placement at the FFI boundary itself
(closing the "caller-side" hole — the UI can no longer fork live
state), `pai_apps_migrate` + `pai_peers_list` exports drive a
migrate-to-peer sheet, and runs accept an args field.

**V4o done (2026-09-15):** capability-shared guest execution —
`pai apps share <app>` mints a signed, expiring token (bearer, or
bound to a peer's device key with `--for`), `apps grants`/`apps
revoke` manage them (audit: AppShared/AppShareRevoked), and
`pai apps run --cap <token> --on <device>` lets a NON-vault device
execute: the `greq/` object carries the token + an ephemeral X25519
key, `broker serve` verifies (signature, revocation, expiry, app
binding, grantee signature when bound) and runs it through the same
sandboxed `app_run_op`; the `gres/` reply seals to the ephemeral key.
Serve also works vault-free — a host that only shares to guests
needs no pairing. Covered by tests/tests/v4n_guest_run.rs.

**V4p done (2026-09-15):** guest read/write handles — `read`/`write`
token actions now consume: `pai apps read <app> <path>` and `apps
write <app> <path> --file|--text` run locally, or as a guest with
`--cap <token> --on <device>`; `share --action` accepts a
comma-separated list (`read,write`). Paths jail under the app dir —
reads limited to `files/`+`data/`, writes to `data/` only, `..` and
absolute paths refused, 8 MiB object cap. The server maps
op→required-action (`app-run`→exec, `app-read`→read,
`app-write`→write), so an exec-only token can't read. Bound-token
request signatures cover the op, so the action is tamper-proof too.

**V4q done (2026-09-15):** share management on-device —
`pai_share_grant`/`pai_share_list`/`pai_share_revoke` FFI exports,
Dart bindings, and an Apps-screen share dialog (action checkboxes,
days, optional bind-to-peer) plus a grants sheet with revoke.
`ShareStore::grant` now takes `DeviceId` (it only ever used
`issuer.id`) so the FFI can mint without fetching the full Device.

**V4r done (2026-09-15):** share re-grant — a bound grantee mints
narrower sub-tokens under its own device key. `Capability` gains an
optional `parent` chain embedded in the token JSON, committed into
the child's signed payload (parent `token_id` appended — absent keeps
the legacy 8-line payload, so pre-V4r tokens still verify).
`ShareStore::delegate` refuses: parent without `share`, bearer
parent, widened actions, `app_id` change, expiry beyond the parent's,
or a mismatched device restriction. `verify_chain` verifies
recursively — each hop's signature checks against the parent's bound
`grantee_key`; the root still resolves `issued_by` to own device or a
paired peer. Revoking a parent tombstone kills every descendant.
CLI: `pai apps delegate --parent <token.json> --action … [--for
<peer|64-hex-key>] [--days N]`. Verified live: paired grantee
delegated an exec-only sub-token, an unpaired third device ran the
app on the host through it, and revoking the parent refused the
child.

**V4s done (2026-09-15):** delegation reaches the shell —
`pai_share_delegate` FFI export (parent token JSON in, child JSON
out; `--for` accepts a paired-peer prefix or a raw 64-hex pubkey,
`days` ≤ 0 keeps the parent's expiry), Dart binding + bridge op, and
a "Re-grant token…" entry in the Apps-screen grants sheet (paste the
parent JSON, checkboxes offered only for the parent's actions). Grant
listings show the chain: `pai apps grants` prints `↳ <parent>` on
delegated tokens and `pai_share_list` gains a `parent` field.

**V4t done (2026-09-15):** guest calls over FFI — `pai_guest_call`
takes one JSON request (`{op, app_id, args, token, to, dir|relay,
timeout_secs}`) and runs `call_guest` on the runtime's tokio loop:
`to` empty resolves the root issuer through a delegated chain, bound
tokens are signed by the local device key (a token bound elsewhere
fails fast). Dart `guestCall` + bridge op + a "Use a shared token"
dialog on the Apps screen — paste the token, pick folder/relay
transport, run/read/write with decoded results. Also fixes
`--on any` on delegated tokens: the target was the child's
`issued_by` (the delegator) instead of the root host — the CLI arm
and `guest_call` resolved it twice; `guest_call` now returns the
resolved target so the label/audit show the real host.

**V4 checklist complete.** Signed sandboxed app packages, per-app
storage, package sync, LAN mesh, portable model packs, backups,
authoring loop, placement + migration, remote execution (broker +
capability-shared guest), mobile runtime + share/delegate UI, and
guest read/write handles are all shipped and pushed.

### V5 — mesh intelligence (in progress)

- [x] **V5a — placement scoring** (`pai-broker`): `bcap` announcements
  carry a `DeviceLoad` hint — live in-flight op count plus registered
  battery/thermal/RAM/cores — and `find_peer` scores candidates
  (battery/thermal dominate, then busy, then hardware; lowest device
  id still breaks ties). Wire-compatible: pre-V5 announcements lack
  `load` and score neutrally. `pai broker serve` reports the live
  busy count via a probe wrapping `handle`. (ADR-0021)
- [x] **V5b — placement preference weights**: `pai broker prefer
  <peer> <w>` stores a local `place_weight.<id>` meta value added to
  that device's score in `find_peer` — positive prefers, negative
  avoids, 0 clears. `broker devices` shows the weight. Client-side
  only; no wire change.
- [x] **V5c — live power/thermal probing**: `probe_power()` samples
  real power state (`GetSystemPowerStatus` on Windows, sysfs +
  cpufreq on Linux) at registration AND per `bcap` announce — a
  device that unplugs stops attracting work within ~60 s. Linux
  cpufreq <80%-of-max reports `thermal_throttled`. Windows RAM probe
  fixed (`GlobalMemoryStatusEx` — was 0). macOS/other OSes report
  `None` (neutral).

- [x] **V5d — rescue-mode restore**: `pai apps rescue <id>` claims
  `active_device` and restores the newest backup when an app's home
  device is dead/lost (migration needs the source alive; rescue is
  the takeover). `pai apps rescue --all --from <dead>` sweeps every
  app placed on that device. The claim propagates via `app/`; a
  resurrected device parks its stale `data/` on pull — same healing
  as migration (ADR-0019 amendment). `app_rescued` audit events.

- [x] **V5e — App Operator tools (PRD §6.8)**: `apps.share` and
  `apps.backup` agent tools over a new `AppOperator` surface on
  `ToolContext` — "give Sarah access" mints a real scoped capability
  (`ShareStore::grant`, exec/read/write/share + expiry + optional
  device binding), "back up the database" runs `backup::create`. Both
  default to `AskUser` (a minted token is a working credential; a
  backup copies app data) and run through the same `PolicyEngine`
  approval flow as every other gated tool. `StoreAppOperator` in
  pai-agent implements it over the local store — wired into both the
  CLI and FFI runtimes, so the Flutter agent gets it too.
  `all_permissions()` also fixed to list every enum variant (email
  writes, calendar, files, mic/camera/contacts were missing from the
  policy-editor surface).

- [x] **V5f — App Operator diagnostics (PRD §6.8)**: "why is my app
  broken?" → `apps.status` tool + `pai apps status <id>` roll install
  state (registry + `apps` row), placement (resolved to a local or
  paired device name, stale-claim flag when the device isn't a peer),
  live `data/` size, backup freshness, share tokens by status, and the
  last 10 audit events mentioning the app (deploy/run/migrate/rescue
  failures included) into one report. Read-only → new
  `Permission::AppInspect` defaults `AlwaysAllow`, `RiskLevel::Low`,
  `ExecutionMode::Local`. `StoreAppOperator::status` is the single
  impl — CLI, FFI, and the agent all answer from the same query.

- [x] **V5g — app run logs**: every `app-run` — local `apps run`,
  broker-served remote, guest — now writes `apps/<id>/logs/<ts>.json`
  (exit code / trap message / stdout / stderr / fuel), newest 20 kept.
  Logs are local-only by design: they never sync, never ride `bkp/`
  paks (`snapshot_data` walks `data/` only), and aren't a sandbox
  preopen. Surfaces: `pai apps logs <id> [-n]`, the `apps.logs` agent
  tool (`AppInspect`), and a `logs` rollup inside `apps.status` —
  "why is my app broken?" now reaches the actual stderr, not just the
  audit outcome. `run_logged` in pai-apps is the single funnel, so all
  callers (CLI arm, `app_run_op`, guest ops) record identically;
  trapped runs log the error text before propagating.
- [x] **V5h — app CRDT documents**: `data/crdt/<doc>.json` under an
  installed app is a shared-mutable JSON document. Each writing device
  publishes its field-set as `acrdt/<app>/<doc>/<writer>` sealed
  objects; peers merge **field-wise** (LWW-map: max `t_ms`, writer-asc
  tiebreak) instead of whole-file LWW, so concurrent edits to different
  fields survive on every device. Plain-JSON apps get collaboration for
  free; `{"v":…,"t":…}` / `{"d":true,"t":…}` cells give per-field
  timestamps + deletes. `app_crdt_cells`/`app_crdt_view` (migration
  V13) hold merge state + provenance so unchanged docs don't re-push
  (FAT32-coarse mtimes can't fake edits) and merged fields re-ship at
  their observed ts — writers can't dominate by echoing fresh mtimes.
  Anti-forgery mirrors backups: key/object/payload writer triple must
  agree and the writer must be paired. Cells that arrive before the
  package materialize on install (`apply_app` hook); crdt/ docs are
  multi-master by design and exempt from `active_device` parking —
  placement governs where an app *runs*, shared docs converge
  everywhere. Deleting the file withdraws that writer's field-set;
  fields other copies still hold survive (observed-delete semantics).

- [x] **V5i — app OAuth ("add Google login", PRD §6.8)**: the host
  holds the credential — RFC 8628 device flow extracted into the new
  `pai-oauth` crate (the email connector re-exports it, so V2o call
  sites are untouched). `apps/<id>/auth.json` stores *public* provider
  config (client id, endpoints, scopes) and rides the `app/` sync
  object's `auth` field so config propagates; refresh tokens never do —
  they live in the OS keystore at `app-oauth:<app>:<provider>` with a
  0600-file fallback under `data_dir/.oauth/` (headless hosts, same
  pattern as `store.key`). At run time `run_logged` resolves each
  provider to a fresh access token injected as `PAI_OAUTH_<NAME>` —
  the sandbox never sees the refresh token; guest-capability runs get a
  dedicated no-token path (`app_run_op_guest`). `auth.json` and `logs/`
  joined `data/` in the reserved set — excluded from packages,
  signatures, backups, and upgrades preserve them. `pai apps auth
  <id> --provider google --client-id …` runs the flow; `--status` /
  `--remove` manage providers; the `apps.configure` agent tool does the
  same two-phase flow under `AppConfigure` (default `AskUser`) with an
  `app_auth_configured` audit event.

- [x] **V5j — stable app URLs** (`pai serve`): every app that opts in
  with `serve = true` gets an HTTP surface at
  `http://<device>:<port>/apps/<id>/<path>` — the gateway runs the app
  CGI-style (request → `REQUEST_METHOD`/`PATH_INFO`/`QUERY_STRING`/
  `HTTP_*` env vars + body on stdin; the app prints `Status:`/
  `Content-Type:` + blank line + body). Follows `active_device`, so
  the same path works on every device — the URL survives migration;
  apps placed elsewhere are forwarded over a new `app-serve` broker op
  (vault members and capability guests alike — guests' requests carry
  no owner OAuth envs, same as `app-run` guest runs). The `serve` flag
  lives in the signed manifest, so the opt-in is re-checked on the
  serving device rather than trusted from the gateway. This is the
  addressable half of the PRD's `app.user.devices`; a DNS/Tailscale
  name layer on top is a deployment concern. Run logs + `app_served`
  audit events cover the surface like any other run.

- [x] **V5k — `pai_fetch`: host-mediated HTTP for sandboxed apps** —
  WASI preview1 has no sockets, so an app that needs HTTP (including
  spending its injected `PAI_OAUTH_*` token) calls the `env::pai_fetch`
  host function: JSON request in (`{"method","url","headers","
  body_b64"}`), JSON response out (`{"status","headers","body_b64"}`),
  with a size-probe return convention. Gated inside the sandbox by
  `network = "outbound"` AND the manifest's new `allowed_hosts` list
  (exact or `*.suffix`, so `network = "outbound"` alone permits no
  fetches — the level is the capability, the list is the scope).
  HTTPS-only except loopback; redirects never auto-followed (a 3xx
  could hop outside the allowlist — the app re-requests `Location`
  through the same gate). 10s timeout, 4 MiB body caps, and every call
  — allowed or denied — appends to `logs/fetch.log` beside the run
  logs, so `pai apps status`/`apps.logs` surfaces cover it.

- [x] **V5l — audio generation** (music, SFX, ambience — not speech):
  `ModelCapability::AudioGeneration`, `MediaJobKind::TextToAudio`, and the
  `AudioGenerationProvider` trait in `pai-inference` (image-gen shape:
  `generate_audio(prompt, duration_secs) -> bytes`). Concrete adapter:
  `pai_media::providers::HttpAudioGen` — POST `{prompt, duration_seconds}`
  to `{url}/generate`, body back is the audio — the same local-server
  convention as whisper-server, so any MusicGen/stable-audio wrapper can
  front it (default `http://127.0.0.1:8179`; `media.json` or
  `PAI_AUDIO_GEN_URL`). `audio.generate` agent tool (`MediaGenerate`
  permission, High/SideEffecting) writes the artifact under
  `<data_dir>/media/` and returns a path — audio bytes never enter model
  context. `pai audio status|configure|gen` runs on the light command
  path (no inference stack). The remaining gap is execution *placement*:
  jobs aren't yet broker-routed (that's the declared MediaJob queue
  track, shared with image/video).

- [x] **V5m — broker-routed media jobs**: `media-run` op executes
  generation on whichever paired device serves it — a phone can ask the
  home GPU box. `BrokerOps` advertises `media-run` only when an audio
  provider is detected; `find_peer("media-run")` picks the least-loaded
  advertiser (placement weights apply). Worker side runs
  `pai_media::jobs::media_run_op`: provider call → result blob +
  `media_jobs` row (migration V14: kind/prompt/params/state/requester/
  worker/result_blob/error — local-only bookkeeping). Requester side
  (`pai audio gen --device <peer>|any`) records the same job lifecycle
  — queued → running → done/failed — stores the returned bytes as a
  blob, writes the WAV. `pai audio jobs` lists recent jobs. Vault-member
  op only (guests can't burn your GPU). Image/video jobs share the
  table + op when those providers land.

## V5n — app-surface completion (done)

The Flutter app now surfaces the full FFI surface end-to-end:

- **Media UI** (`6de76de`) — Media rail destination: submit prompts,
  watch job state chips, export artifacts; `f5eaea0` routes generation
  to paired mesh peers announcing `media-run` when no local
  audio-gen server exists.
- **Devices screen** — live provider endpoints + binaries, model packs
  (install to a drive / rescan / serve — `81bb1e1`, `eaab745`), paired
  devices, share grants.
- **Provider health** — rail health dot (green serving / amber echo /
  grey loading) with a status tooltip; Devices lists every detected
  endpoint.
- **Pairing over FFI** (`2c7ea5e`) — offer/accept/complete file
  exchange; `ffda95d` adds exchange-via-shared-folder (one button per
  device) and `pai_init` now reuses the persisted device identity.
- **Sync in-app** (`2a48557`, `ffda95d`) — Sync-now dialog (LAN /
  shared folder / relay, push/pull/both), persisted targets, and an
  auto-sync scheduler (`sync.auto_minutes` meta).
- **Slash commands** (`9c2cac3`) — `/verb [arg]` in the chat input
  with live autocomplete.
- **Responsive shell** (`4699c7f`) — bottom nav < 640px, icon rail to
  1120px, labeled rail above.
- **Onboarding + help** (`54d15a1`) — first-run welcome, F1 / `?`
  anywhere.
- **A11y pass** (`d8bd318`) — labels on all icon buttons, contrast-safe
  health dot, skeleton/loading announcements, transcript landmark,
  live-region command suggestions.

## What's next (candidate slices, unordered)

Shipped:

- **Removable-drive watch** (`b201589`) — model-pack roots rescan on
  mount: drive letters are polled on Windows, `/media`/`/mnt` roots on
  Linux, no Rescan press needed.
- **Packaged audio backend** — `services/media-gen/` is the reference
  `POST /generate` wrapper (FastAPI-shaped, stdlib-only stubs; MusicGen
  / stable-audio.cpp drop in behind the same contract). `pai media gen`
  works out of the box against it.
- **Image/video generation adapters** — `pai-media` providers:
  `SdCppImageGen` (stable-diffusion.cpp's `/v1/images/generations`),
  `OnnxImageGen` + `OnnxVideoGen` (diffusers-onnx-style `POST /generate`
  + `GET /jobs/{id}` polling). `MediaConfig` gained
  `image_gen_url`/`image_backend`/`video_gen_url` in `media.json`
  (+ `PAI_*_GEN_URL` envs); `media-run` dispatches by `kind` and
  `pai media gen --kind image|video` routes locally or to a mesh peer.
- **Placement visualization** — `pai_devices_placement` FFI + broker
  `list_caps` surface `bcap` ops, live `DeviceLoad`, freshness, and
  per-device place weights; the Devices screen renders the full
  placement card (ops chips, score, weight, effective load).
- **Stable app names** — `app.user.devices` layer on `pai serve`:
  `pai apps names` prints stable `<app>.<device>.devices` names, and
  the serve gateway rewrites the Host header so URLs travel between
  devices.
- **Voice loop** — continuous hands-free mode: a headphones toggle on
  the chat input loops listen → send → speak until switched off
  (auto-enables spoken replies).
- **Mobile ↔ desktop parity** — verified on the Android emulator
  (API 36, x86_64): chat, media gen of both audio and image kinds via
  the device→host mesh, Devices placement, QR render, relaunch
  persistence. Found and fixed a real Android keystore bug
  (`keyring` is memory-only there → store.key file fallback) that made
  every second launch fail to open the vault.
- **QR pairing** — `pai pair offer --qr` / `--accept --qr` render a
  scannable terminal QR; `pai_pair_qr` FFI + the desktop Pair dialog
  render the same payload in-app with a paste path for the counter-offer.
  Scan side stays blocked: camera plugins need Developer Mode on the
  build host.
- **Screen-reader smoke test** — walked under TalkBack on the emulator
  (Narrator/NVDA substitute): focus ring advances, every control
  exposes its label, headings and live regions announce correctly.

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

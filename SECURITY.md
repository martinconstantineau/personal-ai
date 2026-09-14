# Security

Personal AI's value proposition is privacy: **data stays local, the model is
untrusted, and every action is gated and logged.** This file is the summary;
the full threat model lives in `docs/security/threat-model.md`.

## Security model

- **Local-first by default.** `ComputePolicy::LocalOnly` is the default
  placement policy. Remote providers can only run when the user has
  explicitly set a cloud-allowing policy *and* the provider declares itself.
- **The model is not trusted.** Models emit a constrained JSON action
  (`{"action":"tool_call",...}` / `{"action":"final",...}`). They cannot
  execute anything directly — all effects go through the tool registry and
  the permission engine.
- **Least-privilege tools.** `PolicyTable::with_defaults()` denies unknown
  permissions and requires approval for side effects (sends, deletes,
  writes outside the data dir). Most-restrictive matching rule wins.
- **Prompt-injection defense.** Retrieved memories, tool observations, and
  document content carry `TrustLevel::Untrusted`; providers prefix such
  content so it can't impersonate user instructions. AI-inferred memories
  are stored at a lower trust tier and labeled in prompts.
- **Audit everything.** Permission decisions, tool requests/results, memory
  writes, and run lifecycle transitions are appended to a local audit table.
  Argument blobs pass through `pai_audit::redact`, which masks keys named
  like secrets (`password`, `token`, `api_key`, …).
- **At rest.** `personal-ai.db` is SQLCipher-encrypted under a 256-bit key
  held by the OS keystore (Windows Credential Manager / macOS Keychain /
  Secret Service) with a 0600 file fallback; `PAI_PLAINTEXT_STORE=1` is
  the documented escape hatch. See ADR 0014.
- **Keys.** Device signing keys are ed25519, keystore-backed
  (`devices.key_storage` records the backend). Sync adds a separate
  X25519 agreement keypair per device plus a shared 256-bit vault key —
  all keystore-backed the same way (ADR 0015).
- **Email content is untrusted.** Message bodies read via the email
  connector are `TrustLevel::Untrusted` data — never instructions. Send
  and delete are approval-gated `High` risk tools; the connector is
  drafts-first — `send` only exists when an `smtp` block is configured
  in `email.json`, and even then `email.send` still requires approval.
  Passwords resolve from `PAI_EMAIL_PASSWORD` or the OS keystore
  (`email:<user>`) — never from `email.json`, and `pai_audit::redact`
  masks them in logs.
- **Vision is local-only.** `vision.describe`/`pai describe` send images
  to a localhost llama.cpp server (data-URI in the request body); the
  tool reads only jailed paths and its output is untrusted model text.
- **Voice is local-only.** Mic capture and speaker playback use cpal
  (OS audio APIs) — audio is endpointed by a local energy VAD and sent
  to a `whisper-server` on localhost; TTS spawns the local `piper` binary
  (with a timeout and no shell — arguments can't inject commands). No
  audio leaves the machine; nothing records until `voice listen`/
  `turn --mic` opens the stream explicitly.
- **Sync is end-to-end.** Devices pair via an ed25519-signed
  offer/accept exchange; the vault key travels wrapped by an
  ECDH-derived peer key. `SyncObject` payloads are
  XChaCha20-Poly1305-sealed with the object key as AAD — transports and
  shared folders handle ciphertext only. The optional HTTP relay
  (`pai sync serve`) persists the same opaque `.syncobj` blobs via
  `FolderTransport`; a bearer token gates it, but it is still
  ciphertext-only. Synced data is opt-in per object: memories default to
  `synchronized`, conversations and documents default to `device_local`
  and travel only when the user marks them (`pai conv sync`,
  `pai docs sync`, `docs ingest --sync`). Possession of the vault key is
  read+write access; `pair remove` does not rotate it.
- **Broker RPC rides the same vault.** `pai broker call/serve` moves
  compute requests between paired devices as `breq/<to>/<id>` /
  `bres/<to>/<id>` `SyncObject`s — sealed exactly like sync payloads, so
  only the addressed device (a vault holder) can read a request, and the
  sync engine ignores the `b*` prefixes. Device targeting is a routing
  prefix, not an access check: any vault member *can* unseal any broker
  object — same trust boundary as the rest of the vault. A worker only
  executes ops it has providers for (whisper/piper/llama-server on
  localhost) and replies with errors for the rest; prompts, audio, and
  results are ciphertext on every transport.

## Known limitations (groundwork stage)

- Approval UX exists in the CLI (`[y/N]` prompt); richer approval UX is
  the Flutter app's job.
- `llama-server` traffic is localhost HTTP — do not point it at remote hosts
  without TLS in front.
- `pai sync serve` is plain HTTP and binds 127.0.0.1 by default — relaying
  across networks means putting it behind a TLS-terminating proxy. The
  stored blobs are sealed regardless; the bearer token only prevents the
  relay becoming an anonymous object store.
- Sync pairing authenticity depends on the user moving offer/accept
  files over a channel they control — a substituted file can get a
  *different* device paired (never a forged signature). Compare device
  ids out-of-band.
- Vault rotation exists (`pai sync rotate`) but revocation is
  transport-dependent: a removed peer keeps the *old* vault key and can
  still read everything sealed under it — rotation only stops them
  reading *new* objects, and only once remaining peers have pulled the
  rotation object. A peer that never contacts the transport again keeps
  whatever it already copied. Rotations are signed per-device and
  epoch-gated; any paired member can still force-rotate (the vault is a
  group secret, not a leader-follower model).
- Sync conflict resolution is LWW on `updated_at`; clock skew can pick a
  stale winner. No vector clocks yet.

## Reporting

This project is pre-release; report vulnerabilities privately to the
maintainer (open a security advisory or email the repo owner) rather than a
public issue. Do not include secrets, personal memories, or data-dir
contents in reports.

## Hardening checklist for releases

- [x] encrypt store at rest; [x] keystore-backed device keys; [x] sync
      E2EE implementation; [x] interactive CLI approval; [x] supply-chain:
      lockfile + `cargo audit`/`cargo deny` in CI; [ ] sandboxed tool
      execution profiles; [ ] vault rotation / device revocation.

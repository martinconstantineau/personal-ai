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
  and delete are approval-gated `High` risk tools; the IMAP provider has
  no send verb at all (drafts-first by design). Passwords resolve from
  `PAI_EMAIL_PASSWORD` or the OS keystore (`email:<user>`) — never from
  `email.json`, and `pai_audit::redact` masks them in logs.
- **Vision is local-only.** `vision.describe`/`pai describe` send images
  to a localhost llama.cpp server (data-URI in the request body); the
  tool reads only jailed paths and its output is untrusted model text.
- **Voice is local-only.** STT runs against a `whisper-server` on
  localhost; TTS spawns the local `piper` binary (with a timeout and no
  shell — arguments can't inject commands). No audio leaves the machine.
- **Sync is end-to-end.** Devices pair via an ed25519-signed
  offer/accept exchange; the vault key travels wrapped by an
  ECDH-derived peer key. `SyncObject` payloads are
  XChaCha20-Poly1305-sealed with the object key as AAD — transports and
  shared folders handle ciphertext only. Possession of the vault key is
  read+write access; `pair remove` does not rotate it.

## Known limitations (groundwork stage)

- Approval UX exists in the CLI (`[y/N]` prompt); richer approval UX is
  the Flutter app's job.
- `llama-server` traffic is localhost HTTP — do not point it at remote hosts
  without TLS in front.
- Sync pairing authenticity depends on the user moving offer/accept
  files over a channel they control — a substituted file can get a
  *different* device paired (never a forged signature). Compare device
  ids out-of-band.
- No vault rotation / remote wipe: a removed peer may still hold the
  vault key. Rebuild the vault (fresh key + re-pair) to revoke.
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

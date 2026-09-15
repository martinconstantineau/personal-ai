# Security policy

A local-first personal AI handles your private data, your credentials, and
— eventually — your devices. Security posture is part of the product, not a
compliance checkbox.

## Reporting a vulnerability

Please **do not** file public issues for security problems. Email the
maintainer or open a **confidential issue** on the GitLab project
(“New issue → This issue is confidential”). Include a reproduction and the
affected commit. We aim to acknowledge within 72 hours.

## Supply-chain policy

- `cargo audit` and `cargo deny check advisories licenses bans sources`
  run on every PR via GitHub Actions on the mirror
  (`.github/workflows/ci.yml`, `deny.toml`). GitLab CI is intentionally
  unused so the private GitLab project consumes zero compute minutes.
- Dependencies are crates.io only — no git or path sources (`deny.toml`
  `[sources]`).
- Licenses are permissive-only; see `deny.toml` `[licenses]`.
- The agent's permission engine (`pai-permissions`) is deny-by-default and
  **model output cannot change policy**: `policies` rows are written only by
  the CLI/FFI policy APIs, never by a tool. See ADR 0010 + 0012.

## Signed commits

Commits on `main` are required to be **GPG- or SSH-signed and verified**.
Contributors:

1. Configure signing: `git config commit.gpgsign true` plus either
   `user.signingkey` (GPG) or `gpg.format ssh` + `user.signingkey` pointing
   at `~/.ssh/id_*.pub`.
2. Keep `tag.gpgsign` on for release tags.
3. Maintainers enforce this via GitLab's protected `main` branch plus
   signed-commit push rules where the tier allows; CI (fmt/clippy/test/
   audit/deny + flutter analyze/test) must be green before merge.

Emergency/manual commits by maintainers follow the same rule — no
`--no-verify`, no unsigned pushes.

## Data handling

- Everything lives in `~/.local/share/personal-ai/` (or `--data-dir`);
  nothing leaves the device unless you install a connector.
- **`personal-ai.db` is encrypted at rest** (SQLCipher, AES-256-CBC). The
  256-bit key lives in the OS keystore — Windows Credential Manager, macOS
  Keychain, or Linux Secret Service — falling back to a `store.key` file
  with owner-only permissions where no keystore exists (headless Linux).
  Plaintext databases are migrated on first open; `PAI_PLAINTEXT_STORE=1`
  is the documented escape hatch for debugging/recovery.
- Device Ed25519 signing keys use the same keystore path, recorded in
  `devices.key_storage`.
- Audit log (`pai audit`) records every tool execution, approval decision,
  memory write/delete, and policy change — check it first when something
  looks wrong.

## Threat-model limits (honest)

- At-rest encryption protects the file at rest — not a running process's
  memory, and not against malware running as your user (the OS keystore
  answers any process in your session).
- Tool sandboxing is **in-process**: capability-scoped `ToolContext` plus a
  filesystem jail (`allowed_roots`, default `<data_dir>/inbox`) for
  model-driven file reads. It is not an OS sandbox — real seccomp/
  AppArmor-style isolation needs a subprocess boundary and is V2 work.
- A lost OS-keystore key = a lost database. Keep the `store.key` fallback
  file backed up if you rely on the file path.
- **Sync circles are forward-only**: `circle leave`/key loss stops a
  device applying future circle objects, but anything it already
  decrypted stays decrypted — sharing is irrevocable once read (same as
  telling a person a secret). Circle membership changes never re-key
  existing objects; if a member device is compromised, rotate by
  creating a new circle and re-sharing into it.
- Grants are pairwise-sealed (`ckg/` objects under the recipient's
  X25519 peer key) so only the target device can join — but a malicious
  *member* could relay plaintext out-of-band. Circles bound which
  devices hold keys, not what people do with the content.
- A `circle` argument on `memory.remember`/`memory.share` lets the model
  tag data for federation — it flows through `MemoryWrite` permission +
  audit like any write, but a prompt-injection path could tag content
  into a circle you didn't intend. Set `MemoryWrite` to `AskUser` if
  that matters on your policy.
- The Android manifest allows **cleartext traffic** so LAN sync relays
  (`ws://`/`http://` without TLS) work — relay endpoints remain
  ciphertext-only at the sync layer, but a compromised connector config
  could send plaintext HTTP. Tighten to `networkSecurityConfig` with a
  domain allowlist before shipping broadly.
- **Mesh announcements are signed but visible**: `sync serve --announce`
  multicasts a signed datagram (device id, name, platform, relay port,
  timestamp) to `239.255.71.77:47677` — any LAN host sees that a device
  is present. Forged announcements fail Ed25519 verification against
  `sync_peers` and are dropped. Relay auth uses the pairing-derived
  `hex(peer_key)` bearer — replayable on the LAN, but only a paired
  device can mint it, and the objects it protects are sealed anyway.
  Freshness window (±15 min) bounds replay of stale datagrams.
- **Backup paks are plaintext at rest**: `backups/<app>/<writer>.pak`
  holds app `data/` in cleartext — the same exposure as the live
  `data/` dir itself (app sqlite is unencrypted by design). In transit
  they're vault-sealed like every sync object. Don't point data_dir at
  a synced/cloud folder; a future hardening pass could seal paks too.
- **Restore re-verifies, doesn't re-trust**: a pak's embedded package
  must still signature-verify against own/peer keys at `apps restore`
  time — a tampered backup on disk fails closed rather than installing.
- **Migration is the one inline restore**: a `bkp/` object flagged
  `migrate_to=<me>` restores package + data during pull — an explicit,
  authenticated hand-off from a paired writer whose placement update
  landed first. `migrate_to` naming any other device stores the pak
  without touching live state, and an unpaired/forged writer is
  dropped before the flag is read. Inactive devices refuse `run`,
  `backup`, and `restore` so two devices can't fork live state.
- **Parked app data is plaintext residue**: deactivation renames
  `data/` to `apps/.<id>.data.inactive-<ts>` — recoverable, but the
  same cleartext exposure as live `data/` until manually removed.
- **FFI enforces placement too**: `pai_apps_run` checks
  `active_device` in the export itself — the Flutter shell can't
  accidentally run an app that lives on another device (UI is a
  convenience layer, not the trust boundary).
- **Guest execution is capability-gated**: a device outside the vault
  runs an app only by presenting a `pai apps share` token — an
  Ed25519-signed grant checked for issuer signature, revocation
  tombstone, expiry, and app/action binding before `app_run_op` ever
  runs. `greq/` requests travel unsealed (they carry no vault data —
  only the token, args, and an ephemeral X25519 key) while `gres/`
  replies seal to that ephemeral key, so response bodies are private
  even on a shared transport. Tokens bound with `--for` additionally
  require the request to be signed by the grantee's device key.
  Bearer tokens are exactly that — anyone holding the file may run
  the app until expiry or `apps revoke`.
- **Guest file ops are jailed per app**: `app-read`/`app-write`
  resolve paths under `apps/<id>/` — reads limited to `files/` +
  `data/`, writes to `data/` only, `..`/absolute paths refused, and
  an 8 MiB cap bounds the sync objects they produce. The op itself
  names the required token action (`app-write` needs `write`, not
  `exec`), and bound-token request signatures cover the op field so
  a granted `read` can't be upgraded to `exec` in flight.
- Android `RECORD_AUDIO` is declared for voice capture; the runtime
  grant is still required, and there is no in-app permission-request
  flow yet — voice capture simply fails until granted in system
  settings.

# ADR 0014: At-rest encryption (SQLCipher) + OS-keystore master keys

- **Status**: Accepted
- **Date**: 2026-09-13

## Context

V1 stored everything — conversations, memories, document text, audit —
in a plaintext SQLite file. For a privacy-first local assistant that is
the worst place to leave user data. V1.1 makes at-rest encryption the
default without breaking existing installs.

## Decision

**Encryption**: `rusqlite` built with `sqlcipher` + `bundled-sqlcipher-vendored-openssl`
(SQLCipher 4 / OpenSSL 3, compiled from source — no system dependency).
`Store::open(data_dir, key)` takes an optional 32-byte raw key:

- `Some(k)` → `PRAGMA key = "x'<hex>'"` (raw key; the keystore already
  provides KDF-grade entropy, so PBKDF would be redundant work).
- `None` → plaintext. Only for tests and the `PAI_PLAINTEXT_STORE=1`
  escape hatch.

**Migration**: an existing plaintext `personal-ai.db` is detected by
reading its 16-byte header (`"SQLite format 3\0"`) and re-keyed in place
via `sqlcipher_export`, with a `.plaintext-bak` rotation so a failed swap
can roll back. WAL/SHM sidecars are checkpointed+removed during the swap.

**Keys**: `pai_identity::keystore` generates a 256-bit store key once and
persists it in the OS keystore — Windows Credential Manager, macOS
Keychain, or Linux Secret Service — via the `keyring` crate. Where no OS
backend answers (headless Linux, some CI), it falls back to a
`data_dir/store.key` file created with owner-only permissions. The same
path now protects Ed25519 device signing keys; `devices.key_storage`
(schema v3) records which backend holds each key so migrations/audits can
report it.

**Threat model**: protects data at rest against casual file access and
device loss. It does NOT protect a running process's memory, and on
Windows the Credential Manager key is readable by any process running as
the same user — honest limitations, not bugs.

## Alternatives considered

- **Field-level encryption** (AES-GCM per row): works without SQLCipher's
  toolchain but leaves schema, row counts, and FTS indexes readable, and
  forbids FTS on encrypted fields — kills the search features this phase
  adds.
- **libsodium secretbox for blobs only**: same gap — the interesting data
  is in relational tables, not just blobs.
- **age/passphrase**: requires the user to type a passphrase on every
  launch — breaks background/autostart UX; kept as a future option layered
  under the same `EncryptionKey` interface.

## Consequences

- `Store::open` signature changed — every call site (CLI, FFI, tests)
  threads the keystore key through.
- Vendored OpenSSL adds ~10-15 min to a cold build (cached thereafter);
  CI caches `target/`.
- A lost OS key means a lost database — acceptable for a local-first app
  (same trust domain as the OS login), documented in SECURITY.md.
- `cargo-deny` gains `openssl-src`/`sqlcipher` in the tree; licenses
  (Apache-2.0/OpenSSL) are allowlisted.

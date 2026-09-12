# ADR 0011: Device identity — ed25519, file-backed now, keystore later

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

Sync needs verifiable device identity (who wrote this object?) and, later,
key agreement for E2EE. Private keys must be protectable per-platform.

## Decision

Each device gets an ed25519 keypair generated at registration
(`ed25519-dalek` + OS RNG). Secret keys are written `0600` under
`<data_dir>/keys/` today. The `IdentityStore` API (`sign`, `verify`,
`device_pubkey`) is already keystore-shaped — swapping the file backend for
iOS Keychain / Android Keystore / libsecret is a backend change, not an API
change. X25519 key-agreement conversion is spec'd for the sync E2EE layer.

## Alternatives considered

- **Account-level keys synced via password (e.g. age/passphrase wrap)**:
  better UX for device loss, more code; added later as *recovery*, not
  instead of per-device keys.
- **Hardware-only (Secure Enclave) from day one**: blocks desktop Linux
  parity; file-backed keeps the V1 loop testable everywhere.

## Consequences

- Sync objects carry `device_id` + signature; verification is pure crypto,
  no trust server.
- Key compromise = revoke that `Device` row; audit shows which device wrote
  what.
- Known gap: file keys depend on OS disk encryption until keystore lands
  (ROADMAP V1.1).

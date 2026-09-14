# ADR 0015: E2EE sync — device pairing, shared vault key, sealed objects

- **Status**: Accepted
- **Date**: 2026-09-14

## Context

V2 needs user data (memories first) to converge across the user's own
devices without any server holding plaintext — and without requiring a
server at all. ADR 0007 fixed the transport model: `SyncObject`s are
opaque encrypted blobs moved by pluggable transports, defaulting to a
folder shared via Syncthing/rsync/NFS. What remained open: how devices
come to trust each other and which keys seal the objects.

## Decision

**Two keys per concern.** Each device keeps its Ed25519 *signing* key
(ADR 0011) and gains a separate **X25519 agreement keypair**
(`crypto::agreement_key`), keystore-backed like the signing key under
`agreement:<device>` with a 0600 file fallback. Signing keys never do
ECDH — agreement keys are purpose-built.

**Signed offer/accept pairing.** `pai pair offer` writes a JSON message:
device record + agreement pubkey, Ed25519-signed over a fixed transcript
(`pai-pair-v1` domain, kind, device id, both pubkeys, `vault_sealed`).
`pai pair accept` verifies the signature, records the offerer in a new
`sync_peers` table (schema v4 — a peer row is an explicit trust decision,
kept separate from self-registered `devices`), generates-or-loads the
**vault key**, seals it to the offerer's agreement key via
`HKDF-SHA256(X25519 ECDH)` + XChaCha20-Poly1305, and writes a signed
accept. `pai pair complete` verifies, records the acceptor, and adopts
the vault. Two file hops over any channel the user controls.

**Shared 256-bit vault key.** All paired devices hold the same vault key;
`SyncObject.ciphertext` = `v1 || nonce24 || XChaCha20-Poly1305(payload)`
with the **object key as AAD**, so transport attackers cannot rename or
replay objects under different paths. The vault key is namespaced per
`data_dir` in the keystore (`sync-vault:<dir-hash>`) so multiple installs
on one OS account stay isolated.

**Sync scope**: `memories` rows with `sync_scope='synchronized'` (the
default) become `memory/<uuid>` objects carrying a versioned JSON
payload; soft-deleted rows become sealed tombstones. `sync_objects`
mirrors every written/applied object so pulls don't echo back as pushes.
Merge is **LWW on the source row's `updated_at`** (object version =
source `updated_at` epoch-ms).

## Consequences

- **Transport sees ciphertext only.** Verified in tests: no plaintext
  bytes appear in `.syncobj` files.
- **Possession of the vault key = read + write access to every object.**
  `pair remove` does NOT rotate — a removed peer may retain the key.
  Vault rotation and per-peer revocation are roadmap items.
- **Pairing security rests on the file channel.** An attacker who can
  substitute the offer/accept files can't forge signatures but CAN get
  the user to pair *their* device (a MITM offer swap). The CLI prints
  device names + ids; out-of-band confirmation is the user's job, same
  as Signal's safety numbers. QR-code compare is future UX.
- **Clock skew weakens LWW.** A device with a fast clock wins conflicts;
  objects carry `writer` for auditability but no vector clocks. CRDT
  merge remains roadmap for rich types.
- **Conversations, documents, tasks, audit are not yet synced** —
  `tasks`/`memories` already carry `sync_scope`; widening scope is a
  payload-definition exercise, not a crypto change.
- **No remote wipe**: a stolen paired device keeps vault access until
  the vault is rebuilt on remaining devices (manual reset documented).

## Alternatives rejected

- *Per-pair keys (double-ratchet style)*: forward secrecy is real value
  but the complexity is wrong for trusted-device sync of user data —
  peers are equally trusted endpoints. Revisit if we ever sync with
  less-trusted relays pushing signed updates.
- *Ed25519→X25519 key conversion*: avoids a second keypair, but couples
  signing and agreement security; separate keys are cleaner and cheap.
- *Relay server transport*: the folder transport already satisfies the
  zero-server goal; a ciphertext-only relay slots behind `SyncTransport`
  later without touching crypto.

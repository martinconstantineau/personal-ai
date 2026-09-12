# ADR 0007: Sync as E2EE objects over dumb transports

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

Multi-device continuity without a trusted server: any relay or folder must
only ever see ciphertext.

## Decision

State syncs as `SyncObject`s — `(id, kind, lamport clock, device_id,
ciphertext, signature)` — through a `SyncTransport` trait
(`list/put/get/delete`). `FolderTransport` implements it over a directory,
making Syncthing/iCloud/rsync usable transports. Merge is last-writer-wins
on `(lamport, device_id)`; richer CRDT kinds are encoded in `kind`.

## Alternatives considered

- **A bespoke sync server with plaintext**: violates the privacy premise.
- **CRDT-everything (automerge)**: right destination for collaborative text,
  wrong default for personal state — LWW covers V1 with far less machinery.
- **Cloud vendor sync (iCloud KVS etc.)**: per-OS lock-in, can't span
  Android↔Linux.

## Consequences

- Transports are trivially replaceable; adding a relay is a weekend of work.
- Deletes are tombstone objects; `SyncApplied` audit events make remote
  changes visible.
- Encryption-at-object level still to be wired end-to-end (see ROADMAP V2) —
  the schema reserves `ciphertext` and never exposes plaintext to transports.

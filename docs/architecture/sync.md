# Sync

Goal: a user's devices form one logical system — conversations, memories,
and settings converge — **without any server ever seeing plaintext**.

## Model

State syncs as `SyncObject`s:

```
SyncObject {
  id, kind,            // e.g. "memory/<uuid>", "config/compute_policy"
  device_id, lamport,  // writer + logical clock → deterministic order
  updated_at,
  ciphertext,          // opaque payload (plaintext fields only in dev mode)
  signature            // ed25519 over the canonical fields
}
```

- **Encryption**: objects are sealed for the user's device set before they
  hit a transport (X25519 key agreement derived from the ed25519 device
  keys; per-object ephemeral sender — lands with the relay transport, the
  `ciphertext` field is already the only thing transports touch).
- **Merge**: `merge_lww` — last-writer-wins on `(lamport, device_id)` for
  this stage; CRDT-rich kinds (counters, sets) are declared in `kind` so the
  merge strategy can grow per object class.
- **Tombstones**: deletes are objects with an empty payload + a deleted
  marker, propagated like everything else.

## Transports

```rust
trait SyncTransport {
    fn put(&self, obj: &SyncObject) -> Result<()>;
    fn get(&self, id: &str) -> Result<Option<SyncObject>>;
    fn list(&self, prefix: &str) -> Result<Vec<SyncObject>>;
    fn delete(&self, id: &str) -> Result<()>;
}
```

- **`FolderTransport`** (implemented): one `<urlenc-id>.syncobj` JSON file
  per object in a directory — works with Syncthing, iCloud Drive,
  Nextcloud, a mounted phone, or plain `rsync`. This is the debugging and
  power-user transport: sync state is inspectable files.
- **Relay transport** (V2): a dumb append/fetch server; stores ciphertext
  only, can't correlate contents (ids are already encrypted
  deterministically in the design — final format lands with the impl).

## Consistency & conflicts

- Per-object LWW for `V1`; the audit log records every applied remote change
  (`SyncApplied` events) so divergence is observable.
- Memories and conversations have object kinds that permit field-level merge
  later without a transport change.

## What sync deliberately excludes

- No live presence/collaborative editing — this is personal-data sync, not a
  multiplayer editor.
- No global clock dependence — `lamport` + `device_id` order events;
  `updated_at` is display-only.

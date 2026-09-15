# ADR 0018: App backups as sealed `bkp/` objects — store, don't auto-restore

- **Status**: Accepted
- **Date**: 2026-09-15

## Context

V4d made app packages roam (`app/<id>` objects) but deliberately kept
live `data/` device-local — runtime state must never travel as package
content. That left a real gap: app state had *no* off-device copy, so a
lost device meant lost data. The PRD asks for versioned backups
restorable to a new device plus a rescue path.

## Decision

`pai apps backup <id>` snapshots the installed package **and** the live
`data/` subtree into `backups/<app>/<writer>.pak`, records an
`app_backups` row (schema v11), and the sync engine ships it as a
sealed `bkp/<app>/<writer>` object — same vault seal, same transports,
same LWW version field as every other kind.

Four rules keep it safe:

1. **Apply stores, never restores.** A received `bkp/` object lands as
   a pak file + registry row. Nothing touches `apps/` — a stale or
   hostile backup can't silently roll back a live app.
2. **Only the writer pushes.** Rows carry a `writer` segment;
   `push_backups` filters to `writer == self`, so a device never
   re-seals a peer's snapshot as its own. Received backups exist for
   restore, not for re-sharing.
3. **Restore re-verifies.** `pai apps restore` runs the embedded
   package through the same stage → `verify_any_key` → `install_trusted`
   path as synced `app/` objects, then swaps `data/` with the same
   move-aside/rollback semantics upgrades use. A tampered pak on disk
   fails closed.
4. **Writer consistency.** The key segment, `SyncObject.writer`, and
   payload `writer` must all agree and name a currently-paired peer —
   mismatches are permanent drops, not errors.

The payload embeds the whole package, so a pak is self-contained: a
device that lost the install (or never had it) restores package + data
from the backup alone — the rescue path.

## Alternatives considered

- **Sync `data/` as regular `app/` objects**: rejected in V4d — runtime
  state isn't signed package content and would churn the object on
  every write. Backups are explicit snapshots, not live mirroring.
- **Backup to a standalone export file**: works, but a file doesn't
  roam — the whole point is off-device copies on paired hardware via
  the transport we already have.
- **Auto-restore newest on pull**: dangerous — any peer could roll a
  live app back by shipping a backup. Restore must be a conscious act.
- **`bkp/<app>` without the writer segment**: two devices backing up
  the same app would fight over one LWW slot. Per-writer keys keep
  each device's snapshots independent.
- **Encrypted paks at rest**: the pak holds what `data/` holds —
  plaintext sqlite by design, so sealing it locally buys nothing.
  Documented in SECURITY.md; sealing later is a compatible change.

## Consequences

### Positive

- App state finally has an off-device copy; losing the device that ran
  an app no longer loses its data.
- Restore doubles as rescue and as "move the app to another device"
  (a primitive placement operation).
- Deletion propagates via tombstones (`apps backup-delete`), so old
  snapshots don't linger on peers forever.

### Negative

- Whole-snapshot LWW: every backup ships the full `data/` again. Fine
  for the small apps this targets; a chunked/incremental format is
  future work if app state grows.
- A backup is a point-in-time copy — restoring discards newer live
  state. Mitigated by the move-aside rescue dir, which survives until
  the swap succeeds.
- Backups of an app a device doesn't run are just storage cost —
  they accumulate until tombstoned.

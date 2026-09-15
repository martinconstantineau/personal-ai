# ADR 0019: App placement — one `active_device`, migration via flagged backups

- **Status**: Accepted
- **Date**: 2026-09-15

## Context

V4d made app packages roam to every paired device and V4h made state
travel as explicit backups. What was missing: *which* device actually
runs an app. Before V4j every device that installed a package could
grow its own `data/` — fine for per-device apps, wrong for apps meant
to live in one place (a notes db, a home-automation state). Two
devices writing divergent `data/` is a silent fork that no sync kind
can merge — the payload is opaque app state.

## Decision

`apps.active_device` (schema v12) names the single device that owns
live `data/`. `NULL` keeps legacy "runs everywhere" semantics — no
migration is forced on existing installs.

`pai apps migrate <id> --to <peer>` moves the app:

1. **Snapshot while still active** — `backup::create` refuses on an
   inactive app, so the ordering is create → update row → deactivate.
   The pak is flagged `migrate_to=<target>`.
2. **`UPDATE apps SET active_device=<target>`** — the next `app/<id>`
   object carries placement to every peer.
3. **`deactivate_data`** — the source's `data/` is *renamed* to
   `apps/.<id>.data.inactive-<ts>`, not deleted: recoverable if the
   migrate is cancelled, but no longer live.

Pull applies the pair in rank order — `app/` (8) before `bkp/` (9):

- **Every device** upserts `active_device` and installs/updates the
  package (distribution stays global).
- **Non-target devices** with stale live `data/` park it — a device
  that was active under `NULL` semantics cleanly stops being an
  instance.
- **The target** sees `migrate_to=me` on the `bkp/` object and
  restores inline — the *one* exception to ADR-0018's "apply never
  restores" rule, because the flag is an explicit, authenticated
  request from a paired writer and placement already landed.

## Guards against divergent instances

- `apps run` (local **and** via broker `app-run`), `apps backup`, and
  `apps restore` all refuse when `active_device` names another device.
- `migrate_to` naming a device other than the puller never restores —
  third parties just store the pak.
- An unpaired/forged writer is dropped before `migrate_to` is even
  consulted (the same writer-consistency check as backups).
- Migration round-trips work: B can migrate back to A; each direction
  is a fresh flagged snapshot.

## Consequences

- Exactly one live instance per placed app; unplaced apps unchanged.
- Placement is LWW on `updated_at` like every other object — a
  device that misses an `app/` update heals on the next pull.
- Parked `.inactive-*` dirs are user-recoverable debris, not garbage —
  documented rather than auto-merged (merging divergent app state is
  app-specific and out of scope).
- A migration where the target's inline restore fails still leaves
  the pak stored — `apps restore` on the target (which is now the
  active device, so the guard passes) completes the move manually.

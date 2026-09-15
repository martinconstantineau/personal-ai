# ADR 0017: Portable model packs on removable storage

- **Status**: Accepted
- **Date**: 2026-09-15

## Context

Model weights are the largest files pai handles (hundreds of MB to tens
of GB). Users asked to keep them off the system drive entirely — on a
flash drive or external disk — and have any pai device recognize them
on plug-in without re-downloading or re-registering. Two gaps blocked
this:

- `ModelManager::install` always downloaded into `data_dir/models/` and
  the `models.path` column was the only record of where a file lived.
- Nothing described *which* models a directory held, so a foreign device
  seeing `E:\models\qwen.gguf` couldn't map it to a registry entry.

## Decision

**Every model directory is a self-describing "pack".** `install_to`
(download or local copy into a target dir) upserts an `index.json` in
that directory listing the `ModelManifest` of each file it contains —
slug, filename, sha256, capabilities, license. A pack is just a folder:
`pai-models/` under a drive root, a synced folder, a network share.

**Discovery is convention, not a driver.** `pack_roots()` probes
`pai-models/` under every mounted volume — `A:`–`Z:` on Windows,
`/Volumes`, `/media`, `/mnt`, `/run/media` (one and two levels deep) on
unix. No OS drive-type APIs: a fixed disk holding a `pai-models/` folder
is a valid pack too, and `is_dir()` probes on absent letters are cheap.

**Adoption is verify-then-register.** `scan()`/`scan_roots()` reads each
pack's `index.json`, checks the file exists, streams a sha256 against
the manifest pin when present, then `register` (idempotent) +
`installed=1, path=<found>` — including `hf://` manifests this device
never saw. Packs without an index fall back to filename-matching the
built-in catalog. Mismatched files are skipped, never adopted.

**Plug-and-play is lazy, not evented.** `locate(slug)` returns the
recorded path if the file exists; otherwise it runs one `scan()` and
re-reads. `serve`, `runnable`, and any other consumer go through
`locate`/existence checks, so a drive that arrives as a different
letter — or on a different device — just works at use time. No OS
mount-watch subscription needed. `models list` marks installed-but-missing
paths as `(offline — last at …)` rather than hiding them.

## Alternatives considered

- **OS mount notifications / drive-type APIs** (`GetDriveType`, udev,
  NSWorkspace): real "plug" events, but four platform code paths for a
  problem a directory probe solves in ~20 lines. Lazy rescan covers the
  same cases, including packs created on *other* machines.
- **Registry-only installs + `PATH`-style search dirs**: a config list
  of model dirs would need writes on every new drive and still wouldn't
  tell a fresh device what a file *is*. `index.json` carries the
  manifest itself — the pack is the record.
- **Sync models as sealed `SyncObject`s** (like app packages): weights
  are huge and immutable; shipping them through the sync engine would
  balloon every transport. Sneakernet packs are the right transport for
  this size class. Synced *metadata* (which models exist) can ride sync
  later.

## Consequences

- `pai models install <slug> --to <dir>` writes the pack; a drive root
  (`E:\`) is normalized to `E:\pai-models` via `pack_dir_for`.
- `pai models scan` adopts anything verifiable on mounted volumes.
- `uninstall` drops the slug from the pack's `index.json`; a file left
  behind on an unplugged drive will be re-adopted on next scan (it *is*
  still there — honest behavior; delete the file to be rid of it).
- sha256 is verified on adopt, on copy, and on download. Unpinned
  catalog entries record the digest observed at first install.
- Cost: one streaming hash per adopted file per device (~40 s for 4 GB
  over USB3) — paid once, not per `scan`.

## Validation checklist

- [x] `place`/`copy_to`/`install_to` all write `index.json`
- [x] `scan_roots` adopts indexed packs on a device that never
      registered the model
- [x] sha256 mismatch → file skipped, never installed
- [x] `locate` repoints a recorded path after the pack reappears
      elsewhere
- [x] unplugged drive → `list` shows `offline`, `runnable` drops it
- [x] verified live: `install --to` → fresh device `scan` → adopted →
      `subst /d` → `offline` → re-`subst` → online

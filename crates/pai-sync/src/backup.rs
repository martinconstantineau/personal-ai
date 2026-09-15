//! App backups — `bkp/<app>/<writer>` sealed objects carrying a full
//! snapshot (package + live `data/`), so app state — which deliberately
//! never syncs under `app/` — still reaches paired devices as
//! restorable ciphertext.
//!
//! Applying a backup only *stores* it under `backups/<app>/<writer>.pak`
//! and records an `app_backups` row — it never touches the live install
//! in `apps/`. `pai apps restore` is the explicit act of putting that
//! state back. Only self-written rows push; received backups are
//! mirrored for restore but never re-sealed (a device must not echo
//! someone else's snapshot as its own).

use crate::{engine, pair};
use base64::Engine as _;
use pai_core::*;
use pai_storage::{store_err, ts, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One `bkp/` payload: the whole package (self-contained — a fresh
/// device can restore without the `app/` object) plus the live `data/`
/// subtree. `writer` is the snapshotting device id and must match the
/// object key segment and the `SyncObject` writer at apply time.
#[derive(Debug, Serialize, Deserialize)]
pub struct BackupPayload {
    pub v: u8,
    pub app_id: String,
    pub writer: String,
    pub created_at: String,
    pub package: engine::AppPayload,
    /// Files under `data/` — paths relative to that dir.
    pub data_files: Vec<engine::AppFileEntry>,
    /// When set, this backup is a *migration*: the named device is the
    /// app's new home and restores it on apply — the one case a backup
    /// touches live state without a manual `apps restore`.
    #[serde(default)]
    pub migrate_to: Option<String>,
}

/// An `app_backups` row — local snapshots and received ones.
pub struct BackupRow {
    pub app_id: String,
    pub writer: String,
    pub created_at: String,
    pub path: String,
    pub pushed_at: Option<String>,
    pub deleted: bool,
}

/// Snapshot an installed app — package + live `data/` — into
/// `backups/<app>/<writer>.pak` and record the row. `sync push` ships
/// it; re-running replaces the snapshot (new `created_at` bumps the
/// object version, so peers take the newer one).
pub fn create(
    store: &Arc<Store>,
    data_dir: &Path,
    device: DeviceId,
    app_id: &str,
    migrate_to: Option<DeviceId>,
) -> Result<PathBuf> {
    // Snapshotting an inactive instance would capture stale state —
    // backups and migrations only make sense from the active device.
    if let Some(other) = active_elsewhere(store, device, app_id)? {
        return Err(Error::Sync(format!(
            "app {app_id} is active on {other} — migrate it here first"
        )));
    }
    let dir = data_dir.join("apps").join(app_id);
    if !dir.is_dir() {
        return Err(Error::NotFound(format!("app {app_id} not installed")));
    }
    let pkg = pai_apps::AppPackage::load(&dir)
        .map_err(|e| Error::Sync(format!("load app for backup: {e}")))?;
    let m = &pkg.manifest;
    let now = ts(&now());
    // The pak carries placement-at-snapshot so a restore upserts the
    // same active_device rather than clobbering a migration's target.
    let active: Option<String> = store.with_conn(|c| {
        Ok(c.query_row(
            "SELECT active_device FROM apps WHERE id=?1",
            params![app_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten())
    })?;
    let package = engine::app_payload_for(
        &dir,
        app_id,
        &m.app.name,
        &m.app.version,
        &format!("{:?}", m.app.runtime).to_lowercase(),
        &now,
        active.as_deref(),
    )?;
    let data_files = pai_apps::AppRegistry::new(data_dir)
        .snapshot_data(app_id)
        .map_err(|e| Error::Sync(format!("snapshot data: {e}")))?
        .into_iter()
        .map(|(p, b)| engine::AppFileEntry {
            path: p,
            b64: base64::engine::general_purpose::STANDARD.encode(b),
        })
        .collect();
    let payload = BackupPayload {
        v: 1,
        app_id: app_id.into(),
        writer: device.to_string(),
        created_at: now.clone(),
        package,
        data_files,
        migrate_to: migrate_to.map(|d| d.to_string()),
    };
    let rel = format!("backups/{app_id}/{device}.pak");
    let abs = data_dir.join(&rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(store_err)?;
    }
    std::fs::write(
        &abs,
        serde_json::to_vec(&payload).map_err(|e| Error::Sync(e.to_string()))?,
    )
    .map_err(store_err)?;
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO app_backups(app_id, writer, created_at, path,
                pushed_at, deleted) VALUES(?1,?2,?3,?4,NULL,0)
             ON CONFLICT(app_id, writer) DO UPDATE SET
                created_at=excluded.created_at, path=excluded.path,
                pushed_at=NULL, deleted=0",
            params![app_id, device.to_string(), now, rel],
        )?;
        Ok(())
    })?;
    Ok(abs)
}

/// Every known backup row — own snapshots plus ones received from
/// peers — newest first within each app.
pub fn list(store: &Arc<Store>) -> Result<Vec<BackupRow>> {
    store.with_conn(|c| {
        let mut s = c.prepare(
            "SELECT app_id, writer, created_at, path, pushed_at, deleted
             FROM app_backups ORDER BY app_id, created_at DESC",
        )?;
        let rows = s.query_map([], |r| {
            Ok(BackupRow {
                app_id: r.get(0)?,
                writer: r.get(1)?,
                created_at: r.get(2)?,
                path: r.get(3)?,
                pushed_at: r.get(4)?,
                deleted: r.get::<_, i64>(5)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
}

/// Tombstone my backup for `app_id` — the next push ships the deletion
/// so peers drop their copy too. Returns false when I have none.
pub fn delete(store: &Arc<Store>, data_dir: &Path, device: DeviceId, app_id: &str) -> Result<bool> {
    let me = device.to_string();
    let path: Option<String> = store.with_conn(|c| {
        Ok(c.query_row(
            "SELECT path FROM app_backups
             WHERE app_id=?1 AND writer=?2 AND deleted=0",
            params![app_id, me],
            |r| r.get(0),
        )
        .ok())
    })?;
    let Some(rel) = path else { return Ok(false) };
    store.with_conn(|c| {
        // Bump created_at to deletion time — the row doubles as the LWW
        // version, so the tombstone must sort newer than the live object
        // it replaces or peers will skip it as already-seen.
        c.execute(
            "UPDATE app_backups SET deleted=1, created_at=?3
             WHERE app_id=?1 AND writer=?2",
            params![app_id, me, ts(&now())],
        )?;
        Ok(())
    })?;
    let _ = std::fs::remove_file(data_dir.join(&rel));
    Ok(true)
}

/// Restore an app from a backup: stage + verify + install the embedded
/// package through the same trusted path `app/` objects use, upsert the
/// `apps` row, then swap `data/` for the snapshot's — exact state at
/// backup time, with rollback if the write fails midway. `from` is a
/// writer device-id prefix; default picks the newest backup.
pub fn restore(
    store: &Arc<Store>,
    data_dir: &Path,
    device: DeviceId,
    app_id: &str,
    from: Option<&str>,
) -> Result<BackupPayload> {
    // Restoring on an inactive device would fork live state.
    if let Some(other) = active_elsewhere(store, device, app_id)? {
        return Err(Error::Sync(format!(
            "app {app_id} is active on {other} — migrate it here first"
        )));
    }
    restore_unchecked(store, data_dir, app_id, from)
}

/// Restore without the placement guard — used by migration apply,
/// where `active_device == me` was already landed by the `app/`
/// object earlier in the same pull batch.
pub(crate) fn restore_unchecked(
    store: &Arc<Store>,
    data_dir: &Path,
    app_id: &str,
    from: Option<&str>,
) -> Result<BackupPayload> {
    let row = store.with_conn(|c| {
        let pick = |r: &rusqlite::Row| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?));
        let fetched = match from {
            Some(f) => c.query_row(
                "SELECT writer, path FROM app_backups
                 WHERE app_id=?1 AND deleted=0 AND writer LIKE ?2
                 ORDER BY created_at DESC LIMIT 1",
                params![app_id, format!("{f}%")],
                pick,
            ),
            None => c.query_row(
                "SELECT writer, path FROM app_backups
                 WHERE app_id=?1 AND deleted=0
                 ORDER BY created_at DESC LIMIT 1",
                params![app_id],
                pick,
            ),
        };
        Ok(fetched.ok())
    })?;
    let Some((_writer, rel)) = row else {
        return Err(Error::NotFound(format!("no backup for {app_id}")));
    };
    let raw = std::fs::read(data_dir.join(&rel)).map_err(store_err)?;
    let p: BackupPayload =
        serde_json::from_slice(&raw).map_err(|e| Error::Sync(format!("bad backup pak: {e}")))?;
    if p.v != 1 {
        return Err(Error::Sync(format!("unsupported backup v{}", p.v)));
    }
    if p.app_id != app_id {
        return Err(Error::Sync(format!(
            "backup app mismatch: row says {app_id}, payload says {}",
            p.app_id
        )));
    }
    // The embedded package verifies against the same trusted key set as
    // a synced `app/` object — a pak that fails verification is never
    // installed, no matter how it arrived.
    match engine::stage_verify_install(data_dir, store, &p.package, app_id)? {
        engine::StageOutcome::Rejected(e) => {
            return Err(Error::Sync(format!(
                "backup package failed verification: {e}"
            )));
        }
        engine::StageOutcome::Installed(_) => {}
    }
    // Restore the public OAuth config too — a rescued app should know
    // which providers it expects; the device re-auths for tokens.
    if let Some(auth) = &p.package.auth {
        if let Err(e) = auth.save(data_dir, app_id) {
            tracing::warn!(app = %app_id, "auth.json restore: {e}");
        }
    }
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO apps(id, name, version, runtime, installed_at,
                updated_at, deleted, active_device)
                VALUES(?1,?2,?3,?4,?5,?6,0,?7)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name,
                version=excluded.version, runtime=excluded.runtime,
                updated_at=excluded.updated_at, deleted=0",
            // active_device is in the INSERT (fresh row from the pak's
            // snapshot) but NOT the UPDATE — an existing row's
            // placement is more current than the backup's.
            params![
                app_id,
                p.package.name,
                p.package.version,
                p.package.runtime,
                ts(&now()),
                p.package.updated_at,
                p.package.active_device
            ],
        )?;
        Ok(())
    })?;
    let files: Vec<(String, Vec<u8>)> = p
        .data_files
        .iter()
        .map(|f| {
            Ok((
                f.path.clone(),
                base64::engine::general_purpose::STANDARD
                    .decode(&f.b64)
                    .map_err(|e| Error::Sync(format!("bad backup file b64: {e}")))?,
            ))
        })
        .collect::<Result<_>>()?;
    pai_apps::AppRegistry::new(data_dir)
        .restore_data(app_id, &files)
        .map_err(|e| Error::Sync(format!("restore data: {e}")))?;
    Ok(p)
}

/// Store a received backup pak + row — never touches the live install.
/// `writer` is the key segment; key, object writer, and payload must
/// all agree and name a paired peer, else the object is dropped (a
/// crafted or stale-writer backup is permanent rejection, not an
/// error — pull continues).
pub(crate) fn apply(
    store: &Arc<Store>,
    data_dir: &Path,
    me: DeviceId,
    app_id: &str,
    writer: &str,
    obj_writer: DeviceId,
    p: &BackupPayload,
) -> Result<()> {
    if p.v != 1 {
        return Err(Error::Sync(format!("unsupported backup v{}", p.v)));
    }
    if p.app_id != app_id || p.writer != writer || obj_writer.to_string() != writer {
        tracing::warn!(app = %app_id, "backup writer/app mismatch — dropped");
        return Ok(());
    }
    if !is_paired(store, writer)? {
        tracing::warn!(app = %app_id, writer = %writer, "backup from unpaired writer — dropped");
        return Ok(());
    }
    // Both segments land in a path — keep them inside backups/.
    for seg in [app_id, writer] {
        pai_apps::check_rel_path(seg).map_err(|e| Error::Sync(format!("backup path: {e}")))?;
    }
    let rel = format!("backups/{app_id}/{writer}.pak");
    let abs = data_dir.join(&rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(store_err)?;
    }
    std::fs::write(
        &abs,
        serde_json::to_vec(p).map_err(|e| Error::Sync(e.to_string()))?,
    )
    .map_err(store_err)?;
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO app_backups(app_id, writer, created_at, path,
                pushed_at, deleted) VALUES(?1,?2,?3,?4,?5,0)
             ON CONFLICT(app_id, writer) DO UPDATE SET
                created_at=excluded.created_at, path=excluded.path,
                pushed_at=excluded.pushed_at, deleted=0",
            // pushed_at = created_at marks "already shipped" — a
            // received backup must never re-seal and echo back out.
            params![app_id, writer, p.created_at, rel, p.created_at],
        )?;
        Ok(())
    })?;
    // Migration flag: this device is the app's new home. The `app/`
    // object carrying active_device=me applied earlier in this pull
    // batch (rank ordering), so restoring here is the explicit,
    // requested move — a routine backup (no flag) never restores.
    if p.migrate_to.as_deref() == Some(me.to_string().as_str()) {
        match restore_unchecked(store, data_dir, app_id, Some(writer)) {
            Ok(_) => tracing::info!(
                app = %app_id,
                writer = %writer,
                "app migrated in — restored package + data"
            ),
            Err(e) => tracing::warn!(
                app = %app_id,
                "migration restore failed (pak stored for manual restore): {e}"
            ),
        }
    } else {
        tracing::info!(app = %app_id, writer = %writer, "stored app backup");
    }
    Ok(())
}

/// Backup tombstone: drop the local pak and mark the row deleted.
/// Same writer-pairing check — a stale writer's tombstone still
/// applies (the data it deletes was legitimately received earlier).
pub(crate) fn apply_tombstone(
    store: &Arc<Store>,
    data_dir: &Path,
    app_id: &str,
    writer: &str,
) -> Result<()> {
    store.with_conn(|c| {
        c.execute(
            "UPDATE app_backups SET deleted=1 WHERE app_id=?1 AND writer=?2",
            params![app_id, writer],
        )?;
        Ok(())
    })?;
    let _ = std::fs::remove_file(data_dir.join(format!("backups/{app_id}/{writer}.pak")));
    Ok(())
}

/// `Some(other)` when the apps row names a different active device —
/// the signal that backing up / restoring / running on this device
/// would fork live state. `None` = legacy (unplaced) or active-here.
/// What happened to one app during a rescue.
#[derive(Debug)]
pub enum RescueOutcome {
    /// Placement claimed + the newest backup's data restored.
    Restored { writer: String, created_at: String },
    /// Placement claimed; no backup existed — the app is a fresh
    /// install here (its data died with the old device).
    ClaimedNoBackup,
    /// Placement claimed but the backup restore failed — the claim
    /// stands; retry data with `apps restore`.
    ClaimedRestoreFailed(String),
}

/// Rescue an app whose home device is dead/lost: claim `active_device`
/// here, then restore the newest backup's data. Migration can't help —
/// the old device isn't running to snapshot-and-hand-off — so the user
/// asserts takeover. The claim propagates via the `app/` object on next
/// push; if the old device returns, its pull parks its stale `data/`
/// (ADR-0019), bounding the fork window.
pub fn rescue(
    store: &Arc<Store>,
    data_dir: &Path,
    device: DeviceId,
    app_id: &str,
) -> Result<RescueOutcome> {
    claim_active(store, device, app_id)?;
    match restore(store, data_dir, device, app_id, None) {
        Ok(p) => Ok(RescueOutcome::Restored {
            writer: p.writer,
            created_at: p.created_at,
        }),
        Err(Error::NotFound(_)) => Ok(RescueOutcome::ClaimedNoBackup),
        Err(e) => Ok(RescueOutcome::ClaimedRestoreFailed(e.to_string())),
    }
}

/// Bulk rescue: every app whose `active_device` names `from` — the
/// dead/lost device — is claimed here and restored where a backup
/// exists. Apps with no placement (active_device NULL) aren't "on"
/// that device and are skipped; rescue them individually.
pub fn rescue_from(
    store: &Arc<Store>,
    data_dir: &Path,
    device: DeviceId,
    from: DeviceId,
) -> Result<Vec<(String, RescueOutcome)>> {
    if from == device {
        return Err(Error::InvalidInput(
            "rescue --from must name a different device".into(),
        ));
    }
    let apps: Vec<String> = store.with_conn(|c| {
        let mut st =
            c.prepare("SELECT id FROM apps WHERE deleted=0 AND active_device=?1 ORDER BY id")?;
        let rows = st
            .query_map(params![from.to_string()], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    apps.iter()
        .map(|id| rescue(store, data_dir, device, id).map(|o| (id.clone(), o)))
        .collect()
}

/// Point `active_device` at this device — the claim side of rescue.
fn claim_active(store: &Arc<Store>, device: DeviceId, app_id: &str) -> Result<()> {
    let n = store.with_conn(|c| {
        c.execute(
            "UPDATE apps SET active_device=?2, updated_at=?3 WHERE id=?1 AND deleted=0",
            params![app_id, device.to_string(), ts(&now())],
        )
    })?;
    if n == 0 {
        return Err(Error::NotFound(format!("app {app_id} not installed")));
    }
    Ok(())
}

pub fn active_elsewhere(
    store: &Arc<Store>,
    device: DeviceId,
    app_id: &str,
) -> Result<Option<String>> {
    let active: Option<String> = store.with_conn(|c| {
        Ok(c.query_row(
            "SELECT active_device FROM apps WHERE id=?1",
            params![app_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten())
    })?;
    Ok(active.filter(|a| *a != device.to_string()))
}

pub(crate) fn is_paired(store: &Arc<Store>, writer: &str) -> Result<bool> {
    Ok(pair::list_peers(store)?
        .iter()
        .any(|p| p.device_id.to_string() == writer))
}

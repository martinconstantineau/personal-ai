//! App CRDT documents — `data/crdt/<doc>.json` under an installed app
//! is a shared-mutable JSON document: every device that writes it
//! publishes its field-set as an `acrdt/<app>/<doc>/<writer>` sealed
//! object, and every peer converges by merging cells field-wise.
//!
//! Merge rule (LWW-map): each cell carries a ms timestamp; a field's
//! winning cell is `max(t_ms, writer-asc)`. Tombstone cells
//! (`{"d":true,"t":…}`) win the same way and remove the field. A doc
//! file that disappears withdraws that writer's whole field-set (an
//! empty `fields` object). App-facing files stay plain JSON — the
//! `{v,t}` cell encoding lives on the wire and in `app_crdt_cells`.
//!
//! Explicit cells: an app may write `{"field": {"v": X, "t": ms}}` or
//! `{"field": {"d": true, "t": ms}}` to assert its own timestamps;
//! plain values are timestamped by provenance (the `app_crdt_view`
//! row's ts when the value is unchanged, else the file mtime). This
//! keeps re-publishing honest: a merged field re-ships at its observed
//! ts, so writers can't dominate by echoing fresh mtimes.
//!
//! These docs are multi-master by design and exempt from the
//! `active_device` single-writer data model — placement governs where
//! an app *runs*; `crdt/` state converges everywhere.

use pai_core::*;
use pai_storage::Store;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// One field cell — `{"v": <value>, "t": ms}` or `{"d": true, "t": ms}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cell {
    Value { v: serde_json::Value, t: i64 },
    Tomb { d: bool, t: i64 },
}

impl Cell {
    fn t(&self) -> i64 {
        match self {
            Cell::Value { t, .. } | Cell::Tomb { t, .. } => *t,
        }
    }
    fn is_tomb(&self) -> bool {
        matches!(self, Cell::Tomb { d, .. } if *d)
    }
    fn value(&self) -> Option<&serde_json::Value> {
        match self {
            Cell::Value { v, .. } => Some(v),
            Cell::Tomb { .. } => None,
        }
    }
}

/// `acrdt/<app>/<doc>/<writer>` payload — the writer's complete
/// field-set for the doc (apply replaces the writer's rows wholesale).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtDocPayload {
    pub v: u8,
    pub app: String,
    pub doc: String,
    pub writer: String,
    pub fields: BTreeMap<String, Cell>,
    pub updated_at: String,
}

fn store_err(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

fn crdt_dir(data_dir: &Path, app_id: &str) -> std::path::PathBuf {
    data_dir.join("apps").join(app_id).join("data").join("crdt")
}

/// Doc-file name → doc id (strip `.json`, refuse nested/escaped names).
fn doc_name(file: &Path) -> Option<&str> {
    let name = file.file_stem()?.to_str()?;
    (file.extension().and_then(|e| e.to_str()) == Some("json")
        && !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.starts_with('.'))
    .then_some(name)
}

/// A field value that is exactly `{"v": …, "t": int}` or
/// `{"d": true, "t": int}` parses as a cell; anything else is a plain
/// value needing a provenance timestamp.
fn as_cell(raw: &serde_json::Value) -> Option<Cell> {
    serde_json::from_value(raw.clone())
        .ok()
        .filter(|c| match c {
            Cell::Tomb { d, .. } => *d,
            Cell::Value { .. } => true,
        })
}

/// Read + normalize one on-disk doc into a writer field-set.
/// `view` holds last-materialized ts per field for value-unchanged
/// provenance; `mtime_ms` timestamps fields that changed.
fn collect_doc(
    data_dir: &Path,
    store: &Arc<Store>,
    app_id: &str,
    doc: &str,
) -> Result<Option<BTreeMap<String, Cell>>> {
    let path = crdt_dir(data_dir, app_id).join(format!("{doc}.json"));
    if !path.is_file() {
        return Ok(None);
    }
    let mtime_ms = path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(|| now().timestamp_millis());
    let text = std::fs::read_to_string(&path).map_err(store_err)?;
    let serde_json::Value::Object(map) =
        serde_json::from_str(&text).map_err(|e| Error::Sync(format!("crdt doc {doc}: {e}")))?
    else {
        return Err(Error::Sync(format!(
            "crdt doc {doc}: top level must be a JSON object"
        )));
    };
    // Last-materialized view for provenance lookups.
    let view: BTreeMap<String, (String, i64)> = store.with_conn(|c| {
        let mut s = c.prepare(
            "SELECT field, value_json, t_ms FROM app_crdt_view
             WHERE app_id=?1 AND doc=?2",
        )?;
        let rows = s.query_map(params![app_id, doc], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, i64>(2)?),
            ))
        })?;
        rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
    })?;
    let mut fields = BTreeMap::new();
    for (field, raw) in map {
        match as_cell(&raw) {
            Some(c) => {
                fields.insert(field, c);
            }
            None => {
                let vj = serde_json::to_string(&raw).map_err(store_err)?;
                let t = match view.get(&field) {
                    Some((pv, pt)) if *pv == vj => *pt,
                    _ => mtime_ms,
                };
                fields.insert(field, Cell::Value { v: raw, t });
            }
        }
    }
    Ok(Some(fields))
}

/// Replace one writer's cell set for a doc (the object IS the writer's
/// full current view — absent fields are withdrawn, not deleted).
fn replace_cells(
    store: &Arc<Store>,
    app_id: &str,
    doc: &str,
    writer: &str,
    fields: &BTreeMap<String, Cell>,
) -> Result<()> {
    // Serialize values up front — the closure below can only return
    // rusqlite errors.
    let rows: Vec<(&String, Option<String>, i64, i64)> = fields
        .iter()
        .map(|(field, cell)| {
            Ok((
                field,
                cell.value()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(store_err)?,
                cell.t(),
                cell.is_tomb() as i64,
            ))
        })
        .collect::<Result<_>>()?;
    store.with_conn(|c| {
        c.execute(
            "DELETE FROM app_crdt_cells WHERE app_id=?1 AND doc=?2 AND writer=?3",
            params![app_id, doc, writer],
        )?;
        let mut ins = c.prepare(
            "INSERT INTO app_crdt_cells(app_id, doc, writer, field,
                value_json, t_ms, tombstone) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for (field, vj, t, tomb) in &rows {
            ins.execute(params![app_id, doc, writer, field, vj, t, tomb])?;
        }
        Ok(())
    })
}

/// Merged doc + the `(field, value_json, t_ms)` rows materialization
/// records as the last-shown view.
type Merged = (
    serde_json::Map<String, serde_json::Value>,
    Vec<(String, String, i64)>,
);

/// Fold all writers' cells for a doc into the winning plain-JSON object
/// plus the view rows that recorded each field's provenance.
fn winners(store: &Arc<Store>, app_id: &str, doc: &str) -> Result<Merged> {
    let cells: Vec<(String, Option<String>, i64, bool)> = store.with_conn(|c| {
        // Per field: newest t wins; tie → lowest writer id (same cells
        // everywhere → same winner → convergence).
        let mut s = c.prepare(
            "SELECT field, value_json, t_ms, tombstone, writer
             FROM app_crdt_cells WHERE app_id=?1 AND doc=?2
             ORDER BY field, t_ms DESC, writer ASC",
        )?;
        let rows = s.query_map(params![app_id, doc], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)? != 0,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let mut doc_obj = serde_json::Map::new();
    let mut view = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (field, vj, t, tomb) in cells {
        if !seen.insert(field.clone()) {
            continue; // ordered — first row per field is the winner
        }
        if tomb {
            continue; // tombstone winner → field absent
        }
        let vj = vj.ok_or_else(|| Error::Sync("crdt value cell missing value".into()))?;
        let v: serde_json::Value =
            serde_json::from_str(&vj).map_err(|e| Error::Sync(format!("crdt cell: {e}")))?;
        doc_obj.insert(field.clone(), v);
        view.push((field, vj, t));
    }
    Ok((doc_obj, view))
}

/// Rewrite the on-disk doc + view rows from current cells — only when
/// the merged bytes differ (keeps mtimes honest for the next push).
/// No-op when the app isn't installed locally (cells still accrue; the
/// doc materializes when the package lands or the next cell arrives).
pub fn materialize(store: &Arc<Store>, data_dir: &Path, app_id: &str, doc: &str) -> Result<()> {
    if !data_dir.join("apps").join(app_id).is_dir() {
        return Ok(());
    }
    let (doc_obj, view) = winners(store, app_id, doc)?;
    let bytes = serde_json::to_vec_pretty(&doc_obj).map_err(store_err)?;
    let path = crdt_dir(data_dir, app_id).join(format!("{doc}.json"));
    if std::fs::read(&path).map(|b| b == bytes).unwrap_or(false) {
        return Ok(());
    }
    let dir = crdt_dir(data_dir, app_id);
    std::fs::create_dir_all(&dir).map_err(store_err)?;
    std::fs::write(&path, &bytes).map_err(store_err)?;
    store.with_conn(|c| {
        c.execute(
            "DELETE FROM app_crdt_view WHERE app_id=?1 AND doc=?2",
            params![app_id, doc],
        )?;
        let mut ins = c.prepare(
            "INSERT INTO app_crdt_view(app_id, doc, field, value_json, t_ms)
             VALUES(?1,?2,?3,?4,?5)",
        )?;
        for (field, vj, t) in &view {
            ins.execute(params![app_id, doc, field, vj, t])?;
        }
        Ok(())
    })
}

/// Re-materialize every stored doc for an app — called when the package
/// lands so data written before install isn't stranded in the table.
pub fn materialize_all(store: &Arc<Store>, data_dir: &Path, app_id: &str) -> Result<()> {
    let docs: Vec<String> = store.with_conn(|c| {
        let mut s =
            c.prepare("SELECT DISTINCT doc FROM app_crdt_cells WHERE app_id=?1 ORDER BY doc")?;
        let rows = s.query_map(params![app_id], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    for doc in docs {
        materialize(store, data_dir, app_id, &doc)?;
    }
    Ok(())
}

/// Apply a received `acrdt` object: verify the writer triple, replace
/// the writer's field set, re-materialize. Tombstoned objects withdraw
/// the writer's cells entirely.
pub(crate) fn apply(
    store: &Arc<Store>,
    data_dir: &Path,
    app_id: &str,
    doc: &str,
    writer: &str,
    obj: &SyncObject,
    raw: &[u8],
) -> Result<()> {
    // Same anti-forgery as backups: key segment, object writer, and
    // payload writer must agree, and the writer must be paired.
    if obj.writer.to_string() != writer {
        tracing::warn!(key = %format!("acrdt/{app_id}/{doc}/{writer}"), "writer mismatch — dropped");
        return Ok(());
    }
    if !crate::backup::is_paired(store, writer)? {
        tracing::warn!(key = %format!("acrdt/{app_id}/{doc}/{writer}"), "unpaired writer — dropped");
        return Ok(());
    }
    let fields = if obj.tombstone {
        BTreeMap::new()
    } else {
        let p: CrdtDocPayload = serde_json::from_slice(raw)
            .map_err(|e| Error::Sync(format!("bad crdt payload: {e}")))?;
        if p.v != 1 || p.app != app_id || p.doc != doc || p.writer != writer {
            tracing::warn!("crdt payload/key mismatch — dropped");
            return Ok(());
        }
        p.fields
    };
    replace_cells(store, app_id, doc, writer, &fields)?;
    materialize(store, data_dir, app_id, doc)
}

/// Our last-published field-set for a doc — comparing the freshly
/// normalized doc against this is the "did it change?" check that keeps
/// unchanged docs from re-pushing (mtime can tie under coarse FAT32
/// granularity, so version alone can't detect no-ops).
fn our_cells(
    store: &Arc<Store>,
    app_id: &str,
    doc: &str,
    me: &str,
) -> Result<BTreeMap<String, Cell>> {
    let rows: Vec<(String, Option<String>, i64, bool)> = store.with_conn(|c| {
        let mut s = c.prepare(
            "SELECT field, value_json, t_ms, tombstone FROM app_crdt_cells
             WHERE app_id=?1 AND doc=?2 AND writer=?3",
        )?;
        let rows = s.query_map(params![app_id, doc, me], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)? != 0,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let mut out = BTreeMap::new();
    for (field, vj, t, tomb) in rows {
        let cell = if tomb {
            Cell::Tomb { d: true, t }
        } else {
            Cell::Value {
                v: serde_json::from_str(
                    &vj.ok_or_else(|| Error::Sync("crdt value cell missing value".into()))?,
                )
                .map_err(|e| Error::Sync(format!("crdt cell: {e}")))?,
                t,
            }
        };
        out.insert(field, cell);
    }
    Ok(out)
}

/// Collect `data/crdt/*.json` across installed apps whose published
/// cells would differ — returns `(key, payload_bytes, version_hint)`
/// for the caller to seal+push with a mirror-beating version. Docs
/// whose file vanished push an empty field-set (withdrawal). Own cells
/// are replaced and the doc re-materialized here, so a remote winner
/// lands in the file without waiting for a pull.
pub(crate) fn collect(
    store: &Arc<Store>,
    data_dir: &Path,
    me: DeviceId,
) -> Result<Vec<(String, Vec<u8>, i64)>> {
    let apps: Vec<String> = store.with_conn(|c| {
        c.prepare("SELECT id FROM apps WHERE deleted=0")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let mut out = Vec::new();
    let me_s = me.to_string();
    for app_id in &apps {
        // Docs present on disk.
        let dir = crdt_dir(data_dir, app_id);
        let mut on_disk: std::collections::BTreeSet<String> = Default::default();
        if dir.is_dir() {
            for e in std::fs::read_dir(&dir).map_err(store_err)?.flatten() {
                if let Some(d) = doc_name(&e.path()) {
                    on_disk.insert(d.to_string());
                }
            }
        }
        // Docs we previously published cells for (withdrawal check).
        let published: Vec<String> = store.with_conn(|c| {
            c.prepare(
                "SELECT DISTINCT doc FROM app_crdt_cells
                 WHERE app_id=?1 AND writer=?2",
            )?
            .query_map(params![app_id, me_s], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for doc in on_disk.iter().chain(published.iter()) {
            let fields = collect_doc(data_dir, store, app_id, doc)?.unwrap_or_default(); // vanished file → empty set = withdrawal
            if on_disk.contains(doc) && fields == our_cells(store, app_id, doc, &me_s)? {
                continue; // nothing changed since our last publish
            }
            let updated_ms = fields
                .values()
                .map(|c| c.t())
                .max()
                .unwrap_or_else(|| now().timestamp_millis());
            let payload = CrdtDocPayload {
                v: 1,
                app: app_id.clone(),
                doc: doc.clone(),
                writer: me_s.clone(),
                fields: fields.clone(),
                updated_at: now().to_rfc3339(),
            };
            let raw = serde_json::to_vec(&payload).map_err(store_err)?;
            out.push((
                format!("acrdt/{app_id}/{doc}/{me_s}"),
                raw,
                updated_ms.max(1),
            ));
            // Our table mirrors what we published.
            replace_cells(store, app_id, doc, &me_s, &fields)?;
            materialize(store, data_dir, app_id, doc)?;
        }
    }
    Ok(out)
}

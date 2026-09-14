//! Sync engine — moves sealed `SyncObject`s between the local store and a
//! transport.
//!
//! V2a syncs **memories** (`sync_scope='synchronized'`, the default).
//! Each memory row becomes one `memory/<uuid>` object: the row serialized
//! to a versioned payload, sealed under the vault key with the object key
//! as AAD. Soft-deleted rows ship as tombstones.
//!
//! Merge is last-writer-wins on the source row's `updated_at` (recorded as
//! the object version). Clock skew between devices can therefore pick a
//! stale winner — a documented LWW limitation, not silent corruption.
//!
//! `sync_objects` mirrors the sealed objects we've written or applied, so
//! a pull doesn't echo straight back out as a new push.

use crate::crypto;
use crate::SyncTransport;
use pai_core::*;
use pai_storage::{parse_ts, ts, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const MEMORY_PREFIX: &str = "memory/";

/// Versioned plaintext of one synced memory row.
#[derive(Debug, Serialize, Deserialize)]
struct MemoryPayload {
    v: u8,
    id: String,
    scope: String,
    content: String,
    source: String,
    created_at: String,
    updated_at: String,
    confidence: f32,
    importance: f32,
    privacy: String,
    entities: Vec<String>,
    embedding: Option<Vec<f32>>,
    conversation_id: Option<String>,
}

pub struct SyncOutcome {
    pub pushed: usize,
    pub pulled: usize,
    pub skipped: usize,
}

pub struct SyncEngine<T: SyncTransport> {
    transport: T,
    store: Arc<Store>,
    vault: [u8; 32],
    device: DeviceId,
}

impl<T: SyncTransport> SyncEngine<T> {
    pub fn new(transport: T, store: Arc<Store>, vault: [u8; 32], device: DeviceId) -> Self {
        Self {
            transport,
            store,
            vault,
            device,
        }
    }

    /// Seal every locally-changed synchronized memory and push it.
    pub async fn push(&self) -> Result<SyncOutcome> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, type, content, source, created_at, updated_at,
                        confidence, importance, privacy_level, entities_json,
                        embedding, deleted, conversation_id
                 FROM memories WHERE sync_scope='synchronized'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, f32>(6)?,
                    r.get::<_, f32>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, Option<Vec<u8>>>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, Option<String>>(12)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;

        let mut out = SyncOutcome {
            pushed: 0,
            pulled: 0,
            skipped: 0,
        };
        for (
            id,
            scope,
            content,
            source,
            created,
            updated,
            conf,
            imp,
            priv_,
            ents,
            emb,
            deleted,
            conv,
        ) in rows
        {
            let updated_at = parse_ts(&updated);
            let version = updated_at.timestamp_millis().max(1) as u64;
            let key = format!("{MEMORY_PREFIX}{id}");
            if self.mirror_version(&key)? >= version {
                out.skipped += 1;
                continue;
            }
            let tombstone = deleted != 0;
            let ciphertext = if tombstone {
                crypto::seal(&self.vault, key.as_bytes(), b"")?
            } else {
                let embedding: Option<Vec<f32>> = emb.map(|b| {
                    b.chunks(4)
                        .filter_map(|c| <[u8; 4]>::try_from(c).ok().map(f32::from_le_bytes))
                        .collect()
                });
                let payload = MemoryPayload {
                    v: 1,
                    id,
                    scope,
                    content,
                    source,
                    created_at: created,
                    updated_at: updated,
                    confidence: conf,
                    importance: imp,
                    privacy: priv_,
                    entities: serde_json::from_str(&ents).unwrap_or_default(),
                    embedding,
                    conversation_id: conv,
                };
                let raw = serde_json::to_vec(&payload).map_err(|e| Error::Sync(e.to_string()))?;
                crypto::seal(&self.vault, key.as_bytes(), &raw)?
            };
            let obj = SyncObject {
                key: key.clone(),
                ciphertext,
                version,
                writer: self.device,
                updated_at,
                tombstone,
            };
            self.transport.push(&obj).await?;
            self.set_mirror(&obj)?;
            out.pushed += 1;
        }
        Ok(out)
    }

    /// Pull remote objects newer than our mirror and apply them.
    pub async fn pull(&self) -> Result<SyncOutcome> {
        let metas = self.transport.list().await?;
        let mut out = SyncOutcome {
            pushed: 0,
            pulled: 0,
            skipped: 0,
        };
        for meta in metas {
            if meta.writer == self.device || !meta.key.starts_with(MEMORY_PREFIX) {
                out.skipped += 1;
                continue;
            }
            let local = self.mirror(&meta.key)?;
            if let Some((v, t)) = local {
                if (meta.version, meta.updated_at) <= (v, t) {
                    out.skipped += 1;
                    continue;
                }
            }
            let Some(obj) = self.transport.pull(&meta.key).await? else {
                out.skipped += 1;
                continue;
            };
            let raw = match crypto::open(&self.vault, obj.key.as_bytes(), &obj.ciphertext) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(key = %obj.key, error = %e, "skipping unopenable sync object");
                    out.skipped += 1;
                    continue;
                }
            };
            if obj.tombstone {
                self.apply_tombstone(&obj.key[MEMORY_PREFIX.len()..], obj.updated_at)?;
            } else {
                let p: MemoryPayload = serde_json::from_slice(&raw)
                    .map_err(|e| Error::Sync(format!("bad memory payload: {e}")))?;
                self.apply_memory(&p)?;
            }
            self.set_mirror(&obj)?;
            out.pulled += 1;
        }
        Ok(out)
    }

    pub async fn run(&self) -> Result<SyncOutcome> {
        let mut up = self.push().await?;
        let down = self.pull().await?;
        up.pulled = down.pulled;
        up.skipped += down.skipped;
        Ok(up)
    }

    // -- local mirror ----------------------------------------------------

    fn mirror(&self, key: &str) -> Result<Option<(u64, Timestamp)>> {
        self.store.with_conn(|c| {
            c.query_row(
                "SELECT version, updated_at FROM sync_objects WHERE key=?1",
                params![key],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        parse_ts(&r.get::<_, String>(1)?),
                    ))
                },
            )
            .ok()
            .map_or(Ok(None), |x| Ok(Some(x)))
        })
    }

    fn mirror_version(&self, key: &str) -> Result<u64> {
        Ok(self.mirror(key)?.map(|(v, _)| v).unwrap_or(0))
    }

    fn set_mirror(&self, obj: &SyncObject) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO sync_objects(key, ciphertext, version, writer,
                    updated_at, tombstone) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(key) DO UPDATE SET
                    ciphertext=excluded.ciphertext, version=excluded.version,
                    writer=excluded.writer, updated_at=excluded.updated_at,
                    tombstone=excluded.tombstone",
                params![
                    obj.key,
                    obj.ciphertext,
                    obj.version as i64,
                    obj.writer.to_string(),
                    ts(&obj.updated_at),
                    obj.tombstone as i64,
                ],
            )
        })?;
        Ok(())
    }

    // -- apply ------------------------------------------------------------

    fn apply_tombstone(&self, id: &str, remote_updated: Timestamp) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE memories SET deleted=1, updated_at=?2
                 WHERE id=?1 AND updated_at < ?2",
                params![id, ts(&remote_updated)],
            )
        })?;
        Ok(())
    }

    fn apply_memory(&self, p: &MemoryPayload) -> Result<()> {
        if p.v != 1 {
            return Err(Error::Sync(format!("unsupported payload v{}", p.v)));
        }
        let remote_ts = ts(&parse_ts(&p.updated_at));
        // LWW: apply only when strictly newer than the local row.
        let dominated = self.store.with_conn(|c| {
            Ok(c.query_row(
                "SELECT updated_at >= ?2 FROM memories WHERE id=?1",
                params![p.id, remote_ts],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false))
        })?;
        if dominated {
            return Ok(());
        }
        let emb: Option<Vec<u8>> = p
            .embedding
            .as_ref()
            .map(|v| v.iter().flat_map(|f| f.to_le_bytes()).collect());
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO memories(id, type, content, source, created_at,
                    updated_at, confidence, importance, privacy_level,
                    entities_json, embedding, deleted, sync_scope,
                    conversation_id)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,'synchronized',?12)
                 ON CONFLICT(id) DO UPDATE SET
                    type=excluded.type, content=excluded.content,
                    source=excluded.source, updated_at=excluded.updated_at,
                    confidence=excluded.confidence,
                    importance=excluded.importance,
                    privacy_level=excluded.privacy_level,
                    entities_json=excluded.entities_json,
                    embedding=excluded.embedding, deleted=0,
                    conversation_id=excluded.conversation_id",
                params![
                    p.id,
                    p.scope,
                    p.content,
                    p.source,
                    p.created_at,
                    p.updated_at,
                    p.confidence,
                    p.importance,
                    p.privacy,
                    serde_json::to_string(&p.entities).unwrap_or_else(|_| "[]".into()),
                    emb,
                    p.conversation_id,
                ],
            )
        })?;
        Ok(())
    }
}

/// Open a `FolderTransport`-backed engine for this device.
pub fn folder_engine(
    dir: &std::path::Path,
    store: Arc<Store>,
    device: DeviceId,
    data_dir: &std::path::Path,
) -> Result<SyncEngine<crate::FolderTransport>> {
    let vault = crypto::vault_key(data_dir)?
        .ok_or_else(|| Error::Sync("no vault key — pair a device first (pai pair)".into()))?;
    Ok(SyncEngine::new(
        crate::FolderTransport::new(dir.to_path_buf())?,
        store,
        vault,
        device,
    ))
}

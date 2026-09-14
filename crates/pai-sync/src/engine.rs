//! Sync engine — moves sealed `SyncObject`s between the local store and a
//! transport.
//!
//! Synced row kinds, each sealed as one `SyncObject` per row:
//! - `conv/<id>` — conversations with `sync_scope='synchronized'`
//! - `msg/<id>`  — messages under a synchronized, non-deleted conversation
//! - `doc/<id>`  — documents with `sync_scope='synchronized'` (blob bytes
//!   travel inside the sealed payload, base64'd)
//! - `memory/<id>` — memories with `sync_scope='synchronized'` (the default)
//!
//! Objects are sealed under the vault key with the object key as AAD;
//! soft-deleted rows ship as empty-payload tombstones.
//!
//! Merge is last-writer-wins on the source row's `updated_at` (recorded as
//! the object version). Clock skew between devices can therefore pick a
//! stale winner — a documented LWW limitation, not silent corruption.
//!
//! `sync_objects` mirrors the sealed objects we've written or applied, so
//! a pull doesn't echo straight back out as a new push.

use crate::crypto;
use crate::SyncTransport;
use base64::Engine as _;
use pai_core::*;
use pai_storage::{parse_ts, ts, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const CONV_PREFIX: &str = "conv/";
const MSG_PREFIX: &str = "msg/";
const DOC_PREFIX: &str = "doc/";
const MEMORY_PREFIX: &str = "memory/";

/// Apply order on pull: conversations before their messages (FK), then
/// documents, then memories (which may reference conversations).
fn kind_rank(key: &str) -> Option<u8> {
    match key.split('/').next() {
        Some("conv") => Some(0),
        Some("msg") => Some(1),
        Some("doc") => Some(2),
        Some("memory") => Some(3),
        _ => None,
    }
}

/// Millis version of an RFC3339 timestamp — same rule every kind uses.
fn version_of(updated: &str) -> u64 {
    parse_ts(updated).timestamp_millis().max(1) as u64
}

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

/// Versioned plaintext of one synced conversation row.
#[derive(Debug, Serialize, Deserialize)]
struct ConversationPayload {
    v: u8,
    id: String,
    session_id: String,
    title: Option<String>,
    created_at: String,
    updated_at: String,
    sync_scope: String,
    memory_scope: String,
}

/// Versioned plaintext of one synced message row. Messages are immutable —
/// first writer wins; a conversation tombstone removes them on peers.
#[derive(Debug, Serialize, Deserialize)]
struct MessagePayload {
    v: u8,
    id: String,
    conversation_id: String,
    role: String,
    trust: String,
    created_at: String,
    content_json: String,
}

/// One section inside a synced document (embedding bytes ride along so a
/// receiving device doesn't need an embedder to hybrid-search it).
#[derive(Debug, Serialize, Deserialize)]
struct DocSection {
    section: i64,
    text: String,
    embedding: Option<Vec<u8>>,
}

/// Versioned plaintext of one synced document row + sections + blob.
#[derive(Debug, Serialize, Deserialize)]
struct DocumentPayload {
    v: u8,
    id: String,
    title: Option<String>,
    mime: String,
    /// Content-addressed blob bytes, base64 — the receiver re-`put_blob`s.
    blob_b64: String,
    created_at: String,
    updated_at: String,
    trust: String,
    sections: Vec<DocSection>,
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
        self.push_conversations(&mut out).await?;
        self.push_messages(&mut out).await?;
        self.push_documents(&mut out).await?;
        Ok(out)
    }

    /// Shared seal+push+mirror for one row-kind object.
    async fn push_sealed(
        &self,
        key: String,
        raw: &[u8],
        version: u64,
        updated_at: Timestamp,
        tombstone: bool,
        out: &mut SyncOutcome,
    ) -> Result<()> {
        if self.mirror_version(&key)? >= version {
            out.skipped += 1;
            return Ok(());
        }
        let ciphertext = crypto::seal(&self.vault, key.as_bytes(), raw)?;
        let obj = SyncObject {
            key,
            ciphertext,
            version,
            writer: self.device,
            updated_at,
            tombstone,
        };
        self.transport.push(&obj).await?;
        self.set_mirror(&obj)?;
        out.pushed += 1;
        Ok(())
    }

    /// `sync_scope='synchronized'` conversations, tombstones included.
    async fn push_conversations(&self, out: &mut SyncOutcome) -> Result<()> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, session_id, title, created_at,
                        COALESCE(updated_at, created_at), deleted, memory_scope
                 FROM conversations WHERE sync_scope='synchronized'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for (id, session_id, title, created, updated, deleted, mem_scope) in rows {
            let updated_at = parse_ts(&updated);
            let key = format!("{CONV_PREFIX}{id}");
            let tombstone = deleted != 0;
            let raw = if tombstone {
                Vec::new()
            } else {
                serde_json::to_vec(&ConversationPayload {
                    v: 1,
                    id,
                    session_id,
                    title,
                    created_at: created,
                    updated_at: updated.clone(),
                    sync_scope: "synchronized".into(),
                    memory_scope: mem_scope,
                })
                .map_err(|e| Error::Sync(e.to_string()))?
            };
            self.push_sealed(key, &raw, version_of(&updated), updated_at, tombstone, out)
                .await?;
        }
        Ok(())
    }

    /// Messages under synchronized, non-deleted conversations — immutable,
    /// so they push exactly once.
    async fn push_messages(&self, out: &mut SyncOutcome) -> Result<()> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT m.id, m.conversation_id, m.role, m.trust,
                        m.created_at, m.content_json
                 FROM messages m
                 JOIN conversations c ON c.id = m.conversation_id
                 WHERE c.sync_scope='synchronized' AND c.deleted=0",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for (id, conv, role, trust, created, content) in rows {
            let updated_at = parse_ts(&created);
            let raw = serde_json::to_vec(&MessagePayload {
                v: 1,
                id: id.clone(),
                conversation_id: conv,
                role,
                trust,
                created_at: created.clone(),
                content_json: content,
            })
            .map_err(|e| Error::Sync(e.to_string()))?;
            self.push_sealed(
                format!("{MSG_PREFIX}{id}"),
                &raw,
                version_of(&created),
                updated_at,
                false,
                out,
            )
            .await?;
        }
        Ok(())
    }

    /// `sync_scope='synchronized'` documents — sections + blob bytes ride
    /// inside the sealed payload; tombstones carry the delete.
    async fn push_documents(&self, out: &mut SyncOutcome) -> Result<()> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, title, mime, blob, created_at,
                        COALESCE(updated_at, created_at), trust, deleted
                 FROM documents WHERE sync_scope='synchronized'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for (id, title, mime, blob_id, created, updated, trust, deleted) in rows {
            let updated_at = parse_ts(&updated);
            let key = format!("{DOC_PREFIX}{id}");
            let tombstone = deleted != 0;
            let raw = if tombstone {
                Vec::new()
            } else {
                let bytes = match self.store.get_blob(&blob_id) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(%id, error = %e, "sync: document blob missing, skipping");
                        out.skipped += 1;
                        continue;
                    }
                };
                let sections = self.store.with_conn(|c| {
                    let mut stmt = c.prepare(
                        "SELECT section, text, embedding FROM document_sections
                         WHERE document_id=?1 ORDER BY section",
                    )?;
                    let rows = stmt.query_map(params![id], |r| {
                        Ok(DocSection {
                            section: r.get::<_, i64>(0)?,
                            text: r.get::<_, String>(1)?,
                            embedding: r.get::<_, Option<Vec<u8>>>(2)?,
                        })
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })?;
                serde_json::to_vec(&DocumentPayload {
                    v: 1,
                    id,
                    title,
                    mime,
                    blob_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    created_at: created,
                    updated_at: updated.clone(),
                    trust,
                    sections,
                })
                .map_err(|e| Error::Sync(e.to_string()))?
            };
            self.push_sealed(key, &raw, version_of(&updated), updated_at, tombstone, out)
                .await?;
        }
        Ok(())
    }

    /// Pull remote objects newer than our mirror and apply them, in
    /// `kind_rank` order (conversations land before their messages — the
    /// FK chain requires it).
    pub async fn pull(&self) -> Result<SyncOutcome> {
        let metas = self.transport.list().await?;
        let mut out = SyncOutcome {
            pushed: 0,
            pulled: 0,
            skipped: 0,
        };
        let mut pending: Vec<(u8, SyncObject)> = Vec::new();
        for meta in metas {
            let Some(rank) = kind_rank(&meta.key) else {
                out.skipped += 1;
                continue;
            };
            if meta.writer == self.device {
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
            match self.transport.pull(&meta.key).await? {
                Some(obj) => pending.push((rank, obj)),
                None => out.skipped += 1,
            }
        }
        pending.sort_by_key(|(r, o)| (*r, o.updated_at));
        for (_, obj) in pending {
            let raw = match crypto::open(&self.vault, obj.key.as_bytes(), &obj.ciphertext) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(key = %obj.key, error = %e, "skipping unopenable sync object");
                    out.skipped += 1;
                    continue;
                }
            };
            self.apply_obj(&obj, &raw)?;
            self.set_mirror(&obj)?;
            out.pulled += 1;
        }
        Ok(out)
    }

    /// Dispatch one opened object to its row-kind apply path.
    fn apply_obj(&self, obj: &SyncObject, raw: &[u8]) -> Result<()> {
        let key = obj.key.as_str();
        if let Some(id) = key.strip_prefix(MEMORY_PREFIX) {
            if obj.tombstone {
                return self.apply_tombstone(id, obj.updated_at);
            }
            let p: MemoryPayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad memory payload: {e}")))?;
            return self.apply_memory(&p);
        }
        if let Some(id) = key.strip_prefix(CONV_PREFIX) {
            if obj.tombstone {
                return self.apply_conv_tombstone(id, obj.updated_at);
            }
            let p: ConversationPayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad conversation payload: {e}")))?;
            return self.apply_conversation(&p);
        }
        if let Some(id) = key.strip_prefix(DOC_PREFIX) {
            if obj.tombstone {
                return self.apply_doc_tombstone(id, obj.updated_at);
            }
            let p: DocumentPayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad document payload: {e}")))?;
            return self.apply_document(&p);
        }
        if key.starts_with(MSG_PREFIX) {
            if obj.tombstone {
                return Ok(()); // messages carry no tombstones
            }
            let p: MessagePayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad message payload: {e}")))?;
            return self.apply_message(&p);
        }
        Ok(())
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

    /// Conversation tombstone: mark deleted + drop its messages locally.
    fn apply_conv_tombstone(&self, id: &str, remote_updated: Timestamp) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE conversations SET deleted=1, updated_at=?2
                 WHERE id=?1 AND COALESCE(updated_at, created_at) < ?2",
                params![id, ts(&remote_updated)],
            )?;
            c.execute("DELETE FROM messages WHERE conversation_id=?1", params![id])
        })?;
        Ok(())
    }

    fn apply_conversation(&self, p: &ConversationPayload) -> Result<()> {
        if p.v != 1 {
            return Err(Error::Sync(format!("unsupported payload v{}", p.v)));
        }
        self.store.with_conn(|c| {
            // Remote session ids don't exist locally — stub the row (the FK
            // chain wants session → user/device; we point at this device).
            c.execute(
                "INSERT OR IGNORE INTO sessions(id, user_id, device_id, started_at)
                 SELECT ?1, owner, id, ?2 FROM devices WHERE id=?3",
                params![p.session_id, p.created_at, self.device.to_string()],
            )?;
            c.execute(
                "INSERT INTO conversations(id, session_id, title, created_at,
                    updated_at, sync_scope, memory_scope, deleted)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,0)
                 ON CONFLICT(id) DO UPDATE SET
                    title=excluded.title, updated_at=excluded.updated_at,
                    sync_scope=excluded.sync_scope,
                    memory_scope=excluded.memory_scope, deleted=0
                 WHERE excluded.updated_at >
                    COALESCE(conversations.updated_at, conversations.created_at)",
                params![
                    p.id,
                    p.session_id,
                    p.title,
                    p.created_at,
                    p.updated_at,
                    p.sync_scope,
                    p.memory_scope,
                ],
            )
        })?;
        Ok(())
    }

    fn apply_message(&self, p: &MessagePayload) -> Result<()> {
        if p.v != 1 {
            return Err(Error::Sync(format!("unsupported payload v{}", p.v)));
        }
        // Messages only land under a live conversation — a missing or
        // tombstoned parent means this object is stale.
        let live = self.store.with_conn(|c| {
            Ok(c.query_row(
                "SELECT deleted=0 FROM conversations WHERE id=?1",
                params![p.conversation_id],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false))
        })?;
        if !live {
            return Ok(());
        }
        self.store.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO messages(id, conversation_id, role,
                    trust, created_at, content_json)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    p.id,
                    p.conversation_id,
                    p.role,
                    p.trust,
                    p.created_at,
                    p.content_json,
                ],
            )
        })?;
        Ok(())
    }

    /// Document tombstone: drop sections + FTS, keep the tombstoned row.
    fn apply_doc_tombstone(&self, id: &str, remote_updated: Timestamp) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "DELETE FROM documents_fts WHERE rowid IN
                    (SELECT rowid FROM document_sections WHERE document_id=?1)",
                params![id],
            )?;
            c.execute(
                "DELETE FROM document_sections WHERE document_id=?1",
                params![id],
            )?;
            c.execute(
                "UPDATE documents SET deleted=1, updated_at=?2
                 WHERE id=?1 AND COALESCE(updated_at, created_at) < ?2",
                params![id, ts(&remote_updated)],
            )
        })?;
        Ok(())
    }

    fn apply_document(&self, p: &DocumentPayload) -> Result<()> {
        if p.v != 1 {
            return Err(Error::Sync(format!("unsupported payload v{}", p.v)));
        }
        let remote_ts = ts(&parse_ts(&p.updated_at));
        let dominated = self.store.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(updated_at, created_at) >= ?2 FROM documents
                 WHERE id=?1",
                params![p.id, remote_ts],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false))
        })?;
        if dominated {
            return Ok(());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&p.blob_b64)
            .map_err(|e| Error::Sync(format!("bad document blob: {e}")))?;
        let blob_id = self.store.put_blob(&bytes)?;
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO documents(id, title, mime, blob, created_at,
                    updated_at, trust, sync_scope, deleted)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'synchronized',0)
                 ON CONFLICT(id) DO UPDATE SET
                    title=excluded.title, mime=excluded.mime,
                    blob=excluded.blob, updated_at=excluded.updated_at,
                    trust=excluded.trust, deleted=0
                 WHERE excluded.updated_at >
                    COALESCE(documents.updated_at, documents.created_at)",
                params![
                    p.id,
                    p.title,
                    p.mime,
                    blob_id,
                    p.created_at,
                    p.updated_at,
                    p.trust,
                ],
            )?;
            // Rebuild the searchable surface from the payload.
            c.execute(
                "DELETE FROM documents_fts WHERE rowid IN
                    (SELECT rowid FROM document_sections WHERE document_id=?1)",
                params![p.id],
            )?;
            c.execute(
                "DELETE FROM document_sections WHERE document_id=?1",
                params![p.id],
            )?;
            for s in &p.sections {
                c.execute(
                    "INSERT INTO document_sections(document_id, section,
                        text, embedding) VALUES(?1,?2,?3,?4)",
                    params![p.id, s.section, s.text, s.embedding],
                )?;
                c.execute(
                    "INSERT INTO documents_fts(rowid, text)
                     VALUES(last_insert_rowid(), ?1)",
                    params![s.text],
                )?;
            }
            Ok(())
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

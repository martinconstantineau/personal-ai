//! Sync engine — moves sealed `SyncObject`s between the local store and a
//! transport.
//!
//! Synced row kinds, each sealed as one `SyncObject` per row:
//! - `conv/<id>` — conversations with `sync_scope='synchronized'`
//! - `msg/<id>`  — messages under a synchronized, non-deleted conversation
//! - `doc/<id>`  — documents with `sync_scope='synchronized'` (blob bytes
//!   travel inside the sealed payload, base64'd)
//! - `task/<id>` — tasks with `sync_scope='synchronized'`, claim/lease
//!   fields included so only one device runs a due task
//! - `wf/<id>`   — workflow definitions (runs stay device-local)
//! - `memory/<id>` — memories with `sync_scope='synchronized'` (the default)
//! - `app/<id>`  — installed app packages: manifest + files + signature,
//!   verified against a known device/peer key on apply (live `data/`
//!   state does not sync — only the package)
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
use pai_storage::{parse_ts, store_err, ts, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const CONV_PREFIX: &str = "conv/";
const MSG_PREFIX: &str = "msg/";
const DOC_PREFIX: &str = "doc/";
const TASK_PREFIX: &str = "task/";
const WF_PREFIX: &str = "wf/";
const NTF_PREFIX: &str = "ntf/";
const MEMORY_PREFIX: &str = "memory/";
const CKG_PREFIX: &str = "ckg/";
const APP_PREFIX: &str = "app/";

/// Apply order on pull: conversations before their messages (FK), then
/// documents, then tasks and memories (which may reference conversations).
fn kind_rank(key: &str) -> Option<u8> {
    match key.split('/').next() {
        Some("ckg") => Some(0), // circle grants first — keys unlock rows
        Some("conv") => Some(1),
        Some("msg") => Some(2),
        Some("doc") => Some(3),
        Some("task") => Some(4),
        Some("wf") => Some(5),
        Some("memory") => Some(6),
        Some("ntf") => Some(7),
        Some("app") => Some(8),
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

/// Versioned plaintext of one synced task row — claim/lease fields ride
/// along so the claim made by one device suppresses duplicate execution
/// on every other device once the object lands.
#[derive(Debug, Serialize, Deserialize)]
struct TaskPayload {
    v: u8,
    id: String,
    title: String,
    agent_id: String,
    created_at: String,
    run_at: Option<String>,
    state: String,
    trigger_json: String,
    payload_json: String,
    result_json: Option<String>,
    claimed_by: Option<String>,
    lease_expires_at: Option<String>,
    updated_at: String,
}

/// Versioned plaintext of one synced workflow definition — the whole
/// `definition_json` travels opaque; the receiver validates it on
/// load-before-run (never trusts a synced step list blindly).
#[derive(Debug, Serialize, Deserialize)]
struct WorkflowPayload {
    v: u8,
    id: String,
    name: String,
    definition_json: String,
    created_at: String,
    updated_at: String,
}

/// Versioned plaintext of one notification — read state travels so the
/// inbox follows the user across devices.
#[derive(Debug, Serialize, Deserialize)]
struct NotificationPayload {
    v: u8,
    id: String,
    title: String,
    body: String,
    source: String,
    channel: String,
    created_at: String,
    read_at: Option<String>,
    updated_at: String,
}

/// One file inside a synced app package.
#[derive(Debug, Serialize, Deserialize)]
struct AppFileEntry {
    path: String,
    b64: String,
}

/// Versioned plaintext of one synced app package — the whole installable
/// payload: manifest + all package files + the Ed25519 signature. The
/// receiver re-verifies against a known device or peer key before
/// installing; `data/` (live app state) never travels.
#[derive(Debug, Serialize, Deserialize)]
struct AppPayload {
    v: u8,
    id: String,
    name: String,
    version: String,
    runtime: String,
    updated_at: String,
    signature_b64: String,
    files: Vec<AppFileEntry>,
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
    /// Where circle keys + the agreement secret live (keystore/file).
    data_dir: std::path::PathBuf,
}

impl<T: SyncTransport> SyncEngine<T> {
    pub fn new(
        transport: T,
        store: Arc<Store>,
        vault: [u8; 32],
        device: DeviceId,
        data_dir: &std::path::Path,
    ) -> Self {
        Self {
            transport,
            store,
            vault,
            device,
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Seal every locally-changed synchronized memory and push it.
    pub async fn push(&self) -> Result<SyncOutcome> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, type, content, source, created_at, updated_at,
                        confidence, importance, privacy_level, entities_json,
                        embedding, deleted, conversation_id, share_circle
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
                    r.get::<_, Option<String>>(13)?,
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
            share_circle,
        ) in rows
        {
            // A circle-tagged row seals under that circle's key, not the
            // vault — vault peers outside the circle can't open it.
            // Missing local key (left circle, stale row) -> skip, no push.
            let seal_key = match &share_circle {
                None => self.vault,
                Some(c) => match crypto::circle_key(&self.data_dir, c)? {
                    Some(k) => k,
                    None => {
                        tracing::warn!(memory = %id, circle = %c,
                            "no circle key — memory stays unsynced");
                        out.skipped += 1;
                        continue;
                    }
                },
            };
            let updated_at = parse_ts(&updated);
            let version = updated_at.timestamp_millis().max(1) as u64;
            let key = format!("{MEMORY_PREFIX}{id}");
            if self.mirror_version(&key)? >= version {
                out.skipped += 1;
                continue;
            }
            let tombstone = deleted != 0;
            let ciphertext = if tombstone {
                crypto::seal(&seal_key, key.as_bytes(), b"")?
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
                crypto::seal(&seal_key, key.as_bytes(), &raw)?
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
        self.push_tasks(&mut out).await?;
        self.push_workflows(&mut out).await?;
        self.push_notifications(&mut out).await?;
        self.push_apps(&mut out).await?;
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

    /// `sync_scope='synchronized'` tasks — claim/lease columns included so
    /// a claim pushed by the runner propagates to peers on the next sync.
    async fn push_tasks(&self, out: &mut SyncOutcome) -> Result<()> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, title, agent_id, created_at, run_at, state,
                        trigger_json, payload_json, result_json, claimed_by,
                        lease_expires_at, COALESCE(updated_at, created_at),
                        deleted
                 FROM tasks WHERE sync_scope='synchronized'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, i64>(12)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for (
            id,
            title,
            agent,
            created,
            run_at,
            state,
            trigger,
            payload,
            result,
            claimed,
            lease,
            updated,
            deleted,
        ) in rows
        {
            let updated_at = parse_ts(&updated);
            let key = format!("{TASK_PREFIX}{id}");
            let tombstone = deleted != 0;
            let raw = if tombstone {
                Vec::new()
            } else {
                serde_json::to_vec(&TaskPayload {
                    v: 1,
                    id,
                    title,
                    agent_id: agent,
                    created_at: created,
                    run_at,
                    state,
                    trigger_json: trigger,
                    payload_json: payload,
                    result_json: result,
                    claimed_by: claimed,
                    lease_expires_at: lease,
                    updated_at: updated.clone(),
                })
                .map_err(|e| Error::Sync(e.to_string()))?
            };
            self.push_sealed(key, &raw, version_of(&updated), updated_at, tombstone, out)
                .await?;
        }
        Ok(())
    }

    /// `sync_scope='synchronized'` workflow definitions — the definition
    /// JSON travels opaque inside the sealed payload.
    async fn push_workflows(&self, out: &mut SyncOutcome) -> Result<()> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, definition_json, created_at, updated_at, deleted
                 FROM workflows WHERE sync_scope='synchronized'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for (id, name, def_json, created, updated, deleted) in rows {
            let updated_at = parse_ts(&updated);
            let key = format!("{WF_PREFIX}{id}");
            let tombstone = deleted != 0;
            let raw = if tombstone {
                Vec::new()
            } else {
                serde_json::to_vec(&WorkflowPayload {
                    v: 1,
                    id,
                    name,
                    definition_json: def_json,
                    created_at: created,
                    updated_at: updated.clone(),
                })
                .map_err(|e| Error::Sync(e.to_string()))?
            };
            self.push_sealed(key, &raw, version_of(&updated), updated_at, tombstone, out)
                .await?;
        }
        Ok(())
    }

    /// `sync_scope='synchronized'` notifications — inbox rows + read state.
    async fn push_notifications(&self, out: &mut SyncOutcome) -> Result<()> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, title, body, source, channel, created_at, read_at,
                        updated_at, deleted
                 FROM notifications WHERE sync_scope='synchronized'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for (id, title, body, source, channel, created, read_at, updated, deleted) in rows {
            let updated_at = parse_ts(&updated);
            let key = format!("{NTF_PREFIX}{id}");
            let tombstone = deleted != 0;
            let raw = if tombstone {
                Vec::new()
            } else {
                serde_json::to_vec(&NotificationPayload {
                    v: 1,
                    id,
                    title,
                    body,
                    source,
                    channel,
                    created_at: created,
                    read_at,
                    updated_at: updated.clone(),
                })
                .map_err(|e| Error::Sync(e.to_string()))?
            };
            self.push_sealed(key, &raw, version_of(&updated), updated_at, tombstone, out)
                .await?;
        }
        Ok(())
    }

    /// Installed app packages — the `apps` row is the bookkeeping; the
    /// bytes are read from `<data_dir>/apps/<id>/` at push time. `data/`
    /// (live app state) and the registry's `.bak` backups never travel;
    /// deleted rows ship as tombstones so removal propagates.
    async fn push_apps(&self, out: &mut SyncOutcome) -> Result<()> {
        let rows = self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, version, runtime, updated_at, deleted
                 FROM apps",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        for (id, name, version, runtime, updated, deleted) in rows {
            let updated_at = parse_ts(&updated);
            let key = format!("{APP_PREFIX}{id}");
            let tombstone = deleted != 0;
            let raw = if tombstone {
                Vec::new()
            } else {
                let dir = self.data_dir.join("apps").join(&id);
                if !dir.is_dir() {
                    tracing::warn!(app = %id, "apps row without package dir — skipped");
                    out.skipped += 1;
                    continue;
                }
                let payload = self.app_payload(&dir, &id, &name, &version, &runtime, &updated)?;
                serde_json::to_vec(&payload).map_err(|e| Error::Sync(e.to_string()))?
            };
            self.push_sealed(key, &raw, version_of(&updated), updated_at, tombstone, out)
                .await?;
        }
        Ok(())
    }

    /// Read an installed app dir into a sync payload: every package file
    /// base64'd except `signature.bin` (carried separately) and the
    /// reserved `data/` dir (runtime state, never package content).
    fn app_payload(
        &self,
        dir: &std::path::Path,
        id: &str,
        name: &str,
        version: &str,
        runtime: &str,
        updated: &str,
    ) -> Result<AppPayload> {
        let mut files = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).map_err(store_err)? {
                let p = e.map_err(store_err)?.path();
                let rel = p.strip_prefix(dir).map_err(store_err)?.to_path_buf();
                if p.is_dir() {
                    if rel.components().count() == 1
                        && pai_apps::AppPackage::RESERVED_DIRS
                            .contains(&rel.to_str().unwrap_or_default())
                    {
                        continue; // live app state stays device-local
                    }
                    stack.push(p);
                } else if rel == std::path::Path::new("signature.bin") {
                    continue; // carried as signature_b64 below
                } else if p.is_file() {
                    files.push(AppFileEntry {
                        path: rel.to_string_lossy().replace('\\', "/"),
                        b64: base64::engine::general_purpose::STANDARD
                            .encode(std::fs::read(&p).map_err(store_err)?),
                    });
                }
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let signature_b64 = base64::engine::general_purpose::STANDARD
            .encode(std::fs::read(dir.join("signature.bin")).unwrap_or_default());
        Ok(AppPayload {
            v: 1,
            id: id.into(),
            name: name.into(),
            version: version.into(),
            runtime: runtime.into(),
            updated_at: updated.into(),
            signature_b64,
            files,
        })
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
            // ckg/<circle>/<device> grants are sealed to the *target
            // device's* pairwise key — not the vault. Only attempt the
            // ones addressed to us; others stay unreadable by design.
            if obj.key.starts_with(CKG_PREFIX) {
                let addressed = obj.key.ends_with(&format!("/{}", self.device));
                if addressed {
                    match self.open_grant(&obj)? {
                        Some(g) => self.apply_grant(&g)?,
                        None => {
                            out.skipped += 1;
                            continue;
                        }
                    }
                    self.set_mirror(&obj)?;
                    out.pulled += 1;
                } else {
                    out.skipped += 1;
                }
                continue;
            }
            let (raw, circle) = match self.open_scoped(&obj)? {
                Some(v) => v,
                None => {
                    tracing::warn!(key = %obj.key, "skipping unopenable sync object");
                    out.skipped += 1;
                    continue;
                }
            };
            self.apply_obj(&obj, &raw, circle.as_deref())?;
            self.set_mirror(&obj)?;
            out.pulled += 1;
        }
        Ok(out)
    }

    /// Open an object under the key that owns it: the vault key for
    /// shared rows, else each locally-held circle key. Returns the
    /// plaintext plus which circle opened it — the circle a row belongs
    /// to is *derived from the key*, never trusted from the payload.
    fn open_scoped(&self, obj: &SyncObject) -> Result<Option<(Vec<u8>, Option<String>)>> {
        if let Ok(raw) = crypto::open(&self.vault, obj.key.as_bytes(), &obj.ciphertext) {
            return Ok(Some((raw, None)));
        }
        for c in crate::circle::list_circles(&self.store)? {
            if let Some(k) = crypto::circle_key(&self.data_dir, &c.name)? {
                if let Ok(raw) = crypto::open(&k, obj.key.as_bytes(), &obj.ciphertext) {
                    return Ok(Some((raw, Some(c.name))));
                }
            }
        }
        Ok(None)
    }

    /// Open a `ckg/<circle>/<device>` grant — sealed to a pairwise
    /// peer_key, so we try each trusted peer's key until one opens it.
    fn open_grant(&self, obj: &SyncObject) -> Result<Option<crate::circle::CircleGrant>> {
        let agree = crypto::agreement_key(self.device, &self.data_dir)?;
        for p in crate::pair::list_peers(&self.store)? {
            let wk = crypto::peer_key(
                &agree.secret,
                &x25519_dalek::PublicKey::from(p.agree_pubkey),
            );
            if let Ok(raw) = crypto::open(&wk, obj.key.as_bytes(), &obj.ciphertext) {
                let g: crate::circle::CircleGrant = serde_json::from_slice(&raw)
                    .map_err(|e| Error::Sync(format!("bad circle grant: {e}")))?;
                return Ok(Some(g));
            }
        }
        Ok(None)
    }

    /// A received grant: adopt the circle key (idempotent) and record
    /// the membership row so pull knows to try this key on `memory/`.
    fn apply_grant(&self, g: &crate::circle::CircleGrant) -> Result<()> {
        if g.v != 1 {
            return Err(Error::Sync(format!("unsupported grant v{}", g.v)));
        }
        let raw =
            hex::decode(&g.key).map_err(|e| Error::Sync(format!("bad circle grant key: {e}")))?;
        let key: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| Error::Sync("circle grant key wrong length".into()))?;
        crypto::adopt_circle_key(&self.data_dir, &g.circle, &key)?;
        crate::circle::record_membership(&self.store, &g.circle, &g.from)?;
        tracing::info!(circle = %g.circle, from = %g.from, "joined circle via grant");
        Ok(())
    }

    /// Grant `circle` membership to `peer`: emit `ckg/<circle>/<peer>`
    /// sealed to the pairwise key, pushed through the transport. Only a
    /// member can grant — we must hold the circle key to wrap it.
    pub async fn push_circle_grant(
        &self,
        circle: &str,
        peer: &crate::pair::SyncPeer,
    ) -> Result<()> {
        let key = crypto::circle_key(&self.data_dir, circle)?
            .ok_or_else(|| Error::Sync(format!("not a member of circle '{circle}'")))?;
        let agree = crypto::agreement_key(self.device, &self.data_dir)?;
        let wk = crypto::peer_key(
            &agree.secret,
            &x25519_dalek::PublicKey::from(peer.agree_pubkey),
        );
        let key_s = format!("{CKG_PREFIX}{circle}/{}", peer.device_id);
        let payload = serde_json::to_vec(&crate::circle::CircleGrant {
            v: 1,
            circle: circle.to_string(),
            key: hex::encode(key),
            from: self.device.to_string(),
            created_at: now().to_rfc3339(),
        })
        .map_err(|e| Error::Sync(e.to_string()))?;
        let obj = SyncObject {
            key: key_s.clone(),
            ciphertext: crypto::seal(&wk, key_s.as_bytes(), &payload)?,
            version: version_of(&now().to_rfc3339()),
            writer: self.device,
            updated_at: now(),
            tombstone: false,
        };
        self.transport.push(&obj).await?;
        self.set_mirror(&obj)?;
        Ok(())
    }

    /// Dispatch one opened object to its row-kind apply path.
    /// `circle` records which circle key opened it (None = vault).
    fn apply_obj(&self, obj: &SyncObject, raw: &[u8], circle: Option<&str>) -> Result<()> {
        let key = obj.key.as_str();
        if let Some(id) = key.strip_prefix(MEMORY_PREFIX) {
            if obj.tombstone {
                return self.apply_tombstone(id, obj.updated_at);
            }
            let p: MemoryPayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad memory payload: {e}")))?;
            return self.apply_memory(&p, circle);
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
        if let Some(id) = key.strip_prefix(TASK_PREFIX) {
            if obj.tombstone {
                return self.apply_task_tombstone(id, obj.updated_at);
            }
            let p: TaskPayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad task payload: {e}")))?;
            return self.apply_task(&p);
        }
        if let Some(id) = key.strip_prefix(WF_PREFIX) {
            if obj.tombstone {
                return self.apply_wf_tombstone(id, obj.updated_at);
            }
            let p: WorkflowPayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad workflow payload: {e}")))?;
            return self.apply_workflow(&p);
        }
        if let Some(id) = key.strip_prefix(NTF_PREFIX) {
            if obj.tombstone {
                return self.apply_ntf_tombstone(id, obj.updated_at);
            }
            let p: NotificationPayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad notification payload: {e}")))?;
            return self.apply_notification(&p);
        }
        if let Some(id) = key.strip_prefix(APP_PREFIX) {
            if obj.tombstone {
                return self.apply_app_tombstone(id, obj.updated_at);
            }
            let p: AppPayload = serde_json::from_slice(raw)
                .map_err(|e| Error::Sync(format!("bad app payload: {e}")))?;
            return self.apply_app(&p);
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

    /// Apply a synced app package: stage files, verify the signature
    /// against every known signer (own devices + paired peers), install
    /// via `install_trusted`, and upsert the `apps` bookkeeping row.
    /// Unverifiable packages are skipped — never installed, never run.
    fn apply_app(&self, p: &AppPayload) -> Result<()> {
        if p.v != 1 {
            return Err(Error::Sync(format!("unsupported app payload v{}", p.v)));
        }
        let stage = self
            .data_dir
            .join("apps")
            .join(format!(".staging-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&stage).map_err(store_err)?;
        let staged = (|| -> Result<()> {
            std::fs::write(
                stage.join("signature.bin"),
                base64::engine::general_purpose::STANDARD
                    .decode(&p.signature_b64)
                    .map_err(|e| Error::Sync(format!("bad app signature b64: {e}")))?,
            )
            .map_err(store_err)?;
            for f in &p.files {
                pai_apps::check_rel_path(&f.path)
                    .map_err(|e| Error::Sync(format!("app file path: {e}")))?;
                let to = stage.join(&f.path);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent).map_err(store_err)?;
                }
                std::fs::write(
                    &to,
                    base64::engine::general_purpose::STANDARD
                        .decode(&f.b64)
                        .map_err(|e| Error::Sync(format!("bad app file b64: {e}")))?,
                )
                .map_err(store_err)?;
            }
            Ok(())
        })();
        let result = staged.and_then(|_| -> Result<bool> {
            let pkg = pai_apps::AppPackage::load(&stage)
                .map_err(|e| Error::Sync(format!("bad synced package: {e}")))?;
            if pkg.manifest.app_id() != p.id {
                return Err(Error::Sync(format!(
                    "app id mismatch: key says {}, manifest says {}",
                    p.id,
                    pkg.manifest.app_id()
                )));
            }
            // Signer candidates: every local device key + every paired
            // peer's ed_pubkey.
            let ids = pai_identity::IdentityStore::new(self.store.clone());
            let mut cands: Vec<(DeviceId, [u8; 32])> = self.store.with_conn(|c| {
                let mut v = Vec::new();
                let mut s = c.prepare("SELECT id, public_key FROM devices")?;
                for r in s.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })? {
                    let (id, key) = r?;
                    if let Ok(k) = <[u8; 32]>::try_from(key.as_slice()) {
                        let u = uuid::Uuid::parse_str(&id).unwrap_or_else(|_| uuid::Uuid::nil());
                        v.push((DeviceId(u), k));
                    }
                }
                Ok(v)
            })?;
            for peer in crate::pair::list_peers(&self.store)? {
                cands.push((peer.device_id, peer.ed_pubkey));
            }
            // An unverifiable signature is permanent — warn + skip the
            // object (pull continues; it never installs, never retries)
            // rather than poisoning the whole pull batch.
            let signer = match pkg.verify_any_key(&ids, &cands) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(app = %p.id, "rejected synced app: {e}");
                    return Ok(false);
                }
            };
            pai_apps::AppRegistry::new(&self.data_dir)
                .install_trusted(&pkg, true)
                .map_err(|e| Error::Sync(format!("app install: {e}")))?;
            self.store.with_conn(|c| {
                c.execute(
                    "INSERT INTO apps(id, name, version, runtime, installed_at,
                        updated_at, deleted) VALUES(?1,?2,?3,?4,?5,?6,0)
                     ON CONFLICT(id) DO UPDATE SET name=excluded.name,
                        version=excluded.version, runtime=excluded.runtime,
                        updated_at=excluded.updated_at, deleted=0",
                    params![p.id, p.name, p.version, p.runtime, ts(&now()), p.updated_at],
                )?;
                Ok(())
            })?;
            tracing::info!(app = %p.id, signer = %signer, "installed synced app");
            Ok(true)
        });
        let _ = std::fs::remove_dir_all(&stage);
        result.map(|_| ())
    }

    /// App tombstone: mark the row deleted and remove the package dir.
    fn apply_app_tombstone(&self, id: &str, updated_at: Timestamp) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE apps SET deleted=1, updated_at=?2 WHERE id=?1",
                params![id, ts(&updated_at)],
            )?;
            Ok(())
        })?;
        let _ = pai_apps::AppRegistry::new(&self.data_dir).remove(id);
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

    fn apply_memory(&self, p: &MemoryPayload, circle: Option<&str>) -> Result<()> {
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
                    conversation_id, share_circle)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,'synchronized',?12,?13)
                 ON CONFLICT(id) DO UPDATE SET
                    type=excluded.type, content=excluded.content,
                    source=excluded.source, updated_at=excluded.updated_at,
                    confidence=excluded.confidence,
                    importance=excluded.importance,
                    privacy_level=excluded.privacy_level,
                    entities_json=excluded.entities_json,
                    embedding=excluded.embedding, deleted=0,
                    conversation_id=excluded.conversation_id,
                    share_circle=excluded.share_circle",
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
                    circle,
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

    /// Task tombstone: mark deleted locally (claim fields irrelevant —
    /// a tombstoned task never enters the due set).
    fn apply_task_tombstone(&self, id: &str, remote_updated: Timestamp) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET deleted=1, updated_at=?2
                 WHERE id=?1 AND COALESCE(updated_at, created_at) < ?2",
                params![id, ts(&remote_updated)],
            )
        })?;
        Ok(())
    }

    /// Apply a task row LWW-style — the whole row (including claimed_by /
    /// lease_expires_at) goes to the newest writer, so two devices that
    /// claimed the same task before seeing each other's push converge on
    /// whichever claim has the later updated_at. A device only runs a
    /// task it believes it claimed, so the losing claimant's next due
    /// scan finds the row already claimed (or done) and skips it.
    fn apply_task(&self, p: &TaskPayload) -> Result<()> {
        if p.v != 1 {
            return Err(Error::Sync(format!("unsupported payload v{}", p.v)));
        }
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO tasks(id, title, agent_id, created_at, run_at,
                    state, updated_at, deleted, sync_scope, trigger_json,
                    payload_json, result_json, claimed_by, lease_expires_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,0,'synchronized',?8,?9,?10,?11,?12)
                 ON CONFLICT(id) DO UPDATE SET
                    title=excluded.title, run_at=excluded.run_at,
                    state=excluded.state, updated_at=excluded.updated_at,
                    trigger_json=excluded.trigger_json,
                    payload_json=excluded.payload_json,
                    result_json=excluded.result_json,
                    claimed_by=excluded.claimed_by,
                    lease_expires_at=excluded.lease_expires_at, deleted=0
                 WHERE excluded.updated_at >
                    COALESCE(tasks.updated_at, tasks.created_at)",
                params![
                    p.id,
                    p.title,
                    p.agent_id,
                    p.created_at,
                    p.run_at,
                    p.state,
                    p.updated_at,
                    p.trigger_json,
                    p.payload_json,
                    p.result_json,
                    p.claimed_by,
                    p.lease_expires_at,
                ],
            )
        })?;
        Ok(())
    }

    /// Workflow tombstone.
    fn apply_wf_tombstone(&self, id: &str, remote_updated: Timestamp) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE workflows SET deleted=1, updated_at=?2
                 WHERE id=?1 AND updated_at < ?2",
                params![id, ts(&remote_updated)],
            )
        })?;
        Ok(())
    }

    /// Apply a workflow definition LWW-style. `definition_json` is stored
    /// verbatim — `WorkflowDefinition::validate` runs on load/run, so a
    /// synced def with an out-of-allowlist tool step can't execute.
    fn apply_workflow(&self, p: &WorkflowPayload) -> Result<()> {
        if p.v != 1 {
            return Err(Error::Sync(format!("unsupported payload v{}", p.v)));
        }
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO workflows(id, name, definition_json, sync_scope,
                    created_at, updated_at, deleted)
                 VALUES(?1,?2,?3,'synchronized',?4,?5,0)
                 ON CONFLICT(id) DO UPDATE SET
                    name=excluded.name, definition_json=excluded.definition_json,
                    updated_at=excluded.updated_at, deleted=0
                 WHERE excluded.updated_at > workflows.updated_at",
                params![p.id, p.name, p.definition_json, p.created_at, p.updated_at],
            )
        })?;
        Ok(())
    }

    /// Notification tombstone.
    fn apply_ntf_tombstone(&self, id: &str, remote_updated: Timestamp) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE notifications SET deleted=1, updated_at=?2
                 WHERE id=?1 AND updated_at < ?2",
                params![id, ts(&remote_updated)],
            )
        })?;
        Ok(())
    }

    /// Apply a notification LWW-style — `read_at` travels so reading on
    /// one device marks it read everywhere.
    fn apply_notification(&self, p: &NotificationPayload) -> Result<()> {
        if p.v != 1 {
            return Err(Error::Sync(format!("unsupported payload v{}", p.v)));
        }
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO notifications(id, title, body, source, channel,
                    created_at, read_at, sync_scope, updated_at, deleted)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'synchronized',?8,0)
                 ON CONFLICT(id) DO UPDATE SET
                    title=excluded.title, body=excluded.body,
                    read_at=excluded.read_at, updated_at=excluded.updated_at,
                    deleted=0
                 WHERE excluded.updated_at > notifications.updated_at",
                params![
                    p.id,
                    p.title,
                    p.body,
                    p.source,
                    p.channel,
                    p.created_at,
                    p.read_at,
                    p.updated_at
                ],
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
        data_dir,
    ))
}

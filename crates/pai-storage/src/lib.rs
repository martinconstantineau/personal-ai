//! Local-first storage.
//!
//! Layout under `data_dir`:
//! - `personal-ai.db` — SQLite: structured data + FTS5 keyword index.
//! - `blobs/` — content-addressed blob store (documents, media, audio).
//!
//! SQLite was chosen for transactional integrity, zero-admin local operation,
//! and full cross-platform support (see docs/adr/0004-storage-sqlite.md).
//! Field-level encryption is layered on for sensitive columns (see
//! docs/architecture/security.md); disk/OS encryption is the device boundary.

use pai_core::{Error, Result, Timestamp};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const SCHEMA_VERSION: u32 = 8;

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    platform TEXT NOT NULL,
    public_key BLOB NOT NULL,
    registered_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    capabilities_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    device_id TEXT NOT NULL REFERENCES devices(id),
    started_at TEXT NOT NULL,
    ended_at TEXT
);

CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    title TEXT,
    created_at TEXT NOT NULL,
    sync_scope TEXT NOT NULL DEFAULT 'device_local'
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    role TEXT NOT NULL,
    trust TEXT NOT NULL,
    created_at TEXT NOT NULL,
    content_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation
    ON messages(conversation_id, created_at);

CREATE TABLE IF NOT EXISTS models (
    id TEXT PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    family TEXT NOT NULL,
    provider TEXT NOT NULL,
    capabilities_json TEXT NOT NULL,
    context_length INTEGER NOT NULL,
    quantization TEXT,
    size_bytes INTEGER NOT NULL,
    requirements_json TEXT NOT NULL,
    local INTEGER NOT NULL,
    license TEXT,
    installed INTEGER NOT NULL DEFAULT 0,
    path TEXT,
    sha256 TEXT
);

CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    content TEXT NOT NULL,
    source TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    confidence REAL NOT NULL,
    importance REAL NOT NULL,
    privacy_level TEXT NOT NULL,
    entities_json TEXT NOT NULL,
    embedding BLOB,
    deleted INTEGER NOT NULL DEFAULT 0,
    sync_scope TEXT NOT NULL DEFAULT 'synchronized'
);
CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(type, deleted);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
    USING fts5(content, content='memories', content_rowid='rowid');

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TABLE IF NOT EXISTS memory_relationships (
    from_id TEXT NOT NULL REFERENCES memories(id),
    to_id TEXT NOT NULL,
    rel_type TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (from_id, to_id, rel_type)
);

CREATE TABLE IF NOT EXISTS audit_events (
    id TEXT PRIMARY KEY,
    at TEXT NOT NULL,
    device_id TEXT,
    agent_id TEXT,
    run_id TEXT,
    conversation_id TEXT,
    kind TEXT NOT NULL,
    tool TEXT,
    detail_json TEXT NOT NULL,
    outcome TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_at ON audit_events(at);

CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    run_at TEXT,
    state TEXT NOT NULL,
    sync_scope TEXT NOT NULL DEFAULT 'synchronized'
);

CREATE TABLE IF NOT EXISTS sync_objects (
    key TEXT PRIMARY KEY,
    ciphertext BLOB NOT NULL,
    version INTEGER NOT NULL,
    writer TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    tombstone INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS documents (
    id TEXT PRIMARY KEY,
    title TEXT,
    mime TEXT NOT NULL,
    blob TEXT NOT NULL,
    created_at TEXT NOT NULL,
    trust TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS document_sections (
    document_id TEXT NOT NULL,
    section INTEGER NOT NULL,
    text TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts
    USING fts5(text, content='document_sections', content_rowid='rowid');

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#,
    r#"
-- V2: per-conversation memory scoping, run checkpoints, persisted policies.
ALTER TABLE memories ADD COLUMN conversation_id TEXT;
ALTER TABLE conversations ADD COLUMN memory_scope TEXT NOT NULL DEFAULT 'shared';
CREATE INDEX IF NOT EXISTS idx_memories_conversation
    ON memories(conversation_id, deleted);

CREATE TABLE IF NOT EXISTS agent_runs (
    id TEXT PRIMARY KEY,
    agent_id TEXT,
    conversation_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    state TEXT NOT NULL,
    step INTEGER NOT NULL DEFAULT 0,
    input TEXT NOT NULL DEFAULT '',
    checkpoint_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_agent_runs_open
    ON agent_runs(ended_at, state);

CREATE TABLE IF NOT EXISTS policies (
    permission TEXT PRIMARY KEY,
    policy TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
"#,
    r#"
-- V3: OS-keystore marker for device keys; embeddings on document sections.
ALTER TABLE devices ADD COLUMN key_storage TEXT NOT NULL DEFAULT 'file';
ALTER TABLE document_sections ADD COLUMN embedding BLOB;
"#,
    r#"
-- V4: trusted sync peers — devices paired via the signed offer/accept
-- exchange in `pai-sync`. Kept separate from `devices` (self-registered
-- rows) so pairing state is an explicit trust decision, not a capability
-- record.
CREATE TABLE IF NOT EXISTS sync_peers (
    device_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    platform TEXT NOT NULL,
    ed_pubkey BLOB NOT NULL,
    agree_pubkey BLOB NOT NULL,
    paired_at TEXT NOT NULL
);
"#,
    r#"
-- V5: LWW metadata for conversation/document sync — `updated_at` versions
-- every mutable row and `deleted` records tombstones the sync engine can
-- propagate (mirroring the memories model).
ALTER TABLE conversations ADD COLUMN updated_at TEXT;
UPDATE conversations SET updated_at = created_at;
ALTER TABLE conversations ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE documents ADD COLUMN sync_scope TEXT NOT NULL DEFAULT 'device_local';
ALTER TABLE documents ADD COLUMN updated_at TEXT;
UPDATE documents SET updated_at = created_at;
ALTER TABLE documents ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;
"#,
    r#"
-- V6: syncable tasks — LWW metadata + tombstones like the other kinds,
-- persisted trigger/payload/result so a task created on one device can
-- execute on another, and claim/lease fields (`claimed_by`,
-- `lease_expires_at`) so only one device runs a due task at a time.
ALTER TABLE tasks ADD COLUMN updated_at TEXT;
UPDATE tasks SET updated_at = created_at;
ALTER TABLE tasks ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN trigger_json TEXT NOT NULL DEFAULT '{"kind":"manual"}';
ALTER TABLE tasks ADD COLUMN payload_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE tasks ADD COLUMN result_json TEXT;
ALTER TABLE tasks ADD COLUMN claimed_by TEXT;
ALTER TABLE tasks ADD COLUMN lease_expires_at TEXT;
"#,
    r#"
-- V7: workflows — declarative multi-step definitions (syncable like
-- tasks) + per-device run records with a step cursor for crash resume.
-- `definition_json` holds the step list + tool allowlist; runs stay
-- device-local (like agent_runs) and never sync.
CREATE TABLE workflows (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    definition_json TEXT NOT NULL,
    sync_scope TEXT NOT NULL DEFAULT 'synchronized',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE workflow_runs (
    id TEXT PRIMARY KEY,
    workflow_id TEXT NOT NULL REFERENCES workflows(id),
    input TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    step_index INTEGER NOT NULL DEFAULT 0,
    outputs_json TEXT NOT NULL DEFAULT '{}',
    error TEXT,
    started_at TEXT NOT NULL,
    finished_at TEXT
);
"#,
    r#"
-- V8: notifications — the proactive inbox. Rows are the durable record;
-- external channels (email-to-self, webhook) are configured in
-- notify.json and only fan out when present. `read_at` syncs so the
-- inbox follows the user across devices.
CREATE TABLE notifications (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT '',
    channel TEXT NOT NULL DEFAULT 'inbox',
    created_at TEXT NOT NULL,
    read_at TEXT,
    sync_scope TEXT NOT NULL DEFAULT 'synchronized',
    updated_at TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0
);
"#,
];

/// A 32-byte SQLCipher raw key, sourced from the OS keystore (or a 0600
/// file fallback) by the caller — see `pai_identity::keystore::store_key`.
pub type EncryptionKey = [u8; 32];

/// Handle to the platform's local storage.
pub struct Store {
    conn: Mutex<Connection>,
    blob_dir: PathBuf,
}

impl Store {
    /// Open (creating + migrating if needed) the store under `data_dir`.
    ///
    /// `key` controls at-rest encryption (SQLCipher):
    /// - `Some(k)`: opens the DB encrypted; a pre-existing plaintext DB is
    ///   migrated in place via `sqlcipher_export`.
    /// - `None`: plaintext — used by tests and the `PAI_PLAINTEXT_STORE`
    ///   escape hatch.
    pub fn open(data_dir: &Path, key: Option<&EncryptionKey>) -> Result<Self> {
        std::fs::create_dir_all(data_dir).map_err(store_err)?;
        let db_path = data_dir.join("personal-ai.db");
        if let Some(k) = key {
            if db_path.exists() && Self::is_plaintext(&db_path)? {
                Self::migrate_to_encrypted(&db_path, k)?;
            }
        }
        let conn = Connection::open(&db_path).map_err(store_err)?;
        if let Some(k) = key {
            // Raw 32-byte key — skips PBKDF; the keystore/file already
            // gates access.
            conn.pragma_update(None, "key", format!("x'{}'", hex::encode(k)))
                .map_err(store_err)?;
        }
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(store_err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(store_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(store_err)?;
        // Probe: wrong/absent key surfaces here, not mid-query later.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .map_err(|_| Error::Storage("cannot read store: bad or missing key".into()))?;
        let store = Self {
            conn: Mutex::new(conn),
            blob_dir: data_dir.join("blobs"),
        };
        store.migrate()?;
        std::fs::create_dir_all(&store.blob_dir).map_err(store_err)?;
        Ok(store)
    }

    /// True when `path` is a readable *plaintext* SQLite db. SQLCipher with
    /// no key set behaves as plain SQLite, so a successful probe = plaintext.
    fn is_plaintext(db_path: &Path) -> Result<bool> {
        let conn = Connection::open(db_path).map_err(store_err)?;
        match conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(())) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::NotADatabase =>
            {
                Ok(false)
            }
            Err(e) => Err(store_err(e)),
        }
    }

    /// Copy a plaintext db into an encrypted one and swap with a backup.
    fn migrate_to_encrypted(db_path: &Path, key: &EncryptionKey) -> Result<()> {
        let enc_path = db_path.with_extension("enc");
        let bak_path = db_path.with_extension("plaintext-bak");
        for p in [&enc_path, &bak_path] {
            if p.exists() {
                std::fs::remove_file(p).map_err(store_err)?;
            }
        }
        {
            let conn = Connection::open(db_path).map_err(store_err)?;
            // Fold any pending WAL frames into the main file first.
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            let esc = enc_path.to_string_lossy().replace('\'', "''");
            conn.execute_batch(&format!(
                "ATTACH DATABASE '{esc}' AS enc KEY \"x'{}'\";
                 SELECT sqlcipher_export('enc');
                 DETACH DATABASE enc;",
                hex::encode(key)
            ))
            .map_err(store_err)?;
        }
        // Windows rename fails when the destination exists — rotate via a
        // backup instead of renaming over.
        std::fs::rename(db_path, &bak_path).map_err(store_err)?;
        if let Err(e) = std::fs::rename(&enc_path, db_path) {
            let _ = std::fs::rename(&bak_path, db_path); // roll back
            return Err(store_err(e));
        }
        for ext in ["wal", "shm"] {
            let _ = std::fs::remove_file(db_path.with_extension(ext));
        }
        let _ = std::fs::remove_file(&bak_path);
        tracing::info!("migrated store.db to SQLCipher (encrypted at rest)");
        Ok(())
    }

    /// In-memory store for tests.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(store_err)?;
        let dir = std::env::temp_dir().join(format!("pai-blobs-{}", uuid::Uuid::new_v4()));
        let store = Self {
            conn: Mutex::new(conn),
            blob_dir: dir,
        };
        store.migrate()?;
        std::fs::create_dir_all(&store.blob_dir).map_err(store_err)?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let current: u32 = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            conn.execute_batch(migration).map_err(store_err)?;
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![(i + 1).to_string()],
            )
            .map_err(store_err)?;
        }
        Ok(())
    }

    /// Run `f` with the underlying connection. Keeps SQL inside one crate.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        f(&conn).map_err(store_err)
    }

    // -- Blobs ------------------------------------------------------------

    /// Write bytes; returns the content-addressed blob id (hex sha256).
    pub fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let id = hex::encode(Sha256::digest(bytes));
        let path = self.blob_dir.join(&id);
        if !path.exists() {
            std::fs::write(&path, bytes).map_err(store_err)?;
        }
        Ok(id)
    }

    pub fn get_blob(&self, id: &str) -> Result<Vec<u8>> {
        let path = self.blob_dir.join(id);
        std::fs::read(&path).map_err(|_| Error::NotFound(format!("blob {id}")))
    }

    pub fn blob_path(&self, id: &str) -> PathBuf {
        self.blob_dir.join(id)
    }
}

pub fn store_err(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

pub fn ts(t: &Timestamp) -> String {
    t.to_rfc3339()
}

pub fn parse_ts(s: &str) -> Timestamp {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now())
}

//! Cross-device synchronization.
//!
//! Model: `SyncObject`s — opaque, encrypted, versioned blobs keyed by a
//! deterministic path ("memory/<id>", "prefs/<key>"). Transports are
//! pluggable; the free/local default is a **folder transport** — point it at
//! a Syncthing/rsync/NFS-shared directory and devices converge without any
//! server. E2E encryption: objects are ciphertext to the transport.
//!
//! Implemented now: folder transport (push/pull/list, last-writer-wins).
//! Documented-not-built: CRDT merge for richer types, NAT traversal.

use async_trait::async_trait;
use pai_core::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[async_trait]
pub trait SyncTransport: Send + Sync {
    fn id(&self) -> &'static str;
    async fn list(&self) -> Result<Vec<SyncObjectMeta>>;
    async fn push(&self, obj: &SyncObject) -> Result<()>;
    async fn pull(&self, key: &str) -> Result<Option<SyncObject>>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncObjectMeta {
    pub key: String,
    pub version: u64,
    pub writer: DeviceId,
    pub updated_at: Timestamp,
    pub tombstone: bool,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    obj: SyncObject,
}

/// File-per-object transport. Free, local, works with any folder-sync tool.
pub struct FolderTransport {
    dir: PathBuf,
}

impl FolderTransport {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir).map_err(|e| Error::Sync(format!("mkdir {dir:?}: {e}")))?;
        Ok(Self { dir })
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.syncobj", urlenc(key)))
    }
}

fn urlenc(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_string()
            } else {
                format!("%{:02x}", c as u32)
            }
        })
        .collect()
}

#[async_trait]
impl SyncTransport for FolderTransport {
    fn id(&self) -> &'static str {
        "folder"
    }

    async fn list(&self) -> Result<Vec<SyncObjectMeta>> {
        let mut out = vec![];
        let entries = std::fs::read_dir(&self.dir).map_err(|e| Error::Sync(e.to_string()))?;
        for e in entries.flatten() {
            let Ok(raw) = std::fs::read(e.path()) else {
                continue;
            };
            let Ok(w) = serde_json::from_slice::<Wire>(&raw) else {
                continue;
            };
            out.push(SyncObjectMeta {
                key: w.obj.key,
                version: w.obj.version,
                writer: w.obj.writer,
                updated_at: w.obj.updated_at,
                tombstone: w.obj.tombstone,
            });
        }
        Ok(out)
    }

    async fn push(&self, obj: &SyncObject) -> Result<()> {
        let path = self.path(&obj.key);
        let tmp = path.with_extension("part");
        let raw = serde_json::to_vec(&Wire { obj: obj.clone() })
            .map_err(|e| Error::Sync(e.to_string()))?;
        std::fs::write(&tmp, raw).map_err(|e| Error::Sync(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::Sync(e.to_string()))?;
        Ok(())
    }

    async fn pull(&self, key: &str) -> Result<Option<SyncObject>> {
        let path = self.path(key);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&path).map_err(|e| Error::Sync(e.to_string()))?;
        let w: Wire = serde_json::from_slice(&raw).map_err(|e| Error::Sync(e.to_string()))?;
        Ok(Some(w.obj))
    }
}

/// Last-writer-wins merge: pick the higher (version, updated_at) object.
/// Deliberately simple; CRDT merge is on the roadmap for rich types.
pub fn merge_lww(a: &SyncObject, b: &SyncObject) -> SyncObject {
    if (b.version, b.updated_at) > (a.version, a.updated_at) {
        b.clone()
    } else {
        a.clone()
    }
}

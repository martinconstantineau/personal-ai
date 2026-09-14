//! Cross-device synchronization.
//!
//! Model: `SyncObject`s — opaque, encrypted, versioned blobs keyed by a
//! deterministic path ("memory/<id>", "prefs/<key>"). Transports are
//! pluggable; the free/local default is a **folder transport** — point it at
//! a Syncthing/rsync/NFS-shared directory and devices converge without any
//! server. E2E encryption: objects are ciphertext to the transport.
//!
//! Implemented now: folder transport (push/pull/list, last-writer-wins),
//! X25519 pairing + XChaCha20-Poly1305 sealed objects (`crypto`, `pair`),
//! a memory-sync engine (`engine`), and an HTTP relay transport
//! (`relay`) — ciphertext-only objects over the network, no shared
//! folder needed. Documented-not-built: CRDT merge for richer types,
//! NAT traversal, vault rotation / unpairing.

pub mod circle;
pub mod crypto;
pub mod engine;
pub mod pair;
pub mod relay;
pub mod rotate;

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
    /// Remove an object. Transports that can't delete simply keep it —
    /// callers treat this as best-effort GC, never a correctness step.
    async fn delete(&self, _key: &str) -> Result<()> {
        Ok(())
    }
}

/// Lets `SyncEngine` take a runtime-picked transport (folder vs relay).
#[async_trait]
impl SyncTransport for Box<dyn SyncTransport> {
    fn id(&self) -> &'static str {
        (**self).id()
    }
    async fn list(&self) -> Result<Vec<SyncObjectMeta>> {
        (**self).list().await
    }
    async fn push(&self, obj: &SyncObject) -> Result<()> {
        (**self).push(obj).await
    }
    async fn pull(&self, key: &str) -> Result<Option<SyncObject>> {
        (**self).pull(key).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        (**self).delete(key).await
    }
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

/// Percent-encode an object key for paths/URLs (shared by transports).
pub fn urlenc(s: &str) -> String {
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

/// Inverse of [`urlenc`].
pub fn urldec(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(h) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(h);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

    async fn delete(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Sync(e.to_string())),
        }
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

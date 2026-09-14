//! Shared-memory circles — opt-in family/team scopes inside a vault.
//!
//! A circle is a named symmetric key (see [`crate::crypto::circle_key`])
//! held by a subset of vault members. Memories tagged
//! `share_circle = name` seal under the circle key instead of the vault
//! key, so non-member peers receive objects they cannot open and skip
//! them — the federation boundary is cryptographic, not advisory.
//!
//! Keys move member-by-member through `ckg/<circle>/<device>` grant
//! objects sealed to the recipient's pairwise [`crate::crypto::peer_key`]
//! — the same wrap primitive that bootstraps the vault during pairing,
//! so a grant is readable only by its target device.
//!
//! Caveat: leaving a circle is forward-only — a departing member keeps
//! whatever it already decrypted (you can't un-share what's been read),
//! and dropped keys mean the device stops applying future circle
//! objects.

use pai_core::*;
use pai_storage::{ts, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Grant payload inside a `ckg/<circle>/<device>` object — carries the
/// circle key to exactly one member device.
#[derive(Debug, Serialize, Deserialize)]
pub struct CircleGrant {
    pub v: u8,
    pub circle: String,
    /// Hex of the 32-byte circle key.
    pub key: String,
    /// Granting device id (also implied by which peer_key opened it).
    pub from: String,
    pub created_at: String,
}

/// One locally-known circle (bookkeeping row; membership = key held).
#[derive(Debug)]
pub struct CircleInfo {
    pub name: String,
    pub created_by: String,
    pub created_at: Timestamp,
}

/// Create/join bookkeeping row + generate the circle key when absent.
/// Returns the key so callers can seal objects immediately.
pub fn create_circle(
    store: &Arc<Store>,
    data_dir: &Path,
    name: &str,
    by: DeviceId,
) -> Result<[u8; 32]> {
    validate_name(name)?;
    let key = crate::crypto::circle_key_or_generate(data_dir, name)?;
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO circles(name, created_by, created_at, deleted)
             VALUES(?1,?2,?3,0)
             ON CONFLICT(name) DO UPDATE SET deleted=0",
            params![name, by.to_string(), ts(&now())],
        )?;
        Ok(())
    })?;
    Ok(key)
}

/// Live (non-deleted) circle names this device has bookkeeping for —
/// used by pull to try candidate keys on unopenable objects.
pub fn list_circles(store: &Arc<Store>) -> Result<Vec<CircleInfo>> {
    store.with_conn(|c| {
        let mut stmt = c.prepare(
            "SELECT name, created_by, created_at FROM circles
             WHERE deleted=0 ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CircleInfo {
                name: r.get(0)?,
                created_by: r.get(1)?,
                created_at: pai_storage::parse_ts(&r.get::<_, String>(2)?),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
}

/// Leave a circle: drop the key and fence off the rows it federated —
/// they become `device_local` so nothing leaks to the wider vault.
pub fn leave_circle(store: &Arc<Store>, data_dir: &Path, name: &str) -> Result<usize> {
    crate::crypto::drop_circle_key(data_dir, name);
    let n = store.with_conn(|c| {
        c.execute("UPDATE circles SET deleted=1 WHERE name=?1", params![name])?;
        c.execute(
            "UPDATE memories SET sync_scope='device_local', share_circle=NULL
             WHERE share_circle=?1",
            params![name],
        )
    })?;
    Ok(n)
}

/// Record a circle from a received grant (created_by = the granter).
pub fn record_membership(store: &Arc<Store>, name: &str, by: &str) -> Result<()> {
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO circles(name, created_by, created_at, deleted)
             VALUES(?1,?2,?3,0)
             ON CONFLICT(name) DO UPDATE SET deleted=0",
            params![name, by, ts(&now())],
        )?;
        Ok(())
    })
}

/// Circle names are object-key segments — keep them filesystem/key safe.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::InvalidInput(
            "circle name must be 1-64 alphanumeric/-/_ chars".into(),
        ));
    }
    Ok(())
}

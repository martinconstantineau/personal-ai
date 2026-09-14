//! Vault rotation — replace the shared vault key and distribute the new
//! one to every paired peer over the sync transport.
//!
//! Rotation objects ride at `vrot/<to>/<from>` keys — the sync engine
//! ignores the prefix, same as broker objects. Unlike sync payloads the
//! ciphertext is **peer-ECDH-sealed** (`peer_key(agree, peer_pubkey)`),
//! not vault-sealed: delivery can't depend on vault state because the
//! whole point is that vault state is changing. The addressed peer
//! opens it with its own agreement key, verifies the sender's ed25519
//! signature over the rotation transcript, and adopts only when the
//! epoch is newer than its stored one — replayed/stale rotations are
//! ignored, and nobody off the peer list can mint one.
//!
//! Trust note: vault membership is already "read+write everything" —
//! rotation adds revocation, not a new capability. A removed peer that
//! never contacts the transport again simply keeps stale ciphertext it
//! can no longer read.

use crate::{crypto, pair, SyncTransport};
use chacha20poly1305::aead::OsRng;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use pai_core::*;
use pai_identity::IdentityStore;
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use std::path::Path;
use x25519_dalek::PublicKey as XPublic;

/// Object-key prefix: `vrot/<to-device>/<from-device>`.
const ROT_PREFIX: &str = "vrot/";
const ROT_AAD: &[u8] = b"pai-sync-v1 vault-rotation";
const ROT_DOMAIN: &[u8] = b"pai-vrot-v1";

#[derive(Debug, Serialize, Deserialize)]
struct RotationMsg {
    v: u8,
    /// ms since epoch — strictly increasing adoption guard.
    epoch: u64,
    /// base64 of the new 32-byte vault key.
    vault_b64: String,
    /// Rotator's device id — also in the object key; the sync_peers row
    /// for this device supplies the verification keys.
    from: String,
    /// hex ed25519 signature over `rot_transcript`.
    sig: String,
}

fn rot_transcript(epoch: u64, vault: &[u8; 32], from: &str) -> Vec<u8> {
    let mut t = Vec::with_capacity(ROT_DOMAIN.len() + 80);
    t.extend_from_slice(ROT_DOMAIN);
    t.extend_from_slice(&epoch.to_le_bytes());
    t.extend_from_slice(vault);
    t.extend_from_slice(from.as_bytes());
    t
}

// -- local epoch ------------------------------------------------------------

fn epoch_file(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("sync-vault.epoch")
}

/// Highest rotation epoch adopted locally (0 = never rotated).
pub fn vault_epoch(data_dir: &Path) -> u64 {
    std::fs::read_to_string(epoch_file(data_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn store_epoch(data_dir: &Path, epoch: u64) -> Result<()> {
    std::fs::create_dir_all(data_dir).map_err(|e| Error::Storage(e.to_string()))?;
    std::fs::write(epoch_file(data_dir), epoch.to_string())
        .map_err(|e| Error::Storage(e.to_string()))
}

/// Replace the vault key outright — the deliberate rotation path
/// (`adopt_vault_key` refuses this on purpose; rotation is the exception).
fn rotate_vault_key(data_dir: &Path, key: &[u8; 32], epoch: u64) -> Result<()> {
    // store_vault is private to crypto.rs — write through the same path
    // adopt uses: clear then adopt.
    crypto::reset_vault_key(data_dir)?;
    crypto::adopt_vault_key(data_dir, key)?;
    store_epoch(data_dir, epoch)
}

// -- rotate (sender) --------------------------------------------------------

/// Mint a fresh vault key, seal it to every paired peer as a `vrot`
/// object on `transport`, then adopt it locally. Returns how many peers
/// were notified — they rotate on their next `adopt_rotations` pass.
pub async fn push_rotation<T: SyncTransport + ?Sized>(
    transport: &T,
    store: &pai_storage::Store,
    data_dir: &Path,
    device: &Device,
    agree: &crypto::AgreementKeypair,
    ids: &IdentityStore,
    key_dir: &Path,
) -> Result<usize> {
    use base64::Engine;
    let mut vault = [0u8; 32];
    OsRng.fill_bytes(&mut vault);
    let epoch = now().timestamp_millis().max(1) as u64;
    let from = device.id.to_string();

    let mut m = RotationMsg {
        v: 1,
        epoch,
        vault_b64: base64::engine::general_purpose::STANDARD.encode(vault),
        from: from.clone(),
        sig: String::new(),
    };
    m.sig = hex::encode(ids.sign(device.id, key_dir, &rot_transcript(epoch, &vault, &from))?);
    let raw = serde_json::to_vec(&m).map_err(|e| Error::Sync(e.to_string()))?;

    let peers = pair::list_peers(store)?;
    for p in &peers {
        let wrap = crypto::peer_key(&agree.secret, &XPublic::from(p.agree_pubkey));
        let key = format!("{ROT_PREFIX}{}/{from}", p.device_id);
        let obj = SyncObject {
            ciphertext: crypto::seal(&wrap, &[ROT_AAD, key.as_bytes()].concat(), &raw)?,
            key: key.clone(),
            version: epoch,
            writer: device.id,
            updated_at: now(),
            tombstone: false,
        };
        transport.push(&obj).await?;
    }
    rotate_vault_key(data_dir, &vault, epoch)?;
    Ok(peers.len())
}

// -- adopt (recipient) ------------------------------------------------------

/// Pull any `vrot/<me>/*` objects, verify + open each, and adopt the
/// newest epoch seen. On adoption the rotation is **re-broadcast**: a
/// new message signed by *this* device carries the same epoch+vault to
/// this device's own peers, so a rotation started by one member reaches
/// the whole vault even when the mesh isn't fully connected. Re-pushed
/// copies die on the epoch guard, so the gossip terminates.
///
/// Returns how many rotations were adopted (0 or 1 in practice —
/// duplicates and stale epochs are skipped).
pub async fn adopt_rotations<T: SyncTransport + ?Sized>(
    transport: &T,
    store: &pai_storage::Store,
    data_dir: &Path,
    agree: &crypto::AgreementKeypair,
    me: &Device,
    ids: &IdentityStore,
    key_dir: &Path,
) -> Result<usize> {
    use base64::Engine;
    let want = format!("{ROT_PREFIX}{}/", me.id);
    let peers = pair::list_peers(store)?;
    let mut adopted = 0usize;
    for meta in transport.list().await? {
        if !meta.key.starts_with(&want) || meta.tombstone {
            continue;
        }
        let from = &meta.key[want.len()..];
        let Some(peer) = peers.iter().find(|p| p.device_id.to_string() == from) else {
            tracing::warn!(key = %meta.key, "rotation from unknown device — ignored");
            continue;
        };
        let Some(obj) = transport.pull(&meta.key).await? else {
            continue;
        };
        let wrap = crypto::peer_key(&agree.secret, &XPublic::from(peer.agree_pubkey));
        let raw = match crypto::open(
            &wrap,
            &[ROT_AAD, obj.key.as_bytes()].concat(),
            &obj.ciphertext,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(key = %obj.key, error = %e, "unopenable rotation — ignored");
                continue;
            }
        };
        let m: RotationMsg = match serde_json::from_slice(&raw) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(key = %obj.key, error = %e, "bad rotation payload — ignored");
                continue;
            }
        };
        if m.v != 1 || m.from != from {
            continue;
        }
        // Verify the rotator's signature over the rotation transcript.
        let vault: [u8; 32] = match base64::engine::general_purpose::STANDARD
            .decode(&m.vault_b64)
            .ok()
            .and_then(|b| b.as_slice().try_into().ok())
        {
            Some(v) => v,
            None => continue,
        };
        let sig_raw = match hex::decode(&m.sig).ok() {
            Some(s) => s,
            None => continue,
        };
        let sig = match Signature::from_slice(&sig_raw) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let key = match VerifyingKey::from_bytes(&peer.ed_pubkey) {
            Ok(k) => k,
            Err(_) => continue,
        };
        if key
            .verify(&rot_transcript(m.epoch, &vault, &m.from), &sig)
            .is_err()
        {
            tracing::warn!(key = %obj.key, "rotation signature invalid — ignored");
            continue;
        }
        if m.epoch <= vault_epoch(data_dir) {
            continue; // stale/replayed rotation
        }
        rotate_vault_key(data_dir, &vault, m.epoch)?;
        // Gossip: republish to our own peers so indirect members rotate
        // too. They verify *our* signature — they hold our peer row.
        let n = rebroadcast(transport, &peers, &m, me, agree, ids, key_dir).await?;
        tracing::debug!("rotation gossiped to {n} peer(s)");
        adopted += 1;
    }
    Ok(adopted)
}

/// Push the adopted (epoch, vault) to every peer as a rotation signed
/// by `me`. Peers that already adopted skip it via the epoch guard.
async fn rebroadcast<T: SyncTransport + ?Sized>(
    transport: &T,
    peers: &[pair::SyncPeer],
    adopted_msg: &RotationMsg,
    me: &Device,
    agree: &crypto::AgreementKeypair,
    ids: &IdentityStore,
    key_dir: &Path,
) -> Result<usize> {
    use base64::Engine;
    let vault: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(&adopted_msg.vault_b64)
        .ok()
        .and_then(|b| b.as_slice().try_into().ok())
        .ok_or_else(|| Error::Sync("rebroadcast: bad vault in adopted msg".into()))?;
    let from = me.id.to_string();
    let mut m = RotationMsg {
        v: 1,
        epoch: adopted_msg.epoch,
        vault_b64: adopted_msg.vault_b64.clone(),
        from: from.clone(),
        sig: String::new(),
    };
    m.sig = hex::encode(ids.sign(me.id, key_dir, &rot_transcript(m.epoch, &vault, &from))?);
    let raw = serde_json::to_vec(&m).map_err(|e| Error::Sync(e.to_string()))?;
    let mut n = 0usize;
    for p in peers {
        let wrap = crypto::peer_key(&agree.secret, &XPublic::from(p.agree_pubkey));
        let key = format!("{ROT_PREFIX}{}/{from}", p.device_id);
        let obj = SyncObject {
            ciphertext: crypto::seal(&wrap, &[ROT_AAD, key.as_bytes()].concat(), &raw)?,
            key: key.clone(),
            version: m.epoch,
            writer: me.id,
            updated_at: now(),
            tombstone: false,
        };
        transport.push(&obj).await?;
        n += 1;
    }
    Ok(n)
}

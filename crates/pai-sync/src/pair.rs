//! Device pairing — signed offer/accept exchange over files.
//!
//! ```text
//! A: pai pair offer  --out offer.pai        (device record + agree pubkey,
//!                                            ed25519-signed)
//! B: pai pair accept offer.pai --out accept.pai
//!      verifies A's signature, records A as a trusted peer, seals B's
//!      vault key to A's agreement key, signs the response
//! A: pai pair complete accept.pai
//!      verifies B's signature, records B, adopts the vault key
//! ```
//!
//! Two file hops — works over the same shared folder as sync itself, a
//! USB stick, or a QR/channel adapter later. The acceptor's vault key is
//! authoritative: both sides end up in the same vault.
//!
//! Trust model: a `sync_peers` row is an explicit trust decision. Anyone
//! who can trick the user into accepting an offer file gains vault
//! access — the files are meant to move over a channel the user controls.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::{parse_ts, store_err, ts, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::Path;
use x25519_dalek::PublicKey as XPublic;

const DOMAIN: &[u8] = b"pai-pair-v1";
const VAULT_AAD: &[u8] = b"pai-sync-v1 vault-handoff";

/// One side of the exchange. `kind` distinguishes offer (vault_sealed
/// empty) from accept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingMessage {
    pub kind: String,
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub ed_pubkey: String,
    pub agree_pubkey: String,
    /// Hex of `seal(peer_key, VAULT_AAD, vault_key)` — accepts only.
    #[serde(default)]
    pub vault_sealed: String,
    /// ed25519 signature over `transcript(kind, ...)`.
    pub signature: String,
}

fn transcript(m: &PairingMessage) -> Vec<u8> {
    let mut t = Vec::with_capacity(DOMAIN.len() + 128);
    t.extend_from_slice(DOMAIN);
    t.extend_from_slice(m.kind.as_bytes());
    t.extend_from_slice(m.device_id.as_bytes());
    t.extend_from_slice(m.ed_pubkey.as_bytes());
    t.extend_from_slice(m.agree_pubkey.as_bytes());
    t.extend_from_slice(m.vault_sealed.as_bytes());
    t
}

fn hex32(s: &str) -> Result<[u8; 32]> {
    let raw = hex::decode(s).map_err(|e| Error::InvalidInput(format!("bad hex: {e}")))?;
    raw.as_slice()
        .try_into()
        .map_err(|_| Error::InvalidInput("expected 32 bytes".into()))
}

impl PairingMessage {
    fn verify(&self) -> Result<()> {
        let ed = hex32(&self.ed_pubkey)?;
        let key = VerifyingKey::from_bytes(&ed)
            .map_err(|_| Error::InvalidInput("bad ed25519 pubkey".into()))?;
        let sig_raw = hex::decode(&self.signature)
            .map_err(|e| Error::InvalidInput(format!("bad signature hex: {e}")))?;
        let sig = Signature::from_slice(&sig_raw)
            .map_err(|_| Error::InvalidInput("bad signature".into()))?;
        if key.verify(&transcript(self), &sig).is_ok() {
            Ok(())
        } else {
            Err(Error::PermissionDenied(
                "pairing message signature invalid".into(),
            ))
        }
    }

    fn peer(&self) -> Result<SyncPeer> {
        Ok(SyncPeer {
            device_id: DeviceId(
                uuid::Uuid::parse_str(&self.device_id)
                    .map_err(|_| Error::InvalidInput("bad device id".into()))?,
            ),
            name: self.name.clone(),
            platform: self.platform.clone(),
            ed_pubkey: hex32(&self.ed_pubkey)?,
            agree_pubkey: hex32(&self.agree_pubkey)?,
            paired_at: now(),
        })
    }
}

/// A device recorded as trusted via pairing.
#[derive(Debug, Clone)]
pub struct SyncPeer {
    pub device_id: DeviceId,
    pub name: String,
    pub platform: String,
    pub ed_pubkey: [u8; 32],
    pub agree_pubkey: [u8; 32],
    pub paired_at: Timestamp,
}

// -- offer -----------------------------------------------------------------

pub fn make_offer(
    device: &Device,
    agree: &crate::crypto::AgreementKeypair,
    ids: &IdentityStore,
    key_dir: &Path,
) -> Result<PairingMessage> {
    let mut m = PairingMessage {
        kind: "offer".into(),
        device_id: device.id.to_string(),
        name: device.name.clone(),
        platform: format!("{:?}", device.platform).to_lowercase(),
        ed_pubkey: hex::encode(&device.public_key),
        agree_pubkey: hex::encode(agree.public.as_bytes()),
        vault_sealed: String::new(),
        signature: String::new(),
    };
    m.signature = hex::encode(ids.sign(device.id, key_dir, &transcript(&m))?);
    Ok(m)
}

// -- accept ----------------------------------------------------------------

/// Verify `offer`, record the offerer as a peer, and produce the signed
/// accept message carrying the vault key sealed to the offerer. The
/// acceptor's vault is created on first use and is authoritative.
pub fn accept_offer(
    store: &Store,
    offer: &PairingMessage,
    device: &Device,
    agree: &crate::crypto::AgreementKeypair,
    ids: &IdentityStore,
    key_dir: &Path,
    data_dir: &Path,
) -> Result<PairingMessage> {
    if offer.kind != "offer" {
        return Err(Error::InvalidInput("not a pairing offer".into()));
    }
    offer.verify()?;
    if offer.device_id == device.id.to_string() {
        return Err(Error::InvalidInput("cannot pair with self".into()));
    }
    add_peer(store, &offer.peer()?)?;

    let vault = crate::crypto::vault_key_or_generate(data_dir)?;
    let offerer_agree = XPublic::from(hex32(&offer.agree_pubkey)?);
    let wrap = crate::crypto::peer_key(&agree.secret, &offerer_agree);
    let sealed = crate::crypto::seal(&wrap, VAULT_AAD, &vault)?;

    let mut m = PairingMessage {
        kind: "accept".into(),
        device_id: device.id.to_string(),
        name: device.name.clone(),
        platform: format!("{:?}", device.platform).to_lowercase(),
        ed_pubkey: hex::encode(&device.public_key),
        agree_pubkey: hex::encode(agree.public.as_bytes()),
        vault_sealed: hex::encode(sealed),
        signature: String::new(),
    };
    m.signature = hex::encode(ids.sign(device.id, key_dir, &transcript(&m))?);
    Ok(m)
}

// -- complete --------------------------------------------------------------

/// Verify `accept`, record the acceptor as a peer, unwrap and adopt the
/// vault key. Returns the vault key.
pub fn complete_pairing(
    store: &Store,
    accept: &PairingMessage,
    agree: &crate::crypto::AgreementKeypair,
    data_dir: &Path,
) -> Result<[u8; 32]> {
    if accept.kind != "accept" {
        return Err(Error::InvalidInput("not a pairing accept".into()));
    }
    accept.verify()?;
    add_peer(store, &accept.peer()?)?;

    let acceptor_agree = XPublic::from(hex32(&accept.agree_pubkey)?);
    let wrap = crate::crypto::peer_key(&agree.secret, &acceptor_agree);
    let blob = hex::decode(&accept.vault_sealed)
        .map_err(|e| Error::InvalidInput(format!("bad vault_sealed hex: {e}")))?;
    let vault: [u8; 32] = crate::crypto::open(&wrap, VAULT_AAD, &blob)?
        .as_slice()
        .try_into()
        .map_err(|_| Error::Sync("vault key wrong length".into()))?;
    crate::crypto::adopt_vault_key(data_dir, &vault)?;
    Ok(vault)
}

// -- peer table ------------------------------------------------------------

pub fn add_peer(store: &Store, p: &SyncPeer) -> Result<()> {
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO sync_peers(device_id, name, platform, ed_pubkey,
                agree_pubkey, paired_at) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(device_id) DO UPDATE SET
                name=excluded.name, platform=excluded.platform,
                ed_pubkey=excluded.ed_pubkey,
                agree_pubkey=excluded.agree_pubkey",
            params![
                p.device_id.to_string(),
                p.name,
                p.platform,
                p.ed_pubkey.as_slice(),
                p.agree_pubkey.as_slice(),
                ts(&p.paired_at),
            ],
        )
    })?;
    Ok(())
}

pub fn list_peers(store: &Store) -> Result<Vec<SyncPeer>> {
    store.with_conn(|c| {
        let mut stmt = c.prepare(
            "SELECT device_id, name, platform, ed_pubkey, agree_pubkey,
                    paired_at FROM sync_peers ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let ed: Vec<u8> = r.get(3)?;
            let ag: Vec<u8> = r.get(4)?;
            Ok(SyncPeer {
                device_id: DeviceId(
                    uuid::Uuid::parse_str(&id).unwrap_or_else(|_| uuid::Uuid::nil()),
                ),
                name: r.get(1)?,
                platform: r.get(2)?,
                ed_pubkey: ed.as_slice().try_into().unwrap_or([0; 32]),
                agree_pubkey: ag.as_slice().try_into().unwrap_or([0; 32]),
                paired_at: parse_ts(&r.get::<_, String>(5)?),
            })
        })?;
        rows.collect()
    })
}

pub fn remove_peer(store: &Store, id: DeviceId) -> Result<bool> {
    let n = store.with_conn(|c| {
        c.execute(
            "DELETE FROM sync_peers WHERE device_id=?1",
            params![id.to_string()],
        )
    })?;
    Ok(n > 0)
}

// -- file io ---------------------------------------------------------------

pub fn write_message(m: &PairingMessage, path: &Path) -> Result<()> {
    let raw = serde_json::to_vec_pretty(m).map_err(|e| Error::Sync(e.to_string()))?;
    std::fs::write(path, raw).map_err(store_err)
}

pub fn read_message(path: &Path) -> Result<PairingMessage> {
    let raw = std::fs::read(path).map_err(store_err)?;
    serde_json::from_slice(&raw).map_err(|e| Error::InvalidInput(format!("bad pairing file: {e}")))
}

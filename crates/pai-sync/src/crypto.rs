//! Sealing primitives for sync objects and the pairing handoff.
//!
//! - Each device owns a **X25519 agreement keypair**, keystore-backed like
//!   the ed25519 signing key (`agreement:<device>`; file fallback).
//! - All paired devices share one 256-bit **vault key**. Objects are
//!   XChaCha20-Poly1305 sealed under it with the object key as AAD, so a
//!   transport-level attacker cannot swap or replay objects under
//!   different paths.
//! - The vault key moves between devices wrapped by an ECDH-derived peer
//!   key (HKDF-SHA256 over the X25519 shared secret).
//!
//! Security boundary: possession of the vault key is read *and* write
//! access to every synced object. Unpairing does not revoke (the peer may
//! have copied the key); rotation is roadmap work. See ADR 0015.

use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use pai_core::*;
use pai_identity::keystore;
use pai_storage::store_err;
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use std::path::Path;
use x25519_dalek::{PublicKey as XPublic, StaticSecret};

const SEAL_VER: u8 = 1;
const HKDF_INFO: &[u8] = b"pai-sync-v1";

/// X25519 agreement keypair (secret stays in keystore / 0600 file).
pub struct AgreementKeypair {
    pub secret: StaticSecret,
    pub public: XPublic,
}

fn load_or_make_key(data_dir: &Path, keystore_name: &str, file_name: &str) -> Result<[u8; 32]> {
    let file = data_dir.join(file_name);
    if let Some(b) = keystore::load(keystore_name) {
        return b
            .as_slice()
            .try_into()
            .map_err(|_| Error::Sync(format!("corrupt {keystore_name} key")));
    }
    if let Ok(b) = std::fs::read(&file) {
        let raw: [u8; 32] = b
            .as_slice()
            .try_into()
            .map_err(|_| Error::Sync(format!("corrupt {file_name}")))?;
        if keystore::store(keystore_name, &raw) {
            let _ = std::fs::remove_file(&file);
        }
        return Ok(raw);
    }
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    if !keystore::store(keystore_name, &k) {
        std::fs::create_dir_all(data_dir).map_err(store_err)?;
        std::fs::write(&file, k).map_err(store_err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
        tracing::warn!("OS keystore unavailable; {keystore_name} written to {file:?}");
    }
    Ok(k)
}

/// This device's agreement key, created on first use.
pub fn agreement_key(device: DeviceId, data_dir: &Path) -> Result<AgreementKeypair> {
    let raw = load_or_make_key(
        data_dir,
        &format!("agreement:{device}"),
        &format!("agreement-{device}.key"),
    )?;
    let secret = StaticSecret::from(raw);
    let public = XPublic::from(&secret);
    Ok(AgreementKeypair { secret, public })
}

/// Keystore name for this data_dir's vault. Namespaced so two pai
/// installations on one OS account hold distinct vaults (and tests get
/// per-dir isolation); the agreement keys are namespaced by device id
/// the same way.
fn vault_ks_name(data_dir: &Path) -> String {
    let canon = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    let h = Sha256::digest(canon.to_string_lossy().as_bytes());
    format!("sync-vault:{}", hex::encode(&h[..8]))
}

/// The shared vault key. `None` when this device has never paired — it is
/// created by the first `pair accept`/`pair complete` that needs it.
pub fn vault_key(data_dir: &Path) -> Result<Option<[u8; 32]>> {
    let name = vault_ks_name(data_dir);
    let file = data_dir.join("sync-vault.key");
    if let Some(b) = keystore::load(&name) {
        return b
            .as_slice()
            .try_into()
            .map(Some)
            .map_err(|_| Error::Sync("corrupt sync-vault key".into()));
    }
    if let Ok(b) = std::fs::read(&file) {
        let raw: [u8; 32] = b
            .as_slice()
            .try_into()
            .map_err(|_| Error::Sync("corrupt sync-vault.key".into()))?;
        if keystore::store(&name, &raw) {
            let _ = std::fs::remove_file(&file);
        }
        return Ok(Some(raw));
    }
    Ok(None)
}

/// Persist `key` as the vault key. Fails if a *different* vault key
/// already exists — merging two vaults is not supported (see ADR 0015);
/// delete the old key to reset.
pub fn adopt_vault_key(data_dir: &Path, key: &[u8; 32]) -> Result<()> {
    if let Some(existing) = vault_key(data_dir)? {
        if existing == *key {
            return Ok(());
        }
        return Err(Error::Sync(
            "vault key conflict: this device already belongs to a different \
             sync vault. Remove the old peers/vault key before pairing with \
             a different vault."
                .into(),
        ));
    }
    store_vault(data_dir, key)
}

/// Load the vault key, generating a fresh one when absent.
pub fn vault_key_or_generate(data_dir: &Path) -> Result<[u8; 32]> {
    if let Some(k) = vault_key(data_dir)? {
        return Ok(k);
    }
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    store_vault(data_dir, &k)?;
    Ok(k)
}

fn store_vault(data_dir: &Path, key: &[u8; 32]) -> Result<()> {
    if keystore::store(&vault_ks_name(data_dir), key) {
        return Ok(());
    }
    let file = data_dir.join("sync-vault.key");
    std::fs::create_dir_all(data_dir).map_err(store_err)?;
    std::fs::write(&file, key).map_err(store_err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
    tracing::warn!("OS keystore unavailable; sync vault key written to {file:?}");
    Ok(())
}

/// ECDH-derived key used to wrap the vault key during pairing.
pub fn peer_key(secret: &StaticSecret, peer: &XPublic) -> [u8; 32] {
    let shared = secret.diffie_hellman(peer);
    let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());
    let mut k = [0u8; 32];
    hk.expand(HKDF_INFO, &mut k).expect("hkdf expand 32");
    k
}

/// `v1 || nonce(24) || XChaCha20-Poly1305(plaintext)`, `aad` binds the
/// ciphertext to its context (object key, handoff domain, ...).
pub fn seal(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce = XNonce::default();
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Sync("seal failed".into()))?;
    let mut out = Vec::with_capacity(1 + nonce.len() + ct.len());
    out.push(SEAL_VER);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 1 + 24 + 16 || blob[0] != SEAL_VER {
        return Err(Error::Sync("not a v1 sealed blob".into()));
    }
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = XNonce::from_slice(&blob[1..25]);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &blob[25..],
                aad,
            },
        )
        .map_err(|_| Error::Sync("sealed blob failed authentication".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_aad_binding() {
        let key = [7u8; 32];
        let blob = seal(&key, b"memory/x", b"hello").unwrap();
        assert_eq!(open(&key, b"memory/x", &blob).unwrap(), b"hello");
        assert!(open(&key, b"memory/y", &blob).is_err());
        assert!(open(&[8u8; 32], b"memory/x", &blob).is_err());
    }

    #[test]
    fn peer_key_is_symmetric() {
        let sa = StaticSecret::random_from_rng(OsRng);
        let sb = StaticSecret::random_from_rng(OsRng);
        assert_eq!(
            peer_key(&sa, &XPublic::from(&sb)),
            peer_key(&sb, &XPublic::from(&sa))
        );
    }
}

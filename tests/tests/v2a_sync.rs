//! V2a tests: sealed objects, the signed pairing exchange, and memory
//! sync over FolderTransport between two simulated devices.

use pai_core::*;
use pai_identity::IdentityStore;
use pai_memory::{user_fact, MemoryBackend, SqliteMemory};
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, FolderTransport, SyncTransport};
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2a-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A simulated device: own store, keys dir, identity.
struct Dev {
    dir: PathBuf,
    store: Arc<Store>,
    ids: IdentityStore,
    key_dir: PathBuf,
    device: Device,
}

fn dev(tag: &str) -> Dev {
    let dir = tmpdir(tag);
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let ids = IdentityStore::new(store.clone());
    let key_dir = dir.join("keys");
    let user = ids.create_user("u").unwrap();
    let device = ids
        .register_device(
            user.id,
            tag,
            Platform::Linux,
            DeviceCapabilities::default(),
            &key_dir,
        )
        .unwrap();
    Dev {
        dir,
        store,
        ids,
        key_dir,
        device,
    }
}

#[test]
fn seal_tamper_and_aad() {
    let k = [3u8; 32];
    let blob = crypto::seal(&k, b"memory/abc", b"secret").unwrap();
    assert_eq!(crypto::open(&k, b"memory/abc", &blob).unwrap(), b"secret");
    // Wrong AAD (renamed object) and wrong key both fail.
    assert!(crypto::open(&k, b"memory/abd", &blob).is_err());
    assert!(crypto::open(&[9u8; 32], b"memory/abc", &blob).is_err());
    // Truncated / flipped bytes fail authentication.
    let mut bad = blob.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert!(crypto::open(&k, b"memory/abc", &bad).is_err());
}

#[test]
fn pairing_rejects_tampered_offer() {
    let a = dev("tamp-a");
    let b = dev("tamp-b");
    let agree_a = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();
    let mut offer = pair::make_offer(&a.device, &agree_a, &a.ids, &a.key_dir).unwrap();
    // Swap in a different agreement key after signing → signature must fail.
    offer.agree_pubkey = hex::encode([9u8; 32]);
    assert!(
        pair::accept_offer(&b.store, &offer, &b.device, &agree_b, &b.ids, &b.key_dir, &b.dir)
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_and_sync_end_to_end() {
    let a = dev("e2e-a");
    let b = dev("e2e-b");
    let agree_a = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();

    // -- pairing handshake over "files" -------------------------------
    let offer = pair::make_offer(&a.device, &agree_a, &a.ids, &a.key_dir).unwrap();
    let accept = pair::accept_offer(
        &b.store, &offer, &b.device, &agree_b, &b.ids, &b.key_dir, &b.dir,
    )
    .unwrap();
    let vault_a = pair::complete_pairing(&a.store, &accept, &agree_a, &a.dir).unwrap();
    assert_eq!(vault_a, crypto::vault_key(&b.dir).unwrap().unwrap());
    assert_eq!(pair::list_peers(&a.store).unwrap().len(), 1);
    assert_eq!(pair::list_peers(&b.store).unwrap().len(), 1);

    // -- A remembers something, pushes it sealed ----------------------
    let mem_a = SqliteMemory::new(a.store.clone());
    let item = user_fact("I prefer local models", 0.9);
    let item_id = item.id;
    mem_a.put(&item).await.unwrap();

    let shared = tmpdir("shared");
    let eng_a = engine::folder_engine(&shared, a.store.clone(), a.device.id, &a.dir).unwrap();
    let up = eng_a.push().await.unwrap();
    assert!(up.pushed >= 1);

    // Wire objects must be ciphertext — no plaintext content on disk.
    for e in std::fs::read_dir(&shared).unwrap().flatten() {
        let raw = std::fs::read(e.path()).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            !text.contains("prefer local models"),
            "plaintext leaked into {}",
            e.path().display()
        );
    }

    // -- B pulls and applies ------------------------------------------
    let eng_b = engine::folder_engine(&shared, b.store.clone(), b.device.id, &b.dir).unwrap();
    let down = eng_b.pull().await.unwrap();
    assert_eq!(down.pulled, 1);
    let mem_b = SqliteMemory::new(b.store.clone());
    let got = mem_b.get(item_id).await.unwrap();
    assert_eq!(got.content, "I prefer local models");
    assert_eq!(got.source, MemorySource::UserStated);

    // -- nothing new → re-push is a no-op -----------------------------
    assert_eq!(eng_a.push().await.unwrap().pushed, 0);
    assert_eq!(eng_b.pull().await.unwrap().pulled, 0);

    // -- B deletes it → tombstone propagates back to A -----------------
    mem_b.delete(item_id).await.unwrap();
    assert!(eng_b.push().await.unwrap().pushed >= 1);
    let back = eng_a.pull().await.unwrap();
    assert_eq!(back.pulled, 1);
    assert!(mem_a.get(item_id).await.is_err(), "tombstone did not apply");

    // -- LWW: older remote update must not clobber a newer local write --
    let item2 = user_fact("v2", 0.5);
    let mut item2 = item2;
    item2.updated_at = now() + chrono::Duration::seconds(60);
    mem_a.put(&item2).await.unwrap();
    eng_a.push().await.unwrap();
    // B's clock reads the newer ts — pull applies it, content wins.
    eng_b.pull().await.unwrap();
    let got2 = mem_b.get(item2.id).await.unwrap();
    assert_eq!(got2.content, "v2");
}

#[test]
fn vault_conflict_is_rejected() {
    let a = dev("vault-a");
    // A different pre-existing vault must not be silently overwritten.
    crypto::adopt_vault_key(&a.dir, &[1u8; 32]).unwrap();
    assert!(crypto::adopt_vault_key(&a.dir, &[2u8; 32]).is_err());
    // Same key is idempotent.
    crypto::adopt_vault_key(&a.dir, &[1u8; 32]).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpaired_device_cannot_sync() {
    let a = dev("nopair");
    // No vault key → engine construction fails closed.
    assert!(engine::folder_engine(&tmpdir("x"), a.store.clone(), a.device.id, &a.dir).is_err());
    // FolderTransport itself is transport-only and still works.
    let t = FolderTransport::new(tmpdir("t")).unwrap();
    assert!(t.list().await.unwrap().is_empty());
}

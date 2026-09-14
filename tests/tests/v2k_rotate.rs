//! V2k tests: vault rotation + revocation — sealed rotation objects at
//! `vrot/<to>/<from>` keys, epoch-gated adoption, peer-removal cutting
//! a device out of the next rotation, and forgery rejection.

use pai_core::*;
use pai_identity::IdentityStore;
use pai_memory::{user_fact, MemoryBackend, SqliteMemory};
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, rotate, FolderTransport, SyncTransport};
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2k-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

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

/// offerer ← acceptor: the acceptor's vault flows to the offerer.
fn pair_devices(offerer: &Dev, acceptor: &Dev) {
    let agree_a = crypto::agreement_key(offerer.device.id, &offerer.dir).unwrap();
    let agree_b = crypto::agreement_key(acceptor.device.id, &acceptor.dir).unwrap();
    let offer =
        pair::make_offer(&offerer.device, &agree_a, &offerer.ids, &offerer.key_dir).unwrap();
    let accept = pair::accept_offer(
        &acceptor.store,
        &offer,
        &acceptor.device,
        &agree_b,
        &acceptor.ids,
        &acceptor.key_dir,
        &acceptor.dir,
    )
    .unwrap();
    pair::complete_pairing(&offerer.store, &accept, &agree_a, &offerer.dir).unwrap();
}

fn vault(d: &Dev) -> [u8; 32] {
    crypto::vault_key(&d.dir).unwrap().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_distributes_new_vault() {
    let a = dev("rot-a");
    let b = dev("rot-b");
    let c = dev("rot-c");
    // Vault model: new devices join by offering to a member.
    pair_devices(&a, &b);
    pair_devices(&c, &b);
    let shared = tmpdir("shared");
    let transport = FolderTransport::new(shared).unwrap();
    let old = vault(&a);

    // A rotates — notifies only its direct peer B (A does not know
    // C), then adopts locally.
    let agree_a = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let n = rotate::push_rotation(
        &transport, &a.store, &a.dir, &a.device, &agree_a, &a.ids, &a.key_dir,
    )
    .await
    .unwrap();
    assert_eq!(n, 1, "A should only notify its direct peers");
    assert_ne!(vault(&a), old, "A's vault didn't change");

    // B adopts — and gossips the rotation on to its own peers (C).
    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();
    assert_eq!(
        rotate::adopt_rotations(
            &transport, &b.store, &b.dir, &agree_b, &b.device, &b.ids, &b.key_dir
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(vault(&b), vault(&a), "B didn't get the new vault");
    let agree_c = crypto::agreement_key(c.device.id, &c.dir).unwrap();
    assert_eq!(
        rotate::adopt_rotations(
            &transport, &c.store, &c.dir, &agree_c, &c.device, &c.ids, &c.key_dir
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(vault(&c), vault(&a), "gossip did not reach C");

    // Replay guard: the same objects are stale on a second pass.
    assert_eq!(
        rotate::adopt_rotations(
            &transport, &b.store, &b.dir, &agree_b, &b.device, &b.ids, &b.key_dir
        )
        .await
        .unwrap(),
        0,
        "stale rotation adopted twice"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_peer_is_cut_out_of_rotation() {
    let a = dev("rev-a");
    let b = dev("rev-b");
    pair_devices(&a, &b);
    let shared = tmpdir("shared2");
    let transport = FolderTransport::new(shared.clone()).unwrap();
    let old = vault(&b);

    // A removes B, then rotates — nobody is notified.
    assert!(pair::remove_peer(&a.store, b.device.id).unwrap());
    let agree_a = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let n = rotate::push_rotation(
        &transport, &a.store, &a.dir, &a.device, &agree_a, &a.ids, &a.key_dir,
    )
    .await
    .unwrap();
    assert_eq!(n, 0, "removed peer still got a rotation");

    // B's adopt pass finds nothing addressed to it → keeps the old vault.
    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();
    assert_eq!(
        rotate::adopt_rotations(
            &transport, &b.store, &b.dir, &agree_b, &b.device, &b.ids, &b.key_dir
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(vault(&b), old, "B unexpectedly rotated");

    // Revocation is real: objects A seals under the new vault are
    // unopenable at B — the engine skips them, applying nothing.
    let mem = SqliteMemory::new(a.store.clone());
    mem.put(&user_fact("post-rotation secret", 1.0))
        .await
        .unwrap();
    let eng_a = engine::folder_engine(&shared, a.store.clone(), a.device.id, &a.dir).unwrap();
    eng_a.push().await.unwrap();
    let eng_b = engine::folder_engine(&shared, b.store.clone(), b.device.id, &b.dir).unwrap();
    let out = eng_b.pull().await.unwrap();
    assert_eq!(out.pulled, 0, "removed device read post-rotation data");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_from_non_peer_is_ignored() {
    let a = dev("frg-a");
    let b = dev("frg-b");
    let d = dev("frg-d"); // never paired
    pair_devices(&a, &b);
    let shared = tmpdir("shared3");
    let transport = FolderTransport::new(shared).unwrap();
    let old = vault(&b);

    // D forges a rotation addressed to B "from" D — B's peer lookup
    // fails (D was never paired) so it's ignored before any crypto.
    let obj = SyncObject {
        key: format!("vrot/{}/{}", b.device.id, d.device.id),
        ciphertext: crypto::seal(&[7u8; 32], b"aad", b"forged").unwrap(),
        version: 1,
        writer: d.device.id,
        updated_at: now(),
        tombstone: false,
    };
    transport.push(&obj).await.unwrap();

    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();
    assert_eq!(
        rotate::adopt_rotations(
            &transport, &b.store, &b.dir, &agree_b, &b.device, &b.ids, &b.key_dir
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(vault(&b), old, "forged rotation changed B's vault");
}

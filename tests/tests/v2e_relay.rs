//! V2 relay tests: HTTP relay transport roundtrip, token auth, and a
//! full two-device sync over the relay (ciphertext-only on the wire).

use pai_core::*;
use pai_identity::IdentityStore;
use pai_memory::{user_fact, MemoryBackend, SqliteMemory};
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, relay, urldec, urlenc, SyncTransport};
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2e-{tag}-{}", uuid::Uuid::new_v4()));
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

/// Start a relay on 127.0.0.1:0 with a temp store; returns its base URL.
fn start_relay(tag: &str, token: Option<&str>) -> String {
    let srv = relay::bind(tmpdir(tag), "127.0.0.1:0", token.map(|t| t.to_string())).unwrap();
    let url = format!("http://{}", srv.addr());
    std::thread::spawn(move || relay::serve(srv));
    url
}

fn obj(key: &str) -> SyncObject {
    SyncObject {
        key: key.into(),
        ciphertext: b"sealed-blob".to_vec(),
        version: 1,
        writer: DeviceId::new(),
        updated_at: now(),
        tombstone: false,
    }
}

#[test]
fn url_codec_roundtrips() {
    for k in ["memory/abc", "prefs/key:1", "a b/c?d"] {
        assert_eq!(urldec(&urlenc(k)), k);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_transport_roundtrip() {
    let url = start_relay("rt", None);
    let t = relay::RelayTransport::new(&url, None);
    assert_eq!(t.id(), "relay");
    assert!(t.list().await.unwrap().is_empty());

    t.push(&obj("memory/m1")).await.unwrap();
    t.push(&obj("memory/m2")).await.unwrap();
    let metas = t.list().await.unwrap();
    assert_eq!(metas.len(), 2);

    let got = t.pull("memory/m1").await.unwrap().unwrap();
    assert_eq!(got.key, "memory/m1");
    assert_eq!(got.ciphertext, b"sealed-blob");

    // Unknown key → None (404 maps cleanly).
    assert!(t.pull("memory/nope").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_requires_token() {
    let url = start_relay("auth", Some("s3cret"));

    // No token → everything 401s.
    let anon = relay::RelayTransport::new(&url, None);
    assert!(anon.list().await.is_err());
    assert!(anon.push(&obj("k")).await.is_err());
    assert!(anon.pull("k").await.is_err());

    // Token → full access.
    let auth = relay::RelayTransport::new(&url, Some("s3cret".into()));
    auth.push(&obj("memory/x")).await.unwrap();
    assert_eq!(auth.list().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_engine_over_relay() {
    let a = dev("rel-a");
    let b = dev("rel-b");
    let agree_a = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();
    let offer = pair::make_offer(&a.device, &agree_a, &a.ids, &a.key_dir).unwrap();
    let accept = pair::accept_offer(
        &b.store, &offer, &b.device, &agree_b, &b.ids, &b.key_dir, &b.dir,
    )
    .unwrap();
    pair::complete_pairing(&a.store, &accept, &agree_a, &a.dir).unwrap();

    let url = start_relay("e2e", Some("tok"));
    let mk = |d: &Dev| -> engine::SyncEngine<Box<dyn SyncTransport>> {
        let vault = crypto::vault_key(&d.dir).unwrap().unwrap();
        engine::SyncEngine::new(
            Box::new(relay::RelayTransport::new(&url, Some("tok".into()))),
            d.store.clone(),
            vault,
            d.device.id,
            &d.dir,
        )
    };
    let eng_a = mk(&a);
    let eng_b = mk(&b);

    // A remembers + pushes through the relay.
    let mem_a = SqliteMemory::new(a.store.clone());
    let item = user_fact("relay-delivered memory", 0.9);
    let item_id = item.id;
    mem_a.put(&item).await.unwrap();
    assert!(eng_a.push().await.unwrap().pushed >= 1);

    // B pulls through the relay — full E2EE path over HTTP.
    assert_eq!(eng_b.pull().await.unwrap().pulled, 1);
    let got = SqliteMemory::new(b.store.clone())
        .get(item_id)
        .await
        .unwrap();
    assert_eq!(got.content, "relay-delivered memory");
}

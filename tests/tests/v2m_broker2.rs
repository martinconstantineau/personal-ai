//! V2m tests: broker scheduling + GC + streaming — capability
//! announcements (`bcap`), `--any` peer discovery, request expiry,
//! transport `delete` as real GC, and chunked streamed responses.

use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, FolderTransport, SyncTransport};
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2m-{tag}-{}", uuid::Uuid::new_v4()));
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

struct Echo;
#[async_trait::async_trait]
impl pai_broker::rpc::OpHandler for Echo {
    async fn handle(&self, _op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        Ok(payload.to_vec())
    }
}

/// Streams the payload back byte-by-byte — proves chunks work.
struct Splitter;
#[async_trait::async_trait]
impl pai_broker::rpc::OpHandler for Splitter {
    async fn handle(&self, _op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        Ok(payload.to_vec())
    }
    async fn handle_stream(
        &self,
        _op: &str,
        payload: &[u8],
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) -> Result<()> {
        for b in payload {
            tx.send(vec![*b]).await.ok();
        }
        Ok(())
    }
}

/// `--any` discovery: an announced capability resolves to the right
/// device and the call round-trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_announcement_routes_any_call() {
    let a = dev("cap-a");
    let b = dev("cap-b");
    pair_devices(&a, &b);
    let shared = tmpdir("cap-shared");
    let transport = FolderTransport::new(shared).unwrap();
    let (va, vb) = (vault(&a), vault(&b));

    // B announces it serves "echo" + "split".
    let handler = Echo;
    let srv = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler)
        .with_ops(vec!["echo".into(), "split".into()]);
    srv.announce().await.unwrap();

    // A finds B by capability — not by id.
    let client = pai_broker::rpc::BrokerClient::new(&transport, &va, a.device.id);
    let found = client.find_peer("echo").await.unwrap();
    assert_eq!(found, Some(b.device.id));
    assert_eq!(client.find_peer("nonexistent").await.unwrap(), None);

    // And the routed call actually works.
    let mut srv = srv;
    let (call, _) = tokio::join!(
        client.call(
            found.unwrap(),
            "echo",
            b"hi",
            std::time::Duration::from_secs(10)
        ),
        async {
            for _ in 0..40 {
                if srv.serve_once().await.unwrap() > 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    );
    assert_eq!(call.unwrap(), b"hi");
}

/// Expired requests are skipped and deleted — the worker never runs
/// work the caller already gave up on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_requests_are_collected_not_served() {
    let a = dev("exp-a");
    let b = dev("exp-b");
    pair_devices(&a, &b);
    let shared = tmpdir("exp-shared");
    let transport = FolderTransport::new(shared).unwrap();
    let (va, vb) = (vault(&a), vault(&b));

    // A sends a request with a 1ms timeout — it expires instantly.
    let client = pai_broker::rpc::BrokerClient::new(&transport, &va, a.device.id);
    let _ = client
        .call(
            b.device.id,
            "echo",
            b"stale",
            std::time::Duration::from_millis(1),
        )
        .await;

    // B's serve pass sees the expired breq: deletes it, serves nothing.
    let handler = Echo;
    let mut srv = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler);
    let served = srv.serve_once().await.unwrap();
    assert_eq!(served, 1, "expired request wasn't collected");
    // The object is gone from the transport.
    let keys: Vec<_> = transport
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.key)
        .collect();
    assert!(
        !keys.iter().any(|k| k.starts_with("breq/")),
        "expired breq still on transport: {keys:?}"
    );
}

/// Served requests and consumed responses are deleted — the transport
/// doesn't accumulate broker objects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_objects_are_garbage_collected() {
    let a = dev("gc-a");
    let b = dev("gc-b");
    pair_devices(&a, &b);
    let shared = tmpdir("gc-shared");
    let transport = FolderTransport::new(shared).unwrap();
    let (va, vb) = (vault(&a), vault(&b));

    let handler = Echo;
    let mut srv = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler);
    let client = pai_broker::rpc::BrokerClient::new(&transport, &va, a.device.id);

    let (call, _) = tokio::join!(
        client.call(
            b.device.id,
            "echo",
            b"gc",
            std::time::Duration::from_secs(10)
        ),
        async {
            for _ in 0..40 {
                if srv.serve_once().await.unwrap() > 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    );
    call.unwrap();

    // After the roundtrip: no breq, no bres left on the transport.
    let keys: Vec<_> = transport
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.key)
        .collect();
    assert!(
        !keys
            .iter()
            .any(|k| k.starts_with("breq/") || k.starts_with("bres/")),
        "broker objects not GC'd: {keys:?}"
    );
}

/// A streamed call delivers ordered chunks live, then the final marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_call_delivers_chunks_in_order() {
    let a = dev("st-a");
    let b = dev("st-b");
    pair_devices(&a, &b);
    let shared = tmpdir("st-shared");
    let transport = FolderTransport::new(shared).unwrap();
    let (va, vb) = (vault(&a), vault(&b));

    let handler = Splitter;
    let mut srv = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler);
    let client = pai_broker::rpc::BrokerClient::new(&transport, &va, a.device.id);

    let collected = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let c2 = collected.clone();
    let mut on_chunk = |chunk: &[u8]| c2.lock().unwrap().extend_from_slice(chunk);
    let (call, _) = tokio::join!(
        client.call_stream(
            b.device.id,
            "split",
            b"hello",
            std::time::Duration::from_secs(10),
            &mut on_chunk,
        ),
        async {
            for _ in 0..60 {
                if srv.serve_once().await.unwrap() > 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    );
    call.unwrap();
    assert_eq!(*collected.lock().unwrap(), b"hello");

    // Stream objects are consumed/deleted too.
    let keys: Vec<_> = transport
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.key)
        .collect();
    assert!(
        !keys
            .iter()
            .any(|k| k.starts_with("breq/") || k.starts_with("bres/")),
        "stream objects not GC'd: {keys:?}"
    );
}

/// Relay-side delete works over HTTP too (the DELETE endpoint).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_transport_delete_roundtrip() {
    let dir = tmpdir("relay-del");
    let server = pai_sync::relay::bind(dir.clone(), "127.0.0.1:0", None).unwrap();
    let addr = server.addr();
    std::thread::spawn(move || pai_sync::relay::serve(server));
    let t = pai_sync::relay::RelayTransport::new(format!("http://{addr}"), None);

    let obj = SyncObject {
        key: "gc/test".into(),
        ciphertext: vec![1, 2, 3],
        version: 1,
        writer: DeviceId(uuid::Uuid::new_v4()),
        updated_at: now(),
        tombstone: false,
    };
    t.push(&obj).await.unwrap();
    assert!(t.pull("gc/test").await.unwrap().is_some());
    t.delete("gc/test").await.unwrap();
    assert!(t.pull("gc/test").await.unwrap().is_none());
    // Deleting a missing key is fine (idempotent GC).
    t.delete("gc/test").await.unwrap();
}

/// engine.apply ignores bcap/breq/bres objects — they never enter the
/// synced-record surface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_objects_stay_out_of_sync_surface() {
    let a = dev("sk-a");
    let b = dev("sk-b");
    pair_devices(&a, &b);
    let shared = tmpdir("sk-shared");
    let transport = FolderTransport::new(shared.clone()).unwrap();
    let vb = vault(&b);

    let handler = Echo;
    let srv = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler)
        .with_ops(vec!["echo".into()]);
    srv.announce().await.unwrap();

    // A syncs — the bcap object is skipped, not applied as a record.
    let eng = engine::folder_engine(&shared, a.store.clone(), a.device.id, &a.dir).unwrap();
    let out = eng.pull().await.unwrap();
    assert_eq!(out.pulled, 0);
    assert!(out.skipped >= 1);
}

/// V5a: `find_peer` scores announced `DeviceLoad` — a wall-powered
/// modest device beats a beefy one on battery; busy loses to idle at
/// equal hardware; identical scores keep the lowest-id tiebreak.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_peer_scores_device_load() {
    use pai_broker::rpc::DeviceLoad;
    let a = dev("score-a");
    let b = dev("score-b");
    let c = dev("score-c");
    // Acceptor's vault is authoritative — A accepts both offers so
    // all three devices share A's vault.
    pair_devices(&b, &a);
    pair_devices(&c, &a);
    let shared = tmpdir("score-shared");
    let transport = FolderTransport::new(shared).unwrap();
    let (va, vb, vc) = (vault(&a), vault(&b), vault(&c));
    assert_eq!(va, vb);
    assert_eq!(va, vc);

    let handler = Echo;
    // B: beefy but on battery. C: modest, wall-powered, idle.
    let srv_b = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler)
        .with_ops(vec!["echo".into()])
        .with_load_probe(Box::new(|| DeviceLoad {
            busy: 0,
            on_battery: Some(true),
            thermal_throttled: None,
            ram_bytes: 64 << 30,
            cpu_cores: 16,
        }));
    let srv_c = pai_broker::rpc::BrokerServer::new(&transport, &vc, c.device.id, &handler)
        .with_ops(vec!["echo".into()])
        .with_load_probe(Box::new(|| DeviceLoad {
            busy: 0,
            on_battery: Some(false),
            thermal_throttled: Some(false),
            ram_bytes: 8 << 30,
            cpu_cores: 4,
        }));
    srv_b.announce().await.unwrap();
    srv_c.announce().await.unwrap();

    let client = pai_broker::rpc::BrokerClient::new(&transport, &va, a.device.id);
    // Battery penalty outweighs B's hardware advantage.
    assert_eq!(client.find_peer("echo").await.unwrap(), Some(c.device.id));

    // B off the charger with its hardware advantage — wins now.
    let srv_b2 = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler)
        .with_ops(vec!["echo".into()])
        .with_load_probe(Box::new(|| DeviceLoad {
            busy: 0,
            on_battery: Some(false),
            thermal_throttled: Some(false),
            ram_bytes: 64 << 30,
            cpu_cores: 16,
        }));
    srv_b2.announce().await.unwrap();
    assert_eq!(client.find_peer("echo").await.unwrap(), Some(b.device.id));

    // Same hardware as C but running 3 ops — the busy penalty flips
    // placement back to idle C.
    let srv_b3 = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler)
        .with_ops(vec!["echo".into()])
        .with_load_probe(Box::new(|| DeviceLoad {
            busy: 3,
            on_battery: Some(false),
            thermal_throttled: Some(false),
            ram_bytes: 8 << 30,
            cpu_cores: 4,
        }));
    srv_b3.announce().await.unwrap();
    assert_eq!(client.find_peer("echo").await.unwrap(), Some(c.device.id));

    // Identical load on both — deterministic lowest-id tiebreak.
    let srv_b4 = pai_broker::rpc::BrokerServer::new(&transport, &vb, b.device.id, &handler)
        .with_ops(vec!["echo".into()])
        .with_load_probe(Box::new(|| DeviceLoad {
            busy: 0,
            on_battery: Some(false),
            thermal_throttled: Some(false),
            ram_bytes: 8 << 30,
            cpu_cores: 4,
        }));
    srv_b4.announce().await.unwrap();
    let want = if b.device.id.0 < c.device.id.0 { b.device.id } else { c.device.id };
    assert_eq!(client.find_peer("echo").await.unwrap(), Some(want));
}

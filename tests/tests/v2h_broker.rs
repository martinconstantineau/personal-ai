//! V2h tests: broker RPC over the sync transport — sealed request/
//! response objects between paired devices, target filtering, error
//! propagation, and timeout behavior.

use async_trait::async_trait;
use pai_broker::rpc::{BrokerClient, BrokerServer, OpHandler};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, pair, FolderTransport};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2h-{tag}-{}", uuid::Uuid::new_v4()));
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

fn pair_devices(a: &Dev, b: &Dev) {
    let agree_a = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();
    let offer = pair::make_offer(&a.device, &agree_a, &a.ids, &a.key_dir).unwrap();
    let accept = pair::accept_offer(
        &b.store, &offer, &b.device, &agree_b, &b.ids, &b.key_dir, &b.dir,
    )
    .unwrap();
    pair::complete_pairing(&a.store, &accept, &agree_a, &a.dir).unwrap();
}

/// Echo handler: reverses the payload bytes for "reverse", errors on
/// anything else.
struct Echo;
#[async_trait]
impl OpHandler for Echo {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        match op {
            "reverse" => Ok(payload.iter().rev().copied().collect()),
            other => Err(Error::InvalidInput(format!("unknown op {other}"))),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broker_roundtrip_sealed_objects() {
    let a = dev("cli-a");
    let b = dev("srv-b");
    pair_devices(&a, &b);
    let shared = tmpdir("shared");
    let transport = FolderTransport::new(shared.clone()).unwrap();
    let vault = crypto::vault_key(&a.dir).unwrap().unwrap();

    // B serves "reverse".
    let handler = Echo;
    let mut server = BrokerServer::new(&transport, &vault, b.device.id, &handler);

    // A calls B.
    let client = BrokerClient::new(&transport, &vault, a.device.id);
    let call = client.call(
        b.device.id,
        "reverse",
        b"hello broker",
        Duration::from_secs(10),
    );
    let serve = async {
        // Serve until the response lands.
        for _ in 0..50 {
            if server.serve_once().await.unwrap() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("server never saw the request");
    };
    let (resp, _) = tokio::join!(call, serve);
    assert_eq!(resp.unwrap(), b"rekorb olleh");

    // Wire objects are sealed — op name + payload never appear in the clear.
    for e in std::fs::read_dir(&shared).unwrap().flatten() {
        let raw = std::fs::read(e.path()).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            !text.contains("reverse"),
            "op leaked in {}",
            e.path().display()
        );
        assert!(!text.contains("hello broker"), "payload leaked");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_targets_only_addressed_peer() {
    let a = dev("req-a");
    let b = dev("tgt-b");
    let c = dev("tgt-c");
    // Vault model: the acceptor's vault flows to the offerer — a third
    // device joins the same vault by offering to a member.
    pair_devices(&a, &b);
    pair_devices(&c, &b);
    let shared = tmpdir("shared2");
    let transport = FolderTransport::new(shared).unwrap();
    let vault = crypto::vault_key(&a.dir).unwrap().unwrap();

    let handler = Echo;
    let mut srv_c = BrokerServer::new(&transport, &vault, c.device.id, &handler);
    let mut srv_b = BrokerServer::new(&transport, &vault, b.device.id, &handler);

    // A calls B — C's pass must see zero work (key prefix filters).
    let client = BrokerClient::new(&transport, &vault, a.device.id);
    client
        .call(b.device.id, "reverse", b"x", Duration::from_secs(0))
        .await
        .ok(); // timeout is fine — the request was pushed before the wait
    assert_eq!(
        srv_c.serve_once().await.unwrap(),
        0,
        "C picked up B's request"
    );
    assert_eq!(srv_b.serve_once().await.unwrap(), 1, "B missed its request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_error_and_timeout() {
    let a = dev("err-a");
    let b = dev("err-b");
    pair_devices(&a, &b);
    let shared = tmpdir("shared3");
    let transport = FolderTransport::new(shared).unwrap();
    let vault = crypto::vault_key(&a.dir).unwrap().unwrap();

    let handler = Echo;
    let mut server = BrokerServer::new(&transport, &vault, b.device.id, &handler);
    let client = BrokerClient::new(&transport, &vault, a.device.id);

    // Handler error propagates back to the caller. Serve until the
    // request lands — join! can poll serve_once before call's push.
    let call = client.call(b.device.id, "nope", b"x", Duration::from_secs(10));
    let serve = async {
        for _ in 0..50 {
            if server.serve_once().await.unwrap() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("server never saw the request");
    };
    let (resp, _) = tokio::join!(call, serve);
    let err = resp.unwrap_err().to_string();
    assert!(err.contains("unknown op"), "unexpected error: {err}");

    // Nobody serving → timeout.
    let miss = client
        .call(b.device.id, "reverse", b"x", Duration::from_millis(1200))
        .await;
    assert!(miss.unwrap_err().to_string().contains("no response"));
}

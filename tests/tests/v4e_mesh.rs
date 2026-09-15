//! V4e tests: `pai-mesh` LAN discovery — signed multicast announcements
//! locate a paired peer's sync relay, and relay auth is the
//! pairing-derived `hex(peer_key)` bearer token. No shared token file,
//! no manual --relay flag: discover → verify → sync.

use pai_core::*;
use pai_identity::IdentityStore;
use pai_memory::MemoryBackend;
use pai_mesh::*;
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, relay, SyncTransport};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v4e-{tag}-{}", uuid::Uuid::new_v4()));
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

fn ed_key(d: &Dev) -> [u8; 32] {
    d.device.public_key.as_slice().try_into().unwrap()
}

#[test]
fn announcement_signs_and_verifies() {
    let a = dev("a");
    let ann = make_announcement(&a.ids, &a.device, &a.key_dir, 8787).unwrap();
    assert!(verify_announcement(&a.ids, &ann, &ed_key(&a)));

    // Wrong key, tampered field, stale timestamp — all rejected.
    let b = dev("b");
    assert!(!verify_announcement(&a.ids, &ann, &ed_key(&b)));
    let mut tampered = ann.clone();
    tampered.port = 9999;
    assert!(!verify_announcement(&a.ids, &tampered, &ed_key(&a)));
    let mut stale = ann.clone();
    stale.ts = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    // re-sign with the right key — a replayed-but-authentic stale
    // announcement still fails the freshness window.
    let sig = a
        .ids
        .sign(a.device.id, &a.key_dir, stale.signing_payload().as_bytes())
        .unwrap();
    stale.sig = hex::encode(sig);
    assert!(!verify_announcement(&a.ids, &stale, &ed_key(&a)));
}

#[test]
fn discovery_filters_to_paired_verified_peers() {
    let (a, b, e) = (dev("a"), dev("b"), dev("e"));
    pair_devices(&b, &a); // B paired with A; E is a stranger

    // Listener on loopback — unicast "multicast" for a deterministic test.
    let listen_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let sock = bind_listener(listen_addr, None).unwrap();
    let port = sock.local_addr().unwrap().port();

    // A + E both announce to the listener.
    let sender = UdpSocket::bind("0.0.0.0:0").unwrap();
    let dest = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let ann_a = make_announcement(&a.ids, &a.device, &a.key_dir, 8787).unwrap();
    let ann_e = make_announcement(&e.ids, &e.device, &e.key_dir, 9999).unwrap();
    send_announcement(&sender, &ann_a, dest).unwrap();
    send_announcement(&sender, &ann_e, dest).unwrap();

    // B discovers: only A survives pairing + signature filtering.
    let found = discover(&sock, std::time::Duration::from_secs(1));
    assert_eq!(found.len(), 2); // both arrived as datagrams
    let paired = paired_announcements(&b.store, &b.ids, found).unwrap();
    assert_eq!(paired.len(), 1);
    assert_eq!(paired[0].peer.device_id, a.device.id);
    assert_eq!(paired[0].relay_addr.port(), 8787);
    // contact addr is the packet source, not a self-reported field
    assert_eq!(paired[0].relay_addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
}

#[tokio::test]
async fn peer_key_bearer_authenticates_mesh_relay() {
    let (a, b, e) = (dev("a"), dev("b"), dev("e"));
    pair_devices(&b, &a); // B↔A paired; E is on the LAN but unpaired
    let _ = e;

    // A's mesh relay: tokens = hex(peer_key) per paired peer, live.
    let agree_a = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let store_a = a.store.clone();
    let tokens = Arc::new(move || relay_tokens(&store_a, &agree_a.secret).unwrap_or_default());
    let srv = relay::bind_dynamic(tmpdir("relay"), "127.0.0.1:0", tokens).unwrap();
    let addr = srv.addr();
    std::thread::spawn(move || relay::serve(srv));

    // B connects with its pairwise token → accepted.
    let peer_a_on_b = pair::list_peers(&b.store)
        .unwrap()
        .into_iter()
        .find(|p| p.device_id == a.device.id)
        .unwrap();
    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();
    let good = relay::RelayTransport::new(
        format!("http://{addr}"),
        Some(token_for(&agree_b.secret, &peer_a_on_b)),
    );
    assert!(good.list().await.is_ok());

    // An unpaired device guessing at tokens → 401.
    let bad = relay::RelayTransport::new(format!("http://{addr}"), Some(hex::encode([7u8; 32])));
    assert!(bad.list().await.is_err());
}

#[tokio::test]
async fn discovered_relay_carries_real_sync() {
    let (a, b) = (dev("a"), dev("b"));
    pair_devices(&b, &a);

    // A serves a mesh relay (dynamic peer-key auth).
    let agree_a = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let store_a = a.store.clone();
    let tokens = Arc::new(move || relay_tokens(&store_a, &agree_a.secret).unwrap_or_default());
    let srv = relay::bind_dynamic(tmpdir("relay"), "127.0.0.1:0", tokens).unwrap();
    let relay_port: u16 = srv.addr().rsplit(':').next().unwrap().parse().unwrap();
    std::thread::spawn(move || relay::serve(srv));

    // A announces to B's listener (unicast in place of LAN multicast).
    let sock = bind_listener(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), None).unwrap();
    let disc_port = sock.local_addr().unwrap().port();
    let sender = UdpSocket::bind("0.0.0.0:0").unwrap();
    let ann = make_announcement(&a.ids, &a.device, &a.key_dir, relay_port).unwrap();
    send_announcement(
        &sender,
        &ann,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), disc_port),
    )
    .unwrap();

    // B: discover → verify → connect with peer-key token.
    let found = discover(&sock, std::time::Duration::from_secs(1));
    let paired = paired_announcements(&b.store, &b.ids, found).unwrap();
    assert_eq!(paired.len(), 1);
    let target = &paired[0];
    let agree_b = crypto::agreement_key(b.device.id, &b.dir).unwrap();
    let transport = relay::RelayTransport::new(
        format!("http://{}", target.relay_addr),
        Some(token_for(&agree_b.secret, &target.peer)),
    );

    // A writes a memory and pushes it through the same LAN relay.
    let mem = pai_memory::SqliteMemory::new(a.store.clone());
    let item = pai_memory::user_fact("lan sync fact", 0.9);
    mem.put(&item).await.unwrap();
    let agree_a2 = crypto::agreement_key(a.device.id, &a.dir).unwrap();
    let peer_b_on_a = pair::list_peers(&a.store)
        .unwrap()
        .into_iter()
        .find(|p| p.device_id == b.device.id)
        .unwrap();
    let t_a = relay::RelayTransport::new(
        format!("http://{}", target.relay_addr),
        Some(token_for(&agree_a2.secret, &peer_b_on_a)),
    );
    let vault_a = crypto::vault_key(&a.dir).unwrap().unwrap();
    let eng_a = engine::SyncEngine::new(t_a, a.store.clone(), vault_a, a.device.id, &a.dir);
    eng_a.push().await.unwrap();

    // B pulls over the discovered relay — the memory lands.
    let vault_b = crypto::vault_key(&b.dir).unwrap().unwrap();
    let eng_b = engine::SyncEngine::new(transport, b.store.clone(), vault_b, b.device.id, &b.dir);
    eng_b.pull().await.unwrap();
    let mem_b = pai_memory::SqliteMemory::new(b.store.clone());
    assert_eq!(mem_b.get(item.id).await.unwrap().content, "lan sync fact");
}

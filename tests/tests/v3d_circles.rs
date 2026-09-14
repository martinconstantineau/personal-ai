//! V3d tests: shared-memory circles — opt-in family/team scopes inside
//! a sync vault. A circle is a named key held by a subset of members;
//! `share_circle` rows seal under it, so non-member vault peers receive
//! ciphertext they can't open. Membership moves through ckg/ grant
//! objects sealed to each device's pairwise peer_key.

use pai_core::*;
use pai_identity::IdentityStore;
use pai_memory::{user_fact, MemoryBackend, SqliteMemory};
use pai_storage::Store;
use pai_sync::{circle, crypto, engine, pair, FolderTransport};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v3d-{tag}-{}", uuid::Uuid::new_v4()));
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

fn eng(d: &Dev, shared: &Path) -> engine::SyncEngine<FolderTransport> {
    engine::folder_engine(shared, d.store.clone(), d.device.id, &d.dir).unwrap()
}

fn peer_of(from: &Dev, to: &Dev) -> pair::SyncPeer {
    pair::list_peers(&from.store)
        .unwrap()
        .into_iter()
        .find(|p| p.device_id == to.device.id)
        .expect("peer must be paired")
}

async fn memory_on(d: &Dev, id: MemoryId) -> Option<pai_memory::MemoryItem> {
    let mem = SqliteMemory::new(d.store.clone());
    mem.get(id).await.ok()
}

/// Outer Option = row exists; inner = share_circle column value.
fn mem_scope(d: &Dev, id: MemoryId) -> Option<Option<String>> {
    d.store
        .with_conn(|c| {
            Ok(c.query_row(
                &format!("SELECT share_circle FROM memories WHERE id='{id}'"),
                [],
                |r| r.get(0),
            )
            .ok())
        })
        .unwrap()
}

#[test]
fn circle_create_holds_key_and_row() {
    let a = dev("a");
    let k = circle::create_circle(&a.store, &a.dir, "family", a.device.id).unwrap();
    assert_eq!(crypto::circle_key(&a.dir, "family").unwrap(), Some(k));
    let listed = circle::list_circles(&a.store).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "family");
    // bad names rejected
    assert!(circle::create_circle(&a.store, &a.dir, "bad/name", a.device.id).is_err());
    assert!(circle::create_circle(&a.store, &a.dir, "", a.device.id).is_err());
}

#[tokio::test]
async fn grant_reaches_only_the_target() {
    let (a, b, c) = (dev("a"), dev("b"), dev("c"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    pair_devices(&c, &a);
    circle::create_circle(&a.store, &a.dir, "family", a.device.id).unwrap();

    // Grant B — one object on the transport, sealed to B's pairwise key.
    eng(&a, &shared)
        .push_circle_grant("family", &peer_of(&a, &b))
        .await
        .unwrap();
    eng(&b, &shared).pull().await.unwrap();
    eng(&c, &shared).pull().await.unwrap();

    // B joined; C can't open the grant and stays a non-member.
    assert!(crypto::circle_key(&b.dir, "family").unwrap().is_some());
    assert_eq!(circle::list_circles(&b.store).unwrap().len(), 1);
    assert!(crypto::circle_key(&c.dir, "family").unwrap().is_none());
    assert!(circle::list_circles(&c.store).unwrap().is_empty());
}

#[tokio::test]
async fn circle_memory_federates_to_members_only() {
    let (a, b, c) = (dev("a"), dev("b"), dev("c"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    pair_devices(&c, &a);
    circle::create_circle(&a.store, &a.dir, "family", a.device.id).unwrap();
    eng(&a, &shared)
        .push_circle_grant("family", &peer_of(&a, &b))
        .await
        .unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // A remembers one circle fact + one vault fact.
    let mem_a = SqliteMemory::new(a.store.clone());
    let mut circ = user_fact("wifi password is hunter2", 0.9);
    circ.share_circle = Some("family".into());
    mem_a.put(&circ).await.unwrap();
    let vault = user_fact("prefers dark mode", 0.9);
    mem_a.put(&vault).await.unwrap();
    eng(&a, &shared).push().await.unwrap();

    // B (member): both arrive; the circle row is tagged.
    eng(&b, &shared).pull().await.unwrap();
    assert_eq!(mem_scope(&b, circ.id), Some(Some("family".into())));
    assert_eq!(
        memory_on(&b, circ.id).await.unwrap().content,
        "wifi password is hunter2"
    );
    assert!(memory_on(&b, vault.id).await.is_some());

    // C (non-member): vault row lands, circle row is unopenable -> absent.
    eng(&c, &shared).pull().await.unwrap();
    assert!(memory_on(&c, circ.id).await.is_none());
    assert!(memory_on(&c, vault.id).await.is_some());
}

#[tokio::test]
async fn grant_and_memory_apply_in_one_pull() {
    // ckg ranks before memory/ — a device granted a key in the same
    // sync batch as the row can open it immediately.
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    circle::create_circle(&a.store, &a.dir, "team", a.device.id).unwrap();
    let mem_a = SqliteMemory::new(a.store.clone());
    let mut m = user_fact("standup is 9:30", 0.9);
    m.share_circle = Some("team".into());
    mem_a.put(&m).await.unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&a, &shared)
        .push_circle_grant("team", &peer_of(&a, &b))
        .await
        .unwrap();

    eng(&b, &shared).pull().await.unwrap();
    assert_eq!(
        memory_on(&b, m.id).await.unwrap().content,
        "standup is 9:30"
    );
    assert_eq!(mem_scope(&b, m.id), Some(Some("team".into())));
}

#[tokio::test]
async fn circle_tombstone_propagates_to_members() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    circle::create_circle(&a.store, &a.dir, "family", a.device.id).unwrap();
    let mem_a = SqliteMemory::new(a.store.clone());
    let mut m = user_fact("temp fact", 0.9);
    m.share_circle = Some("family".into());
    mem_a.put(&m).await.unwrap();
    eng(&a, &shared)
        .push_circle_grant("family", &peer_of(&a, &b))
        .await
        .unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(memory_on(&b, m.id).await.is_some());

    // Delete on A — tombstone seals under the circle key too.
    mem_a.delete(m.id).await.unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(memory_on(&b, m.id).await.is_none());
    // Row exists but is marked deleted.
    let gone: bool = b
        .store
        .with_conn(|c| {
            c.query_row(
                &format!("SELECT deleted FROM memories WHERE id='{}'", m.id),
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(gone);
}

#[tokio::test]
async fn leaving_stops_future_circle_objects() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    circle::create_circle(&a.store, &a.dir, "family", a.device.id).unwrap();
    eng(&a, &shared)
        .push_circle_grant("family", &peer_of(&a, &b))
        .await
        .unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let mem_a = SqliteMemory::new(a.store.clone());
    let mut m1 = user_fact("first circle fact", 0.9);
    m1.share_circle = Some("family".into());
    mem_a.put(&m1).await.unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(memory_on(&b, m1.id).await.is_some());

    // B leaves: key dropped, the received row fenced to device-local.
    let n = circle::leave_circle(&b.store, &b.dir, "family").unwrap();
    assert_eq!(n, 1);
    assert!(crypto::circle_key(&b.dir, "family").unwrap().is_none());
    let scope: String = b
        .store
        .with_conn(|c| {
            c.query_row(
                &format!("SELECT sync_scope FROM memories WHERE id='{}'", m1.id),
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(scope, "device_local");

    // New circle objects are now unopenable on B.
    let mut m2 = user_fact("after B left", 0.9);
    m2.share_circle = Some("family".into());
    mem_a.put(&m2).await.unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(memory_on(&b, m2.id).await.is_none());
}

#[tokio::test]
async fn nonmember_grant_is_a_noop_error() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    // A never created 'ghost' — granting it must fail, not push junk.
    assert!(eng(&a, &shared)
        .push_circle_grant("ghost", &peer_of(&a, &b))
        .await
        .is_err());
}

#[tokio::test]
async fn member_pushes_back_to_the_circle() {
    // Federation is symmetric: B (granted member) writes a circle row;
    // it reaches A but not a third vault device.
    let (a, b, c) = (dev("a"), dev("b"), dev("c"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    pair_devices(&c, &a);
    circle::create_circle(&a.store, &a.dir, "family", a.device.id).unwrap();
    eng(&a, &shared)
        .push_circle_grant("family", &peer_of(&a, &b))
        .await
        .unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let mem_b = SqliteMemory::new(b.store.clone());
    let mut m = user_fact("B's circle reply", 0.9);
    m.share_circle = Some("family".into());
    mem_b.put(&m).await.unwrap();
    eng(&b, &shared).push().await.unwrap();

    eng(&a, &shared).pull().await.unwrap();
    eng(&c, &shared).pull().await.unwrap();
    assert_eq!(mem_scope(&a, m.id), Some(Some("family".into())));
    assert!(memory_on(&c, m.id).await.is_none());
}

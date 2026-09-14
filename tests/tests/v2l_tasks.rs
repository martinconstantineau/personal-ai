//! V2l tests: synced tasks with claim/lease — a due task is claimed by
//! one device so it doesn't execute everywhere, claims propagate over
//! the sealed sync transport, conflicting claims converge via LWW, and
//! expired leases make crashed runners' tasks reclaimable.

use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, FolderTransport};
use pai_tasks::{store as ts, ScheduledTask, TaskHandler};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2l-{tag}-{}", uuid::Uuid::new_v4()));
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

fn agent() -> AgentId {
    AgentId(uuid::Uuid::nil())
}

struct CountHandler {
    ran: AtomicUsize,
}

#[async_trait::async_trait]
impl TaskHandler for CountHandler {
    async fn handle(&self, _t: &ScheduledTask) -> Result<()> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A created task replicates to a peer with payload + trigger intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_sync_roundtrip() {
    let a = dev("rt-a");
    let b = dev("rt-b");
    pair_devices(&a, &b);
    let shared = tmpdir("rt-shared");

    let id = ts::create_task(
        &a.store,
        "check mail",
        agent(),
        None,
        &Trigger::Schedule {
            cron: "@every 60s".into(),
        },
        &serde_json::json!({"kind": "prompt", "text": "summarize inbox"}),
        SyncScope::Synchronized,
    )
    .unwrap();
    eng(&a, &shared).push().await.unwrap();

    let out = eng(&b, &shared).pull().await.unwrap();
    assert_eq!(out.pulled, 1);
    let t = ts::get_task(&b.store, id)
        .unwrap()
        .expect("task missing on B");
    assert_eq!(t.title, "check mail");
    assert_eq!(t.state, TaskState::Pending);
    assert_eq!(t.sync_scope, SyncScope::Synchronized);
    assert!(matches!(t.trigger, Trigger::Schedule { .. }));
    assert_eq!(t.payload["text"], "summarize inbox");
    assert!(t.claimed_by.is_none());
}

/// A claim made on one device suppresses the due task on the other —
/// the second device can't claim or run it while the lease is live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_propagates_and_suppresses_double_run() {
    let a = dev("cl-a");
    let b = dev("cl-b");
    pair_devices(&a, &b);
    let shared = tmpdir("cl-shared");

    let id = ts::create_task(
        &a.store,
        "reminder",
        agent(),
        None,
        &Trigger::Manual,
        &serde_json::json!({}),
        SyncScope::Synchronized,
    )
    .unwrap();
    assert!(ts::claim_task(&a.store, id, a.device.id, 300).unwrap());
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let t = ts::get_task(&b.store, id).unwrap().unwrap();
    assert_eq!(t.claimed_by, Some(a.device.id));
    assert_eq!(t.state, TaskState::Running);
    // Not due on B: live claim by another device.
    assert!(ts::due_tasks(&b.store, now()).unwrap().is_empty());
    assert!(
        !ts::claim_task(&b.store, id, b.device.id, 300).unwrap(),
        "B stole A's live claim"
    );
}

/// Two devices claiming the same task before seeing each other's push
/// both "win" locally, then converge to one holder once the objects
/// exchange — the newest claim survives on both replicas.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_claims_converge() {
    let a = dev("cc-a");
    let b = dev("cc-b");
    pair_devices(&a, &b);
    let shared = tmpdir("cc-shared");

    let id = ts::create_task(
        &a.store,
        "contended",
        agent(),
        None,
        &Trigger::Manual,
        &serde_json::json!({}),
        SyncScope::Synchronized,
    )
    .unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // Both claim while partitioned — B's claim is later.
    assert!(ts::claim_task(&a.store, id, a.device.id, 300).unwrap());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(ts::claim_task(&b.store, id, b.device.id, 300).unwrap());

    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).push().await.unwrap();
    eng(&a, &shared).pull().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let ta = ts::get_task(&a.store, id).unwrap().unwrap();
    let tb = ts::get_task(&b.store, id).unwrap().unwrap();
    assert_eq!(ta.claimed_by, tb.claimed_by, "replicas diverged");
    assert_eq!(ta.state, tb.state);
    assert_eq!(
        ta.claimed_by,
        Some(b.device.id),
        "the later claim should win on both devices"
    );
    // The loser no longer offers the task as due.
    assert!(ts::due_tasks(&a.store, now()).unwrap().is_empty());
}

/// A claim whose lease expired is reclaimable — crash recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_lease_is_reclaimable() {
    let a = dev("lx-a");
    let store = a.store.clone();
    let id = ts::create_task(
        &store,
        "recoverable",
        agent(),
        None,
        &Trigger::Manual,
        &serde_json::json!({}),
        SyncScope::Synchronized,
    )
    .unwrap();
    // Claim with an already-past lease (simulates a crashed runner).
    assert!(ts::claim_task(&store, id, a.device.id, -60).unwrap());
    // Another device can take it immediately.
    let other = DeviceId(uuid::Uuid::new_v4());
    assert!(
        ts::claim_task(&store, id, other, 300).unwrap(),
        "expired lease not reclaimable"
    );
    let t = ts::get_task(&store, id).unwrap().unwrap();
    assert_eq!(t.claimed_by, Some(other));
}

/// claim_and_run executes each due task exactly once on the claiming
/// device; a peer's pass finds nothing (finished + claim recorded).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_and_run_executes_once() {
    let a = dev("rn-a");
    let b = dev("rn-b");
    pair_devices(&a, &b);
    let shared = tmpdir("rn-shared");

    let id = ts::create_task(
        &a.store,
        "one-shot",
        agent(),
        None,
        &Trigger::Manual,
        &serde_json::json!({}),
        SyncScope::Synchronized,
    )
    .unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let handler = CountHandler {
        ran: AtomicUsize::new(0),
    };
    let ran = ts::claim_and_run(&a.store, &handler, a.device.id, now(), 300)
        .await
        .unwrap();
    assert_eq!(ran, vec![(id, true)]);
    assert_eq!(handler.ran.load(Ordering::SeqCst), 1);

    let t = ts::get_task(&a.store, id).unwrap().unwrap();
    assert_eq!(t.state, TaskState::Done);
    assert_eq!(t.claimed_by, Some(a.device.id));

    // Sync the finished row; B's run pass has nothing to do.
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    let ran_b = ts::claim_and_run(&b.store, &handler, b.device.id, now(), 300)
        .await
        .unwrap();
    assert!(ran_b.is_empty(), "B re-ran a finished task");
    assert_eq!(handler.ran.load(Ordering::SeqCst), 1, "task ran twice");
}

/// A schedule-triggered task requeues after running instead of
/// finishing — and the new run_at propagates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedule_task_requeues() {
    let a = dev("sc-a");
    let id = ts::create_task(
        &a.store,
        "recurring",
        agent(),
        None,
        &Trigger::Schedule {
            cron: "@every 3600s".into(),
        },
        &serde_json::json!({}),
        SyncScope::Synchronized,
    )
    .unwrap();
    let handler = CountHandler {
        ran: AtomicUsize::new(0),
    };
    let ran = ts::claim_and_run(&a.store, &handler, a.device.id, now(), 300)
        .await
        .unwrap();
    assert_eq!(ran, vec![(id, true)]);
    let t = ts::get_task(&a.store, id).unwrap().unwrap();
    assert_eq!(t.state, TaskState::Pending, "schedule task didn't requeue");
    assert!(t.run_at.unwrap() > now(), "requeue didn't move run_at");
    assert!(t.claimed_by.is_none(), "claim not released on requeue");
}

/// Removing a task propagates a tombstone — the peer never runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_tombstone_propagates() {
    let a = dev("tb-a");
    let b = dev("tb-b");
    pair_devices(&a, &b);
    let shared = tmpdir("tb-shared");

    let id = ts::create_task(
        &a.store,
        "doomed",
        agent(),
        None,
        &Trigger::Manual,
        &serde_json::json!({}),
        SyncScope::Synchronized,
    )
    .unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(ts::get_task(&b.store, id).unwrap().is_some());

    assert!(ts::remove_task(&a.store, id).unwrap());
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let t = ts::get_task(&b.store, id).unwrap().unwrap();
    assert!(t.deleted, "tombstone didn't reach B");
    assert!(ts::due_tasks(&b.store, now()).unwrap().is_empty());
}

/// A device-local task never leaves the device — scope is honored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_task_does_not_sync() {
    let a = dev("lc-a");
    let b = dev("lc-b");
    pair_devices(&a, &b);
    let shared = tmpdir("lc-shared");

    ts::create_task(
        &a.store,
        "private",
        agent(),
        None,
        &Trigger::Manual,
        &serde_json::json!({}),
        SyncScope::DeviceLocal,
    )
    .unwrap();
    eng(&a, &shared).push().await.unwrap();
    let out = eng(&b, &shared).pull().await.unwrap();
    assert_eq!(out.pulled, 0);
}

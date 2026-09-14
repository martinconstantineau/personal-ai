//! V2f tests: sync scope beyond memories — conversations+messages and
//! documents flow sealed through the same engine, tombstones included.

use pai_agent::ConversationStore;
use pai_core::*;
use pai_documents::DocumentStore;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, FolderTransport};
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2f-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Dev {
    dir: PathBuf,
    store: Arc<Store>,
    ids: IdentityStore,
    key_dir: PathBuf,
    device: Device,
    user: User,
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
        user,
    }
}

/// Pair two devices (offer/accept/complete) so both hold the vault key.
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

fn folder(d: &Dev, shared: &std::path::Path) -> engine::SyncEngine<FolderTransport> {
    engine::folder_engine(shared, d.store.clone(), d.device.id, &d.dir).unwrap()
}

fn msg(conv: ConversationId, role: Role, text: &str) -> Message {
    Message {
        id: MessageId::new(),
        conversation: conv,
        role,
        created_at: now(),
        content: vec![Content::Text { text: text.into() }],
        trust: TrustLevel::User,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversations_and_messages_sync() {
    let a = dev("conv-a");
    let b = dev("conv-b");
    pair_devices(&a, &b);
    let shared = tmpdir("shared");

    // A: synchronized conversation with two messages.
    let convs_a = ConversationStore::new(a.store.clone());
    let sess = convs_a
        .get_or_create_session(a.user.id, a.device.id)
        .unwrap();
    let conv = convs_a.create(sess, MemoryIsolation::Shared).unwrap();
    convs_a
        .set_sync_scope(conv.id, SyncScope::Synchronized)
        .unwrap();
    convs_a
        .append(&msg(conv.id, Role::User, "hello from A"))
        .unwrap();
    convs_a
        .append(&msg(conv.id, Role::Assistant, "hi there"))
        .unwrap();
    convs_a.rename(conv.id, "synced chat").unwrap();

    // A: a device-local conversation that must NOT travel.
    let local = convs_a.create(sess, MemoryIsolation::Shared).unwrap();
    convs_a
        .append(&msg(local.id, Role::User, "stays here"))
        .unwrap();

    let eng_a = folder(&a, &shared);
    assert!(eng_a.push().await.unwrap().pushed >= 3); // conv + 2 msgs

    let eng_b = folder(&b, &shared);
    let down = eng_b.pull().await.unwrap();
    assert!(down.pulled >= 3);

    // B sees the synced conversation, titled, with both messages in order.
    let convs_b = ConversationStore::new(b.store.clone());
    let got = convs_b.get(conv.id).unwrap();
    assert_eq!(got.title.as_deref(), Some("synced chat"));
    let msgs = convs_b.messages(conv.id).unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(matches!(msgs[0].role, Role::User));
    assert_eq!(
        msgs[0].content[0]
            .as_text()
            .map(String::from)
            .unwrap_or_default(),
        "hello from A"
    );
    // The device-local conversation never left A.
    assert!(convs_b.get(local.id).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_rename_and_delete_propagate() {
    let a = dev("ren-a");
    let b = dev("ren-b");
    pair_devices(&a, &b);
    let shared = tmpdir("shared2");

    let convs_a = ConversationStore::new(a.store.clone());
    let sess = convs_a
        .get_or_create_session(a.user.id, a.device.id)
        .unwrap();
    let conv = convs_a.create(sess, MemoryIsolation::Shared).unwrap();
    convs_a
        .set_sync_scope(conv.id, SyncScope::Synchronized)
        .unwrap();
    convs_a.append(&msg(conv.id, Role::User, "draft")).unwrap();

    let eng_a = folder(&a, &shared);
    eng_a.push().await.unwrap();
    let eng_b = folder(&b, &shared);
    eng_b.pull().await.unwrap();
    let convs_b = ConversationStore::new(b.store.clone());

    // Rename on A propagates (updated_at bump wins LWW).
    std::thread::sleep(std::time::Duration::from_millis(5));
    convs_a.rename(conv.id, "renamed").unwrap();
    eng_a.push().await.unwrap();
    eng_b.pull().await.unwrap();
    assert_eq!(
        convs_b.get(conv.id).unwrap().title.as_deref(),
        Some("renamed")
    );

    // Delete on B → tombstone back to A: conv hidden, messages gone.
    std::thread::sleep(std::time::Duration::from_millis(5));
    convs_b.delete(conv.id).unwrap();
    eng_b.push().await.unwrap();
    eng_a.pull().await.unwrap();
    assert!(
        !convs_a.list().unwrap().iter().any(|c| c.id == conv.id),
        "tombstoned conversation still listed"
    );
    assert!(convs_a.messages(conv.id).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn documents_sync_with_sections_and_blob() {
    let a = dev("doc-a");
    let b = dev("doc-b");
    pair_devices(&a, &b);
    let shared = tmpdir("shared3");

    let docs_a = DocumentStore::new(a.store.clone());
    let id = docs_a
        .ingest(
            b"alpha bravo charlie\n\ndelta echo foxtrot".as_slice(),
            "text/plain",
            Some("field notes"),
        )
        .await
        .unwrap();
    docs_a.set_sync_scope(id, SyncScope::Synchronized).unwrap();

    // A device-local document stays behind.
    let hidden = docs_a
        .ingest(b"secret".as_slice(), "text/plain", Some("hidden"))
        .await
        .unwrap();

    let eng_a = folder(&a, &shared);
    eng_a.push().await.unwrap();
    let eng_b = folder(&b, &shared);
    assert_eq!(eng_b.pull().await.unwrap().pulled, 1);

    // B: doc listed, FTS-searchable, blob bytes present.
    let docs_b = DocumentStore::new(b.store.clone());
    let list = docs_b.list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, id);
    assert_eq!(list[0].1.as_deref(), Some("field notes"));
    let hits = docs_b.search("bravo", 5).await.unwrap();
    assert!(!hits.is_empty(), "synced sections not searchable");
    assert_eq!(hits[0].document_id, id);

    // Blob bytes landed in B's content-addressed store.
    let blob_id: String = b
        .store
        .with_conn(|c| {
            c.query_row(
                &format!("SELECT blob FROM documents WHERE id='{id}'"),
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    let bytes = b.store.get_blob(&blob_id).unwrap();
    assert_eq!(bytes, b"alpha bravo charlie\n\ndelta echo foxtrot");

    // The hidden document never synced.
    assert!(docs_b.list().unwrap().iter().all(|(d, ..)| *d != hidden));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_delete_propagates_tombstone() {
    let a = dev("del-a");
    let b = dev("del-b");
    pair_devices(&a, &b);
    let shared = tmpdir("shared4");

    let docs_a = DocumentStore::new(a.store.clone());
    let id = docs_a
        .ingest(b"gone soon".as_slice(), "text/plain", None)
        .await
        .unwrap();
    docs_a.set_sync_scope(id, SyncScope::Synchronized).unwrap();

    let eng_a = folder(&a, &shared);
    eng_a.push().await.unwrap();
    let eng_b = folder(&b, &shared);
    eng_b.pull().await.unwrap();
    let docs_b = DocumentStore::new(b.store.clone());
    assert_eq!(docs_b.list().unwrap().len(), 1);

    std::thread::sleep(std::time::Duration::from_millis(5));
    docs_a.delete(id).unwrap();
    eng_a.push().await.unwrap();
    eng_b.pull().await.unwrap();
    assert!(
        docs_b.list().unwrap().is_empty(),
        "tombstoned document still listed"
    );
}

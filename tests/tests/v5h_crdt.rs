//! V5h tests: app CRDT documents — `data/crdt/<doc>.json` under an
//! installed app syncs as `acrdt/<app>/<doc>/<writer>` objects and
//! merges field-wise (LWW-map). Plain-JSON apps get collaboration for
//! free; explicit `{"v":..,"t":..}` / `{"d":true,"t":..}` cells give
//! per-field timestamps and deletes.

use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, FolderTransport, SyncTransport};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5h-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Dev {
    dir: PathBuf,
    store: Arc<Store>,
    ids: IdentityStore,
    key_dir: PathBuf,
    user: User,
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
        user,
        device,
    }
}

/// Acceptor's vault wins — pair both devices against the same acceptor
/// so every engine seals with the same key.
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

const MANIFEST: &str = r#"
[app]
name = "Crdt Notes"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"
"#;

fn deploy(d: &Dev) -> String {
    let dir = d.dir.join(format!("src-pkg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("app.wasm"), b"\0asm\x01\0\0\0").unwrap();
    let pkg = AppPackage::load(&dir).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    let devices = d.ids.list_devices(d.user.id).unwrap();
    let signer = pkg.verify_any(&d.ids, &devices).unwrap();
    let dev = devices.iter().find(|x| x.id == signer).unwrap();
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, dev, false)
        .unwrap();
    let now = pai_storage::ts(&now());
    d.store
        .with_conn(|c| {
            c.execute(
                "INSERT INTO apps(id, name, version, runtime, installed_at,
                    updated_at, deleted) VALUES(?1,?2,?3,?4,?5,?6,0)",
                rusqlite::params![
                    pkg.manifest.app_id(),
                    pkg.manifest.app.name,
                    pkg.manifest.app.version,
                    "wasm",
                    now,
                    now
                ],
            )?;
            Ok(())
        })
        .unwrap();
    pkg.manifest.app_id()
}

fn eng(d: &Dev, shared: &Path) -> engine::SyncEngine<FolderTransport> {
    engine::folder_engine(shared, d.store.clone(), d.device.id, &d.dir).unwrap()
}

fn write_doc(d: &Dev, app_id: &str, doc: &str, body: &str) {
    let dir = d.dir.join("apps").join(app_id).join("data").join("crdt");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{doc}.json")), body).unwrap();
}

fn read_doc(d: &Dev, app_id: &str, doc: &str) -> serde_json::Value {
    let p = d
        .dir
        .join("apps")
        .join(app_id)
        .join("data")
        .join("crdt")
        .join(format!("{doc}.json"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Concurrent field writes converge — A adds `title`, B adds `body`,
/// and after a push/pull cycle both see both fields.
#[tokio::test]
async fn concurrent_fields_merge_and_converge() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    let app_id = deploy(&a);
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap(); // package lands

    write_doc(&a, &app_id, "note", r#"{"title": "from-a"}"#);
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert_eq!(read_doc(&b, &app_id, "note")["title"], "from-a");

    // B edits a different field — its push republishes the merged doc.
    write_doc(
        &b,
        &app_id,
        "note",
        r#"{"title": "from-a", "body": "from-b"}"#,
    );
    eng(&b, &shared).push().await.unwrap();
    eng(&a, &shared).pull().await.unwrap();
    let doc = read_doc(&a, &app_id, "note");
    assert_eq!(doc["title"], "from-a");
    assert_eq!(doc["body"], "from-b");
}

/// A push with no doc changes ships nothing — provenance tracking keeps
/// the diff check honest even when mtimes are coarse.
#[tokio::test]
async fn unchanged_docs_do_not_repush() {
    let a = dev("a");
    let b = dev("b");
    pair_devices(&b, &a);
    let shared = tmpdir("shared");
    let app_id = deploy(&a);
    write_doc(&a, &app_id, "note", r#"{"k": "v"}"#);
    let first = eng(&a, &shared).push().await.unwrap();
    assert!(first.pushed > 0);
    let second = eng(&a, &shared).push().await.unwrap();
    assert_eq!(second.pushed, 0, "unchanged docs must not re-push");
}

/// Same field, two writers — the later timestamp wins on every peer,
/// and both sides land on the same value (no fork).
#[tokio::test]
async fn conflicting_field_resolves_lww() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    let app_id = deploy(&a);
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // A writes an old timestamp via an explicit cell; B writes a newer
    // plain value — B's newer cell must win regardless of pull order.
    write_doc(&a, &app_id, "note", r#"{"v": {"v": "a-old", "t": 1000}}"#);
    eng(&a, &shared).push().await.unwrap();
    write_doc(&b, &app_id, "note", r#"{"v": "b-new"}"#);
    eng(&b, &shared).push().await.unwrap();
    eng(&a, &shared).pull().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    assert_eq!(read_doc(&a, &app_id, "note")["v"], "b-new");
    assert_eq!(read_doc(&b, &app_id, "note")["v"], "b-new");
}

/// An explicit tombstone cell removes the field on the remote too.
#[tokio::test]
async fn tombstone_cell_deletes_field() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    let app_id = deploy(&a);
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    write_doc(&a, &app_id, "note", r#"{"x": 1, "y": 2}"#);
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert_eq!(read_doc(&b, &app_id, "note")["x"], 1);

    // A tombstones x with a far-future ts so it can't be out-written.
    write_doc(
        &a,
        &app_id,
        "note",
        r#"{"x": {"d": true, "t": 99999999999999}, "y": 2}"#,
    );
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    let doc = read_doc(&b, &app_id, "note");
    assert!(doc["x"].is_null());
    assert_eq!(doc["y"], 2);
}

/// An unpaired writer's object is dropped — the cells never land and
/// the doc never materializes.
#[tokio::test]
async fn unpaired_writer_object_is_dropped() {
    let (a, b, c, d) = (dev("a"), dev("b"), dev("c"), dev("d"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a); // A + B share a vault
    let app_id = deploy(&a);
    eng(&a, &shared).push().await.unwrap();

    // C has its own vault (paired with D, never with A) and forges an
    // acrdt object for A's app — sealed under a key A doesn't hold, it
    // must be skipped as unopenable, never applied.
    pair_devices(&d, &c); // C acceptor — C's own vault, A stays out
    let vault_c = crypto::vault_key(&c.dir).unwrap().unwrap();
    let key = format!("acrdt/{app_id}/note/{}", c.device.id);
    let payload = serde_json::json!({
        "v": 1, "app": app_id, "doc": "note",
        "writer": c.device.id.to_string(),
        "fields": {"x": {"v": 1, "t": 1}},
        "updated_at": pai_storage::ts(&now()),
    });
    let obj = SyncObject {
        key: key.clone(),
        ciphertext: crypto::seal(&vault_c, key.as_bytes(), payload.to_string().as_bytes()).unwrap(),
        version: 1,
        writer: c.device.id,
        updated_at: now(),
        tombstone: false,
    };
    FolderTransport::new(shared.clone())
        .unwrap()
        .push(&obj)
        .await
        .unwrap();
    eng(&b, &shared).pull().await.unwrap(); // B pulls — forged object skipped
    assert!(!b
        .dir
        .join("apps")
        .join(&app_id)
        .join("data/crdt/note.json")
        .exists());
}

/// CRDT cells that arrive before the package materialize on install —
/// data written ahead of the app isn't stranded in the table. The app
/// object is withheld from B's first pull so the cells land first.
#[tokio::test]
async fn cells_arriving_before_install_materialize() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a);
    write_doc(&a, &app_id, "note", r#"{"early": true}"#);
    eng(&a, &shared).push().await.unwrap();

    // Withhold the app/ object — B pulls only the acrdt cells.
    let aside = tmpdir("aside");
    let app_obj = shared.join(format!("app%2f{app_id}.syncobj"));
    std::fs::rename(&app_obj, aside.join("app.syncobj")).unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(!b.dir.join("apps").join(&app_id).is_dir()); // not installed
    assert!(!b
        .dir
        .join("apps")
        .join(&app_id)
        .join("data/crdt/note.json")
        .exists());

    // Now the package lands — materialize_all fires on install.
    std::fs::rename(aside.join("app.syncobj"), &app_obj).unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert_eq!(read_doc(&b, &app_id, "note")["early"], true);
}

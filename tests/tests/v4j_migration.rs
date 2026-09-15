//! V4j tests: app placement + migration — `apps.active_device` marks
//! the one device that runs an app's live `data/`; `pai apps migrate`
//! ships a migrate-flagged `bkp/` object plus the placement update in
//! the `app/` object, and pull applies them in rank order (package
//! first, then the inline restore on the addressed device only).

use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{backup, crypto, engine, pair, FolderTransport, SyncTransport};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v4j-{tag}-{}", uuid::Uuid::new_v4()));
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

fn manifest(name: &str) -> String {
    format!(
        r#"[app]
name = "{name}"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"

[storage]
type = "sqlite"
path = "schema.sql"

[migration]
auto_migrate = true
"#
    )
}

fn signed_pkg_named(root: &Path, d: &Dev, name: &str) -> PathBuf {
    let dir = root.join(format!("src-pkg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.toml"), manifest(name)).unwrap();
    std::fs::write(dir.join("app.wasm"), b"\0asm\x01\0\0\0").unwrap();
    std::fs::write(
        dir.join("schema.sql"),
        b"create table if not exists notes(x);",
    )
    .unwrap();
    let pkg = AppPackage::load(&dir).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    dir
}

fn signed_pkg(root: &Path, d: &Dev) -> PathBuf {
    signed_pkg_named(root, d, "Backed Up Notes")
}

fn deploy(d: &Dev, pkg_dir: &Path) -> String {
    let pkg = AppPackage::load(pkg_dir).unwrap();
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

fn app_dir(d: &Dev, id: &str) -> PathBuf {
    d.dir.join("apps").join(id)
}

fn seed_note(d: &Dev, app_id: &str, val: &str) {
    let db = app_dir(d, app_id).join("data/data.db");
    let c = rusqlite::Connection::open(db).unwrap();
    c.execute("insert into notes(x) values (?1)", [val])
        .unwrap();
}

fn note_vals(d: &Dev, app_id: &str) -> Vec<String> {
    let db = app_dir(d, app_id).join("data/data.db");
    if !db.is_file() {
        return vec![];
    }
    let c = rusqlite::Connection::open(db).unwrap();
    let mut s = c.prepare("select x from notes").unwrap();
    s.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn active_device(d: &Dev, app_id: &str) -> Option<String> {
    d.store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT active_device FROM apps WHERE id=?1",
                rusqlite::params![app_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
        })
        .unwrap()
}

/// The `pai apps migrate <id> --to <target>` arm, minus CLI printing:
/// snapshot flagged for the target, hand over placement, park data.
fn migrate(d: &Dev, app_id: &str, target: DeviceId) {
    backup::create(&d.store, &d.dir, d.device.id, app_id, Some(target)).unwrap();
    let now = pai_storage::ts(&now());
    d.store
        .with_conn(|c| {
            c.execute(
                "UPDATE apps SET active_device=?2, updated_at=?3 WHERE id=?1",
                rusqlite::params![app_id, target.to_string(), now],
            )?;
            Ok(())
        })
        .unwrap();
    AppRegistry::new(&d.dir).deactivate_data(app_id).unwrap();
}

fn parked_data(d: &Dev, app_id: &str) -> Vec<PathBuf> {
    let root = d.dir.join("apps");
    std::fs::read_dir(&root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&format!(".{app_id}.data.inactive-")))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The happy path: A migrates to B — B installs the package AND gets
/// the live data inline; A's instance is quiesced.
#[tokio::test]
async fn migrate_moves_active_instance_to_target() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    seed_note(&a, &app_id, "live-on-a");
    migrate(&a, &app_id, b.device.id);
    assert_eq!(
        active_device(&a, &app_id).as_deref(),
        Some(b.device.id.to_string().as_str())
    );
    assert!(!app_dir(&a, &app_id).join("data").exists());
    assert_eq!(parked_data(&a, &app_id).len(), 1);

    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // B: package installed, placement says B, data restored inline.
    assert!(app_dir(&b, &app_id).join("manifest.toml").is_file());
    assert_eq!(
        active_device(&b, &app_id).as_deref(),
        Some(b.device.id.to_string().as_str())
    );
    assert_eq!(note_vals(&b, &app_id), vec!["live-on-a".to_string()]);
}

/// After migrating away, the source refuses everything that would
/// fork live state; the un-migrated app stays unrestricted.
#[tokio::test]
async fn inactive_device_refuses_backup_restore_and_run_check() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    seed_note(&a, &app_id, "x");
    migrate(&a, &app_id, b.device.id);
    eng(&a, &shared).push().await.unwrap();

    assert_eq!(
        backup::active_elsewhere(&a.store, a.device.id, &app_id).unwrap(),
        Some(b.device.id.to_string())
    );
    assert!(backup::create(&a.store, &a.dir, a.device.id, &app_id, None).is_err());
    assert!(
        backup::restore(&a.store, &a.dir, a.device.id, &app_id, None).is_err(),
        "restore on the inactive source must refuse"
    );

    // An unplaced app (active_device NULL) has no restriction.
    let app2 = deploy(&a, &signed_pkg_named(&a.dir, &a, "Other App"));
    assert_eq!(
        backup::active_elsewhere(&a.store, a.device.id, &app2).unwrap(),
        None
    );
    assert!(backup::create(&a.store, &a.dir, a.device.id, &app2, None).is_ok());
}

/// A third device keeps distributing the package but sheds its stale
/// live data and must not restore the migrate-flagged backup.
#[tokio::test]
async fn third_device_keeps_package_sheds_data_no_restore() {
    let (a, b, c) = (dev("a"), dev("b"), dev("c"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    pair_devices(&c, &a);

    // Legacy phase: app unplaced, C pulls it and grows live data.
    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    eng(&a, &shared).push().await.unwrap();
    eng(&c, &shared).pull().await.unwrap();
    assert_eq!(active_device(&c, &app_id), None);
    seed_note(&c, &app_id, "c-stale");

    // A migrates to B; C pulls the updated objects.
    migrate(&a, &app_id, b.device.id);
    eng(&a, &shared).push().await.unwrap();
    eng(&c, &shared).pull().await.unwrap();

    // C: package still installed, placement says B, live data parked.
    assert!(app_dir(&c, &app_id).join("manifest.toml").is_file());
    assert_eq!(
        active_device(&c, &app_id).as_deref(),
        Some(b.device.id.to_string().as_str())
    );
    assert!(!app_dir(&c, &app_id).join("data").exists());
    assert_eq!(parked_data(&c, &app_id).len(), 1);

    // The bkp object naming migrate_to=B must not restore on C —
    // even though C stored the pak.
    assert_eq!(note_vals(&c, &app_id), Vec::<String>::new());
    assert!(backup::create(&c.store, &c.dir, c.device.id, &app_id, None).is_err());
    assert!(backup::restore(&c.store, &c.dir, c.device.id, &app_id, None).is_err());
}

/// Pulling the same migration twice is a no-op — mirror versions
/// already cover it, and a second restore would be harmless anyway.
#[tokio::test]
async fn repeated_pull_is_idempotent() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    seed_note(&a, &app_id, "once");
    migrate(&a, &app_id, b.device.id);
    eng(&a, &shared).push().await.unwrap();

    eng(&b, &shared).pull().await.unwrap();
    let second = eng(&b, &shared).pull().await.unwrap();
    assert_eq!(second.pulled, 0);
    assert_eq!(note_vals(&b, &app_id), vec!["once".to_string()]);
}

/// A migrate-flagged backup from an unpaired writer is dropped at
/// apply — it can never light up an instance on the target.
#[tokio::test]
async fn forged_migration_writer_never_restores() {
    let (a, b, mallory) = (dev("a"), dev("b"), dev("mallory"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    // mallory is NOT paired with b.

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // A vault-holder crafts an object naming mallory as writer and B
    // as migrate_to — consistent key/writer/payload, but the writer
    // names a device B never paired with.
    let payload = serde_json::json!({
        "v": 1,
        "app_id": app_id,
        "writer": mallory.device.id.to_string(),
        "created_at": pai_storage::ts(&now()),
        "package": {
            "v": 1, "id": app_id, "name": "Notes", "version": "1.0.0",
            "runtime": "wasm", "updated_at": pai_storage::ts(&now()),
            "signature_b64": "", "files": []
        },
        "data_files": [{"path": "x.txt", "b64": "aGk="}],
        "migrate_to": b.device.id.to_string()
    });
    let key = format!("bkp/{app_id}/{}", mallory.device.id);
    let vault = crypto::vault_key(&a.dir).unwrap().unwrap();
    let ciphertext = crypto::seal(&vault, key.as_bytes(), payload.to_string().as_bytes()).unwrap();
    let obj = SyncObject {
        key: key.clone(),
        ciphertext,
        version: 9,
        writer: mallory.device.id,
        updated_at: now(),
        tombstone: false,
    };
    FolderTransport::new(shared.clone())
        .unwrap()
        .push(&obj)
        .await
        .unwrap();

    eng(&b, &shared).pull().await.unwrap();
    // Dropped: no pak, no row, no restored data, no placement change.
    assert_eq!(note_vals(&b, &app_id), Vec::<String>::new());
    assert_eq!(active_device(&b, &app_id), None);
}

/// Migrate, then migrate back: A→B→A leaves exactly one live
/// instance and B's instance is quiesced by the placement update.
#[tokio::test]
async fn migrate_back_reactivates_source() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    seed_note(&a, &app_id, "round-trip");
    migrate(&a, &app_id, b.device.id);
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert_eq!(note_vals(&b, &app_id), vec!["round-trip".to_string()]);

    // B adds state, migrates back to A.
    seed_note(&b, &app_id, "b-added");
    migrate(&b, &app_id, a.device.id);
    eng(&b, &shared).push().await.unwrap();
    eng(&a, &shared).pull().await.unwrap();

    let mut vals = note_vals(&a, &app_id);
    vals.sort();
    assert_eq!(vals, vec!["b-added".to_string(), "round-trip".to_string()]);
    assert_eq!(
        active_device(&a, &app_id).as_deref(),
        Some(a.device.id.to_string().as_str())
    );
    // B is inactive again.
    assert_eq!(
        active_device(&b, &app_id).as_deref(),
        Some(a.device.id.to_string().as_str())
    );
    assert!(!app_dir(&b, &app_id).join("data").exists());
}

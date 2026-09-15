//! V4f tests: app backups — `pai apps backup` snapshots an installed
//! app's package + live `data/` into a sealed `bkp/<app>/<writer>`
//! object that roams to paired devices. Applying a backup only stores
//! the pak (never touches the live install); `apps restore` is the
//! explicit, verified path that puts the state back.

use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{backup, crypto, engine, pair, FolderTransport, SyncTransport};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v4f-{tag}-{}", uuid::Uuid::new_v4()));
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

const MANIFEST: &str = r#"
[app]
name = "Backed Up Notes"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"

[storage]
type = "sqlite"
path = "schema.sql"

[migration]
auto_migrate = true
"#;

fn signed_pkg(root: &Path, d: &Dev) -> PathBuf {
    let dir = root.join(format!("src-pkg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.toml"), MANIFEST).unwrap();
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
    let c = rusqlite::Connection::open(db).unwrap();
    let mut s = c.prepare("select x from notes").unwrap();
    s.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn backup_row(d: &Dev, app_id: &str, writer: &str) -> Option<(String, bool)> {
    d.store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT created_at, deleted FROM app_backups
                 WHERE app_id=?1 AND writer=?2",
                rusqlite::params![app_id, writer],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)),
            )
            .ok())
        })
        .unwrap()
}

fn pak_path(d: &Dev, app_id: &str, writer: &DeviceId) -> PathBuf {
    d.dir
        .join("backups")
        .join(app_id)
        .join(format!("{writer}.pak"))
}

/// Full happy path: backup ships, lands, and restores live state.
#[tokio::test]
async fn backup_syncs_and_restores_state() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    seed_note(&a, &app_id, "from-a");
    backup::create(&a.store, &a.dir, a.device.id, &app_id, None).unwrap();
    eng(&a, &shared).push().await.unwrap();

    let out = eng(&b, &shared).pull().await.unwrap();
    assert!(out.pulled >= 2); // app/<id> + bkp/<id>/<a>
    assert!(pak_path(&b, &app_id, &a.device.id).is_file());
    assert_eq!(
        backup_row(&b, &app_id, &a.device.id.to_string()).map(|(_, d)| d),
        Some(false)
    );

    // The synced install provisioned an empty db; restore swaps in A's.
    assert!(note_vals(&b, &app_id).is_empty());
    let p = backup::restore(&b.store, &b.dir, b.device.id, &app_id, None).unwrap();
    assert_eq!(p.writer, a.device.id.to_string());
    assert_eq!(note_vals(&b, &app_id), vec!["from-a".to_string()]);
}

/// A pak is self-contained: after the install is lost, restore
/// rebuilds package + data without the `app/` object being re-pulled.
#[tokio::test]
async fn restore_rebuilds_app_from_pak_alone() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    seed_note(&a, &app_id, "rescue-me");
    backup::create(&a.store, &a.dir, a.device.id, &app_id, None).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // Simulate losing the install entirely.
    assert!(AppRegistry::new(&b.dir).remove(&app_id).unwrap());
    assert!(!app_dir(&b, &app_id).exists());

    backup::restore(&b.store, &b.dir, b.device.id, &app_id, None).unwrap();
    assert!(app_dir(&b, &app_id).join("manifest.toml").is_file());
    assert_eq!(note_vals(&b, &app_id), vec!["rescue-me".to_string()]);
}

/// A backup received from a peer is stored for restore but must never
/// re-seal and echo back out under our writer id.
#[tokio::test]
async fn received_backup_never_repushes() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    backup::create(&a.store, &a.dir, a.device.id, &app_id, None).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(pak_path(&b, &app_id, &a.device.id).is_file());

    // B's turn to push — every bkp/ object on the transport must still
    // be A-written; B stored the row but ships nothing.
    eng(&b, &shared).push().await.unwrap();
    let metas = FolderTransport::new(shared.clone())
        .unwrap()
        .list()
        .await
        .unwrap();
    for m in metas.iter().filter(|m| m.key.starts_with("bkp/")) {
        assert_eq!(m.writer, a.device.id, "B echoed A's backup: {}", m.key);
    }
}

/// `apps backup-delete` tombstones my backup — peers drop their copy.
#[tokio::test]
async fn backup_delete_propagates_tombstone() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    backup::create(&a.store, &a.dir, a.device.id, &app_id, None).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(pak_path(&b, &app_id, &a.device.id).is_file());

    assert!(backup::delete(&a.store, &a.dir, a.device.id, &app_id).unwrap());
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    assert!(!pak_path(&b, &app_id, &a.device.id).exists());
    assert_eq!(
        backup_row(&b, &app_id, &a.device.id.to_string()).map(|(_, d)| d),
        Some(true)
    );
}

/// A sealed object whose key-segment writer, object writer, and payload
/// writer disagree is dropped — never stored, never restorable.
#[tokio::test]
async fn forged_writer_backup_is_dropped() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    // B crafts an object claiming to be A's backup of a ghost app: key
    // names A, payload names A, but B sealed it — so SyncObject.writer
    // is B and the claims disagree.
    let payload = serde_json::json!({
        "v": 1,
        "app_id": "ghost-app",
        "writer": a.device.id.to_string(),
        "created_at": pai_storage::ts(&now()),
        "package": {
            "v": 1, "id": "ghost-app", "name": "Ghost", "version": "1.0.0",
            "runtime": "wasm", "updated_at": pai_storage::ts(&now()),
            "signature_b64": "", "files": []
        },
        "data_files": []
    });
    let key = format!("bkp/ghost-app/{}", a.device.id);
    let vault = crypto::vault_key(&b.dir).unwrap().unwrap();
    let ciphertext = crypto::seal(&vault, key.as_bytes(), payload.to_string().as_bytes()).unwrap();
    let obj = SyncObject {
        key: key.clone(),
        ciphertext,
        version: 1,
        writer: b.device.id,
        updated_at: now(),
        tombstone: false,
    };
    FolderTransport::new(shared.clone())
        .unwrap()
        .push(&obj)
        .await
        .unwrap();

    eng(&a, &shared).pull().await.unwrap();
    assert!(!pak_path(&a, "ghost-app", &a.device.id).exists());
    assert!(backup_row(&a, "ghost-app", &a.device.id.to_string()).is_none());
}

/// After unpairing, a writer's newer backups stop landing — old objects
/// stay, but nothing fresh is accepted from a device we no longer trust.
#[tokio::test]
async fn unpaired_writer_backup_is_dropped() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg(&a.dir, &a));
    backup::create(&a.store, &a.dir, a.device.id, &app_id, None).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    let first = backup_row(&b, &app_id, &a.device.id.to_string()).unwrap().0;

    // B unpairs A.
    b.store
        .with_conn(|c| {
            c.execute(
                "DELETE FROM sync_peers WHERE device_id=?1",
                rusqlite::params![a.device.id.to_string()],
            )?;
            Ok(())
        })
        .unwrap();

    // A re-snapshots (newer created_at) and ships it.
    std::thread::sleep(std::time::Duration::from_millis(2));
    backup::create(&a.store, &a.dir, a.device.id, &app_id, None).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // The newer object opened fine (vault seal) but the writer is no
    // longer paired — the stored row keeps the first snapshot's stamp.
    assert_eq!(
        backup_row(&b, &app_id, &a.device.id.to_string()).unwrap().0,
        first
    );
}

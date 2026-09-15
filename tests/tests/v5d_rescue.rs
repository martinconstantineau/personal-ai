//! V5d tests: rescue — `backup::rescue` claims `active_device` for a
//! dead/lost device and restores the newest backup's data; the claim
//! propagates via `app/` so a resurrected device parks its stale data
//! on pull (bounded fork window, same healing path as migration).

use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{backup, crypto, engine, pair, FolderTransport};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5d-{tag}-{}", uuid::Uuid::new_v4()));
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

fn deploy(d: &Dev, pkg_dir: &Path, placed_on_self: bool) -> String {
    let pkg = AppPackage::load(pkg_dir).unwrap();
    let devices = d.ids.list_devices(d.user.id).unwrap();
    let signer = pkg.verify_any(&d.ids, &devices).unwrap();
    let dev = devices.iter().find(|x| x.id == signer).unwrap();
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, dev, false)
        .unwrap();
    let now = pai_storage::ts(&now());
    let active = placed_on_self.then(|| d.device.id.to_string());
    d.store
        .with_conn(|c| {
            c.execute(
                "INSERT INTO apps(id, name, version, runtime, installed_at,
                    updated_at, deleted, active_device) VALUES(?1,?2,?3,?4,?5,?6,0,?7)",
                rusqlite::params![
                    pkg.manifest.app_id(),
                    pkg.manifest.app.name,
                    pkg.manifest.app.version,
                    "wasm",
                    now,
                    now,
                    active
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

/// The rescue story: app lived on A (placed, backed up). A dies.
/// B pulls the world, then rescues — claims placement and restores
/// the backup's data. B's push + A's hypothetical next pull parks A's
/// stale data — the bounded fork window.
#[tokio::test]
async fn rescue_claims_placement_and_restores_data() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a); // A accepts — A's vault is authoritative.

    let app_id = deploy(&a, &signed_pkg_named(&a.dir, &a, "Notes"), true);
    seed_note(&a, &app_id, "live-on-a");
    backup::create(&a.store, &a.dir, a.device.id, &app_id, None).unwrap();

    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // B has the package + a stored backup, but placement is A's —
    // plain restore is refused, and the app's data/ isn't live.
    assert_eq!(
        active_device(&b, &app_id).as_deref(),
        Some(a.device.id.to_string().as_str())
    );
    assert!(backup::restore(&b.store, &b.dir, b.device.id, &app_id, None).is_err());

    // A is gone — B rescues.
    let outcome = backup::rescue(&b.store, &b.dir, b.device.id, &app_id).unwrap();
    match outcome {
        backup::RescueOutcome::Restored { .. } => {}
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(
        active_device(&b, &app_id).as_deref(),
        Some(b.device.id.to_string().as_str())
    );
    assert_eq!(note_vals(&b, &app_id), vec!["live-on-a".to_string()]);

    // B's claim propagates; if A resurrects and pulls, its stale
    // data/ is parked — the fork window closes.
    eng(&b, &shared).push().await.unwrap();
    eng(&a, &shared).pull().await.unwrap();
    assert_eq!(
        active_device(&a, &app_id).as_deref(),
        Some(b.device.id.to_string().as_str())
    );
    assert!(!app_dir(&a, &app_id).join("data").exists());
    assert_eq!(parked_data(&a, &app_id).len(), 1);
}

/// `rescue --all --from <dead>`: every app placed on the dead device
/// is claimed + restored; unplaced apps aren't "on" it and are left.
#[tokio::test]
async fn rescue_from_claims_all_apps_on_dead_device() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app1 = deploy(&a, &signed_pkg_named(&a.dir, &a, "One"), true);
    let app2 = deploy(&a, &signed_pkg_named(&a.dir, &a, "Two"), true);
    // Unplaced (NULL) — runs-everywhere semantics, not "on" A.
    let app3 = deploy(&a, &signed_pkg_named(&a.dir, &a, "Three"), false);
    seed_note(&a, &app1, "d1");
    seed_note(&a, &app2, "d2");
    backup::create(&a.store, &a.dir, a.device.id, &app1, None).unwrap();
    backup::create(&a.store, &a.dir, a.device.id, &app2, None).unwrap();
    backup::create(&a.store, &a.dir, a.device.id, &app3, None).unwrap();

    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let results = backup::rescue_from(&b.store, &b.dir, b.device.id, a.device.id).unwrap();
    assert_eq!(results.len(), 2);
    for (id, outcome) in &results {
        assert!(
            matches!(outcome, backup::RescueOutcome::Restored { .. }),
            "{id}: {outcome:?}"
        );
        assert_eq!(
            active_device(&b, id).as_deref(),
            Some(b.device.id.to_string().as_str())
        );
    }
    // The unplaced app was untouched.
    assert_eq!(active_device(&b, &app3), None);

    // Same-device --from is meaningless and refused.
    assert!(backup::rescue_from(&b.store, &b.dir, b.device.id, b.device.id).is_err());
}

/// Rescue with no backup: placement is still claimed — the app
/// becomes a fresh install here.
#[tokio::test]
async fn rescue_without_backup_claims_anyway() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let app_id = deploy(&a, &signed_pkg_named(&a.dir, &a, "NoBackup"), true);
    seed_note(&a, &app_id, "dies-with-a");
    // No backup::create — the snapshot never happened.
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let outcome = backup::rescue(&b.store, &b.dir, b.device.id, &app_id).unwrap();
    assert!(matches!(outcome, backup::RescueOutcome::ClaimedNoBackup));
    assert_eq!(
        active_device(&b, &app_id).as_deref(),
        Some(b.device.id.to_string().as_str())
    );
    assert_eq!(note_vals(&b, &app_id), Vec::<String>::new());
}

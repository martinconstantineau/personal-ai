//! V4d tests: app-package sync — a deployed app roams to paired devices
//! as a sealed `app/<id>` object carrying the whole package + signature.
//! The receiver verifies the signature against a known key (own devices
//! or `sync_peers.ed_pubkey`) before installing; removals ship as
//! tombstones. Live `data/` (app state) never travels.

use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, FolderTransport};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v4d-{tag}-{}", uuid::Uuid::new_v4()));
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

const MANIFEST: &str = r#"
[app]
name = "Synced Notes"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"

[storage]
type = "sqlite"
path = "schema.sql"

[migration]
auto_migrate = true
"#;

const MANIFEST_V2: &str = r#"
[app]
name = "Synced Notes"
version = "2.0.0"
entrypoint = "app.wasm"
runtime = "wasm"

[storage]
type = "sqlite"
path = "schema.sql"

[migration]
auto_migrate = true
"#;

/// A package dir under `root/src-pkg`, signed by `d`'s device.
fn signed_pkg(root: &Path, d: &Dev, manifest: &str) -> PathBuf {
    let dir = root.join(format!("src-pkg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("files")).unwrap();
    std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
    std::fs::write(dir.join("app.wasm"), b"\0asm\x01\0\0\0").unwrap();
    // schema.sql is re-applied on every provision — must be idempotent.
    std::fs::write(
        dir.join("schema.sql"),
        b"create table if not exists notes(x);",
    )
    .unwrap();
    std::fs::write(dir.join("files/seed.txt"), b"hello").unwrap();
    let pkg = AppPackage::load(&dir).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    dir
}

/// Mirror of the CLI deploy path: verify, install, upsert `apps` row.
fn deploy(d: &Dev, pkg_dir: &Path, upgrade: bool) -> String {
    let pkg = AppPackage::load(pkg_dir).unwrap();
    let devices = d.ids.list_devices(d.user.id).unwrap();
    let signer = pkg.verify_any(&d.ids, &devices).unwrap();
    let dev = devices.iter().find(|x| x.id == signer).unwrap();
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, dev, upgrade)
        .unwrap();
    upsert_app_row(d, &pkg, None);
    pkg.manifest.app_id()
}

fn upsert_app_row(d: &Dev, pkg: &AppPackage, updated_at: Option<String>) {
    let now = pai_storage::ts(&now());
    d.store
        .with_conn(|c| {
            c.execute(
                "INSERT INTO apps(id, name, version, runtime, installed_at,
                    updated_at, deleted) VALUES(?1,?2,?3,?4,?5,?6,0)
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name,
                    version=excluded.version, runtime=excluded.runtime,
                    updated_at=excluded.updated_at, deleted=0",
                rusqlite::params![
                    pkg.manifest.app_id(),
                    pkg.manifest.app.name,
                    pkg.manifest.app.version,
                    format!("{:?}", pkg.manifest.app.runtime).to_lowercase(),
                    now,
                    updated_at.unwrap_or_else(|| now.clone()),
                ],
            )?;
            Ok(())
        })
        .unwrap();
}

fn apps_row(d: &Dev, id: &str) -> Option<(String, bool)> {
    d.store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT version, deleted FROM apps WHERE id=?1",
                rusqlite::params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)),
            )
            .ok())
        })
        .unwrap()
}

fn app_dir(d: &Dev, id: &str) -> PathBuf {
    d.dir.join("apps").join(id)
}

#[tokio::test]
async fn deployed_app_installs_on_paired_device() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let pkg_dir = signed_pkg(&a.dir, &a, MANIFEST);
    let app_id = deploy(&a, &pkg_dir, false);
    eng(&a, &shared).push().await.unwrap();

    let out = eng(&b, &shared).pull().await.unwrap();
    assert!(out.pulled >= 1);

    // Package landed, signature-verified, storage provisioned.
    let reg = AppRegistry::new(&b.dir);
    let pkg = reg.get(&app_id).unwrap().expect("app installed on B");
    assert_eq!(pkg.manifest.app.name, "Synced Notes");
    assert!(app_dir(&b, &app_id).join("signature.bin").is_file());
    assert!(app_dir(&b, &app_id).join("files/seed.txt").is_file());
    assert!(app_dir(&b, &app_id).join("data").is_dir());
    assert_eq!(apps_row(&b, &app_id), Some(("1.0.0".into(), false)));
}

#[tokio::test]
async fn tampered_package_is_rejected() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    let pkg_dir = signed_pkg(&a.dir, &a, MANIFEST);
    let app_id = deploy(&a, &pkg_dir, false);

    // Tamper with the installed copy after signing — the payload ships
    // the modified bytes with the stale signature; B must reject it.
    std::fs::write(app_dir(&a, &app_id).join("app.wasm"), b"tampered").unwrap();
    eng(&a, &shared).push().await.unwrap();

    let out = eng(&b, &shared).pull().await.unwrap();
    assert!(out.pulled >= 1); // object was pulled + mirrored…
    assert!(AppRegistry::new(&b.dir).get(&app_id).unwrap().is_none()); // …but rejected
    assert!(apps_row(&b, &app_id).is_none());
    assert!(!app_dir(&b, &app_id).join(".staging-marker").exists());
    // staging dir cleaned up
    let leftovers: Vec<_> = std::fs::read_dir(b.dir.join("apps"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(".staging"))
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn app_signed_by_unpaired_device_is_rejected() {
    // E signs a package; A plants + pushes it (as if it arrived by some
    // other channel); B has never heard of E and must refuse the install.
    let (a, b, e) = (dev("a"), dev("b"), dev("e"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);

    let pkg_dir = signed_pkg(&a.dir, &e, MANIFEST); // signed by E's key
    let pkg = AppPackage::load(&pkg_dir).unwrap();
    AppRegistry::new(&a.dir)
        .install_trusted(&pkg, false)
        .unwrap();
    upsert_app_row(&a, &pkg, None);
    eng(&a, &shared).push().await.unwrap();

    eng(&b, &shared).pull().await.unwrap();
    let id = pkg.manifest.app_id();
    assert!(AppRegistry::new(&b.dir).get(&id).unwrap().is_none());
    assert!(apps_row(&b, &id).is_none());
}

#[tokio::test]
async fn app_removal_propagates_tombstone() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    let pkg_dir = signed_pkg(&a.dir, &a, MANIFEST);
    let app_id = deploy(&a, &pkg_dir, false);
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(AppRegistry::new(&b.dir).get(&app_id).unwrap().is_some());

    // Remove on A (CLI path: dir gone + row tombstoned), push, pull.
    AppRegistry::new(&a.dir).remove(&app_id).unwrap();
    let later = chrono::Utc::now() + chrono::Duration::seconds(60);
    a.store
        .with_conn(|c| {
            c.execute(
                "UPDATE apps SET deleted=1, updated_at=?2 WHERE id=?1",
                rusqlite::params![app_id, pai_storage::ts(&later)],
            )?;
            Ok(())
        })
        .unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    assert!(!app_dir(&b, &app_id).exists());
    assert_eq!(apps_row(&b, &app_id).map(|(_, d)| d), Some(true));
}

#[tokio::test]
async fn upgrade_sync_preserves_remote_data() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    let pkg_dir = signed_pkg(&a.dir, &a, MANIFEST);
    let app_id = deploy(&a, &pkg_dir, false);
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    // B writes runtime state under data/ — must survive the v2 sync.
    std::fs::write(app_dir(&b, &app_id).join("data/mine.txt"), b"keep me").unwrap();

    // A ships v2 — a strictly-later updated_at so LWW accepts it.
    let pkg2 = signed_pkg(&a.dir, &a, MANIFEST_V2);
    let pkg = AppPackage::load(&pkg2).unwrap();
    let devices = a.ids.list_devices(a.user.id).unwrap();
    let signer = pkg.verify_any(&a.ids, &devices).unwrap();
    let dev = devices.iter().find(|x| x.id == signer).unwrap();
    AppRegistry::new(&a.dir)
        .install(&pkg, &a.ids, dev, true)
        .unwrap();
    let later = chrono::Utc::now() + chrono::Duration::seconds(60);
    upsert_app_row(&a, &pkg, Some(pai_storage::ts(&later)));
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();

    let pkg_b = AppRegistry::new(&b.dir).get(&app_id).unwrap().unwrap();
    assert_eq!(pkg_b.manifest.app.version, "2.0.0");
    assert_eq!(
        std::fs::read(app_dir(&b, &app_id).join("data/mine.txt")).unwrap(),
        b"keep me"
    );
    assert_eq!(apps_row(&b, &app_id), Some(("2.0.0".into(), false)));
}

#[tokio::test]
async fn data_dir_state_never_syncs() {
    // data/ is runtime state: writing to it must not change what the
    // package pushes (content digest stays stable) and must not land on B.
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    let pkg_dir = signed_pkg(&a.dir, &a, MANIFEST);
    let app_id = deploy(&a, &pkg_dir, false);

    // Runtime state on A — package content digest must ignore it.
    let pkg_a = AppRegistry::new(&a.dir).get(&app_id).unwrap().unwrap();
    let digest_before = pkg_a.content_digest;
    std::fs::write(app_dir(&a, &app_id).join("data/local.bin"), b"state").unwrap();
    let pkg_a2 = AppRegistry::new(&a.dir).get(&app_id).unwrap().unwrap();
    assert_eq!(digest_before, pkg_a2.content_digest);

    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(app_dir(&b, &app_id).join("data").is_dir()); // provisioned fresh
    assert!(!app_dir(&b, &app_id).join("data/local.bin").exists());
}

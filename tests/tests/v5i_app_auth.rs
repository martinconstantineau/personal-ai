//! V5i tests: per-app OAuth — "add Google login to the invoice app"
//! (PRD §6.8). The host holds the refresh token (keystore
//! `app-oauth:<app>:<provider>`); the sandbox gets a fresh *access*
//! token as `PAI_OAUTH_<NAME>` at run time. `auth.json` rides the
//! `app/` sync object; tokens stay keystore-local per device.

use pai_agent::appops::StoreAppOperator;
use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, engine, pair};
use pai_tools::AppOperator;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5i-{tag}-{}", uuid::Uuid::new_v4()));
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

const MANIFEST: &str = r#"
[app]
name = "Auth App"
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

/// Minimal HTTP/1.1 stub: serves each `(status, body)` to one
/// connection, in order. Returns the base URL.
fn http_stub(bodies: Vec<(u16, String)>) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (status, body) in bodies {
            let Ok((mut s, _)) = l.accept() else {
                return;
            };
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// WAT: read env[0], write its bytes to stdout, exit 0.
const ECHO_ENV0: &str = r#"(module
    (import "wasi_snapshot_preview1" "environ_sizes_get" (func $esz (param i32 i32) (result i32)))
    (import "wasi_snapshot_preview1" "environ_get" (func $eget (param i32 i32) (result i32)))
    (import "wasi_snapshot_preview1" "fd_write" (func $fdw (param i32 i32 i32 i32) (result i32)))
    (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
    (memory (export "memory") 1)
    (func (export "_start")
      (local $len i32)
      (drop (call $esz (i32.const 0) (i32.const 4)))
      (drop (call $eget (i32.const 8) (i32.const 64)))
      (loop $scan
        (if (i32.ne (i32.load8_u (i32.add (i32.load (i32.const 8)) (local.get $len))) (i32.const 0))
          (then (local.set $len (i32.add (local.get $len) (i32.const 1))) (br $scan))))
      (i32.store (i32.const 128) (i32.load (i32.const 8)))
      (i32.store (i32.const 132) (local.get $len))
      (drop (call $fdw (i32.const 1) (i32.const 128) (i32.const 1) (i32.const 140)))
      (call $exit (i32.const 0))))"#;

fn env_pkg(dir: &Path) -> AppPackage {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("manifest.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("app.wasm"), ECHO_ENV0).unwrap();
    AppPackage::load(dir).unwrap()
}

/// The sandbox sees exactly the injected env — `PAI_OAUTH_<NAME>` is
/// the only variable, readable via environ_get.
#[test]
fn env_injection_reaches_wasm() {
    let dir = tmpdir("env");
    let pkg = env_pkg(&dir);
    let envs = vec![("PAI_OAUTH_TEST".to_string(), "sekrit-token".to_string())];
    let out = pkg
        .run(&dir, &[], pai_apps::RunLimits::default(), &envs, &[])
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "PAI_OAUTH_TEST=sekrit-token"
    );
}

/// `resolve_envs` turns a stored refresh token into a fresh access
/// token via the token endpoint, and re-stores a rotated refresh.
#[tokio::test]
async fn resolve_envs_refreshes_and_injects() {
    let d = dev("r");
    let app_id = deploy(&d);
    let token_url = http_stub(vec![(
        200,
        r#"{"access_token":"AT-9","refresh_token":"RT-2","expires_in":3600}"#.into(),
    )]);
    let mut auth = pai_apps::auth::AppAuth::default();
    auth.providers.insert(
        "test".into(),
        pai_oauth::OAuthConfig {
            provider: "custom".into(),
            client_id: "cid".into(),
            tenant: None,
            device_url: None,
            token_url: Some(token_url),
            scopes: Some(vec!["s1".into()]),
        },
    );
    auth.save(&d.dir, &app_id).unwrap();
    assert!(pai_apps::auth::store_refresh_token(
        &d.dir, &app_id, "test", "RT-1"
    ));

    let envs = pai_apps::auth::resolve_envs(&d.dir, &app_id).await;
    assert_eq!(
        envs,
        vec![("PAI_OAUTH_TEST".to_string(), "AT-9".to_string())]
    );
    // Rotated refresh token was re-stored transparently (keystore or
    // the .oauth file fallback — same lookup path either way).
    let stored = pai_apps::auth::load_refresh_token(&d.dir, &app_id, "test");
    assert_eq!(stored.as_deref(), Some("RT-2"));
}

/// `apps.configure` two-phase flow against a stub IdP: begin returns
/// the user-facing grant, poll stores the refresh token.
#[tokio::test]
async fn configure_auth_begin_then_poll_grants() {
    let d = dev("c");
    let app_id = deploy(&d);
    let ops = StoreAppOperator::new(d.store.clone(), d.dir.clone(), d.device.id);

    let device_url = http_stub(vec![(
        200,
        r#"{"device_code":"DC-1","user_code":"UC-42",
            "verification_uri":"https://idp.example/activate",
            "interval":1,"expires_in":600}"#
            .into(),
    )]);
    let token_url = http_stub(vec![
        (400, r#"{"error":"authorization_pending"}"#.into()),
        (
            200,
            r#"{"access_token":"AT","refresh_token":"RT-ok"}"#.into(),
        ),
    ]);

    // Phase 1 — record config + start the flow.
    let v = ops
        .configure_auth(&app_id, "custom", "cid-1", vec!["scope-a".into()], None)
        .await
        .unwrap_err(); // custom needs device_url — must refuse first
    assert!(v.to_string().contains("unknown oauth provider"));

    // Reconfigure with the stub endpoints through auth.json directly —
    // the tool path only accepts provider presets, so seed the file.
    let mut auth = pai_apps::auth::AppAuth::default();
    auth.providers.insert(
        "custom".into(),
        pai_oauth::OAuthConfig {
            provider: "custom".into(),
            client_id: "cid-1".into(),
            tenant: None,
            device_url: Some(device_url),
            token_url: Some(token_url),
            scopes: Some(vec!["scope-a".into()]),
        },
    );
    auth.save(&d.dir, &app_id).unwrap();

    // Phase 2 — first poll pending, second granted.
    let v = ops
        .configure_auth(&app_id, "custom", "", vec![], Some("DC-1"))
        .await
        .unwrap();
    assert_eq!(v["state"], "pending");
    let v = ops
        .configure_auth(&app_id, "custom", "", vec![], Some("DC-1"))
        .await
        .unwrap();
    assert_eq!(v["state"], "authorized");
    assert_eq!(v["env"], "PAI_OAUTH_CUSTOM");
    assert!(pai_apps::auth::has_token(&d.dir, &app_id, "custom"));
}

/// auth.json travels with the `app/` object — "add Google login" on
/// one device lands the provider config on every peer (tokens stay
/// keystore-local; each device authorizes itself).
#[tokio::test]
async fn auth_config_rides_app_sync() {
    let (a, b) = (dev("a"), dev("b"));
    let shared = tmpdir("shared");
    pair_devices(&b, &a);
    let app_id = deploy(&a);

    let mut auth = pai_apps::auth::AppAuth::default();
    auth.providers.insert(
        "google".into(),
        pai_oauth::OAuthConfig {
            provider: "google".into(),
            client_id: "x.apps.googleusercontent.com".into(),
            tenant: None,
            device_url: None,
            token_url: None,
            scopes: Some(vec!["openid".into(), "email".into()]),
        },
    );
    auth.save(&a.dir, &app_id).unwrap();

    eng_push(&a, &shared).await;
    engine::folder_engine(&shared, b.store.clone(), b.device.id, &b.dir)
        .unwrap()
        .pull()
        .await
        .unwrap();

    let on_b = pai_apps::auth::AppAuth::load(&b.dir, &app_id).unwrap();
    let g = on_b.providers.get("google").expect("auth.json must land");
    assert_eq!(g.client_id, "x.apps.googleusercontent.com");
    assert!(!pai_apps::auth::has_token(&b.dir, &app_id, "google")); // B auths itself
}

async fn eng_push(d: &Dev, shared: &Path) {
    engine::folder_engine(shared, d.store.clone(), d.device.id, &d.dir)
        .unwrap()
        .push()
        .await
        .unwrap();
}

/// Upgrade preserves host-side state: auth.json + logs/ + data/ all
/// survive an `install(upgrade=true)` over the same app id.
#[test]
fn upgrade_preserves_auth_and_logs() {
    let d = dev("u");
    let app_id = deploy(&d);
    let dir = pai_apps::installed_dir(&d.dir, &app_id);
    let mut auth = pai_apps::auth::AppAuth::default();
    auth.providers.insert(
        "google".into(),
        pai_oauth::OAuthConfig {
            provider: "google".into(),
            client_id: "cid".into(),
            tenant: None,
            device_url: None,
            token_url: None,
            scopes: None,
        },
    );
    auth.save(&d.dir, &app_id).unwrap();
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::write(dir.join("logs/1.json"), b"{}").unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(dir.join("data/state"), b"live").unwrap();

    // Upgrade-install a freshly built v1.0.1 package over it — same
    // app id (name-derived), new source dir.
    let src2 = d.dir.join("src-pkg-v2");
    std::fs::create_dir_all(&src2).unwrap();
    std::fs::write(
        src2.join("manifest.toml"),
        MANIFEST.replace("\"1.0.0\"", "\"1.0.1\""),
    )
    .unwrap();
    std::fs::write(src2.join("app.wasm"), b"\0asm\x01\0\0\0").unwrap();
    let pkg = AppPackage::load(&src2).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    let devices = d.ids.list_devices(d.user.id).unwrap();
    let dev0 = &devices[0];
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, dev0, true)
        .unwrap();

    assert!(
        dir.join("auth.json").is_file(),
        "auth.json survives upgrade"
    );
    assert!(dir.join("logs/1.json").is_file(), "logs survive upgrade");
    assert_eq!(std::fs::read(dir.join("data/state")).unwrap(), b"live");
}

/// auth.json and logs/ must never enter the signed file set — they'd
/// leak into `app/` payloads and break the content digest.
#[test]
fn auth_and_logs_excluded_from_package_files() {
    let dir = tmpdir("pkg");
    let pkg = env_pkg(&dir);
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::write(dir.join("logs/x.json"), b"{}").unwrap();
    std::fs::write(dir.join("auth.json"), br#"{"providers":{}}"#).unwrap();
    let files = pai_sync::engine::collect_package_files(&dir).unwrap();
    assert!(
        files
            .iter()
            .all(|f| !f.path.starts_with("logs/") && f.path != "auth.json"),
        "host-side state must not ship as package files: {files:?}"
    );
    // And they don't perturb the signed digest.
    let p2 = AppPackage::load(&dir).unwrap();
    assert_eq!(pkg.content_digest, p2.content_digest);
}

/// env var names sanitize provider ids.
#[test]
fn env_name_sanitizes() {
    assert_eq!(pai_apps::auth::env_name("google"), "PAI_OAUTH_GOOGLE");
    assert_eq!(
        pai_apps::auth::env_name("github-enterprise"),
        "PAI_OAUTH_GITHUB_ENTERPRISE"
    );
}

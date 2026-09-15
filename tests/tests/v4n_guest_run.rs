//! V4n tests: capability-shared guest execution — a device that is NOT
//! a vault member runs an app on the host by presenting a capability
//! token minted with `pai apps share`. Requests ride `greq/` objects;
//! responses seal to the request's ephemeral X25519 key.

use pai_apps::{AppPackage, AppRegistry};
use pai_broker::rpc::BrokerServer;
use pai_core::*;
use pai_identity::IdentityStore;
use pai_share::guest::{call_guest, GuestServer};
use pai_share::{Action, GrantSpec, ShareStore};
use pai_storage::Store;
use pai_sync::FolderTransport;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v4n-{tag}-{}", uuid::Uuid::new_v4()));
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

const HELLO_WAT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fdw (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "hello guest\n")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 8))
    (i32.store (i32.const 4) (i32.const 12))
    (drop (call $fdw (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 100)))))"#;

const MANIFEST: &str = r#"
[app]
name = "guest-app"
version = "1.0.0"
runtime = "wasm"
entrypoint = "app.wasm"
"#;

fn install_app(d: &Dev) {
    let src = d.dir.join("pkg-src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("manifest.toml"), MANIFEST).unwrap();
    std::fs::write(src.join("app.wasm"), HELLO_WAT).unwrap();
    let pkg = AppPackage::load(&src).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    let pkg = AppPackage::load(&src).unwrap();
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, &d.device, false)
        .unwrap();
}

/// The host's broker with a guest endpoint — mirrors the CLI's
/// `broker serve` arm: guest requests dispatch to `app_run_op`. The
/// vault key is a dummy: sealed breq/bres ops are never exercised —
/// the guest channel carries its own auth.
static VAULT: [u8; 32] = [7u8; 32];

fn guest_broker<'a>(
    t: &'a FolderTransport,
    host: &'a Dev,
) -> BrokerServer<'a, FolderTransport, StaticHandler> {
    static HANDLER: StaticHandler = StaticHandler;
    let data = host.dir.clone();
    let guests = GuestServer::new(host.device.clone(), host.store.clone(), &host.dir);
    BrokerServer::new(t, &VAULT, host.device.id, &HANDLER).with_guest_handler(
        guests,
        Box::new(move |op, app_id, args| match op {
            "app-run" => {
                pai_apps::app_run_op(&data, app_id, args).map_err(|e| Error::Other(e.to_string()))
            }
            "app-read" => {
                pai_apps::app_read_op(&data, app_id, args).map_err(|e| Error::Other(e.to_string()))
            }
            "app-write" => {
                pai_apps::app_write_op(&data, app_id, args).map_err(|e| Error::Other(e.to_string()))
            }
            other => Err(Error::InvalidInput(format!("unknown guest op '{other}'"))),
        }),
    )
}

struct StaticHandler;

#[async_trait::async_trait]
impl pai_broker::rpc::OpHandler for StaticHandler {
    async fn handle(&self, _op: &str, _payload: &[u8]) -> Result<Vec<u8>> {
        Err(Error::InvalidInput(
            "vault ops unused in guest tests".into(),
        ))
    }
}

#[tokio::test]
async fn guest_run_roundtrip_and_revocation() {
    let host = dev("host");
    let guest = dev("guest"); // never paired — no shared vault
    install_app(&host);

    let shared = tmpdir("bus");
    let transport = FolderTransport::new(shared.clone()).unwrap();

    // Host mints a bearer exec token for the guest.
    let shares = ShareStore::new(&host.dir);
    let cap = shares
        .grant(
            &host.ids,
            &host.key_dir,
            host.device.id,
            GrantSpec::for_app("guest-app", vec![Action::Exec]),
        )
        .unwrap();

    let server = guest_broker(&transport, &host);
    let call = call_guest(
        &transport,
        host.device.id,
        cap.clone(),
        "app-run",
        &[],
        Duration::from_secs(15),
        None,
    );
    let serve = async {
        loop {
            if server.serve_guests_once().await.unwrap() > 0 {
                break;
            }
        }
    };
    let (resp, _) = tokio::join!(call, serve);
    let resp = resp.unwrap();

    use base64::Engine;
    let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
    let stdout = base64::engine::general_purpose::STANDARD
        .decode(v["stdout_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&stdout), "hello guest\n");

    // Revocation refuses the same token on the next request.
    assert!(shares.revoke(&cap.token_id).unwrap());
    let err = call_guest(
        &transport,
        host.device.id,
        cap,
        "app-run",
        &[],
        Duration::from_secs(15),
        None,
    );
    let serve = async {
        loop {
            if server.serve_guests_once().await.unwrap() > 0 {
                break;
            }
        }
    };
    let (res, _) = tokio::join!(err, serve);
    let err = res.unwrap_err().to_string();
    assert!(err.contains("revoked"), "unexpected error: {err}");

    let _ = std::fs::remove_dir_all(&host.dir);
    let _ = std::fs::remove_dir_all(&guest.dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[tokio::test]
async fn guest_run_grantee_bound_needs_signature() {
    let host = dev("host2");
    let guest = dev("guest2");
    install_app(&host);

    let shared = tmpdir("bus2");
    let transport = FolderTransport::new(shared.clone()).unwrap();

    // Bound token: only the guest's device key may present it.
    let shares = ShareStore::new(&host.dir);
    let mut spec = GrantSpec::for_app("guest-app", vec![Action::Exec]);
    spec.grantee_key = Some(guest.device.public_key.as_slice().try_into().unwrap());
    let cap = shares
        .grant(&host.ids, &host.key_dir, host.device.id, spec)
        .unwrap();

    let server = guest_broker(&transport, &host);

    // Unsigned presentation of a bound token is refused client-side.
    let res = call_guest(
        &transport,
        host.device.id,
        cap.clone(),
        "app-run",
        &[],
        Duration::from_secs(5),
        None,
    )
    .await;
    assert!(res.is_err());

    // Signed by the bound guest device: accepted end-to-end.
    let ids = IdentityStore::new(guest.store.clone());
    let kd = guest.key_dir.clone();
    let gid = guest.device.id;
    let signer = move |msg: &[u8]| {
        ids.sign(gid, &kd, msg)
            .map_err(|e| pai_share::ShareError::InvalidInput(e.to_string()))
    };
    let call = call_guest(
        &transport,
        host.device.id,
        cap,
        "app-run",
        &[],
        Duration::from_secs(15),
        Some(&signer),
    );
    let serve = async {
        loop {
            if server.serve_guests_once().await.unwrap() > 0 {
                break;
            }
        }
    };
    let (resp, _) = tokio::join!(call, serve);
    resp.unwrap();

    let _ = std::fs::remove_dir_all(&host.dir);
    let _ = std::fs::remove_dir_all(&guest.dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[tokio::test]
async fn guest_read_write_roundtrip() {
    let host = dev("host3");
    let guest = dev("guest3");
    install_app(&host);
    // Seed a package file so app-read has content under files/.
    std::fs::create_dir_all(host.dir.join("apps/guest-app/files")).unwrap();
    std::fs::write(
        host.dir.join("apps/guest-app/files/seed.txt"),
        b"seed-guest",
    )
    .unwrap();

    let shared = tmpdir("bus3");
    let transport = FolderTransport::new(shared.clone()).unwrap();
    let shares = ShareStore::new(&host.dir);
    let cap = shares
        .grant(
            &host.ids,
            &host.key_dir,
            host.device.id,
            GrantSpec::for_app("guest-app", vec![Action::Read, Action::Write]),
        )
        .unwrap();

    let server = guest_broker(&transport, &host);
    let serve = async {
        for _ in 0..200 {
            server.serve_guests_once().await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    };

    let read_args = vec!["files/seed.txt".to_string()];
    let work = async {
        // read files/seed.txt
        let resp = call_guest(
            &transport,
            host.device.id,
            cap.clone(),
            "app-read",
            &read_args,
            Duration::from_secs(15),
            None,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD
            .decode(v["data_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(data, b"seed-guest");

        // write data/note.txt, then read it back
        let write_args = vec![
            "data/note.txt".to_string(),
            base64::engine::general_purpose::STANDARD.encode(b"from-guest"),
        ];
        call_guest(
            &transport,
            host.device.id,
            cap.clone(),
            "app-write",
            &write_args,
            Duration::from_secs(15),
            None,
        )
        .await
        .unwrap();
        let resp = call_guest(
            &transport,
            host.device.id,
            cap.clone(),
            "app-read",
            &["data/note.txt".to_string()],
            Duration::from_secs(15),
            None,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        let data = base64::engine::general_purpose::STANDARD
            .decode(v["data_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(data, b"from-guest");

        // jail: a traversal attempt is refused
        let err = call_guest(
            &transport,
            host.device.id,
            cap.clone(),
            "app-read",
            &["../secret".to_string()],
            Duration::from_secs(15),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("files/ or data/"), "unexpected: {err}");

        // write into files/ is refused — only data/ is writable
        let err = call_guest(
            &transport,
            host.device.id,
            cap,
            "app-write",
            &[
                "files/x".to_string(),
                base64::engine::general_purpose::STANDARD.encode(b"nope"),
            ],
            Duration::from_secs(15),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("outside"), "unexpected: {err}");
    };
    tokio::join!(serve, work);

    let _ = std::fs::remove_dir_all(&host.dir);
    let _ = std::fs::remove_dir_all(&guest.dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[tokio::test]
async fn guest_run_delegated_token_roundtrip() {
    let host = dev("host4");
    let mid = dev("mid4"); // bound grantee who re-grants
    let sub = dev("sub4"); // end guest — paired with nobody
    install_app(&host);

    let shared = tmpdir("bus4");
    let transport = FolderTransport::new(shared.clone()).unwrap();

    // Host grants mid exec+share, bound to mid's device key.
    let shares = ShareStore::new(&host.dir);
    let mut pspec = GrantSpec::for_app("guest-app", vec![Action::Exec, Action::Share]);
    pspec.grantee_key = Some(mid.device.public_key.as_slice().try_into().unwrap());
    let parent = shares
        .grant(&host.ids, &host.key_dir, host.device.id, pspec)
        .unwrap();

    // Mid delegates an exec-only sub-token bound to sub's key. The
    // child is minted on mid's store — it embeds the parent chain.
    let mid_shares = ShareStore::new(&mid.dir);
    let mut cspec = GrantSpec::for_app("guest-app", vec![Action::Exec]);
    cspec.grantee_key = Some(sub.device.public_key.as_slice().try_into().unwrap());
    let child = mid_shares
        .delegate(&mid.ids, &mid.key_dir, mid.device.id, &parent, cspec)
        .unwrap();
    eprintln!("delegated {}", child.token_id);

    let server = guest_broker(&transport, &host);

    // Sub presents the child: bound, so the request needs sub's sig.
    let ids = IdentityStore::new(sub.store.clone());
    let kd = sub.key_dir.clone();
    let sid = sub.device.id;
    let signer = move |msg: &[u8]| {
        ids.sign(sid, &kd, msg)
            .map_err(|e| pai_share::ShareError::InvalidInput(e.to_string()))
    };
    let work = async {
        let resp = call_guest(
            &transport,
            host.device.id,
            child.clone(),
            "app-run",
            &[],
            Duration::from_secs(15),
            Some(&signer),
        )
        .await
        .unwrap();
        use base64::Engine;
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        let stdout = base64::engine::general_purpose::STANDARD
            .decode(v["stdout_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&stdout), "hello guest\n");
    };
    let serve = async {
        loop {
            if server.serve_guests_once().await.unwrap() > 0 {
                break;
            }
        }
    };
    // Poll `work` first — the serve loop never yields Pending on an
    // empty bus, so it must find the request already pushed.
    tokio::join!(work, serve);

    // Revoking the parent on the host kills the delegated child.
    assert!(shares.revoke(&parent.token_id).unwrap());
    let call = call_guest(
        &transport,
        host.device.id,
        child,
        "app-run",
        &[],
        Duration::from_secs(15),
        Some(&signer),
    );
    let serve = async {
        loop {
            if server.serve_guests_once().await.unwrap() > 0 {
                break;
            }
        }
    };
    let (res, _) = tokio::join!(call, serve);
    let err = res.unwrap_err().to_string();
    assert!(err.contains("revoked"), "unexpected error: {err}");

    let _ = std::fs::remove_dir_all(&host.dir);
    let _ = std::fs::remove_dir_all(&mid.dir);
    let _ = std::fs::remove_dir_all(&sub.dir);
    let _ = std::fs::remove_dir_all(&shared);
}

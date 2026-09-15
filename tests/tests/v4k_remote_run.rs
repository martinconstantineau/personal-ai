//! V4j tests: remote app execution — the `app-run` broker op runs an
//! installed app's wasm entrypoint on a paired device inside the same
//! wasmi sandbox, gated by the placement guard (`active_elsewhere`) so
//! an app only runs on the device it's active on.

use pai_apps::{AppPackage, AppRegistry};
use pai_broker::rpc::{BrokerClient, BrokerServer, OpHandler};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, pair, FolderTransport};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

/// A wasmi-WAT module that writes "hello remote\n" to stdout via
/// wasi fd_write — the smallest app that proves output crosses the
/// broker round-trip.
const HELLO_WAT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fdw (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "hello remote\n")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 8))
    (i32.store (i32.const 4) (i32.const 13))
    (drop (call $fdw (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 100)))))"#;

const MANIFEST: &str = r#"
[app]
name = "remote-app"
version = "1.0.0"
runtime = "wasm"
entrypoint = "app.wasm"
"#;

/// Install a signed wasm package into `d`'s registry (as `pai deploy`).
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

/// Mirrors the CLI's `app-run` arm minus its placement guard (the
/// guard lives in `pai_sync::backup::active_elsewhere`, layered on top
/// of this same op body).
struct AppRunHandler {
    dir: PathBuf,
}

#[async_trait::async_trait]
impl OpHandler for AppRunHandler {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        assert_eq!(op, "app-run");
        #[derive(serde::Deserialize)]
        struct A {
            id: String,
            #[serde(default)]
            args: Vec<String>,
        }
        let a: A =
            serde_json::from_slice(payload).map_err(|e| Error::InvalidInput(e.to_string()))?;
        pai_apps::app_run_op(&self.dir, &a.id, &a.args).map_err(|e| Error::Other(e.to_string()))
    }
}

#[tokio::test]
async fn remote_app_run_roundtrip() {
    let a = dev("caller");
    let b = dev("runner");
    pair_devices(&a, &b);
    install_app(&b);

    let shared = tmpdir("xport");
    let transport = FolderTransport::new(shared.clone()).unwrap();
    let vault = crypto::vault_key(&a.dir).unwrap().unwrap();

    let handler = AppRunHandler { dir: b.dir.clone() };
    let mut server = BrokerServer::new(&transport, &vault, b.device.id, &handler);
    let client = BrokerClient::new(&transport, &vault, a.device.id);

    let payload = serde_json::json!({"id": "remote-app", "args": []})
        .to_string()
        .into_bytes();
    let call = client.call(b.device.id, "app-run", &payload, Duration::from_secs(10));
    let serve = async {
        loop {
            if server.serve_once().await.unwrap() > 0 {
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
    assert_eq!(String::from_utf8_lossy(&stdout), "hello remote\n");
    assert_eq!(v["exit_code"], serde_json::Value::Null);

    let _ = std::fs::remove_dir_all(&a.dir);
    let _ = std::fs::remove_dir_all(&b.dir);
    let _ = std::fs::remove_dir_all(&shared);
}

#[tokio::test]
async fn remote_app_run_unknown_app_errors() {
    let a = dev("caller2");
    let b = dev("runner2");
    pair_devices(&a, &b);

    let shared = tmpdir("xport2");
    let transport = FolderTransport::new(shared.clone()).unwrap();
    let vault = crypto::vault_key(&a.dir).unwrap().unwrap();

    let handler = AppRunHandler { dir: b.dir.clone() };
    let mut server = BrokerServer::new(&transport, &vault, b.device.id, &handler);
    let client = BrokerClient::new(&transport, &vault, a.device.id);

    let payload = serde_json::json!({"id": "missing-app"})
        .to_string()
        .into_bytes();
    let call = client.call(b.device.id, "app-run", &payload, Duration::from_secs(10));
    let serve = async {
        loop {
            if server.serve_once().await.unwrap() > 0 {
                break;
            }
        }
    };
    let (resp, _) = tokio::join!(call, serve);
    let err = resp.unwrap_err().to_string();
    assert!(err.contains("missing-app"), "unexpected error: {err}");

    let _ = std::fs::remove_dir_all(&a.dir);
    let _ = std::fs::remove_dir_all(&b.dir);
    let _ = std::fs::remove_dir_all(&shared);
}

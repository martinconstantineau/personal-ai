//! V5k tests: `pai_fetch` — the sandbox's only network. A host function
//! gated by `[permissions] network = "outbound"` + `allowed_hosts`;
//! HTTPS everywhere except loopback; redirects never auto-followed;
//! every call appended to `logs/fetch.log`.

use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5k-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Dev {
    dir: PathBuf,
    #[allow(dead_code)]
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

/// WAT: calls `pai_fetch` on the JSON request baked in at 512, writes
/// the response buffer to stdout, exits 1 + "FETCH_ERR" on negative.
/// REQJSON is injected (quotes hex-escaped for WAT), REQLEN substituted.
const FETCH_WAT: &str = r#"(module
  (import "env" "pai_fetch" (func $fetch (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fdw (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 2)
  (data (i32.const 512) "REQJSON")
  (data (i32.const 2048) "FETCH_ERR")
  (func $emit (param $p i32) (param $l i32)
    (i32.store (i32.const 256) (local.get $p))
    (i32.store (i32.const 260) (local.get $l))
    (drop (call $fdw (i32.const 1) (i32.const 256) (i32.const 1) (i32.const 268))))
  (func (export "_start")
    (local $n i32)
    (local.set $n (call $fetch
      (i32.const 512) (i32.const REQLEN) (i32.const 8192) (i32.const 60000)))
    (if (i32.lt_s (local.get $n) (i32.const 0))
      (then (call $emit (i32.const 2048) (i32.const 9))
            (call $exit (i32.const 1)))
      (else (call $emit (i32.const 8192) (local.get $n))
            (call $exit (i32.const 0))))))"#;

fn manifest(network: &str, allowed_hosts: &str) -> String {
    format!(
        r#"
[app]
name = "Fetch App"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"

[permissions]
network = "{network}"
{allowed_hosts}
"#
    )
}

/// Install a fetch-capable package; returns app id.
fn install(d: &Dev, manifest: &str, req_json: &str) -> String {
    let src = d.dir.join(format!("pkg-src-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("manifest.toml"), manifest).unwrap();
    let wat = FETCH_WAT
        .replace("REQJSON", &req_json.replace('"', "\\22"))
        .replace("REQLEN", &req_json.len().to_string());
    std::fs::write(src.join("app.wasm"), wat).unwrap();
    let pkg = AppPackage::load(&src).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    let pkg = AppPackage::load(&src).unwrap();
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, &d.device, false)
        .unwrap();
    pkg.manifest.app_id()
}

/// Minimal HTTP/1.1 stub: serves `body` with `status` to one conn.
fn http_stub(status: u16, body: &str) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let body = body.to_string();
    std::thread::spawn(move || {
        let Ok((mut s, _)) = l.accept() else { return };
        use std::io::{Read, Write};
        let mut buf = [0u8; 8192];
        let _ = s.read(&mut buf);
        let resp = format!(
            "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = s.write_all(resp.as_bytes());
    });
    format!("http://127.0.0.1:{port}")
}

fn run_app(d: &Dev, app_id: &str) -> std::result::Result<pai_apps::RunOutput, pai_apps::AppError> {
    pai_apps::app_run_op_guest(&d.dir, app_id, &[]).map(|env| {
        use base64::Engine;
        let v: serde_json::Value = serde_json::from_slice(&env).unwrap();
        pai_apps::RunOutput {
            stdout: base64::engine::general_purpose::STANDARD
                .decode(v["stdout_b64"].as_str().unwrap_or_default())
                .unwrap_or_default(),
            stderr: base64::engine::general_purpose::STANDARD
                .decode(v["stderr_b64"].as_str().unwrap_or_default())
                .unwrap_or_default(),
            exit_code: v["exit_code"].as_i64().map(|c| c as u32),
            fuel_consumed: v["fuel"].as_u64().unwrap_or(0),
        }
    })
}

fn fetch_log(d: &Dev, app_id: &str) -> String {
    std::fs::read_to_string(d.dir.join("apps").join(app_id).join("logs/fetch.log"))
        .unwrap_or_default()
}

#[test]
fn fetch_roundtrip_to_allowed_host() {
    let d = dev("ok");
    let url = http_stub(200, r#"{"hello":"world"}"#);
    let manifest = manifest("outbound", r#"allowed_hosts = ["127.0.0.1"]"#);
    let req = format!(r#"{{"method":"GET","url":"{url}/data"}}"#);
    let app_id = install(&d, &manifest, &req);

    let out = run_app(&d, &app_id).unwrap();
    assert_eq!(
        out.exit_code,
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], 200);
    use base64::Engine;
    let body = base64::engine::general_purpose::STANDARD
        .decode(v["body_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&body), r#"{"hello":"world"}"#);
    assert!(fetch_log(&d, &app_id).contains("→ 200"));
}

#[test]
fn fetch_denied_host_not_allowlisted() {
    let d = dev("deny");
    let url = http_stub(200, "x");
    // outbound granted but 127.0.0.1 isn't in the allowlist.
    let manifest = manifest("outbound", r#"allowed_hosts = ["api.github.com"]"#);
    let req = format!(r#"{{"method":"GET","url":"{url}/"}}"#);
    let app_id = install(&d, &manifest, &req);

    let out = run_app(&d, &app_id).unwrap();
    assert_eq!(out.exit_code, Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "FETCH_ERR");
    assert!(
        fetch_log(&d, &app_id).contains("DENIED"),
        "{}",
        fetch_log(&d, &app_id)
    );
}

#[test]
fn fetch_denied_without_outbound() {
    let d = dev("no-net");
    let url = http_stub(200, "x");
    // Allowlisted host but network = "none".
    let manifest = manifest("none", r#"allowed_hosts = ["127.0.0.1"]"#);
    let req = format!(r#"{{"method":"GET","url":"{url}/"}}"#);
    let app_id = install(&d, &manifest, &req);

    let out = run_app(&d, &app_id).unwrap();
    assert_eq!(out.exit_code, Some(1));
    assert!(fetch_log(&d, &app_id).contains("DENIED"));
}

#[test]
fn fetch_http_only_on_loopback() {
    let d = dev("scheme");
    // example.com IS allowlisted but plain http:// is refused off-loopback.
    let manifest = manifest("outbound", r#"allowed_hosts = ["example.com"]"#);
    let req = r#"{"method":"GET","url":"http://example.com/"}"#.to_string();
    let app_id = install(&d, &manifest, &req);

    let out = run_app(&d, &app_id).unwrap();
    assert_eq!(out.exit_code, Some(1));
    assert!(fetch_log(&d, &app_id).contains("DENIED"));
}

#[test]
fn host_match_rules() {
    use pai_apps::host_match;
    assert!(host_match("api.github.com", "api.github.com"));
    assert!(host_match("api.github.com", "API.GITHUB.COM"));
    assert!(!host_match("api.github.com", "github.com"));
    assert!(host_match("*.googleapis.com", "a.googleapis.com"));
    assert!(host_match("*.googleapis.com", "googleapis.com"));
    assert!(!host_match("*.googleapis.com", "evilgoogleapis.com"));
    assert!(!host_match("*.googleapis.com", "googleapis.com.evil.tld"));
}

#[test]
fn fetch_log_lives_under_logs_not_data() {
    let d = dev("layout");
    let url = http_stub(200, "x");
    let manifest = manifest("outbound", r#"allowed_hosts = ["127.0.0.1"]"#);
    let req = format!(r#"{{"method":"GET","url":"{url}/"}}"#);
    let app_id = install(&d, &manifest, &req);
    let _ = run_app(&d, &app_id).unwrap();

    let app_dir = d.dir.join("apps").join(&app_id);
    assert!(app_dir.join("logs/fetch.log").is_file());
    // The app's own data/ dir stays clean — fetch.log isn't app-visible.
    assert!(!app_dir.join("data/fetch.log").exists());
}

//! V5j tests: stable app URLs — `pai serve` answers `/apps/<id>/<path>`
//! by running the app CGI-style (request → env vars + stdin → CGI
//! response on stdout). Placement-aware: an app active elsewhere is
//! forwarded over the broker `app-serve` op, so the URL survives
//! migration. Served runs are guest-class — no OAuth envs — and the
//! app must opt in with `serve = true` in its manifest.

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
    let d = std::env::temp_dir().join(format!("pai-v5j-{tag}-{}", uuid::Uuid::new_v4()));
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

const MANIFEST: &str = r#"
[app]
name = "Serve App"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"
serve = true
"#;

const MANIFEST_NOSERVE: &str = r#"
[app]
name = "Quiet App"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"
"#;

/// WAT app: emits a CGI response — `Status: 201` + headers — then dumps
/// every env var (one per line) and echoes stdin after `BODY:`.
/// Proves the request's method/path/query/headers arrive as env vars
/// and the request body arrives on stdin.
const CGI_WAT: &str = r#"(module
  (import "wasi_snapshot_preview1" "environ_sizes_get" (func $esz (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get" (func $eget (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fdw (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $fdr (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 2)
  (data (i32.const 512) "Status: 201\0aX-App: test\0aContent-Type: text/plain\0a\0a")
  (data (i32.const 600) "BODY:")
  (data (i32.const 608) "\0a")
  (func $strlen (param $p i32) (result i32)
    (local $l i32)
    (loop $s
      (if (i32.ne (i32.load8_u (i32.add (local.get $p) (local.get $l))) (i32.const 0))
        (then (local.set $l (i32.add (local.get $l) (i32.const 1))) (br $s))))
    (local.get $l))
  (func $emit (param $p i32) (param $l i32)
    (i32.store (i32.const 256) (local.get $p))
    (i32.store (i32.const 260) (local.get $l))
    (drop (call $fdw (i32.const 1) (i32.const 256) (i32.const 1) (i32.const 268))))
  (func (export "_start")
    (local $i i32) (local $n i32)
    ;; CGI response head: 50 bytes at 512.
    (call $emit (i32.const 512) (i32.const 50))
    ;; Dump all env vars: ptr array at 16, strings at 1024+.
    (drop (call $esz (i32.const 0) (i32.const 4)))
    (drop (call $eget (i32.const 16) (i32.const 1024)))
    (local.set $n (i32.load (i32.const 0)))
    (loop $envs
      (if (i32.lt_u (local.get $i) (local.get $n))
        (then
          (call $emit
            (i32.load (i32.add (i32.const 16) (i32.mul (local.get $i) (i32.const 4))))
            (call $strlen
              (i32.load (i32.add (i32.const 16) (i32.mul (local.get $i) (i32.const 4))))))
          (call $emit (i32.const 608) (i32.const 1))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $envs))))
    ;; BODY: + stdin contents.
    (call $emit (i32.const 600) (i32.const 5))
    (i32.store (i32.const 256) (i32.const 8192))
    (i32.store (i32.const 260) (i32.const 4096))
    (drop (call $fdr (i32.const 0) (i32.const 256) (i32.const 1) (i32.const 272)))
    (call $emit (i32.const 8192) (i32.load (i32.const 272)))
    (call $exit (i32.const 0))))"#;

/// Install a signed package (as `pai deploy`), returning the app id.
fn install(d: &Dev, manifest: &str, wat: &str) -> String {
    let src = d.dir.join(format!("pkg-src-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("manifest.toml"), manifest).unwrap();
    std::fs::write(src.join("app.wasm"), wat).unwrap();
    let pkg = AppPackage::load(&src).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    let pkg = AppPackage::load(&src).unwrap();
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, &d.device, false)
        .unwrap();
    pkg.manifest.app_id()
}

fn req(method: &str, path: &str, query: &str, body: &[u8]) -> pai_apps::serve::ServeRequest {
    use base64::Engine;
    pai_apps::serve::ServeRequest {
        method: method.into(),
        path: path.into(),
        query: query.into(),
        headers: vec![
            ("X-Custom-Flag".into(), "yes".into()),
            ("Content-Type".into(), "application/json".into()),
            ("Connection".into(), "keep-alive".into()),
        ],
        body_b64: base64::engine::general_purpose::STANDARD.encode(body),
        base: String::new(),
    }
}

/// Decode the `encode_run` envelope → parsed CGI response.
fn serve_call(
    d: &Dev,
    app_id: &str,
    r: &pai_apps::serve::ServeRequest,
) -> pai_apps::serve::ServeResponse {
    use base64::Engine;
    let env = pai_apps::app_serve_op(&d.dir, app_id, &[r.to_json()]).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&env).unwrap();
    let stdout = base64::engine::general_purpose::STANDARD
        .decode(v["stdout_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        v["exit_code"].as_i64(),
        Some(0),
        "stderr: {}",
        v["stderr_b64"]
    );
    pai_apps::serve::parse_cgi(&stdout)
}

#[test]
fn cgi_envs_map_request() {
    let r = req("POST", "/items/new", "a=1&b=2", b"{}");
    let envs = pai_apps::serve::cgi_envs("my-app", &r, 2);
    let get = |k: &str| {
        envs.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    };
    assert_eq!(get("REQUEST_METHOD"), "POST");
    assert_eq!(get("PATH_INFO"), "/items/new");
    assert_eq!(get("QUERY_STRING"), "a=1&b=2");
    assert_eq!(get("REQUEST_URI"), "/items/new?a=1&b=2");
    assert_eq!(get("CONTENT_TYPE"), "application/json");
    assert_eq!(get("CONTENT_LENGTH"), "2");
    assert_eq!(get("SERVER_NAME"), "my-app.apps.local");
    assert_eq!(get("GATEWAY_INTERFACE"), "CGI/1.1");
    assert_eq!(get("HTTP_X_CUSTOM_FLAG"), "yes");
    // Hop-by-hop and already-mapped headers don't leak as HTTP_*.
    assert_eq!(get("HTTP_CONNECTION"), "");
    assert_eq!(get("HTTP_CONTENT_TYPE"), "");
    assert_eq!(get("HTTP_HOST"), "");
}

#[test]
fn parse_cgi_variants() {
    // Full CGI head.
    let r = pai_apps::serve::parse_cgi(
        b"Status: 404 Not Found\nContent-Type: text/html\nX-A: b\n\n<h1>no</h1>",
    );
    assert_eq!(r.status, 404);
    assert_eq!(r.body, b"<h1>no</h1>");
    assert!(r.headers.iter().any(|(k, v)| k == "X-A" && v == "b"));
    assert!(r.headers.iter().any(|(k, _)| k == "Content-Type"));

    // CRLF separators.
    let r = pai_apps::serve::parse_cgi(b"Content-Type: text/html\r\n\r\nhi");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hi");

    // Bare output — whole thing is the body, default text/plain.
    let r = pai_apps::serve::parse_cgi(b"just some output");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"just some output");
    assert_eq!(
        r.headers
            .iter()
            .find(|(k, _)| k == "Content-Type")
            .unwrap()
            .1,
        "text/plain"
    );
}

#[test]
fn serve_runs_cgi_roundtrip() {
    let d = dev("serve");
    let app_id = install(&d, MANIFEST, CGI_WAT);

    let r = req("POST", "/items/new", "a=1", b"{\"k\":1}");
    let resp = serve_call(&d, &app_id, &r);
    assert_eq!(resp.status, 201);
    assert!(resp
        .headers
        .iter()
        .any(|(k, v)| k == "X-App" && v == "test"));
    let body = String::from_utf8_lossy(&resp.body);
    // The CGI env contract arrived inside the sandbox.
    for needle in [
        "REQUEST_METHOD=POST",
        "PATH_INFO=/items/new",
        "QUERY_STRING=a=1",
        "CONTENT_TYPE=application/json",
        "HTTP_X_CUSTOM_FLAG=yes",
        "GATEWAY_INTERFACE=CGI/1.1",
    ] {
        assert!(body.contains(needle), "missing {needle} in:\n{body}");
    }
    // And stdin carried the request body.
    assert!(body.contains("BODY:{\"k\":1}"), "stdin body: {body}");
}

#[test]
fn serve_requires_manifest_opt_in() {
    let d = dev("gate");
    let app_id = install(&d, MANIFEST_NOSERVE, CGI_WAT);
    let e =
        pai_apps::app_serve_op(&d.dir, &app_id, &[req("GET", "/", "", b"").to_json()]).unwrap_err();
    assert!(e.to_string().contains("does not serve"), "{e}");
}

#[test]
fn serve_gets_no_oauth_envs() {
    let d = dev("oauth");
    let app_id = install(&d, MANIFEST, CGI_WAT);
    // Configure a provider + token as if `pai apps auth` ran — the
    // serve path must NOT inject them (the HTTP caller isn't the owner).
    let mut auth = pai_apps::auth::AppAuth::default();
    auth.providers.insert(
        "test".into(),
        pai_oauth::OAuthConfig {
            provider: "custom".into(),
            client_id: "cid".into(),
            tenant: None,
            device_url: Some("http://127.0.0.1:1/x".into()),
            token_url: Some("http://127.0.0.1:1/t".into()),
            scopes: None,
        },
    );
    auth.save(&d.dir, &app_id).unwrap();
    assert!(pai_apps::auth::store_refresh_token(
        &d.dir, &app_id, "test", "RT"
    ));

    let resp = serve_call(&d, &app_id, &req("GET", "/", "", b""));
    let body = String::from_utf8_lossy(&resp.body);
    assert!(
        !body.contains("PAI_OAUTH"),
        "served run must not see owner tokens:\n{body}"
    );
}

// --- broker forwarding -------------------------------------------------

/// Mirrors the CLI's vault-member `app-serve` arm.
struct ServeHandler {
    dir: PathBuf,
}

#[async_trait::async_trait]
impl OpHandler for ServeHandler {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        assert_eq!(op, "app-serve");
        #[derive(serde::Deserialize)]
        struct A {
            id: String,
            #[serde(default)]
            args: Vec<String>,
        }
        let a: A =
            serde_json::from_slice(payload).map_err(|e| Error::InvalidInput(e.to_string()))?;
        pai_apps::app_serve_op(&self.dir, &a.id, &a.args).map_err(|e| Error::Other(e.to_string()))
    }
}

#[tokio::test]
async fn remote_app_serve_roundtrip() {
    let a = dev("caller");
    let b = dev("runner");
    pair_devices(&a, &b);
    let app_id = install(&b, MANIFEST, CGI_WAT);

    let shared = tmpdir("xport");
    let transport = FolderTransport::new(shared.clone()).unwrap();
    let vault = crypto::vault_key(&a.dir).unwrap().unwrap();

    let handler = ServeHandler { dir: b.dir.clone() };
    let mut server = BrokerServer::new(&transport, &vault, b.device.id, &handler);
    let client = BrokerClient::new(&transport, &vault, a.device.id);

    let r = req("GET", "/hello", "x=1", b"");
    let payload = serde_json::json!({"id": app_id, "args": [r.to_json()]})
        .to_string()
        .into_bytes();
    let call = client.call(b.device.id, "app-serve", &payload, Duration::from_secs(10));
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
    let parsed = pai_apps::serve::parse_cgi(&stdout);
    assert_eq!(parsed.status, 201);
    let body = String::from_utf8_lossy(&parsed.body);
    assert!(body.contains("REQUEST_METHOD=GET"), "{body}");
    assert!(body.contains("PATH_INFO=/hello"), "{body}");

    let _ = std::fs::remove_dir_all(&a.dir);
    let _ = std::fs::remove_dir_all(&b.dir);
    let _ = std::fs::remove_dir_all(&shared);
}

// --- real HTTP round-trip ------------------------------------------------

/// Spin a tiny_http server whose handler does exactly what
/// `serve_request` does for a local app: build a ServeRequest from the
/// HTTP request, call `app_serve_op`, parse stdout as CGI, respond.
fn gateway(data_dir: PathBuf) -> String {
    let http = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = http.server_addr().to_string();
    std::thread::spawn(move || {
        use base64::Engine;
        for mut req in http.incoming_requests() {
            let url = req.url().to_string();
            let (path, query) = match url.split_once('?') {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (url, String::new()),
            };
            let resp: tiny_http::Response<std::io::Cursor<Vec<u8>>> = (|| {
                let Some(rest) = path.strip_prefix("/apps/") else {
                    return tiny_http::Response::from_data(b"not found".to_vec())
                        .with_status_code(404);
                };
                let (id, sub) = match rest.split_once('/') {
                    Some((a, b)) => (a.to_string(), format!("/{b}")),
                    None => (rest.to_string(), "/".to_string()),
                };
                let mut body = Vec::new();
                let _ = req.as_reader().read_to_end(&mut body);
                let sreq = pai_apps::serve::ServeRequest {
                    method: req.method().as_str().into(),
                    path: sub,
                    query,
                    headers: req
                        .headers()
                        .iter()
                        .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
                        .collect(),
                    body_b64: base64::engine::general_purpose::STANDARD.encode(&body),
                    base: format!("/apps/{id}"),
                };
                match pai_apps::app_serve_op(&data_dir, &id, &[sreq.to_json()]) {
                    Ok(env) => {
                        let v: serde_json::Value = serde_json::from_slice(&env).unwrap_or_default();
                        let stdout = v["stdout_b64"]
                            .as_str()
                            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                            .unwrap_or_default();
                        let parsed = pai_apps::serve::parse_cgi(&stdout);
                        let mut r = tiny_http::Response::from_data(parsed.body)
                            .with_status_code(parsed.status);
                        for (k, val) in parsed.headers {
                            if let Ok(h) = tiny_http::Header::from_bytes(k.as_str(), val.as_str()) {
                                r.add_header(h);
                            }
                        }
                        r
                    }
                    Err(e) => tiny_http::Response::from_data(e.to_string().into_bytes())
                        .with_status_code(502),
                }
            })();
            let _ = req.respond(resp);
        }
    });
    format!("http://{addr}")
}

fn http_get(url: &str) -> (u16, Vec<(String, String)>, String) {
    use std::io::{Read, Write};
    let host = url.strip_prefix("http://").unwrap();
    let (host, path) = host.split_once('/').unwrap();
    let mut s = std::net::TcpStream::connect(host).unwrap();
    write!(
        s,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nX-Custom-Flag: yes\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    (status, headers, body.to_string())
}

#[test]
fn gateway_http_roundtrip() {
    let d = dev("gw");
    let app_id = install(&d, MANIFEST, CGI_WAT);
    let base = gateway(d.dir.clone());

    let (status, headers, body) = http_get(&format!("{base}/apps/{app_id}/items?a=1"));
    assert_eq!(status, 201);
    assert!(headers.iter().any(|(k, v)| k == "X-App" && v == "test"));
    assert!(body.contains("REQUEST_METHOD=GET"), "{body}");
    assert!(body.contains("PATH_INFO=/items"), "{body}");
    assert!(body.contains("QUERY_STRING=a=1"), "{body}");
    assert!(body.contains("HTTP_X_CUSTOM_FLAG=yes"), "{body}");

    // Unknown app → error status, not a hang.
    let (status, _, _) = http_get(&format!("{base}/apps/ghost-app/"));
    assert_eq!(status, 502);
}

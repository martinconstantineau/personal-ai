//! `pai serve` — stable app URLs (V5j): CGI-style HTTP gateway plus the
//! `/api/bridge` endpoint the Flutter web build talks to.

use crate::ctx::Base;
use crate::util::*;
use crate::Cli;
use pai_core::*;
use std::sync::{Arc, Mutex};

/// `pai serve` flag bundle — keeps `run` under the arg-count lint.
pub(crate) struct ServeOpts<'a> {
    pub bind: &'a str,
    pub dir: &'a Option<String>,
    pub relay: &'a Option<String>,
    pub token: &'a Option<String>,
    pub bridge: &'a bool,
    pub bridge_token: &'a Option<String>,
}

/// `pai serve` — bind the gateway; `--bridge` also hosts a `PaiRuntime`
/// for the web PWA.
pub(crate) async fn run(opts: &ServeOpts<'_>, b: &Base, cli: &Cli) -> Result<()> {
    let (bind, dir, relay, token, bridge, bridge_token) = (
        opts.bind,
        opts.dir,
        opts.relay,
        opts.token,
        opts.bridge,
        opts.bridge_token,
    );
    let cfg = b.cfg.clone();
    let store = b.store.clone();
    let user = b.user.clone();
    let device = b.device.clone();
    let http = tiny_http::Server::http(bind)
        .map_err(|e| Error::Other(format!("serve bind {bind}: {e}")))?;
    let user_slug = name_slug(&user.display_name);
    println!("serving apps on http://{bind}/apps/<app-id>/<path> — ctrl-c to stop");
    println!(
        "name layer: http://<app>.{user_slug}.devices[:port] — \
                 `pai apps names` emits hosts-file lines"
    );
    // The Flutter web build (PWA) drives the same runtime the
    // native app does — hosted here behind POST /api/bridge.
    let bridge_state = if *bridge {
        let loopback = bind
            .split(':')
            .next()
            .is_some_and(|h| h == "127.0.0.1" || h == "localhost" || h == "::1");
        if !loopback && bridge_token.is_none() {
            return Err(Error::InvalidInput(
                "--bridge on a non-loopback bind requires --bridge-token".into(),
            ));
        }
        let init = serde_json::json!({
            "data_dir": cfg.data_dir,
            "provider": cli.provider,
            "model": cli.model,
            "server_url": cli.server_url,
        })
        .to_string();
        // PaiRuntime::new block_on's its own tokio runtime — that
        // panics on a thread already driving one, so init happens
        // on a dedicated OS thread and hands the handle back.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<std::result::Result<usize, String>>();
        std::thread::spawn(move || {
            let r = unsafe { pai_ffi::bridge::bridge_init(&init) }
                .map(|h| h as usize)
                .map_err(|e| e.to_string());
            let _ = init_tx.send(r);
        });
        match init_rx
            .recv()
            .unwrap_or_else(|e| Err(format!("init thread: {e}")))
            .map(|h| h as *mut pai_ffi::PaiRuntime)
        {
            Ok(handle) => {
                let events = Arc::new(Mutex::new(Vec::new()));
                unsafe {
                    pai_ffi::bridge::bridge_install_event_sink(handle, events.clone());
                }
                println!("bridge: POST /api/bridge live — web PWA can drive this device");
                Some(Arc::new(BridgeState {
                    handle: BridgeHandle(handle),
                    lock: Mutex::new(()),
                    events,
                    token: bridge_token.clone(),
                }))
            }
            Err(e) => {
                eprintln!("bridge: runtime init failed ({e}) — /api/bridge off");
                None
            }
        }
    } else {
        None
    };
    let cx = Arc::new(ServeCtx {
        data_dir: cfg.data_dir.clone(),
        store: store.clone(),
        device: device.clone(),
        dir: dir.clone(),
        relay: relay.clone(),
        token: token.clone(),
        user_slug,
        bridge: bridge_state,
        inflight: std::sync::atomic::AtomicUsize::new(0),
    });
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        // One thread per request — a long `send` run must not
        // block `pollEvents`/`approve` from the same web app.
        for mut req in http.incoming_requests() {
            let cx = cx.clone();
            let rt = rt.clone();
            const MAX_INFLIGHT: usize = 64;
            if cx
                .inflight
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                >= MAX_INFLIGHT
            {
                cx.inflight
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                let _ = req.respond(serve_response(
                    503,
                    "text/plain",
                    b"too many requests\n".to_vec(),
                ));
                continue;
            }
            std::thread::spawn(move || {
                let resp = serve_request(&cx, &rt, &mut req);
                let _ = req.respond(resp);
                cx.inflight
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
    })
    .await
    .map_err(|e| Error::Other(format!("serve loop: {e}")))?;
    Ok(())
}

// --- V5j: stable app URLs — CGI-style HTTP gateway -------------------

struct ServeCtx {
    data_dir: std::path::PathBuf,
    store: Arc<pai_storage::Store>,
    device: Device,
    dir: Option<String>,
    relay: Option<String>,
    token: Option<String>,
    /// Local user's `app.user.devices` slug — Host-header routing only
    /// answers names in this namespace.
    user_slug: String,
    /// POST /api/bridge runtime — present when `--bridge` was passed and
    /// `pai_init` succeeded.
    bridge: Option<Arc<BridgeState>>,
    /// Live request threads — the loop caps spawning so a connection
    /// flood can't exhaust the process (local slowloris/thread bomb).
    inflight: std::sync::atomic::AtomicUsize,
}

/// `*mut PaiRuntime` — Send+Sync because every op either holds
/// `BridgeState::lock` or is whitelisted concurrent by
/// `pai_ffi::bridge::bridge_op_concurrent` (it only touches the
/// runtime's own internal locks).
struct BridgeHandle(*mut pai_ffi::PaiRuntime);
unsafe impl Send for BridgeHandle {}
unsafe impl Sync for BridgeHandle {}

struct BridgeState {
    handle: BridgeHandle,
    /// Serializes non-concurrent ops — the runtime uses `&mut self`
    /// semantics on most calls.
    lock: Mutex<()>,
    /// Live event queue — the FFI event callback pushes here and the
    /// web client's `pollEvents` op drains it (approvals, run progress).
    events: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Required bearer token on bridge calls when set (non-loopback).
    token: Option<String>,
}

/// POST /api/bridge — `{op, arg?}` → the same call the native bridge
/// would make through dart:ffi. Answers the Flutter *web* build.
fn serve_bridge(
    cx: &ServeCtx,
    req: &mut tiny_http::Request,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    use std::io::Read as _;
    let Some(b) = &cx.bridge else {
        return serve_response(404, "text/plain", b"not found\n".to_vec());
    };
    if !req.method().as_str().eq_ignore_ascii_case("post") {
        return serve_response(405, "text/plain", b"POST only\n".to_vec());
    }
    // CSRF guard: cross-origin pages can only send CORS-safelisted
    // content types without a preflight (OPTIONS already 405s), so
    // requiring JSON makes every real call preflight — a drive-by
    // `text/plain` POST can never fire an op.
    let json_ct = req.headers().iter().any(|h| {
        h.field
            .as_str()
            .as_str()
            .eq_ignore_ascii_case("content-type")
            && h.value.as_str().starts_with("application/json")
    });
    if !json_ct {
        return serve_response(
            415,
            "text/plain",
            b"content-type must be application/json\n".to_vec(),
        );
    }
    // Loopback binds carry no token, so pin the Host header to loopback
    // or this user's own name-layer: a DNS-rebinding page would arrive
    // with a foreign Host and must not be able to read answers
    // cross-origin. With a token configured the token is the boundary.
    if b.token.is_none() {
        let host = req
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("host"))
            .map(|h| {
                h.value
                    .as_str()
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .to_lowercase()
            })
            .unwrap_or_default();
        let ok = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
            || host == format!("{}.devices", cx.user_slug)
            || host.ends_with(&format!(".{}.devices", cx.user_slug));
        if !ok {
            return serve_response(421, "text/plain", b"host not allowed\n".to_vec());
        }
    }
    if let Some(t) = &b.token {
        let ok = req.headers().iter().any(|h| {
            (h.field
                .as_str()
                .as_str()
                .eq_ignore_ascii_case("x-pai-bridge-token")
                && h.value.as_str() == t.as_str())
                || (h
                    .field
                    .as_str()
                    .as_str()
                    .eq_ignore_ascii_case("authorization")
                    && h.value.as_str() == format!("Bearer {t}").as_str())
        });
        if !ok {
            return serve_response(401, "text/plain", b"bridge token required\n".to_vec());
        }
    }
    let mut body = Vec::new();
    if req
        .as_reader()
        .take(pai_apps::serve::BODY_CAP as u64 + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return serve_response(400, "text/plain", b"bad request body\n".to_vec());
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return serve_response(
            400,
            "text/plain",
            b"body must be JSON {op, arg?}\n".to_vec(),
        );
    };
    let Some(op) = v["op"].as_str() else {
        return serve_response(400, "text/plain", b"missing 'op'\n".to_vec());
    };
    let arg = v["arg"].as_str();
    // The poll drain is the bridge's own queue — never touches the runtime.
    if op == "pollEvents" {
        let evs: Vec<serde_json::Value> = b
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        return serve_response(
            200,
            "application/json",
            serde_json::json!({"events": evs}).to_string().into_bytes(),
        );
    }
    // A panic inside an op unwinds across the FFI boundary and aborts the
    // process — catch it so one bad op can't take the gateway down.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if pai_ffi::bridge::bridge_op_concurrent(op) {
            unsafe { pai_ffi::bridge::bridge_dispatch(b.handle.0, op, arg) }
        } else {
            let _g = b.lock.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { pai_ffi::bridge::bridge_dispatch(b.handle.0, op, arg) }
        }
    }));
    match res {
        Ok(Ok(json)) => serve_response(200, "application/json", json.into_bytes()),
        Ok(Err(e)) => serve_response(
            400,
            "application/json",
            serde_json::json!({"error": e.to_string()})
                .to_string()
                .into_bytes(),
        ),
        Err(_) => serve_response(
            500,
            "application/json",
            serde_json::json!({"error": format!("bridge op '{op}' panicked")})
                .to_string()
                .into_bytes(),
        ),
    }
}

fn serve_response(
    status: u16,
    ct: &str,
    body: Vec<u8>,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_data(body)
        .with_status_code(status)
        .with_header(tiny_http::Header::from_bytes("Content-Type", ct).expect("valid header"))
}

/// One HTTP request → app run → HTTP response. Local apps run in
/// process; apps placed elsewhere are forwarded over the broker
/// `app-serve` op — either way the wire shape is the `encode_run`
/// JSON envelope and stdout is parsed as a CGI response.
fn serve_request(
    cx: &ServeCtx,
    rt: &tokio::runtime::Handle,
    req: &mut tiny_http::Request,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    use base64::Engine as _;
    use std::io::Read as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let url = req.url().to_string();
    let (mut path, query) = match url.split_once('?') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (url, String::new()),
    };
    // `app.user.devices` name layer — a Host like
    // `notes.alice.devices` routes `/x` to `/apps/notes/x`, so names
    // stay valid no matter which device the app is placed on.
    // Gateway API — checked before the name-layer rewrite so a Host like
    // `<app>.<user>.devices` can't shadow it into an app route.
    if path == "/api/bridge" {
        return serve_bridge(cx, req);
    }
    let mut named = false;
    if let Some(app) = req
        .headers()
        .iter()
        .find(|h| h.field.to_string().eq_ignore_ascii_case("host"))
        .and_then(|h| parse_app_name(h.value.as_str(), &cx.user_slug))
    {
        path = format!("/apps/{app}{path}");
        named = true;
    }
    if path == "/" || path == "/apps" || path == "/apps/" {
        // Index: apps that opted into serving.
        let reg = pai_apps::AppRegistry::new(&cx.data_dir);
        let mut items = String::new();
        for (id, m) in reg.list().unwrap_or_default() {
            if m.app.serve {
                let esc = |s: &str| {
                    s.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                };
                items.push_str(&format!(
                    "<li><a href=\"/apps/{id}/\">{}</a> \
                     <small>{id} · v{} · {:?}</small></li>",
                    esc(&m.app.name),
                    esc(&m.app.version),
                    m.app.runtime
                ));
            }
        }
        let html = format!(
            "<!doctype html><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>Personal AI apps</title>\
             <body><h1>apps on this device</h1><ul>{items}</ul>"
        );
        return serve_response(200, "text/html; charset=utf-8", html.into_bytes());
    }
    let Some(rest) = path.strip_prefix("/apps/") else {
        return serve_response(404, "text/plain", b"not found\n".to_vec());
    };
    let (id, sub) = match rest.split_once('/') {
        Some((a, b)) => (a.to_string(), format!("/{b}")),
        None => (rest.to_string(), "/".to_string()),
    };
    if id.is_empty() {
        return serve_response(404, "text/plain", b"not found\n".to_vec());
    }
    let mut body = Vec::new();
    if req
        .as_reader()
        .take(pai_apps::serve::BODY_CAP as u64 + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return serve_response(400, "text/plain", b"bad request body\n".to_vec());
    }
    if body.len() > pai_apps::serve::BODY_CAP {
        return serve_response(413, "text/plain", b"body too large\n".to_vec());
    }
    let sreq = pai_apps::serve::ServeRequest {
        method: req.method().as_str().into(),
        path: sub,
        query,
        headers: req
            .headers()
            .iter()
            .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
            .collect(),
        body_b64: b64.encode(&body),
        // PWA links (manifest/sw/icons) are minted relative to where the
        // browser mounted the app: root on name-layer hosts, the /apps
        // path prefix otherwise.
        base: if named {
            String::new()
        } else {
            format!("/apps/{id}")
        },
    };
    let arg = sreq.to_json();
    // Follow placement: an app active elsewhere is proxied over the
    // broker so the URL is stable across migration.
    let remote = pai_sync::backup::active_elsewhere(&cx.store, cx.device.id, &id)
        .ok()
        .flatten();
    let envelope = if let Some(ref other) = remote {
        let t = match sync_transport(&cx.dir, &cx.relay, &cx.token) {
            Ok(t) => t,
            Err(e) => {
                return serve_response(
                    502,
                    "text/plain",
                    format!("app {id} lives on {other} — transport needed: {e}\n").into_bytes(),
                )
            }
        };
        let vault = match pai_sync::crypto::vault_key(&cx.data_dir) {
            Ok(Some(v)) => v,
            _ => {
                return serve_response(
                    502,
                    "text/plain",
                    "no vault key — cannot reach the app's device\n"
                        .as_bytes()
                        .to_vec(),
                )
            }
        };
        let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, cx.device.id)
            .with_weights(place_weights(&cx.store));
        let payload = serde_json::json!({"id": id, "args": [arg]})
            .to_string()
            .into_bytes();
        let to = match uuid::Uuid::parse_str(other) {
            Ok(u) => DeviceId(u),
            Err(e) => {
                return serve_response(
                    502,
                    "text/plain",
                    format!("bad active_device id '{other}': {e}\n").into_bytes(),
                )
            }
        };
        match rt.block_on(client.call(
            to,
            "app-serve",
            &payload,
            std::time::Duration::from_secs(120),
        )) {
            Ok(r) => r,
            Err(e) => {
                return serve_response(
                    502,
                    "text/plain",
                    format!("remote serve: {e}\n").into_bytes(),
                )
            }
        }
    } else {
        match pai_apps::app_serve_op(&cx.data_dir, &id, &[arg]) {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                let status = if msg.contains("does not serve") {
                    403
                } else if msg.contains("not installed") {
                    404
                } else {
                    502
                };
                return serve_response(status, "text/plain", format!("{msg}\n").into_bytes());
            }
        }
    };
    let v: serde_json::Value = serde_json::from_slice(&envelope).unwrap_or_default();
    let stdout = v["stdout_b64"]
        .as_str()
        .and_then(|s| b64.decode(s).ok())
        .unwrap_or_default();
    let exit = v["exit_code"].as_i64().unwrap_or(-1);
    let served = pai_apps::serve::parse_cgi(&stdout);
    let _ = record_audit(
        &cx.store,
        cx.device.id,
        AuditKind::AppServed,
        serde_json::json!({
            "app_id": id,
            "method": req.method().as_str(),
            "status": served.status,
            "remote": remote.is_some(),
            "exit_code": exit,
        }),
    );
    if exit != 0 {
        let err = v["stderr_b64"]
            .as_str()
            .and_then(|s| b64.decode(s).ok())
            .unwrap_or_default();
        return serve_response(
            502,
            "text/plain",
            format!("app exited {exit}: {}\n", String::from_utf8_lossy(&err)).into_bytes(),
        );
    }
    let mut resp = tiny_http::Response::from_data(served.body).with_status_code(served.status);
    for (k, val) in served.headers {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_str(), val.as_str()) {
            resp.add_header(h);
        }
    }
    resp
}

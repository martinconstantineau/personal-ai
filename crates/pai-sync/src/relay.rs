//! HTTP relay transport — E2EE sync across networks, no shared folder.
//!
//! The relay is a dumb ciphertext store: it never sees keys or plaintext.
//! API (all bodies are the transport `Wire` JSON):
//!
//! ```text
//! GET     /v1/objects           → {"objects": [SyncObjectMeta]}
//! GET     /v1/objects/{key}     → Wire | 404
//! PUT     /v1/objects/{key}     → Wire → 204
//! DELETE  /v1/objects/{key}     → 204 (404s are fine — idempotent GC)
//! ```
//!
//! `Authorization: Bearer <token>` is required only when the server was
//! started with one — anyone who can store blobs can deny service by
//! flooding, so a token is strongly recommended on shared networks.
//! TLS is deliberately out of scope here: objects are already
//! XChaCha20-sealed, and a reverse proxy can add TLS when the relay
//! leaves localhost.

use crate::{urldec, urlenc, FolderTransport, SyncObjectMeta, SyncTransport, Wire};
use async_trait::async_trait;
use pai_core::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

/// Bearer-token validator — `Some` gates every request.
type AuthCheck = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// A bound relay server — call [`serve`] to run the request loop
/// (blocking; spawn it on a thread for tests or run it in `main`).
pub struct RelayServer {
    inner: tiny_http::Server,
    store: Arc<FolderTransport>,
    auth: Option<AuthCheck>,
}

/// Bind a relay storing objects under `dir`. `token: Some(t)` requires
/// `Authorization: Bearer t` on every request.
pub fn bind(dir: PathBuf, addr: &str, token: Option<String>) -> Result<RelayServer> {
    let auth = token.map(|t| Arc::new(move |given: &str| given == t) as AuthCheck);
    bind_auth(dir, addr, auth)
}

/// Bind a relay whose valid bearer set is recomputed per request —
/// LAN mesh mode: tokens are `hex(peer_key)` for each paired peer, so
/// pairing and unpairing take effect without restarting the relay.
pub fn bind_dynamic(
    dir: PathBuf,
    addr: &str,
    tokens: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
) -> Result<RelayServer> {
    bind_auth(
        dir,
        addr,
        Some(Arc::new(move |given: &str| {
            tokens().iter().any(|t| t == given)
        })),
    )
}

/// Largest accepted object payload — sync objects carry base64'd app
/// packages, so this sits above the 4 MiB bridge cap.
const MAX_OBJ_BODY: u64 = 32 * 1024 * 1024;

/// Concurrent requests the relay will service — beyond this, excess
/// connections get a 503 instead of an unbounded thread spawn.
const MAX_REQUESTS: usize = 32;

/// A listener is exposed when its bind address isn't loopback —
/// `0.0.0.0`, a LAN address, or a hostname all count.
fn bind_is_loopback(addr: &str) -> bool {
    if let Ok(a) = addr.parse::<std::net::SocketAddr>() {
        return a.ip().is_loopback();
    }
    addr.split(':')
        .next()
        .map(|h| matches!(h, "localhost" | "127.0.0.1" | "::1"))
        .unwrap_or(false)
}

fn bind_auth(dir: PathBuf, addr: &str, auth: Option<AuthCheck>) -> Result<RelayServer> {
    if auth.is_none() && !bind_is_loopback(addr) {
        eprintln!(
            "WARNING: sync relay on {addr} has NO bearer token — \
             anything that can reach it can store/delete objects"
        );
        tracing::warn!(
            addr,
            "sync relay listening without auth on a non-loopback bind"
        );
    }
    let store = FolderTransport::new(dir)?;
    let inner = tiny_http::Server::http(addr)
        .map_err(|e| Error::Sync(format!("relay bind {addr}: {e}")))?;
    Ok(RelayServer {
        inner,
        store: Arc::new(store),
        auth,
    })
}

impl RelayServer {
    /// "127.0.0.1:PORT" — useful when bound to port 0.
    pub fn addr(&self) -> String {
        self.inner.server_addr().to_string()
    }
}

fn json_response(
    status: u16,
    body: serde_json::Value,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let raw = serde_json::to_vec(&body).unwrap_or_default();
    tiny_http::Response::from_data(raw)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes("Content-Type", "application/json")
                .expect("valid header"),
        )
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    // The server thread is plain std — a minimal block_on suffices for the
    // transport's file I/O (no timers/IO drivers needed).
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut f = std::pin::pin!(f);
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

/// Run the relay loop forever (blocking). Requests are handled on
/// bounded worker threads — one drip-feeding client stalls a slot, not
/// every peer's sync.
pub fn serve(server: RelayServer) -> ! {
    let store = server.store;
    let auth = server.auth;
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for mut req in server.inner.incoming_requests() {
        if in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_REQUESTS {
            in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            let _ = req.respond(json_response(503, serde_json::json!({"error": "busy"})));
            continue;
        }
        let (store, auth, ctr) = (store.clone(), auth.clone(), in_flight.clone());
        std::thread::spawn(move || {
            let resp = handle(&store, auth.as_deref(), &mut req);
            let _ = req.respond(resp);
            ctr.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    unreachable!("incoming_requests never ends")
}

fn handle(
    store: &FolderTransport,
    auth: Option<&(dyn Fn(&str) -> bool + Send + Sync)>,
    req: &mut tiny_http::Request,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    // Auth gate — ciphertext blobs aren't secret-bearing but the relay
    // shouldn't be a free-for-all object store.
    if let Some(check) = auth {
        let ok = req.headers().iter().any(|h| {
            h.field
                .as_str()
                .as_str()
                .eq_ignore_ascii_case("authorization")
                && h.value.as_str().strip_prefix("Bearer ").is_some_and(check)
        });
        if !ok {
            return json_response(401, serde_json::json!({"error": "unauthorized"}));
        }
    }
    let method = req.method().as_str();
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let key_prefix = "/v1/objects/";
    match (method, path.as_str()) {
        ("GET", "/v1/objects") => match block_on(store.list()) {
            Ok(objects) => json_response(200, serde_json::json!({"objects": objects})),
            Err(e) => json_response(500, serde_json::json!({"error": e.to_string()})),
        },
        ("GET", p) if p.starts_with(key_prefix) => {
            let key = urldec(&p[key_prefix.len()..]);
            match block_on(store.pull(&key)) {
                Ok(Some(obj)) => json_response(200, serde_json::json!(Wire { obj })),
                Ok(None) => json_response(404, serde_json::json!({"error": "not found"})),
                Err(e) => json_response(500, serde_json::json!({"error": e.to_string()})),
            }
        }
        ("PUT", p) if p.starts_with(key_prefix) => {
            let key = urldec(&p[key_prefix.len()..]);
            let mut body = String::new();
            let mut rd = std::io::Read::take(req.as_reader(), MAX_OBJ_BODY + 1);
            if std::io::Read::read_to_string(&mut rd, &mut body).is_err() {
                return json_response(400, serde_json::json!({"error": "bad body"}));
            }
            if body.len() as u64 > MAX_OBJ_BODY {
                return json_response(
                    413,
                    serde_json::json!({"error": "object exceeds 32 MiB cap"}),
                );
            }
            let w: Wire = match serde_json::from_str(&body) {
                Ok(w) => w,
                Err(e) => {
                    return json_response(400, serde_json::json!({"error": e.to_string()}));
                }
            };
            if w.obj.key != key {
                return json_response(
                    400,
                    serde_json::json!({"error": "key mismatch between path and body"}),
                );
            }
            match block_on(store.push(&w.obj)) {
                Ok(()) => json_response(204, serde_json::json!({})),
                Err(e) => json_response(500, serde_json::json!({"error": e.to_string()})),
            }
        }
        ("DELETE", p) if p.starts_with(key_prefix) => {
            let key = urldec(&p[key_prefix.len()..]);
            match block_on(store.delete(&key)) {
                Ok(()) => json_response(204, serde_json::json!({})),
                Err(e) => json_response(500, serde_json::json!({"error": e.to_string()})),
            }
        }
        _ => json_response(404, serde_json::json!({"error": "not found"})),
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// `SyncTransport` over a running relay. Same object format as the
/// folder — a relay dir can be browsed/copied as syncobj files.
pub struct RelayTransport {
    base: String,
    token: Option<String>,
    client: reqwest::Client,
    timeout: Duration,
}

impl RelayTransport {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base: base_url.into().trim_end_matches('/').to_string(),
            token: token.or_else(|| std::env::var("PAI_SYNC_TOKEN").ok()),
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(30),
        }
    }

    fn req(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        let r = self.client.request(method, url).timeout(self.timeout);
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }
}

fn http_err(e: impl std::fmt::Display) -> Error {
    Error::Sync(format!("relay: {e}"))
}

#[async_trait]
impl SyncTransport for RelayTransport {
    fn id(&self) -> &'static str {
        "relay"
    }

    async fn list(&self) -> Result<Vec<SyncObjectMeta>> {
        let resp = self
            .req(reqwest::Method::GET, format!("{}/v1/objects", self.base))
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(http_err(format!("HTTP {} on list", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(http_err)?;
        serde_json::from_value(v["objects"].clone()).map_err(http_err)
    }

    async fn push(&self, obj: &SyncObject) -> Result<()> {
        let body = serde_json::to_vec(&Wire { obj: obj.clone() }).map_err(http_err)?;
        let resp = self
            .req(
                reqwest::Method::PUT,
                format!("{}/v1/objects/{}", self.base, urlenc(&obj.key)),
            )
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(http_err(format!(
                "HTTP {} on push {}",
                resp.status(),
                obj.key
            )));
        }
        Ok(())
    }

    async fn pull(&self, key: &str) -> Result<Option<SyncObject>> {
        let resp = self
            .req(
                reqwest::Method::GET,
                format!("{}/v1/objects/{}", self.base, urlenc(key)),
            )
            .send()
            .await
            .map_err(http_err)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(http_err(format!("HTTP {} on pull {key}", resp.status())));
        }
        let w: Wire = resp.json().await.map_err(http_err)?;
        Ok(Some(w.obj))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let resp = self
            .req(
                reqwest::Method::DELETE,
                format!("{}/v1/objects/{}", self.base, urlenc(key)),
            )
            .send()
            .await
            .map_err(http_err)?;
        // 404 = already gone; GC is idempotent.
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(http_err(format!("HTTP {} on delete {key}", resp.status())));
        }
        Ok(())
    }
}

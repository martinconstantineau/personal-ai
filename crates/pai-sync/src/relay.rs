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

/// A bound relay server — call [`serve`] to run the request loop
/// (blocking; spawn it on a thread for tests or run it in `main`).
pub struct RelayServer {
    inner: tiny_http::Server,
    store: Arc<FolderTransport>,
    token: Option<String>,
}

/// Bind a relay storing objects under `dir`. `token: Some(t)` requires
/// `Authorization: Bearer t` on every request.
pub fn bind(dir: PathBuf, addr: &str, token: Option<String>) -> Result<RelayServer> {
    let store = FolderTransport::new(dir)?;
    let inner = tiny_http::Server::http(addr)
        .map_err(|e| Error::Sync(format!("relay bind {addr}: {e}")))?;
    Ok(RelayServer {
        inner,
        store: Arc::new(store),
        token,
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

/// Run the relay loop forever (blocking).
pub fn serve(server: RelayServer) -> ! {
    let store = server.store;
    let token = server.token;
    for mut req in server.inner.incoming_requests() {
        let resp = handle(&store, token.as_deref(), &mut req);
        let _ = req.respond(resp);
    }
    unreachable!("incoming_requests never ends")
}

fn handle(
    store: &FolderTransport,
    token: Option<&str>,
    req: &mut tiny_http::Request,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    // Auth gate — one flat token; ciphertext blobs aren't secret-bearing
    // but the relay shouldn't be a free-for-all object store.
    if let Some(t) = token {
        let want = format!("Bearer {t}");
        let ok = req.headers().iter().any(|h| {
            h.field
                .as_str()
                .as_str()
                .eq_ignore_ascii_case("authorization")
                && h.value.as_str() == want.as_str()
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
            if std::io::Read::read_to_string(req.as_reader(), &mut body).is_err() {
                return json_response(400, serde_json::json!({"error": "bad body"}));
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

//! Stable app URLs (V5j) — CGI-style HTTP serving for WASM apps.
//!
//! WASM apps can't hold sockets (WASI preview1 has none), so the host
//! serves them: an HTTP request becomes CGI 1.1 env vars + stdin body,
//! the app writes a CGI response (`Status:`/`Content-Type:` headers,
//! blank line, body) to stdout. `pai serve` is the gateway; because
//! it follows `active_device` placement, `/apps/<id>/…` resolves on
//! every device — the URL survives migration. This is the addressable
//! half of the PRD's `app.user.devices`; the DNS/Tailscale half is a
//! deployment concern on top.
//!
//! Served runs are guest-class: no `PAI_OAUTH_*` envs — the HTTP
//! caller is not the owner.

use serde::{Deserialize, Serialize};

/// Largest request/response body the gateway will carry (4 MiB).
pub const BODY_CAP: usize = 4 * 1024 * 1024;

/// One HTTP request, serialized over `app-serve` broker ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeRequest {
    pub method: String,
    /// Path below the app's mount point (`/apps/<id>/x` → `/x`).
    pub path: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body_b64: String,
    /// URL prefix the app is mounted under on the answering gateway —
    /// `/apps/<id>` on path-layer URLs, empty on `app.user.devices`
    /// name-layer hosts. Lets generated PWA links work on both.
    #[serde(default)]
    pub base: String,
}

/// What the gateway writes back to the HTTP client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ServeRequest {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

/// Decode the request body (base64 over the wire).
pub fn request_body(req: &ServeRequest) -> AppResult<Vec<u8>> {
    use base64::Engine;
    let body = base64::engine::general_purpose::STANDARD
        .decode(&req.body_b64)
        .map_err(|e| AppError::Layout(format!("serve body b64: {e}")))?;
    if body.len() > BODY_CAP {
        return Err(AppError::Layout(format!(
            "serve body {} bytes exceeds {} cap",
            body.len(),
            BODY_CAP
        )));
    }
    Ok(body)
}

/// CGI 1.1 env vars for one request. Header names become `HTTP_FOO_BAR`
/// (uppercased, `-` → `_`); hop-by-hop headers (Connection et al.) are
/// dropped. `SERVER_NAME` is the stable URL host.
pub fn cgi_envs(app_id: &str, req: &ServeRequest, body_len: usize) -> Vec<(String, String)> {
    let mut envs = vec![
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("SERVER_SOFTWARE".into(), "pai-apps".into()),
        ("SERVER_PROTOCOL".into(), "HTTP/1.1".into()),
        ("SERVER_NAME".into(), format!("{app_id}.apps.local")),
        ("REQUEST_METHOD".into(), req.method.clone()),
        ("REQUEST_URI".into(), {
            let mut u = req.path.clone();
            if !req.query.is_empty() {
                u.push('?');
                u.push_str(&req.query);
            }
            u
        }),
        ("PATH_INFO".into(), req.path.clone()),
        ("QUERY_STRING".into(), req.query.clone()),
        ("CONTENT_LENGTH".into(), body_len.to_string()),
    ];
    if let Some(ct) = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        envs.push(("CONTENT_TYPE".into(), ct.1.clone()));
    }
    for (k, v) in &req.headers {
        let lk = k.to_ascii_lowercase();
        // Hop-by-hop + already-mapped headers don't become HTTP_* vars.
        if matches!(
            lk.as_str(),
            "connection"
                | "keep-alive"
                | "transfer-encoding"
                | "content-length"
                | "content-type"
                | "host"
        ) {
            continue;
        }
        let name: String = k
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        envs.push((format!("HTTP_{name}"), v.clone()));
    }
    envs
}

/// Run the app for one HTTP request — guest-class (no OAuth envs).
/// Gated on `serve = true` in the manifest: the op handler runs this
/// on whichever device hosts the app, so the opt-in travels with the
/// package rather than trusting the gateway.
///
/// `runtime = "web"` packages answer from their static files — no wasm
/// invocation. Either way the response is post-processed for PWA
/// installability: synthesized `manifest.webmanifest`/`sw.js`/icons on
/// a miss, and a manifest link + service-worker registration injected
/// into served HTML.
pub fn run_request(
    data_dir: &std::path::Path,
    app_id: &str,
    req: &ServeRequest,
) -> AppResult<RunOutput> {
    let reg = crate::AppRegistry::new(data_dir);
    let pkg = reg
        .get(app_id)?
        .ok_or_else(|| AppError::Layout(format!("app {app_id} not installed")))?;
    if !pkg.manifest.app.serve {
        return Err(AppError::Layout(format!(
            "app {app_id} does not serve http — set `serve = true` in the manifest"
        )));
    }
    if pkg.manifest.app.runtime == crate::AppRuntime::Web {
        let resp = postprocess(&pkg, req, serve_static(&pkg, req)?);
        return Ok(cgi_run(&resp));
    }
    let body = request_body(req)?;
    let envs = cgi_envs(app_id, req, body.len());
    let mut out = crate::run::run_logged_inner(data_dir, app_id, &[], &envs, &body)?;
    let mut resp = parse_cgi(&out.stdout);
    if resp.status == 404 {
        if let Some(r) = pwa_synth(&pkg, req) {
            out.stdout = cgi_bytes(&r);
            return Ok(out);
        }
    }
    if inject_pwa(&mut resp, req) {
        out.stdout = cgi_bytes(&resp);
    }
    Ok(out)
}

/// Parse a CGI response from captured stdout: headers until the first
/// blank line (`\r\n\r\n` or `\n\n`), then the body. `Status: NNN …`
/// sets the code; a missing Content-Type defaults to text/plain. If
/// there is no header block at all, the entire output is the body —
/// lenient, since vibe-coded apps often just print.
pub fn parse_cgi(stdout: &[u8]) -> ServeResponse {
    let (head, body) = if let Some(i) = find_subslice(stdout, b"\r\n\r\n") {
        (&stdout[..i], &stdout[i + 4..])
    } else if let Some(i) = find_subslice(stdout, b"\n\n") {
        (&stdout[..i], &stdout[i + 2..])
    } else {
        // No header block — whole output is the body.
        return ServeResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: stdout.to_vec(),
        };
    };
    let mut status = 200u16;
    let mut headers = Vec::new();
    let mut saw_ct = false;
    for line in head.split(|&b| b == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("").trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim(), value.trim());
            if name.eq_ignore_ascii_case("status") {
                status = value
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(200);
            } else {
                if name.eq_ignore_ascii_case("content-type") {
                    saw_ct = true;
                }
                headers.push((name.to_string(), value.to_string()));
            }
        }
    }
    if !saw_ct {
        headers.push(("Content-Type".into(), "text/plain".into()));
    }
    ServeResponse {
        status,
        headers,
        body: body.to_vec(),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Static web bundles + PWA scaffolding
// ---------------------------------------------------------------------------

/// Content types for the static-file path — enough of the web platform
/// to serve any bundled PWA.
fn mime_for(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" | "otf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

/// Package subtrees that must never be served: host-side state and
/// signing material. First path component or exact file name.
const SERVE_BLOCKED: &[&str] = &[
    "data",
    "logs",
    "versions",
    "manifest.toml",
    "signature.bin",
    "auth.json",
];

/// Serve one request from a `runtime = "web"` package's files. `/` maps
/// to the entrypoint; a directory tries `<dir>/index.html`; a missing
/// extensionless path falls back to the entrypoint (client-side routing).
/// Returns 404/405 responses as data — callers see a ServeResponse, not
/// an Err, for ordinary web misses.
fn serve_static(pkg: &crate::AppPackage, req: &ServeRequest) -> AppResult<ServeResponse> {
    use std::path::Component;
    let plain = |status: u16, msg: &str| ServeResponse {
        status,
        headers: vec![("Content-Type".into(), "text/plain".into())],
        body: format!("{msg}\n").into_bytes(),
    };
    if !matches!(req.method.as_str(), "GET" | "HEAD") {
        return Ok(plain(405, "method not allowed"));
    }
    let mut rel = req.path.trim_start_matches('/').to_string();
    if rel.is_empty() {
        rel = pkg.manifest.app.entrypoint.clone();
    }
    // Jail: no traversal, no absolute/prefixed paths, no reserved files.
    for c in std::path::Path::new(&rel).components() {
        if !matches!(c, Component::Normal(_)) {
            return Ok(plain(404, "not found"));
        }
    }
    let first = rel.split(['/', '\\']).next().unwrap_or_default();
    if SERVE_BLOCKED.contains(&first) {
        return Ok(plain(404, "not found"));
    }
    let mut file = pkg.dir.join(&rel);
    if file.is_dir() {
        file = file.join("index.html");
    }
    if !file.is_file() {
        // SPA fallback: extensionless paths load the app shell.
        let has_ext = rel
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|s| s.contains('.'));
        if !has_ext && rel != pkg.manifest.app.entrypoint {
            file = pkg.dir.join(&pkg.manifest.app.entrypoint);
        }
    }
    if !file.is_file() {
        return Ok(plain(404, "not found"));
    }
    let body = std::fs::read(&file)?;
    Ok(ServeResponse {
        status: 200,
        headers: vec![("Content-Type".into(), mime_for(&rel).into())],
        body: if req.method == "HEAD" {
            Vec::new()
        } else {
            body
        },
    })
}

/// Synthesized PWA pieces for paths the package didn't provide:
/// `manifest.webmanifest`, `sw.js`, `icon-192.png`, `icon-512.png`.
/// `None` for anything else — caller keeps its own response.
fn pwa_synth(pkg: &crate::AppPackage, req: &ServeRequest) -> Option<ServeResponse> {
    let name = req.path.trim_start_matches('/');
    let base = &req.base;
    let (status, ct, body) = match name {
        "manifest.webmanifest" => (200, "application/manifest+json", pwa_manifest(pkg, base)),
        "sw.js" => (200, "text/javascript; charset=utf-8", pwa_sw(pkg, base)),
        "icon-192.png" => (200, "image/png", pwa_icon(&pkg.manifest.app_id(), 192)),
        "icon-512.png" => (200, "image/png", pwa_icon(&pkg.manifest.app_id(), 512)),
        _ => return None,
    };
    Some(ServeResponse {
        status,
        headers: vec![("Content-Type".into(), ct.into())],
        body,
    })
}

/// Shared tail of [`run_request`]: fill PWA gaps on a 404, inject the
/// manifest link + service-worker registration into HTML responses.
fn postprocess(
    pkg: &crate::AppPackage,
    req: &ServeRequest,
    mut resp: ServeResponse,
) -> ServeResponse {
    if resp.status == 404 {
        if let Some(r) = pwa_synth(pkg, req) {
            return r;
        }
    }
    inject_pwa(&mut resp, req);
    resp
}

/// A web-app manifest synthesized from the package manifest. All URLs
/// hang off `base` so the manifest is correct on both `/apps/<id>/` and
/// `<app>.<user>.devices` mounts.
fn pwa_manifest(pkg: &crate::AppPackage, base: &str) -> Vec<u8> {
    let a = &pkg.manifest.app;
    let short = if a.name.chars().count() > 12 {
        a.name.chars().take(12).collect::<String>()
    } else {
        a.name.clone()
    };
    serde_json::to_vec_pretty(&serde_json::json!({
        "id": format!("{base}/"),
        "name": a.name,
        "short_name": short,
        "start_url": format!("{base}/"),
        "scope": format!("{base}/"),
        "display": "standalone",
        "background_color": "#ffffff",
        "theme_color": format!("#{:06x}", icon_color(&pkg.manifest.app_id())),
        "icons": [
            {"src": format!("{base}/icon-192.png"), "sizes": "192x192", "type": "image/png"},
            {"src": format!("{base}/icon-512.png"), "sizes": "512x512", "type": "image/png"},
        ],
    }))
    .unwrap_or_default()
}

/// A default service worker: precaches the shell, network-first for
/// navigations (fresh online, cached offline), cache-first for other
/// in-scope GETs. Scoped to `base`, so it controls exactly the app.
/// First `n` hex chars of a digest — no hex dep for four bytes.
fn hex_prefix(b: &[u8; 32], n: usize) -> String {
    b.iter().take(n / 2).map(|x| format!("{x:02x}")).collect()
}

fn pwa_sw(pkg: &crate::AppPackage, base: &str) -> Vec<u8> {
    // Content digest in the name: redeploying the same version still
    // invalidates — otherwise a stale SW keeps serving the old bundle.
    let digest = hex_prefix(&pkg.content_digest, 8);
    let cache = format!(
        "pai-{}-v{}-{}",
        pkg.manifest.app_id(),
        pkg.manifest.app.version,
        digest
    );
    let base_json = serde_json::to_string(&format!("{base}/")).unwrap_or_else(|_| "\"/\"".into());
    let cache_json = serde_json::to_string(&cache).unwrap();
    format!(
        r#"// Generated by pai serve — installable-offline default. A package
// file named sw.js at this path takes precedence over this script.
const CACHE = {cache_json};
const SHELL = {base_json};
self.addEventListener('install', e => e.waitUntil(
  caches.open(CACHE).then(c => c.add(SHELL)).then(() => self.skipWaiting())));
self.addEventListener('activate', e => e.waitUntil(
  caches.keys()
    .then(ks => Promise.all(ks.filter(k => k !== CACHE).map(k => caches.delete(k))))
    .then(() => self.clients.claim())));
self.addEventListener('fetch', e => {{
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET' || url.origin !== location.origin) return;
  if (e.request.mode === 'navigate') {{
    e.respondWith(fetch(e.request).then(r => {{
      if (r.ok) {{ const c = r.clone(); caches.open(CACHE).then(k => k.put(SHELL, c)); }}
      return r;
    }}).catch(() => caches.match(SHELL).then(h => h || Response.error())));
    return;
  }}
  if (url.pathname.startsWith(SHELL)) {{
    e.respondWith(caches.match(e.request).then(hit => hit || fetch(e.request).then(r => {{
      if (r.ok) {{ const c = r.clone(); caches.open(CACHE).then(k => k.put(e.request, c)); }}
      return r;
    }})));
  }}
}});
// First-visit assets load before this worker controls the page, so the
// injected page script posts its discovered subresources here to seed
// the cache — a single visit then suffices for full offline use.
self.addEventListener('message', e => {{
  if (e.data && e.data.type === 'pai-precache' && Array.isArray(e.data.urls))
    e.waitUntil(caches.open(CACHE).then(c => Promise.allSettled(
      e.data.urls.filter(u => new URL(u, location).origin === location.origin)
        .map(u => c.add(u)))));
}});
"#
    )
    .into_bytes()
}

/// Insert the PWA glue a page is missing: `<link rel=manifest>` when it
/// declares none, and a `sw.js` service-worker registration when it
/// doesn't register one itself. Only 200 text/html pages are touched.
/// Returns whether the body changed.
fn inject_pwa(resp: &mut ServeResponse, req: &ServeRequest) -> bool {
    if resp.status != 200
        || resp.body.is_empty()
        || !resp
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.contains("text/html"))
    {
        return false;
    }
    let Ok(mut html) = String::from_utf8(resp.body.clone()) else {
        return false;
    };
    // A `<base href="/">` (Flutter web) only resolves under a root mount —
    // rewrite it so the same package also works under `/apps/<id>`.
    let mut changed = false;
    if !req.base.is_empty() {
        for pat in ["<base href=\"/\">", "<base href='/'>"] {
            if html.contains(pat) {
                html = html.replace(pat, &format!("<base href=\"{}/\">", req.base));
                changed = true;
                break;
            }
        }
    }
    let mut snippet = String::new();
    if !html.contains("rel=\"manifest\"") && !html.contains("rel='manifest'") {
        snippet.push_str(&format!(
            "<link rel=\"manifest\" href=\"{}/manifest.webmanifest\">",
            req.base
        ));
    }
    // Note: Flutter ≥3.35 ships `flutter_service_worker.js` as a
    // self-unregistering stub (flutter#156910) — its presence means the
    // page has NO real worker, so it still gets ours.
    if !html.contains("serviceWorker.register") {
        // Seed the SW cache with everything the page has fetched — the
        // DOM list covers declared subresources, the resource-timing
        // buffer catches dynamically-fetched ones (Flutter's main.dart.js
        // / canvaskit.wasm). POST endpoints like /api/bridge are excluded:
        // addAll would GET them and fail wholesale on a non-ok reply.
        // A second pass runs later to catch lazy loads after `ready`.
        snippet.push_str(&format!(
            "<script>addEventListener('load',()=>{{if(!('serviceWorker'in navigator))return;\
             navigator.serviceWorker.register('{base}/sw.js');\
             const seed=()=>{{const dom=[...document.\
             querySelectorAll('link[href],script[src],img[src]')].map(e=>e.href||e.src);\
             const net=(performance.getEntriesByType('resource')||[]).map(e=>e.name);\
             const u=[...new Set([...dom,...net])].filter(u=>!u.includes('/api/'));\
             navigator.serviceWorker.ready.then(r=>r.active&&r.active.postMessage(\
             {{type:'pai-precache',urls:u}}));}};\
             seed();setTimeout(seed,5000);}});</script>",
            base = req.base
        ));
    }
    if snippet.is_empty() {
        if changed {
            resp.body = html.into_bytes();
        }
        return changed;
    }
    if let Some(i) = html.find("</head>") {
        html.insert_str(i, &snippet);
    } else if let Some(i) = html.find("</body>") {
        html.insert_str(i, &snippet);
    } else {
        html.push_str(&snippet);
    }
    resp.body = html.into_bytes();
    true
}

/// Re-encode a ServeResponse as CGI stdout — `Status:` line, headers,
/// blank line, body — so web-runtime answers flow through the same
/// `encode_run` envelope as wasm runs.
fn cgi_bytes(resp: &ServeResponse) -> Vec<u8> {
    let mut out = format!("Status: {}\n", resp.status).into_bytes();
    for (k, v) in &resp.headers {
        out.extend_from_slice(format!("{k}: {v}\n").as_bytes());
    }
    out.push(b'\n');
    out.extend_from_slice(&resp.body);
    out
}

fn cgi_run(resp: &ServeResponse) -> RunOutput {
    RunOutput {
        stdout: cgi_bytes(resp),
        stderr: Vec::new(),
        exit_code: Some(0),
        fuel_consumed: 0,
    }
}

/// Deterministic per-app icon color — a small material-ish palette.
fn icon_color(seed: &str) -> u32 {
    const PALETTE: &[u32] = &[
        0x5c6bc0, 0x26a69a, 0xef5350, 0xab47bc, 0x42a5f5, 0x7e57c2, 0x66bb6a, 0xffa726, 0x8d6e63,
        0x78909c, 0xd4e157, 0x26c6da,
    ];
    let h = seed
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(h as usize) % PALETTE.len()]
}

/// A generated app icon: `size`×`size` RGBA PNG, rounded square in the
/// app's color. Hand-rolled encoder — zlib "stored" deflate blocks, no
/// compression deps.
fn pwa_icon(seed: &str, size: u32) -> Vec<u8> {
    let c = icon_color(seed);
    let (r, g, b) = ((c >> 16) as u8, (c >> 8) as u8, c as u8);
    let rad = size as f32 * 0.2;
    let mut raw = Vec::with_capacity((size * size * 4 + size) as usize);
    for y in 0..size {
        raw.push(0); // filter: none
        for x in 0..size {
            // Inside the rounded rect? Nearest-corner distance test.
            let fx = (x as f32).min((size - 1 - x) as f32);
            let fy = (y as f32).min((size - 1 - y) as f32);
            let inside = if fx < rad && fy < rad {
                let dx = rad - fx;
                let dy = rad - fy;
                dx * dx + dy * dy <= rad * rad
            } else {
                true
            };
            if inside {
                raw.extend_from_slice(&[r, g, b, 255]);
            } else {
                raw.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    png_encode(size, size, &raw)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb88320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// zlib stream of uncompressed (stored) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for (i, chunk) in data.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= data.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_in = kind.to_vec();
    crc_in.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
    out
}

/// Minimal PNG: 8-bit RGBA (`color_type` 6), single IDAT.
fn png_encode(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&png_chunk(b"IDAT", &zlib_stored(rgba)));
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    out
}

use crate::{AppError, AppResult, RunOutput};

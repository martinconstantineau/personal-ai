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
    let body = request_body(req)?;
    let envs = cgi_envs(app_id, req, body.len());
    crate::run::run_logged_inner(data_dir, app_id, &[], &envs, &body)
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

use crate::{AppError, AppResult, RunOutput};

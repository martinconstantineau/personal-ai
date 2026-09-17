//! V5t: installable PWAs — `runtime = "web"` packages serve their files
//! straight off disk through `pai serve`, and every serve-enabled app
//! (web or wasm) gains PWA scaffolding: a synthesized web manifest, a
//! default offline service worker, generated icons, and manifest+SW
//! injection into served HTML — so any app installs to a home screen
//! and keeps working offline.

use pai_apps::{AppPackage, AppRegistry};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use std::path::PathBuf;
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5t-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Dev {
    dir: PathBuf,
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
        ids,
        key_dir,
        device,
    }
}

const WEB_MANIFEST: &str = r#"
[app]
name = "Scratch"
version = "1.0.0"
runtime = "web"
serve = true
"#;

const INDEX: &str = r#"<!doctype html>
<html><head><title>Scratch</title></head>
<body><h1>Scratch</h1><script src="app.js"></script></body></html>
"#;

/// Install a package dir, returning the app id. `files` are
/// (relative path, contents) written alongside manifest.toml.
fn install(d: &Dev, manifest: &str, files: &[(&str, &str)]) -> String {
    let src = d.dir.join(format!("pkg-src-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("manifest.toml"), manifest).unwrap();
    for (rel, body) in files {
        let p = src.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }
    let pkg = AppPackage::load(&src).unwrap();
    pkg.sign(&d.ids, &d.device, &d.key_dir).unwrap();
    let pkg = AppPackage::load(&src).unwrap();
    AppRegistry::new(&d.dir)
        .install(&pkg, &d.ids, &d.device, false)
        .unwrap();
    pkg.manifest.app_id()
}

fn get(d: &Dev, app_id: &str, path: &str) -> pai_apps::serve::ServeResponse {
    serve(d, app_id, "GET", path, "")
}

fn serve(
    d: &Dev,
    app_id: &str,
    method: &str,
    path: &str,
    base: &str,
) -> pai_apps::serve::ServeResponse {
    let req = pai_apps::serve::ServeRequest {
        method: method.into(),
        path: path.into(),
        query: String::new(),
        headers: vec![],
        body_b64: String::new(),
        base: base.into(),
    };
    let env = pai_apps::app_serve_op(&d.dir, app_id, &[req.to_json()]).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&env).unwrap();
    assert_eq!(
        v["exit_code"].as_i64(),
        Some(0),
        "stderr: {}",
        v["stderr_b64"]
    );
    use base64::Engine;
    let stdout = base64::engine::general_purpose::STANDARD
        .decode(v["stdout_b64"].as_str().unwrap())
        .unwrap();
    pai_apps::serve::parse_cgi(&stdout)
}

fn ctype(r: &pai_apps::serve::ServeResponse) -> String {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn text(r: &pai_apps::serve::ServeResponse) -> String {
    String::from_utf8(r.body.clone()).unwrap()
}

#[test]
fn web_app_serves_files_with_mime() {
    let d = dev("files");
    let id = install(
        &d,
        WEB_MANIFEST,
        &[
            ("index.html", INDEX),
            ("app.js", "console.log('hi');\n"),
            ("style.css", "body{color:#111}\n"),
        ],
    );
    assert_eq!(id, "scratch");

    let r = get(&d, &id, "/");
    assert_eq!(r.status, 200);
    assert!(ctype(&r).contains("text/html"), "{}", ctype(&r));
    let html = text(&r);
    assert!(html.contains("<h1>Scratch</h1>"));
    // PWA glue injected — path-layer base.
    assert!(html.contains("manifest.webmanifest"), "{html}");
    assert!(html.contains("serviceWorker.register"), "{html}");

    let r = get(&d, &id, "/app.js");
    assert_eq!(r.status, 200);
    assert!(ctype(&r).contains("javascript"), "{}", ctype(&r));
    assert!(text(&r).contains("console.log"));

    let r = get(&d, &id, "/style.css");
    assert!(ctype(&r).contains("text/css"), "{}", ctype(&r));
}

#[test]
fn web_app_jails_and_blocks_reserved() {
    let d = dev("jail");
    let id = install(
        &d,
        WEB_MANIFEST,
        &[("index.html", INDEX), ("data/secret.txt", "s3cr3t")],
    );
    for p in [
        "/manifest.toml",
        "/signature.bin",
        "/auth.json",
        "/data/secret.txt",
        "/logs/run.log",
        "/../manifest.toml",
        "/..%2fmanifest.toml",
        "/versions/1/index.html",
    ] {
        let r = get(&d, &id, p);
        assert_eq!(r.status, 404, "{p} -> {}", r.status);
    }
}

#[test]
fn web_app_spa_fallback() {
    let d = dev("spa");
    let id = install(&d, WEB_MANIFEST, &[("index.html", INDEX)]);
    // Extensionless path → app shell (client-side routing).
    let r = get(&d, &id, "/notes/42");
    assert_eq!(r.status, 200);
    assert!(text(&r).contains("<h1>Scratch</h1>"));
    // Missing asset stays a real 404.
    let r = get(&d, &id, "/missing.js");
    assert_eq!(r.status, 404);
    // Directory → its index.html.
    let r = get(&d, &id, "/");
    assert_eq!(r.status, 200);
}

#[test]
fn pwa_pieces_synthesize_on_miss() {
    let d = dev("synth");
    let base = "/apps/scratch";
    let id = install(&d, WEB_MANIFEST, &[("index.html", INDEX)]);

    let r = serve(&d, &id, "GET", "/manifest.webmanifest", base);
    assert_eq!(r.status, 200);
    assert!(ctype(&r).contains("manifest"), "{}", ctype(&r));
    let m: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(m["name"], "Scratch");
    assert_eq!(m["display"], "standalone");
    assert_eq!(m["start_url"], "/apps/scratch/");
    assert_eq!(m["scope"], "/apps/scratch/");
    assert_eq!(m["icons"][0]["src"], "/apps/scratch/icon-192.png");
    assert_eq!(m["icons"][1]["src"], "/apps/scratch/icon-512.png");

    // Name-layer host (base="") → root-relative URLs.
    let r = serve(&d, &id, "GET", "/manifest.webmanifest", "");
    let m: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(m["start_url"], "/");

    let r = serve(&d, &id, "GET", "/sw.js", base);
    assert_eq!(r.status, 200);
    assert!(ctype(&r).contains("javascript"), "{}", ctype(&r));
    let sw = text(&r);
    assert!(sw.contains("caches.open"), "{sw}");
    assert!(sw.contains("pai-scratch-v1.0.0"), "{sw}");
    assert!(sw.contains("/apps/scratch/"), "{sw}");

    for (p, w) in [("/icon-192.png", 192u32), ("/icon-512.png", 512)] {
        let r = serve(&d, &id, "GET", p, base);
        assert_eq!(r.status, 200);
        assert_eq!(ctype(&r), "image/png");
        assert_eq!(&r.body[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR width.
        let width = u32::from_be_bytes(r.body[16..20].try_into().unwrap());
        assert_eq!(width, w);
    }
}

#[test]
fn package_pwa_files_win_over_synth() {
    let d = dev("own");
    let id = install(
        &d,
        WEB_MANIFEST,
        &[
            ("index.html", INDEX),
            (
                "manifest.webmanifest",
                r#"{"name":"Mine","display":"fullscreen","icons":[]}"#,
            ),
            ("sw.js", "// custom worker\n"),
        ],
    );
    let r = get(&d, &id, "/manifest.webmanifest");
    let m: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(m["name"], "Mine");
    assert_eq!(m["display"], "fullscreen");
    let r = get(&d, &id, "/sw.js");
    assert_eq!(text(&r), "// custom worker\n");
}

#[test]
fn injection_respects_existing_glue() {
    let d = dev("inject");
    // Page already declares a manifest — only the SW register is added.
    let id = install(
        &d,
        WEB_MANIFEST,
        &[(
            "index.html",
            r#"<!doctype html><html><head>
<link rel="manifest" href="my.webmanifest">
</head><body>x</body></html>"#,
        )],
    );
    let r = get(&d, &id, "/");
    let html = text(&r);
    assert_eq!(html.matches("rel=\"manifest\"").count(), 1, "{html}");
    assert!(html.contains("my.webmanifest"));
    assert!(html.contains("serviceWorker.register"));
}

#[test]
fn head_request_returns_headers_only() {
    let d = dev("head");
    let id = install(&d, WEB_MANIFEST, &[("index.html", INDEX)]);
    let r = serve(&d, &id, "HEAD", "/", "");
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
}

#[test]
fn non_get_methods_refused_for_web() {
    let d = dev("m405");
    let id = install(&d, WEB_MANIFEST, &[("index.html", INDEX)]);
    let r = serve(&d, &id, "POST", "/", "");
    assert_eq!(r.status, 405);
}

#[test]
fn web_runtime_requires_index() {
    let d = dev("noidx");
    let src = d.dir.join("pkg-src-noidx");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("manifest.toml"), WEB_MANIFEST).unwrap();
    // No index.html → load refuses.
    let err = match AppPackage::load(&src) {
        Ok(_) => panic!("load should refuse a web package without index.html"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("index.html"), "{err}");
}

/// Wasm apps get the same PWA scaffolding: a 404 from the CGI response
/// is filled with the synthesized piece.
const CGI_404_WAT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fdw (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "Status: 404\0aContent-Type: text/plain\0a\0anope")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 16))
    (i32.store (i32.const 4) (i32.const 42))
    (drop (call $fdw (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
    (call $exit (i32.const 0))))"#;

const CGI_HTML_WAT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fdw (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "Status: 200\0aContent-Type: text/html\0a\0a<html><head></head><body>hi</body></html>")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 16))
    (i32.store (i32.const 4) (i32.const 78))
    (drop (call $fdw (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
    (call $exit (i32.const 0))))"#;

const WASM_MANIFEST: &str = r#"
[app]
name = "Wasm App"
version = "2.0.0"
entrypoint = "app.wasm"
runtime = "wasm"
serve = true
"#;

#[test]
fn wasm_app_gets_pwa_fallback() {
    let d = dev("wasm");
    let id = install(&d, WASM_MANIFEST, &[("app.wasm", CGI_404_WAT)]);
    assert_eq!(id, "wasm-app");

    // The app 404s everything → synthesized manifest fills the gap.
    let r = serve(&d, &id, "GET", "/manifest.webmanifest", "/apps/wasm-app");
    assert_eq!(r.status, 200);
    let m: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(m["name"], "Wasm App");
    assert_eq!(m["start_url"], "/apps/wasm-app/");

    let r = serve(&d, &id, "GET", "/sw.js", "/apps/wasm-app");
    assert!(text(&r).contains("pai-wasm-app-v2.0.0"));
}

#[test]
fn wasm_html_gets_injected() {
    let d = dev("wasmhtml");
    let id = install(&d, WASM_MANIFEST, &[("app.wasm", CGI_HTML_WAT)]);
    let r = serve(&d, &id, "GET", "/", "/apps/wasm-app");
    assert_eq!(r.status, 200);
    let html = text(&r);
    assert!(
        html.contains("rel=\"manifest\" href=\"/apps/wasm-app/manifest.webmanifest\""),
        "{html}"
    );
    assert!(html.contains("/apps/wasm-app/sw.js"), "{html}");
}

//! V5o: model packs over FFI — `pai_models_list`/`pai_models_scan`
//! surface installed + pack-resident models with online/serving state,
//! and `pai_models_serve` fails cleanly for absent slugs without
//! spawning a process.

use pai_ffi::*;
use std::ffi::{CStr, CString};

fn tmpdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5o-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

unsafe fn init() -> *mut PaiRuntime {
    let cfg = CString::new(
        serde_json::json!({
            "data_dir": tmpdir().to_string_lossy(),
            "provider": "echo",
        })
        .to_string(),
    )
    .unwrap();
    let h = pai_init(cfg.as_ptr());
    assert!(!h.is_null(), "pai_init failed");
    h
}

unsafe fn json(p: *mut std::ffi::c_char) -> serde_json::Value {
    assert!(!p.is_null());
    let v: serde_json::Value = serde_json::from_str(CStr::from_ptr(p).to_str().unwrap()).unwrap();
    pai_free_string(p);
    v
}

#[test]
fn models_list_is_json_array() {
    unsafe {
        let h = init();
        let v = json(pai_models_list(h));
        assert!(v.is_array(), "expected a list, got {v}");
        pai_free(h);
    }
}

#[test]
fn models_scan_then_list() {
    unsafe {
        let h = init();
        let v = json(pai_models_scan(h));
        assert!(v.is_array(), "scan should return the list, got {v}");
        pai_free(h);
    }
}

#[test]
fn catalog_lists_builtin_models() {
    unsafe {
        let h = init();
        let v = json(pai_models_catalog(h));
        let rows = v.as_array().unwrap();
        assert!(!rows.is_empty(), "catalog should not be empty");
        assert!(rows[0]["slug"].as_str().unwrap().len() > 3);
        pai_free(h);
    }
}

#[test]
fn install_unknown_slug_errors() {
    unsafe {
        let h = init();
        let req =
            CString::new(serde_json::json!({"slug": "no-such-model-xyz"}).to_string()).unwrap();
        let v = json(pai_models_install(h, req.as_ptr()));
        assert!(v["error"].as_str().unwrap().contains("no-such-model"));
        pai_free(h);
    }
}

#[test]
fn serve_missing_slug_errors_without_spawning() {
    unsafe {
        let h = init();
        let slug = CString::new("no-such-model-xyz").unwrap();
        let v = json(pai_models_serve(h, slug.as_ptr(), 0));
        assert!(v["error"].as_str().unwrap().contains("not found"));
        // And the provider stays echo — a failed serve must not re-point.
        let st = json(pai_status(h));
        assert_eq!(st["provider"], "echo");
        pai_free(h);
    }
}

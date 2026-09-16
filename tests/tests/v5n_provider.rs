//! V5n: runtime provider status + hot-swap — `pai_status` reports the
//! resolved provider/model, and `pai_set_provider` re-points chat at a
//! different endpoint/model without re-init (audit-logged).

use pai_ffi::*;
use std::ffi::{CStr, CString};

fn tmpdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5n-{}", uuid::Uuid::new_v4()));
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
    let v: serde_json::Value =
        serde_json::from_str(CStr::from_ptr(p).to_str().unwrap()).unwrap();
    pai_free_string(p);
    v
}

#[test]
fn status_reports_resolved_provider() {
    unsafe {
        let h = init();
        let v = json(pai_status(h));
        assert_eq!(v["provider"], "echo");
        assert!(v.get("model").is_some());
        assert!(v["device"].as_str().unwrap().len() > 8);
        pai_free(h);
    }
}

#[test]
fn set_provider_repoints_and_audits() {
    unsafe {
        let h = init();
        let req = CString::new(
            serde_json::json!({
                "server_url": "http://127.0.0.1:9",
                "model": "test-model-7b",
            })
            .to_string(),
        )
        .unwrap();
        let v = json(pai_set_provider(h, req.as_ptr()));
        assert_eq!(v["provider"], "llama-server");
        assert_eq!(v["model"], "test-model-7b");

        // Status reflects the swap.
        let s = json(pai_status(h));
        assert_eq!(s["provider"], "llama-server");
        assert_eq!(s["model"], "test-model-7b");

        // The config change is in the audit trail.
        let a = json(pai_audit(h, 10));
        assert!(
            a.as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "config_changed"),
            "expected a config_changed audit event: {a}"
        );
        pai_free(h);
    }
}

#[test]
fn set_provider_bad_json_errors_cleanly() {
    unsafe {
        let h = init();
        let bad = CString::new("not json").unwrap();
        let v = json(pai_set_provider(h, bad.as_ptr()));
        assert!(v.get("error").is_some());
        pai_free(h);
    }
}

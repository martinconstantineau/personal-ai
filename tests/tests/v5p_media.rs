//! V5p: media jobs over FFI — `pai_media_list` exposes the job log,
//! `pai_media_gen` records a failed job when no audio-gen server is
//! configured, and `pai_media_export` fails cleanly for absent results.

use pai_ffi::*;
use std::ffi::{CStr, CString};

fn tmpdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5p-{}", uuid::Uuid::new_v4()));
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
fn media_list_is_json_array() {
    unsafe {
        let h = init();
        let v = json(pai_media_list(h));
        assert!(v.is_array(), "expected a list, got {v}");
        pai_free(h);
    }
}

#[test]
fn media_gen_without_server_fails_and_records() {
    unsafe {
        let h = init();
        let req = CString::new(
            serde_json::json!({"prompt": "test tone", "duration_seconds": 1})
                .to_string(),
        )
        .unwrap();
        let v = json(pai_media_gen(h, req.as_ptr()));
        // No audio-gen server configured in a temp dir → honest error.
        assert!(v["error"].as_str().unwrap().contains("audio-gen"));
        // …and the failed job is in the log, not silently dropped.
        let jobs = json(pai_media_list(h));
        assert_eq!(jobs.as_array().unwrap().len(), 1);
        assert_eq!(jobs[0]["prompt"], "test tone");
        assert_eq!(jobs[0]["state"], "failed");
        pai_free(h);
    }
}

#[test]
fn media_export_missing_job_errors() {
    unsafe {
        let h = init();
        let id = CString::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let dest = CString::new("out.wav").unwrap();
        let v = json(pai_media_export(h, id.as_ptr(), dest.as_ptr()));
        assert!(v["error"].as_str().unwrap().contains("not found"));
        pai_free(h);
    }
}

#[test]
fn media_gen_rejects_empty_prompt() {
    unsafe {
        let h = init();
        let req = CString::new(
            serde_json::json!({"prompt": "   "}).to_string(),
        )
        .unwrap();
        let v = json(pai_media_gen(h, req.as_ptr()));
        assert!(v["error"].as_str().unwrap().contains("empty prompt"));
        pai_free(h);
    }
}

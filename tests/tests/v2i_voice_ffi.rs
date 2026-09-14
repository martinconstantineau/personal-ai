//! V2i tests: FFI voice ops — `pai_voice_status` always answers (with or
//! without providers), and the provider-backed ops fail cleanly instead
//! of crashing when whisper-server/piper aren't configured.

use pai_ffi::*;
use std::ffi::{CStr, CString};

fn tmpdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2i-{}", uuid::Uuid::new_v4()));
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
fn voice_status_reports_shape() {
    unsafe {
        let h = init();
        let v = json(pai_voice_status(h));
        for k in ["stt", "tts", "mic", "speaker", "whisper_url"] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(v["stt"], false, "no whisper-server on this machine");
        pai_free(h);
    }
}

#[test]
fn voice_say_without_piper_errors_cleanly() {
    unsafe {
        let h = init();
        let text = CString::new("hello").unwrap();
        let v = json(pai_voice_say(h, text.as_ptr()));
        // piper isn't installed here → clean error, not a crash.
        assert!(
            v["error"].as_str().unwrap_or("").contains("piper"),
            "unexpected result: {v}"
        );
        pai_free(h);
    }
}

#[test]
fn voice_transcribe_without_whisper_errors_cleanly() {
    unsafe {
        let h = init();
        let p = CString::new("nonexistent.wav").unwrap();
        let v = json(pai_voice_transcribe(h, p.as_ptr()));
        assert!(
            v["error"].as_str().unwrap_or("").contains("whisper"),
            "unexpected result: {v}"
        );
        pai_free(h);
    }
}

//! V5q: device pairing over FFI — the three-step file exchange between
//! two runtimes installs the shared vault key on both sides, after
//! which `pai_sync_now` has something to encrypt to.

use pai_ffi::*;
use std::ffi::{CStr, CString};

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5q-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

unsafe fn init(dir: &std::path::Path) -> *mut PaiRuntime {
    let cfg = CString::new(
        serde_json::json!({
            "data_dir": dir.to_string_lossy(),
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
fn pair_offer_accept_complete_end_to_end() {
    unsafe {
        let dir_a = tmpdir("a");
        let dir_b = tmpdir("b");
        let a = init(&dir_a);
        let b = init(&dir_b);

        // 1. A offers.
        let offer = dir_a.join("offer.pai");
        let out = CString::new(offer.to_string_lossy().to_string()).unwrap();
        let v = json(pai_pair_offer(a, out.as_ptr()));
        assert!(v["error"].is_null(), "offer: {v}");
        assert!(offer.exists());

        // 2. B accepts — offerer becomes a peer; accept.pai seals vault.
        let accept = dir_b.join("accept.pai");
        let op = CString::new(offer.to_string_lossy().to_string()).unwrap();
        let ap = CString::new(accept.to_string_lossy().to_string()).unwrap();
        let v = json(pai_pair_accept(b, op.as_ptr(), ap.as_ptr()));
        assert!(v["error"].is_null(), "accept: {v}");
        assert!(accept.exists());
        assert!(v["peer"].as_str().is_some());

        // 3. A completes — vault key adopted.
        let acp = CString::new(accept.to_string_lossy().to_string()).unwrap();
        let v = json(pai_pair_complete(a, acp.as_ptr()));
        assert!(v["error"].is_null(), "complete: {v}");

        // Both sides now have a vault → folder sync no longer errors
        // with "vault key".
        let shared = tmpdir("shared");
        let req = CString::new(
            serde_json::json!({"dir": shared.to_string_lossy(), "mode": "push"})
                .to_string(),
        )
        .unwrap();
        let v = json(pai_sync_now(a, req.as_ptr()));
        assert!(
            v["error"].is_null() || !v["error"].as_str().unwrap().contains("vault key"),
            "paired device should sync: {v}"
        );

        pai_free(a);
        pai_free(b);
    }
}

#[test]
fn pair_offer_bad_path_errors() {
    unsafe {
        let h = init(&tmpdir("solo"));
        let out = CString::new("Z:\\no\\such\\dir\\offer.pai").unwrap();
        let v = json(pai_pair_offer(h, out.as_ptr()));
        assert!(v["error"].as_str().is_some());
        pai_free(h);
    }
}

#[test]
fn init_reuses_persisted_identity() {
    unsafe {
        let dir = tmpdir("reinit");
        let a = init(&dir);
        let status_a = json(pai_status(a));
        pai_free(a);
        let b = init(&dir);
        let status_b = json(pai_status(b));
        // Same data dir → same device across inits (pairing targets a
        // stable identity).
        assert_eq!(status_a["device"], status_b["device"]);
        pai_free(b);
    }
}

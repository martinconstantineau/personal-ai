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
    let v: serde_json::Value = serde_json::from_str(CStr::from_ptr(p).to_str().unwrap()).unwrap();
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
            serde_json::json!({"dir": shared.to_string_lossy(), "mode": "push"}).to_string(),
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

/// The folder exchange: two runtimes share a sync dir; alternating
/// `pai_pair_folder` calls walk the whole offer/accept/complete dance —
/// no manual file shuffling.
#[test]
fn pair_folder_exchange_end_to_end() {
    unsafe {
        let dir_a = tmpdir("fa");
        let dir_b = tmpdir("fb");
        let shared = tmpdir("fshared");
        let a = init(&dir_a);
        let b = init(&dir_b);

        // Configure sync.dir on both — a sync_now with a dir persists
        // the target even though the run itself fails pre-pairing.
        for h in [a, b] {
            let req =
                CString::new(serde_json::json!({"dir": shared.to_string_lossy()}).to_string())
                    .unwrap();
            let _ = json(pai_sync_now(h, req.as_ptr()));
        }

        // Round 1: A publishes its offer.
        let v = json(pai_pair_folder(a));
        assert_eq!(v["accepted"].as_array().unwrap().len(), 0, "{v}");
        assert_eq!(v["completed"].as_array().unwrap().len(), 0, "{v}");

        // Round 2: B publishes, finds A's offer, writes an accept.
        let v = json(pai_pair_folder(b));
        assert!(v["error"].is_null(), "{v}");
        assert_eq!(v["accepted"].as_array().unwrap().len(), 1, "{v}");
        assert_eq!(v["completed"].as_array().unwrap().len(), 0, "{v}");

        // Round 3: A completes B's accept (adopts B's vault) and
        // answers B's offer with an accept.
        let v = json(pai_pair_folder(a));
        assert_eq!(v["completed"].as_array().unwrap().len(), 1, "{v}");
        assert_eq!(v["accepted"].as_array().unwrap().len(), 1, "{v}");

        // Round 4: crossed offers resolve — B already recorded A as a
        // peer at accept time, so completing A's accept is a no-op (and
        // would conflict anyway: both already live in B's vault).
        let v = json(pai_pair_folder(b));
        assert_eq!(v["completed"].as_array().unwrap().len(), 0, "{v}");

        // Re-running is quiet: existing peers are skipped.
        let v = json(pai_pair_folder(a));
        assert_eq!(v["completed"].as_array().unwrap().len(), 0, "{v}");
        assert_eq!(v["accepted"].as_array().unwrap().len(), 0, "{v}");

        // Status: both report a peer + a vault.
        for h in [a, b] {
            let st = json(pai_sync_status(h));
            assert_eq!(st["peers"].as_u64().unwrap(), 1, "{st}");
            assert_eq!(st["has_vault"].as_bool().unwrap(), true, "{st}");
            assert_eq!(
                st["dir"].as_str().unwrap(),
                shared.to_string_lossy(),
                "{st}"
            );
        }

        // auto_minutes persists through sync_now and shows in status.
        let req =
            CString::new(serde_json::json!({"auto_minutes": 15u64, "mode": "push"}).to_string())
                .unwrap();
        let v = json(pai_sync_now(a, req.as_ptr()));
        assert!(v["error"].is_null(), "paired folder sync: {v}");
        let st = json(pai_sync_status(a));
        assert_eq!(st["auto_minutes"].as_u64().unwrap(), 15, "{st}");

        pai_free(a);
        pai_free(b);
    }
}

#[test]
fn pair_folder_without_sync_dir_errors() {
    unsafe {
        let h = init(&tmpdir("nofolder"));
        let v = json(pai_pair_folder(h));
        assert!(
            v["error"].as_str().unwrap().contains("shared folder"),
            "{v}"
        );
        pai_free(h);
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

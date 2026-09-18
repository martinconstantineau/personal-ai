//! V5r: in-app email configuration — `pai_email_configure` writes
//! email.json, stores the password in the keystore when one is
//! reachable, and hot-swaps the provider so search/read/draft stop
//! returning "not configured" without a restart.
//!
//! `PAI_KEYSTORE_OFF` keeps the test out of the OS credential vault —
//! secret storage then degrades to the callers' 0600-file fallbacks.

use pai_ffi::*;
use std::ffi::{CStr, CString};

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5r-{tag}-{}", uuid::Uuid::new_v4()));
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
fn configure_hot_swaps_provider() {
    std::env::set_var("PAI_KEYSTORE_OFF", "1");
    unsafe {
        let dir = tmpdir("cfg");
        let h = init(&dir);

        // Before configuration the connector reports the honest error.
        let r = json(pai_email_search(h, std::ptr::null()));
        assert_eq!(r["error"], "email not configured (email.json)");

        // Configure with an unreachable host — config + credential writes
        // succeed; only live calls would hit the network. The smtp host
        // shares the imap domain: the configure gate refuses cross-domain
        // relays (creds may only reach the account's own domain).
        let cfg = CString::new(
            serde_json::json!({
                "host": "imap.example.com",
                "port": 993,
                "user": "me@example.com",
                "password": "app-password",
                "smtp": {"host": "smtp.example.com", "port": 465, "tls": "tls"},
            })
            .to_string(),
        )
        .unwrap();
        let r = json(pai_email_configure(h, cfg.as_ptr()));
        assert_eq!(r["configured"], true, "{r}");
        assert!(r["password_stored"].is_boolean(), "{r}");

        // email.json landed — and the password is NOT in it.
        let raw = std::fs::read_to_string(dir.join("email.json")).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved["host"], "imap.example.com");
        assert_eq!(saved["smtp"]["host"], "smtp.example.com");
        assert!(
            !raw.contains("app-password"),
            "password must not be persisted"
        );

        // The live provider is swapped — search now fails with a
        // connection error, not "not configured".
        let r = json(pai_email_search(h, std::ptr::null()));
        let err = r["error"].as_str().unwrap_or_default();
        assert!(r["error"].is_string(), "{r}");
        assert!(!err.contains("not configured"), "{err}");

        // A fresh init picks the config up from disk.
        let h2 = init(&dir);
        let r = json(pai_email_search(h2, std::ptr::null()));
        let err = r["error"].as_str().unwrap_or_default();
        assert!(!err.contains("not configured"), "{err}");

        pai_free(h);
        pai_free(h2);
    }
}

#[test]
fn configure_validates_input() {
    std::env::set_var("PAI_KEYSTORE_OFF", "1");
    unsafe {
        let dir = tmpdir("bad");
        let h = init(&dir);

        // Missing host → error, nothing written.
        let cfg = CString::new(r#"{"host":"","user":"me@example.com"}"#).unwrap();
        let r = json(pai_email_configure(h, cfg.as_ptr()));
        assert!(r["error"].is_string(), "{r}");
        assert!(!dir.join("email.json").exists());

        // Bad smtp tls mode → error, nothing written.
        let cfg = CString::new(
            r#"{"host":"imap.invalid","user":"me@example.com",
                "smtp":{"host":"smtp.invalid","tls":"carrier-pigeon"}}"#,
        )
        .unwrap();
        let r = json(pai_email_configure(h, cfg.as_ptr()));
        assert!(
            r["error"].as_str().unwrap_or_default().contains("tls"),
            "{r}"
        );
        assert!(!dir.join("email.json").exists());

        pai_free(h);
    }
}

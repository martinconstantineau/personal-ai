//! V2o tests: OAuth2 device flow + XOAUTH2 auth for the email connector.
//! Token endpoints are mocked over loopback HTTP (reqwest needs no TLS
//! for 127.0.0.1); the SMTP AUTH XOAUTH2 path runs against the same
//! wire-faithful mock server pattern as V2j. No real IdP is contacted.

use base64::Engine;
use pai_connector_email::imap::ImapConfig;
use pai_connector_email::oauth::{self, AuthMaterial, OAuthConfig, Poll, SmtpAuth};
use pai_connector_email::smtp::{SmtpConfig, SmtpProvider, SmtpTls};
use pai_connector_email::{Draft, EmailAddress};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

// -- mock OAuth HTTP server --------------------------------------------

/// Canned JSON by request path; logs each request body for assertions.
struct MockIdp {
    port: u16,
    bodies: mpsc::Receiver<String>,
}

fn mock_idp(responses: Vec<(&'static str, u16, String)>) -> (MockIdp, mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (body_tx, body_rx) = mpsc::channel::<String>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let responses = std::sync::Arc::new(responses);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            let mut s = stream.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read headers + body (content-length driven).
            let mut headers_end = None;
            while headers_end.is_none() {
                let n = s.read(&mut chunk).unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                headers_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
            }
            let he = headers_end.unwrap();
            let head = String::from_utf8_lossy(&buf[..he]).to_string();
            let clen: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse().ok())
                })
                .unwrap_or(0);
            while buf.len() < he + clen {
                let n = s.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8_lossy(&buf[he..he + clen]).to_string();
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let _ = body_tx.send(format!("{path} {body}"));
            let (status, payload) = responses
                .iter()
                .find(|(p, _, _)| path.starts_with(p))
                .map(|(_, s, b)| (*s, b.clone()))
                .unwrap_or((404, "{\"error\":\"not_found\"}".into()));
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
        }
    });
    (
        MockIdp {
            port,
            bodies: body_rx,
        },
        stop_tx,
    )
}

fn oauth_cfg(idp: &MockIdp) -> OAuthConfig {
    OAuthConfig {
        provider: "custom".into(),
        client_id: "pai-test-client".into(),
        tenant: None,
        device_url: Some(format!("http://127.0.0.1:{}/device", idp.port)),
        token_url: Some(format!("http://127.0.0.1:{}/token", idp.port)),
        scopes: Some(vec!["mail".into()]),
    }
}

// -- endpoint presets ---------------------------------------------------

#[test]
fn provider_presets_resolve() {
    let g = OAuthConfig {
        provider: "google".into(),
        client_id: "x".into(),
        tenant: None,
        device_url: None,
        token_url: None,
        scopes: None,
    };
    assert_eq!(
        g.device_url().unwrap(),
        "https://oauth2.googleapis.com/device/code"
    );
    assert_eq!(
        g.token_url().unwrap(),
        "https://oauth2.googleapis.com/token"
    );
    assert_eq!(g.scope_string().unwrap(), "https://mail.google.com/");

    let m = OAuthConfig {
        provider: "microsoft".into(),
        client_id: "x".into(),
        tenant: Some("consumers".into()),
        device_url: None,
        token_url: None,
        scopes: None,
    };
    assert!(m
        .device_url()
        .unwrap()
        .contains("login.microsoftonline.com/consumers/"));
    assert!(m.scope_string().unwrap().contains("offline_access"));
    assert!(m.scope_string().unwrap().contains("IMAP.AccessAsUser.All"));

    // Unknown provider without overrides errors cleanly.
    let bad = OAuthConfig {
        provider: "aol".into(),
        client_id: "x".into(),
        tenant: None,
        device_url: None,
        token_url: None,
        scopes: None,
    };
    assert!(bad.device_url().is_err());
}

// -- SASL-IR ------------------------------------------------------------

#[test]
fn xoauth2_ir_is_rfc_form() {
    let ir = oauth::xoauth2_ir("me@x.example", "tok123");
    assert_eq!(ir, "user=me@x.example\x01auth=Bearer tok123\x01\x01");
    // SMTP's single-line form is its base64.
    let b64 = oauth::xoauth2_b64("me@x.example", "tok123");
    let back = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .unwrap();
    assert_eq!(String::from_utf8(back).unwrap(), ir);
}

// -- device flow + refresh ----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_flow_pendings_then_grant() {
    let (idp, _stop) = mock_idp(vec![
        (
            "/device",
            200,
            r#"{"device_code":"dc-1","user_code":"ABCD-EFGH","verification_uri":"https://idp.example/activate","interval":1,"expires_in":300}"#
                .into(),
        ),
        (
            "/token",
            400,
            r#"{"error":"authorization_pending","error_description":"not yet"}"#.into(),
        ),
    ]);
    // Second /token response differs — the mock serves the FIRST match
    // for every request, so use two servers or a stateful handler.
    // Here: pending server for the first poll, then swap to a granting
    // server for the second.
    let (idp2, _stop2) = mock_idp(vec![(
        "/token",
        200,
        r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#.into(),
    )]);
    let cfg = oauth_cfg(&idp);
    let grant = oauth::device_flow(&cfg).await.unwrap();
    assert_eq!(grant.user_code, "ABCD-EFGH");
    assert_eq!(grant.device_code, "dc-1");
    // First poll → Pending.
    match oauth::poll_token_once(&cfg, &grant.device_code)
        .await
        .unwrap()
    {
        Poll::Pending => {}
        Poll::Granted(_) => panic!("expected pending"),
    }
    // Granted server.
    let cfg2 = oauth_cfg(&idp2);
    match oauth::poll_token_once(&cfg2, "dc-1").await.unwrap() {
        Poll::Granted(t) => {
            assert_eq!(t.access_token, "at-1");
            assert_eq!(t.refresh_token.as_deref(), Some("rt-1"));
        }
        Poll::Pending => panic!("expected grant"),
    }
    // The form posts carried the right fields.
    let first = idp.bodies.recv().unwrap();
    assert!(first.contains("client_id=pai-test-client"));
    let poll = idp.bodies.recv().unwrap();
    assert!(poll.contains("device_code=dc-1"));
    assert!(poll.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_swaps_access_token() {
    let (idp, _stop) = mock_idp(vec![(
        "/token",
        200,
        r#"{"access_token":"at-new","refresh_token":"rt-rotated","expires_in":3600}"#.into(),
    )]);
    let cfg = oauth_cfg(&idp);
    let (access, rotated) = oauth::refresh_access_token(&cfg, "rt-old").await.unwrap();
    assert_eq!(access, "at-new");
    assert_eq!(rotated.as_deref(), Some("rt-rotated"));
    let body = idp.bodies.recv().unwrap();
    assert!(body.contains("grant_type=refresh_token"));
    assert!(body.contains("refresh_token=rt-old"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_flow_errors_on_denied() {
    let (idp, _stop) = mock_idp(vec![(
        "/token",
        400,
        r#"{"error":"access_denied","error_description":"user said no"}"#.into(),
    )]);
    let cfg = oauth_cfg(&idp);
    let err = oauth::poll_token_once(&cfg, "dc").await.unwrap_err();
    assert!(err.to_string().contains("user said no"), "got: {err}");
}

// -- SMTP AUTH XOAUTH2 wire ----------------------------------------------

/// Mock submission server that replies 235 to any AUTH and logs lines.
fn mock_smtp() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut log = String::new();
        let mut buf = Vec::new();
        s.write_all(b"220 mock ESMTP\r\n").unwrap();
        loop {
            let mut b = [0u8; 1];
            if s.read(&mut b).unwrap_or(0) == 0 {
                break;
            }
            buf.push(b[0]);
            if b[0] != b'\n' {
                continue;
            }
            let line = String::from_utf8_lossy(&buf).trim_end().to_string();
            log.push_str(&line);
            log.push('\n');
            let resp = if line.starts_with("EHLO") {
                "250-mock\r\n250 AUTH PLAIN XOAUTH2\r\n"
            } else if line.starts_with("AUTH") {
                "235 ok\r\n"
            } else if line.starts_with("MAIL") || line.starts_with("RCPT") {
                "250 ok\r\n"
            } else if line == "DATA" {
                "354 go\r\n"
            } else if line == "." {
                "250 queued\r\n"
            } else if line == "QUIT" {
                "221 bye\r\n"
            } else {
                buf.clear();
                continue;
            };
            s.write_all(resp.as_bytes()).unwrap();
            buf.clear();
            if line == "QUIT" {
                break;
            }
        }
        let _ = tx.send(log);
    });
    (port, rx)
}

fn draft() -> Draft {
    Draft {
        to: vec![EmailAddress {
            name: None,
            address: "bob@x.example".into(),
        }],
        cc: vec![],
        subject: "oauth".into(),
        body: "hi".into(),
        in_reply_to: None,
    }
}

#[test]
fn smtp_auth_xoauth2_wire() {
    let (port, rx) = mock_smtp();
    let cfg = SmtpConfig {
        host: "127.0.0.1".into(),
        port,
        tls: SmtpTls::None,
        user: None,
    };
    let p = SmtpProvider::new(cfg, "me@x.example".into());
    let b64 = oauth::xoauth2_b64("me@x.example", "live-token-42");
    p.send_blocking(&draft(), &SmtpAuth::Xoauth2(b64.clone()))
        .unwrap();
    let log = rx.recv().unwrap();
    assert!(log.contains(&format!("AUTH XOAUTH2 {b64}")), "log: {log}");
    assert!(log.contains("MAIL FROM:<me@x.example>"));
    assert!(log.contains("QUIT"));
}

// -- auth selection ------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_auth_picks_password_without_oauth_block() {
    std::env::set_var("PAI_EMAIL_PASSWORD", "env-pw");
    let cfg = ImapConfig {
        host: "x".into(),
        port: 993,
        user: "me@x.example".into(),
        mailbox: "INBOX".into(),
        drafts_mailbox: "D".into(),
        archive_mailbox: "A".into(),
        smtp: None,
        oauth: None,
    };
    match oauth::resolve_auth(&cfg).await.unwrap() {
        AuthMaterial::Password(p) => assert_eq!(p, "env-pw"),
        _ => panic!("expected password path"),
    }
    std::env::remove_var("PAI_EMAIL_PASSWORD");
}

#[test]
fn oauth_block_survives_config_roundtrip() {
    let dir = std::env::temp_dir().join(format!("pai-v2o-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = ImapConfig {
        host: "imap.gmail.com".into(),
        port: 993,
        user: "me@gmail.com".into(),
        mailbox: "INBOX".into(),
        drafts_mailbox: "[Gmail]/Drafts".into(),
        archive_mailbox: "[Gmail]/All Mail".into(),
        smtp: None,
        oauth: Some(OAuthConfig {
            provider: "google".into(),
            client_id: "cid-123".into(),
            tenant: None,
            device_url: None,
            token_url: None,
            scopes: None,
        }),
    };
    cfg.save(&dir).unwrap();
    // The file holds the oauth block but NO tokens.
    let raw = std::fs::read_to_string(dir.join("email.json")).unwrap();
    assert!(raw.contains("\"oauth\""));
    assert!(raw.contains("cid-123"));
    assert!(!raw.contains("refresh"));
    let back = ImapConfig::load(&dir).unwrap().unwrap();
    let o = back.oauth.unwrap();
    assert_eq!(o.provider, "google");
    assert_eq!(o.client_id, "cid-123");
}

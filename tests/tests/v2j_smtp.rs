//! V2j tests: SMTP submission — a mock server on localhost verifies the
//! wire dialogue (EHLO/AUTH/MAIL/RCPT/DATA/dot-stuffing), and the
//! `ImapProvider::send` delegation path works through `email.json`'s
//! `smtp` block.

use pai_connector_email::imap::ImapConfig;
use pai_connector_email::oauth::SmtpAuth;
use pai_connector_email::smtp::SmtpProvider;
use pai_connector_email::{Draft, EmailAddress, EmailProvider, SmtpConfig, SmtpTls};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// Speak just enough SMTP to satisfy one send; returns the full
/// client-side transcript via the channel.
fn mock_server() -> (u16, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        let mut r = BufReader::new(sock.try_clone().unwrap());
        let mut w = sock;
        let mut log = String::new();
        let say = |w: &mut dyn Write, s: &str| w.write_all(s.as_bytes()).unwrap();
        say(&mut w, "220 mock ready\r\n");
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let l = line.trim_end().to_string();
            log.push_str(&l);
            log.push('\n');
            let verb = l.split_whitespace().next().unwrap_or("");
            match verb {
                "EHLO" => say(&mut w, "250-mock\r\n250 AUTH PLAIN\r\n"),
                "AUTH" => say(&mut w, "235 authenticated\r\n"),
                "MAIL" => say(&mut w, "250 sender ok\r\n"),
                "RCPT" => say(&mut w, "250 rcpt ok\r\n"),
                "DATA" => {
                    say(&mut w, "354 go ahead\r\n");
                    // Read until the lone-dot terminator.
                    loop {
                        line.clear();
                        if r.read_line(&mut line).unwrap() == 0 {
                            break;
                        }
                        let l = line.trim_end().to_string();
                        if l == "." {
                            break;
                        }
                        log.push_str(&l);
                        log.push('\n');
                    }
                    say(&mut w, "250 queued\r\n");
                }
                "QUIT" => {
                    say(&mut w, "221 bye\r\n");
                    break;
                }
                "RSET" => say(&mut w, "250 ok\r\n"),
                _ => say(&mut w, "250 ok\r\n"),
            }
        }
        let _ = tx.send(log);
    });
    (port, rx)
}

fn draft() -> Draft {
    Draft {
        to: vec![EmailAddress {
            name: Some("Bob".into()),
            address: "bob@x.example".into(),
        }],
        cc: vec![EmailAddress {
            name: None,
            address: "carol@x.example".into(),
        }],
        subject: "hello there".into(),
        body: "first line\n.dotty line\nlast".into(),
        in_reply_to: None,
    }
}

fn smtp_cfg(port: u16) -> SmtpConfig {
    SmtpConfig {
        host: "127.0.0.1".into(),
        port,
        tls: SmtpTls::None,
        user: None,
    }
}

#[test]
fn smtp_dialog_sends_full_message() {
    let (port, rx) = mock_server();
    let p = SmtpProvider::new(smtp_cfg(port), "me@x.example".into());
    p.send_blocking(&draft(), &SmtpAuth::Plain("secret".into()))
        .unwrap();
    let log = rx.recv().unwrap();

    // AUTH PLAIN: base64("\0me@x.example\0secret")
    use base64::Engine;
    let expect_auth =
        base64::engine::general_purpose::STANDARD.encode("\0me@x.example\0secret".as_bytes());
    assert!(
        log.contains(&format!("AUTH PLAIN {expect_auth}")),
        "log: {log}"
    );
    assert!(log.contains("MAIL FROM:<me@x.example>"));
    assert!(log.contains("RCPT TO:<bob@x.example>"));
    assert!(log.contains("RCPT TO:<carol@x.example>"));
    assert!(log.contains("Subject: hello there"));
    assert!(log.contains("Date: "));
    assert!(log.contains("Message-ID: <"));
    // Dot-stuffed line: ".dotty line" → "..dotty line"
    assert!(log.contains("..dotty line"), "log: {log}");
    assert!(log.contains("QUIT"));
}

#[test]
fn smtp_no_recipients_errors() {
    let (port, _rx) = mock_server();
    let p = SmtpProvider::new(smtp_cfg(port), "me@x.example".into());
    let mut d = draft();
    d.to.clear();
    d.cc.clear();
    let err = p
        .send_blocking(&d, &SmtpAuth::None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no recipients"), "unexpected: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imap_provider_send_delegates_to_smtp() {
    let (port, rx) = mock_server();
    std::env::set_var("PAI_EMAIL_PASSWORD", "pw-from-env");
    let cfg = ImapConfig {
        host: "imap.unreachable".into(),
        port: 993,
        user: "me@x.example".into(),
        mailbox: "INBOX".into(),
        drafts_mailbox: "Drafts".into(),
        archive_mailbox: "Archive".into(),
        smtp: Some(smtp_cfg(port)),
        oauth: None,
    };
    // send() never touches IMAP — only the smtp block matters.
    pai_connector_email::ImapProvider::new(cfg)
        .send(&draft())
        .await
        .unwrap();
    let log = rx.recv().unwrap();
    assert!(log.contains("MAIL FROM:<me@x.example>"));
    assert!(log.contains("RCPT TO:<bob@x.example>"));
    std::env::remove_var("PAI_EMAIL_PASSWORD");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imap_provider_send_without_smtp_errors() {
    let cfg = ImapConfig {
        host: "imap.unreachable".into(),
        port: 993,
        user: "me@x.example".into(),
        mailbox: "INBOX".into(),
        drafts_mailbox: "Drafts".into(),
        archive_mailbox: "Archive".into(),
        smtp: None,
        oauth: None,
    };
    let err = pai_connector_email::ImapProvider::new(cfg)
        .send(&draft())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("smtp"), "unexpected: {err}");
}

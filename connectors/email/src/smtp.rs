//! SMTP submission — the send path behind `EmailProvider::send`.
//!
//! Hand-rolled over the same rustls + Mozilla-roots stack as the IMAP
//! adapter (no lettre/native-tls — those pull OS-cert chains that don't
//! build on every target here). Only what's needed for submission:
//! EHLO → (STARTTLS) → AUTH PLAIN → MAIL/RCPT/DATA → QUIT.
//!
//! Config lives in `email.json` as an optional `smtp` block — the
//! password resolves through the same `email:<user>` keystore entry as
//! IMAP (app passwords cover both protocols).

use crate::imap::build_draft_message;
use crate::oauth::SmtpAuth;
use crate::Draft;
use pai_core::*;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// TLS mode for the submission connection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    /// Implicit TLS from connect (port 465).
    Tls,
    /// Plain connect, upgrade via STARTTLS (port 587).
    StartTls,
    /// No TLS — localhost/relay testing only, never for the open internet.
    None,
}

/// Submission account. Optional block inside `email.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    #[serde(default = "default_smtp_tls")]
    pub tls: SmtpTls,
    /// Login + envelope-from. Defaults to the IMAP user when omitted.
    pub user: Option<String>,
}

fn default_smtp_port() -> u16 {
    465
}
fn default_smtp_tls() -> SmtpTls {
    SmtpTls::Tls
}

/// Sends drafts through a submission server.
pub struct SmtpProvider {
    cfg: SmtpConfig,
    /// Account user (auth + envelope from) — usually the IMAP user.
    user: String,
}

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

fn err(e: impl std::fmt::Display) -> Error {
    Error::Provider(format!("smtp: {e}"))
}

/// Line-buffered dialog over a duplex stream. `S: Read+Write` keeps one
/// implementation for TCP and TLS streams (a `StreamOwned` can't be
/// split into read/write halves, so reads buffer internally).
struct Dialog<S> {
    s: S,
    buf: Vec<u8>,
}

impl<S: Read + Write> Dialog<S> {
    fn new(s: S) -> Self {
        Self { s, buf: Vec::new() }
    }

    /// One reply line (without CRLF). SMTP replies are tiny — byte-at-a-
    /// time reads are fine and keep `buf` handling trivial.
    fn line(&mut self) -> Result<String> {
        let mut out = Vec::with_capacity(128);
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                out.extend_from_slice(&self.buf[..pos]);
                self.buf.drain(..=pos);
                if out.last() == Some(&b'\r') {
                    out.pop();
                }
                return String::from_utf8(out)
                    .map_err(|_| Error::Provider("smtp: non-UTF8 reply".into()));
            }
            let mut chunk = [0u8; 512];
            let n = self.s.read(&mut chunk).map_err(err)?;
            if n == 0 {
                return Err(Error::Provider("smtp: connection closed".into()));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Read a (possibly multiline) reply → its 3-digit code.
    fn reply(&mut self) -> Result<u16> {
        loop {
            let line = self.line()?;
            if line.len() < 4 {
                return Err(Error::Provider(format!("smtp: bad reply {line:?}")));
            }
            let code: u16 = line[..3]
                .parse()
                .map_err(|_| Error::Provider(format!("smtp: bad reply {line:?}")))?;
            if line.as_bytes()[3] == b' ' {
                return Ok(code);
            }
            // '-' continues the multiline reply.
        }
    }

    fn expect(&mut self, want: u16) -> Result<()> {
        let got = self.reply()?;
        if got != want {
            return Err(Error::Provider(format!(
                "smtp: expected {want}, server replied {got}"
            )));
        }
        Ok(())
    }

    fn cmd(&mut self, line: &str, want: u16) -> Result<()> {
        self.s.write_all(line.as_bytes()).map_err(err)?;
        self.s.write_all(b"\r\n").map_err(err)?;
        self.s.flush().map_err(err)?;
        self.expect(want)
    }
}

/// Dot-stuff + CRLF-normalize the DATA payload per RFC 5321 §4.5.2.
fn data_block(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len() + 16);
    for line in msg.replace("\r\n", "\n").split('\n') {
        if let Some(rest) = line.strip_prefix('.') {
            out.push('.');
            out.push('.');
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

fn tls_wrap(host: &str, tcp: TcpStream) -> Result<Tls> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| Error::InvalidInput(format!("bad smtp host {host}: {e}")))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(err)?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

/// AUTH (PLAIN or XOAUTH2) → MAIL FROM → RCPT TO* → DATA → QUIT.
/// Shared by all TLS modes once the transport is up.
fn session<S: Read + Write>(
    d: &mut Dialog<S>,
    ehlo: &str,
    user: &str,
    auth: &SmtpAuth,
    draft: &Draft,
) -> Result<()> {
    use base64::Engine;
    d.cmd(&format!("EHLO {ehlo}"), 250)?;
    match auth {
        SmtpAuth::None => {}
        SmtpAuth::Plain(pass) => {
            // AUTH PLAIN: base64("\0user\0pass")
            let a = base64::engine::general_purpose::STANDARD
                .encode(format!("\0{user}\0{pass}").as_bytes());
            d.cmd(&format!("AUTH PLAIN {a}"), 235)?;
        }
        SmtpAuth::Xoauth2(b64_ir) => {
            d.cmd(&format!("AUTH XOAUTH2 {b64_ir}"), 235)?;
        }
    }
    d.cmd(&format!("MAIL FROM:<{user}>"), 250)?;
    let mut rcpts = 0usize;
    for a in draft.to.iter().chain(draft.cc.iter()) {
        d.cmd(&format!("RCPT TO:<{}>", a.address), 250)?;
        rcpts += 1;
    }
    if rcpts == 0 {
        return Err(Error::InvalidInput("draft has no recipients".into()));
    }
    d.cmd("DATA", 354)?;
    let msg = build_draft_message(draft, user);
    d.s.write_all(data_block(&msg).as_bytes()).map_err(err)?;
    d.s.flush().map_err(err)?;
    d.expect(250)?;
    let _ = d.cmd("QUIT", 221); // some servers drop without replying
    Ok(())
}

impl SmtpProvider {
    pub fn new(cfg: SmtpConfig, user: String) -> Self {
        Self { cfg, user }
    }

    /// Blocking send — call inside `spawn_blocking`.
    pub fn send_blocking(&self, draft: &Draft, auth: &SmtpAuth) -> Result<()> {
        let cfg = &self.cfg;
        let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port)).map_err(err)?;
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(60)))
            .map_err(err)?;
        tcp.set_write_timeout(Some(std::time::Duration::from_secs(60)))
            .map_err(err)?;

        match cfg.tls {
            SmtpTls::Tls => {
                let mut d = Dialog::new(tls_wrap(&cfg.host, tcp)?);
                d.expect(220)?;
                session(&mut d, &cfg.host, &self.user, auth, draft)
            }
            SmtpTls::StartTls => {
                let mut d = Dialog::new(tcp);
                d.expect(220)?;
                d.cmd(&format!("EHLO {}", cfg.host), 250)?;
                d.cmd("STARTTLS", 220)?;
                let mut d = Dialog::new(tls_wrap(&cfg.host, d.s)?);
                session(&mut d, &cfg.host, &self.user, auth, draft)
            }
            SmtpTls::None => {
                let mut d = Dialog::new(tcp);
                d.expect(220)?;
                session(&mut d, &cfg.host, &self.user, auth, draft)
            }
        }
    }

    /// Send with explicit auth material (password or XOAUTH2 IR —
    /// resolved by the caller, usually `ImapProvider::send`).
    pub async fn send_with_auth(&self, draft: &Draft, auth: SmtpAuth) -> Result<()> {
        let d = draft.clone();
        let me = Self {
            cfg: self.cfg.clone(),
            user: self.user.clone(),
        };
        tokio::task::spawn_blocking(move || me.send_blocking(&d, &auth))
            .await
            .map_err(|e| Error::Provider(format!("smtp task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_stuffs_and_crlfs() {
        let msg = "Subject: x\r\n\r\nline1\n.evil\nline3\n";
        let out = data_block(msg);
        assert!(out.contains("\r\n..evil\r\n"), "dot not stuffed: {out:?}");
        assert!(out.ends_with("\r\n.\r\n"));
        assert!(!out.contains("\n.evil"));
    }
}

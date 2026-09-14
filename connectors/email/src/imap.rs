//! IMAP adapter — the first real `EmailProvider`.
//!
//! Deliberately boring: each op opens a short-lived TLS session inside
//! `spawn_blocking` (the `imap` crate is synchronous), does its work, and
//! logs out. Email ops are user-initiated and infrequent; connection
//! pooling is premature.
//!
//! Drafts-first by design: `send` is unsupported — IMAP has no SEND
//! verb. The agent creates drafts (`create_draft` APPENDs to the drafts
//! mailbox); the user hits send in their own client. SMTP/lettre is
//! roadmap.
//!
//! Mailbox mapping: `EmailSearch.label` selects that mailbox (Gmail
//! exposes labels as IMAP mailboxes — `[Gmail]/All Mail`, `INBOX`).

use crate::{
    Draft, EmailAddress, EmailAttachment, EmailMessage, EmailProvider, EmailSearch, EmailSummary,
};
use async_trait::async_trait;
use mail_parser::{Addr, MessageParser, MimeHeaders};
use pai_core::*;
use pai_identity::keystore;
use serde::{Deserialize, Serialize};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

/// One IMAP account. Persisted as `<data_dir>/email.json`; the password
/// is never in it — see [`resolve_password`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImapConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    /// Mailbox `search`/`read`/`archive`/`delete` operate on.
    #[serde(default = "default_mailbox")]
    pub mailbox: String,
    /// Where drafts are APPENDed. Gmail: "[Gmail]/Drafts".
    #[serde(default = "default_drafts")]
    pub drafts_mailbox: String,
    /// MOVE target for `archive`. Gmail: "[Gmail]/All Mail".
    #[serde(default = "default_archive")]
    pub archive_mailbox: String,
}

fn default_port() -> u16 {
    993
}
fn default_mailbox() -> String {
    "INBOX".into()
}
fn default_drafts() -> String {
    "Drafts".into()
}
fn default_archive() -> String {
    "Archive".into()
}

impl ImapConfig {
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let f = data_dir.join("email.json");
        if !f.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&f).map_err(|e| Error::Storage(e.to_string()))?;
        let cfg = serde_json::from_slice(&raw)
            .map_err(|e| Error::InvalidInput(format!("bad email.json: {e}")))?;
        Ok(Some(cfg))
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir).map_err(|e| Error::Storage(e.to_string()))?;
        let raw = serde_json::to_vec_pretty(self).map_err(|e| Error::Storage(e.to_string()))?;
        std::fs::write(data_dir.join("email.json"), raw).map_err(|e| Error::Storage(e.to_string()))
    }
}

/// Password resolution: `PAI_EMAIL_PASSWORD` env → OS keystore
/// `email:<user>`. Keystore writes happen via [`store_password`] so the
/// secret never touches email.json.
pub fn resolve_password(user: &str) -> Result<String> {
    if let Ok(p) = std::env::var("PAI_EMAIL_PASSWORD") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    keystore::load(&format!("email:{user}"))
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| {
            Error::InvalidInput(
                "no email password — set PAI_EMAIL_PASSWORD or run `pai email configure`".into(),
            )
        })
}

pub fn store_password(user: &str, password: &str) -> bool {
    keystore::store(&format!("email:{user}"), password.as_bytes())
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct ImapProvider {
    cfg: ImapConfig,
}

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
type Session = imap::Session<Tls>;

/// rustls + Mozilla roots — the same stack reqwest uses for HTTPS. This
/// avoids the `imap` crate's rustls-connector/native-tls feature chain
/// (which needs OS cert loading and extra platform TLS deps).
fn tls_stream(host: &str, port: u16) -> Result<Tls> {
    let tcp = TcpStream::connect((host, port)).map_err(err)?;
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .map_err(err)?;
    tcp.set_write_timeout(Some(std::time::Duration::from_secs(60)))
        .map_err(err)?;
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| Error::InvalidInput(format!("bad imap host {host}: {e}")))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(err)?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

impl ImapProvider {
    pub fn new(cfg: ImapConfig) -> Self {
        Self { cfg }
    }

    fn session(&self) -> Result<Session> {
        let cfg = &self.cfg;
        let pass = resolve_password(&cfg.user)?;
        let tls = tls_stream(&cfg.host, cfg.port)?;
        let mut client = imap::Client::new(tls);
        client.read_greeting().map_err(err)?;
        client
            .login(&cfg.user, &pass)
            .map_err(|(e, _)| err(e))
            .map_err(err_auth_hint)
    }

    /// Run `f` on a fresh session in `spawn_blocking`.
    async fn with_session<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Session, &ImapConfig) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let cfg = self.cfg.clone();
        let mut s = self.session()?;
        tokio::task::spawn_blocking(move || {
            let out = f(&mut s, &cfg);
            let _ = s.logout();
            out
        })
        .await
        .map_err(|e| Error::Provider(format!("imap task: {e}")))?
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::Provider(format!("imap: {e}"))
}

fn err_auth_hint(e: Error) -> Error {
    match e {
        Error::Provider(m) => Error::Provider(format!(
            "{m} — Gmail/Outlook need an app password or OAuth, not the account password"
        )),
        other => other,
    }
}

/// Escape a value for an IMAP quoted string.
fn imap_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// IMAP SEARCH criteria for `q` (mailbox selection happens separately).
fn search_query(q: &EmailSearch) -> String {
    let mut parts: Vec<String> = vec![];
    if q.unread_only {
        parts.push("UNSEEN".into());
    }
    if let Some(f) = &q.from {
        parts.push(format!("FROM {}", imap_quote(f)));
    }
    if let Some(t) = &q.query {
        parts.push(format!("TEXT {}", imap_quote(t)));
    }
    if let Some(d) = &q.since {
        parts.push(format!("SINCE {}", d.format("%d-%b-%Y")));
    }
    if parts.is_empty() {
        "ALL".into()
    } else {
        parts.join(" ")
    }
}

fn addr_text(a: &Addr<'_>) -> EmailAddress {
    EmailAddress {
        name: a.name.as_ref().map(|n| n.to_string()),
        address: a
            .address
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_default(),
    }
}

fn addr_list(h: Option<&mail_parser::Address<'_>>) -> Vec<EmailAddress> {
    let Some(a) = h else {
        return vec![];
    };
    match a {
        mail_parser::Address::List(list) => list.iter().map(addr_text).collect(),
        mail_parser::Address::Group(groups) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .map(addr_text)
            .collect(),
    }
}

fn first_addr(h: Option<&mail_parser::Address<'_>>) -> EmailAddress {
    addr_list(h).into_iter().next().unwrap_or(EmailAddress {
        name: None,
        address: String::new(),
    })
}

fn message_date(m: &mail_parser::Message<'_>) -> Timestamp {
    m.date()
        .and_then(|d| chrono::DateTime::from_timestamp(d.to_timestamp(), 0))
        .unwrap_or_else(now)
}

fn summary_from_headers(uid: u32, flags: &[imap::types::Flag<'_>], raw: &[u8]) -> EmailSummary {
    let parsed = MessageParser::default().parse_headers(raw);
    let (subject, from, to, date, msg_id) = match &parsed {
        Some(m) => (
            m.subject().unwrap_or("").to_string(),
            first_addr(m.from()),
            addr_list(m.to()),
            message_date(m),
            m.message_id().unwrap_or("").to_string(),
        ),
        None => (
            String::new(),
            EmailAddress {
                name: None,
                address: String::new(),
            },
            vec![],
            now(),
            String::new(),
        ),
    };
    let labels: Vec<String> = flags
        .iter()
        .filter_map(|f| match f {
            imap::types::Flag::Custom(c) => Some(c.to_string()),
            _ => None,
        })
        .collect();
    EmailSummary {
        id: uid.to_string(),
        thread_id: msg_id,
        from,
        to,
        snippet: String::new(), // header-only fetch; body comes with read()
        subject,
        received_at: date,
        labels,
        has_attachments: false, // needs BODYSTRUCTURE; see read()
    }
}

fn message_from_raw(uid: u32, raw: &[u8], mailbox: &str) -> Result<EmailMessage> {
    let m = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Provider("unparseable message".into()))?;
    let summary = EmailSummary {
        id: uid.to_string(),
        thread_id: m.message_id().unwrap_or("").to_string(),
        from: first_addr(m.from()),
        to: addr_list(m.to()),
        subject: m.subject().unwrap_or("").to_string(),
        snippet: m
            .body_text(0)
            .map(|t| t.chars().take(200).collect())
            .unwrap_or_default(),
        received_at: message_date(&m),
        labels: vec![],
        has_attachments: m.attachment_count() > 0,
    };
    let body_text = m
        .body_text(0)
        .map(|t| t.to_string())
        .or_else(|| m.body_html(0).map(|h| strip_tags(&h)));
    let attachments = m
        .attachments()
        .map(|a| EmailAttachment {
            filename: a
                .attachment_name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "attachment".into()),
            mime: a
                .content_type()
                .map(|c| {
                    format!(
                        "{}/{}",
                        c.c_type,
                        c.c_subtype.as_deref().unwrap_or("octet-stream")
                    )
                })
                .unwrap_or_default(),
            // Attachment bytes stay server-side; a fetch op is roadmap.
            blob: format!("imap:{mailbox}/{uid}"),
            size_bytes: part_body_len(a) as u64,
        })
        .collect();
    Ok(EmailMessage {
        summary,
        body_text,
        body_html_blob: None,
        attachments,
    })
}

fn part_body_len(p: &mail_parser::MessagePart<'_>) -> usize {
    use mail_parser::PartType;
    match &p.body {
        PartType::Text(t) | PartType::Html(t) => t.len(),
        PartType::Binary(b) | PartType::InlineBinary(b) => b.len(),
        PartType::Message(m) => m.raw_message.len(),
        _ => 0,
    }
}

/// Minimal HTML→text for messages with no text/plain part.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// RFC822 source for a draft — enough for APPEND into a drafts mailbox.
fn build_draft_message(d: &Draft, from_addr: &str) -> String {
    let encode_addr = |a: &EmailAddress| match &a.name {
        Some(n) => format!("{} <{}>", n, a.address),
        None => a.address.clone(),
    };
    let mut s = String::with_capacity(d.body.len() + 512);
    s.push_str(&format!("From: {from_addr}\r\n"));
    s.push_str(&format!(
        "To: {}\r\n",
        d.to.iter().map(encode_addr).collect::<Vec<_>>().join(", ")
    ));
    if !d.cc.is_empty() {
        s.push_str(&format!(
            "Cc: {}\r\n",
            d.cc.iter().map(encode_addr).collect::<Vec<_>>().join(", ")
        ));
    }
    // RFC2047-encode non-ASCII subjects.
    if d.subject.is_ascii() {
        s.push_str(&format!("Subject: {}\r\n", d.subject));
    } else {
        use base64::Engine;
        s.push_str(&format!(
            "Subject: =?UTF-8?B?{}?=\r\n",
            base64::engine::general_purpose::STANDARD.encode(d.subject.as_bytes())
        ));
    }
    if let Some(rt) = &d.in_reply_to {
        s.push_str(&format!("In-Reply-To: {rt}\r\nReferences: {rt}\r\n"));
    }
    s.push_str("MIME-Version: 1.0\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\n");
    s.push_str(&d.body.replace("\r\n", "\n").replace('\n', "\r\n"));
    s
}

#[async_trait]
impl EmailProvider for ImapProvider {
    fn id(&self) -> &'static str {
        "imap"
    }

    async fn search(&self, q: &EmailSearch) -> Result<Vec<EmailSummary>> {
        let q = q.clone();
        self.with_session(move |s, cfg| {
            // A label search selects that mailbox (Gmail labels-as-mailboxes).
            let mailbox = q.label.as_deref().unwrap_or(&cfg.mailbox);
            s.select(mailbox).map_err(err)?;
            let uids = s.uid_search(search_query(&q)).map_err(err)?;
            let mut ids: Vec<u32> = uids.into_iter().collect();
            ids.sort_unstable_by(|a, b| b.cmp(a)); // newest first
            ids.truncate(q.limit.max(1) as usize);
            if ids.is_empty() {
                return Ok(vec![]);
            }
            let set = ids
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let fetches = s
                .uid_fetch(
                    &set,
                    "(UID FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT FROM TO DATE MESSAGE-ID)])",
                )
                .map_err(err)?;
            let mut out = Vec::with_capacity(fetches.len());
            for f in fetches.iter() {
                let uid = f.uid.unwrap_or(0);
                if let Some(raw) = f.body() {
                    out.push(summary_from_headers(uid, f.flags(), raw));
                }
            }
            Ok(out)
        })
        .await
    }

    async fn read(&self, id: &str) -> Result<EmailMessage> {
        let uid: u32 = id
            .parse()
            .map_err(|_| Error::InvalidInput(format!("bad message id {id}")))?;
        let id_s = id.to_string();
        self.with_session(move |s, cfg| {
            s.select(&cfg.mailbox).map_err(err)?;
            let fetches = s.uid_fetch(uid.to_string(), "(UID RFC822)").map_err(err)?;
            let f = fetches
                .iter()
                .next()
                .ok_or_else(|| Error::NotFound(format!("message {id_s}")))?;
            let raw = f
                .body()
                .ok_or_else(|| Error::Provider("empty message body".into()))?;
            message_from_raw(uid, raw, &cfg.mailbox)
        })
        .await
    }

    async fn create_draft(&self, draft: &Draft) -> Result<String> {
        let msg = build_draft_message(draft, &self.cfg.user);
        self.with_session(move |s, cfg| {
            s.append(&cfg.drafts_mailbox, msg.as_bytes())
                .flag("\\Draft".into())
                .finish()
                .map_err(err)?;
            Ok(format!("draft in {}", cfg.drafts_mailbox))
        })
        .await
    }

    async fn send(&self, _draft: &Draft) -> Result<()> {
        Err(Error::Provider(
            "IMAP cannot send — the draft API is the send path by design \
             (SMTP via lettre is roadmap)"
                .into(),
        ))
    }

    async fn archive(&self, id: &str) -> Result<()> {
        let uid: u32 = id
            .parse()
            .map_err(|_| Error::InvalidInput(format!("bad message id {id}")))?;
        self.with_session(move |s, cfg| {
            s.select(&cfg.mailbox).map_err(err)?;
            s.uid_mv(uid.to_string(), &cfg.archive_mailbox)
                .map_err(err)?;
            Ok(())
        })
        .await
    }

    async fn label(&self, id: &str, label: &str) -> Result<()> {
        let uid: u32 = id
            .parse()
            .map_err(|_| Error::InvalidInput(format!("bad message id {id}")))?;
        let label = label.to_string();
        self.with_session(move |s, cfg| {
            s.select(&cfg.mailbox).map_err(err)?;
            // Gmail-style: labels are mailboxes; COPY applies the label.
            s.uid_copy(uid.to_string(), &label).map_err(err)?;
            Ok(())
        })
        .await
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let uid: u32 = id
            .parse()
            .map_err(|_| Error::InvalidInput(format!("bad message id {id}")))?;
        self.with_session(move |s, cfg| {
            s.select(&cfg.mailbox).map_err(err)?;
            s.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
                .map_err(err)?;
            s.expunge().map_err(err)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_query_builds_safely() {
        let q = EmailSearch {
            query: Some("quarterly \"report\"".into()),
            from: Some("boss@corp.example".into()),
            unread_only: true,
            ..Default::default()
        };
        assert_eq!(
            search_query(&q),
            "UNSEEN FROM \"boss@corp.example\" TEXT \"quarterly \\\"report\\\"\""
        );
        assert_eq!(search_query(&EmailSearch::default()), "ALL");
    }

    #[test]
    fn draft_message_is_wellformed() {
        let d = Draft {
            to: vec![EmailAddress {
                name: Some("Bob".into()),
                address: "bob@x.example".into(),
            }],
            cc: vec![],
            subject: "Réunion".into(),
            body: "line1\nline2".into(),
            in_reply_to: Some("<m1@x>".into()),
        };
        let msg = build_draft_message(&d, "me@x.example");
        assert!(msg.contains("From: me@x.example\r\n"));
        assert!(msg.contains("To: Bob <bob@x.example>\r\n"));
        assert!(msg.contains("Subject: =?UTF-8?B?"));
        assert!(msg.contains("In-Reply-To: <m1@x>\r\n"));
        assert!(msg.contains("\r\n\r\nline1\r\nline2"));
    }

    #[test]
    fn strips_html_to_text() {
        assert_eq!(strip_tags("<p>Hello <b>world</b></p>"), "Hello world");
    }
}

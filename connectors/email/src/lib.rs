//! Email connector interface — vendor-neutral.
//!
//! Gmail, Outlook/Microsoft Graph, and plain IMAP adapters all implement
//! `EmailProvider`; the core only ever sees this trait. Adapters map their
//! API onto these types and declare which `Permission` each op needs.

pub mod imap;
pub use imap::{ImapConfig, ImapProvider};

use async_trait::async_trait;
use pai_core::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailSummary {
    pub id: String,
    pub thread_id: String,
    pub from: EmailAddress,
    pub to: Vec<EmailAddress>,
    pub subject: String,
    /// Short preview — safe to surface.
    pub snippet: String,
    pub received_at: Timestamp,
    pub labels: Vec<String>,
    pub has_attachments: bool,
}

/// A full message body is always `TrustLevel::Untrusted` — email text is
/// data to summarize/act on, never instructions to obey.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailMessage {
    pub summary: EmailSummary,
    pub body_text: Option<String>,
    pub body_html_blob: Option<String>,
    pub attachments: Vec<EmailAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailAttachment {
    pub filename: String,
    pub mime: String,
    pub blob: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmailSearch {
    pub query: Option<String>,
    pub from: Option<String>,
    pub since: Option<Timestamp>,
    pub label: Option<String>,
    pub unread_only: bool,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
}

/// Vendor-neutral email operations. Each method's required permission is
/// documented next to it — the tool layer enforces before dispatch.
#[async_trait]
pub trait EmailProvider: Send + Sync {
    fn id(&self) -> &'static str;

    /// EMAIL_SEARCH
    async fn search(&self, q: &EmailSearch) -> Result<Vec<EmailSummary>>;
    /// EMAIL_READ
    async fn read(&self, id: &str) -> Result<EmailMessage>;
    /// EMAIL_DRAFT
    async fn create_draft(&self, draft: &Draft) -> Result<String>;
    /// EMAIL_SEND — the runtime gates this behind user approval by default.
    async fn send(&self, draft: &Draft) -> Result<()>;
    /// EMAIL_ARCHIVE
    async fn archive(&self, id: &str) -> Result<()>;
    /// EMAIL_LABEL
    async fn label(&self, id: &str, label: &str) -> Result<()>;
    /// EMAIL_DELETE — approval by default; can be NEVER_ALLOW.
    async fn delete(&self, id: &str) -> Result<()>;
}

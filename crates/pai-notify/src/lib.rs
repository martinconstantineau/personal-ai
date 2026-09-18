//! Notification inbox + delivery channels — the proactive surface.
//!
//! Every publish lands in the `notifications` table (the durable inbox);
//! `ntf/<id>` sync objects roam it across paired devices like tasks.
//! *External* delivery — email-to-self or a webhook POST — is opt-in:
//! channels only fire for targets the user wrote into `notify.json`.
//! Configuration is the consent; a `notify.send` tool call can't reach
//! a channel that isn't configured.
//!
//! `NotifySink` is the narrow capability handed to tools (`ToolContext`)
//! and task handlers — publish + external fan-out, nothing else.

use pai_connector_email::{Draft, EmailAddress, EmailProvider};
use pai_core::*;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// External delivery targets, loaded from `<data_dir>/notify.json`.
/// Empty/absent = inbox-only — nothing leaves the device.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifyConfig {
    /// Send a copy to this address via the configured email connector.
    #[serde(default)]
    pub email_to: Option<String>,
    /// POST `{title, body, source, ts}` JSON to this URL (e.g. a
    /// self-hosted ntfy topic).
    #[serde(default)]
    pub webhook_url: Option<String>,
}

pub fn load_config(data_dir: &Path) -> Result<NotifyConfig> {
    let p = data_dir.join("notify.json");
    if !p.exists() {
        return Ok(NotifyConfig::default());
    }
    let raw = std::fs::read_to_string(&p)
        .map_err(|e| Error::InvalidInput(format!("notify.json: {e}")))?;
    serde_json::from_str(&raw).map_err(|e| Error::InvalidInput(format!("notify.json: {e}")))
}

/// The narrow notify capability — what tools and task handlers get.
#[async_trait::async_trait]
pub trait NotifySink: Send + Sync {
    /// Write an inbox row; returns the notification id.
    fn publish(&self, title: &str, body: &str, source: &str, scope: SyncScope) -> Result<String>;
    /// Fan out to the user's configured external channels; returns the
    /// channel names that accepted the delivery.
    async fn deliver_external(&self, title: &str, body: &str, source: &str) -> Result<Vec<String>>;
}

/// Inbox + configured external channels. Construct per-app where the
/// store, email connector, and data dir are all in scope.
pub struct StoreNotifySink {
    pub store: Arc<pai_storage::Store>,
    pub config: NotifyConfig,
    /// Required for the `email` channel — absent means email deliveries
    /// are reported as unavailable rather than attempted.
    pub email: Option<Arc<dyn EmailProvider>>,
}

#[async_trait::async_trait]
impl NotifySink for StoreNotifySink {
    fn publish(&self, title: &str, body: &str, source: &str, scope: SyncScope) -> Result<String> {
        store::publish(&self.store, title, body, source, scope)
    }

    /// Every configured channel is attempted — a broken `email_to`
    /// doesn't starve a working webhook. Err only when channels were
    /// configured and *none* delivered.
    async fn deliver_external(&self, title: &str, body: &str, source: &str) -> Result<Vec<String>> {
        let mut fired = Vec::new();
        let mut first_err: Option<Error> = None;
        let mut configured = 0usize;
        if let Some(addr) = &self.config.email_to {
            configured += 1;
            match &self.email {
                Some(provider) => {
                    match provider
                        .send(&Draft {
                            to: vec![EmailAddress {
                                name: None,
                                address: addr.clone(),
                            }],
                            cc: vec![],
                            subject: format!("pai: {title}"),
                            body: format!("{body}\n\n— {source}"),
                            in_reply_to: None,
                        })
                        .await
                    {
                        Ok(()) => fired.push("email".to_string()),
                        Err(e) => {
                            first_err = Some(Error::Other(format!("email channel: {e}")));
                        }
                    }
                }
                None => {
                    first_err = Some(Error::InvalidInput(
                        "email_to configured but no email provider".into(),
                    ));
                }
            }
        }
        if let Some(url) = &self.config.webhook_url {
            configured += 1;
            match reqwest::Client::new()
                .post(url)
                .timeout(std::time::Duration::from_secs(10))
                .json(&serde_json::json!({
                    "title": title,
                    "body": body,
                    "source": source,
                    "ts": now().to_rfc3339(),
                }))
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(_) => fired.push("webhook".to_string()),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(Error::Other(format!("webhook channel: {e}")));
                    }
                }
            }
        }
        match (fired.is_empty(), configured > 0, first_err) {
            (true, true, Some(e)) => Err(e),
            _ => Ok(fired),
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub mod store {
    use super::*;
    use rusqlite::params;

    #[derive(Debug, Clone)]
    pub struct Notification {
        pub id: String,
        pub title: String,
        pub body: String,
        pub source: String,
        pub channel: String,
        pub created_at: Timestamp,
        pub read_at: Option<Timestamp>,
        pub sync_scope: String,
        pub updated_at: Timestamp,
    }

    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Notification> {
        Ok(Notification {
            id: r.get(0)?,
            title: r.get(1)?,
            body: r.get(2)?,
            source: r.get(3)?,
            channel: r.get(4)?,
            created_at: pai_storage::parse_ts(&r.get::<_, String>(5)?),
            read_at: r
                .get::<_, Option<String>>(6)?
                .map(|s| pai_storage::parse_ts(&s)),
            sync_scope: r.get(7)?,
            updated_at: pai_storage::parse_ts(&r.get::<_, String>(8)?),
        })
    }

    const COLS: &str =
        "id, title, body, source, channel, created_at, read_at, sync_scope, updated_at";

    /// Inbox row — the durable record every publish starts from.
    pub fn publish(
        store: &Arc<pai_storage::Store>,
        title: &str,
        body: &str,
        source: &str,
        scope: SyncScope,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let scope_s = serde_json::to_string(&scope)
            .unwrap_or_else(|_| "\"synchronized\"".into())
            .trim_matches('"')
            .to_string();
        let now_s = pai_storage::ts(&now());
        store.with_conn(|c| {
            c.execute(
                &format!(
                    "INSERT INTO notifications({COLS}, deleted)
                     VALUES(?1,?2,?3,?4,'inbox',?5,NULL,?6,?5,0)"
                ),
                params![id, title, body, source, now_s, scope_s],
            )
        })?;
        Ok(id)
    }

    /// Newest first. `unread_only` filters to rows with no `read_at`.
    pub fn list(
        store: &Arc<pai_storage::Store>,
        unread_only: bool,
        limit: usize,
    ) -> Result<Vec<Notification>> {
        store.with_conn(|c| {
            let sql = format!(
                "SELECT {COLS} FROM notifications WHERE deleted=0 {}
                 ORDER BY created_at DESC LIMIT ?1",
                if unread_only {
                    "AND read_at IS NULL"
                } else {
                    ""
                }
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params![limit as i64], row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    pub fn get(store: &Arc<pai_storage::Store>, id: &str) -> Result<Option<Notification>> {
        store.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {COLS} FROM notifications
                 WHERE deleted=0 AND (id=?1 OR substr(id,1,8)=?1)"
            ))?;
            let mut rows = stmt.query_map(params![id], row)?;
            rows.next().transpose()
        })
    }

    /// Mark one notification read (`read_at` syncs to peers).
    pub fn mark_read(store: &Arc<pai_storage::Store>, id: &str) -> Result<bool> {
        let n = store.with_conn(|c| {
            c.execute(
                "UPDATE notifications SET read_at=?2, updated_at=?2
                 WHERE deleted=0 AND (id=?1 OR substr(id,1,8)=?1) AND read_at IS NULL",
                params![id, pai_storage::ts(&now())],
            )
        })?;
        Ok(n > 0)
    }

    pub fn mark_all_read(store: &Arc<pai_storage::Store>) -> Result<usize> {
        store.with_conn(|c| {
            c.execute(
                "UPDATE notifications SET read_at=?1, updated_at=?1
                 WHERE deleted=0 AND read_at IS NULL",
                params![pai_storage::ts(&now())],
            )
        })
    }

    pub fn unread_count(store: &Arc<pai_storage::Store>) -> Result<usize> {
        store
            .with_conn(|c| {
                c.query_row(
                    "SELECT count(*) FROM notifications WHERE deleted=0 AND read_at IS NULL",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .map(|n| n as usize)
    }

    /// Soft-delete — the tombstone propagates.
    pub fn remove(store: &Arc<pai_storage::Store>, id: &str) -> Result<bool> {
        let n = store.with_conn(|c| {
            c.execute(
                "UPDATE notifications SET deleted=1, updated_at=?2
                 WHERE deleted=0 AND (id=?1 OR substr(id,1,8)=?1)",
                params![id, pai_storage::ts(&now())],
            )
        })?;
        Ok(n > 0)
    }
}

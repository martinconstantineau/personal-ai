//! `pai notify …` — the proactive inbox + configured delivery channels.

use crate::ctx::Ctx;
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum NotifyCmd {
    /// List notifications, newest first.
    List {
        /// Only unread rows.
        #[arg(long)]
        unread: bool,
    },
    /// Show a notification and mark it read.
    Open { id: String },
    /// Publish a notification to the inbox.
    Send {
        title: String,
        body: Option<String>,
        /// Also deliver via notify.json's external channels
        /// (email_to / webhook_url).
        #[arg(long)]
        external: bool,
    },
    /// Mark every notification read.
    Clear,
    /// Soft-delete a notification (tombstone propagates).
    Remove { id: String },
    /// Configure external channels — writes notify.json.
    Configure {
        #[arg(long)]
        email_to: Option<String>,
        #[arg(long)]
        webhook: Option<String>,
    },
    /// Deliver a test notification through the configured external
    /// channels — verifies notify.json actually reaches you.
    Test,
}

/// `pai notify ...` — the proactive inbox + configured delivery channels.
/// `pai notify` — the proactive inbox + delivery channels.
pub(crate) async fn run(cmd: &NotifyCmd, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    run_notify_cmds(cmd, ctx, cfg).await
}

async fn run_notify_cmds(cmd: &NotifyCmd, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    use pai_notify::{store as ns, NotifySink, StoreNotifySink};
    let sink = StoreNotifySink {
        store: ctx.store.clone(),
        config: pai_notify::load_config(&cfg.data_dir)?,
        email: ctx.email.clone(),
    };
    match cmd {
        NotifyCmd::List { unread } => {
            for n in ns::list(&ctx.store, *unread, 50)? {
                println!(
                    "{:.8}  {} {:<40} {}",
                    n.id,
                    if n.read_at.is_some() { " " } else { "●" },
                    n.title,
                    n.created_at,
                );
            }
            let unread = ns::unread_count(&ctx.store)?;
            if unread > 0 {
                println!("{unread} unread");
            }
        }
        NotifyCmd::Open { id } => {
            let n = ns::get(&ctx.store, id)?
                .ok_or_else(|| Error::NotFound(format!("notification {id}")))?;
            println!(
                "{}

{}

— {} ({})",
                n.title, n.body, n.source, n.created_at
            );
            ns::mark_read(&ctx.store, &n.id)?;
        }
        NotifyCmd::Send {
            title,
            body,
            external,
        } => {
            let body_s = body.clone().unwrap_or_default();
            let id = sink.publish(title, &body_s, "cli", SyncScope::Synchronized)?;
            if *external {
                let fired = sink.deliver_external(title, &body_s, "cli").await?;
                println!(
                    "sent {:.8} → inbox{}",
                    id,
                    if fired.is_empty() {
                        String::new()
                    } else {
                        format!(" +{}", fired.join("+"))
                    }
                );
            } else {
                println!("sent {:.8} → inbox", id);
            }
        }
        NotifyCmd::Clear => {
            println!("{} marked read", ns::mark_all_read(&ctx.store)?);
        }
        NotifyCmd::Remove { id } => {
            if ns::remove(&ctx.store, id)? {
                println!("removed {id}");
            } else {
                println!("no notification {id}");
            }
        }
        NotifyCmd::Configure { email_to, webhook } => {
            let path = cfg.data_dir.join("notify.json");
            let mut c = pai_notify::load_config(&cfg.data_dir)?;
            if let Some(e) = email_to {
                c.email_to = if e.is_empty() { None } else { Some(e.clone()) };
            }
            if let Some(w) = webhook {
                c.webhook_url = if w.is_empty() { None } else { Some(w.clone()) };
            }
            let json =
                serde_json::to_string_pretty(&c).map_err(|e| Error::InvalidInput(e.to_string()))?;
            std::fs::write(&path, json).map_err(|e| Error::Storage(e.to_string()))?;
            println!(
                "notify.json: email_to={} webhook={}",
                c.email_to.as_deref().unwrap_or("(none)"),
                c.webhook_url.as_deref().unwrap_or("(none)")
            );
        }
        NotifyCmd::Test => {
            let fired = sink
                .deliver_external("pai test", "notification channel check", "cli:test")
                .await?;
            if fired.is_empty() {
                println!("no external channels configured — see `pai notify configure`");
            } else {
                println!("delivered via: {}", fired.join(", "));
            }
        }
    }
    Ok(())
}

//! `pai email …` — IMAP/SMTP connector: interactive configure (password
//! or OAuth device flow) plus direct mailbox operations.

use crate::util::{read_prompt, wait_device_grant};
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum EmailCmd {
    /// Configure the IMAP account: writes email.json; the password goes
    /// to the OS keystore (`email:<user>`), never the file.
    /// `--oauth google|microsoft` uses the device-authorization flow
    /// instead — a refresh token replaces the app password.
    Configure {
        /// OAuth2 device flow for Gmail/Outlook (needs a client_id from
        /// your own cloud app registration).
        #[arg(long)]
        oauth: Option<String>,
    },
    /// Show the configured account (never prints the password).
    Status,
    /// Search messages.
    Search {
        query: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value = "10")]
        limit: u32,
    },
    /// Read one message body by id.
    Read { id: String },
    /// Create a draft (the safe send path).
    Draft {
        #[arg(long)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        in_reply_to: Option<String>,
    },
    /// Send immediately via SMTP (needs the `smtp` block in email.json —
    /// `pai email configure` writes one). Drafts remain the default.
    Send {
        #[arg(long)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        in_reply_to: Option<String>,
    },
    /// Archive a message by id.
    Archive { id: String },
    /// Apply a label/mailbox to a message by id.
    Label { id: String, label: String },
    /// Delete a message by id.
    Delete { id: String },
}

async fn email_provider(cfg: &pai_config::Config) -> Result<pai_connector_email::ImapProvider> {
    let c = pai_connector_email::ImapConfig::load(&cfg.data_dir)?.ok_or_else(|| {
        Error::InvalidInput("no email account — run `pai email configure`".into())
    })?;
    Ok(pai_connector_email::ImapProvider::new(c))
}

/// `pai email` — IMAP/SMTP connector ops.
pub(crate) async fn run(cmd: &EmailCmd, cfg: &pai_config::Config) -> Result<()> {
    run_email_cmds(cmd, cfg).await
}

async fn run_email_cmds(cmd: &EmailCmd, cfg: &pai_config::Config) -> Result<()> {
    use pai_connector_email::EmailProvider;
    match cmd {
        EmailCmd::Configure { oauth } => {
            let read = read_prompt;
            let host = read("IMAP host", "imap.gmail.com")?;
            let port: u16 = read("Port", "993")?
                .parse()
                .map_err(|_| Error::InvalidInput("bad port".into()))?;
            let user = read("User (email address)", "")?;
            if user.is_empty() {
                return Err(Error::InvalidInput("user is required".into()));
            }
            let mailbox = read("Mailbox", "INBOX")?;
            let drafts = read("Drafts mailbox", "[Gmail]/Drafts")?;
            let archive = read("Archive mailbox", "[Gmail]/All Mail")?;
            // Optional SMTP submission — empty host keeps drafts-only.
            let smtp_host = read("SMTP host (empty = drafts only)", "smtp.gmail.com")?;
            let smtp = if smtp_host.is_empty() {
                None
            } else {
                let smtp_port: u16 = read("SMTP port", "465")?
                    .parse()
                    .map_err(|_| Error::InvalidInput("bad smtp port".into()))?;
                let smtp_tls = match read("SMTP TLS (tls/starttls/none)", "tls")?.as_str() {
                    "tls" => pai_connector_email::SmtpTls::Tls,
                    "starttls" => pai_connector_email::SmtpTls::StartTls,
                    "none" => pai_connector_email::SmtpTls::None,
                    other => {
                        return Err(Error::InvalidInput(format!(
                            "bad tls mode {other:?} — tls|starttls|none"
                        )))
                    }
                };
                Some(pai_connector_email::SmtpConfig {
                    host: smtp_host,
                    port: smtp_port,
                    tls: smtp_tls,
                    user: None, // same login as IMAP
                })
            };
            let oauth_cfg = match oauth.as_deref() {
                None => None,
                Some(provider) => {
                    let client_id = read("OAuth client_id (from your app registration)", "")?;
                    if client_id.is_empty() {
                        return Err(Error::InvalidInput(
                            "client_id is required — register an app first".into(),
                        ));
                    }
                    let tenant = if provider == "microsoft" {
                        Some(read("Tenant", "common")?)
                    } else {
                        None
                    };
                    Some(pai_connector_email::OAuthConfig {
                        provider: provider.to_string(),
                        client_id,
                        tenant,
                        device_url: None,
                        token_url: None,
                        scopes: None,
                    })
                }
            };
            let password = if oauth_cfg.is_none() {
                rpassword::prompt_password("Password (app password for Gmail/Outlook): ")
                    .map_err(|e| Error::Other(e.to_string()))?
            } else {
                String::new()
            };
            let c = pai_connector_email::ImapConfig {
                host,
                port,
                user: user.clone(),
                mailbox,
                drafts_mailbox: drafts,
                archive_mailbox: archive,
                smtp,
                oauth: oauth_cfg,
            };
            if let Some(oc) = &c.oauth {
                // Device-authorization flow: print the code, poll until
                // the user authorizes (or the grant expires).
                let grant = pai_connector_email::oauth::device_flow(oc).await?;
                let tokens = wait_device_grant(oc, &grant).await?;
                let refresh = tokens.refresh_token.ok_or_else(|| {
                    Error::Provider(
                        "oauth grant returned no refresh_token — add `offline_access` scope".into(),
                    )
                })?;
                c.save(&cfg.data_dir)?;
                if pai_connector_email::oauth::store_refresh_token(&user, &c.host, &refresh) {
                    println!(
                        "\nrefresh token stored in OS keystore (email-oauth:{user}@{})",
                        c.host
                    );
                } else {
                    return Err(Error::Other(
                        "keystore unavailable — cannot persist the refresh token".into(),
                    ));
                }
            } else {
                c.save(&cfg.data_dir)?;
                if password.is_empty() {
                    println!("no password stored — set PAI_EMAIL_PASSWORD at run time");
                } else if pai_connector_email::imap::store_password(&user, &c.host, &password) {
                    println!("password stored in OS keystore (email:{user}@{})", c.host);
                } else {
                    println!("keystore unavailable — set PAI_EMAIL_PASSWORD at run time");
                }
            }
            println!("account written to {}/email.json", cfg.data_dir.display());
        }
        EmailCmd::Status => match pai_connector_email::ImapConfig::load(&cfg.data_dir)? {
            Some(c) => {
                println!("imap://{}:{}/{}", c.user, c.host, c.mailbox);
                println!(
                    "drafts: {}  archive: {}",
                    c.drafts_mailbox, c.archive_mailbox
                );
                match &c.smtp {
                    Some(s) => println!("smtp: {}:{} ({:?}) — send enabled", s.host, s.port, s.tls),
                    None => println!("smtp: not configured — drafts only"),
                }
                match &c.oauth {
                    Some(o) => {
                        let has_rt =
                            pai_identity::keystore::load(&format!("email-oauth:{}", c.user))
                                .is_some();
                        println!(
                            "auth: oauth2 ({}) — refresh token {}",
                            o.provider,
                            if has_rt { "available" } else { "MISSING" }
                        );
                    }
                    None => {
                        let pw =
                            pai_connector_email::imap::resolve_password(&c.user, &c.host).is_ok();
                        println!(
                            "auth: password — {}",
                            if pw { "available" } else { "MISSING" }
                        );
                    }
                }
            }
            None => println!("not configured — run `pai email configure`"),
        },
        EmailCmd::Search {
            query,
            from,
            label,
            unread,
            limit,
        } => {
            let p = email_provider(cfg).await?;
            let hits = p
                .search(&pai_connector_email::EmailSearch {
                    query: query.clone(),
                    from: from.clone(),
                    label: label.clone(),
                    unread_only: *unread,
                    limit: *limit,
                    ..Default::default()
                })
                .await?;
            for m in &hits {
                println!(
                    "  {:>6}  {:<40} {:<45} {}",
                    m.id,
                    m.from.address,
                    m.subject,
                    m.received_at.format("%Y-%m-%d")
                );
            }
            if hits.is_empty() {
                println!("no messages");
            }
        }
        EmailCmd::Read { id } => {
            let m = email_provider(cfg).await?.read(id).await?;
            println!("from: {}", m.summary.from.address);
            println!(
                "to:   {}",
                m.summary
                    .to
                    .iter()
                    .map(|a| a.address.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("date: {}", m.summary.received_at.format("%Y-%m-%d %H:%M"));
            println!("subject: {}\n", m.summary.subject);
            println!("{}", m.body_text.unwrap_or_else(|| "(no text body)".into()));
            for a in &m.attachments {
                println!(
                    "  attachment: {} ({}, {} bytes)",
                    a.filename, a.mime, a.size_bytes
                );
            }
        }
        EmailCmd::Draft {
            to,
            cc,
            subject,
            body,
            in_reply_to,
        } => {
            let addr = |a: &String| pai_connector_email::EmailAddress {
                name: None,
                address: a.clone(),
            };
            let id = email_provider(cfg)
                .await?
                .create_draft(&pai_connector_email::Draft {
                    to: to.iter().map(addr).collect(),
                    cc: cc.iter().map(addr).collect(),
                    subject: subject.clone(),
                    body: body.clone(),
                    in_reply_to: in_reply_to.clone(),
                })
                .await?;
            println!("{id}");
        }
        EmailCmd::Send {
            to,
            cc,
            subject,
            body,
            in_reply_to,
        } => {
            let addr = |a: &String| pai_connector_email::EmailAddress {
                name: None,
                address: a.clone(),
            };
            email_provider(cfg)
                .await?
                .send(&pai_connector_email::Draft {
                    to: to.iter().map(addr).collect(),
                    cc: cc.iter().map(addr).collect(),
                    subject: subject.clone(),
                    body: body.clone(),
                    in_reply_to: in_reply_to.clone(),
                })
                .await?;
            println!("sent");
        }
        EmailCmd::Archive { id } => {
            email_provider(cfg).await?.archive(id).await?;
            println!("archived {id}");
        }
        EmailCmd::Label { id, label } => {
            email_provider(cfg).await?.label(id, label).await?;
            println!("labeled {id} → {label}");
        }
        EmailCmd::Delete { id } => {
            email_provider(cfg).await?.delete(id).await?;
            println!("deleted {id}");
        }
    }
    Ok(())
}

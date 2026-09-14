//! V2b tests: email connector tools, permission gating, config lifecycle.
//! Uses a mock EmailProvider — the real ImapProvider needs a live server.

use pai_connector_email::*;
use pai_core::*;
use pai_permissions::{Permission, PolicyTable, RiskLevel};
use pai_tools::{Tool, ToolContext};

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2b-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// In-memory provider: canned INBOX, records mutating calls.
struct MockMail {
    deleted: std::sync::Mutex<Vec<String>>,
}

impl MockMail {
    fn summary(id: &str, subject: &str) -> EmailSummary {
        EmailSummary {
            id: id.into(),
            thread_id: "t1".into(),
            from: EmailAddress {
                name: Some("Alice".into()),
                address: "alice@x.test".into(),
            },
            to: vec![EmailAddress {
                name: None,
                address: "me@y.test".into(),
            }],
            subject: subject.into(),
            snippet: "preview…".into(),
            received_at: now(),
            labels: vec![],
            has_attachments: false,
        }
    }
}

#[async_trait::async_trait]
impl EmailProvider for MockMail {
    fn id(&self) -> &'static str {
        "mock"
    }
    async fn search(&self, q: &EmailSearch) -> Result<Vec<EmailSummary>> {
        let all = vec![
            Self::summary("1", "dentist monday"),
            Self::summary("2", "lunch friday"),
        ];
        Ok(match &q.query {
            Some(needle) => all
                .into_iter()
                .filter(|m| m.subject.contains(needle.as_str()))
                .collect(),
            None => all,
        })
    }
    async fn read(&self, id: &str) -> Result<EmailMessage> {
        Ok(EmailMessage {
            summary: Self::summary(id, "dentist monday"),
            body_text: Some("Reminder: appointment 9am".into()),
            body_html_blob: None,
            attachments: vec![],
        })
    }
    async fn create_draft(&self, _d: &Draft) -> Result<String> {
        Ok("draft-42".into())
    }
    async fn send(&self, _d: &Draft) -> Result<()> {
        Err(Error::Provider("mock cannot send".into()))
    }
    async fn archive(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn label(&self, _id: &str, _label: &str) -> Result<()> {
        Ok(())
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.deleted.lock().unwrap().push(id.into());
        Ok(())
    }
}

fn ctx<'a>(mail: Option<&'a dyn EmailProvider>) -> ToolContext<'a> {
    ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: None,
        email: mail,
        vision: None,
        allowed_roots: &[],
    }
}

#[tokio::test]
async fn email_tools_search_read_draft() {
    let mail = MockMail {
        deleted: Default::default(),
    };
    let c = ctx(Some(&mail));

    let out = pai_tools::EmailSearchTool
        .execute(serde_json::json!({"query": "dentist"}), &c)
        .await
        .unwrap();
    let hits = out.value["results"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["subject"], "dentist monday");

    let out = pai_tools::EmailReadTool
        .execute(serde_json::json!({"id": "1"}), &c)
        .await
        .unwrap();
    assert_eq!(
        out.value["message"]["body_text"],
        "Reminder: appointment 9am"
    );

    let out = pai_tools::EmailDraftTool
        .execute(
            serde_json::json!({
                "to": ["alice@x.test"],
                "subject": "re: dentist",
                "body": "confirmed",
            }),
            &c,
        )
        .await
        .unwrap();
    assert_eq!(out.value["draft_id"], "draft-42");
}

#[tokio::test]
async fn email_tools_without_provider_error_cleanly() {
    let c = ctx(None);
    let err = pai_tools::EmailSearchTool
        .execute(serde_json::json!({}), &c)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("email"));
}

#[tokio::test]
async fn email_mutation_tools_gate_on_permissions() {
    let reg = pai_tools::builtin_registry();
    let expect = [
        ("email.search", Permission::EmailSearch, RiskLevel::Low),
        ("email.read", Permission::EmailRead, RiskLevel::Low),
        ("email.draft", Permission::EmailDraft, RiskLevel::Medium),
        ("email.send", Permission::EmailSend, RiskLevel::High),
        ("email.archive", Permission::EmailArchive, RiskLevel::Medium),
        ("email.label", Permission::EmailLabel, RiskLevel::Medium),
        ("email.delete", Permission::EmailDelete, RiskLevel::High),
    ];
    for (name, perm, risk) in expect {
        let d = reg
            .descriptors()
            .into_iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} not registered"));
        assert_eq!(d.required_permissions, vec![perm], "{name} permission");
        assert_eq!(d.risk, risk, "{name} risk");
    }
}

#[tokio::test]
async fn email_delete_reaches_provider_and_send_fails() {
    let mail = MockMail {
        deleted: Default::default(),
    };
    let c = ctx(Some(&mail));

    pai_tools::EmailDeleteTool
        .execute(serde_json::json!({"id": "7"}), &c)
        .await
        .unwrap();
    assert_eq!(mail.deleted.lock().unwrap().as_slice(), ["7".to_string()]);

    // IMAP-style providers refuse send — drafts are the safe path.
    let err = pai_tools::EmailSendTool
        .execute(
            serde_json::json!({"to": ["a@b.c"], "subject": "s", "body": "b"}),
            &c,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("send"));
}

#[test]
fn email_permissions_have_sensible_defaults() {
    let d = PolicyTable::with_defaults();
    use ExecutionPolicy::*;
    assert_eq!(d.get(Permission::EmailSearch), Some(AlwaysAllow));
    assert_eq!(d.get(Permission::EmailRead), Some(AlwaysAllow));
    assert_eq!(d.get(Permission::EmailDraft), Some(AlwaysAllow));
    assert_eq!(d.get(Permission::EmailSend), Some(AskUser));
    assert_eq!(d.get(Permission::EmailDelete), Some(AskUser));
}

#[test]
fn imap_config_roundtrip() {
    let dir = tmpdir("cfg");
    assert!(ImapConfig::load(&dir).unwrap().is_none());
    let cfg = ImapConfig {
        host: "imap.example.test".into(),
        port: 993,
        user: "me@example.test".into(),
        mailbox: "INBOX".into(),
        archive_mailbox: "Archive".into(),
        drafts_mailbox: "Drafts".into(),
        smtp: None,
        oauth: None,
    };
    cfg.save(&dir).unwrap();
    let back = ImapConfig::load(&dir).unwrap().unwrap();
    assert_eq!(back.host, cfg.host);
    assert_eq!(back.user, cfg.user);
    // No password anywhere in the file — credentials are prompted/keystored.
    let raw = std::fs::read_to_string(dir.join("email.json")).unwrap();
    assert!(!raw.contains("password"));
}

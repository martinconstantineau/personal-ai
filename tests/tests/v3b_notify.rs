//! V3b tests: the notification surface — inbox persistence, `notify.send`
//! tool through the permission-gated sink path, external channels
//! (email-to-self + webhook) gated by notify.json, and `ntf/` sync
//! roundtrips carrying read state + tombstones.

use pai_connector_email::{Draft, EmailMessage, EmailProvider, EmailSearch, EmailSummary};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_notify::{store as ns, NotifySink, StoreNotifySink};
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, FolderTransport};
use pai_tools::{NotifySendTool, Tool, ToolContext};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v3b-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn store(tag: &str) -> Arc<Store> {
    Arc::new(Store::open(&tmpdir(tag), None).unwrap())
}

fn sink(store: &Arc<Store>) -> StoreNotifySink {
    StoreNotifySink {
        store: store.clone(),
        config: pai_notify::NotifyConfig::default(),
        email: None,
    }
}

#[test]
fn inbox_publish_list_read() {
    let s = store("inbox");
    let id = ns::publish(&s, "hello", "world", "test", SyncScope::Synchronized).unwrap();
    let items = ns::list(&s, false, 50).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "hello");
    assert_eq!(ns::unread_count(&s).unwrap(), 1);

    // unread filter + mark_read (prefix ids work)
    assert_eq!(ns::list(&s, true, 50).unwrap().len(), 1);
    assert!(ns::mark_read(&s, &id[..8]).unwrap());
    assert_eq!(ns::unread_count(&s).unwrap(), 0);
    assert_eq!(ns::list(&s, true, 50).unwrap().len(), 0);
    let got = ns::get(&s, &id[..8]).unwrap().unwrap();
    assert!(got.read_at.is_some());

    // tombstone hides it
    assert!(ns::remove(&s, &id).unwrap());
    assert!(ns::list(&s, false, 50).unwrap().is_empty());
}

#[tokio::test]
async fn notify_send_tool_publishes_to_inbox() {
    let s = store("tool");
    let snk = sink(&s);
    let tool = NotifySendTool;
    let ctx = ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: None,
        email: None,
        vision: None,
        notify: Some(&snk),
        apps: None,
        allowed_roots: &[],
    };
    let out = tool
        .execute(serde_json::json!({"title": "t1", "body": "b1"}), &ctx)
        .await
        .unwrap();
    assert!(out.summary.contains("inbox"));
    let items = ns::list(&s, false, 10).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].body, "b1");
}

#[tokio::test]
async fn no_sink_reports_unavailable() {
    let tool = NotifySendTool;
    let ctx = ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: None,
        email: None,
        vision: None,
        notify: None,
        apps: None,
        allowed_roots: &[],
    };
    let err = tool
        .execute(serde_json::json!({"title": "x"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no notification sink"));
}

#[tokio::test]
async fn external_with_no_config_fires_nothing() {
    let s = store("noext");
    let snk = sink(&s);
    let fired = NotifySink::deliver_external(&snk, "t", "b", "test")
        .await
        .unwrap();
    assert!(fired.is_empty());
}

// -- a mock email provider capturing sends -------------------------------

struct CaptureEmail(Mutex<Vec<Draft>>);

#[async_trait::async_trait]
impl EmailProvider for CaptureEmail {
    fn id(&self) -> &'static str {
        "capture"
    }
    async fn search(&self, _q: &EmailSearch) -> Result<Vec<EmailSummary>> {
        Ok(vec![])
    }
    async fn read(&self, _id: &str) -> Result<EmailMessage> {
        Err(Error::InvalidInput("nope".into()))
    }
    async fn create_draft(&self, _d: &Draft) -> Result<String> {
        Ok("draft-1".into())
    }
    async fn send(&self, d: &Draft) -> Result<()> {
        self.0.lock().unwrap().push(d.clone());
        Ok(())
    }
    async fn archive(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn label(&self, _id: &str, _label: &str) -> Result<()> {
        Ok(())
    }
    async fn delete(&self, _id: &str) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn email_channel_sends_to_configured_address() {
    let s = store("email");
    let captured = Arc::new(CaptureEmail(Mutex::new(vec![])));
    let snk = StoreNotifySink {
        store: s.clone(),
        config: pai_notify::NotifyConfig {
            email_to: Some("me@example.com".into()),
            webhook_url: None,
        },
        email: Some(captured.clone()),
    };
    let fired = NotifySink::deliver_external(&snk, "Hi", "Body text", "test")
        .await
        .unwrap();
    assert_eq!(fired, vec!["email"]);
    let sent = captured.0.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to[0].address, "me@example.com");
    assert!(sent[0].subject.contains("Hi"));
    assert!(sent[0].body.contains("Body text"));
}

// -- a loopback HTTP endpoint capturing POSTs -----------------------------

fn mock_webhook() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let got = Arc::new(Mutex::new(Vec::<String>::new()));
    let got2 = got.clone();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut r = BufReader::new(stream);
            let mut line = String::new();
            let mut len = 0usize;
            // headers
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let l = line.trim().to_string();
                if let Some(v) = l.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                if l.is_empty() {
                    break;
                }
            }
            let mut body = vec![0u8; len];
            if r.read_exact(&mut body).is_ok() {
                got2.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).into_owned());
            }
            let mut w = r.into_inner();
            let _ = w.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });
    (format!("http://127.0.0.1:{port}/hook"), got)
}

#[tokio::test]
async fn webhook_channel_posts_json() {
    let s = store("hook");
    let (url, got) = mock_webhook();
    let snk = StoreNotifySink {
        store: s.clone(),
        config: pai_notify::NotifyConfig {
            email_to: None,
            webhook_url: Some(url),
        },
        email: None,
    };
    let fired = NotifySink::deliver_external(&snk, "Ping", "webhook body", "test")
        .await
        .unwrap();
    assert_eq!(fired, vec!["webhook"]);
    // give the listener thread a beat to record
    for _ in 0..50 {
        if !got.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let bodies = got.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let v: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(v["title"], "Ping");
    assert_eq!(v["body"], "webhook body");
}

#[tokio::test]
async fn tool_external_fans_out_when_configured() {
    let s = store("fanout");
    let captured = Arc::new(CaptureEmail(Mutex::new(vec![])));
    let snk = StoreNotifySink {
        store: s.clone(),
        config: pai_notify::NotifyConfig {
            email_to: Some("me@x.io".into()),
            webhook_url: None,
        },
        email: Some(captured.clone()),
    };
    let tool = NotifySendTool;
    let ctx = ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: None,
        email: None,
        vision: None,
        notify: Some(&snk),
        apps: None,
        allowed_roots: &[],
    };
    // external=true → inbox row AND the email channel
    let out = tool
        .execute(
            serde_json::json!({"title": "n", "body": "x", "external": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.summary.contains("+email"), "{}", out.summary);
    assert_eq!(ns::list(&s, false, 10).unwrap().len(), 1);
    assert_eq!(captured.0.lock().unwrap().len(), 1);
}

// ---------------------------------------------------------------- sync

struct Dev {
    dir: PathBuf,
    store: Arc<Store>,
    ids: IdentityStore,
    key_dir: PathBuf,
    device: Device,
}

fn dev(tag: &str) -> Dev {
    let dir = tmpdir(tag);
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let ids = IdentityStore::new(store.clone());
    let key_dir = dir.join("keys");
    let user = ids.create_user("u").unwrap();
    let device = ids
        .register_device(
            user.id,
            tag,
            Platform::Linux,
            DeviceCapabilities::default(),
            &key_dir,
        )
        .unwrap();
    Dev {
        dir,
        store,
        ids,
        key_dir,
        device,
    }
}

fn pair_devices(offerer: &Dev, acceptor: &Dev) {
    let agree_a = crypto::agreement_key(offerer.device.id, &offerer.dir).unwrap();
    let agree_b = crypto::agreement_key(acceptor.device.id, &acceptor.dir).unwrap();
    let offer =
        pair::make_offer(&offerer.device, &agree_a, &offerer.ids, &offerer.key_dir).unwrap();
    let accept = pair::accept_offer(
        &acceptor.store,
        &offer,
        &acceptor.device,
        &agree_b,
        &acceptor.ids,
        &acceptor.key_dir,
        &acceptor.dir,
    )
    .unwrap();
    pair::complete_pairing(&offerer.store, &accept, &agree_a, &offerer.dir).unwrap();
}

fn eng(d: &Dev, shared: &Path) -> engine::SyncEngine<FolderTransport> {
    engine::folder_engine(shared, d.store.clone(), d.device.id, &d.dir).unwrap()
}

#[tokio::test]
async fn notification_syncs_with_read_state() {
    let a = dev("na");
    let b = dev("nb");
    pair_devices(&a, &b);
    let shared = tmpdir("shared");

    let id = ns::publish(&a.store, "sync me", "body", "test", SyncScope::Synchronized).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    let items = ns::list(&b.store, false, 10).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "sync me");
    assert_eq!(ns::unread_count(&b.store).unwrap(), 1);

    // read on B → read_at propagates back to A
    ns::mark_read(&b.store, &id).unwrap();
    eng(&b, &shared).push().await.unwrap();
    eng(&a, &shared).pull().await.unwrap();
    assert_eq!(ns::unread_count(&a.store).unwrap(), 0);
    let got = ns::get(&a.store, &id).unwrap().unwrap();
    assert!(got.read_at.is_some());
}

#[tokio::test]
async fn notification_tombstone_and_local_scope() {
    let a = dev("ta");
    let b = dev("tb");
    pair_devices(&a, &b);
    let shared = tmpdir("shared");

    // device-local stays put
    ns::publish(&a.store, "local only", "b", "t", SyncScope::DeviceLocal).unwrap();
    let id = ns::publish(&a.store, "roam", "b", "t", SyncScope::Synchronized).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    let items = ns::list(&b.store, false, 10).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "roam");

    // tombstone propagates
    ns::remove(&a.store, &id).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(ns::list(&b.store, false, 10).unwrap().is_empty());
}

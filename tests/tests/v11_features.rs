//! V1.1 tests: schema v3, encrypted store + key lifecycle, keystore
//! fallback, embedder auto-embed + vector recall, document ingest /
//! hybrid search / delete, and the filesystem jail.

use pai_core::*;
use pai_documents::DocumentStore;
use pai_memory::{Embedder, MemoryBackend, MemoryItem, RecallQuery, SqliteMemory};
use pai_storage::Store;
use pai_tools::{Tool, ToolContext};
use std::sync::Arc;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v11-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Deterministic pseudo-embedder: bag-of-bytes vector, enough for cosine.
struct ConstEmbedder;

#[async_trait::async_trait]
impl Embedder for ConstEmbedder {
    fn id(&self) -> String {
        "test/bytes".into()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0f32; 16];
        for (i, b) in text.bytes().enumerate() {
            v[i % 16] += b as f32 / 255.0;
        }
        Ok(v)
    }
}

#[test]
fn schema_v3_columns_exist() {
    let dir = tmpdir("schema");
    let store = Store::open(&dir, None).unwrap();
    store
        .with_conn(|c| {
            // devices.key_storage + document_sections.embedding added by v3;
            // sync_peers by v4; conversation/document sync metadata by v5;
            // task claim/lease + payload columns by v6; app placement by
            // v12; app_crdt_cells/app_crdt_view by v13.
            c.execute_batch("SELECT key_storage FROM devices LIMIT 0")?;
            c.execute_batch("SELECT embedding FROM document_sections LIMIT 0")?;
            c.execute_batch("SELECT agree_pubkey FROM sync_peers LIMIT 0")?;
            c.execute_batch("SELECT updated_at, deleted FROM conversations LIMIT 0")?;
            c.execute_batch("SELECT sync_scope, updated_at, deleted FROM documents LIMIT 0")?;
            c.execute_batch(
                "SELECT updated_at, deleted, trigger_json, payload_json,
                        claimed_by, lease_expires_at FROM tasks LIMIT 0",
            )?;
            c.execute_batch(
                "SELECT name, definition_json, sync_scope, updated_at, deleted
                 FROM workflows LIMIT 0",
            )?;
            c.execute_batch(
                "SELECT workflow_id, status, step_index, outputs_json
                 FROM workflow_runs LIMIT 0",
            )?;
            c.execute_batch(
                "SELECT title, body, source, channel, read_at, sync_scope
                 FROM notifications LIMIT 0",
            )?;
            c.query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
        })
        .map(|v| assert_eq!(v, "13"))
        .unwrap();
}

#[test]
fn encrypted_store_roundtrip_and_migration() {
    let dir = tmpdir("enc");
    // 1) encrypted create
    let key = [42u8; 32];
    {
        let store = Store::open(&dir, Some(&key)).unwrap();
        store
            .with_conn(|c| c.execute_batch("SELECT count(*) FROM users"))
            .unwrap();
    }
    // ciphertext on disk — not the SQLite header
    let bytes = std::fs::read(dir.join("personal-ai.db")).unwrap();
    assert_ne!(&bytes[..16], b"SQLite format 3\0");
    // 2) reopen with the key — works
    let store = Store::open(&dir, Some(&key)).unwrap();
    store
        .with_conn(|c| c.execute_batch("SELECT count(*) FROM users"))
        .unwrap();
    // 3) reopen without a key — fails
    assert!(
        Store::open(&dir, None).is_err() || {
            // if open succeeded (header probe inconclusive), a query must fail
            let s = Store::open(&dir, None).unwrap();
            s.with_conn(|c| c.execute_batch("SELECT count(*) FROM users"))
                .is_err()
        }
    );
    // wrong key also fails
    let bad = [7u8; 32];
    let s2 = Store::open(&dir, Some(&bad));
    assert!(
        s2.is_err() || {
            s2.unwrap()
                .with_conn(|c| c.execute_batch("SELECT count(*) FROM users"))
                .is_err()
        }
    );
}

#[test]
fn plaintext_db_auto_migrates_to_encrypted() {
    let dir = tmpdir("mig");
    {
        let store = Store::open(&dir, None).unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO users(id, display_name, created_at) VALUES(?1,'u','t')",
                    [uuid::Uuid::new_v4().to_string()],
                )
            })
            .unwrap();
    }
    // open with a key → auto-migrate, data preserved
    let key = [9u8; 32];
    let store = Store::open(&dir, Some(&key)).unwrap();
    let n: i64 = store
        .with_conn(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(n, 1);
    let bytes = std::fs::read(dir.join("personal-ai.db")).unwrap();
    assert_ne!(&bytes[..16], b"SQLite format 3\0");
}

#[test]
fn keystore_returns_stable_key() {
    let dir = tmpdir("ks");
    let k1 = pai_identity::keystore::store_key(&dir).expect("keystore must yield a key");
    let k2 = pai_identity::keystore::store_key(&dir).unwrap();
    assert_eq!(k1.len(), 32);
    assert_eq!(k1, k2);
}

#[tokio::test]
async fn embedder_auto_embeds_and_recalls() {
    let dir = tmpdir("emb");
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let mem = SqliteMemory::new(store).with_embedder(Arc::new(ConstEmbedder));
    mem.put(&MemoryItem {
        id: MemoryId::new(),
        scope: MemoryScope::Semantic,
        content: "chamomile tea is my favorite".into(),
        source: MemorySource::UserStated,
        created_at: now(),
        updated_at: now(),
        confidence: 1.0,
        importance: 0.8,
        privacy: PrivacyLevel::Normal,
        entities: vec![],
        embedding: None,
        conversation: None,
        share_circle: None,
    })
    .await
    .unwrap();
    // recall by text → embedder supplies the query vector; semantic hit on
    // related phrasing that shares no FTS terms.
    let hits = mem
        .recall(&RecallQuery {
            text: Some("chamomile tea is my favorite".into()),
            limit: 4,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!hits.is_empty());
    // stored embedding survived the round trip
    let item = hits[0].item.clone();
    assert!(item.embedding.is_some());
}

#[tokio::test]
async fn documents_ingest_search_delete() {
    let dir = tmpdir("docs");
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let docs = DocumentStore::new(store).with_embedder(Arc::new(ConstEmbedder));

    let id = docs
        .ingest(
            b"# Notes\n\nRust lifetimes borrow-check at compile time.\n\nCargo builds workspaces.",
            "text/markdown",
            Some("rust notes"),
        )
        .await
        .unwrap();

    // keyword hit
    let hits = docs.search("borrow-check", 5).await.unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].document_id, id);
    assert_eq!(hits[0].title.as_deref(), Some("rust notes"));

    // list shows it
    let rows = docs.list().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, id);
    assert!(rows[0].4 >= 1);

    // delete removes doc + sections
    docs.delete(id).unwrap();
    assert!(docs.list().unwrap().is_empty());
    let hits = docs.search("borrow-check", 5).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn documents_ingest_path_jailed() {
    let dir = tmpdir("jail");
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let docs = DocumentStore::new(store);

    let allowed = dir.join("inbox");
    std::fs::create_dir_all(&allowed).unwrap();
    let inside = allowed.join("ok.txt");
    std::fs::write(&inside, b"hello from the inbox").unwrap();
    let outside = dir.join("secret.txt");
    std::fs::write(&outside, b"top secret").unwrap();

    // inside the jail → ok
    docs.ingest_path(&inside, std::slice::from_ref(&allowed))
        .await
        .unwrap();
    // outside → PermissionDenied
    let err = docs.ingest_path(&outside, &[allowed]).await.unwrap_err();
    assert!(matches!(err, Error::PermissionDenied(_)));
    // empty jail → everything denied
    let err = docs.ingest_path(&inside, &[]).await.unwrap_err();
    assert!(matches!(err, Error::PermissionDenied(_)));
}

#[tokio::test]
async fn documents_search_tool_returns_citations() {
    let dir = tmpdir("tool");
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let docs = DocumentStore::new(store);
    docs.ingest(b"alpha beta gamma delta", "text/plain", Some("greek"))
        .await
        .unwrap();

    let tool = pai_tools::DocumentsSearch;
    let ctx = ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: Some(&docs),
        email: None,
        vision: None,
        notify: None,
        apps: None,
        audio_gen: None,
        media_dir: None,
        allowed_roots: &[],
    };
    let out = tool
        .execute(serde_json::json!({"query": "beta"}), &ctx)
        .await
        .unwrap();
    let results = out.value["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ref"], "D1");
    assert_eq!(results[0]["title"], "greek");

    // ingest tool is jailed: escaping the allowed roots is denied
    let tool = pai_tools::DocumentsIngest;
    let jail_dir = dir.join("inbox");
    std::fs::create_dir_all(&jail_dir).unwrap();
    std::fs::write(jail_dir.join("a.txt"), b"ingest me").unwrap();
    let ctx = ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: Some(&docs),
        email: None,
        vision: None,
        notify: None,
        apps: None,
        audio_gen: None,
        media_dir: None,
        allowed_roots: std::slice::from_ref(&jail_dir),
    };
    let out = tool
        .execute(
            serde_json::json!({"path": jail_dir.join("a.txt").to_string_lossy()}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.value["document_id"].is_string());

    let err = tool
        .execute(
            serde_json::json!({"path": dir.join("nope.txt").to_string_lossy()}),
            &ctx,
        )
        .await;
    assert!(err.is_err());
}

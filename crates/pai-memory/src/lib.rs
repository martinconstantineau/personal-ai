//! Memory subsystem. Multiple memory *types*, not one vector dump.
//!
//! - WorkingMemory — the current run's scratch context (ephemeral)
//! - EpisodicMemory — events/conversations that happened
//! - SemanticMemory — facts and preferences
//! - ProceduralMemory — how-to knowledge, learned workflows
//! - RelationshipMemory — entities and their links (people, projects)
//!
//! Every item carries provenance (`UserStated` vs `AiInferred`), confidence,
//! importance, and a privacy level. Inferred memories never silently become
//! fact — confidence stays low and the UI/agent must treat them as guesses.

use async_trait::async_trait;
use pai_core::*;
use pai_storage::{parse_ts, ts, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: MemoryId,
    pub scope: MemoryScope,
    pub content: String,
    pub source: MemorySource,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// 0..1. User-stated defaults to 1.0; AI inferences start lower.
    pub confidence: f32,
    /// 0..1. Drives retention + ranking.
    pub importance: f32,
    pub privacy: PrivacyLevel,
    /// Named entities this memory mentions (people, projects, products).
    pub entities: Vec<String>,
    /// Optional embedding (provider-supplied); brute-force cosine in V1.
    pub embedding: Option<Vec<f32>>,
    /// Conversation this memory is scoped to. `None` = global (shared by
    /// every conversation); `Some(c)` = visible only inside conversation `c`.
    #[serde(default)]
    pub conversation: Option<ConversationId>,
}

#[derive(Debug, Clone, Default)]
pub struct RecallQuery {
    pub text: Option<String>,
    pub scopes: Vec<MemoryScope>,
    pub entities: Vec<String>,
    pub min_confidence: Option<f32>,
    pub since: Option<Timestamp>,
    pub limit: usize,
    /// Query embedding for semantic ranking.
    pub embedding: Option<Vec<f32>>,
    /// Visibility filter for conversation-scoped memories.
    pub memory_scope: MemoryScopeQuery,
}

/// Which memories a recall is allowed to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryScopeQuery {
    /// No filtering — memory browser and admin views.
    #[default]
    All,
    /// Only global (unscoped) memories — runs outside any conversation.
    GlobalOnly,
    /// `Scoped(c, include_global)`: `c`'s own memories, plus global memories
    /// when `include_global` — i.e. a "shared" vs "isolated" conversation.
    Scoped(ConversationId, bool),
}

#[derive(Debug, Clone)]
pub struct ScoredMemory {
    pub item: MemoryItem,
    pub score: f32,
}

#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn put(&self, item: &MemoryItem) -> Result<()>;
    async fn get(&self, id: MemoryId) -> Result<MemoryItem>;
    /// Soft delete — recoverable until purged; respects "delete my data".
    async fn delete(&self, id: MemoryId) -> Result<()>;
    /// User correction: replace content, mark user-stated, confidence 1.
    async fn correct(&self, id: MemoryId, new_content: &str) -> Result<()>;
    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ScoredMemory>>;
    async fn link(&self, from: MemoryId, to_id: &str, rel: &str, confidence: f32) -> Result<()>;
}

// ---------------------------------------------------------------------------

pub struct SqliteMemory {
    store: std::sync::Arc<Store>,
}

impl SqliteMemory {
    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }

    fn scope_name(s: MemoryScope) -> &'static str {
        match s {
            MemoryScope::Working => "working",
            MemoryScope::Episodic => "episodic",
            MemoryScope::Semantic => "semantic",
            MemoryScope::Procedural => "procedural",
            MemoryScope::Relationship => "relationship",
        }
    }

    fn scope_from(s: &str) -> MemoryScope {
        match s {
            "episodic" => MemoryScope::Episodic,
            "procedural" => MemoryScope::Procedural,
            "relationship" => MemoryScope::Relationship,
            "working" => MemoryScope::Working,
            _ => MemoryScope::Semantic,
        }
    }

    fn source_name(s: MemorySource) -> &'static str {
        match s {
            MemorySource::UserStated => "user_stated",
            MemorySource::AiInferred => "ai_inferred",
            MemorySource::Imported => "imported",
            MemorySource::SystemObserved => "system_observed",
        }
    }

    fn source_from(s: &str) -> MemorySource {
        match s {
            "ai_inferred" => MemorySource::AiInferred,
            "imported" => MemorySource::Imported,
            "system_observed" => MemorySource::SystemObserved,
            _ => MemorySource::UserStated,
        }
    }

    fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryItem> {
        let emb: Option<Vec<u8>> = r.get(10)?;
        Ok(MemoryItem {
            id: MemoryId(
                uuid::Uuid::parse_str(&r.get::<_, String>(0)?)
                    .unwrap_or_else(|_| uuid::Uuid::nil()),
            ),
            scope: Self::scope_from(&r.get::<_, String>(1)?),
            content: r.get(2)?,
            source: Self::source_from(&r.get::<_, String>(3)?),
            created_at: parse_ts(&r.get::<_, String>(4)?),
            updated_at: parse_ts(&r.get::<_, String>(5)?),
            confidence: r.get(6)?,
            importance: r.get(7)?,
            privacy: match r.get::<_, String>(8)?.as_str() {
                "sensitive" => PrivacyLevel::Sensitive,
                "secret" => PrivacyLevel::Secret,
                _ => PrivacyLevel::Normal,
            },
            entities: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
            embedding: emb.map(|b| {
                b.chunks(4)
                    .filter_map(|c| <[u8; 4]>::try_from(c).ok().map(f32::from_le_bytes))
                    .collect()
            }),
            conversation: r.get::<_, Option<String>>(11)?.map(|s| {
                ConversationId(uuid::Uuid::parse_str(&s).unwrap_or_else(|_| uuid::Uuid::nil()))
            }),
        })
    }

    const COLS: &'static str = "id, type, content, source, created_at, updated_at, confidence,
         importance, privacy_level, entities_json, embedding, conversation_id";
}

/// SQL fragment implementing [`RecallQuery::memory_scope`]. The id renders
/// as a UUID literal — hex + dashes only, so direct interpolation is safe.
fn conversation_clause(q: &RecallQuery) -> String {
    match q.memory_scope {
        MemoryScopeQuery::All => String::new(),
        MemoryScopeQuery::GlobalOnly => " AND conversation_id IS NULL".into(),
        MemoryScopeQuery::Scoped(c, true) => {
            format!(" AND (conversation_id IS NULL OR conversation_id = '{c}')")
        }
        MemoryScopeQuery::Scoped(c, false) => {
            format!(" AND conversation_id = '{c}'")
        }
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

#[async_trait]
impl MemoryBackend for SqliteMemory {
    async fn put(&self, item: &MemoryItem) -> Result<()> {
        let emb: Option<Vec<u8>> = item
            .embedding
            .as_ref()
            .map(|v| v.iter().flat_map(|f| f.to_le_bytes()).collect());
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO memories(id, type, content, source, created_at,
                    updated_at, confidence, importance, privacy_level,
                    entities_json, embedding, deleted, sync_scope,
                    conversation_id)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,?12,?13)",
                params![
                    item.id.to_string(),
                    Self::scope_name(item.scope),
                    item.content,
                    Self::source_name(item.source),
                    ts(&item.created_at),
                    ts(&item.updated_at),
                    item.confidence,
                    item.importance,
                    match item.privacy {
                        PrivacyLevel::Normal => "normal",
                        PrivacyLevel::Sensitive => "sensitive",
                        PrivacyLevel::Secret => "secret",
                    },
                    serde_json::to_string(&item.entities).unwrap(),
                    emb,
                    "synchronized",
                    item.conversation.map(|c| c.to_string()),
                ],
            )
        })?;
        Ok(())
    }

    async fn get(&self, id: MemoryId) -> Result<MemoryItem> {
        self.store
            .with_conn(|c| {
                c.query_row(
                    &format!(
                        "SELECT {} FROM memories WHERE id=?1 AND deleted=0",
                        Self::COLS
                    ),
                    params![id.to_string()],
                    Self::row_to_item,
                )
            })
            .map_err(|_| Error::NotFound(format!("memory {id}")))
    }

    async fn delete(&self, id: MemoryId) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE memories SET deleted=1, updated_at=?2 WHERE id=?1",
                params![id.to_string(), ts(&now())],
            )
        })?;
        Ok(())
    }

    async fn correct(&self, id: MemoryId, new_content: &str) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE memories SET content=?2, source='user_stated',
                    confidence=1.0, updated_at=?3 WHERE id=?1",
                params![id.to_string(), new_content, ts(&now())],
            )
        })?;
        Ok(())
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ScoredMemory>> {
        let mut out: Vec<ScoredMemory> = Vec::new();
        let limit = query.limit.max(1);
        let conv = conversation_clause(query);

        if let Some(text) = &query.text {
            // FTS5 keyword path — OR over tokens so natural-language queries
            // ("what do I prefer?") still hit partial matches.
            let escaped = text
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| t.len() > 1)
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(" OR ");
            if escaped.is_empty() {
                return Ok(out);
            }
            let rows: Vec<MemoryItem> = self.store.with_conn(|c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {} FROM memories m
                     JOIN memories_fts ON memories_fts.rowid = m.rowid
                     WHERE memories_fts MATCH ?1 AND m.deleted=0{conv}
                     ORDER BY rank LIMIT ?2",
                    Self::COLS
                        .split(',')
                        .map(|s| format!("m.{s}"))
                        .collect::<Vec<_>>()
                        .join(",")
                ))?;
                let rows = stmt.query_map(params![escaped, limit as i64], Self::row_to_item)?;
                rows.collect()
            })?;
            for item in rows {
                out.push(ScoredMemory {
                    score: item.importance + item.confidence,
                    item,
                });
            }
        }

        // Semantic path — brute-force cosine over stored embeddings.
        if let Some(qe) = &query.embedding {
            let rows: Vec<MemoryItem> = self.store.with_conn(|c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {} FROM memories WHERE embedding IS NOT NULL AND deleted=0{conv}",
                    Self::COLS
                ))?;
                let rows = stmt.query_map([], Self::row_to_item)?;
                rows.collect()
            })?;
            for item in rows {
                if let Some(e) = &item.embedding {
                    let s = cosine(qe, e);
                    if s > 0.3 {
                        out.push(ScoredMemory {
                            score: s + item.importance,
                            item,
                        });
                    }
                }
            }
        }

        // Browsing path — no query text/embedding: most recent first.
        if query.text.is_none() && query.embedding.is_none() {
            let rows: Vec<MemoryItem> = self.store.with_conn(|c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {} FROM memories WHERE deleted=0{conv}
                     ORDER BY updated_at DESC LIMIT ?1",
                    Self::COLS
                ))?;
                let rows = stmt.query_map(params![limit as i64], Self::row_to_item)?;
                rows.collect()
            })?;
            for item in rows {
                out.push(ScoredMemory {
                    score: item.importance,
                    item,
                });
            }
        }

        // Filters
        out.retain(|sm| {
            (query.scopes.is_empty() || query.scopes.contains(&sm.item.scope))
                && query
                    .min_confidence
                    .map(|c| sm.item.confidence >= c)
                    .unwrap_or(true)
                && query.since.map(|s| sm.item.created_at >= s).unwrap_or(true)
                && (query.entities.is_empty()
                    || query
                        .entities
                        .iter()
                        .any(|e| sm.item.entities.iter().any(|x| x == e)))
        });
        out.sort_by(|a, b| b.score.total_cmp(&a.score));
        out.truncate(limit);
        Ok(out)
    }

    async fn link(&self, from: MemoryId, to_id: &str, rel: &str, confidence: f32) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO memory_relationships(from_id, to_id, rel_type, confidence)
                 VALUES(?1,?2,?3,?4)
                 ON CONFLICT(from_id,to_id,rel_type)
                 DO UPDATE SET confidence=excluded.confidence",
                params![from.to_string(), to_id, rel, confidence],
            )
        })?;
        Ok(())
    }
}

/// Ephemeral per-run scratch memory. Not persisted.
#[derive(Default)]
pub struct WorkingMemory {
    items: Vec<MemoryItem>,
}

impl WorkingMemory {
    pub fn push(&mut self, content: impl Into<String>) {
        self.items.push(MemoryItem {
            id: MemoryId::new(),
            scope: MemoryScope::Working,
            content: content.into(),
            source: MemorySource::SystemObserved,
            created_at: now(),
            updated_at: now(),
            confidence: 1.0,
            importance: 0.0,
            privacy: PrivacyLevel::Normal,
            entities: vec![],
            embedding: None,
            conversation: None,
        });
    }

    pub fn items(&self) -> &[MemoryItem] {
        &self.items
    }
}

/// Convenience constructor for a user-stated semantic memory.
pub fn user_fact(content: impl Into<String>, importance: f32) -> MemoryItem {
    MemoryItem {
        id: MemoryId::new(),
        scope: MemoryScope::Semantic,
        content: content.into(),
        source: MemorySource::UserStated,
        created_at: now(),
        updated_at: now(),
        confidence: 1.0,
        importance,
        privacy: PrivacyLevel::Normal,
        entities: vec![],
        embedding: None,
        conversation: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_recall_delete() {
        let store = std::sync::Arc::new(Store::in_memory().unwrap());
        let mem = SqliteMemory::new(store);
        let item = user_fact("I prefer local models", 0.9);
        mem.put(&item).await.unwrap();

        let hits = mem
            .recall(&RecallQuery {
                text: Some("prefer local".into()),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item.content, "I prefer local models");
        assert_eq!(hits[0].item.source, MemorySource::UserStated);

        mem.correct(item.id, "I strongly prefer local models")
            .await
            .unwrap();
        let fixed = mem.get(item.id).await.unwrap();
        assert!(fixed.content.contains("strongly"));
        assert_eq!(fixed.confidence, 1.0);

        mem.delete(item.id).await.unwrap();
        assert!(mem.get(item.id).await.is_err());
    }

    #[test]
    fn cosine_similarity() {
        assert!(cosine(&[1.0, 0.0], &[1.0, 0.0]) > 0.99);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]) < 0.01);
    }
}

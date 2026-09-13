//! Document subsystem. Documents are a first-class modality:
//! metadata + extracted structure + embeddings, all citable.
//!
//! Implemented now: plain-text/markdown extraction + chunking + keyword
//! search via SQLite FTS. PDF/EPUB/DOCX extraction and OCR are declared
//! behind `Extractor` and land as adapters (pure-rs crates keep it free:
//! `pdf-extract`, `epub`, `docx-rs`, `tesseract` via leptess).

use async_trait::async_trait;
use pai_core::*;
use pai_memory::Embedder;
use pai_storage::{store_err, ts, Store};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A structural slice of a document — sections, paragraphs, tables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub index: u32,
    pub heading: Option<String>,
    pub text: String,
    /// Page number when the format has pages.
    pub page: Option<u32>,
}

#[async_trait]
pub trait Extractor: Send + Sync {
    fn mime(&self) -> &'static str;
    async fn extract(&self, bytes: &[u8]) -> Result<Vec<Section>>;
}

/// text/* and markdown — fully implemented.
pub struct TextExtractor;

#[async_trait]
impl Extractor for TextExtractor {
    fn mime(&self) -> &'static str {
        "text/plain"
    }

    async fn extract(&self, bytes: &[u8]) -> Result<Vec<Section>> {
        let text = String::from_utf8_lossy(bytes);
        Ok(chunk(&text, 1500)
            .into_iter()
            .enumerate()
            .map(|(i, t)| Section {
                index: i as u32,
                heading: None,
                text: t,
                page: None,
            })
            .collect())
    }
}

/// Split text into overlapping ~`size`-char chunks on paragraph boundaries.
pub fn chunk(text: &str, size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for para in text.split("\n\n") {
        if cur.len() + para.len() > size && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push_str(para);
        cur.push_str("\n\n");
    }
    if !cur.trim().is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Row returned by `DocumentStore::list`: (id, title, mime, created, #sections).
pub type DocumentRow = (DocumentId, Option<String>, String, Timestamp, u32);

/// Internal SQL row for section scans: (doc_id, section, text, title, embedding).
type SectionRow = (String, u32, String, Option<String>, Vec<u8>);
/// Internal SQL row for FTS hits: (doc_id, section, snippet, title).
type FtsRow = (String, u32, String, Option<String>);

/// A search hit: document, section index, snippet, and a blended score.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub document_id: DocumentId,
    pub title: Option<String>,
    pub section: u32,
    pub snippet: String,
    pub score: f32,
}

/// Ingest a document: store blob, extract sections, index for search.
/// Owns an `Arc<Store>` so it can live in `ToolContext`/runtimes.
pub struct DocumentStore {
    store: Arc<Store>,
    embedder: Option<Arc<dyn Embedder>>,
}

impl DocumentStore {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            embedder: None,
        }
    }

    pub fn with_embedder(mut self, e: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(e);
        self
    }

    pub async fn ingest(
        &self,
        bytes: &[u8],
        mime: &str,
        title: Option<&str>,
    ) -> Result<DocumentId> {
        let blob = self.store.put_blob(bytes)?;
        let id = DocumentId::new();
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO documents(id, title, mime, blob, created_at, trust)
                 VALUES(?1,?2,?3,?4,?5,'untrusted')",
                rusqlite::params![id.to_string(), title, mime, blob, ts(&now())],
            )
        })?;

        let sections: Vec<Section> = match mime {
            "text/plain" | "text/markdown" => TextExtractor.extract(bytes).await?,
            "text/html" => {
                let text = strip_html(&String::from_utf8_lossy(bytes));
                TextExtractor.extract(text.as_bytes()).await?
            }
            other => {
                // Declared-not-implemented formats land here until their
                // extractor adapter ships. The document is still stored.
                tracing::info!(%other, "no extractor for mime; stored blob only");
                vec![]
            }
        };
        for s in &sections {
            let emb: Option<Vec<u8>> = match &self.embedder {
                Some(e) => e
                    .embed(&s.text)
                    .await
                    .ok()
                    .map(|v| v.iter().flat_map(|f| f.to_le_bytes()).collect()),
                None => None,
            };
            self.store.with_conn(|c| {
                c.execute(
                    "INSERT INTO document_sections(document_id, section, text, embedding)
                     VALUES(?1,?2,?3,?4)",
                    rusqlite::params![id.to_string(), s.index, s.text, emb],
                )?;
                c.execute(
                    "INSERT INTO documents_fts(rowid, text)
                     VALUES (last_insert_rowid(), ?1)",
                    rusqlite::params![s.text],
                )
            })?;
        }
        Ok(id)
    }

    /// Ingest a file from disk. `jail` = allowed roots; an empty slice means
    /// "no filesystem reads permitted" (tool-initiated ingest stays jailed;
    /// the CLI/UI ingest path passes roots it owns).
    pub async fn ingest_path(
        &self,
        path: &std::path::Path,
        jail: &[std::path::PathBuf],
    ) -> Result<DocumentId> {
        let canon = std::fs::canonicalize(path)
            .map_err(|e| Error::InvalidInput(format!("{path:?}: {e}")))?;
        if !jail.iter().any(|root| {
            std::fs::canonicalize(root)
                .map(|r| canon.starts_with(r))
                .unwrap_or(false)
        }) {
            return Err(Error::PermissionDenied(format!(
                "path {canon:?} is outside the allowed ingest roots"
            )));
        }
        let bytes = std::fs::read(&canon).map_err(store_err)?;
        let mime = match canon
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str()
        {
            "md" | "markdown" => "text/markdown",
            "html" | "htm" => "text/html",
            _ => "text/plain",
        };
        let title = canon.file_name().and_then(|n| n.to_str());
        self.ingest(&bytes, mime, title).await
    }

    /// List ingested documents (id, title, mime, created, section count).
    pub fn list(&self) -> Result<Vec<DocumentRow>> {
        self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT d.id, d.title, d.mime, d.created_at,
                        (SELECT count(*) FROM document_sections s
                          WHERE s.document_id = d.id) AS sections
                 FROM documents d ORDER BY d.created_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    DocumentId(
                        uuid::Uuid::parse_str(&r.get::<_, String>(0)?)
                            .unwrap_or_else(|_| uuid::Uuid::nil()),
                    ),
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    pai_storage::parse_ts(&r.get::<_, String>(3)?),
                    r.get::<_, u32>(4)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    pub fn delete(&self, id: DocumentId) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "DELETE FROM documents_fts WHERE rowid IN
                    (SELECT rowid FROM document_sections WHERE document_id=?1)",
                rusqlite::params![id.to_string()],
            )?;
            c.execute(
                "DELETE FROM document_sections WHERE document_id=?1",
                rusqlite::params![id.to_string()],
            )?;
            c.execute(
                "DELETE FROM documents WHERE id=?1",
                rusqlite::params![id.to_string()],
            )
        })?;
        Ok(())
    }

    /// Hybrid search: FTS keyword hits merged with embedding-cosine hits.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let mut hits: Vec<SearchHit> = Vec::new();

        // FTS path — OR over tokens like memory recall.
        let escaped = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 1)
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        if !escaped.is_empty() {
            let rows: Vec<FtsRow> = self.store.with_conn(|c| {
                let mut stmt = c.prepare(
                    "SELECT d.document_id, d.section,
                            snippet(documents_fts, 0, '[', ']', '…', 32),
                            (SELECT title FROM documents WHERE id = d.document_id)
                     FROM documents_fts
                     JOIN document_sections d ON d.rowid = documents_fts.rowid
                     WHERE documents_fts MATCH ?1
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(rusqlite::params![escaped, limit as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })?;
            for (doc, sec, snip, title) in rows {
                hits.push(SearchHit {
                    document_id: DocumentId(
                        uuid::Uuid::parse_str(&doc).unwrap_or_else(|_| uuid::Uuid::nil()),
                    ),
                    title,
                    section: sec,
                    snippet: snip,
                    score: 1.0,
                });
            }
        }

        // Vector path — brute-force cosine over section embeddings.
        if let Some(e) = &self.embedder {
            if let Ok(qe) = e.embed(query).await {
                let rows: Vec<SectionRow> = self.store.with_conn(|c| {
                    let mut stmt = c.prepare(
                        "SELECT s.document_id, s.section,
                                    substr(s.text,1,240),
                                    (SELECT title FROM documents WHERE id = s.document_id),
                                    s.embedding
                             FROM document_sections s
                             WHERE s.embedding IS NOT NULL",
                    )?;
                    let rows = stmt.query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, u32>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Vec<u8>>(4)?,
                        ))
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })?;
                for (doc, sec, snip, title, raw) in rows {
                    let emb: Vec<f32> = raw
                        .chunks(4)
                        .filter_map(|c| <[u8; 4]>::try_from(c).ok().map(f32::from_le_bytes))
                        .collect();
                    let s = pai_memory_cosine(&qe, &emb);
                    if s > 0.35 {
                        hits.push(SearchHit {
                            document_id: DocumentId(
                                uuid::Uuid::parse_str(&doc).unwrap_or_else(|_| uuid::Uuid::nil()),
                            ),
                            title,
                            section: sec,
                            snippet: snip,
                            score: s,
                        });
                    }
                }
            }
        }

        // Dedupe on (doc, section): keep the best score.
        let mut best: std::collections::HashMap<(String, u32), SearchHit> = Default::default();
        for h in hits {
            best.entry((h.document_id.to_string(), h.section))
                .and_modify(|e| e.score = e.score.max(h.score))
                .or_insert(h);
        }
        let mut out: Vec<SearchHit> = best.into_values().collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score));
        out.truncate(limit);
        Ok(out)
    }
}

fn pai_memory_cosine(a: &[f32], b: &[f32]) -> f32 {
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

/// Naive tag stripper for HTML — good enough for citation snippets.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

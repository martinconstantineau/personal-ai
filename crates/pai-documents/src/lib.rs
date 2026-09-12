//! Document subsystem. Documents are a first-class modality:
//! metadata + extracted structure + embeddings, all citable.
//!
//! Implemented now: plain-text/markdown extraction + chunking + keyword
//! search via SQLite FTS. PDF/EPUB/DOCX extraction and OCR are declared
//! behind `Extractor` and land as adapters (pure-rs crates keep it free:
//! `pdf-extract`, `epub`, `docx-rs`, `tesseract` via leptess).

use async_trait::async_trait;
use pai_core::*;
use pai_storage::{ts, Store};
use serde::{Deserialize, Serialize};

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

/// Ingest a document: store blob, extract sections, index for search.
pub struct DocumentStore<'a> {
    store: &'a Store,
}

impl<'a> DocumentStore<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
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
            other => {
                // Declared-not-implemented formats land here until their
                // extractor adapter ships. The document is still stored.
                tracing::info!(%other, "no extractor for mime; stored blob only");
                vec![]
            }
        };
        for s in &sections {
            self.store.with_conn(|c| {
                c.execute(
                    "INSERT INTO document_sections(document_id, section, text)
                     VALUES(?1,?2,?3)",
                    rusqlite::params![id.to_string(), s.index, s.text],
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

    /// Keyword search across all ingested documents; returns
    /// (document_id, section, snippet).
    pub fn search(&self, query: &str) -> Result<Vec<(DocumentId, u32, String)>> {
        self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT d.document_id, d.section, snippet(documents_fts, 0,
                        '[', ']', '…', 32)
                 FROM documents_fts
                 JOIN document_sections d ON d.rowid = documents_fts.rowid
                 WHERE documents_fts MATCH ?1
                 LIMIT 20",
            )?;
            let rows = stmt.query_map(rusqlite::params![query], |r| {
                Ok((
                    DocumentId(uuid::Uuid::parse_str(&r.get::<_, String>(0)?).unwrap_or_default()),
                    r.get::<_, u32>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.collect()
        })
    }
}

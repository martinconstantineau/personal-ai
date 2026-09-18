//! Document subsystem. Documents are a first-class modality:
//! metadata + extracted structure + embeddings, all citable.
//!
//! Implemented: text/markdown, HTML, PDF (pdf-extract — sections keep
//! page numbers), EPUB and DOCX (zip + quick-xml, no native deps), plus
//! chunking + keyword search via SQLite FTS. OCR is the remaining
//! declared-not-implemented adapter (`tesseract` via leptess).

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

/// application/pdf — `by_pages` keeps the page number on each
/// section, so citations can point at the page.
pub struct PdfExtractor;

#[async_trait]
impl Extractor for PdfExtractor {
    fn mime(&self) -> &'static str {
        "application/pdf"
    }

    async fn extract(&self, bytes: &[u8]) -> Result<Vec<Section>> {
        let pages = pdf_extract::extract_text_from_mem_by_pages(bytes)
            .map_err(|e| Error::InvalidInput(format!("pdf: {e}")))?;
        let mut sections = Vec::new();
        for (page, text) in pages.iter().enumerate() {
            for t in chunk(text, 1500) {
                sections.push(Section {
                    index: sections.len() as u32,
                    heading: None,
                    text: t,
                    page: Some(page as u32 + 1),
                });
            }
        }
        Ok(sections)
    }
}

/// application/epub+zip — container.xml locates the OPF, the spine
/// gives reading order, each spine XHTML is tag-stripped into
/// sections. Hand-rolled on zip+quick-xml: no extra format crate.
pub struct EpubExtractor;

#[async_trait]
impl Extractor for EpubExtractor {
    fn mime(&self) -> &'static str {
        "application/epub+zip"
    }

    async fn extract(&self, bytes: &[u8]) -> Result<Vec<Section>> {
        let invalid = |m: String| Error::InvalidInput(m);
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| invalid(format!("epub: {e}")))?;
        let container = zip_text(&mut zip, "META-INF/container.xml")?;
        let opf_path = attr_value(&container, "full-path")
            .ok_or_else(|| invalid("epub: no rootfile".into()))?;
        let opf = zip_text(&mut zip, &opf_path)?;
        let manifest = manifest_hrefs(&opf);
        let base = opf_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let mut sections = Vec::new();
        for id in spine_idrefs(&opf) {
            let Some(href) = manifest.get(&id) else {
                continue;
            };
            let path = if base.is_empty() {
                href.clone()
            } else {
                format!("{base}/{href}")
            };
            let Ok(xhtml) = zip_text(&mut zip, &path) else {
                continue;
            };
            for t in chunk(&xhtml_to_text(&xhtml), 1500) {
                sections.push(Section {
                    index: sections.len() as u32,
                    heading: None,
                    text: t,
                    page: None,
                });
            }
        }
        Ok(sections)
    }
}

/// DOCX — word/document.xml: `w:p` paragraphs, `w:t` runs,
/// `w:tab`/`w:br` whitespace.
pub struct DocxExtractor;

#[async_trait]
impl Extractor for DocxExtractor {
    fn mime(&self) -> &'static str {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    }

    async fn extract(&self, bytes: &[u8]) -> Result<Vec<Section>> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| Error::InvalidInput(format!("docx: {e}")))?;
        let xml = zip_text(&mut zip, "word/document.xml")?;
        Ok(chunk(&docx_text(&xml), 1500)
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

/// Extension → MIME for ingest paths — shared by `ingest_path`, the
/// FFI boundary, and the CLI so they can't drift.
pub fn mime_for_path(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "text/plain",
    }
}

fn zip_text(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Result<String> {
    use std::io::Read;
    let mut f = zip
        .by_name(name)
        .map_err(|e| Error::InvalidInput(format!("{name}: {e}")))?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| Error::InvalidInput(format!("{name}: {e}")))?;
    Ok(s)
}

/// Attribute lookup over a tag/XML fragment — container and OPF
/// markup is regular enough that a string scan beats a full parser.
fn attr_value(xml: &str, attr: &str) -> Option<String> {
    let pat = format!(" {attr}=\"");
    let i = xml.find(&pat)? + pat.len();
    let j = xml[i..].find('"')? + i;
    Some(xml[i..j].to_string())
}

/// OPF manifest: `<item id=… href=…>` → id → href.
fn manifest_hrefs(opf: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut rest = opf;
    while let Some(i) = rest.find("<item ") {
        rest = &rest[i..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        if let (Some(id), Some(href)) = (attr_value(tag, "id"), attr_value(tag, "href")) {
            map.insert(id, href);
        }
        rest = &rest[end..];
    }
    map
}

/// OPF spine: `<itemref idref=…>` in reading order.
fn spine_idrefs(opf: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = opf;
    while let Some(i) = rest.find("<itemref ") {
        rest = &rest[i..];
        let end = rest.find('>').unwrap_or(rest.len());
        if let Some(id) = attr_value(&rest[..end], "idref") {
            out.push(id);
        }
        rest = &rest[end..];
    }
    out
}

/// XHTML → text: drop script/style/head, newline at block boundaries.
fn xhtml_to_text(xhtml: &str) -> String {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_str(xhtml);
    let mut out = String::new();
    let mut skip = 0u32;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"script" | b"style" | b"head" => skip += 1,
                b"p" | b"div" | b"section" | b"article" | b"h1" | b"h2" | b"h3" | b"h4" | b"h5"
                | b"h6" | b"li" | b"tr" | b"blockquote" => out.push_str("\n\n"),
                b"br" => out.push('\n'),
                _ => {}
            },
            Ok(Event::End(e)) => match local_name(e.name().as_ref()) {
                b"script" | b"style" | b"head" => skip = skip.saturating_sub(1),
                b"p" | b"div" | b"li" | b"tr" | b"blockquote" => out.push_str("\n\n"),
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == b"br" {
                    out.push('\n');
                }
            }
            Ok(Event::Text(e)) if skip == 0 => {
                if let Ok(t) = e.decode() {
                    out.push_str(&t);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// document.xml → text: paragraphs on `w:p`, text only inside `w:t`
/// (field codes in `w:instrText` are skipped), tabs/breaks preserved.
fn docx_text(xml: &str) -> String {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_str(xml);
    let mut out = String::new();
    let mut in_t = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(e.name().as_ref()) {
                b"p" => {
                    if !out.is_empty() && !out.ends_with("\n\n") {
                        out.push_str("\n\n");
                    }
                }
                b"t" => in_t = true,
                b"tab" => out.push('\t'),
                b"br" | b"cr" => out.push('\n'),
                _ => {}
            },
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == b"t" {
                    in_t = false;
                }
            }
            Ok(Event::Text(e)) if in_t => {
                if let Ok(t) = e.decode() {
                    out.push_str(&t);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Strip a namespace prefix: `w:t` → `t`, `opf:itemref` → `itemref`.
fn local_name(q: &[u8]) -> &[u8] {
    match q.iter().position(|&b| b == b':') {
        Some(i) => &q[i + 1..],
        None => q,
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

/// Internal SQL row for section scans: (doc_id, section, text, title, embedding, page).
type SectionRow = (String, u32, String, Option<String>, Vec<u8>, Option<u32>);
/// Internal SQL row for FTS hits: (doc_id, section, snippet, title, page).
type FtsRow = (String, u32, String, Option<String>, Option<u32>);

/// A search hit: document, section index, snippet, and a blended score.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub document_id: DocumentId,
    pub title: Option<String>,
    pub section: u32,
    /// Page number when the source format paginates (PDF).
    pub page: Option<u32>,
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
                "INSERT INTO documents(id, title, mime, blob, created_at,
                    updated_at, trust)
                 VALUES(?1,?2,?3,?4,?5,?5,'untrusted')",
                rusqlite::params![id.to_string(), title, mime, blob, ts(&now())],
            )
        })?;

        let sections: Vec<Section> = match mime {
            "text/plain" | "text/markdown" => TextExtractor.extract(bytes).await?,
            "text/html" => {
                let text = strip_html(&String::from_utf8_lossy(bytes));
                TextExtractor.extract(text.as_bytes()).await?
            }
            "application/pdf" => PdfExtractor.extract(bytes).await?,
            "application/epub+zip" => EpubExtractor.extract(bytes).await?,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                DocxExtractor.extract(bytes).await?
            }
            other => {
                // Unknown formats still store the blob — the row stays
                // citable even without searchable sections.
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
                    "INSERT INTO document_sections(document_id, section, text, embedding, page, heading)
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![id.to_string(), s.index, s.text, emb, s.page, s.heading],
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
        let mime = mime_for_path(&canon);
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
                 FROM documents d WHERE d.deleted=0 ORDER BY d.created_at DESC",
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

    /// Tombstone the document: searchable sections are dropped, the row
    /// stays so the sync engine can propagate the delete to peers.
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
                "UPDATE documents SET deleted=1, updated_at=?2 WHERE id=?1",
                rusqlite::params![id.to_string(), ts(&now())],
            )
        })?;
        Ok(())
    }

    /// Mark the document for E2EE sync (or pull it back to this device).
    pub fn set_sync_scope(&self, id: DocumentId, scope: SyncScope) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE documents SET sync_scope=?2, updated_at=?3 WHERE id=?1",
                rusqlite::params![
                    id.to_string(),
                    match scope {
                        SyncScope::Synchronized => "synchronized",
                        SyncScope::DeviceLocal => "device_local",
                    },
                    ts(&now())
                ],
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
                            (SELECT title FROM documents WHERE id = d.document_id),
                            d.page
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
                        r.get::<_, Option<u32>>(4)?,
                    ))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })?;
            for (doc, sec, snip, title, page) in rows {
                hits.push(SearchHit {
                    document_id: DocumentId(
                        uuid::Uuid::parse_str(&doc).unwrap_or_else(|_| uuid::Uuid::nil()),
                    ),
                    title,
                    section: sec,
                    page,
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
                                    s.embedding,
                                    s.page
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
                            r.get::<_, Option<u32>>(5)?,
                        ))
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })?;
                for (doc, sec, snip, title, raw, page) in rows {
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
                            page,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// In-memory zip of name → body entries — DOCX/EPUB fixtures are
    /// just zips with well-known member names.
    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// Minimal valid PDF: one Helvetica text run per page, real xref.
    fn pdf_fixture(texts: &[&str]) -> Vec<u8> {
        let mut objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                (0..texts.len())
                    .map(|i| format!("{} 0 R", 4 + 2 * i))
                    .collect::<Vec<_>>()
                    .join(" "),
                texts.len()
            )
            .into_bytes(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        for (i, t) in texts.iter().enumerate() {
            let pid = 4 + 2 * i;
            objects.push(
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                     /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                    pid + 1
                )
                .into_bytes(),
            );
            let stream = format!("BT /F1 24 Tf 72 720 Td ({t}) Tj ET");
            objects.push(
                format!(
                    "<< /Length {} >>\nstream\n{stream}\nendstream",
                    stream.len()
                )
                .into_bytes(),
            );
        }
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n", i + 1).into_bytes());
            pdf.extend(body);
            pdf.extend(b"\nendobj\n");
        }
        let xref = pdf.len();
        let size = objects.len() + 1;
        pdf.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").into_bytes());
        for off in offsets {
            pdf.extend(format!("{off:010} 00000 n \n").into_bytes());
        }
        pdf.extend(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .into_bytes(),
        );
        pdf
    }

    #[tokio::test]
    async fn pdf_sections_keep_page_numbers() {
        let pdf = pdf_fixture(&["first page text", "second page text"]);
        let sections = PdfExtractor.extract(&pdf).await.unwrap();
        assert_eq!(sections.len(), 2);
        assert!(sections[0].text.contains("first page text"));
        assert_eq!(sections[0].page, Some(1));
        assert!(sections[1].text.contains("second page text"));
        assert_eq!(sections[1].page, Some(2));
    }

    #[tokio::test]
    async fn epub_reads_spine_in_order() {
        let epub = zip_bytes(&[
            (
                "META-INF/container.xml",
                r#"<?xml version="1.0"?>
                   <container><rootfiles>
                     <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
                   </rootfiles></container>"#,
            ),
            (
                "OEBPS/content.opf",
                r#"<package><manifest>
                       <item id="c2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
                       <item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
                     </manifest><spine>
                       <itemref idref="c1"/><itemref idref="c2"/>
                     </spine></package>"#,
            ),
            (
                "OEBPS/ch1.xhtml",
                "<html><head><title>T</title></head><body><p>alpha chapter</p></body></html>",
            ),
            (
                "OEBPS/ch2.xhtml",
                "<html><body><p>beta chapter</p></body></html>",
            ),
        ]);
        let sections = EpubExtractor.extract(&epub).await.unwrap();
        assert_eq!(sections.len(), 2);
        assert!(sections[0].text.contains("alpha chapter"));
        assert!(sections[1].text.contains("beta chapter"));
    }

    #[tokio::test]
    async fn docx_extracts_paragraph_text() {
        let docx = zip_bytes(&[(
            "word/document.xml",
            r#"<w:document xmlns:w="x"><w:body>
                 <w:p><w:r><w:t>Hello document</w:t></w:r></w:p>
                 <w:p><w:r><w:instrText>PAGE</w:instrText><w:t>second para</w:t></w:r></w:p>
               </w:body></w:document>"#,
        )]);
        let sections = DocxExtractor.extract(&docx).await.unwrap();
        assert_eq!(sections.len(), 1);
        assert!(sections[0].text.contains("Hello document"));
        assert!(sections[0].text.contains("second para"));
        assert!(!sections[0].text.contains("PAGE")); // field code skipped
    }

    #[test]
    fn mime_for_path_covers_extractors() {
        use std::path::Path;
        assert_eq!(mime_for_path(Path::new("a.pdf")), "application/pdf");
        assert_eq!(mime_for_path(Path::new("a.epub")), "application/epub+zip");
        assert!(mime_for_path(Path::new("a.docx")).contains("wordprocessingml"));
        assert_eq!(mime_for_path(Path::new("a.md")), "text/markdown");
        assert_eq!(mime_for_path(Path::new("a.bin")), "text/plain");
    }
}

//! `pai docs …` — document ingest/search/lifecycle.

use crate::ctx::Ctx;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum DocsCmd {
    /// Ingest a file (txt/md/html) into the document store.
    Ingest {
        path: String,
        /// Mark the document `synchronized` for E2EE sync.
        #[arg(long)]
        sync: bool,
    },
    /// Set sync scope: synchronized | device-local (default).
    Sync { id: String, mode: String },
    /// List ingested documents.
    List,
    /// Search document sections.
    Search { query: String },
    /// Remove a document and its sections.
    Delete { id: String },
}

pub(crate) async fn run(cmd: DocsCmd, ctx: &Ctx) -> Result<()> {
    match cmd {
        DocsCmd::Ingest { path, sync } => {
            // User-initiated: no jail — they named the file.
            let canon = std::fs::canonicalize(&path)
                .map_err(|e| Error::InvalidInput(format!("{path}: {e}")))?;
            let bytes = std::fs::read(&canon)
                .map_err(|e| Error::InvalidInput(format!("{canon:?}: {e}")))?;
            let mime = pai_documents::mime_for_path(&canon);
            let id = ctx
                .documents
                .ingest(&bytes, mime, canon.file_name().and_then(|n| n.to_str()))
                .await?;
            if sync {
                ctx.documents.set_sync_scope(id, SyncScope::Synchronized)?;
            }
            println!(
                "ingested: {id}{}",
                if sync { " (synchronized)" } else { "" }
            );
        }
        DocsCmd::Sync { id, mode } => {
            let sc = match mode.as_str() {
                "synchronized" => SyncScope::Synchronized,
                "device-local" | "device_local" => SyncScope::DeviceLocal,
                _ => {
                    return Err(Error::InvalidInput(
                        "mode: synchronized|device-local".into(),
                    ))
                }
            };
            ctx.documents
                .set_sync_scope(DocumentId(parse_uuid(&id, "document")?), sc)?;
        }
        DocsCmd::List => {
            for (id, title, mime, at, sections) in ctx.documents.list()? {
                println!(
                    "  {}  {:<28} {:<14} {} sections  {}",
                    &id.to_string()[..8],
                    title.unwrap_or_else(|| "untitled".into()),
                    mime,
                    sections,
                    at.format("%Y-%m-%d")
                );
            }
        }
        DocsCmd::Search { query } => {
            for h in ctx.documents.search(&query, 10).await? {
                println!(
                    "  {:.2}  {}  {}  {}",
                    h.score,
                    &h.document_id.to_string()[..8],
                    h.title.unwrap_or_else(|| "untitled".into()),
                    h.snippet.replace('\n', " ")
                );
            }
        }
        DocsCmd::Delete { id } => {
            ctx.documents
                .delete(DocumentId(parse_uuid(&id, "document")?))?;
            println!("deleted {id}");
        }
    }
    Ok(())
}

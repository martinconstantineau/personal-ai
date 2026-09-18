//! `pai models …` — model registry: catalog, install (incl. hf://),
//! scan removable packs, detect local inference, serve.

use clap::Subcommand;
use pai_core::*;
use pai_storage::Store;
use std::sync::Arc;

#[derive(Subcommand)]
pub(crate) enum ModelsCmd {
    /// Show the catalog + installed models (incl. hf:// installs).
    List,
    /// Download + verify + install a model — a catalog slug or an
    /// `hf://owner/repo/file.gguf` reference.
    Install {
        model: String,
        /// Install into this directory instead of the local store —
        /// e.g. a flash drive (`E:\pai-models`). The pack is
        /// self-describing: plug it into any pai device and it is
        /// adopted on scan.
        #[arg(long)]
        to: Option<String>,
    },
    /// Re-scan mounted drives for `pai-models/` packs and adopt any
    /// models found — run after plugging in a model drive.
    Scan,
    /// Remove an installed model.
    Uninstall { slug: String },
    /// Models that fit this device's hardware.
    Runnable,
    /// Probe local inference endpoints + provider binaries.
    Detect,
    /// Search Hugging Face for GGUF model repos.
    Search { query: String },
    /// List .gguf files inside a hub repo ("owner/repo").
    Files {
        repo: String,
        #[arg(long, default_value = "main")]
        revision: String,
    },
    /// Serve an installed model via a local `llama-server` binary.
    Serve {
        slug: String,
        #[arg(long, default_value = "8080")]
        port: u16,
    },
}

pub(crate) async fn run(cmd: &ModelsCmd, cfg: &pai_config::Config) -> Result<()> {
    let mgr = pai_models::ModelManager::new(
        Arc::new(Store::open(
            &cfg.data_dir,
            pai_identity::keystore::store_key(&cfg.data_dir).as_ref(),
        )?),
        &cfg.data_dir,
    );
    for m in pai_models::builtin_catalog() {
        mgr.register(&m)?;
    }
    match cmd {
        ModelsCmd::List => {
            let caps = pai_identity::probe_capabilities();
            for (m, installed, path) in mgr.list()? {
                let fits = pai_models::fits(&m, &caps)
                    .map(|_| "fits")
                    .unwrap_or("too large");
                println!(
                    "  {:<44} [{}MB, {}, {}] {:<10}{}",
                    m.slug,
                    m.size_bytes / 1_000_000,
                    m.quantization.clone().unwrap_or_default(),
                    m.license.clone().unwrap_or_else(|| "?".into()),
                    fits,
                    if installed {
                        match path {
                            Some(p) if p.exists() => {
                                format!("installed → {}", p.display())
                            }
                            Some(p) => {
                                format!("installed (offline — last at {})", p.display())
                            }
                            None => "installed".into(),
                        }
                    } else {
                        String::new()
                    }
                );
            }
            println!("\ninstall: pai models install <slug|hf://owner/repo/file.gguf>");
        }
        ModelsCmd::Install { model, to } => {
            let manifest = pai_models::resolve_model_arg(model).await?;
            mgr.register(&manifest)?;
            let p = match to {
                Some(d) => {
                    let dir = pai_models::pack_dir_for(std::path::Path::new(d));
                    mgr.install_to(&manifest.model.slug, &manifest, &dir)
                        .await?
                }
                None => mgr.install(&manifest.model.slug, &manifest).await?,
            };
            println!("installed: {}", p.display());
            if to.is_some() {
                println!("pack:      plug into any pai device — adopted on `pai models scan`");
            }
            println!("serve:    pai models serve {}", manifest.model.slug);
        }
        ModelsCmd::Scan => {
            let found = mgr.scan()?;
            if found.is_empty() {
                println!("no model packs found (looked for pai-models/ on mounted drives)");
            }
            for s in found {
                println!("  adopted {:<44} {}", s.slug, s.path.display());
            }
        }
        ModelsCmd::Uninstall { slug } => {
            mgr.uninstall(slug)?;
            println!("uninstalled {slug}");
        }
        ModelsCmd::Runnable => {
            let caps = pai_identity::probe_capabilities();
            for slug in mgr.runnable(&caps)? {
                println!("  {slug}");
            }
        }
        ModelsCmd::Detect => {
            println!("endpoints:");
            let eps = pai_inference::detect_endpoints(std::time::Duration::from_secs(2)).await;
            if eps.is_empty() {
                println!("  (none live)");
            }
            for ep in &eps {
                println!(
                    "  {} {} — models: {}",
                    ep.provider,
                    ep.base_url,
                    if ep.models.is_empty() {
                        "(none reported)".into()
                    } else {
                        ep.models.join(", ")
                    }
                );
            }
            println!("binaries:");
            for b in ["llama-server", "ollama", "lms"] {
                match pai_inference::find_in_path(b) {
                    Some(p) => println!("  {b:<14} {}", p.display()),
                    None => println!("  {b:<14} (not on PATH)"),
                }
            }
            if eps.is_empty() {
                println!("\nget started: pai models install <slug> && pai models serve <slug>");
            }
        }
        ModelsCmd::Search { query } => {
            let repos = pai_models::hf::HfClient::new().search(query, 15).await?;
            for r in repos {
                println!(
                    "  {:<48} ⬇ {:<10} ♥ {}",
                    r.id,
                    r.downloads.map(|d| d.to_string()).unwrap_or("?".into()),
                    r.likes.map(|l| l.to_string()).unwrap_or("?".into())
                );
            }
            println!("\nfiles: pai models files <owner/repo> — install: pai models install hf://<owner/repo>/<file.gguf>");
        }
        ModelsCmd::Files { repo, revision } => {
            let files = pai_models::hf::HfClient::new()
                .list_gguf_files(repo, revision)
                .await?;
            if files.is_empty() {
                println!("(no .gguf files in {repo}@{revision})");
            }
            for f in files {
                let size = f
                    .size
                    .map(|s| format!("{}MB", s / 1_000_000))
                    .unwrap_or_else(|| "?".into());
                println!("  {size:>8}  hf://{repo}/{}", f.path);
            }
        }
        ModelsCmd::Serve { slug, port } => {
            let path = mgr
                .locate(slug)?
                .ok_or_else(|| Error::NotFound(format!("{slug} not installed")))?;
            let bin = pai_inference::find_in_path("llama-server").ok_or_else(|| {
                Error::NotFound(
                    "llama-server not on PATH — install llama.cpp, or run Ollama/LM Studio and use --provider auto".into(),
                )
            })?;
            println!(
                "serving {} at http://127.0.0.1:{port} (ctrl-c to stop)",
                path.display()
            );
            let mut child = std::process::Command::new(&bin)
                .args([
                    "-m",
                    &path.to_string_lossy(),
                    "--port",
                    &port.to_string(),
                    "--host",
                    "127.0.0.1",
                ])
                .spawn()
                .map_err(|e| Error::Other(format!("spawn llama-server: {e}")))?;
            // Wait for readiness, then hand the process to the user.
            let url = format!("http://127.0.0.1:{port}");
            for _ in 0..60 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if pai_inference::detect_endpoints(std::time::Duration::from_millis(300))
                    .await
                    .iter()
                    .any(|e| e.base_url == url)
                {
                    println!("ready — chat with: pai chat --provider llama-server --server-url {url} --model {slug}");
                    break;
                }
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(Error::Other(format!("llama-server exited early: {status}")));
                }
            }
            let _ = child.wait();
        }
    }
    Ok(())
}

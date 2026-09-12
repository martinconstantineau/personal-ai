//! `pai` — Personal AI CLI. Drives the vertical slice end-to-end:
//! Flutter-quality UX is in apps/desktop; this is the same Rust core.

use clap::{Parser, Subcommand};
use pai_agent::{AgentDefinition, AgentEvent, AgentRuntime, AutoApprove, CancelToken, RunRequest};
use pai_core::*;
use pai_inference::{EchoProvider, LlamaServerProvider};
use pai_memory::{MemoryBackend, RecallQuery, SqliteMemory};
use pai_permissions::{PolicyEngine, PolicyTable};
use pai_storage::Store;
use std::io::Write;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "pai", about = "Personal AI — local-first, free models only")]
struct Cli {
    /// Data directory (default: ~/.local/share/personal-ai)
    #[arg(long, global = true)]
    data_dir: Option<String>,
    /// Inference provider: echo (offline stub) or llama-server
    #[arg(long, global = true, default_value = "echo")]
    provider: String,
    /// Base URL for the local inference server
    #[arg(long, global = true, default_value = "http://127.0.0.1:8080")]
    server_url: String,
    /// Model slug to request from the provider
    #[arg(long, global = true)]
    model: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the vertical slice end-to-end: remember → recall → tool → audit.
    Demo,
    /// Interactive chat REPL.
    Chat,
    /// Model registry operations.
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
    /// Dump the audit log — "what did my AI do?"
    Audit {
        #[arg(long, default_value = "50")]
        limit: usize,
    },
    /// List memories.
    Memories,
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// Show the built-in free-model catalog + install state.
    List,
    /// Download + verify + install a model by slug.
    Install { slug: String },
    /// Remove an installed model.
    Uninstall { slug: String },
    /// Models that fit this device's hardware.
    Runnable,
}

struct Ctx {
    agent: AgentRuntime,
    memory: Arc<dyn MemoryBackend>,
    audit: Arc<pai_audit::AuditLog>,
}

fn build(cli: &Cli) -> Result<(Ctx, pai_config::Config)> {
    let cfg = match &cli.data_dir {
        Some(d) => pai_config::Config {
            data_dir: d.into(),
            inference: pai_config::InferenceConfig {
                local_server_url: cli.server_url.clone(),
                ..pai_config::Config::default().inference
            },
            ..pai_config::Config::default()
        },
        None => pai_config::Config::load(None)?,
    };
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| Error::Storage(e.to_string()))?;
    let store = Arc::new(Store::open(&cfg.data_dir)?);

    // Identity: reuse existing user/device or create on first run.
    let ids = pai_identity::IdentityStore::new(store.clone());
    let key_dir = cfg.data_dir.join("keys");
    let device = {
        // find-or-create: first user + first device
        let user = match store.with_conn(|c| {
            c.query_row("SELECT id FROM users LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
        }) {
            Ok(id) => ids.get_user(UserId(uuid::Uuid::parse_str(&id).unwrap()))?,
            Err(_) => ids.create_user("local-user")?,
        };
        match ids.list_devices(user.id)?.into_iter().next() {
            Some(d) => d,
            None => ids.register_device(
                user.id,
                "cli-host",
                Platform::Linux,
                pai_identity::probe_capabilities(),
                &key_dir,
            )?,
        }
    };

    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemory::new(store.clone()));
    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));

    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    providers.register(Arc::new(LlamaServerProvider::new(
        &cfg.inference.local_server_url,
        cli.model
            .clone()
            .unwrap_or_else(|| cfg.inference.default_model.clone()),
    )));

    let agent = AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(PolicyTable::with_defaults())),
        memory: memory.clone(),
        audit: audit.clone(),
        max_steps: cfg.inference.max_agent_steps,
        step_timeout: std::time::Duration::from_secs(cfg.inference.request_timeout_secs),
        device: device.id,
    };

    Ok((
        Ctx {
            agent,
            memory,
            audit,
        },
        cfg,
    ))
}

fn agent_def(provider: &str, model: Option<String>) -> AgentDefinition {
    AgentDefinition {
        name: "assistant".into(),
        description: "Default assistant".into(),
        purpose: "general assistance".into(),
        tools: vec![], // all registered
        memory_scopes: vec![MemoryScope::Semantic, MemoryScope::Episodic],
        provider: provider.into(),
        model,
    }
}

fn print_event(e: &AgentEvent) {
    match e {
        AgentEvent::RunStarted { run } => println!("  ▸ run {}", &run.to_string()[..8]),
        AgentEvent::Step { index } => println!("  ▸ step {index}"),
        AgentEvent::ToolCallRequested { tool, risk, .. } => {
            println!("  ▸ tool call: {tool} (risk: {risk})")
        }
        AgentEvent::ApprovalNeeded {
            tool,
            summary,
            permissions,
            ..
        } => {
            println!("  ▸ APPROVAL NEEDED: {tool} — {summary} [{permissions:?}]")
        }
        AgentEvent::ToolExecuted { tool, summary, .. } => {
            println!("  ▸ executed {tool}: {summary}")
        }
        AgentEvent::ToolDenied { tool, .. } => println!("  ▸ DENIED: {tool}"),
        AgentEvent::TextDelta { .. } => {}
        AgentEvent::Done { state, .. } => println!("  ▸ finished: {state:?}"),
    }
}

async fn send(ctx: &Ctx, def: &AgentDefinition, text: &str) -> Result<Option<String>> {
    let emit = |e: AgentEvent| print_event(&e);
    let out = ctx
        .agent
        .run(RunRequest {
            definition: def,
            history: vec![],
            input: text.to_string(),
            conversation: None,
            approval: &AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
        })
        .await?;
    Ok(out.answer)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let (ctx, cfg) = build(&cli)?;

    match cli.cmd {
        Cmd::Demo => {
            let def = agent_def(&cli.provider, cli.model.clone());
            println!("== Vertical slice: provider={} ==\n", cli.provider);

            println!("user: Remember that I prefer local models");
            let a = send(&ctx, &def, "Remember that I prefer local models").await?;
            println!("assistant: {}\n", a.unwrap_or_default());

            println!("user: What do I prefer for AI models?");
            let a = send(&ctx, &def, "What do I prefer for AI models?").await?;
            println!("assistant: {}\n", a.unwrap_or_default());

            println!("user: What is 41 + 1?");
            let a = send(&ctx, &def, "What is 41 + 1?").await?;
            println!("assistant: {}\n", a.unwrap_or_default());

            println!("== Audit log (last 15) ==");
            for e in ctx.audit.recent(15)? {
                println!(
                    "  {} {:?} {:?} tool={:?} outcome={:?}",
                    e.at.format("%H:%M:%S"),
                    e.kind,
                    e.detail,
                    e.tool,
                    e.outcome
                );
            }
            println!("\nData dir: {}", cfg.data_dir.display());
        }

        Cmd::Chat => {
            let def = agent_def(&cli.provider, cli.model.clone());
            println!("pai chat (provider={}). 'quit' to exit.", cli.provider);
            let stdin = std::io::stdin();
            loop {
                print!("you> ");
                std::io::stdout().flush().ok();
                let mut line = String::new();
                if stdin.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim();
                if line.is_empty() || line == "quit" {
                    break;
                }
                match send(&ctx, &def, line).await {
                    Ok(Some(a)) => println!("pai> {a}"),
                    Ok(None) => println!("pai> (no answer)"),
                    Err(e) => println!("pai> error: {e}"),
                }
            }
        }

        Cmd::Models { cmd } => {
            let mgr =
                pai_models::ModelManager::new(Arc::new(Store::open(&cfg.data_dir)?), &cfg.data_dir);
            for m in pai_models::builtin_catalog() {
                mgr.register(&m)?;
            }
            match cmd {
                ModelsCmd::List => {
                    for m in pai_models::builtin_catalog() {
                        let installed = mgr.installed_path(&m.model.slug)?.is_some();
                        println!(
                            "  {} [{}MB, {}, license: {}] {}",
                            m.model.slug,
                            m.model.size_bytes / 1_000_000,
                            m.model.quantization.unwrap_or_default(),
                            m.model.license.unwrap_or_default(),
                            if installed { "installed" } else { "" }
                        );
                    }
                }
                ModelsCmd::Install { slug } => {
                    let m = pai_models::builtin_catalog()
                        .into_iter()
                        .find(|m| m.model.slug == slug)
                        .ok_or_else(|| Error::NotFound(slug.clone()))?;
                    let p = mgr.install(&slug, &m).await?;
                    println!("installed: {}", p.display());
                }
                ModelsCmd::Uninstall { slug } => {
                    mgr.uninstall(&slug)?;
                    println!("uninstalled {slug}");
                }
                ModelsCmd::Runnable => {
                    let caps = pai_identity::probe_capabilities();
                    for slug in mgr.runnable(&caps)? {
                        println!("  {slug}");
                    }
                }
            }
        }

        Cmd::Audit { limit } => {
            for e in ctx.audit.recent(limit)? {
                println!(
                    "{}  {:<18} tool={:<16} outcome={:?}",
                    e.at.to_rfc3339(),
                    format!("{:?}", e.kind),
                    e.tool.unwrap_or_default(),
                    e.outcome
                );
            }
        }

        Cmd::Memories => {
            let items = ctx
                .memory
                .recall(&RecallQuery {
                    text: None,
                    limit: 200,
                    ..Default::default()
                })
                .await?;
            if items.is_empty() {
                // Fall back: list all via a permissive query.
                println!("(no memories yet — try `pai demo`)");
            }
            for s in items {
                println!(
                    "  [{:?}|{:?}] {} (conf={}, imp={})",
                    s.item.scope,
                    s.item.source,
                    s.item.content,
                    s.item.confidence,
                    s.item.importance
                );
            }
        }
    }
    Ok(())
}

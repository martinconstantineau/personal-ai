//! `pai` — Personal AI CLI. Drives the vertical slice end-to-end:
//! Flutter-quality UX is in apps/desktop; this is the same Rust core.

use clap::{Parser, Subcommand};
use pai_agent::{
    AgentDefinition, AgentEvent, AgentRuntime, AutoApprove, CancelToken, ConversationStore,
    Persistence, RunRequest, RunStore,
};
use pai_core::*;
use pai_inference::{EchoProvider, LlamaServerProvider};
use pai_memory::{MemoryBackend, MemoryScopeQuery, RecallQuery, SqliteMemory};
use pai_permissions::{all_permissions, Permission, PolicyEngine, PolicyTable};
use pai_storage::Store;
use pai_tools::Tool;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "pai", about = "Personal AI — local-first, free models only")]
struct Cli {
    /// Data directory (default: ~/.local/share/personal-ai)
    #[arg(long, global = true)]
    data_dir: Option<String>,
    /// Inference provider: echo (offline stub), llama-server, or auto
    /// (probe llama-server/Ollama/LM Studio on localhost).
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
    /// Interactive chat REPL (persistent; see `pai conversations`).
    Chat {
        /// Continue an existing conversation id.
        #[arg(long)]
        conversation: Option<String>,
        /// Isolate this conversation's memory from global recall.
        #[arg(long)]
        isolated: bool,
    },
    /// Model registry operations (catalog + Hugging Face).
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
    /// Conversation management.
    Conversations {
        #[command(subcommand)]
        cmd: ConvCmd,
    },
    /// Agent run recovery.
    Runs {
        #[command(subcommand)]
        cmd: RunsCmd,
    },
    /// Permission policy management.
    Policies {
        #[command(subcommand)]
        cmd: PoliciesCmd,
    },
    /// Dump the audit log — "what did my AI do?"
    Audit {
        #[arg(long, default_value = "50")]
        limit: usize,
    },
    /// Memory operations.
    Memories {
        #[command(subcommand)]
        cmd: Option<MemCmd>,
    },
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// Show the catalog + installed models (incl. hf:// installs).
    List,
    /// Download + verify + install a model — a catalog slug or an
    /// `hf://owner/repo/file.gguf` reference.
    Install { model: String },
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

#[derive(Subcommand)]
enum ConvCmd {
    /// List conversations, newest first.
    List,
    /// Start a new conversation (prints its id).
    New {
        #[arg(long)]
        isolated: bool,
    },
    Rename {
        id: String,
        title: String,
    },
    Delete {
        id: String,
    },
    /// Show a conversation's transcript.
    History {
        id: String,
    },
    /// Set memory scope: shared | isolated.
    Scope {
        id: String,
        mode: String,
    },
}

#[derive(Subcommand)]
enum RunsCmd {
    /// Runs that never finished — crash/interrupt candidates.
    Interrupted,
    /// Resume an interrupted run from its last checkpoint.
    Resume { id: String },
    /// Mark an interrupted run failed (give up on it).
    Abandon { id: String },
}

#[derive(Subcommand)]
enum PoliciesCmd {
    /// Every permission and its effective policy.
    List,
    /// Set a policy: `pai policies set EMAIL_SEND ASK_USER`.
    Set { permission: String, policy: String },
}

#[derive(Subcommand)]
enum MemCmd {
    /// Forget a memory by uuid, or by a text query.
    Forget { target: String },
}

struct Ctx {
    agent: AgentRuntime,
    memory: Arc<dyn MemoryBackend>,
    audit: Arc<pai_audit::AuditLog>,
    conversations: Arc<ConversationStore>,
    runs: Arc<RunStore>,
    session: SessionId,
    provider_name: String,
    model: Option<String>,
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
    let (user, device) = {
        let user = match store.with_conn(|c| {
            c.query_row("SELECT id FROM users LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
        }) {
            Ok(id) => ids.get_user(UserId(uuid::Uuid::parse_str(&id).unwrap()))?,
            Err(_) => ids.create_user("local-user")?,
        };
        let device = match ids.list_devices(user.id)?.into_iter().next() {
            Some(d) => d,
            None => ids.register_device(
                user.id,
                "cli-host",
                current_platform(),
                pai_identity::probe_capabilities(),
                &key_dir,
            )?,
        };
        (user, device)
    };

    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemory::new(store.clone()));
    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));
    let conversations = Arc::new(ConversationStore::new(store.clone()));
    let runs = Arc::new(RunStore::new(store.clone()));
    let session = conversations.get_or_create_session(user.id, device.id)?;

    // Provider resolution. "auto" probes live endpoints (needs a runtime).
    let mut provider_name = cli.provider.clone();
    let mut model = cli.model.clone();
    let mut server_url = cfg.inference.local_server_url.clone();
    if provider_name == "auto" {
        let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Other(e.to_string()))?;
        match rt.block_on(pai_inference::detect_endpoints(
            std::time::Duration::from_secs(2),
        )) {
            found if !found.is_empty() => {
                let ep = &found[0];
                eprintln!("auto: using {} at {}", ep.provider, ep.base_url);
                server_url = ep.base_url.clone();
                if model.is_none() {
                    model = ep.models.first().cloned();
                }
                provider_name = "llama-server".into();
            }
            _ => {
                eprintln!("auto: no local server found — falling back to echo");
                provider_name = "echo".into();
            }
        }
    }

    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    providers.register(Arc::new(LlamaServerProvider::new(
        &server_url,
        model
            .clone()
            .unwrap_or_else(|| cfg.inference.default_model.clone()),
    )));

    let agent = AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(load_policies(&store))),
        memory: memory.clone(),
        audit: audit.clone(),
        max_steps: cfg.inference.max_agent_steps,
        step_timeout: std::time::Duration::from_secs(cfg.inference.request_timeout_secs),
        device: device.id,
        persistence: Some(Persistence {
            conversations: conversations.clone(),
            runs: runs.clone(),
        }),
    };

    Ok((
        Ctx {
            agent,
            memory,
            audit,
            conversations,
            runs,
            session,
            provider_name,
            model,
        },
        cfg,
    ))
}

fn current_platform() -> Platform {
    match std::env::consts::OS {
        "macos" => Platform::MacOs,
        "windows" => Platform::Windows,
        "ios" => Platform::Ios,
        "android" => Platform::Android,
        _ => Platform::Linux,
    }
}

/// `policies` table overlays the shipped defaults.
fn load_policies(store: &Store) -> PolicyTable {
    let mut table = PolicyTable::with_defaults();
    if let Ok(rows) = store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT permission, policy FROM policies")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }) {
        for (perm, policy) in rows {
            if let (Ok(p), Ok(pol)) = (
                serde_json::from_value::<Permission>(serde_json::json!(perm)),
                serde_json::from_value::<ExecutionPolicy>(serde_json::json!(policy)),
            ) {
                table.set(p, pol);
            }
        }
    }
    table
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
        AgentEvent::TextDelta { text } => print!("{text}"),
        AgentEvent::Done { state, .. } => println!("\n  ▸ finished: {state:?}"),
    }
}

struct SendOutcome {
    answer: Option<String>,
    /// True when the answer was already printed token-by-token.
    streamed: bool,
}

async fn send(
    ctx: &Ctx,
    def: &AgentDefinition,
    text: &str,
    conversation: Option<ConversationId>,
    resume_from: Option<AgentRunId>,
) -> Result<SendOutcome> {
    let streamed = Arc::new(AtomicBool::new(false));
    let flag = streamed.clone();
    let emit = move |e: AgentEvent| {
        if matches!(e, AgentEvent::TextDelta { .. }) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        print_event(&e);
    };
    let history = match conversation {
        Some(c) => ctx.conversations.messages(c).unwrap_or_default(),
        None => vec![],
    };
    let out = ctx
        .agent
        .run(RunRequest {
            definition: def,
            history,
            input: text.to_string(),
            conversation,
            approval: &AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: true,
            resume_from,
        })
        .await?;
    Ok(SendOutcome {
        answer: out.answer,
        streamed: streamed.load(std::sync::atomic::Ordering::SeqCst),
    })
}

fn parse_uuid(s: &str, what: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|_| Error::InvalidInput(format!("invalid {what} id '{s}'")))
}

async fn run_models(cmd: &ModelsCmd, cfg: &pai_config::Config) -> Result<()> {
    let mgr = pai_models::ModelManager::new(Arc::new(Store::open(&cfg.data_dir)?), &cfg.data_dir);
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
                        format!("installed → {}", path.unwrap_or_default().display())
                    } else {
                        String::new()
                    }
                );
            }
            println!("\ninstall: pai models install <slug|hf://owner/repo/file.gguf>");
        }
        ModelsCmd::Install { model } => {
            let manifest = pai_models::resolve_model_arg(model).await?;
            mgr.register(&manifest)?;
            let p = mgr.install(&manifest.model.slug, &manifest).await?;
            println!("installed: {}", p.display());
            println!("serve:    pai models serve {}", manifest.model.slug);
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
                .installed_path(slug)?
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
            let def = agent_def(&ctx.provider_name, ctx.model.clone());
            let conv = ctx
                .conversations
                .create(ctx.session, MemoryIsolation::Shared)?;
            println!("== Vertical slice: provider={} ==\n", ctx.provider_name);

            println!("user: Remember that I prefer local models");
            let r = send(
                &ctx,
                &def,
                "Remember that I prefer local models",
                Some(conv.id),
                None,
            )
            .await?;
            if !r.streamed {
                println!("assistant: {}", r.answer.unwrap_or_default());
            }
            println!();

            println!("user: What do I prefer for AI models?");
            let r = send(
                &ctx,
                &def,
                "What do I prefer for AI models?",
                Some(conv.id),
                None,
            )
            .await?;
            if !r.streamed {
                println!("assistant: {}", r.answer.unwrap_or_default());
            }
            println!();

            println!("user: What is 41 + 1?");
            let r = send(&ctx, &def, "What is 41 + 1?", Some(conv.id), None).await?;
            if !r.streamed {
                println!("assistant: {}", r.answer.unwrap_or_default());
            }
            println!();

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

        Cmd::Chat {
            conversation,
            isolated,
        } => {
            let conv = match conversation {
                Some(id) => {
                    let cid = ConversationId(parse_uuid(&id, "conversation")?);
                    ctx.conversations.get(cid)?;
                    cid
                }
                None => {
                    ctx.conversations
                        .create(
                            ctx.session,
                            if isolated {
                                MemoryIsolation::Isolated
                            } else {
                                MemoryIsolation::Shared
                            },
                        )?
                        .id
                }
            };
            let def = agent_def(&ctx.provider_name, ctx.model.clone());
            println!(
                "pai chat (provider={}, conversation={:.8}{}). 'quit' to exit.",
                ctx.provider_name,
                conv.to_string(),
                if isolated { ", isolated memory" } else { "" }
            );
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
                match send(&ctx, &def, line, Some(conv), None).await {
                    Ok(o) if o.streamed => println!(),
                    Ok(o) => match o.answer {
                        Some(a) => println!("pai> {a}"),
                        None => println!("pai> (no answer)"),
                    },
                    Err(e) => println!("pai> error: {e}"),
                }
            }
        }

        Cmd::Models { cmd } => run_models(&cmd, &cfg).await?,

        Cmd::Conversations { cmd } => match cmd {
            ConvCmd::List => {
                for c in ctx.conversations.list()? {
                    let n = ctx.conversations.messages(c.id)?.len();
                    println!(
                        "  {}  {:<9} {:<30} ({} msgs) {}",
                        &c.id.to_string()[..8],
                        match c.memory {
                            MemoryIsolation::Shared => "shared",
                            MemoryIsolation::Isolated => "isolated",
                        },
                        c.title.unwrap_or_else(|| "(untitled)".into()),
                        n,
                        c.created_at.format("%Y-%m-%d %H:%M"),
                    );
                }
            }
            ConvCmd::New { isolated } => {
                let c = ctx.conversations.create(
                    ctx.session,
                    if isolated {
                        MemoryIsolation::Isolated
                    } else {
                        MemoryIsolation::Shared
                    },
                )?;
                println!("{}", c.id);
            }
            ConvCmd::Rename { id, title } => {
                ctx.conversations
                    .rename(ConversationId(parse_uuid(&id, "conversation")?), &title)?;
            }
            ConvCmd::Delete { id } => {
                ctx.conversations
                    .delete(ConversationId(parse_uuid(&id, "conversation")?))?;
            }
            ConvCmd::History { id } => {
                for m in ctx
                    .conversations
                    .messages(ConversationId(parse_uuid(&id, "conversation")?))?
                {
                    let text: String = m
                        .content
                        .iter()
                        .filter_map(|c| c.as_text().map(String::from))
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("{:>9}: {}", format!("{:?}", m.role), text);
                }
            }
            ConvCmd::Scope { id, mode } => {
                let m = match mode.as_str() {
                    "isolated" => MemoryIsolation::Isolated,
                    "shared" => MemoryIsolation::Shared,
                    _ => return Err(Error::InvalidInput("mode: shared|isolated".into())),
                };
                ctx.conversations
                    .set_memory_scope(ConversationId(parse_uuid(&id, "conversation")?), m)?;
            }
        },

        Cmd::Runs { cmd } => match cmd {
            RunsCmd::Interrupted => {
                let runs = ctx.runs.interrupted()?;
                if runs.is_empty() {
                    println!("(no interrupted runs)");
                }
                for r in runs {
                    println!(
                        "  {}  state={:?}  started={}  conv={:?}",
                        r.id,
                        r.state,
                        r.started_at.format("%Y-%m-%d %H:%M:%S"),
                        r.conversation.map(|c| c.to_string())
                    );
                }
            }
            RunsCmd::Resume { id } => {
                let rid = AgentRunId(parse_uuid(&id, "run")?);
                let def = agent_def(&ctx.provider_name, ctx.model.clone());
                send(&ctx, &def, "", None, Some(rid)).await?;
            }
            RunsCmd::Abandon { id } => {
                ctx.runs.abandon(AgentRunId(parse_uuid(&id, "run")?))?;
            }
        },

        Cmd::Policies { cmd } => match cmd {
            PoliciesCmd::List => {
                for p in all_permissions() {
                    println!(
                        "  {:<22} {:?}",
                        format!("{p:?}"),
                        ctx.agent.permissions.effective(p)
                    );
                }
            }
            PoliciesCmd::Set { permission, policy } => {
                let p = serde_json::from_value::<Permission>(serde_json::json!(permission))
                    .map_err(|_| {
                        Error::InvalidInput(format!("unknown permission '{permission}'"))
                    })?;
                let pol = serde_json::from_value::<ExecutionPolicy>(serde_json::json!(policy))
                    .map_err(|_| Error::InvalidInput(format!("unknown policy '{policy}'")))?;
                ctx.agent.permissions.set_policy(p, pol);
                // Persist.
                let store = Store::open(&cfg.data_dir)?;
                store.with_conn(|c| {
                    c.execute(
                        "INSERT INTO policies(permission, policy, updated_at) VALUES(?1,?2,?3)
                         ON CONFLICT(permission) DO UPDATE SET policy=excluded.policy,
                         updated_at=excluded.updated_at",
                        rusqlite::params![permission, policy, pai_storage::ts(&now()),],
                    )
                })?;
                println!("{permission} → {policy}");
            }
        },

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

        Cmd::Memories { cmd } => match cmd {
            None => {
                let items = ctx
                    .memory
                    .recall(&RecallQuery {
                        text: None,
                        limit: 200,
                        memory_scope: MemoryScopeQuery::All,
                        ..Default::default()
                    })
                    .await?;
                if items.is_empty() {
                    println!("(no memories yet — try `pai demo`)");
                }
                for s in items {
                    let scope = s
                        .item
                        .conversation
                        .map(|c| format!("conv:{:.8}", c.to_string()))
                        .unwrap_or_else(|| "global".into());
                    println!(
                        "  {} [{:?}|{:?}|{}] {} (conf={}, imp={})",
                        &s.item.id.to_string()[..8],
                        s.item.scope,
                        s.item.source,
                        scope,
                        s.item.content,
                        s.item.confidence,
                        s.item.importance
                    );
                }
            }
            Some(MemCmd::Forget { target }) => {
                let tool = pai_tools::MemoryForget;
                let args = if uuid::Uuid::parse_str(&target).is_ok() {
                    serde_json::json!({"memory_id": target})
                } else {
                    serde_json::json!({"query": target})
                };
                let ctx_tool = pai_tools::ToolContext {
                    run: AgentRunId::new(),
                    device: ctx.agent.device,
                    memory: Some(ctx.memory.as_ref()),
                    memory_scope: None,
                };
                // CLI user is the operator — direct invocation, still audited
                // via the audit log write below.
                let out = tool.execute(args, &ctx_tool).await?;
                let mut e = pai_audit::event(AuditKind::MemoryDeleted, AuditOutcome::Ok);
                e.device = Some(ctx.agent.device);
                e.detail = out.value.clone();
                ctx.audit.record(&e)?;
                println!("{}", out.summary);
            }
        },
    }
    Ok(())
}

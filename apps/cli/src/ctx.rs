//! Runtime context: config + store + identity (`Base`), and the full
//! agent stack (`Ctx`) for commands that need inference.

use crate::Cli;
use pai_agent::{
    AgentDefinition, AgentEvent, AgentRuntime, ApprovalHandler, CancelToken, ConversationStore,
    Persistence, RunRequest, RunStore,
};
use pai_core::*;
use pai_inference::{EchoProvider, LlamaServerProvider};
use pai_memory::{Embedder, MemoryBackend, SqliteMemory};
use pai_permissions::{Permission, PolicyEngine, PolicyTable};
use pai_storage::Store;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Everything the no-inference command path needs: config, store, identity.
pub(crate) struct Base {
    pub cfg: pai_config::Config,
    pub store: Arc<Store>,
    pub key_dir: std::path::PathBuf,
    pub user: User,
    pub device: Device,
}

impl Base {
    /// Identity store over the same `Store` — rebuilt on demand since it
    /// holds nothing else.
    pub(crate) fn ids(&self) -> pai_identity::IdentityStore {
        pai_identity::IdentityStore::new(self.store.clone())
    }
}

pub(crate) struct Ctx {
    pub(crate) store: Arc<Store>,
    pub(crate) documents: Arc<pai_documents::DocumentStore>,
    pub(crate) email: Option<Arc<dyn pai_connector_email::EmailProvider>>,
    pub(crate) gitlab: Option<Arc<dyn pai_connector_gitlab::GitLabProvider>>,
    pub(crate) agent: AgentRuntime,
    pub(crate) memory: Arc<dyn MemoryBackend>,
    pub(crate) audit: Arc<pai_audit::AuditLog>,
    pub(crate) conversations: Arc<ConversationStore>,
    pub(crate) runs: Arc<RunStore>,
    pub(crate) session: SessionId,
    pub(crate) provider_name: String,
    pub(crate) model: Option<String>,
}

/// Config + store + identity without inference/embedder probing — used by
/// commands that don't need a model (pair, sync).
pub(crate) async fn base(cli: &Cli) -> Result<Base> {
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
    let store_key = pai_identity::keystore::store_key(&cfg.data_dir);
    let store = Arc::new(Store::open(&cfg.data_dir, store_key.as_ref())?);

    // Identity: reuse existing user/device or create on first run.
    let ids = pai_identity::IdentityStore::new(store.clone());
    let key_dir = cfg.data_dir.join("keys");
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
    Ok(Base {
        cfg,
        store,
        key_dir,
        user,
        device,
    })
}

pub(crate) async fn build(cli: &Cli) -> Result<(Ctx, pai_config::Config)> {
    let b = base(cli).await?;
    let (cfg, store, user, device) = (b.cfg, b.store, b.user, b.device);

    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));
    let conversations = Arc::new(ConversationStore::new(store.clone()));
    let runs = Arc::new(RunStore::new(store.clone()));
    let session = conversations.get_or_create_session(user.id, device.id)?;

    // Provider resolution. "auto" probes live endpoints (needs a runtime).
    let mut provider_name = cli.provider.clone();
    let mut model = cli.model.clone();
    let mut server_url = cfg.inference.local_server_url.clone();
    // An inference.json process adapter is an explicit on-device
    // choice — auto prefers it over probing HTTP endpoints.
    let process_adapter = pai_inference::InferenceFileConfig::load(&cfg.data_dir)?
        .and_then(|c| c.process)
        .and_then(pai_inference::ProcessInferenceProvider::detect);
    if provider_name == "auto" && process_adapter.is_some() {
        provider_name = "process".into();
        eprintln!("auto: using process adapter from inference.json");
    }
    if provider_name == "auto" {
        match pai_inference::detect_endpoints(std::time::Duration::from_secs(2)).await {
            found if !found.is_empty() => {
                let ep = &found[0];
                eprintln!("auto: using {} at {}", ep.provider, ep.base_url);
                server_url = ep.base_url.clone();
                if model.is_none() {
                    model = pai_inference::pick_chat_model(&ep.models);
                }
                provider_name = "llama-server".into();
            }
            _ => {
                eprintln!("auto: no local server found — falling back to echo");
                provider_name = "echo".into();
            }
        }
    }

    // Vector recall: probe the resolved server for an Ollama embedding
    // model (/api/tags is Ollama-only, so detection is self-gating).
    let embedder: Option<Arc<pai_memory::OllamaEmbedder>> =
        pai_memory::OllamaEmbedder::detect(&server_url, std::time::Duration::from_secs(2))
            .await
            .map(Arc::new);
    if let Some(e) = &embedder {
        eprintln!("embedder: {}", e.id());
    }
    let mut mem_impl = SqliteMemory::new(store.clone());
    let mut doc_impl = pai_documents::DocumentStore::new(store.clone());
    if let Some(e) = &embedder {
        mem_impl = mem_impl.with_embedder(e.clone());
        doc_impl = doc_impl.with_embedder(e.clone());
    }
    let memory: Arc<dyn MemoryBackend> = Arc::new(mem_impl);
    let documents = Arc::new(doc_impl);
    // Model-driven file reads are jailed to <data_dir>/inbox; user-driven
    // `pai docs ingest <path>` bypasses the jail.
    let inbox = cfg.data_dir.join("inbox");
    std::fs::create_dir_all(&inbox).ok();

    // Connectors: email provider when an account is configured.
    let email: Option<Arc<dyn pai_connector_email::EmailProvider>> =
        pai_connector_email::ImapConfig::load(&cfg.data_dir)?
            .map(|c| Arc::new(pai_connector_email::ImapProvider::new(c)) as _);
    let gitlab: Option<Arc<dyn pai_connector_gitlab::GitLabProvider>> =
        pai_connector_gitlab::GitLabConfig::load(&cfg.data_dir)?
            .map(|c| Arc::new(pai_connector_gitlab::RestGitLab::new(c)) as _);

    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    providers.register(Arc::new(LlamaServerProvider::new(
        &server_url,
        model
            .clone()
            .unwrap_or_else(|| cfg.inference.default_model.clone()),
    )));
    if let Some(p) = process_adapter {
        providers.register(Arc::new(p));
    }

    // Vision: a process adapter from vision.json wins when configured;
    // else llama-server (the only OpenAI-image_url provider we know).
    let vision: Option<Arc<dyn pai_inference::ImageUnderstandingProvider>> = vision_provider(&cfg)
        .or_else(|| {
            (provider_name == "llama-server").then(|| {
                Arc::new(pai_vision::LlamaVisionProvider::new(
                    &server_url,
                    model
                        .clone()
                        .unwrap_or_else(|| cfg.inference.default_model.clone()),
                )) as _
            })
        });

    let audio_gen: Option<Arc<dyn pai_inference::AudioGenerationProvider>> =
        pai_media::providers::detect(&cfg.data_dir, std::time::Duration::from_secs(2))
            .await
            .map(|p| Arc::new(p) as _);

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
        documents: Some(documents.clone()),
        email: email.clone(),
        gitlab: gitlab.clone(),
        vision,
        notify: Some(Arc::new(pai_notify::StoreNotifySink {
            store: store.clone(),
            config: pai_notify::load_config(&cfg.data_dir)?,
            email: email.clone(),
        })),
        apps: Some(Arc::new(pai_agent::appops::StoreAppOperator::new(
            store.clone(),
            cfg.data_dir.clone(),
            device.id,
        ))),
        audio_gen,
        media_dir: Some(cfg.data_dir.join("media")),
        allowed_roots: vec![inbox],
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
            documents,
            email,
            gitlab,
            store,
        },
        cfg,
    ))
}

pub(crate) fn current_platform() -> Platform {
    match std::env::consts::OS {
        "macos" => Platform::MacOs,
        "windows" => Platform::Windows,
        "ios" => Platform::Ios,
        "android" => Platform::Android,
        _ => Platform::Linux,
    }
}

/// `policies` table overlays the shipped defaults.
pub(crate) fn load_policies(store: &Store) -> PolicyTable {
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

pub(crate) fn agent_def(provider: &str, model: Option<String>) -> AgentDefinition {
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

pub(crate) fn print_event(e: &AgentEvent) {
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

pub(crate) struct SendOutcome {
    pub(crate) answer: Option<String>,
    /// True when the answer was already printed token-by-token.
    pub(crate) streamed: bool,
}

pub(crate) async fn send(
    ctx: &Ctx,
    def: &AgentDefinition,
    text: &str,
    conversation: Option<ConversationId>,
    resume_from: Option<AgentRunId>,
    approval: &dyn ApprovalHandler,
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
            approval,
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

/// Interactive approval handler for `chat`: prints the request and reads a
/// y/n answer on stdin. Blocking inside `decide` is fine — the REPL owns
/// the loop, and a run has nothing else to do while awaiting approval.
pub(crate) struct CliApproval;

#[async_trait::async_trait]
impl ApprovalHandler for CliApproval {
    async fn decide(&self, req: &pai_permissions::ApprovalRequest) -> bool {
        println!("  ┌─ approval needed ───────────────────────────");
        println!("  │ tool:        {}", req.tool);
        println!("  │ action:      {}", req.summary);
        println!("  │ permissions: {:?}", req.permissions);
        println!("  │ risk:        {:?}", req.risk);
        print!("  └─ allow? [y/N] ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
    }
}

/// Vision provider selection: a `process` block in `vision.json` wins
/// (its command must resolve on PATH); otherwise None — callers fall
/// back to llama-server.
pub(crate) fn vision_provider(
    cfg: &pai_config::Config,
) -> Option<Arc<dyn pai_inference::ImageUnderstandingProvider>> {
    let pc = pai_vision::VisionFileConfig::load(&cfg.data_dir)
        .ok()
        .flatten()?
        .process?;
    let p = pai_vision::ProcessVisionProvider::detect(pc)?;
    Some(Arc::new(p))
}

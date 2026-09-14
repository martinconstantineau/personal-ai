//! `pai` — Personal AI CLI. Drives the vertical slice end-to-end:
//! Flutter-quality UX is in apps/desktop; this is the same Rust core.

use clap::{Parser, Subcommand};
use pai_agent::{
    AgentDefinition, AgentEvent, AgentRuntime, ApprovalHandler, AutoApprove, CancelToken,
    ConversationStore, DenyApprovals, Persistence, RunRequest, RunStore,
};
use pai_core::*;
use pai_inference::{EchoProvider, LlamaServerProvider};
use pai_memory::{Embedder, MemoryBackend, MemoryScopeQuery, RecallQuery, SqliteMemory};
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
    /// Manage the document store
    Docs {
        #[command(subcommand)]
        cmd: DocsCmd,
    },
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
    /// Device pairing for end-to-end encrypted sync.
    Pair {
        #[command(subcommand)]
        cmd: PairCmd,
    },
    /// Cross-device sync over a shared folder.
    Sync {
        #[command(subcommand)]
        cmd: SyncCmd,
    },
    /// Trusted-device compute: serve ops to paired peers or call one.
    Broker {
        #[command(subcommand)]
        cmd: BrokerCmd,
    },
    /// Email connector (IMAP) — configure + direct ops.
    Email {
        #[command(subcommand)]
        cmd: EmailCmd,
    },
    /// Voice pipeline — whisper-server STT, piper TTS, energy VAD.
    Voice {
        #[command(subcommand)]
        cmd: VoiceCmd,
    },
    /// Background tasks — synced across devices; a due task is claimed by
    /// one device under a lease so it doesn't run everywhere at once.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Describe/answer a question about an image via a local multimodal
    /// model (llama.cpp server with --mmproj).
    Describe {
        /// Image file (png/jpg/webp/gif/bmp).
        image: String,
        /// Question or instruction about the image.
        #[arg(long, default_value = "Describe this image in detail.")]
        prompt: String,
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
enum TaskCmd {
    /// List tasks, newest first.
    List,
    /// Create a task. With --prompt it runs through the agent on
    /// whichever device claims it.
    Add {
        title: String,
        /// When to run: RFC3339 timestamp or +N seconds from now.
        /// Omit to run on the next tick.
        #[arg(long)]
        at: Option<String>,
        /// Repeat every N seconds ("@every Ns" trigger).
        #[arg(long)]
        every: Option<u64>,
        /// Prompt text handed to the agent when the task fires.
        #[arg(long)]
        prompt: Option<String>,
        /// Keep the task on this device (default: synchronized).
        #[arg(long)]
        local: bool,
    },
    /// Soft-delete a task — the tombstone propagates to peers.
    Remove { id: String },
    /// Claim and run due tasks. Claims are pushed before each run when a
    /// transport is given, so peers see them before results land.
    Tick {
        /// Claim lease in seconds — a crashed runner's tasks become
        /// claimable again after this.
        #[arg(long, default_value = "300")]
        lease_secs: i64,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Change a task's sync scope (synchronized | device_local).
    Sync { id: String, mode: String },
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
    /// Set sync scope: synchronized | device-local (default).
    Sync {
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

#[derive(Subcommand)]
enum DocsCmd {
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
#[derive(Subcommand)]
enum PairCmd {
    /// Write a signed pairing offer for another device to accept.
    Offer {
        #[arg(long)]
        out: String,
    },
    /// Accept an offer file; writes the signed accept (carries the vault
    /// key sealed to the offerer).
    Accept {
        offer: String,
        #[arg(long)]
        out: String,
    },
    /// Complete pairing from an accept file; installs the vault key.
    Complete { accept: String },
    /// List trusted peer devices.
    List,
    /// Remove a peer. Note: does NOT rotate the vault key — a removed
    /// peer may still hold it.
    Remove { id: String },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Seal + push local changes to a shared folder or relay.
    Push {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        /// Bearer token for the relay (or PAI_SYNC_TOKEN).
        #[arg(long)]
        token: Option<String>,
    },
    /// Pull + apply remote changes from a shared folder or relay.
    Pull {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Push then pull in one pass.
    Run {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Rotate the vault key and push sealed rotation objects to every
    /// paired peer — they adopt on their next sync pull/run. Use after
    /// `pair remove` to actually revoke a device's access.
    Rotate {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Peers + object count at the destination.
    Status {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Run a sync relay server — stores ciphertext objects under --dir.
    /// Put it behind TLS (reverse proxy) off localhost; the blobs are
    /// sealed anyway, but auth keeps it from being a free object store.
    Serve {
        #[arg(long)]
        dir: String,
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: String,
        /// Require `Authorization: Bearer <token>` (or PAI_SYNC_TOKEN).
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum BrokerCmd {
    /// List paired peer devices (potential trusted executors).
    Devices,
    /// Answer broker requests addressed to this device, forever.
    /// Ops are served by this device's providers: stt (whisper-server),
    /// tts (piper), infer/describe (llama-server).
    Serve {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Poll interval, seconds.
        #[arg(long, default_value = "2")]
        poll_secs: u64,
    },
    /// Send one request to a paired peer and wait for its response.
    Call {
        /// Peer device id or unambiguous prefix (see `broker devices`),
        /// or the literal `any` to route by announced capability.
        device: String,
        /// Operation: stt | tts | infer | describe.
        op: String,
        /// Stream the response — chunks print as they arrive (infer).
        #[arg(long)]
        stream: bool,
        /// UTF-8 payload (tts/infer/describe prompt) — or --file for bytes.
        #[arg(long)]
        text: Option<String>,
        /// Binary payload file (stt WAV, describe image).
        #[arg(long)]
        file: Option<String>,
        /// Write response bytes to a file instead of stdout.
        #[arg(long)]
        out: Option<String>,
        #[arg(long, default_value = "60")]
        timeout_secs: u64,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum EmailCmd {
    /// Configure the IMAP account: writes email.json; the password goes
    /// to the OS keystore (`email:<user>`), never the file.
    Configure,
    /// Show the configured account (never prints the password).
    Status,
    /// Search messages.
    Search {
        query: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value = "10")]
        limit: u32,
    },
    /// Read one message body by id.
    Read { id: String },
    /// Create a draft (the safe send path).
    Draft {
        #[arg(long)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        in_reply_to: Option<String>,
    },
    /// Send immediately via SMTP (needs the `smtp` block in email.json —
    /// `pai email configure` writes one). Drafts remain the default.
    Send {
        #[arg(long)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        in_reply_to: Option<String>,
    },
    /// Archive a message by id.
    Archive { id: String },
    /// Apply a label/mailbox to a message by id.
    Label { id: String, label: String },
    /// Delete a message by id.
    Delete { id: String },
}

#[derive(Subcommand)]
enum VoiceCmd {
    /// Show detected voice providers (whisper-server reachability, piper
    /// binary/model, VAD) and the effective config.
    Status,
    /// Write voice.json: whisper-server URL, piper binary + voice model.
    Configure {
        #[arg(long)]
        whisper_url: Option<String>,
        #[arg(long)]
        piper_bin: Option<String>,
        #[arg(long)]
        piper_model: Option<String>,
    },
    /// Transcribe an audio file (WAV) via whisper-server.
    Transcribe { file: String },
    /// Synthesize text to a WAV file via piper.
    Say {
        text: String,
        #[arg(long, default_value = "reply.wav")]
        out: String,
    },
    /// One conversational turn: WAV in → transcript → agent → reply WAV.
    Turn {
        /// WAV file to transcribe; omit with --mic to capture instead.
        file: Option<String>,
        /// Capture the utterance from the default microphone and play the
        /// spoken reply through the speakers.
        #[arg(long)]
        mic: bool,
        #[arg(long, default_value = "reply.wav")]
        out: String,
    },
    /// Capture one utterance from the mic (VAD-endpointed) and transcribe.
    Listen {
        /// Hard cap on capture length, seconds.
        #[arg(long, default_value = "30")]
        max_secs: u32,
        /// Stream partial transcripts — each pause-finalized segment
        /// transcribes while you keep talking.
        #[arg(long)]
        stream: bool,
    },
}

struct Ctx {
    store: Arc<Store>,
    documents: Arc<pai_documents::DocumentStore>,
    email: Option<Arc<dyn pai_connector_email::EmailProvider>>,
    agent: AgentRuntime,
    memory: Arc<dyn MemoryBackend>,
    audit: Arc<pai_audit::AuditLog>,
    conversations: Arc<ConversationStore>,
    runs: Arc<RunStore>,
    session: SessionId,
    provider_name: String,
    model: Option<String>,
}

/// Config + store + identity without inference/embedder probing — used by
/// commands that don't need a model (pair, sync).
async fn base(
    cli: &Cli,
) -> Result<(
    pai_config::Config,
    Arc<Store>,
    pai_identity::IdentityStore,
    std::path::PathBuf,
    User,
    Device,
)> {
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
    Ok((cfg, store, ids, key_dir, user, device))
}

async fn build(cli: &Cli) -> Result<(Ctx, pai_config::Config)> {
    let (cfg, store, _ids, _key_dir, user, device) = base(cli).await?;

    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));
    let conversations = Arc::new(ConversationStore::new(store.clone()));
    let runs = Arc::new(RunStore::new(store.clone()));
    let session = conversations.get_or_create_session(user.id, device.id)?;

    // Provider resolution. "auto" probes live endpoints (needs a runtime).
    let mut provider_name = cli.provider.clone();
    let mut model = cli.model.clone();
    let mut server_url = cfg.inference.local_server_url.clone();
    if provider_name == "auto" {
        match pai_inference::detect_endpoints(std::time::Duration::from_secs(2)).await {
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

    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    providers.register(Arc::new(LlamaServerProvider::new(
        &server_url,
        model
            .clone()
            .unwrap_or_else(|| cfg.inference.default_model.clone()),
    )));

    // Vision: only llama-server speaks the OpenAI image_url protocol —
    // constructing it for other providers would just fail at call time.
    let vision: Option<Arc<dyn pai_inference::ImageUnderstandingProvider>> =
        (provider_name == "llama-server").then(|| {
            Arc::new(pai_vision::LlamaVisionProvider::new(
                &server_url,
                model
                    .clone()
                    .unwrap_or_else(|| cfg.inference.default_model.clone()),
            )) as _
        });

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
        vision,
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
            store,
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
pub struct CliApproval;

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

fn parse_uuid(s: &str, what: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|_| Error::InvalidInput(format!("invalid {what} id '{s}'")))
}

async fn run_models(cmd: &ModelsCmd, cfg: &pai_config::Config) -> Result<()> {
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

/// Resolve --dir / --relay into a transport; mutually exclusive.
fn sync_transport(
    dir: &Option<String>,
    relay: &Option<String>,
    token: &Option<String>,
) -> Result<Box<dyn pai_sync::SyncTransport>> {
    match (dir, relay) {
        (Some(d), None) => Ok(Box::new(pai_sync::FolderTransport::new(d.into())?)),
        (None, Some(r)) => Ok(Box::new(pai_sync::relay::RelayTransport::new(
            r,
            token.clone(),
        ))),
        (None, None) => Err(Error::InvalidInput("specify --dir or --relay".into())),
        (Some(_), Some(_)) => Err(Error::InvalidInput(
            "--dir and --relay are mutually exclusive".into(),
        )),
    }
}

/// Match a device-id prefix against paired peers.
fn resolve_peer(store: &Store, prefix: &str) -> Result<DeviceId> {
    use pai_sync::pair;
    let peers = pair::list_peers(store)?;
    let matches: Vec<_> = peers
        .iter()
        .filter(|p| p.device_id.to_string().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(Error::NotFound(format!(
            "no paired device matching '{prefix}' — `pai broker devices`"
        ))),
        1 => Ok(matches[0].device_id),
        _ => Err(Error::InvalidInput(format!(
            "'{prefix}' matches {} devices — be more specific",
            matches.len()
        ))),
    }
}

/// Broker op dispatch for `pai broker serve`: each op resolves through
/// whatever this device actually runs — whisper-server (stt), piper
/// (tts), llama-server (infer/describe).
struct BrokerOps {
    stt: Option<pai_voice::WhisperServerStt>,
    tts: Option<pai_voice::PiperTts>,
    server_url: String,
    model: String,
}

impl BrokerOps {
    fn ops(&self) -> Vec<String> {
        let mut v = vec!["infer".to_string(), "describe".to_string()];
        if self.stt.is_some() {
            v.push("stt".into());
        }
        if self.tts.is_some() {
            v.push("tts".into());
        }
        v
    }

    fn describe(&self) -> String {
        self.ops().join(", ")
    }
}

#[async_trait::async_trait]
impl pai_broker::rpc::OpHandler for BrokerOps {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        use base64::Engine as _;
        use pai_inference::{
            ImageUnderstandingProvider, InferenceProvider, SpeechToTextProvider,
            TextToSpeechProvider,
        };
        match op {
            "stt" => {
                let stt = self
                    .stt
                    .as_ref()
                    .ok_or_else(|| Error::Provider("no whisper-server here".into()))?;
                Ok(stt.transcribe(payload, "audio/wav").await?.into_bytes())
            }
            "tts" => {
                let tts = self
                    .tts
                    .as_ref()
                    .ok_or_else(|| Error::Provider("no piper here".into()))?;
                let text = String::from_utf8(payload.to_vec())
                    .map_err(|_| Error::InvalidInput("tts payload must be UTF-8".into()))?;
                tts.synthesize(&text, None).await
            }
            "infer" => {
                let prompt = String::from_utf8(payload.to_vec())
                    .map_err(|_| Error::InvalidInput("infer payload must be UTF-8".into()))?;
                let p = LlamaServerProvider::new(&self.server_url, self.model.clone());
                let req = pai_inference::AIRequest {
                    messages: vec![Message {
                        id: MessageId::new(),
                        conversation: ConversationId::new(),
                        role: Role::User,
                        created_at: now(),
                        content: vec![Content::Text { text: prompt }],
                        trust: TrustLevel::User,
                    }],
                    tools: vec![],
                    model: Some(self.model.clone()),
                    temperature: None,
                    max_tokens: None,
                    require_structured: false,
                };
                Ok(p.generate(&req).await?.text.into_bytes())
            }
            "describe" => {
                #[derive(serde::Deserialize)]
                struct DescribeArgs {
                    image_b64: String,
                    mime: String,
                    prompt: String,
                }
                let args: DescribeArgs = serde_json::from_slice(payload)
                    .map_err(|e| Error::InvalidInput(format!("describe payload JSON: {e}")))?;
                let img = base64::engine::general_purpose::STANDARD
                    .decode(&args.image_b64)
                    .map_err(|e| Error::InvalidInput(format!("describe image_b64: {e}")))?;
                let p = pai_vision::LlamaVisionProvider::new(&self.server_url, self.model.clone());
                Ok(p.describe(&img, &args.mime, &args.prompt)
                    .await?
                    .into_bytes())
            }
            other => Err(Error::InvalidInput(format!("unknown broker op {other}"))),
        }
    }

    /// Streaming infer: llama-server deltas become broker chunks.
    /// Everything else falls back to one-chunk `handle`.
    async fn handle_stream(
        &self,
        op: &str,
        payload: &[u8],
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) -> Result<()> {
        use futures::StreamExt;
        use pai_inference::InferenceProvider;
        if op != "infer" {
            let out = self.handle(op, payload).await?;
            let _ = tx.send(out).await;
            return Ok(());
        }
        let prompt = String::from_utf8(payload.to_vec())
            .map_err(|_| Error::InvalidInput("infer payload must be UTF-8".into()))?;
        let p = LlamaServerProvider::new(&self.server_url, self.model.clone());
        let req = pai_inference::AIRequest {
            messages: vec![Message {
                id: MessageId::new(),
                conversation: ConversationId::new(),
                role: Role::User,
                created_at: now(),
                content: vec![Content::Text { text: prompt }],
                trust: TrustLevel::User,
            }],
            tools: vec![],
            model: Some(self.model.clone()),
            temperature: None,
            max_tokens: None,
            require_structured: false,
        };
        let mut stream = p.stream(req);
        while let Some(ev) = stream.next().await {
            match ev? {
                pai_inference::StreamEvent::Delta(text) => {
                    if tx.send(text.into_bytes()).await.is_err() {
                        break;
                    }
                }
                pai_inference::StreamEvent::Done(_) => break,
                pai_inference::StreamEvent::Error(e) => {
                    return Err(Error::Provider(e));
                }
            }
        }
        Ok(())
    }
}

/// Detect this device's serveable ops (whisper/piper via voice config;
/// llama-server from inference config).
async fn broker_ops(cfg: &pai_config::Config, cli: &Cli) -> BrokerOps {
    let (stt, tts) = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2))
        .await
        .map(|v| (v.stt, v.tts))
        .unwrap_or((None, None));
    let model = cli
        .model
        .clone()
        .unwrap_or_else(|| cfg.inference.default_model.clone());
    BrokerOps {
        stt,
        tts,
        server_url: cfg.inference.local_server_url.clone(),
        model,
    }
}

/// `pai pair` + `pai sync` — store/identity only, no inference stack.
async fn run_sync_cmds(cli: &Cli) -> Result<()> {
    use pai_sync::{crypto, engine, pair, SyncTransport};
    let (cfg, store, ids, key_dir, _user, device) = base(cli).await?;
    match &cli.cmd {
        Cmd::Pair { cmd } => match cmd {
            PairCmd::Offer { out } => {
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let m = pair::make_offer(&device, &agree, &ids, &key_dir)?;
                pair::write_message(&m, std::path::Path::new(out))?;
                println!("offer for '{}' written to {out}", device.name);
                println!("send it to the other device: pai pair accept {out} --out accept.pai");
            }
            PairCmd::Accept { offer, out } => {
                let offer = pair::read_message(std::path::Path::new(offer))?;
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let m = pair::accept_offer(
                    &store,
                    &offer,
                    &device,
                    &agree,
                    &ids,
                    &key_dir,
                    &cfg.data_dir,
                )?;
                pair::write_message(&m, std::path::Path::new(out))?;
                println!(
                    "paired with '{}' ({}…); accept written to {out}",
                    offer.name,
                    &offer.device_id[..8.min(offer.device_id.len())]
                );
                println!("return it to the offering device: pai pair complete {out}");
            }
            PairCmd::Complete { accept } => {
                let accept = pair::read_message(std::path::Path::new(accept))?;
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                pair::complete_pairing(&store, &accept, &agree, &cfg.data_dir)?;
                println!("paired with '{}' — vault key installed", accept.name);
            }
            PairCmd::List => {
                let peers = pair::list_peers(&store)?;
                if peers.is_empty() {
                    println!("no paired devices — see `pai pair offer`");
                }
                for p in peers {
                    println!(
                        "  {}  {:<16} {:<10} paired {}",
                        &p.device_id.to_string()[..8],
                        p.name,
                        p.platform,
                        p.paired_at.format("%Y-%m-%d %H:%M")
                    );
                }
            }
            PairCmd::Remove { id } => {
                if pair::remove_peer(&store, DeviceId(parse_uuid(id, "device")?))? {
                    println!(
                        "removed peer {id} — run `pai sync rotate` to revoke \
                         their access (they keep a stale vault key until then)"
                    );
                } else {
                    println!("no such peer: {id}");
                }
            }
        },
        Cmd::Sync { cmd } => match cmd {
            SyncCmd::Push { dir, relay, token }
            | SyncCmd::Pull { dir, relay, token }
            | SyncCmd::Run { dir, relay, token } => {
                let t = sync_transport(dir, relay, token)?;
                let kind = t.id().to_string();
                // Adopt any pending vault rotation first — objects sealed
                // under the new vault need the new key before pull.
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let adopted = pai_sync::rotate::adopt_rotations(
                    &*t,
                    &store,
                    &cfg.data_dir,
                    &agree,
                    &device,
                    &ids,
                    &key_dir,
                )
                .await?;
                if adopted > 0 {
                    println!(
                        "adopted vault rotation (epoch {})",
                        pai_sync::rotate::vault_epoch(&cfg.data_dir)
                    );
                }
                let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                    Error::Sync("no vault key — pair a device first (pai pair)".into())
                })?;
                let eng = engine::SyncEngine::new(t, store.clone(), vault, device.id);
                let out = match cmd {
                    SyncCmd::Push { .. } => eng.push().await?,
                    SyncCmd::Pull { .. } => eng.pull().await?,
                    _ => eng.run().await?,
                };
                println!(
                    "sync via {kind}: pushed {}, pulled {}, skipped {}",
                    out.pushed, out.pulled, out.skipped
                );
            }
            SyncCmd::Rotate { dir, relay, token } => {
                let t = sync_transport(dir, relay, token)?;
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let n = pai_sync::rotate::push_rotation(
                    &*t,
                    &store,
                    &cfg.data_dir,
                    &device,
                    &agree,
                    &ids,
                    &key_dir,
                )
                .await?;
                println!(
                    "vault rotated (epoch {}) — notified {n} peer(s); \
                     they adopt on their next sync pull/run",
                    pai_sync::rotate::vault_epoch(&cfg.data_dir)
                );
            }
            SyncCmd::Status { dir, relay, token } => {
                let peers = pair::list_peers(&store)?;
                println!("{} paired device(s)", peers.len());
                if dir.is_some() || relay.is_some() {
                    let t = sync_transport(dir, relay, token)?;
                    let metas = t.list().await?;
                    let tombstones = metas.iter().filter(|m| m.tombstone).count();
                    println!(
                        "{}: {} object(s) ({} tombstone(s))",
                        t.id(),
                        metas.len(),
                        tombstones
                    );
                }
            }
            SyncCmd::Serve { dir, addr, token } => {
                let token = token
                    .clone()
                    .or_else(|| std::env::var("PAI_SYNC_TOKEN").ok());
                let srv = pai_sync::relay::bind(dir.into(), addr, token.clone())?;
                println!(
                    "relay on http://{} storing under {dir} {}",
                    srv.addr(),
                    if token.is_some() {
                        "(token required)"
                    } else {
                        "(NO AUTH — localhost use only)"
                    }
                );
                pai_sync::relay::serve(srv);
            }
        },
        Cmd::Broker { cmd } => match cmd {
            BrokerCmd::Devices => {
                let peers = pair::list_peers(&store)?;
                if peers.is_empty() {
                    println!("(no paired devices — `pai pair` first)");
                }
                for p in peers {
                    println!(
                        "  {}  {:<20} {:<10} paired {}",
                        p.device_id,
                        p.name,
                        p.platform,
                        p.paired_at.format("%Y-%m-%d")
                    );
                }
            }
            BrokerCmd::Serve {
                dir,
                relay,
                token,
                poll_secs,
            } => {
                let t = sync_transport(dir, relay, token)?;
                let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                    Error::Sync("no vault key — pair a device first (pai pair)".into())
                })?;
                let ops = broker_ops(&cfg, cli).await;
                let mut srv = pai_broker::rpc::BrokerServer::new(&*t, &vault, device.id, &ops)
                    .with_ops(ops.ops());
                println!(
                    "broker serving {} on {} — ops: {}",
                    device.id,
                    t.id(),
                    ops.describe()
                );
                srv.serve(std::time::Duration::from_secs(*poll_secs)).await;
            }
            BrokerCmd::Call {
                device: dev,
                op,
                stream,
                text,
                file,
                out,
                timeout_secs,
                dir,
                relay,
                token,
            } => {
                let t = sync_transport(dir, relay, token)?;
                let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                    Error::Sync("no vault key — pair a device first (pai pair)".into())
                })?;
                let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, device.id);
                let to = if dev == "any" {
                    client.find_peer(op).await?.ok_or_else(|| {
                        Error::NotFound(format!(
                            "no paired device advertises '{op}' — is `pai broker serve` running?"
                        ))
                    })?
                } else {
                    resolve_peer(&store, dev)?
                };
                let payload = if let Some(f) = file {
                    std::fs::read(f).map_err(|e| Error::InvalidInput(format!("{f}: {e}")))?
                } else if let Some(t) = text {
                    t.clone().into_bytes()
                } else {
                    return Err(Error::InvalidInput("pass --text or --file".into()));
                };
                let timeout = std::time::Duration::from_secs(*timeout_secs);
                let resp = if *stream {
                    client
                        .call_stream(to, op, &payload, timeout, &mut |chunk| {
                            if let Ok(s) = std::str::from_utf8(chunk) {
                                print!("{s}");
                                std::io::Write::flush(&mut std::io::stdout()).ok();
                            }
                        })
                        .await?
                } else {
                    client.call(to, op, &payload, timeout).await?
                };
                if let Some(f) = out {
                    std::fs::write(f, &resp).map_err(|e| Error::Storage(e.to_string()))?;
                    println!("wrote {f} ({} bytes)", resp.len());
                } else {
                    match String::from_utf8(resp.clone()) {
                        Ok(s) => println!("{s}"),
                        Err(_) => println!("({} bytes, binary — use --out)", resp.len()),
                    }
                }
            }
        },
        Cmd::Email { cmd } => run_email_cmds(cmd, &cfg).await?,
        Cmd::Describe { image, prompt } => {
            use pai_inference::ImageUnderstandingProvider;
            let path = std::path::Path::new(image);
            let mime = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(pai_vision::mime_for_ext)
                .unwrap_or("image/png");
            let bytes =
                std::fs::read(path).map_err(|e| Error::InvalidInput(format!("{image}: {e}")))?;
            let model = cfg.inference.default_model.clone();
            let p = pai_vision::LlamaVisionProvider::new(&cfg.inference.local_server_url, model);
            match p.describe(&bytes, mime, prompt).await {
                Ok(text) => println!("{text}"),
                Err(e) => {
                    eprintln!("{e}");
                    eprintln!(
                        "hint: serve a multimodal model — e.g. llama-server \
                         -m model.gguf --mmproj mmproj.gguf --port 8080"
                    );
                    return Err(e);
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// Voice ops. `transcribe`/`say` use one provider each; `turn` runs the
/// full Mic→VAD→STT→Agent→TTS→Speaker pipeline (file-based I/O — live mic
/// capture is the next step, needs OS audio permissions).
async fn run_voice_cmds(cmd: &VoiceCmd, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    use pai_inference::{SpeechToTextProvider, TextToSpeechProvider};
    use pai_voice::{UtteranceHandler, VoicePipeline};
    match cmd {
        VoiceCmd::Status => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            println!(
                "whisper-server: {}",
                if s.stt.is_some() {
                    format!("reachable at {}", s.cfg.whisper_url())
                } else {
                    format!("NOT reachable ({})", s.cfg.whisper_url())
                }
            );
            println!(
                "piper: {}",
                if s.tts.is_some() {
                    "found"
                } else {
                    "NOT found (set `pai voice configure --piper-bin/--piper-model` or PAI_PIPER_MODEL)"
                }
            );
            println!("vad: energy-vad (always available)");
            println!(
                "mic: {} | speaker: {}",
                if pai_voice::mic::input_available() {
                    "default input found"
                } else {
                    "NONE"
                },
                if pai_voice::mic::output_available() {
                    "default output found"
                } else {
                    "NONE"
                }
            );
        }
        VoiceCmd::Configure {
            whisper_url,
            piper_bin,
            piper_model,
        } => {
            let mut c = pai_voice::VoiceConfig::load(&cfg.data_dir)?;
            if let Some(u) = whisper_url {
                c.whisper_url = Some(u.clone());
            }
            if let Some(b) = piper_bin {
                c.piper_bin = Some(b.into());
            }
            if let Some(m) = piper_model {
                c.piper_model = Some(m.into());
            }
            c.save(&cfg.data_dir)?;
            println!("voice.json written — `pai voice status` to verify");
        }
        VoiceCmd::Transcribe { file } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let stt = s.stt.ok_or_else(|| {
                Error::Provider(
                    "whisper-server unreachable — start it or `pai voice configure --whisper-url`"
                        .into(),
                )
            })?;
            let audio =
                std::fs::read(file).map_err(|e| Error::InvalidInput(format!("{file}: {e}")))?;
            let text = stt.transcribe(&audio, "audio/wav").await?;
            println!("{text}");
        }
        VoiceCmd::Say { text, out } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let tts = s.tts.ok_or_else(|| {
                Error::Provider(
                    "piper not found — `pai voice configure --piper-bin/--piper-model`".into(),
                )
            })?;
            let wav = tts.synthesize(text, None).await?;
            std::fs::write(out, &wav).map_err(|e| Error::Storage(e.to_string()))?;
            println!("wrote {out} ({} bytes)", wav.len());
        }
        VoiceCmd::Turn { file, mic, out } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let pipeline: VoicePipeline = s.pipeline().ok_or_else(|| {
                Error::Provider(
                    "voice turn needs both whisper-server AND piper — `pai voice status`".into(),
                )
            })?;
            let audio = if *mic {
                println!("listening… (speak, then pause)");
                let pcm = pai_voice::mic::capture_utterance(&pai_voice::EnergyVad::default(), 30)?;
                if pcm.is_empty() {
                    println!("(nothing heard)");
                    return Ok(());
                }
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                pai_voice::pcm16_to_wav(&bytes, pai_voice::mic::TARGET_RATE)
            } else {
                let f = file
                    .as_deref()
                    .ok_or_else(|| Error::InvalidInput("pass a WAV file or --mic".into()))?;
                std::fs::read(f).map_err(|e| Error::InvalidInput(format!("{f}: {e}")))?
            };
            let def = agent_def(&ctx.provider_name, ctx.model.clone());
            struct AgentVoice<'a> {
                ctx: &'a Ctx,
                def: &'a AgentDefinition,
            }
            #[async_trait::async_trait]
            impl UtteranceHandler for AgentVoice<'_> {
                async fn respond(&self, transcript: &str) -> Result<String> {
                    let out =
                        send(self.ctx, self.def, transcript, None, None, &CliApproval).await?;
                    Ok(out.answer.unwrap_or_else(|| "(no reply)".into()))
                }
            }
            let handler = AgentVoice { ctx, def: &def };
            let (transcript, speech) = pipeline.turn(&audio, "audio/wav", &handler).await?;
            println!("you said: {transcript}");
            if *mic {
                let (rate, pcm) = pai_voice::mic::wav_to_pcm16(&speech)?;
                pai_voice::mic::play(&pcm, rate)?;
                println!("reply spoken");
            } else {
                std::fs::write(out, &speech).map_err(|e| Error::Storage(e.to_string()))?;
                println!("reply → {out} ({} bytes)", speech.len());
            }
        }
        VoiceCmd::Listen { max_secs, stream } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let stt = s.stt.ok_or_else(|| {
                Error::Provider(
                    "whisper-server unreachable — start it or `pai voice configure --whisper-url`"
                        .into(),
                )
            })?;
            if *stream {
                println!("listening… (partials print as segments close)");
                let mut n = 0usize;
                let text = pai_voice::stream_transcribe(
                    &stt,
                    &pai_voice::EnergyVad::default(),
                    *max_secs,
                    &mut |part| {
                        n += 1;
                        println!("  [{n}] {part}");
                    },
                )?;
                if text.is_empty() {
                    println!("(nothing heard)");
                } else {
                    println!("final: {text}");
                }
            } else {
                println!("listening… (speak, then pause)");
                let pcm =
                    pai_voice::mic::capture_utterance(&pai_voice::EnergyVad::default(), *max_secs)?;
                if pcm.is_empty() {
                    println!("(nothing heard)");
                    return Ok(());
                }
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                let wav = pai_voice::pcm16_to_wav(&bytes, pai_voice::mic::TARGET_RATE);
                let text = stt.transcribe(&wav, "audio/wav").await?;
                println!("{text}");
            }
        }
    }
    Ok(())
}

async fn email_provider(cfg: &pai_config::Config) -> Result<pai_connector_email::ImapProvider> {
    let c = pai_connector_email::ImapConfig::load(&cfg.data_dir)?.ok_or_else(|| {
        Error::InvalidInput("no email account — run `pai email configure`".into())
    })?;
    Ok(pai_connector_email::ImapProvider::new(c))
}

async fn run_email_cmds(cmd: &EmailCmd, cfg: &pai_config::Config) -> Result<()> {
    use pai_connector_email::EmailProvider;
    match cmd {
        EmailCmd::Configure => {
            let read = |prompt: &str, default: &str| -> Result<String> {
                print!("{prompt} [{default}]: ");
                std::io::Write::flush(&mut std::io::stdout()).ok();
                let mut s = String::new();
                std::io::stdin()
                    .read_line(&mut s)
                    .map_err(|e| Error::Other(e.to_string()))?;
                let s = s.trim();
                Ok(if s.is_empty() {
                    default.to_string()
                } else {
                    s.to_string()
                })
            };
            let host = read("IMAP host", "imap.gmail.com")?;
            let port: u16 = read("Port", "993")?
                .parse()
                .map_err(|_| Error::InvalidInput("bad port".into()))?;
            let user = read("User (email address)", "")?;
            if user.is_empty() {
                return Err(Error::InvalidInput("user is required".into()));
            }
            let mailbox = read("Mailbox", "INBOX")?;
            let drafts = read("Drafts mailbox", "[Gmail]/Drafts")?;
            let archive = read("Archive mailbox", "[Gmail]/All Mail")?;
            // Optional SMTP submission — empty host keeps drafts-only.
            let smtp_host = read("SMTP host (empty = drafts only)", "smtp.gmail.com")?;
            let smtp = if smtp_host.is_empty() {
                None
            } else {
                let smtp_port: u16 = read("SMTP port", "465")?
                    .parse()
                    .map_err(|_| Error::InvalidInput("bad smtp port".into()))?;
                let smtp_tls = match read("SMTP TLS (tls/starttls/none)", "tls")?.as_str() {
                    "tls" => pai_connector_email::SmtpTls::Tls,
                    "starttls" => pai_connector_email::SmtpTls::StartTls,
                    "none" => pai_connector_email::SmtpTls::None,
                    other => {
                        return Err(Error::InvalidInput(format!(
                            "bad tls mode {other:?} — tls|starttls|none"
                        )))
                    }
                };
                Some(pai_connector_email::SmtpConfig {
                    host: smtp_host,
                    port: smtp_port,
                    tls: smtp_tls,
                    user: None, // same login as IMAP
                })
            };
            let password =
                rpassword::prompt_password("Password (app password for Gmail/Outlook): ")
                    .map_err(|e| Error::Other(e.to_string()))?;
            let c = pai_connector_email::ImapConfig {
                host,
                port,
                user: user.clone(),
                mailbox,
                drafts_mailbox: drafts,
                archive_mailbox: archive,
                smtp,
            };
            c.save(&cfg.data_dir)?;
            if password.is_empty() {
                println!("no password stored — set PAI_EMAIL_PASSWORD at run time");
            } else if pai_connector_email::imap::store_password(&user, &password) {
                println!("password stored in OS keystore (email:{user})");
            } else {
                println!("keystore unavailable — set PAI_EMAIL_PASSWORD at run time");
            }
            println!("account written to {}/email.json", cfg.data_dir.display());
        }
        EmailCmd::Status => match pai_connector_email::ImapConfig::load(&cfg.data_dir)? {
            Some(c) => {
                println!("imap://{}:{}/{}", c.user, c.host, c.mailbox);
                println!(
                    "drafts: {}  archive: {}",
                    c.drafts_mailbox, c.archive_mailbox
                );
                match &c.smtp {
                    Some(s) => println!("smtp: {}:{} ({:?}) — send enabled", s.host, s.port, s.tls),
                    None => println!("smtp: not configured — drafts only"),
                }
                let pw = pai_connector_email::imap::resolve_password(&c.user).is_ok();
                println!("password: {}", if pw { "available" } else { "MISSING" });
            }
            None => println!("not configured — run `pai email configure`"),
        },
        EmailCmd::Search {
            query,
            from,
            label,
            unread,
            limit,
        } => {
            let p = email_provider(cfg).await?;
            let hits = p
                .search(&pai_connector_email::EmailSearch {
                    query: query.clone(),
                    from: from.clone(),
                    label: label.clone(),
                    unread_only: *unread,
                    limit: *limit,
                    ..Default::default()
                })
                .await?;
            for m in &hits {
                println!(
                    "  {:>6}  {:<40} {:<45} {}",
                    m.id,
                    m.from.address,
                    m.subject,
                    m.received_at.format("%Y-%m-%d")
                );
            }
            if hits.is_empty() {
                println!("no messages");
            }
        }
        EmailCmd::Read { id } => {
            let m = email_provider(cfg).await?.read(id).await?;
            println!("from: {}", m.summary.from.address);
            println!(
                "to:   {}",
                m.summary
                    .to
                    .iter()
                    .map(|a| a.address.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("date: {}", m.summary.received_at.format("%Y-%m-%d %H:%M"));
            println!("subject: {}\n", m.summary.subject);
            println!("{}", m.body_text.unwrap_or_else(|| "(no text body)".into()));
            for a in &m.attachments {
                println!(
                    "  attachment: {} ({}, {} bytes)",
                    a.filename, a.mime, a.size_bytes
                );
            }
        }
        EmailCmd::Draft {
            to,
            cc,
            subject,
            body,
            in_reply_to,
        } => {
            let addr = |a: &String| pai_connector_email::EmailAddress {
                name: None,
                address: a.clone(),
            };
            let id = email_provider(cfg)
                .await?
                .create_draft(&pai_connector_email::Draft {
                    to: to.iter().map(addr).collect(),
                    cc: cc.iter().map(addr).collect(),
                    subject: subject.clone(),
                    body: body.clone(),
                    in_reply_to: in_reply_to.clone(),
                })
                .await?;
            println!("{id}");
        }
        EmailCmd::Send {
            to,
            cc,
            subject,
            body,
            in_reply_to,
        } => {
            let addr = |a: &String| pai_connector_email::EmailAddress {
                name: None,
                address: a.clone(),
            };
            email_provider(cfg)
                .await?
                .send(&pai_connector_email::Draft {
                    to: to.iter().map(addr).collect(),
                    cc: cc.iter().map(addr).collect(),
                    subject: subject.clone(),
                    body: body.clone(),
                    in_reply_to: in_reply_to.clone(),
                })
                .await?;
            println!("sent");
        }
        EmailCmd::Archive { id } => {
            email_provider(cfg).await?.archive(id).await?;
            println!("archived {id}");
        }
        EmailCmd::Label { id, label } => {
            email_provider(cfg).await?.label(id, label).await?;
            println!("labeled {id} → {label}");
        }
        EmailCmd::Delete { id } => {
            email_provider(cfg).await?.delete(id).await?;
            println!("deleted {id}");
        }
    }
    Ok(())
}

/// Runs a claimed task through the agent when its payload is a prompt.
/// Approvals are denied — a background tick has nobody to ask, so
/// interactive-permission actions are refused, not silently granted.
struct PromptTaskHandler<'a> {
    ctx: &'a Ctx,
    def: &'a AgentDefinition,
}

#[async_trait::async_trait]
impl pai_tasks::TaskHandler for PromptTaskHandler<'_> {
    async fn handle(&self, t: &pai_tasks::ScheduledTask) -> Result<()> {
        if t.payload.get("kind").and_then(|k| k.as_str()) != Some("prompt") {
            return Ok(()); // marker/reminder payloads just complete
        }
        let text = t
            .payload
            .get("text")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        let out = send(self.ctx, self.def, text, None, None, &DenyApprovals).await?;
        pai_tasks::store::set_result(
            &self.ctx.store,
            t.task.id,
            serde_json::json!({"reply": out.answer}),
        )?;
        Ok(())
    }
}

/// `pai task …` — synced background tasks with claim/lease execution.
async fn run_task_cmds(
    cmd: &TaskCmd,
    ctx: &Ctx,
    provider: &str,
    model: Option<String>,
    cfg: &pai_config::Config,
) -> Result<()> {
    use pai_tasks::{store as ts, TaskHandler};
    match cmd {
        TaskCmd::List => {
            let me = ctx.agent.device.to_string();
            for t in ts::list_tasks(&ctx.store, false)? {
                let claim = match (&t.claimed_by, &t.lease_expires_at) {
                    (Some(c), Some(l)) => format!(
                        " claimed:{}{}",
                        if c.to_string() == me {
                            "me".to_string()
                        } else {
                            c.to_string()[..8].to_string()
                        },
                        if *l > now() { "·live" } else { "·expired" }
                    ),
                    _ => String::new(),
                };
                println!(
                    "{id:.8}  {state:<8} {run:<20}{claim}  {title}",
                    id = t.id.to_string(),
                    state = serde_json::to_string(&t.state)
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string(),
                    run = t
                        .run_at
                        .map(|r| pai_storage::ts(&r))
                        .unwrap_or_else(|| "on-tick".into()),
                    claim = claim,
                    title = t.title,
                );
            }
        }
        TaskCmd::Add {
            title,
            at,
            every,
            prompt,
            local,
        } => {
            let run_at = match at {
                Some(s) => Some(parse_at(s)?),
                None => None,
            };
            let trigger = every
                .map(|s| Trigger::Schedule {
                    cron: format!("@every {s}s"),
                })
                .unwrap_or(Trigger::Manual);
            let payload = match prompt {
                Some(p) => serde_json::json!({"kind": "prompt", "text": p}),
                None => serde_json::json!({}),
            };
            let scope = if *local {
                SyncScope::DeviceLocal
            } else {
                SyncScope::Synchronized
            };
            let id = ts::create_task(
                &ctx.store,
                title,
                AgentId(uuid::Uuid::nil()),
                run_at,
                &trigger,
                &payload,
                scope,
            )?;
            println!(
                "task {id} created{}{}",
                if *local { " (device-local)" } else { "" },
                run_at
                    .map(|r| format!(" — runs {}", pai_storage::ts(&r)))
                    .unwrap_or_default()
            );
        }
        TaskCmd::Remove { id } => {
            if ts::remove_task(&ctx.store, TaskId(parse_uuid(id, "task")?))? {
                println!("removed task {id}");
            } else {
                println!("no such task: {id}");
            }
        }
        TaskCmd::Sync { id, mode } => {
            let scope = match mode.as_str() {
                "synchronized" | "sync" => SyncScope::Synchronized,
                "device_local" | "local" => SyncScope::DeviceLocal,
                _ => {
                    return Err(Error::InvalidInput(
                        "scope must be 'synchronized' or 'device_local'".into(),
                    ))
                }
            };
            if ts::set_sync_scope(&ctx.store, TaskId(parse_uuid(id, "task")?), scope)? {
                println!("task {id} → {mode}");
            } else {
                println!("no such task: {id}");
            }
        }
        TaskCmd::Tick {
            lease_secs,
            dir,
            relay,
            token,
        } => {
            // Optional engine to advertise claims/results over a transport.
            let engine = match (dir.is_some() || relay.is_some())
                .then(|| sync_transport(dir, relay, token))
            {
                Some(Ok(t)) => {
                    let vault = pai_sync::crypto::vault_key(&cfg.data_dir)?
                        .ok_or_else(|| Error::Sync("no vault key — pair a device first".into()))?;
                    Some(pai_sync::engine::SyncEngine::new(
                        t,
                        ctx.store.clone(),
                        vault,
                        ctx.agent.device,
                    ))
                }
                Some(Err(e)) => return Err(e),
                None => None,
            };
            let def = agent_def(provider, model);
            let handler = PromptTaskHandler { ctx, def: &def };
            let mut ran = 0usize;
            for t in ts::due_tasks(&ctx.store, now())? {
                if !ts::claim_task(&ctx.store, t.id, ctx.agent.device, *lease_secs)? {
                    println!("{:.8}  claimed by another device — skipped", t.id);
                    continue;
                }
                // Advertise the claim before running — shrinks the window
                // where a peer could claim the same task.
                if let Some(e) = &engine {
                    e.push().await.ok();
                }
                let ok = handler.handle(&t.scheduled()).await.is_ok();
                if ok {
                    if let Some(next) = pai_tasks::next_fire(&t.trigger, now()) {
                        ts::requeue_task(&ctx.store, t.id, next)?;
                        println!("{:.8}  ran — requeued for {}", t.id, pai_storage::ts(&next));
                        ran += 1;
                        continue;
                    }
                }
                ts::finish_task(
                    &ctx.store,
                    t.id,
                    if ok {
                        TaskState::Done
                    } else {
                        TaskState::Failed
                    },
                    (!ok).then(|| serde_json::json!({"error": "handler failed"})),
                )?;
                println!("{:.8}  {}", t.id, if ok { "done" } else { "FAILED" });
                ran += 1;
            }
            if let Some(e) = &engine {
                let out = e.push().await?;
                println!("pushed {0} sync object(s)", out.pushed);
            }
            println!("{ran} task(s) ran");
        }
    }
    Ok(())
}

/// "--at" accepts RFC3339 or "+N" seconds from now.
fn parse_at(s: &str) -> Result<Timestamp> {
    if let Some(secs) = s.strip_prefix('+').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(now() + chrono::Duration::seconds(secs));
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|e| Error::InvalidInput(format!("bad --at '{s}' (RFC3339 or +N secs): {e}")))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // Pair/sync/broker/email/describe need no agent — skip provider probing.
    if matches!(
        cli.cmd,
        Cmd::Pair { .. }
            | Cmd::Sync { .. }
            | Cmd::Broker { .. }
            | Cmd::Email { .. }
            | Cmd::Describe { .. }
    ) {
        return run_sync_cmds(&cli).await;
    }

    let (ctx, cfg) = build(&cli).await?;

    match cli.cmd {
        Cmd::Docs { cmd } => match cmd {
            DocsCmd::Ingest { path, sync } => {
                // User-initiated: no jail — they named the file.
                let canon = std::fs::canonicalize(&path)
                    .map_err(|e| Error::InvalidInput(format!("{path}: {e}")))?;
                let bytes = std::fs::read(&canon)
                    .map_err(|e| Error::InvalidInput(format!("{canon:?}: {e}")))?;
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
        },
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
                &AutoApprove,
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
                &AutoApprove,
            )
            .await?;
            if !r.streamed {
                println!("assistant: {}", r.answer.unwrap_or_default());
            }
            println!();

            println!("user: What is 41 + 1?");
            let r = send(
                &ctx,
                &def,
                "What is 41 + 1?",
                Some(conv.id),
                None,
                &AutoApprove,
            )
            .await?;
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
                match send(&ctx, &def, line, Some(conv), None, &CliApproval).await {
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
            ConvCmd::Sync { id, mode } => {
                let sc = match mode.as_str() {
                    "synchronized" => SyncScope::Synchronized,
                    "device-local" | "device_local" => SyncScope::DeviceLocal,
                    _ => {
                        return Err(Error::InvalidInput(
                            "mode: synchronized|device-local".into(),
                        ))
                    }
                };
                ctx.conversations
                    .set_sync_scope(ConversationId(parse_uuid(&id, "conversation")?), sc)?;
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
                send(&ctx, &def, "", None, Some(rid), &CliApproval).await?;
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
                let store = ctx.store.as_ref();
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
                    documents: Some(ctx.documents.as_ref()),
                    email: ctx.email.as_deref(),
                    vision: None,
                    allowed_roots: &[],
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
        Cmd::Voice { cmd } => run_voice_cmds(&cmd, &ctx, &cfg).await?,
        Cmd::Task { cmd } => {
            run_task_cmds(&cmd, &ctx, &cli.provider, cli.model.clone(), &cfg).await?
        }
        Cmd::Pair { .. }
        | Cmd::Sync { .. }
        | Cmd::Broker { .. }
        | Cmd::Email { .. }
        | Cmd::Describe { .. } => {
            unreachable!("handled before build")
        }
    }
    Ok(())
}

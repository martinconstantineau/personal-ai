//! C ABI for the Flutter UI. Convention: JSON strings in, JSON strings out,
//! caller frees every returned pointer with `pai_free_string`.
//!
//! - `pai_init(config_json)` → opaque runtime handle
//! - `pai_send(handle, user_text)` → JSON `{events: [...], answer, run_id}`
//! - `pai_set_event_callback(handle, cb, user_data)` — live `AgentEvent`s as
//!   JSON while a run executes (token streaming, approval requests, ...)
//! - `pai_approve(handle, call_id, granted)` — resolve a pending approval
//! - `pai_audit(handle, limit)` / `pai_memories(handle)` / `pai_forget`
//! - `pai_conversations*` — list / new / select / rename / delete / scope
//! - `pai_policies` / `pai_set_policy` — the permission policy editor
//! - `pai_runs` / `pai_resume` — interrupted-run recovery
//! - `pai_detect` — probe local inference endpoints + binaries
//! - `pai_free(handle)` / `pai_free_string(ptr)`
//!
//! `pai_send` and `pai_resume` are blocking (call them from a Dart isolate);
//! the event callback delivers incremental progress while they run.

use base64::Engine as _;
use pai_agent::{
    AgentDefinition, AgentRuntime, ApprovalHandler, CancelToken, ConversationStore, Persistence,
    RunRequest, RunStore,
};
use pai_core::*;
use pai_inference::{
    AudioGenerationProvider, EchoProvider, LlamaServerProvider, SpeechToTextProvider,
    TextToSpeechProvider,
};
use pai_memory::{MemoryBackend, MemoryScopeQuery, RecallQuery, SqliteMemory};
use pai_models::ModelManager;
use pai_permissions::{all_permissions, Permission, PolicyEngine, PolicyTable};
use pai_storage::Store;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// `void cb(const char* json_event, void* user_data)` — invoked on the
/// thread running `pai_send` for every [`pai_agent::AgentEvent`].
pub type PaiEventCallback = extern "C" fn(*const c_char, *mut std::ffi::c_void);

pub struct PaiRuntime {
    rt: tokio::runtime::Runtime,
    agent: AgentRuntime,
    memory: Arc<dyn MemoryBackend>,
    audit: Arc<pai_audit::AuditLog>,
    conversations: Arc<ConversationStore>,
    runs: Arc<RunStore>,
    def: AgentDefinition,
    session: SessionId,
    /// The conversation `pai_send` currently writes into.
    conversation: ConversationId,
    /// Serializes `pai_send`/`pai_resume` — callers may invoke from any
    /// thread/isolate.
    send_lock: Mutex<()>,
    /// Pending tool approvals: call_id → resolver for `ApprovalHandler`.
    pending: Arc<Mutex<HashMap<ToolCallId, oneshot::Sender<bool>>>>,
    /// UI event sink (callback fn + user_data pointer, stored as usize for
    /// Send). Set via `pai_set_event_callback`.
    event_cb: Arc<Mutex<Option<(PaiEventCallback, usize)>>>,
    /// Cancel handle for the in-flight run.
    cancel: Arc<Mutex<CancelToken>>,
    store: Arc<Store>,
    data_dir: String,
    device: DeviceId,
    /// Full device record — pairing ops (`make_offer`, `accept_offer`)
    /// need name/platform/pubkey, not just the id.
    device_rec: pai_core::Device,
    documents: Arc<pai_documents::DocumentStore>,
    email: Option<Arc<dyn pai_connector_email::EmailProvider>>,
    /// Detected voice providers (whisper-server STT / piper TTS) — None
    /// when neither is configured. Mic/speaker probes are cheap enough
    /// to answer live in `pai_voice_status`.
    voice: Option<pai_voice::VoiceSetup>,
    /// Base URL the chat provider points at — kept so `pai_set_provider`
    /// can rebuild it on a different model/endpoint without re-init.
    server_url: String,
    /// llama-server child spawned by `pai_models_serve` — killed on free
    /// and replaced when a different model is served.
    llama_child: Mutex<Option<std::process::Child>>,
    /// Slug that child is serving — the Devices screen badges it.
    serving_slug: Mutex<Option<String>>,
}

#[derive(Deserialize)]
struct InitConfig {
    data_dir: String,
    /// "echo", "llama-server", or "auto" (probe local endpoints).
    provider: Option<String>,
    model: Option<String>,
    server_url: Option<String>,
    user_name: Option<String>,
    device_name: Option<String>,
    /// Restore this conversation as active instead of starting a new one.
    conversation: Option<String>,
    /// New conversations' memory scope: "shared" (default) or "isolated".
    memory_scope: Option<String>,
}

/// An `ApprovalHandler` that parks the run until the UI resolves it via
/// `pai_approve` — with a hard timeout so a closed UI can't hang a run.
struct UiApproval {
    pending: Arc<Mutex<HashMap<ToolCallId, oneshot::Sender<bool>>>>,
}

#[async_trait::async_trait]
impl ApprovalHandler for UiApproval {
    async fn decide(&self, req: &pai_permissions::ApprovalRequest) -> bool {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req.id, tx);
        // Fail closed: unanswered approvals resolve to denied.
        matches!(
            tokio::time::timeout(Duration::from_secs(300), rx).await,
            Ok(Ok(true))
        )
    }
}

fn init_runtime(cfg: InitConfig) -> Result<PaiRuntime> {
    let data_dir = std::path::PathBuf::from(&cfg.data_dir);
    std::fs::create_dir_all(&data_dir).map_err(|e| Error::Storage(e.to_string()))?;
    let key = pai_identity::keystore::store_key(&data_dir);
    let store = Arc::new(Store::open(&data_dir, key.as_ref())?);
    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Other(e.to_string()))?;

    let ids = pai_identity::IdentityStore::new(store.clone());
    // Reuse the persisted identity — a fresh user/device per launch
    // would make pairing ephemeral and audit rows untrustworthy.
    let user = match store.with_conn(|c| {
        c.query_row("SELECT id FROM users LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
    }) {
        Ok(id) => ids.get_user(UserId(
            uuid::Uuid::parse_str(&id).map_err(|e| Error::Storage(format!("users.id: {e}")))?,
        ))?,
        Err(_) => ids.create_user(cfg.user_name.as_deref().unwrap_or("user"))?,
    };
    let key_dir = data_dir.join("keys");
    let device = match ids.list_devices(user.id)?.into_iter().next() {
        Some(d) => d,
        None => ids.register_device(
            user.id,
            cfg.device_name.as_deref().unwrap_or("this-device"),
            current_platform(),
            pai_identity::probe_capabilities(),
            &key_dir,
        )?,
    };

    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));
    let conversations = Arc::new(ConversationStore::new(store.clone()));
    let runs = Arc::new(RunStore::new(store.clone()));
    let session = conversations.get_or_create_session(user.id, device.id)?;

    // Provider: explicit choice, or auto-detect a live local endpoint.
    let mut provider_name = cfg.provider.unwrap_or_else(|| "echo".into());
    let mut model = cfg.model.clone();
    let mut server_url = cfg
        .server_url
        .unwrap_or_else(|| "http://127.0.0.1:8080".into());
    if provider_name == "auto" {
        match rt.block_on(pai_inference::detect_endpoints(Duration::from_secs(2))) {
            found if !found.is_empty() => {
                let ep = &found[0];
                server_url = ep.base_url.clone();
                if model.is_none() {
                    model = pai_inference::pick_chat_model(&ep.models);
                }
                provider_name = "llama-server".into();
            }
            _ => provider_name = "echo".into(),
        }
    }

    // Vector recall: probe the resolved server for an Ollama embedding
    // model (/api/tags is Ollama-only, so detection is self-gating).
    let embedder = rt
        .block_on(pai_memory::OllamaEmbedder::detect(
            &server_url,
            Duration::from_secs(2),
        ))
        .map(Arc::new);
    let mut mem_impl = SqliteMemory::new(store.clone());
    let mut doc_impl = pai_documents::DocumentStore::new(store.clone());
    if let Some(e) = &embedder {
        mem_impl = mem_impl.with_embedder(e.clone());
        doc_impl = doc_impl.with_embedder(e.clone());
    }
    let memory: Arc<dyn MemoryBackend> = Arc::new(mem_impl);
    let documents = Arc::new(doc_impl);
    // Tool-file jail: model-driven reads confined to <data_dir>/inbox.
    let inbox = data_dir.join("inbox");
    std::fs::create_dir_all(&inbox).ok();

    let email: Option<Arc<dyn pai_connector_email::EmailProvider>> =
        pai_connector_email::ImapConfig::load(&data_dir)?
            .map(|c| Arc::new(pai_connector_email::ImapProvider::new(c)) as _);

    // Voice: probe whisper-server + piper once at init (2s budget). A
    // missing provider just means the corresponding FFI op errors.
    let voice = rt
        .block_on(pai_voice::detect(&data_dir, Duration::from_secs(2)))
        .ok();

    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    providers.register(Arc::new(
        LlamaServerProvider::new(&server_url, model.clone().unwrap_or_default())
            .with_timeout(Duration::from_secs(120)),
    ));

    let vision: Option<Arc<dyn pai_inference::ImageUnderstandingProvider>> =
        (provider_name == "llama-server").then(|| {
            Arc::new(pai_vision::LlamaVisionProvider::new(
                &server_url,
                model.clone().unwrap_or_default(),
            )) as _
        });

    let agent = AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(load_policies(&store))),
        memory: memory.clone(),
        audit: audit.clone(),
        max_steps: 8,
        step_timeout: Duration::from_secs(120),
        device: device.id,
        persistence: Some(Persistence {
            conversations: conversations.clone(),
            runs: runs.clone(),
        }),
        documents: Some(documents.clone()),
        email: email.clone(),
        vision,
        notify: Some(Arc::new(pai_notify::StoreNotifySink {
            store: store.clone(),
            config: pai_notify::load_config(std::path::Path::new(&cfg.data_dir))
                .unwrap_or_default(),
            email: email.clone(),
        })),
        apps: Some(Arc::new(pai_agent::appops::StoreAppOperator::new(
            store.clone(),
            data_dir.clone(),
            device.id,
        ))),
        audio_gen: None,
        media_dir: None,
        allowed_roots: vec![inbox],
    };

    // Active conversation: explicit restore, else the most recent chat,
    // else a fresh one — avoids spawning an empty 'Untitled' per launch.
    let conversation = cfg
        .conversation
        .as_deref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(ConversationId)
        .or_else(|| conversations.list().ok()?.first().map(|c| c.id))
        .unwrap_or_else(|| {
            conversations
                .create(
                    session,
                    match cfg.memory_scope.as_deref() {
                        Some("isolated") => MemoryIsolation::Isolated,
                        _ => MemoryIsolation::Shared,
                    },
                )
                .map(|c| c.id)
                .unwrap_or_default()
        });

    spawn_sync_scheduler(
        store.clone(),
        cfg.data_dir.clone(),
        device.id,
        audit.clone(),
    );

    Ok(PaiRuntime {
        rt,
        agent,
        memory,
        audit,
        conversations,
        runs,
        def: AgentDefinition {
            name: "assistant".into(),
            description: "Default personal assistant".into(),
            purpose: "general assistance".into(),
            tools: vec![], // empty = all registered tools
            memory_scopes: vec![
                MemoryScope::Semantic,
                MemoryScope::Episodic,
                MemoryScope::Relationship,
            ],
            provider: provider_name,
            model,
        },
        session,
        conversation,
        send_lock: Mutex::new(()),
        pending: Arc::new(Mutex::new(HashMap::new())),
        event_cb: Arc::new(Mutex::new(None)),
        cancel: Arc::new(Mutex::new(CancelToken::default())),
        store,
        data_dir: cfg.data_dir.clone(),
        device: device.id,
        device_rec: device,
        documents,
        email,
        voice,
        server_url,
        llama_child: Mutex::new(None),
        serving_slug: Mutex::new(None),
    })
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

// ---------------------------------------------------------------------------
// Persisted policies (`policies` table overlays the shipped defaults)
// ---------------------------------------------------------------------------

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

fn persist_policy(store: &Store, p: Permission, policy: ExecutionPolicy) -> Result<()> {
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO policies(permission, policy, updated_at) VALUES(?1,?2,?3)
             ON CONFLICT(permission) DO UPDATE SET policy=excluded.policy,
             updated_at=excluded.updated_at",
            rusqlite::params![
                serde_json::to_value(p)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
                serde_json::to_value(policy)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
                pai_storage::ts(&now()),
            ],
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// FFI plumbing
// ---------------------------------------------------------------------------

fn read_str<'a>(p: *const c_char) -> Result<&'a str> {
    if p.is_null() {
        return Err(Error::InvalidInput("null string".into()));
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|e| Error::InvalidInput(e.to_string()))
}

fn to_c(v: impl Serialize) -> *mut c_char {
    let s = serde_json::to_string(&v).unwrap_or_else(|_| "{}".into());
    CString::new(s).unwrap_or_default().into_raw()
}

fn parse_uuid(s: &str, what: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|_| Error::InvalidInput(format!("invalid {what} id '{s}'")))
}

/// config_json: {"data_dir": "...", "provider": "echo"|"llama-server"|"auto"}
/// Returns an opaque handle, or null on failure.
/// # Safety
/// `config_json` must be a valid NUL-terminated UTF-8 string or null.
#[no_mangle]
pub unsafe extern "C" fn pai_init(config_json: *const c_char) -> *mut PaiRuntime {
    let cfg: InitConfig = match read_str(config_json)
        .and_then(|s| serde_json::from_str(s).map_err(|e| Error::InvalidInput(e.to_string())))
    {
        Ok(c) => c,
        Err(_) => return std::ptr::null_mut(),
    };
    match init_runtime(cfg) {
        Ok(rt) => Box::into_raw(Box::new(rt)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Register the live-event callback. `cb` is invoked on the `pai_send`
/// thread for each `AgentEvent` as a JSON string; `user_data` is passed
/// through untouched. Call with NULL to unregister.
/// # Safety
/// `handle` must come from `pai_init`. `cb` must remain callable for the
/// lifetime of the registration (Dart: a `NativeCallable.listener`).
#[no_mangle]
pub unsafe extern "C" fn pai_set_event_callback(
    handle: *mut PaiRuntime,
    cb: Option<PaiEventCallback>,
    user_data: *mut std::ffi::c_void,
) {
    if handle.is_null() {
        return;
    }
    let rt = &*handle;
    *rt.event_cb.lock().unwrap() = cb.map(|f| (f, user_data as usize));
}

#[derive(Serialize)]
struct SendResult {
    run_id: String,
    state: String,
    answer: Option<String>,
    events: Vec<serde_json::Value>,
    error: Option<String>,
}

/// Shared emit sink: collects events for the final result *and* forwards
/// them to the registered UI callback as they happen.
fn emit_fn(
    rt: &PaiRuntime,
    sink: Arc<Mutex<Vec<serde_json::Value>>>,
) -> impl Fn(pai_agent::AgentEvent) + Send + Sync + '_ {
    let cb = rt.event_cb.clone();
    move |e: pai_agent::AgentEvent| {
        let v = serde_json::to_value(&e).unwrap_or_default();
        sink.lock().unwrap().push(v.clone());
        if let Some((f, ud)) = *cb.lock().unwrap() {
            if let Ok(s) = serde_json::to_string(&v) {
                if let Ok(cs) = CString::new(s) {
                    f(cs.as_ptr(), ud as *mut std::ffi::c_void);
                }
            }
        }
    }
}

fn send_result(
    outcome: Result<pai_agent::RunOutcome>,
    events: Vec<serde_json::Value>,
) -> SendResult {
    match outcome {
        Ok(o) => SendResult {
            run_id: o.run.id.to_string(),
            state: format!("{:?}", o.run.state).to_lowercase(),
            answer: o.answer,
            events,
            error: None,
        },
        Err(e) => SendResult {
            run_id: String::new(),
            state: "failed".into(),
            answer: None,
            events,
            error: Some(e.to_string()),
        },
    }
}

/// Send one user message; returns JSON with the run's events + final answer.
/// Live events are also pushed to the registered callback while this blocks.
/// # Safety
/// `handle` must come from `pai_init` and `message` must be a valid
/// NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn pai_send(handle: *mut PaiRuntime, message: *const c_char) -> *mut c_char {
    let rt = &mut *handle;
    let text = match read_str(message) {
        Ok(t) => t.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let _guard = rt.send_lock.lock().unwrap();

    // The active conversation may have been deleted — lazily start a
    // fresh shared one rather than writing into a dangling id.
    if rt.conversations.get(rt.conversation).is_err() {
        if let Ok(c) = rt.conversations.create(rt.session, MemoryIsolation::Shared) {
            rt.conversation = c.id;
        }
    }

    let events = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let emit = emit_fn(rt, events.clone());
    let approval = UiApproval {
        pending: rt.pending.clone(),
    };
    *rt.cancel.lock().unwrap() = CancelToken::default();
    let cancel = rt.cancel.lock().unwrap().clone();

    // Transcript comes from the store — the run itself appends to it.
    let history = rt
        .conversations
        .messages(rt.conversation)
        .unwrap_or_default();

    let outcome = rt.rt.block_on(rt.agent.run(RunRequest {
        definition: &rt.def,
        history,
        input: text,
        conversation: Some(rt.conversation),
        approval: &approval,
        cancel,
        emit: &emit,
        stream: true,
        resume_from: None,
    }));

    let events = events.lock().unwrap().clone();
    to_c(send_result(outcome, events))
}

/// Resume an interrupted run (see `pai_runs`). Same result shape as `pai_send`.
/// # Safety
/// `handle` must come from `pai_init`; `run_id` is a NUL-terminated uuid.
#[no_mangle]
pub unsafe extern "C" fn pai_resume(handle: *mut PaiRuntime, run_id: *const c_char) -> *mut c_char {
    let rt = &mut *handle;
    let rid = match read_str(run_id).and_then(|s| parse_uuid(s, "run").map(AgentRunId)) {
        Ok(r) => r,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let _guard = rt.send_lock.lock().unwrap();

    let events = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let emit = emit_fn(rt, events.clone());
    let approval = UiApproval {
        pending: rt.pending.clone(),
    };
    *rt.cancel.lock().unwrap() = CancelToken::default();
    let cancel = rt.cancel.lock().unwrap().clone();

    let outcome = rt.rt.block_on(rt.agent.run(RunRequest {
        definition: &rt.def,
        history: vec![],
        input: String::new(),
        conversation: None,
        approval: &approval,
        cancel,
        emit: &emit,
        stream: true,
        resume_from: Some(rid),
    }));

    let events = events.lock().unwrap().clone();
    to_c(send_result(outcome, events))
}

/// Resolve a pending tool approval. Returns 1 when a matching request was
/// waiting, 0 otherwise.
/// # Safety
/// `handle` must come from `pai_init`; `call_id` is a NUL-terminated uuid.
/// Callable from any thread.
#[no_mangle]
pub unsafe extern "C" fn pai_approve(
    handle: *mut PaiRuntime,
    call_id: *const c_char,
    granted: i32,
) -> i32 {
    if handle.is_null() {
        return 0;
    }
    let rt = &*handle;
    let id = match read_str(call_id).and_then(|s| parse_uuid(s, "call").map(ToolCallId)) {
        Ok(i) => i,
        Err(_) => return 0,
    };
    match rt.pending.lock().unwrap().remove(&id) {
        Some(tx) => {
            let _ = tx.send(granted != 0);
            1
        }
        None => 0,
    }
}

/// Cancel the in-flight run, if any.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_cancel(handle: *mut PaiRuntime) {
    if handle.is_null() {
        return;
    }
    (*handle).cancel.lock().unwrap().cancel();
}

/// Runs that never reached a terminal state (crashed/interrupted).
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_runs(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    match rt.runs.interrupted() {
        Ok(runs) => to_c(runs),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------

/// JSON list of conversations, newest first; `active` marks the one
/// `pai_send` writes into.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_conversations(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    match rt.conversations.list() {
        Ok(list) => to_c(
            list.iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id.to_string(),
                        "title": c.title,
                        "created_at": c.created_at,
                        "memory": match c.memory {
                            MemoryIsolation::Shared => "shared",
                            MemoryIsolation::Isolated => "isolated",
                        },
                        "active": c.id == rt.conversation,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Start a new conversation and make it active.
/// `config_json`: `{"memory": "shared"|"isolated"}` or null.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_conversation_new(
    handle: *mut PaiRuntime,
    config_json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let isolated = read_str(config_json)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v["memory"].as_str().map(String::from))
        .as_deref()
        == Some("isolated");
    match rt.conversations.create(
        rt.session,
        if isolated {
            MemoryIsolation::Isolated
        } else {
            MemoryIsolation::Shared
        },
    ) {
        Ok(c) => {
            rt.conversation = c.id;
            to_c(serde_json::json!({"id": c.id.to_string()}))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Make an existing conversation active; returns its transcript.
/// # Safety
/// `handle` must come from `pai_init`; `id` is a NUL-terminated uuid.
#[no_mangle]
pub unsafe extern "C" fn pai_conversation_select(
    handle: *mut PaiRuntime,
    id: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let cid = match read_str(id).and_then(|s| parse_uuid(s, "conversation").map(ConversationId)) {
        Ok(c) => c,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    match rt
        .conversations
        .get(cid)
        .and_then(|_| rt.conversations.messages(cid))
    {
        Ok(msgs) => {
            rt.conversation = cid;
            to_c(serde_json::json!({"id": cid.to_string(), "messages": msgs}))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Transcript of the active conversation.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_history(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    match rt.conversations.messages(rt.conversation) {
        Ok(msgs) => to_c(msgs),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// # Safety
/// `handle` must come from `pai_init`; both args are NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn pai_conversation_rename(
    handle: *mut PaiRuntime,
    id: *const c_char,
    title: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let cid = match read_str(id).and_then(|s| parse_uuid(s, "conversation").map(ConversationId)) {
        Ok(c) => c,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    match read_str(title).and_then(|t| rt.conversations.rename(cid, t)) {
        Ok(()) => to_c(serde_json::json!({"ok": true})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// # Safety
/// `handle` must come from `pai_init`; `id` is a NUL-terminated uuid.
#[no_mangle]
pub unsafe extern "C" fn pai_conversation_delete(
    handle: *mut PaiRuntime,
    id: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let cid = match read_str(id).and_then(|s| parse_uuid(s, "conversation").map(ConversationId)) {
        Ok(c) => c,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    match rt.conversations.delete(cid) {
        Ok(()) => {
            // Leave rt.conversation stale — pai_send lazily creates a
            // fresh conversation the next time it points at a deleted
            // one, so no phantom empty chat appears in the drawer.
            to_c(serde_json::json!({"ok": true, "was_active": rt.conversation == cid}))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Set a conversation's memory scope ("shared" | "isolated").
/// # Safety
/// `handle` must come from `pai_init`; args are NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn pai_conversation_set_memory(
    handle: *mut PaiRuntime,
    id: *const c_char,
    mode: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let cid = match read_str(id).and_then(|s| parse_uuid(s, "conversation").map(ConversationId)) {
        Ok(c) => c,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let mode = match read_str(mode) {
        Ok(m) => m,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let iso = match mode {
        "isolated" => MemoryIsolation::Isolated,
        "shared" => MemoryIsolation::Shared,
        _ => {
            return to_c(serde_json::json!({"error": "mode must be shared|isolated"}));
        }
    };
    match rt.conversations.set_memory_scope(cid, iso) {
        Ok(()) => to_c(serde_json::json!({"ok": true})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------

/// All permissions with their effective policy, for the policy editor.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_policies(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    to_c(
        all_permissions()
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "permission": serde_json::to_value(p).unwrap(),
                    "policy": serde_json::to_value(rt.agent.permissions.effective(p)).unwrap(),
                })
            })
            .collect::<Vec<_>>(),
    )
}

/// Apply + persist a policy edit. `policy` ∈ ALWAYS_ALLOW | ASK_USER |
/// ALLOW_WITH_RULE | NEVER_ALLOW.
/// # Safety
/// `handle` must come from `pai_init`; args are NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn pai_set_policy(
    handle: *mut PaiRuntime,
    permission: *const c_char,
    policy: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let parsed = (|| {
        let p = serde_json::from_value::<Permission>(serde_json::json!(read_str(permission)?))
            .map_err(|_| Error::InvalidInput("unknown permission".into()))?;
        let pol = serde_json::from_value::<ExecutionPolicy>(serde_json::json!(read_str(policy)?))
            .map_err(|_| Error::InvalidInput("unknown policy".into()))?;
        Ok::<_, Error>((p, pol))
    })();
    let (p, pol) = match parsed {
        Ok(x) => x,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    rt.agent.permissions.set_policy(p, pol);
    match persist_policy(&rt.store, p, pol) {
        Ok(()) => {
            let mut e = pai_audit::event(AuditKind::PermissionChanged, AuditOutcome::Ok);
            e.device = Some(rt.device);
            e.detail =
                serde_json::json!({"permission": format!("{p:?}"), "policy": format!("{pol:?}")});
            let _ = rt.audit.record(&e);
            to_c(serde_json::json!({"ok": true}))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Memories
// ---------------------------------------------------------------------------

/// Recent audit events as JSON.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_audit(handle: *mut PaiRuntime, limit: u32) -> *mut c_char {
    let rt = &mut *handle;
    match rt.audit.recent(limit as usize) {
        Ok(ev) => to_c(ev),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// All non-deleted memories as JSON (the memory browser's "all" scope).
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_memories(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    let out = rt.rt.block_on(async {
        rt.memory
            .recall(&RecallQuery {
                text: None,
                limit: 500,
                memory_scope: MemoryScopeQuery::All,
                ..Default::default()
            })
            .await
    });
    match out {
        Ok(items) => to_c(items.iter().map(|s| &s.item).collect::<Vec<_>>()),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Soft-delete a memory by uuid — the memory browser's forget action.
/// Records `MemoryDeleted` in the audit log.
/// # Safety
/// `handle` must come from `pai_init`; `memory_id` is a NUL-terminated uuid.
#[no_mangle]
pub unsafe extern "C" fn pai_forget(
    handle: *mut PaiRuntime,
    memory_id: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let id = match read_str(memory_id).and_then(|s| parse_uuid(s, "memory").map(MemoryId)) {
        Ok(i) => i,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let out = rt.rt.block_on(async {
        let item = rt.memory.get(id).await?;
        rt.memory.delete(id).await?;
        Ok::<_, Error>(item)
    });
    match out {
        Ok(item) => {
            let mut e = pai_audit::event(AuditKind::MemoryDeleted, AuditOutcome::Ok);
            e.device = Some(rt.device);
            e.detail = serde_json::json!({"memory_id": id.to_string(), "content": item.content});
            let _ = rt.audit.record(&e);
            to_c(serde_json::json!({"deleted_id": id.to_string(), "content": item.content}))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Documents — ingest/list/search/delete for the Flutter Documents screen
// ---------------------------------------------------------------------------

/// List ingested documents.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_docs(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    match rt.documents.list() {
        Ok(rows) => to_c(
            rows.iter()
                .map(|(id, title, mime, at, sections)| {
                    serde_json::json!({
                        "id": id.to_string(),
                        "title": title,
                        "mime": mime,
                        "created_at": at.to_rfc3339(),
                        "sections": sections,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Ingest a file path — user-initiated, so no tool jail applies.
/// # Safety
/// `handle` must come from `pai_init`; `path` is NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_docs_ingest(
    handle: *mut PaiRuntime,
    path: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let path = match read_str(path) {
        Ok(p) => p,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let out = rt.rt.block_on(async {
        let canon =
            std::fs::canonicalize(path).map_err(|e| Error::InvalidInput(format!("{path}: {e}")))?;
        let bytes =
            std::fs::read(&canon).map_err(|e| Error::InvalidInput(format!("{canon:?}: {e}")))?;
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
        rt.documents
            .ingest(&bytes, mime, canon.file_name().and_then(|n| n.to_str()))
            .await
    });
    match out {
        Ok(id) => to_c(serde_json::json!({"document_id": id.to_string()})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Search document sections (hybrid FTS + vector when an embedder is live).
/// # Safety
/// `handle` must come from `pai_init`; `query` is NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_docs_search(
    handle: *mut PaiRuntime,
    query: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let q = match read_str(query) {
        Ok(q) => q,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let out = rt.rt.block_on(async { rt.documents.search(q, 10).await });
    match out {
        Ok(hits) => to_c(
            hits.iter()
                .map(|h| {
                    serde_json::json!({
                        "document": h.document_id.to_string(),
                        "title": h.title,
                        "section": h.section,
                        "snippet": h.snippet,
                        "score": h.score,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Delete a document and its sections.
/// # Safety
/// `handle` must come from `pai_init`; `id` is a NUL-terminated uuid.
#[no_mangle]
pub unsafe extern "C" fn pai_docs_delete(
    handle: *mut PaiRuntime,
    id: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let id = match read_str(id).and_then(|s| parse_uuid(s, "document").map(DocumentId)) {
        Ok(i) => i,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    match rt.documents.delete(id) {
        Ok(()) => to_c(serde_json::json!({"deleted_id": id.to_string()})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Email connector
// ---------------------------------------------------------------------------

fn email_err() -> *mut c_char {
    to_c(serde_json::json!({"error": "email not configured (email.json)"}))
}

/// Search the mailbox. `query_json`: {query?, from?, label?, unread_only?, limit?}
/// # Safety
/// `handle` must come from `pai_init`; `query_json` NUL-terminated JSON or NULL.
#[no_mangle]
pub unsafe extern "C" fn pai_email_search(
    handle: *mut PaiRuntime,
    query_json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let Some(email) = &rt.email else {
        return email_err();
    };
    let q = match query_json {
        q if q.is_null() => pai_connector_email::EmailSearch {
            limit: 20,
            ..Default::default()
        },
        q => match read_str(q) {
            Ok(s) => serde_json::from_str(s).unwrap_or_default(),
            Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
        },
    };
    let out = rt.rt.block_on(async { email.search(&q).await });
    match out {
        Ok(hits) => to_c(serde_json::json!({"results": hits})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Read a message by id. Returns the full EmailMessage (untrusted content).
/// # Safety
/// `handle` must come from `pai_init`; `id` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn pai_email_read(handle: *mut PaiRuntime, id: *const c_char) -> *mut c_char {
    let rt = &mut *handle;
    let Some(email) = &rt.email else {
        return email_err();
    };
    let id = match read_str(id) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    match rt.rt.block_on(async { email.read(&id).await }) {
        Ok(m) => to_c(serde_json::json!({"message": m})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Create a draft. `draft_json`: {to:[],cc:[],subject,body,in_reply_to?}
/// # Safety
/// `handle` must come from `pai_init`; `draft_json` NUL-terminated JSON.
#[no_mangle]
pub unsafe extern "C" fn pai_email_draft(
    handle: *mut PaiRuntime,
    draft_json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let Some(email) = &rt.email else {
        return email_err();
    };
    let draft: pai_connector_email::Draft = match read_str(draft_json)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(s).map_err(|e| e.to_string()))
    {
        Ok(d) => d,
        Err(e) => return to_c(serde_json::json!({"error": e})),
    };
    match rt.rt.block_on(async { email.create_draft(&draft).await }) {
        Ok(id) => to_c(serde_json::json!({"draft_id": id})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Send a draft immediately via SMTP (the `smtp` block in email.json).
/// `draft_json`: same shape as `pai_email_draft`. Returns `{sent: true}`.
/// # Safety
/// `handle` must come from `pai_init`; `draft_json` NUL-terminated JSON.
#[no_mangle]
pub unsafe extern "C" fn pai_email_send(
    handle: *mut PaiRuntime,
    draft_json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let Some(email) = &rt.email else {
        return email_err();
    };
    let draft: pai_connector_email::Draft = match read_str(draft_json)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(s).map_err(|e| e.to_string()))
    {
        Ok(d) => d,
        Err(e) => return to_c(serde_json::json!({"error": e})),
    };
    match rt.rt.block_on(async { email.send(&draft).await }) {
        Ok(()) => to_c(serde_json::json!({"sent": true})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Voice
// ---------------------------------------------------------------------------

/// Notification inbox: `{notifications: [...], unread: n}`.
/// `unread_only` filters to rows with no read_at. Rows sync across
/// paired devices, so the inbox follows the user.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_notify_list(
    handle: *mut PaiRuntime,
    unread_only: bool,
) -> *mut c_char {
    let rt = &mut *handle;
    match pai_notify::store::list(&rt.store, unread_only, 200) {
        Ok(items) => {
            let unread = pai_notify::store::unread_count(&rt.store).unwrap_or(0);
            to_c(serde_json::json!({
                "notifications": items.iter().map(|n| serde_json::json!({
                    "id": n.id,
                    "title": n.title,
                    "body": n.body,
                    "source": n.source,
                    "channel": n.channel,
                    "created_at": n.created_at.to_rfc3339(),
                    "read_at": n.read_at.map(|t| t.to_rfc3339()),
                })).collect::<Vec<_>>(),
                "unread": unread,
            }))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Mark a notification read — the `read_at` propagates to peers on sync.
/// Returns `{ok: bool}`.
/// # Safety
/// `handle` must come from `pai_init`; `id` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn pai_notify_mark_read(
    handle: *mut PaiRuntime,
    id: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let id = match read_str(id) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    match pai_notify::store::mark_read(&rt.store, &id) {
        Ok(ok) => to_c(serde_json::json!({"ok": ok})),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Installed app packages: `{apps: [{id, name, version, runtime}]}`.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_apps_list(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    match pai_apps::AppRegistry::new(std::path::Path::new(&rt.data_dir)).list() {
        Ok(apps) => {
            let placement: std::collections::HashMap<String, Option<String>> = rt
                .store
                .with_conn(|c| {
                    let mut s = c.prepare("SELECT id, active_device FROM apps")?;
                    let rows = s.query_map([], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                    })?;
                    rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()
                })
                .unwrap_or_default();
            to_c(serde_json::json!({
                "device": rt.device.to_string(),
                "apps": apps.iter().map(|(id, m)| serde_json::json!({
                    "id": id,
                    "name": m.app.name,
                    "version": m.app.version,
                    "runtime": m.app.runtime,
                    "active_device": placement.get(id).and_then(|p| p.clone()),
                })).collect::<Vec<_>>(),
            }))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Run an installed app in the wasmi sandbox — the mobile/desktop
/// runtime path. `args_json` is a JSON array of strings (NULL → no
/// args). Returns `{stdout, stderr, exit_code, fuel}` or `{error}`.
/// Placement enforcement (the app's `active_device`) is the caller's
/// concern — see `pai apps run`.
/// # Safety
/// `handle` must come from `pai_init`; `id`/`args_json` are
/// NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn pai_apps_run(
    handle: *mut PaiRuntime,
    id: *const c_char,
    args_json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let id = match read_str(id) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let args: Vec<String> = if args_json.is_null() {
        Vec::new()
    } else {
        match read_str(args_json)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(s).map_err(|e| e.to_string()))
        {
            Ok(v) => v,
            Err(e) => return to_c(serde_json::json!({"error": e})),
        }
    };
    match pai_sync::backup::active_elsewhere(&rt.store, rt.device, &id) {
        Ok(Some(other)) => {
            return to_c(serde_json::json!({
                "error": format!("app {id} is active on {other} — migrate it here first"),
            }))
        }
        Ok(None) => {}
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    }
    let reg = pai_apps::AppRegistry::new(std::path::Path::new(&rt.data_dir));
    let pkg = match reg.get(&id) {
        Ok(Some(p)) => p,
        Ok(None) => return to_c(serde_json::json!({"error": format!("app {id} not installed")})),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let dir = pai_apps::installed_dir(std::path::Path::new(&rt.data_dir), &id);
    // FFI runs get no injected envs — there's no async context here to
    // resolve oauth tokens (device-flow is a CLI/agent path anyway).
    match pkg.run(&dir, &args, pai_apps::RunLimits::default(), &[], &[]) {
        Ok(out) => to_c(serde_json::json!({
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
            "exit_code": out.exit_code,
            "fuel": out.fuel_consumed,
        })),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

fn voice_err(msg: &str) -> *mut c_char {
    to_c(serde_json::json!({"error": msg}))
}

/// What the runtime resolved: `{provider, model, device, data_dir}` —
/// the chat header's "who am I talking to" line.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_status(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    to_c(serde_json::json!({
        "provider": rt.def.provider,
        "model": rt.def.model,
        "device": rt.device.to_string(),
        "data_dir": rt.data_dir,
    }))
}

/// Point chat at an OpenAI-compatible endpoint — shared by
/// `pai_set_provider` and `pai_models_serve`.
fn apply_provider(rt: &mut PaiRuntime, base_url: &str, model: &str) {
    rt.server_url = base_url.to_string();
    rt.def.model = Some(model.to_string());
    rt.agent.providers.register(Arc::new(
        LlamaServerProvider::new(base_url, model.to_string())
            .with_timeout(Duration::from_secs(120)),
    ));
    rt.def.provider = "llama-server".into();
    rt.agent.vision = Some(Arc::new(pai_vision::LlamaVisionProvider::new(
        base_url,
        model.to_string(),
    )));
}

/// Re-point chat at a different endpoint/model without re-init:
/// `{"server_url"?, "model"?}`. Rebuilds the llama-server provider
/// and vision adapter so the next send uses them. Returns the resolved
/// `{provider, model}`.
/// # Safety
/// `handle` must come from `pai_init`; `json` must be a valid
/// NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn pai_set_provider(
    handle: *mut PaiRuntime,
    json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    #[derive(Deserialize)]
    struct Cfg {
        server_url: Option<String>,
        model: Option<String>,
    }
    let cfg: Cfg = match read_str(json)
        .and_then(|s| serde_json::from_str(s).map_err(|e| Error::InvalidInput(e.to_string())))
    {
        Ok(c) => c,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    if let Some(u) = cfg.server_url {
        rt.server_url = u;
    }
    if let Some(m) = cfg.model {
        rt.def.model = Some(m);
    }
    let url = rt.server_url.clone();
    let model = rt.def.model.clone().unwrap_or_default();
    apply_provider(rt, &url, &model);
    let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
    e.device = Some(rt.device);
    e.detail = serde_json::json!({
        "server_url": rt.server_url, "model": rt.def.model});
    let _ = rt.audit.record(&e);
    to_c(serde_json::json!({
        "provider": rt.def.provider, "model": rt.def.model}))
}

/// Serialize the models table for the Devices screen — marks each row
/// online/offline by whether its file exists right now (a pack on an
/// unplugged drive lists offline) and `serving` for the managed
/// llama-server child.
fn models_json(
    rt: &PaiRuntime,
    rows: Vec<(Model, bool, Option<std::path::PathBuf>)>,
) -> serde_json::Value {
    let serving = rt.serving_slug.lock().unwrap().clone();
    serde_json::json!(rows
        .iter()
        .map(|(m, installed, path)| {
            serde_json::json!({
                "slug": m.slug,
                "family": m.family,
                "provider": m.provider,
                "quant": m.quantization,
                "size_mb": m.size_bytes / 1_000_000,
                "context": m.context_length,
                "capabilities": m.capabilities,
                "installed": installed,
                "online": path.as_ref().map(|p| p.exists()).unwrap_or(false),
                "path": path.as_ref().map(|p| p.display().to_string()),
                "serving": serving.as_deref() == Some(m.slug.as_str()),
            })
        })
        .collect::<Vec<_>>())
}

/// Known models: everything the registry has seen — installed locally,
/// adopted from a pack, or catalog-only. `[{slug, ..., online, serving}]`.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_models_list(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    let mgr = ModelManager::new(rt.store.clone(), std::path::Path::new(&rt.data_dir));
    match mgr.list() {
        Ok(rows) => to_c(models_json(rt, rows)),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Rescan mounted `pai-models/` pack roots — a freshly plugged drive
/// counts — adopt anything found, then return the list. This is the
/// plug-and-play path: copy a pack onto a flash drive on one device,
/// plug it into another, scan, serve.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_models_scan(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    let mgr = ModelManager::new(rt.store.clone(), std::path::Path::new(&rt.data_dir));
    match mgr.scan().and_then(|_| mgr.list()) {
        Ok(rows) => to_c(models_json(rt, rows)),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Serve an installed or pack-resident model via `llama-server` and
/// point chat at it. `slug` resolves through `locate()` — recorded path
/// first, pack roots on remapped drive letters after. `port` <= 0
/// defaults to 8090. Returns `{serving, url, provider, model}`.
/// # Safety
/// `handle` must come from `pai_init`; `slug` is NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_models_serve(
    handle: *mut PaiRuntime,
    slug: *const c_char,
    port: i32,
) -> *mut c_char {
    let rt = &mut *handle;
    let slug = match read_str(slug) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let port = if port > 0 { port as u16 } else { 8090 };
    let mgr = ModelManager::new(rt.store.clone(), std::path::Path::new(&rt.data_dir));
    let path = match mgr.locate(&slug) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return to_c(serde_json::json!({
                "error": format!("{slug} not found — is its drive plugged in?")
            }))
        }
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let bin = match pai_inference::find_in_path("llama-server") {
        Some(b) => b,
        None => {
            return to_c(serde_json::json!({
                "error": "llama-server not on PATH — install llama.cpp"
            }))
        }
    };
    if let Some(mut c) = rt.llama_child.lock().unwrap().take() {
        let _ = c.kill();
    }
    let mut child = match std::process::Command::new(&bin)
        .args([
            "-m",
            &path.to_string_lossy(),
            "--port",
            &port.to_string(),
            "--host",
            "127.0.0.1",
        ])
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return to_c(serde_json::json!({
                "error": format!("spawn llama-server: {e}")
            }))
        }
    };
    let url = format!("http://127.0.0.1:{port}");
    let outcome: std::result::Result<(), String> = rt.rt.block_on(async {
        for _ in 0..45 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("llama-server exited early: {status}"));
            }
            if pai_inference::detect_endpoints(Duration::from_millis(400))
                .await
                .iter()
                .any(|e| e.base_url == url)
            {
                return Ok(());
            }
        }
        Err("llama-server did not come up in 45s".into())
    });
    if let Err(e) = outcome {
        let _ = child.kill();
        return to_c(serde_json::json!({"error": e}));
    }
    *rt.llama_child.lock().unwrap() = Some(child);
    *rt.serving_slug.lock().unwrap() = Some(slug.to_string());
    apply_provider(rt, &url, &slug);
    let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
    e.device = Some(rt.device);
    e.detail = serde_json::json!({"served_model": slug, "url": url});
    let _ = rt.audit.record(&e);
    to_c(serde_json::json!({
        "serving": slug, "url": url,
        "provider": "llama-server", "model": slug}))
}

/// The built-in model catalog — what `pai models install <slug>` can
/// fetch. Rows: `{slug, family, quant, size_mb, capabilities, context}`.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_models_catalog(handle: *mut PaiRuntime) -> *mut c_char {
    let _ = handle;
    let rows: Vec<serde_json::Value> = pai_models::builtin_catalog()
        .iter()
        .map(|m| {
            serde_json::json!({
                "slug": m.model.slug,
                "family": m.model.family,
                "provider": m.model.provider,
                "quant": m.model.quantization,
                "size_mb": m.model.size_bytes / 1_000_000,
                "context": m.model.context_length,
                "capabilities": m.model.capabilities,
                "license": m.model.license,
            })
        })
        .collect();
    to_c(serde_json::Value::Array(rows))
}

/// Install a model: `json` is `{"slug"|"ref"(hf://…), "dest_dir"?}`.
/// `dest_dir` empty/absent installs to the internal model dir; a path
/// like `D:\pai-models` writes a portable pack (copies the file instead
/// of re-downloading when the model is already on disk). Blocking —
/// downloads are large; call from a worker thread.
/// # Safety
/// `handle` must come from `pai_init`; `json` is NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_models_install(
    handle: *mut PaiRuntime,
    json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let raw = match read_str(json) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    #[derive(serde::Deserialize)]
    struct Req {
        slug: String,
        #[serde(default)]
        dest_dir: Option<String>,
    }
    let req: Req = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => return to_c(serde_json::json!({"error": format!("bad JSON: {e}")})),
    };
    let mgr = ModelManager::new(rt.store.clone(), std::path::Path::new(&rt.data_dir));
    let out: std::result::Result<std::path::PathBuf, String> = rt.rt.block_on(async {
        let manifest = pai_models::resolve_model_arg(&req.slug)
            .await
            .map_err(|e| e.to_string())?;
        match req.dest_dir.as_deref().filter(|d| !d.trim().is_empty()) {
            Some(d) => mgr
                .install_to(&manifest.model.slug, &manifest, std::path::Path::new(d))
                .await
                .map_err(|e| e.to_string()),
            None => mgr
                .install(&manifest.model.slug, &manifest)
                .await
                .map_err(|e| e.to_string()),
        }
    });
    match out {
        Ok(path) => {
            let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
            e.device = Some(rt.device);
            e.detail = serde_json::json!({
                "installed_model": req.slug, "path": path.to_string_lossy()});
            let _ = rt.audit.record(&e);
            to_c(serde_json::json!({
                "installed": req.slug, "path": path.to_string_lossy()}))
        }
        Err(e) => to_c(serde_json::json!({"error": e})),
    }
}

/// Resolved sync request — explicit args override the saved `sync.*`
/// target inside `run_sync`.
struct SyncArgs {
    mode: String,
    dir: Option<String>,
    relay: Option<String>,
    token: Option<String>,
    lan: bool,
}

/// Shared sync path for `pai_sync_now` and the auto-sync scheduler:
/// resolve the transport (explicit args → saved `sync.*` meta), run the
/// engine under `executor`, audit the outcome. Returns a JSON value.
fn run_sync(
    executor: &tokio::runtime::Runtime,
    store: &Arc<Store>,
    data_dir: &str,
    device: DeviceId,
    audit: &Arc<pai_audit::AuditLog>,
    args: &SyncArgs,
) -> serde_json::Value {
    let meta = |k: &str| store.meta_get(&format!("sync.{k}")).ok().flatten();
    let lan = args.lan || meta("lan").as_deref() == Some("1");
    let dir = args.dir.clone().or_else(|| meta("dir"));
    let relay = args.relay.clone().or_else(|| meta("relay"));
    let token = args.token.clone().or_else(|| meta("token"));
    let transport: Box<dyn pai_sync::SyncTransport> = if lan {
        // Mesh discovery — paired peer announcing its relay on the LAN.
        let ids = pai_identity::IdentityStore::new(store.clone());
        let bind = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            pai_mesh::MULTICAST_PORT,
        );
        let sock = match pai_mesh::bind_listener(bind, Some(pai_mesh::MULTICAST_GROUP)) {
            Ok(s) => s,
            Err(e) => return serde_json::json!({"error": e.to_string()}),
        };
        let found = pai_mesh::discover(&sock, Duration::from_secs(3));
        let paired = match pai_mesh::paired_announcements(store, &ids, found) {
            Ok(p) => p,
            Err(e) => return serde_json::json!({"error": e.to_string()}),
        };
        let Some(target) = paired.first() else {
            return serde_json::json!({
                "error": "no paired mesh peer announcing — \
                          run `pai sync serve --announce` on it"
            });
        };
        let agree = match pai_sync::crypto::agreement_key(device, std::path::Path::new(data_dir)) {
            Ok(a) => a,
            Err(e) => return serde_json::json!({"error": e.to_string()}),
        };
        let token = pai_mesh::token_for(&agree.secret, &target.peer);
        Box::new(pai_sync::relay::RelayTransport::new(
            format!("http://{}", target.relay_addr),
            Some(token),
        ))
    } else if let Some(d) = dir.clone() {
        match pai_sync::FolderTransport::new(d.into()) {
            Ok(t) => Box::new(t),
            Err(e) => return serde_json::json!({"error": e.to_string()}),
        }
    } else if let Some(r) = relay.clone() {
        Box::new(pai_sync::relay::RelayTransport::new(r, token))
    } else {
        return serde_json::json!({
            "error": "no sync target — pass lan:true, a dir, or a relay"
        });
    };
    // Persist the choice so the next call needs no args.
    let _ = store.meta_set("sync.lan", if lan { "1" } else { "0" });
    if let Some(d) = &dir {
        let _ = store.meta_set("sync.dir", d);
    }
    if let Some(r) = &relay {
        let _ = store.meta_set("sync.relay", r);
    }
    let vault = match pai_sync::crypto::vault_key(std::path::Path::new(data_dir)) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return serde_json::json!({
                "error": "no vault key — pair a device first (pai pair)"
            })
        }
        Err(e) => return serde_json::json!({"error": e.to_string()}),
    };
    let eng = pai_sync::engine::SyncEngine::new(
        transport,
        store.clone(),
        vault,
        device,
        std::path::Path::new(data_dir),
    );
    let out = executor.block_on(async {
        match args.mode.as_str() {
            "push" => eng.push().await,
            "pull" => eng.pull().await,
            _ => eng.run().await,
        }
    });
    match out {
        Ok(o) => {
            let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
            e.device = Some(device);
            e.detail = serde_json::json!({
                "sync": args.mode, "pushed": o.pushed, "pulled": o.pulled});
            let _ = audit.record(&e);
            serde_json::json!({
                "pushed": o.pushed, "pulled": o.pulled, "skipped": o.skipped})
        }
        Err(e) => serde_json::json!({"error": e.to_string()}),
    }
}

/// Auto-sync loop: every minute, if `sync.auto_minutes` meta is set and
/// `sync.last_auto` is older than that interval, run a `run` sync on the
/// saved target. `last_auto` is stamped before the attempt so a failing
/// target backs off for the full interval instead of retrying at 60s.
fn spawn_sync_scheduler(
    store: Arc<Store>,
    data_dir: String,
    device: DeviceId,
    audit: Arc<pai_audit::AuditLog>,
) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        loop {
            std::thread::sleep(Duration::from_secs(60));
            let auto: u64 = store
                .meta_get("sync.auto_minutes")
                .ok()
                .flatten()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if auto == 0 {
                continue;
            }
            let now_s = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let last: u64 = store
                .meta_get("sync.last_auto")
                .ok()
                .flatten()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if now_s.saturating_sub(last) < auto.saturating_mul(60) {
                continue;
            }
            let _ = store.meta_set("sync.last_auto", &now_s.to_string());
            let _ = run_sync(
                &rt,
                &store,
                &data_dir,
                device,
                &audit,
                &SyncArgs {
                    mode: "run".into(),
                    dir: None,
                    relay: None,
                    token: None,
                    lan: false,
                },
            );
        }
    });
}

/// Persisted sync configuration + readiness: `{lan, dir, relay,
/// token_set, auto_minutes, last_auto, peers, has_vault}` — the Devices
/// screen prefills its dialog and badges the schedule from this.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_sync_status(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    let meta = |k: &str| rt.store.meta_get(&format!("sync.{k}")).ok().flatten();
    let peers = pai_sync::pair::list_peers(&rt.store)
        .map(|p| p.len())
        .unwrap_or(0);
    let has_vault = matches!(
        pai_sync::crypto::vault_key(std::path::Path::new(&rt.data_dir)),
        Ok(Some(_))
    );
    to_c(serde_json::json!({
        "lan": meta("lan").as_deref() == Some("1"),
        "dir": meta("dir"),
        "relay": meta("relay"),
        "token_set": meta("token").is_some(),
        "auto_minutes": meta("auto_minutes")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
        "last_auto": meta("last_auto")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
        "peers": peers,
        "has_vault": has_vault,
    }))
}

/// One-shot sync: `json` is `{"mode": "push"|"pull"|"run",
/// "dir"?, "relay"?, "token"?, "lan"?, "auto_minutes"?}`. With no
/// transport args the last-used target (saved under `sync.*` meta keys)
/// applies. `lan` discovers a paired mesh peer — zero config on the
/// same network. `auto_minutes` (0 = off) schedules background syncs.
/// Returns `{pushed, pulled, skipped}`.
/// # Safety
/// `handle` must come from `pai_init`; `json` is NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_sync_now(handle: *mut PaiRuntime, json: *const c_char) -> *mut c_char {
    let rt = &mut *handle;
    let raw = match read_str(json) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    #[derive(serde::Deserialize)]
    struct Req {
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        dir: Option<String>,
        #[serde(default)]
        relay: Option<String>,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        lan: bool,
        #[serde(default)]
        auto_minutes: Option<u64>,
    }
    let req: Req = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => return to_c(serde_json::json!({"error": format!("bad JSON: {e}")})),
    };
    if let Some(m) = req.auto_minutes {
        let _ = rt.store.meta_set("sync.auto_minutes", &m.to_string());
    }
    to_c(run_sync(
        &rt.rt,
        &rt.store,
        &rt.data_dir,
        rt.device,
        &rt.audit,
        &SyncArgs {
            mode: req.mode.unwrap_or_else(|| "run".into()),
            dir: req.dir,
            relay: req.relay,
            token: req.token,
            lan: req.lan,
        },
    ))
}

// ---------------------------------------------------------------------------
// Pairing — the file-based three-step flow (offer → accept → complete)
// that installs the shared vault key on both devices.

/// Create a pairing offer file: `offer.pai` written to `out`. Send it to
/// the other device (flash drive, shared folder, message) — it carries
/// only public keys and a signature.
/// # Safety
/// `handle` must come from `pai_init`; `out` is NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_pair_offer(
    handle: *mut PaiRuntime,
    out: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let out = match read_str(out) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let dir = std::path::Path::new(&rt.data_dir);
    let ids = pai_identity::IdentityStore::new(rt.store.clone());
    (|| -> Result<()> {
        let agree = pai_sync::crypto::agreement_key(rt.device, dir)?;
        let m = pai_sync::pair::make_offer(&rt.device_rec, &agree, &ids, &dir.join("keys"))?;
        pai_sync::pair::write_message(&m, std::path::Path::new(&out))
    })()
    .map(|()| {
        let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
        e.device = Some(rt.device);
        e.detail = serde_json::json!({"pair_offer": out});
        let _ = rt.audit.record(&e);
        to_c(serde_json::json!({
            "offer": out, "device": rt.device_rec.name}))
    })
    .unwrap_or_else(|e| to_c(serde_json::json!({"error": e.to_string()})))
}

/// Accept a pairing offer: verifies the signature, records the offerer
/// as a peer, and writes `accept.pai` (vault key sealed to them) to
/// `out`. Return it to the offering device to finish.
/// # Safety
/// `handle` must come from `pai_init`; paths are NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_pair_accept(
    handle: *mut PaiRuntime,
    offer: *const c_char,
    out: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let offer_p = match read_str(offer) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let out_p = match read_str(out) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let dir = std::path::Path::new(&rt.data_dir);
    let ids = pai_identity::IdentityStore::new(rt.store.clone());
    (|| -> Result<String> {
        let offer = pai_sync::pair::read_message(std::path::Path::new(&offer_p))?;
        let agree = pai_sync::crypto::agreement_key(rt.device, dir)?;
        let m = pai_sync::pair::accept_offer(
            &rt.store,
            &offer,
            &rt.device_rec,
            &agree,
            &ids,
            &dir.join("keys"),
            dir,
        )?;
        pai_sync::pair::write_message(&m, std::path::Path::new(&out_p))?;
        Ok(offer.name)
    })()
    .map(|name| {
        let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
        e.device = Some(rt.device);
        e.detail = serde_json::json!({"pair_accepted": name});
        let _ = rt.audit.record(&e);
        to_c(serde_json::json!({"accept": out_p, "peer": name}))
    })
    .unwrap_or_else(|e| to_c(serde_json::json!({"error": e.to_string()})))
}

/// Complete pairing on the offering device: verifies the accept,
/// records the acceptor as a peer, unwraps and adopts the vault key.
/// After this, `pai_sync_now` has a vault to encrypt to.
/// # Safety
/// `handle` must come from `pai_init`; `accept` is NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_pair_complete(
    handle: *mut PaiRuntime,
    accept: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let accept_p = match read_str(accept) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let dir = std::path::Path::new(&rt.data_dir);
    (|| -> Result<String> {
        let accept = pai_sync::pair::read_message(std::path::Path::new(&accept_p))?;
        let agree = pai_sync::crypto::agreement_key(rt.device, dir)?;
        pai_sync::pair::complete_pairing(&rt.store, &accept, &agree, dir)?;
        Ok(accept.name)
    })()
    .map(|name| {
        let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
        e.device = Some(rt.device);
        e.detail = serde_json::json!({"pair_completed": name});
        let _ = rt.audit.record(&e);
        to_c(serde_json::json!({"paired": name}))
    })
    .unwrap_or_else(|e| to_c(serde_json::json!({"error": e.to_string()})))
}

/// Pairing over the configured shared sync folder — one call performs
/// the whole exchange step that's currently possible:
/// publishes `offer-<id>.pai` under `<sync.dir>/pairing/`, accepts any
/// offers from devices we aren't already paired with (writing
/// `accept-<offerer>-<us>.pai` back), and completes any accepts
/// addressed to us. Two presses — one per device — finish the pair.
/// The folder is the channel the user controls; nothing new is trusted.
/// Returns `{published, accepted: [names], completed: [names],
/// rejected: [files]}`.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_pair_folder(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    let Some(dir) = rt.store.meta_get("sync.dir").ok().flatten() else {
        return to_c(serde_json::json!({
            "error": "no shared folder configured — set one under Sync now"
        }));
    };
    let pdir = std::path::Path::new(&dir).join("pairing");
    if let Err(e) = std::fs::create_dir_all(&pdir) {
        return to_c(serde_json::json!({"error": e.to_string()}));
    }
    let data_dir = std::path::Path::new(&rt.data_dir);
    let ids = pai_identity::IdentityStore::new(rt.store.clone());
    let our_id = rt.device.to_string();
    let known: std::collections::HashSet<String> = pai_sync::pair::list_peers(&rt.store)
        .map(|ps| ps.into_iter().map(|p| p.device_id.to_string()).collect())
        .unwrap_or_default();
    let mut accepted: Vec<String> = Vec::new();
    let mut completed: Vec<String> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    let out = (|| -> Result<()> {
        let agree = pai_sync::crypto::agreement_key(rt.device, data_dir)?;
        let offer =
            pai_sync::pair::make_offer(&rt.device_rec, &agree, &ids, &data_dir.join("keys"))?;
        pai_sync::pair::write_message(&offer, &pdir.join(format!("offer-{our_id}.pai")))?;
        for entry in std::fs::read_dir(&pdir).map_err(pai_storage::store_err)? {
            let entry = entry.map_err(pai_storage::store_err)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&format!("accept-{our_id}-")) {
                match pai_sync::pair::read_message(&entry.path()).and_then(|m| {
                    if known.contains(&m.device_id) {
                        return Err(Error::InvalidInput("already paired".into()));
                    }
                    pai_sync::pair::complete_pairing(&rt.store, &m, &agree, data_dir)
                        .map(|_| m.name)
                }) {
                    Ok(n) => completed.push(n),
                    Err(e) if e.to_string().contains("already paired") => {}
                    Err(_) => rejected.push(name),
                }
            } else if name.starts_with("offer-") {
                match pai_sync::pair::read_message(&entry.path()).and_then(|m| {
                    if m.device_id == our_id || known.contains(&m.device_id) {
                        return Err(Error::InvalidInput("skip".into()));
                    }
                    pai_sync::pair::accept_offer(
                        &rt.store,
                        &m,
                        &rt.device_rec,
                        &agree,
                        &ids,
                        &data_dir.join("keys"),
                        data_dir,
                    )
                    .and_then(|a| {
                        pai_sync::pair::write_message(
                            &a,
                            &pdir.join(format!("accept-{}-{our_id}.pai", m.device_id)),
                        )
                        .map(|_| m.name)
                    })
                }) {
                    Ok(n) => accepted.push(n),
                    Err(e) if e.to_string().contains("skip") => {}
                    Err(_) => rejected.push(name),
                }
            }
        }
        Ok(())
    })();
    match out {
        Ok(()) => {
            if !accepted.is_empty() || !completed.is_empty() {
                let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
                e.device = Some(rt.device);
                e.detail = serde_json::json!({
                    "pair_folder": &pdir,
                    "accepted": accepted, "completed": completed});
                let _ = rt.audit.record(&e);
            }
            to_c(serde_json::json!({
                "published": our_id,
                "accepted": accepted,
                "completed": completed,
                "rejected": rejected}))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Media jobs — local audio generation + the shared job log. Remote
// broker-routed jobs recorded by the CLI/broker also appear in
// `pai_media_list`; `pai_media_gen` generates on this device only.

/// Newest-first media job rows: `{id, kind, prompt, params, state,
/// requester, worker, result_blob, error, created_at, updated_at}`.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_media_list(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    match pai_media::jobs::list(&rt.store, 100) {
        Ok(rows) => to_c(serde_json::Value::Array(rows)),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Generate audio locally: `json` is `{"prompt", "duration_seconds"?}`
/// (seconds clamped to 1–300, default 10). Records the job in
/// `media_jobs`, stores the WAV in the blob store, returns
/// `{job_id, bytes, blob, state}`.
/// # Safety
/// `handle` must come from `pai_init`; `json` is NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_media_gen(
    handle: *mut PaiRuntime,
    json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let raw = match read_str(json) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    #[derive(serde::Deserialize)]
    struct Req {
        prompt: String,
        #[serde(default)]
        duration_seconds: Option<u32>,
    }
    let req: Req = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => return to_c(serde_json::json!({"error": format!("bad JSON: {e}")})),
    };
    if req.prompt.trim().is_empty() {
        return to_c(serde_json::json!({"error": "empty prompt"}));
    }
    let secs = req.duration_seconds.unwrap_or(10).clamp(1, 300);
    let dir = std::path::PathBuf::from(&rt.data_dir);
    // Local audio-gen server first; when none is configured, fall back
    // to a paired mesh peer advertising `media-run` — same routing the
    // CLI's `pai audio gen --on any` uses.
    let out: std::result::Result<(Vec<u8>, DeviceId), String> = rt.rt.block_on(async {
        if let Some(gen) = pai_media::providers::detect(&dir, Duration::from_secs(2)).await {
            return gen
                .generate_audio(&req.prompt, secs)
                .await
                .map(|b| (b, rt.device))
                .map_err(|e| e.to_string());
        }
        // Mesh: discover a paired peer, build a tokened relay transport
        // to it, and let its broker find the media-run worker.
        let ids = pai_identity::IdentityStore::new(rt.store.clone());
        let bind = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            pai_mesh::MULTICAST_PORT,
        );
        let sock = pai_mesh::bind_listener(bind, Some(pai_mesh::MULTICAST_GROUP))
            .map_err(|e| e.to_string())?;
        let found = pai_mesh::discover(&sock, Duration::from_secs(3));
        let paired =
            pai_mesh::paired_announcements(&rt.store, &ids, found).map_err(|e| e.to_string())?;
        let target = paired.first().ok_or_else(|| {
            "no audio-gen server here and no paired mesh peer announcing".to_string()
        })?;
        let agree = pai_sync::crypto::agreement_key(rt.device, &dir).map_err(|e| e.to_string())?;
        let token = pai_mesh::token_for(&agree.secret, &target.peer);
        let transport = pai_sync::relay::RelayTransport::new(
            format!("http://{}", target.relay_addr),
            Some(token),
        );
        let vault = pai_sync::crypto::vault_key(&dir)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no vault key — pair a device first".to_string())?;
        let client = pai_broker::rpc::BrokerClient::new(&transport, &vault, rt.device);
        let to = client
            .find_peer("media-run")
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no paired device advertises media-run".to_string())?;
        let payload = serde_json::json!({
            "prompt": req.prompt, "duration_seconds": secs,
        })
        .to_string()
        .into_bytes();
        let resp = client
            .call(to, "media-run", &payload, Duration::from_secs(660))
            .await
            .map_err(|e| e.to_string())?;
        let v: serde_json::Value =
            serde_json::from_slice(&resp).map_err(|e| format!("bad media-run reply: {e}"))?;
        let bytes = v["audio_b64"]
            .as_str()
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .ok_or_else(|| "media-run reply missing audio_b64".to_string())?;
        Ok((bytes, to))
    });
    let mut job = pai_media::jobs::new_job(pai_media::MediaJobKind::TextToAudio, &req.prompt);
    let params = serde_json::json!({"duration_seconds": secs}).to_string();
    let _ = pai_media::jobs::record(&rt.store, &job, Some(&params), Some(rt.device), None);
    match out {
        Ok((bytes, worker)) => {
            job.state = pai_media::JobState::Done;
            job.placement_device = Some(worker);
            let blob = match rt.store.put_blob(&bytes) {
                Ok(b) => b,
                Err(e) => {
                    job.state = pai_media::JobState::Failed;
                    let _ = pai_media::jobs::record(
                        &rt.store,
                        &job,
                        Some(&params),
                        Some(rt.device),
                        Some(&e.to_string()),
                    );
                    return to_c(serde_json::json!({"error": e.to_string()}));
                }
            };
            job.result_blob = Some(blob.clone());
            let _ = pai_media::jobs::record(&rt.store, &job, Some(&params), Some(rt.device), None);
            let mut e = pai_audit::event(AuditKind::ConfigChanged, AuditOutcome::Ok);
            e.device = Some(rt.device);
            e.detail =
                serde_json::json!({"media_gen": "audio", "secs": secs, "bytes": bytes.len()});
            let _ = rt.audit.record(&e);
            to_c(serde_json::json!({
                "job_id": job.id.to_string(), "state": "done",
                "bytes": bytes.len(), "blob": blob}))
        }
        Err(e) => {
            job.state = pai_media::JobState::Failed;
            let _ =
                pai_media::jobs::record(&rt.store, &job, Some(&params), Some(rt.device), Some(&e));
            to_c(serde_json::json!({"error": e}))
        }
    }
}

/// Export a finished job's result blob to `dest` — writes the stored
/// bytes (a WAV today) and returns `{path, bytes}`.
/// # Safety
/// `handle` must come from `pai_init`; `job_id`/`dest` are NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pai_media_export(
    handle: *mut PaiRuntime,
    job_id: *const c_char,
    dest: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let id = match read_str(job_id) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let dest = match read_str(dest) {
        Ok(s) => s,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let task = match uuid::Uuid::parse_str(&id) {
        Ok(u) => TaskId(u),
        Err(e) => return to_c(serde_json::json!({"error": format!("bad job id: {e}")})),
    };
    let row = match pai_media::jobs::get(&rt.store, task) {
        Ok(Some(r)) => r,
        Ok(None) => return to_c(serde_json::json!({"error": "job not found"})),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let blob = match row["result_blob"].as_str() {
        Some(b) => b.to_string(),
        None => return to_c(serde_json::json!({"error": "job has no result yet"})),
    };
    match rt.store.get_blob(&blob) {
        Ok(bytes) => match std::fs::write(&dest, &bytes) {
            Ok(()) => to_c(serde_json::json!({"path": dest, "bytes": bytes.len()})),
            Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
        },
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Voice capability probe: `{stt, tts, mic, speaker, whisper_url}`.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_voice_status(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    let (stt, tts, whisper_url) = match &rt.voice {
        Some(v) => (v.stt.is_some(), v.tts.is_some(), v.cfg.whisper_url()),
        None => (false, false, String::new()),
    };
    to_c(serde_json::json!({
        "stt": stt,
        "tts": tts,
        "mic": pai_voice::mic::input_available(),
        "speaker": pai_voice::mic::output_available(),
        "whisper_url": whisper_url,
    }))
}

/// Capture one VAD-endpointed utterance from the default mic, then
/// transcribe it when whisper-server is configured. Blocks up to
/// `max_secs` (clamped 1..=120). Returns `{heard, text?, wav_b64}`.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_voice_listen(handle: *mut PaiRuntime, max_secs: u32) -> *mut c_char {
    let rt = &mut *handle;
    let Some(v) = &rt.voice else {
        return voice_err("voice not configured — `pai voice configure`");
    };
    if !pai_voice::mic::input_available() {
        return voice_err("no microphone detected");
    }
    let pcm = match pai_voice::mic::capture_utterance(&v.vad, max_secs.clamp(1, 120)) {
        Ok(p) => p,
        Err(e) => return voice_err(&e.to_string()),
    };
    if pcm.is_empty() {
        return to_c(serde_json::json!({"heard": false}));
    }
    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    let wav = pai_voice::pcm16_to_wav(&bytes, pai_voice::mic::TARGET_RATE);
    let text = match &v.stt {
        Some(stt) => match rt.rt.block_on(stt.transcribe(&wav, "audio/wav")) {
            Ok(t) => Some(t),
            Err(e) => return voice_err(&e.to_string()),
        },
        None => None,
    };
    to_c(serde_json::json!({
        "heard": true,
        "text": text,
        "wav_b64": base64::engine::general_purpose::STANDARD.encode(&wav),
    }))
}

/// Streaming listen: transcribe each pause-finalized segment as it
/// closes. FFI can't push live events, so partials return collected —
/// `{heard, text, partials[], wav_b64}` where `partials` is the ordered
/// per-segment transcript (the UI can render the segmentation and the
/// text is the joined final).
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_voice_listen_stream(
    handle: *mut PaiRuntime,
    max_secs: u32,
) -> *mut c_char {
    let rt = &mut *handle;
    let Some(v) = &rt.voice else {
        return voice_err("voice not configured — `pai voice configure`");
    };
    if !pai_voice::mic::input_available() {
        return voice_err("no microphone detected");
    }
    let Some(stt) = &v.stt else {
        return voice_err("whisper-server unreachable — `pai voice configure`");
    };
    let mut partials: Vec<String> = Vec::new();
    let text = match pai_voice::stream_transcribe(stt, &v.vad, max_secs.clamp(1, 120), &mut |p| {
        partials.push(p.to_string())
    }) {
        Ok(t) => t,
        Err(e) => return voice_err(&e.to_string()),
    };
    to_c(serde_json::json!({
        "heard": !text.is_empty(),
        "text": text,
        "partials": partials,
    }))
}

/// Transcribe a WAV file by absolute path. Returns `{text}`.
/// # Safety
/// `handle` must come from `pai_init`; `path` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn pai_voice_transcribe(
    handle: *mut PaiRuntime,
    path: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let Some(v) = &rt.voice else {
        return voice_err("voice not configured — `pai voice configure`");
    };
    let Some(stt) = &v.stt else {
        return voice_err("whisper-server not configured/unreachable");
    };
    let path = match read_str(path) {
        Ok(s) => s.to_string(),
        Err(e) => return voice_err(&e.to_string()),
    };
    let wav = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return voice_err(&format!("{path}: {e}")),
    };
    match rt.rt.block_on(stt.transcribe(&wav, "audio/wav")) {
        Ok(text) => to_c(serde_json::json!({"text": text})),
        Err(e) => voice_err(&e.to_string()),
    }
}

/// Speak `text` through the default speaker via piper. Returns
/// `{ok, played}` — when playback fails the WAV is returned as
/// `{ok, played:false, wav_b64}` so the UI can render it another way.
/// # Safety
/// `handle` must come from `pai_init`; `text` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn pai_voice_say(
    handle: *mut PaiRuntime,
    text: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let Some(v) = &rt.voice else {
        return voice_err("voice not configured — `pai voice configure`");
    };
    let Some(tts) = &v.tts else {
        return voice_err("piper not configured — `pai voice configure`");
    };
    let text = match read_str(text) {
        Ok(s) => s.to_string(),
        Err(e) => return voice_err(&e.to_string()),
    };
    let wav = match rt.rt.block_on(tts.synthesize(&text, None)) {
        Ok(w) => w,
        Err(e) => return voice_err(&e.to_string()),
    };
    let played = pai_voice::mic::wav_to_pcm16(&wav)
        .and_then(|(rate, pcm)| pai_voice::mic::play(&pcm, rate))
        .is_ok();
    if played {
        to_c(serde_json::json!({"ok": true, "played": true}))
    } else {
        to_c(serde_json::json!({
            "ok": true,
            "played": false,
            "wav_b64": base64::engine::general_purpose::STANDARD.encode(&wav),
        }))
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Probe local inference endpoints (llama-server / Ollama / LM Studio) and
/// provider binaries on PATH. No handle needed — used by first-run setup.
/// # Safety
/// No pointer requirements; result must be freed with `pai_free_string`.
#[no_mangle]
pub unsafe extern "C" fn pai_detect() -> *mut c_char {
    let endpoints = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt.block_on(pai_inference::detect_endpoints(Duration::from_secs(2))),
        Err(_) => vec![],
    };
    let binaries = serde_json::json!({
        "llama-server": pai_inference::find_in_path("llama-server")
            .map(|p| p.to_string_lossy().into_owned()),
        "ollama": pai_inference::find_in_path("ollama")
            .map(|p| p.to_string_lossy().into_owned()),
        "lms": pai_inference::find_in_path("lms")
            .map(|p| p.to_string_lossy().into_owned()),
    });
    to_c(serde_json::json!({
        "endpoints": endpoints,
        "binaries": binaries,
    }))
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

/// # Safety
/// `s` must be a pointer previously returned by this library.
/// List paired peer devices — the migrate picker needs them.
/// Returns `{peers: [{id, name, platform}]}` or `{error}`.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_peers_list(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    match pai_sync::pair::list_peers(&rt.store) {
        Ok(peers) => to_c(serde_json::json!({
            "peers": peers.iter().map(|p| serde_json::json!({
                "id": p.device_id.to_string(),
                "name": p.name,
                "platform": p.platform,
            })).collect::<Vec<_>>(),
        })),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Migrate an app to a paired device — the same steps as
/// `pai apps migrate`: a migrate-flagged backup, the placement update,
/// and local `data/` parked (recoverable). Ships on the next sync
/// push; the target restores inline on its next pull.
/// `to` is a device-id prefix resolved against paired peers.
/// Returns `{ok: true, pak}` or `{error}`.
/// # Safety
/// `handle` must come from `pai_init`; `id`/`to` are NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn pai_apps_migrate(
    handle: *mut PaiRuntime,
    id: *const c_char,
    to: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let id = match read_str(id) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let prefix = match read_str(to) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let peers = match pai_sync::pair::list_peers(&rt.store) {
        Ok(p) => p,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let matches: Vec<_> = peers
        .iter()
        .filter(|p| p.device_id.to_string().starts_with(&prefix))
        .collect();
    let target = match matches.as_slice() {
        [p] => p.device_id,
        [] => {
            return to_c(serde_json::json!({"error":
                format!("no paired device matching '{prefix}'")}))
        }
        _ => {
            return to_c(serde_json::json!({"error":
                format!("'{prefix}' matches {} devices — be more specific",
                    matches.len())}))
        }
    };
    // Order matters: snapshot while still active (create refuses on an
    // inactive app), then hand over placement, then park local data.
    let pak = match pai_sync::backup::create(
        &rt.store,
        std::path::Path::new(&rt.data_dir),
        rt.device,
        &id,
        Some(target),
    ) {
        Ok(p) => p,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let now = pai_core::now().to_rfc3339();
    let upd = rt.store.with_conn(|c| {
        c.execute(
            "UPDATE apps SET active_device=?2, updated_at=?3 WHERE id=?1",
            rusqlite::params![id, target.to_string(), now],
        )
    });
    if let Err(e) = upd {
        return to_c(serde_json::json!({"error": e.to_string()}));
    }
    match pai_apps::AppRegistry::new(std::path::Path::new(&rt.data_dir)).deactivate_data(&id) {
        Ok(_) => {}
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    }
    let mut ev = pai_audit::event(AuditKind::AppMigrated, AuditOutcome::Ok);
    ev.device = Some(rt.device);
    ev.detail = serde_json::json!({"app_id": id, "to": target.to_string()});
    let _ = rt.audit.record(&ev);
    to_c(serde_json::json!({"ok": true, "pak": pak.display().to_string()}))
}

/// Mint a capability token for an installed app — the `pai apps
/// share` op. `actions_csv` is a comma list of exec/read/write/share;
/// `for_device` optionally binds the grant to a paired peer's device
/// key (empty string = bearer token anyone holding it may use).
/// `days` bounds validity. Returns `{ok, token_id, token_json}` —
/// `token_json` is the file the guest holds; it also lands under
/// `<data_dir>/share/tokens/`.
/// # Safety
/// `handle` must come from `pai_init`; strings are NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn pai_share_grant(
    handle: *mut PaiRuntime,
    app_id: *const c_char,
    actions_csv: *const c_char,
    days: i64,
    for_device: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let app_id = match read_str(app_id) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let actions_csv = match read_str(actions_csv) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let for_device = match read_str(for_device) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let reg = pai_apps::AppRegistry::new(std::path::Path::new(&rt.data_dir));
    match reg.get(&app_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return to_c(serde_json::json!({"error":
                format!("app {app_id} not installed")}))
        }
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    }
    let mut actions = Vec::new();
    for a in actions_csv.split(',') {
        actions.push(match a.trim() {
            "exec" => pai_share::Action::Exec,
            "read" => pai_share::Action::Read,
            "write" => pai_share::Action::Write,
            "share" => pai_share::Action::Share,
            other => {
                return to_c(serde_json::json!({"error":
                    format!("unknown action '{other}'")}))
            }
        });
    }
    let grantee_key = if for_device.is_empty() {
        None
    } else {
        let peers = match pai_sync::pair::list_peers(&rt.store) {
            Ok(p) => p,
            Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
        };
        let m: Vec<_> = peers
            .iter()
            .filter(|p| p.device_id.to_string().starts_with(&for_device))
            .collect();
        match m.len() {
            1 => Some(m[0].ed_pubkey),
            0 => {
                return to_c(serde_json::json!({"error":
                    format!("no paired device matching '{for_device}'")}))
            }
            n => {
                return to_c(serde_json::json!({"error":
                    format!("'{for_device}' matches {n} devices")}))
            }
        }
    };
    let mut spec = pai_share::GrantSpec::for_app(app_id.clone(), actions.clone());
    spec.grantee_key = grantee_key;
    spec.expires = Some(pai_core::now().timestamp() + days.max(1) * 86_400);
    let shares = pai_share::ShareStore::new(std::path::Path::new(&rt.data_dir));
    let ids = pai_identity::IdentityStore::new(rt.store.clone());
    let key_dir = std::path::Path::new(&rt.data_dir).join("keys");
    let cap = match shares.grant(&ids, &key_dir, rt.device, spec) {
        Ok(c) => c,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let json = match cap.to_json() {
        Ok(j) => j,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let path = std::path::Path::new(&rt.data_dir)
        .join("share")
        .join("tokens")
        .join(format!("{}-{}.json", cap.app_id, cap.token_id));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &json);
    let mut ev = pai_audit::event(AuditKind::AppShared, AuditOutcome::Ok);
    ev.device = Some(rt.device);
    ev.detail = serde_json::json!({
        "app_id": app_id,
        "token_id": cap.token_id,
        "actions": actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "bound": cap.grantee_key.is_some(),
        "expires": cap.expires,
    });
    let _ = rt.audit.record(&ev);
    to_c(serde_json::json!({
        "ok": true,
        "token_id": cap.token_id,
        "token_json": json,
        "path": path.display().to_string(),
        "expires": cap.expires,
        "bound": cap.grantee_key.is_some(),
    }))
}

/// Re-grant a narrower sub-token from a parent token this device
/// holds — `parent_json` is a token file's contents (from `pai apps
/// share` or `pai_share_grant`). The parent must carry `share` and be
/// bound to this device's key. `actions_csv` narrows within the
/// parent's set; `days` ≤ 0 keeps the parent's expiry. `for_key` is a
/// paired-device prefix or a 64-hex Ed25519 pubkey (empty = bearer).
/// Returns `{ok, token_id, token_json}` — the child embeds the chain.
/// # Safety
/// `handle` must come from `pai_init`; strings are NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn pai_share_delegate(
    handle: *mut PaiRuntime,
    parent_json: *const c_char,
    actions_csv: *const c_char,
    days: i64,
    for_key: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let parent_json = match read_str(parent_json) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let actions_csv = match read_str(actions_csv) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let for_key = match read_str(for_key) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let parent_cap = match pai_share::Capability::from_json(&parent_json) {
        Ok(c) => c,
        Err(e) => {
            return to_c(serde_json::json!({"error":
                format!("bad parent token: {e}")}))
        }
    };
    // The chain only verifies when this device's key is the key the
    // parent was bound to — fail early with a readable message.
    let my_key: Option<String> = rt
        .store
        .with_conn(|c| {
            c.query_row(
                "SELECT public_key FROM devices WHERE id=?1",
                rusqlite::params![rt.device.to_string()],
                |r| r.get::<_, Vec<u8>>(0),
            )
        })
        .ok()
        .map(hex::encode);
    if parent_cap.grantee_key.as_deref() != my_key.as_deref() {
        return to_c(serde_json::json!({"error":
            "parent token isn't bound to this device's key"}));
    }
    let mut actions = Vec::new();
    for a in actions_csv.split(',') {
        actions.push(match a.trim() {
            "exec" => pai_share::Action::Exec,
            "read" => pai_share::Action::Read,
            "write" => pai_share::Action::Write,
            "share" => pai_share::Action::Share,
            other => {
                return to_c(serde_json::json!({"error":
                    format!("unknown action '{other}'")}))
            }
        });
    }
    let grantee_key = if for_key.is_empty() {
        None
    } else if let Ok(raw) = hex::decode(&for_key) {
        match <[u8; 32]>::try_from(raw.as_slice()) {
            Ok(k) => Some(k),
            Err(_) => {
                return to_c(serde_json::json!({"error":
                    "for_key hex must be 32 bytes (64 hex chars)"}))
            }
        }
    } else {
        let peers = match pai_sync::pair::list_peers(&rt.store) {
            Ok(p) => p,
            Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
        };
        let m: Vec<_> = peers
            .iter()
            .filter(|p| p.device_id.to_string().starts_with(&for_key))
            .collect();
        match m.len() {
            1 => Some(m[0].ed_pubkey),
            0 => {
                return to_c(serde_json::json!({"error":
                    format!("no paired device matching '{for_key}'")}))
            }
            n => {
                return to_c(serde_json::json!({"error":
                    format!("'{for_key}' matches {n} devices")}))
            }
        }
    };
    let mut spec = pai_share::GrantSpec::for_app(parent_cap.app_id.clone(), actions.clone());
    spec.grantee_key = grantee_key;
    spec.device = parent_cap.device;
    spec.expires = if days > 0 {
        Some(pai_core::now().timestamp() + days * 86_400)
    } else {
        parent_cap.expires
    };
    let shares = pai_share::ShareStore::new(std::path::Path::new(&rt.data_dir));
    let ids = pai_identity::IdentityStore::new(rt.store.clone());
    let key_dir = std::path::Path::new(&rt.data_dir).join("keys");
    let cap = match shares.delegate(&ids, &key_dir, rt.device, &parent_cap, spec) {
        Ok(c) => c,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let json = match cap.to_json() {
        Ok(j) => j,
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let path = std::path::Path::new(&rt.data_dir)
        .join("share")
        .join("tokens")
        .join(format!("{}-{}.json", cap.app_id, cap.token_id));
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(&path, &json);
    let mut ev = pai_audit::event(AuditKind::AppShared, AuditOutcome::Ok);
    ev.device = Some(rt.device);
    ev.detail = serde_json::json!({
        "app_id": cap.app_id,
        "token_id": cap.token_id,
        "parent": parent_cap.token_id,
        "delegated": true,
        "actions": actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "bound": cap.grantee_key.is_some(),
        "expires": cap.expires,
    });
    let _ = rt.audit.record(&ev);
    to_c(serde_json::json!({
        "ok": true,
        "token_id": cap.token_id,
        "token_json": json,
        "path": path.display().to_string(),
        "parent": parent_cap.token_id,
        "expires": cap.expires,
        "bound": cap.grantee_key.is_some(),
    }))
}

/// List issued capability grants — `{grants: [{token_id, app_id,
/// actions, status, expires, bound}]}` newest first.
/// # Safety
/// `handle` must come from `pai_init`.
#[no_mangle]
pub unsafe extern "C" fn pai_share_list(handle: *mut PaiRuntime) -> *mut c_char {
    let rt = &mut *handle;
    let shares = pai_share::ShareStore::new(std::path::Path::new(&rt.data_dir));
    match shares.list() {
        Ok(list) => to_c(serde_json::json!({
            "grants": list.iter().map(|(c, st)| serde_json::json!({
                "token_id": c.token_id,
                "app_id": c.app_id,
                "actions": c.actions.iter().map(|a| a.to_string())
                    .collect::<Vec<_>>(),
                "status": format!("{st:?}").to_lowercase(),
                "expires": c.expires,
                "bound": c.grantee_key.is_some(),
                "parent": c.parent.as_ref().map(|p| p.token_id.clone()),
            })).collect::<Vec<_>>(),
        })),
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// Revoke a capability grant — the token is refused from the next
/// guest request. Returns `{ok: true}` or `{error}`.
/// # Safety
/// `handle` must come from `pai_init`; `token_id` is NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn pai_share_revoke(
    handle: *mut PaiRuntime,
    token_id: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let token_id = match read_str(token_id) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let shares = pai_share::ShareStore::new(std::path::Path::new(&rt.data_dir));
    match shares.revoke(&token_id) {
        Ok(true) => {}
        Ok(false) => {
            return to_c(serde_json::json!({"error":
                format!("no grant {token_id}")}))
        }
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    }
    let mut ev = pai_audit::event(AuditKind::AppShareRevoked, AuditOutcome::Ok);
    ev.device = Some(rt.device);
    ev.detail = serde_json::json!({"token_id": token_id});
    let _ = rt.audit.record(&ev);
    to_c(serde_json::json!({"ok": true}))
}

/// Guest-side capability call — the `apps run/read/write --cap` ops
/// over FFI. `request_json`:
/// `{op: "app-run"|"app-read"|"app-write", app_id, args: [..],
///   token: <token JSON>, to: <device id | "" → root issuer>,
///   dir: <shared folder> | relay: <url>, relay_token,
///   timeout_secs}`.
/// Bound tokens are signed with this device's key — a token bound
/// elsewhere fails fast with a readable error. Returns
/// `{ok, payload_b64}` or `{error}`.
/// # Safety
/// `handle` must come from `pai_init`; `request_json` is NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn pai_guest_call(
    handle: *mut PaiRuntime,
    request_json: *const c_char,
) -> *mut c_char {
    let rt = &mut *handle;
    let req_raw = match read_str(request_json) {
        Ok(s) => s.to_string(),
        Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
    };
    let req: serde_json::Value = match serde_json::from_str(&req_raw) {
        Ok(v) => v,
        Err(e) => return to_c(serde_json::json!({"error": format!("bad request json: {e}")})),
    };
    let get = |k: &str| req.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let op = get("op").to_string();
    let app_id = get("app_id").to_string();
    let args: Vec<String> = req
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let token_json = get("token").to_string();
    let timeout_secs = req
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let capability = match pai_share::Capability::from_json(&token_json) {
        Ok(c) => c,
        Err(e) => return to_c(serde_json::json!({"error": format!("bad token: {e}")})),
    };
    if capability.app_id != app_id {
        return to_c(serde_json::json!({"error":
            format!("token grants app {} not {app_id}", capability.app_id)}));
    }
    // `to` empty → the root issuer (walks a delegated chain's parents).
    let to_str = get("to");
    let to = if to_str.is_empty() {
        let mut root = &capability;
        while let Some(p) = &root.parent {
            root = p;
        }
        root.issued_by
    } else {
        match uuid::Uuid::parse_str(to_str) {
            Ok(u) => DeviceId(u),
            Err(_) => {
                // fall back to a paired-peer prefix
                let peers = match pai_sync::pair::list_peers(&rt.store) {
                    Ok(p) => p,
                    Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
                };
                match peers
                    .iter()
                    .find(|p| p.device_id.to_string().starts_with(to_str))
                {
                    Some(p) => p.device_id,
                    None => {
                        return to_c(serde_json::json!({"error":
                            format!("no device matching '{to_str}'")}))
                    }
                }
            }
        }
    };
    // A bound token must be signed by THIS device — otherwise it can
    // never verify on the host.
    if capability.grantee_key.is_some() {
        let my_key: Option<String> = rt
            .store
            .with_conn(|c| {
                c.query_row(
                    "SELECT public_key FROM devices WHERE id=?1",
                    rusqlite::params![rt.device.to_string()],
                    |r| r.get::<_, Vec<u8>>(0),
                )
            })
            .ok()
            .map(hex::encode);
        if capability.grantee_key.as_deref() != my_key.as_deref() {
            return to_c(serde_json::json!({"error":
                "token is bound to a different device's key"}));
        }
    }
    let dir = get("dir").to_string();
    let relay = get("relay").to_string();
    let relay_token = req
        .get("relay_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let ids = pai_identity::IdentityStore::new(rt.store.clone());
    let key_dir = std::path::Path::new(&rt.data_dir).join("keys");
    let dev = rt.device;
    let signer = move |msg: &[u8]| {
        ids.sign(dev, &key_dir, msg)
            .map_err(|e| pai_share::ShareError::InvalidInput(e.to_string()))
    };
    let out = if !dir.is_empty() {
        let t = match pai_sync::FolderTransport::new(dir.into()) {
            Ok(t) => t,
            Err(e) => return to_c(serde_json::json!({"error": e.to_string()})),
        };
        rt.rt.block_on(pai_share::guest::call_guest(
            &t,
            to,
            capability,
            &op,
            &args,
            Duration::from_secs(timeout_secs),
            Some(&signer),
        ))
    } else if !relay.is_empty() {
        let t = pai_sync::relay::RelayTransport::new(relay, relay_token);
        rt.rt.block_on(pai_share::guest::call_guest(
            &t,
            to,
            capability,
            &op,
            &args,
            Duration::from_secs(timeout_secs),
            Some(&signer),
        ))
    } else {
        return to_c(serde_json::json!({"error": "pass dir or relay"}));
    };
    match out {
        Ok(payload) => {
            use base64::Engine as _;
            to_c(serde_json::json!({
                "ok": true,
                "payload_b64": base64::engine::general_purpose::STANDARD.encode(payload),
            }))
        }
        Err(e) => to_c(serde_json::json!({"error": e.to_string()})),
    }
}

/// # Safety
/// `s` must be a pointer previously returned by this library.
#[no_mangle]
pub unsafe extern "C" fn pai_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

/// # Safety
/// `handle` must come from `pai_init` and must not be used after this call.
#[no_mangle]
pub unsafe extern "C" fn pai_free(handle: *mut PaiRuntime) {
    if !handle.is_null() {
        let rt = Box::from_raw(handle);
        if let Some(mut c) = rt.llama_child.lock().unwrap().take() {
            let _ = c.kill();
        }
        drop(rt);
    }
}

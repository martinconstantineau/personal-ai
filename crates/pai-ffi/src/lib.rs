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
    EchoProvider, LlamaServerProvider, SpeechToTextProvider, TextToSpeechProvider,
};
use pai_memory::{MemoryBackend, MemoryScopeQuery, RecallQuery, SqliteMemory};
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
    device: DeviceId,
    documents: Arc<pai_documents::DocumentStore>,
    email: Option<Arc<dyn pai_connector_email::EmailProvider>>,
    /// Detected voice providers (whisper-server STT / piper TTS) — None
    /// when neither is configured. Mic/speaker probes are cheap enough
    /// to answer live in `pai_voice_status`.
    voice: Option<pai_voice::VoiceSetup>,
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
    let user = ids.create_user(cfg.user_name.as_deref().unwrap_or("user"))?;
    let key_dir = data_dir.join("keys");
    let device = ids.register_device(
        user.id,
        cfg.device_name.as_deref().unwrap_or("this-device"),
        current_platform(),
        pai_identity::probe_capabilities(),
        &key_dir,
    )?;

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
                    model = ep.models.first().cloned();
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
        allowed_roots: vec![inbox],
    };

    // Active conversation: restored, or a fresh one.
    let conversation = cfg
        .conversation
        .as_deref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(ConversationId)
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
        device: device.id,
        documents,
        email,
        voice,
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
            if rt.conversation == cid {
                // Fall back to a fresh shared conversation.
                if let Ok(c) = rt.conversations.create(rt.session, MemoryIsolation::Shared) {
                    rt.conversation = c.id;
                }
            }
            to_c(serde_json::json!({"ok": true}))
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

fn voice_err(msg: &str) -> *mut c_char {
    to_c(serde_json::json!({"error": msg}))
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
        drop(Box::from_raw(handle));
    }
}

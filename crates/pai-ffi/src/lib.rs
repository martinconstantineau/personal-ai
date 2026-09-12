//! C ABI for the Flutter UI. Convention: JSON strings in, JSON strings out,
//! caller frees every returned pointer with `pai_free_string`.
//!
//! - `pai_init(config_json)` → opaque runtime handle
//! - `pai_send(handle, user_text)` → JSON `{events: [...], answer, run_id}`
//! - `pai_audit(handle, limit)` → JSON array of recent audit events
//! - `pai_memories(handle)` → JSON array of memory items
//! - `pai_free(handle)` / `pai_free_string(ptr)`
//!
//! `pai_send` is blocking (call it from a Dart isolate); a callback-based
//! streaming variant is the documented next step.

use pai_agent::{AgentDefinition, AgentRuntime, AutoApprove, CancelToken, RunRequest};
use pai_core::*;
use pai_inference::{EchoProvider, LlamaServerProvider};
use pai_memory::{MemoryBackend, RecallQuery, SqliteMemory};
use pai_permissions::{PolicyEngine, PolicyTable};
use pai_storage::Store;
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, CStr, CString};
use std::sync::Arc;

pub struct PaiRuntime {
    rt: tokio::runtime::Runtime,
    agent: AgentRuntime,
    memory: Arc<dyn MemoryBackend>,
    audit: Arc<pai_audit::AuditLog>,
    def: AgentDefinition,
    history: Vec<Message>,
    conversation: ConversationId,
    /// Serializes `pai_send` — callers may invoke from any thread/isolate.
    send_lock: std::sync::Mutex<()>,
}

#[derive(Deserialize)]
struct InitConfig {
    data_dir: String,
    /// "echo" (always available) or "llama-server".
    provider: Option<String>,
    model: Option<String>,
    server_url: Option<String>,
    user_name: Option<String>,
    device_name: Option<String>,
}

fn init_runtime(cfg: InitConfig) -> Result<PaiRuntime> {
    let data_dir = std::path::PathBuf::from(&cfg.data_dir);
    let store = Arc::new(Store::open(&data_dir)?);

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

    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemory::new(store.clone()));
    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));

    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    let server_url = cfg
        .server_url
        .unwrap_or_else(|| "http://127.0.0.1:8080".into());
    providers.register(Arc::new(
        LlamaServerProvider::new(server_url, cfg.model.clone().unwrap_or_default())
            .with_timeout(std::time::Duration::from_secs(120)),
    ));

    let agent = AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(PolicyTable::with_defaults())),
        memory: memory.clone(),
        audit: audit.clone(),
        max_steps: 8,
        step_timeout: std::time::Duration::from_secs(120),
        device: device.id,
    };

    Ok(PaiRuntime {
        rt: tokio::runtime::Runtime::new().map_err(|e| Error::Other(e.to_string()))?,
        agent,
        memory,
        audit,
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
            provider: cfg.provider.unwrap_or_else(|| "echo".into()),
            model: cfg.model,
        },
        history: vec![],
        conversation: ConversationId::new(),
        send_lock: std::sync::Mutex::new(()),
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

/// config_json: {"data_dir": "...", "provider": "echo"|"llama-server", ...}
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

#[derive(Serialize)]
struct SendResult {
    run_id: String,
    state: String,
    answer: Option<String>,
    events: Vec<serde_json::Value>,
    error: Option<String>,
}

/// Send one user message; returns JSON with the run's events + final answer.
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

    let events = std::sync::Mutex::new(Vec::<serde_json::Value>::new());
    let emit = |e: pai_agent::AgentEvent| {
        events
            .lock()
            .unwrap()
            .push(serde_json::to_value(&e).unwrap_or_default());
    };

    let history = std::mem::take(&mut rt.history);
    let outcome = rt.rt.block_on(rt.agent.run(RunRequest {
        definition: &rt.def,
        history,
        input: text.clone(),
        conversation: Some(rt.conversation),
        approval: &AutoApprove,
        cancel: CancelToken::default(),
        emit: &emit,
    }));

    match outcome {
        Ok(o) => {
            rt.history.push(Message {
                id: MessageId::new(),
                conversation: rt.conversation,
                role: Role::User,
                created_at: now(),
                content: vec![Content::text(&text)],
                trust: TrustLevel::User,
            });
            if let Some(a) = &o.answer {
                rt.history.push(Message {
                    id: MessageId::new(),
                    conversation: rt.conversation,
                    role: Role::Assistant,
                    created_at: now(),
                    content: vec![Content::text(a)],
                    trust: TrustLevel::Generated,
                });
            }
            to_c(SendResult {
                run_id: o.run.id.to_string(),
                state: format!("{:?}", o.run.state).to_lowercase(),
                answer: o.answer,
                events: events.into_inner().unwrap(),
                error: None,
            })
        }
        Err(e) => to_c(SendResult {
            run_id: String::new(),
            state: "failed".into(),
            answer: None,
            events: events.into_inner().unwrap(),
            error: Some(e.to_string()),
        }),
    }
}

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

/// All non-deleted memories as JSON.
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
                ..Default::default()
            })
            .await
    });
    match out {
        Ok(items) => to_c(items.iter().map(|s| &s.item).collect::<Vec<_>>()),
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
        drop(Box::from_raw(handle));
    }
}

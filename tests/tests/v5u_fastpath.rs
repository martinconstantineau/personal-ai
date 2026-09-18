//! V5u tests: the deterministic intent fast-path + protocol recovery.
//!
//! - Strong local intents (write code, file ops, remember) execute
//!   through the same permission/approval/audit gate as model calls —
//!   the model is only asked for file *contents*, never protocol JSON.
//! - Malformed near-protocol model output gets one correction round
//!   instead of silently becoming the "answer".

use pai_agent::{
    AgentDefinition, AgentEvent, AgentRuntime, ApprovalHandler, AutoApprove, CancelToken,
    ConversationStore, DenyApprovals, Persistence, RunRequest, RunStore,
};
use pai_core::*;
use pai_inference::{AIRequest, GenerateResponse, InferenceProvider, ProviderRegistry};
use pai_memory::SqliteMemory;
use pai_permissions::{PolicyEngine, PolicyTable};
use pai_storage::Store;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5u-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Provider replaying canned responses in order — drives the recovery
/// and codegen paths without a real model.
struct Scripted {
    responses: Mutex<VecDeque<String>>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(lines: &[&str]) -> Self {
        Self {
            responses: Mutex::new(lines.iter().map(|s| s.to_string()).collect()),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl InferenceProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn capabilities(&self) -> Vec<ModelCapability> {
        vec![ModelCapability::TextGeneration]
    }
    async fn available_models(&self) -> Result<Vec<String>> {
        Ok(vec!["scripted-0".into()])
    }
    async fn generate(&self, _req: &AIRequest) -> Result<GenerateResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "{\"type\":\"final\",\"content\":\"done\"}".into());
        Ok(GenerateResponse {
            text: text.clone(),
            action: pai_inference::parse_action(&text),
            ..Default::default()
        })
    }
}

fn runtime(
    store: Arc<Store>,
    workspace: PathBuf,
    provider: Arc<dyn InferenceProvider>,
) -> AgentRuntime {
    let mut providers = ProviderRegistry::default();
    providers.register(provider);
    AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(PolicyTable::with_defaults())),
        memory: Arc::new(SqliteMemory::new(store.clone())),
        audit: Arc::new(pai_audit::AuditLog::new(store.clone())),
        max_steps: 8,
        step_timeout: std::time::Duration::from_secs(10),
        device: DeviceId::new(),
        persistence: Some(Persistence {
            conversations: Arc::new(ConversationStore::new(store.clone())),
            runs: Arc::new(RunStore::new(store)),
        }),
        documents: None,
        email: None,
        gitlab: None,
        vision: None,
        notify: None,
        apps: None,
        audio_gen: None,
        media_dir: None,
        allowed_roots: vec![workspace],
    }
}

fn def(provider: &str) -> AgentDefinition {
    AgentDefinition {
        name: "assistant".into(),
        description: "test".into(),
        purpose: "test".into(),
        tools: vec![],
        memory_scopes: vec![MemoryScope::Semantic],
        provider: provider.into(),
        model: None,
    }
}

fn events() -> (Arc<Mutex<Vec<String>>>, impl Fn(AgentEvent) + Send + Sync) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    let emit = move |e: AgentEvent| {
        let kind = match e {
            AgentEvent::ToolExecuted { tool, .. } => format!("executed:{tool}"),
            AgentEvent::ToolDenied { tool, .. } => format!("denied:{tool}"),
            AgentEvent::ToolCallRequested { tool, .. } => format!("requested:{tool}"),
            AgentEvent::Done { .. } => "done".into(),
            AgentEvent::TextDelta { .. } => "delta".into(),
            _ => "other".into(),
        };
        l.lock().unwrap().push(kind);
    };
    (log, emit)
}

fn req<'a>(
    def: &'a AgentDefinition,
    input: &str,
    approval: &'a dyn ApprovalHandler,
    emit: &'a (dyn Fn(AgentEvent) + Send + Sync),
) -> RunRequest<'a> {
    RunRequest {
        definition: def,
        history: vec![],
        input: input.into(),
        conversation: None,
        approval,
        cancel: CancelToken::new(),
        emit,
        stream: false,
        resume_from: None,
    }
}

#[tokio::test]
async fn fast_path_writes_code_through_gate() {
    let dir = tmpdir("write");
    let ws = dir.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let provider = Arc::new(Scripted::new(&[
        "Here's the code:\n```python\nprint('hi from test')\n```\nDone.",
    ]));
    let agent = runtime(store, ws.clone(), provider);
    let (log, emit) = events();
    let out = agent
        .run(req(
            &def("scripted"),
            "write a python script named hello.py that prints hi",
            &AutoApprove,
            &emit,
        ))
        .await
        .unwrap();
    // The file landed in the workspace jail with the model's contents.
    assert_eq!(
        std::fs::read_to_string(ws.join("hello.py")).unwrap(),
        "print('hi from test')"
    );
    assert!(out.answer.unwrap().contains("hello.py"));
    assert!(log.lock().unwrap().contains(&"executed:fs.write".into()));
}

#[tokio::test]
async fn fast_path_scaffolds_when_model_returns_prose() {
    let dir = tmpdir("scaffold");
    let ws = dir.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let provider = Arc::new(Scripted::new(&["I cannot produce code for that."]));
    let agent = runtime(store, ws.clone(), provider);
    let (log, emit) = events();
    let out = agent
        .run(req(
            &def("scripted"),
            "write a python script that prints hello",
            &AutoApprove,
            &emit,
        ))
        .await
        .unwrap();
    // Model gave prose → scaffold still lands a runnable file.
    let written = std::fs::read_to_string(ws.join("main.py")).unwrap();
    assert!(written.contains("def main()"));
    assert!(out.answer.unwrap().contains("scaffold"));
    assert!(log.lock().unwrap().contains(&"executed:fs.write".into()));
}

#[tokio::test]
async fn fast_path_denied_when_approval_refused() {
    let dir = tmpdir("denied");
    let ws = dir.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let provider = Arc::new(Scripted::new(&["```python\nprint(1)\n```"]));
    let agent = runtime(store, ws.clone(), provider);
    let (log, emit) = events();
    let out = agent
        .run(req(
            &def("scripted"),
            "write a python script that prints hello",
            &DenyApprovals,
            &emit,
        ))
        .await
        .unwrap();
    assert!(!ws.join("main.py").exists());
    assert!(out.answer.unwrap().contains("Couldn't write"));
    assert!(log.lock().unwrap().contains(&"denied:fs.write".into()));
}

#[tokio::test]
async fn fast_path_remember_without_model_call() {
    let dir = tmpdir("remember");
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let provider = Arc::new(Scripted::new(&[]));
    let calls = provider.clone();
    let agent = runtime(store, dir.join("workspace"), provider);
    let (_log, emit) = events();
    let out = agent
        .run(req(
            &def("scripted"),
            "remember that my birthday is May 5",
            &AutoApprove,
            &emit,
        ))
        .await
        .unwrap();
    assert!(out
        .answer
        .unwrap()
        .contains("Remembered: my birthday is May 5"));
    // Pure-Rust intent — the model was never consulted.
    assert_eq!(calls.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_tool_call_gets_one_correction_round() {
    let dir = tmpdir("retry");
    let store = Arc::new(Store::open(&dir, None).unwrap());
    // First response: truncated near-JSON (has `{` + `"name"` → looks
    // like a failed tool call). Second: a valid tool_call envelope.
    let provider = Arc::new(Scripted::new(&[
        "Let me call the tool: {\"name\": \"calculator.add\", \"arguments\": {\"a\": 2,",
        "{\"type\":\"tool_call\",\"name\":\"calculator.add\",\"arguments\":{\"a\":2,\"b\":3}}",
        "{\"type\":\"final\",\"content\":\"The sum is 5.\"}",
    ]));
    let calls = provider.clone();
    let agent = runtime(store, dir.join("workspace"), provider);
    let (log, emit) = events();
    let out = agent
        .run(req(
            &def("scripted"),
            "what is two plus three",
            &AutoApprove,
            &emit,
        ))
        .await
        .unwrap();
    let log = log.lock().unwrap();
    assert!(
        log.contains(&"executed:calculator.add".into()),
        "malformed output never recovered: {log:?}"
    );
    assert_eq!(
        calls.calls.load(Ordering::SeqCst),
        3,
        "one retry + final answer"
    );
    assert_eq!(out.answer.as_deref(), Some("The sum is 5."));
}

#[tokio::test]
async fn plain_prose_answer_is_not_retried() {
    let dir = tmpdir("prose");
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let provider = Arc::new(Scripted::new(&["Paris is the capital of France."]));
    let calls = provider.clone();
    let agent = runtime(store, dir.join("workspace"), provider);
    let (_log, emit) = events();
    let out = agent
        .run(req(
            &def("scripted"),
            "what is the capital of france",
            &AutoApprove,
            &emit,
        ))
        .await
        .unwrap();
    assert_eq!(
        out.answer.as_deref(),
        Some("Paris is the capital of France.")
    );
    assert_eq!(
        calls.calls.load(Ordering::SeqCst),
        1,
        "prose must not retry"
    );
}

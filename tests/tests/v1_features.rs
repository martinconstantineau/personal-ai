//! V1 feature tests: streaming, memory.forget, conversation persistence +
//! memory scoping, crash-safe run resume, persisted policies.

use pai_agent::{
    AgentDefinition, AgentRuntime, ApprovalHandler, CancelToken, ConversationStore, Persistence,
    RunRequest, RunStore,
};
use pai_core::*;
use pai_inference::EchoProvider;
use pai_memory::{MemoryBackend, MemoryScopeQuery, RecallQuery, SqliteMemory};
use pai_permissions::{Permission, PermissionDecision, PolicyEngine, PolicyTable};
use pai_storage::Store;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct DenyApprovals(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl ApprovalHandler for DenyApprovals {
    async fn decide(&self, _r: &pai_permissions::ApprovalRequest) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        false
    }
}

/// A real user+device+session chain so conversation FK constraints hold.
fn test_session(store: &Arc<Store>) -> SessionId {
    let ids = pai_identity::IdentityStore::new(store.clone());
    let user = ids.create_user("tester").unwrap();
    let dir = std::env::temp_dir().join(format!("pai-test-keys-{}", uuid::Uuid::new_v4()));
    let device = ids
        .register_device(
            user.id,
            "test-device",
            Platform::Linux,
            DeviceCapabilities::default(),
            &dir,
        )
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    ConversationStore::new(store.clone())
        .get_or_create_session(user.id, device.id)
        .unwrap()
}

fn runtime(store: Arc<Store>, table: PolicyTable) -> (AgentRuntime, Arc<SqliteMemory>) {
    let memory = Arc::new(SqliteMemory::new(store.clone()));
    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));
    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    let agent = AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(table)),
        memory: memory.clone(),
        audit,
        max_steps: 8,
        step_timeout: std::time::Duration::from_secs(10),
        device: DeviceId::new(),
        persistence: Some(Persistence {
            conversations: Arc::new(ConversationStore::new(store.clone())),
            runs: Arc::new(RunStore::new(store)),
        }),
        documents: None,
        email: None,
        vision: None,
        notify: None,
        allowed_roots: vec![],
    };
    (agent, memory)
}

fn def() -> AgentDefinition {
    AgentDefinition {
        name: "assistant".into(),
        description: "test".into(),
        purpose: "test".into(),
        tools: vec![],
        memory_scopes: vec![MemoryScope::Semantic],
        provider: "echo".into(),
        model: None,
    }
}

fn collect(
    events: Arc<Mutex<Vec<AgentEventKind>>>,
) -> impl Fn(pai_agent::AgentEvent) + Send + Sync {
    move |e| {
        let kind = match e {
            pai_agent::AgentEvent::RunStarted { .. } => AgentEventKind::RunStarted,
            pai_agent::AgentEvent::Step { .. } => AgentEventKind::Step,
            pai_agent::AgentEvent::ToolCallRequested { .. } => AgentEventKind::ToolCallRequested,
            pai_agent::AgentEvent::ApprovalNeeded { .. } => AgentEventKind::ApprovalNeeded,
            pai_agent::AgentEvent::ToolExecuted { .. } => AgentEventKind::ToolExecuted,
            pai_agent::AgentEvent::ToolDenied { .. } => AgentEventKind::ToolDenied,
            pai_agent::AgentEvent::TextDelta { text } => AgentEventKind::TextDelta(text),
            pai_agent::AgentEvent::Done { .. } => AgentEventKind::Done,
        };
        events.lock().unwrap().push(kind);
    }
}

#[derive(Debug, Clone, PartialEq)]
enum AgentEventKind {
    RunStarted,
    Step,
    ToolCallRequested,
    ApprovalNeeded,
    ToolExecuted,
    ToolDenied,
    TextDelta(String),
    Done,
}

#[tokio::test]
async fn streaming_emits_answer_tokens_incrementally() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, _m) = runtime(store, PolicyTable::with_defaults());
    let events = Arc::new(Mutex::new(Vec::new()));

    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "hello there".into(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &collect(events.clone()),
            stream: true,
            resume_from: None,
        })
        .await
        .unwrap();

    let deltas: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|k| match k {
            AgentEventKind::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    // Echo chunks the final answer into ~3 deltas; concatenated they must
    // equal the final answer exactly.
    assert!(deltas.len() > 1, "expected streamed deltas, got {deltas:?}");
    assert_eq!(deltas.concat(), out.answer.unwrap());
}

#[tokio::test]
async fn streaming_never_leaks_tool_call_json() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, _m) = runtime(store, PolicyTable::with_defaults());
    let events = Arc::new(Mutex::new(Vec::new()));

    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "what is 41 + 1?".into(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &collect(events.clone()),
            stream: true,
            resume_from: None,
        })
        .await
        .unwrap();

    assert!(out.answer.unwrap().contains("42"));
    for k in events.lock().unwrap().iter() {
        if let AgentEventKind::TextDelta(t) = k {
            assert!(
                !t.contains("\"tool_call\"") && !t.contains("calculator.add"),
                "tool-call JSON leaked into a delta: {t}"
            );
        }
    }
}

#[tokio::test]
async fn forget_tool_deletes_and_audits() {
    let store = Arc::new(Store::in_memory().unwrap());
    // Grant MemoryDelete outright for this run (default is AskUser).
    let mut table = PolicyTable::with_defaults();
    table.set(Permission::MemoryDelete, ExecutionPolicy::AlwaysAllow);
    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));
    let (agent, memory) = runtime(store, table);
    let emit = |_| {};

    for text in ["Remember that I like tea", "forget that I like tea"] {
        agent
            .run(RunRequest {
                definition: &def(),
                history: vec![],
                input: text.into(),
                conversation: None,
                approval: &pai_agent::AutoApprove,
                cancel: CancelToken::default(),
                emit: &emit,
                stream: false,
                resume_from: None,
            })
            .await
            .unwrap();
    }

    assert!(memory
        .recall(&RecallQuery {
            text: Some("tea".into()),
            limit: 5,
            ..Default::default()
        })
        .await
        .unwrap()
        .is_empty());
    assert!(audit
        .recent(50)
        .unwrap()
        .iter()
        .any(|e| e.kind == AuditKind::MemoryDeleted));
}

#[tokio::test]
async fn forget_denied_keeps_the_memory() {
    let store = Arc::new(Store::in_memory().unwrap());
    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));
    let (agent, memory) = runtime(store, PolicyTable::with_defaults());
    let emit = |_| {};

    memory
        .put(&pai_memory::user_fact("I like tea", 0.9))
        .await
        .unwrap();

    let asked = Arc::new(AtomicUsize::new(0));
    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "forget that I like tea".into(),
            conversation: None,
            approval: &DenyApprovals(asked.clone()),
            cancel: CancelToken::default(),
            emit: &emit,
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();

    // Default policy is AskUser → the handler was consulted and denied.
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(out.run.state, RunState::Completed);
    assert_eq!(
        memory
            .recall(&RecallQuery {
                text: Some("tea".into()),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        1,
        "denied forget must not delete"
    );
    assert!(audit
        .recent(50)
        .unwrap()
        .iter()
        .any(|e| { e.kind == AuditKind::ApprovalResolved && e.outcome == AuditOutcome::Denied }));
}

#[tokio::test]
async fn conversations_persist_and_scope_memory() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, memory) = runtime(store.clone(), PolicyTable::with_defaults());
    let convs = ConversationStore::new(store.clone());
    let emit = |_| {};

    let session = test_session(&store);
    let shared = convs.create(session, MemoryIsolation::Shared).unwrap();
    let iso = convs.create(session, MemoryIsolation::Isolated).unwrap();

    // Remember something inside the isolated conversation.
    agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "Remember that my secret code is 987".into(),
            conversation: Some(iso.id),
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();

    // The write was tagged to the isolated conversation…
    let items = memory
        .recall(&RecallQuery {
            text: Some("secret code".into()),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].item.conversation, Some(iso.id));

    // …visible inside it…
    assert_eq!(
        memory
            .recall(&RecallQuery {
                text: Some("secret".into()),
                limit: 10,
                memory_scope: MemoryScopeQuery::Scoped(iso.id, false),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    // …invisible to global-only and other isolated scopes.
    for scope in [
        MemoryScopeQuery::GlobalOnly,
        MemoryScopeQuery::Scoped(shared.id, false),
    ] {
        assert!(memory
            .recall(&RecallQuery {
                text: Some("secret".into()),
                limit: 10,
                memory_scope: scope,
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
    }

    // Transcript persisted: user message + tool observation + assistant reply.
    let msgs = convs.messages(iso.id).unwrap();
    assert!(msgs.iter().any(|m| m.role == Role::User));
    assert!(msgs.iter().any(|m| m.role == Role::Assistant));

    // CRUD: rename + list + delete.
    convs.rename(shared.id, "my chat").unwrap();
    let listed = convs.list().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed
            .iter()
            .find(|c| c.id == shared.id)
            .unwrap()
            .title
            .as_deref(),
        Some("my chat")
    );
    convs.delete(shared.id).unwrap();
    assert_eq!(convs.list().unwrap().len(), 1);
    assert!(convs.messages(shared.id).unwrap().is_empty());
}

#[tokio::test]
async fn shared_conversation_writes_global_memories() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, memory) = runtime(store.clone(), PolicyTable::with_defaults());
    let convs = ConversationStore::new(store.clone());
    let emit = |_| {};

    let shared = convs
        .create(test_session(&store), MemoryIsolation::Shared)
        .unwrap();
    agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "Remember that I prefer decaf".into(),
            conversation: Some(shared.id),
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();

    // Written globally → visible from a run with no conversation at all.
    let items = memory
        .recall(&RecallQuery {
            text: Some("decaf".into()),
            limit: 10,
            memory_scope: MemoryScopeQuery::GlobalOnly,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].item.conversation, None);
}

#[tokio::test]
async fn interrupted_run_resumes_from_checkpoint() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, _m) = runtime(store.clone(), PolicyTable::with_defaults());
    let runs = RunStore::new(store.clone());
    let emit = |_| {};

    // Simulate a crash: a run that checkpointed mid-flight and never finished.
    let run = AgentRun {
        id: AgentRunId::new(),
        agent: AgentId::new(),
        conversation: None,
        started_at: now(),
        ended_at: None,
        state: RunState::Running,
    };
    let user_msg = Message {
        id: MessageId::new(),
        conversation: ConversationId::new(),
        role: Role::User,
        created_at: now(),
        content: vec![Content::text("what is 41 + 1?")],
        trust: TrustLevel::User,
    };
    runs.begin(&run, "what is 41 + 1?").unwrap();
    runs.checkpoint(&run, 0, &[user_msg]).unwrap();

    // It shows up as interrupted…
    let interrupted = runs.interrupted().unwrap();
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].id, run.id);

    // …and resumes to a completed calculator answer.
    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: String::new(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: false,
            resume_from: Some(run.id),
        })
        .await
        .unwrap();
    assert_eq!(out.run.state, RunState::Completed);
    assert!(out.answer.unwrap().contains("42"));
    assert!(runs.interrupted().unwrap().is_empty());
}

#[tokio::test]
async fn completed_runs_leave_no_interrupted_rows() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, _m) = runtime(store.clone(), PolicyTable::with_defaults());
    let runs = RunStore::new(store);
    let emit = |_| {};

    agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "hi".into(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();
    assert!(runs.interrupted().unwrap().is_empty());
}

#[test]
fn policy_edits_change_decisions() {
    let engine = PolicyEngine::new(PolicyTable::with_defaults());
    assert_eq!(
        engine.decide(&[Permission::EmailSend]),
        PermissionDecision::AskUser
    );
    engine.set_policy(Permission::EmailSend, ExecutionPolicy::AlwaysAllow);
    assert_eq!(
        engine.decide(&[Permission::EmailSend]),
        PermissionDecision::Allow
    );
    engine.set_policy(Permission::EmailSend, ExecutionPolicy::NeverAllow);
    assert_eq!(
        engine.decide(&[Permission::EmailSend]),
        PermissionDecision::Deny
    );
}

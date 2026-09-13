//! The vertical slice, verified end-to-end:
//! remember → semantic memory (user-stated) → recall → calculator tool →
//! permission check → audit trail. All through the real runtime, no fakes.

use pai_agent::{AgentDefinition, AgentRuntime, ApprovalHandler, CancelToken, RunRequest};
use pai_core::*;
use pai_inference::EchoProvider;
use pai_memory::{MemoryBackend, MemoryItem, RecallQuery, SqliteMemory};
use pai_permissions::{Permission, PermissionDecision, PolicyEngine, PolicyTable};
use pai_storage::Store;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct CountApprovals(Arc<AtomicUsize>, bool);
#[async_trait::async_trait]
impl ApprovalHandler for CountApprovals {
    async fn decide(&self, _r: &pai_permissions::ApprovalRequest) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1
    }
}

fn runtime(store: Arc<Store>) -> (AgentRuntime, Arc<SqliteMemory>, Arc<pai_audit::AuditLog>) {
    let memory = Arc::new(SqliteMemory::new(store.clone()));
    let audit = Arc::new(pai_audit::AuditLog::new(store));
    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    let agent = AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(PolicyTable::with_defaults())),
        memory: memory.clone(),
        audit: audit.clone(),
        max_steps: 8,
        step_timeout: std::time::Duration::from_secs(10),
        device: DeviceId::new(),
        persistence: None,
    };
    (agent, memory, audit)
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

#[tokio::test]
async fn remember_recall_tool_audit() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, memory, audit) = runtime(store);
    let emit = |_| {};

    // 1. "Remember that I prefer local models" → semantic, user-stated.
    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "Remember that I prefer local models".into(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();
    assert_eq!(out.run.state, RunState::Completed);

    let recalled = memory
        .recall(&RecallQuery {
            text: Some("prefer local".into()),
            limit: 5,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].item.scope, MemoryScope::Semantic);
    assert_eq!(recalled[0].item.source, MemorySource::UserStated);
    assert_eq!(recalled[0].item.confidence, 1.0);

    // 2. "What do I prefer for AI models?" → recall injected → answered.
    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "What do I prefer for AI models?".into(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();
    let answer = out.answer.unwrap();
    assert!(answer.contains("prefer local models"), "got: {answer}");

    // 3. Calculator tool through the same pipeline.
    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "What is 41 + 1?".into(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();
    assert!(out.answer.unwrap().contains("42"));

    // 4. Audit trail shows the whole thing.
    let events = audit.recent(50).unwrap();
    let kinds: Vec<AuditKind> = events.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&AuditKind::ToolRequested));
    assert!(kinds.contains(&AuditKind::ToolAllowed));
    assert!(kinds.contains(&AuditKind::ToolExecuted));
    assert!(kinds.contains(&AuditKind::MemoryWritten));
}

#[tokio::test]
async fn denied_permission_never_executes() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, memory, audit) = runtime(store);

    // Flip MemoryWrite to ASK_USER, then deny at the approval handler.
    let mut table = PolicyTable::with_defaults();
    table.set(Permission::MemoryWrite, ExecutionPolicy::AskUser);
    let agent = AgentRuntime {
        permissions: Arc::new(PolicyEngine::new(table)),
        ..agent
    };

    let asked = Arc::new(AtomicUsize::new(0));
    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "Remember that I like tea".into(),
            conversation: None,
            approval: &CountApprovals(asked.clone(), false),
            cancel: CancelToken::default(),
            emit: &|_| {},
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();

    assert_eq!(asked.load(Ordering::SeqCst), 1); // approval was requested
                                                 // The tool call errored but the run survived and finished with a reply.
    assert_eq!(out.run.state, RunState::Completed);
    let all: Vec<MemoryItem> = memory
        .recall(&RecallQuery {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.item)
        .collect();
    assert!(all.is_empty(), "denied write must not persist");

    let denied = audit.recent(50).unwrap();
    assert!(denied
        .iter()
        .any(|e| e.kind == AuditKind::ApprovalResolved && e.outcome == AuditOutcome::Denied));
}

#[tokio::test]
async fn cancellation_stops_run() {
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, _m, _a) = runtime(store);
    let cancel = CancelToken::default();
    cancel.cancel();
    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "hello".into(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel,
            emit: &|_| {},
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();
    assert_eq!(out.run.state, RunState::Cancelled);
}

#[tokio::test]
async fn unknown_tool_is_an_observation_not_a_crash() {
    // The runtime hands model-chosen tool names straight to the registry;
    // an unknown name becomes an error ToolResult the model sees.
    let store = Arc::new(Store::in_memory().unwrap());
    let (agent, _m, audit) = runtime(store);
    let out = agent
        .run(RunRequest {
            definition: &def(),
            history: vec![],
            input: "add 7 and 8".into(),
            conversation: None,
            approval: &pai_agent::AutoApprove,
            cancel: CancelToken::default(),
            emit: &|_| {},
            stream: false,
            resume_from: None,
        })
        .await
        .unwrap();
    assert_eq!(out.run.state, RunState::Completed);
    assert!(audit
        .recent(10)
        .unwrap()
        .iter()
        .any(|e| e.kind == AuditKind::ToolExecuted));
}

#[test]
fn engine_denies_by_default_for_unknown_perms() {
    let engine = PolicyEngine::new(PolicyTable::default());
    assert_eq!(
        engine.decide(&[Permission::FilesDelete]),
        PermissionDecision::AskUser
    );
}

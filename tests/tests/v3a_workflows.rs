//! V3a tests: declarative multi-step workflows — prompt steps through a
//! narrowed agent, deterministic tool steps, `{{input}}`/`{{steps.x}}`
//! templates, crash-safe resume via the persisted cursor, permission-
//! bounded tools (empty allowlist grants nothing), and `wf/` sync
//! roundtrips + tombstones.

use pai_agent::{
    AgentEvent, AgentRuntime, ApprovalHandler, AutoApprove, ConversationStore, Persistence,
    RunStore,
};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_inference::{EchoProvider, ProviderRegistry};
use pai_memory::{MemoryBackend, RecallQuery, SqliteMemory};
use pai_permissions::{ApprovalRequest, Permission, PolicyEngine, PolicyTable};
use pai_storage::Store;
use pai_sync::{crypto, engine, pair, FolderTransport};
use pai_workflows::{store as ws, WorkflowDefinition, WorkflowRunner};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v3a-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn store(tag: &str) -> Arc<Store> {
    Arc::new(Store::open(&tmpdir(tag), None).unwrap())
}

fn runtime(store: Arc<Store>, table: PolicyTable) -> AgentRuntime {
    let mut providers = ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(table)),
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
        vision: None,
        notify: None,
        allowed_roots: vec![],
    }
}

fn runner<'a>(agent: &'a AgentRuntime, store: &'a Arc<Store>) -> WorkflowRunner<'a> {
    WorkflowRunner {
        agent,
        store,
        provider: "echo".into(),
        model: Some("echo-0".into()),
    }
}

fn no_emit(_: AgentEvent) {}

fn two_step() -> WorkflowDefinition {
    serde_json::from_value(serde_json::json!({
        "name": "summarise",
        "description": "test wf",
        "steps": [
            {"id": "first", "kind": "prompt", "prompt": "say {{input}}"},
            {"id": "second", "kind": "prompt", "prompt": "prev was: {{steps.first.output}}"},
        ],
    }))
    .unwrap()
}

async fn recalls(mem: &SqliteMemory, text: &str) -> bool {
    !mem.recall(&RecallQuery {
        text: Some(text.into()),
        limit: 5,
        ..Default::default()
    })
    .await
    .unwrap()
    .is_empty()
}

#[test]
fn definition_validation() {
    // empty steps
    assert!(
        serde_json::from_value::<WorkflowDefinition>(serde_json::json!({
            "name": "x", "steps": []
        }))
        .unwrap()
        .validate()
        .is_err()
    );
    // duplicate step ids
    assert!(
        serde_json::from_value::<WorkflowDefinition>(serde_json::json!({
            "name": "x",
            "steps": [
                {"id": "a", "kind": "prompt", "prompt": "p"},
                {"id": "a", "kind": "prompt", "prompt": "q"},
            ]
        }))
        .unwrap()
        .validate()
        .is_err()
    );
    // tool step missing tool name
    assert!(
        serde_json::from_value::<WorkflowDefinition>(serde_json::json!({
            "name": "x",
            "steps": [{"id": "a", "kind": "tool", "args": {}}]
        }))
        .is_err()
    );
    // tool step outside the allowlist
    assert!(
        serde_json::from_value::<WorkflowDefinition>(serde_json::json!({
            "name": "x",
            "tools": ["memory.remember"],
            "steps": [{"id": "a", "kind": "tool", "tool": "fs.write", "args": {}}]
        }))
        .unwrap()
        .validate()
        .is_err()
    );
    // forward step reference rejected (only prior steps may be referenced)
    assert!(
        serde_json::from_value::<WorkflowDefinition>(serde_json::json!({
            "name": "x",
            "steps": [
                {"id": "a", "kind": "prompt", "prompt": "{{steps.b.output}}"},
                {"id": "b", "kind": "prompt", "prompt": "hi"},
            ]
        }))
        .unwrap()
        .validate()
        .is_err()
    );
    // valid definition
    two_step().validate().unwrap();
}

#[tokio::test]
async fn prompt_steps_chain_outputs() {
    let s = store("chain");
    let agent = runtime(s.clone(), PolicyTable::with_defaults());
    let def = two_step();
    let wid = ws::create_workflow(&s, &def, SyncScope::DeviceLocal).unwrap();
    let wf = ws::get_workflow(&s, &wid).unwrap().unwrap();
    let out = runner(&agent, &s)
        .run(&wf, "hello", &AutoApprove, &no_emit)
        .await
        .unwrap();
    assert_eq!(out.run.status, "done");
    // EchoProvider's final answer echoes the rendered prompt — the second
    // step saw the first step's output via {{steps.first.output}}.
    let second = out.run.outputs["second"].as_str().unwrap();
    let first = out.run.outputs["first"].as_str().unwrap();
    assert!(first.contains("say hello"), "first: {first}");
    assert!(second.contains("prev was:"), "second: {second}");
    // the persisted run row carries the accumulated outputs
    let rec = ws::get_run(&s, &out.run.id).unwrap().unwrap();
    assert_eq!(rec.status, "done");
    assert!(!rec.outputs.is_empty());
}

#[tokio::test]
async fn tool_step_executes_deterministically() {
    let s = store("tool");
    let agent = runtime(s.clone(), PolicyTable::with_defaults());
    let def: WorkflowDefinition = serde_json::from_value(serde_json::json!({
        "name": "remember",
        "tools": ["memory.remember"],
        "steps": [{
            "id": "save", "kind": "tool",
            "tool": "memory.remember",
            "args": {"content": "user likes {{input}}", "memory_type": "semantic"}
        }],
    }))
    .unwrap();
    let wid = ws::create_workflow(&s, &def, SyncScope::DeviceLocal).unwrap();
    let wf = ws::get_workflow(&s, &wid).unwrap().unwrap();
    let out = runner(&agent, &s)
        .run(&wf, "tea", &AutoApprove, &no_emit)
        .await
        .unwrap();
    assert_eq!(out.run.status, "done");
    // the memory actually landed — args rendered {{input}} first
    let mem = SqliteMemory::new(s.clone());
    assert!(
        recalls(&mem, "tea").await,
        "tool step should have stored 'tea'"
    );
}

#[tokio::test]
async fn empty_tool_allowlist_denies_everything() {
    let s = store("deny");
    let agent = runtime(s.clone(), PolicyTable::with_defaults());
    // prompt step only — EchoProvider turns "remember that X" into a
    // memory.remember call; with no allowlisted tools it must be denied.
    let def: WorkflowDefinition = serde_json::from_value(serde_json::json!({
        "name": "sneaky",
        "steps": [{"id": "a", "kind": "prompt", "prompt": "remember that secret-plan"}],
    }))
    .unwrap();
    let wid = ws::create_workflow(&s, &def, SyncScope::DeviceLocal).unwrap();
    let wf = ws::get_workflow(&s, &wid).unwrap().unwrap();
    let out = runner(&agent, &s)
        .run(&wf, "", &AutoApprove, &no_emit)
        .await
        .unwrap();
    assert_eq!(out.run.status, "done"); // denial is an observation; run completes
    let mem = SqliteMemory::new(s.clone());
    assert!(
        !recalls(&mem, "secret-plan").await,
        "empty allowlist must grant no tools"
    );
}

struct DenyForget;
#[async_trait::async_trait]
impl ApprovalHandler for DenyForget {
    async fn decide(&self, req: &ApprovalRequest) -> bool {
        req.tool != "memory.forget"
    }
}

#[tokio::test]
async fn approval_denial_blocks_tool_step() {
    let s = store("approval");
    let mut table = PolicyTable::with_defaults();
    table.set(Permission::MemoryDelete, ExecutionPolicy::AskUser);
    let agent = runtime(s.clone(), table);
    let def: WorkflowDefinition = serde_json::from_value(serde_json::json!({
        "name": "forgetful",
        "tools": ["memory.forget"],
        "steps": [{
            "id": "wipe", "kind": "tool",
            "tool": "memory.forget",
            "args": {"query": "anything"}
        }],
    }))
    .unwrap();
    let wid = ws::create_workflow(&s, &def, SyncScope::DeviceLocal).unwrap();
    let wf = ws::get_workflow(&s, &wid).unwrap().unwrap();
    let res = runner(&agent, &s).run(&wf, "", &DenyForget, &no_emit).await;
    let err = match res {
        Err(e) => e,
        Ok(_) => panic!("denied tool step should fail the run"),
    };
    assert!(err.to_string().contains("denied"), "err: {err}");
    // the run row records the failure — not a silent drop
    let runs = ws::list_runs(&s, &wf.id).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "failed");
    assert!(runs[0].error.as_deref().unwrap_or("").contains("denied"));
}

#[tokio::test]
async fn crash_resume_continues_from_cursor() {
    let s = store("resume");
    let agent = runtime(s.clone(), PolicyTable::with_defaults());
    let def = two_step();
    let wid = ws::create_workflow(&s, &def, SyncScope::DeviceLocal).unwrap();
    let wf = ws::get_workflow(&s, &wid).unwrap().unwrap();

    // Simulate a crash after step "first": run record at step_index=1
    // with the first step's output recorded.
    let rid = ws::start_run(&s, &wf.id, "hi").unwrap();
    let mut outputs = serde_json::Map::new();
    outputs.insert(
        "first".into(),
        serde_json::Value::String("canned-output".into()),
    );
    ws::save_progress(&s, &rid, 1, &outputs).unwrap();

    let out = runner(&agent, &s)
        .resume(&rid, &AutoApprove, &no_emit)
        .await
        .unwrap();
    assert_eq!(out.run.status, "done");
    // "first" kept its pre-crash output; "second" ran and saw it.
    assert_eq!(out.run.outputs["first"].as_str().unwrap(), "canned-output");
    assert!(
        out.run.outputs["second"]
            .as_str()
            .unwrap()
            .contains("canned-output"),
        "second step should render the pre-crash output"
    );
}

#[tokio::test]
async fn finished_run_does_not_resume() {
    let s = store("rerun");
    let agent = runtime(s.clone(), PolicyTable::with_defaults());
    let def = two_step();
    let wid = ws::create_workflow(&s, &def, SyncScope::DeviceLocal).unwrap();
    let wf = ws::get_workflow(&s, &wid).unwrap().unwrap();
    let out = runner(&agent, &s)
        .run(&wf, "x", &AutoApprove, &no_emit)
        .await
        .unwrap();
    assert_eq!(out.run.status, "done");
    // a done run refuses to re-drive
    assert!(runner(&agent, &s)
        .resume(&out.run.id, &AutoApprove, &no_emit)
        .await
        .is_err());
}

// ---------------------------------------------------------------- sync

struct Dev {
    dir: PathBuf,
    store: Arc<Store>,
    ids: IdentityStore,
    key_dir: PathBuf,
    device: Device,
}

fn dev(tag: &str) -> Dev {
    let dir = tmpdir(tag);
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let ids = IdentityStore::new(store.clone());
    let key_dir = dir.join("keys");
    let user = ids.create_user("u").unwrap();
    let device = ids
        .register_device(
            user.id,
            tag,
            Platform::Linux,
            DeviceCapabilities::default(),
            &key_dir,
        )
        .unwrap();
    Dev {
        dir,
        store,
        ids,
        key_dir,
        device,
    }
}

fn pair_devices(offerer: &Dev, acceptor: &Dev) {
    let agree_a = crypto::agreement_key(offerer.device.id, &offerer.dir).unwrap();
    let agree_b = crypto::agreement_key(acceptor.device.id, &acceptor.dir).unwrap();
    let offer =
        pair::make_offer(&offerer.device, &agree_a, &offerer.ids, &offerer.key_dir).unwrap();
    let accept = pair::accept_offer(
        &acceptor.store,
        &offer,
        &acceptor.device,
        &agree_b,
        &acceptor.ids,
        &acceptor.key_dir,
        &acceptor.dir,
    )
    .unwrap();
    pair::complete_pairing(&offerer.store, &accept, &agree_a, &offerer.dir).unwrap();
}

fn eng(d: &Dev, shared: &Path) -> engine::SyncEngine<FolderTransport> {
    engine::folder_engine(shared, d.store.clone(), d.device.id, &d.dir).unwrap()
}

#[tokio::test]
async fn workflow_definition_syncs_and_tombstones() {
    let a = dev("a");
    let b = dev("b");
    pair_devices(&a, &b);
    let shared = tmpdir("shared");

    let def = two_step();
    let wid = ws::create_workflow(&a.store, &def, SyncScope::Synchronized).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    let got = ws::get_workflow(&b.store, "summarise").unwrap().unwrap();
    assert_eq!(got.id, wid);
    assert_eq!(got.definition.steps.len(), 2);

    // update propagates via LWW
    let mut def2 = def.clone();
    def2.description = "v2".into();
    ws::update_workflow(&a.store, &wid, &def2).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert_eq!(
        ws::get_workflow(&b.store, "summarise")
            .unwrap()
            .unwrap()
            .definition
            .description,
        "v2"
    );

    // tombstone propagates
    ws::remove_workflow(&a.store, "summarise").unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(ws::get_workflow(&b.store, "summarise").unwrap().is_none());
}

#[tokio::test]
async fn device_local_workflow_does_not_sync() {
    let a = dev("la");
    let b = dev("lb");
    pair_devices(&a, &b);
    let shared = tmpdir("shared");
    ws::create_workflow(&a.store, &two_step(), SyncScope::DeviceLocal).unwrap();
    eng(&a, &shared).push().await.unwrap();
    eng(&b, &shared).pull().await.unwrap();
    assert!(ws::get_workflow(&b.store, "summarise").unwrap().is_none());
}

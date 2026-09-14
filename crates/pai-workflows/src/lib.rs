//! Declarative, permission-bounded multi-step workflows.
//!
//! A `WorkflowDefinition` is an ordered step list plus a tool allowlist.
//! Steps are either `prompt` (run through the agent runtime with the
//! workflow's *narrowed* tool set — a workflow can never reach a tool it
//! didn't declare) or `tool` (a deterministic direct call that still
//! passes the full policy + approval gate via `AgentRuntime::invoke_tool`).
//!
//! Templates: `{{input}}` is the run's input text; `{{steps.<id>.output}}`
//! is a previous step's output. Substitution happens per step, in order.
//!
//! Definitions persist in the `workflows` table and sync like tasks
//! (`wf/<id>` objects). `workflow_runs` rows carry a step cursor +
//! per-step outputs — a crash mid-run resumes from the last completed
//! step, not from scratch. Runs are device-local and never sync.

use pai_agent::{AgentDefinition, AgentRuntime, ApprovalHandler, CancelToken, RunRequest};
use pai_core::*;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub mod store {
    use super::*;

    /// A persisted workflow definition row.
    #[derive(Debug, Clone)]
    pub struct WorkflowRow {
        pub id: String,
        pub name: String,
        pub definition: WorkflowDefinition,
        pub sync_scope: String,
        pub created_at: Timestamp,
        pub updated_at: Timestamp,
    }

    /// A device-local run record — the resume cursor.
    #[derive(Debug, Clone)]
    pub struct WorkflowRunRow {
        pub id: String,
        pub workflow_id: String,
        pub input: String,
        pub status: String, // running | done | failed | cancelled
        pub step_index: usize,
        pub outputs: serde_json::Map<String, serde_json::Value>,
        pub error: Option<String>,
        pub started_at: Timestamp,
        pub finished_at: Option<Timestamp>,
    }

    fn wf_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowRow> {
        let def_json: String = r.get(2)?;
        Ok(WorkflowRow {
            id: r.get(0)?,
            name: r.get(1)?,
            definition: serde_json::from_str(&def_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
            sync_scope: r.get(3)?,
            created_at: pai_storage::parse_ts(&r.get::<_, String>(4)?),
            updated_at: pai_storage::parse_ts(&r.get::<_, String>(5)?),
        })
    }

    fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowRunRow> {
        let outputs_json: String = r.get(5)?;
        Ok(WorkflowRunRow {
            id: r.get(0)?,
            workflow_id: r.get(1)?,
            input: r.get(2)?,
            status: r.get(3)?,
            step_index: r.get::<_, i64>(4)? as usize,
            outputs: serde_json::from_str::<serde_json::Value>(&outputs_json)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default(),
            error: r.get(6)?,
            started_at: pai_storage::parse_ts(&r.get::<_, String>(7)?),
            finished_at: r
                .get::<_, Option<String>>(8)?
                .map(|s| pai_storage::parse_ts(&s)),
        })
    }

    const WF_COLS: &str = "id, name, definition_json, sync_scope, created_at, updated_at";
    const RUN_COLS: &str = "id, workflow_id, input, status, step_index,
        outputs_json, error, started_at, finished_at";

    /// Persist a new definition. `sync_scope` decides whether it roams.
    pub fn create_workflow(
        store: &Arc<pai_storage::Store>,
        def: &WorkflowDefinition,
        scope: SyncScope,
    ) -> Result<String> {
        def.validate()?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = now();
        store.with_conn(|c| {
            c.execute(
                &format!("INSERT INTO workflows({WF_COLS}, deleted) VALUES(?1,?2,?3,?4,?5,?5,0)"),
                params![
                    id,
                    def.name,
                    serde_json::to_string(def).unwrap_or_else(|_| "{}".into()),
                    serde_json::to_string(&scope)
                        .unwrap_or_else(|_| "\"device_local\"".into())
                        .trim_matches('"')
                        .to_string(),
                    pai_storage::ts(&now),
                ],
            )
        })?;
        Ok(id)
    }

    /// Update a definition in place (bumps `updated_at` for sync LWW).
    pub fn update_workflow(
        store: &Arc<pai_storage::Store>,
        id: &str,
        def: &WorkflowDefinition,
    ) -> Result<bool> {
        def.validate()?;
        let n = store.with_conn(|c| {
            c.execute(
                "UPDATE workflows SET name=?2, definition_json=?3, updated_at=?4
                 WHERE id=?1 AND deleted=0",
                params![
                    id,
                    def.name,
                    serde_json::to_string(def).unwrap_or_else(|_| "{}".into()),
                    pai_storage::ts(&now()),
                ],
            )
        })?;
        Ok(n > 0)
    }

    /// Fetch by id, or by exact name when `id_or_name` isn't a row id.
    pub fn get_workflow(
        store: &Arc<pai_storage::Store>,
        id_or_name: &str,
    ) -> Result<Option<WorkflowRow>> {
        store.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {WF_COLS} FROM workflows
                 WHERE deleted=0 AND (id=?1 OR name=?1) ORDER BY updated_at DESC"
            ))?;
            let mut rows = stmt.query_map(params![id_or_name], wf_row)?;
            rows.next().transpose()
        })
    }

    pub fn list_workflows(
        store: &Arc<pai_storage::Store>,
        include_deleted: bool,
    ) -> Result<Vec<WorkflowRow>> {
        store.with_conn(|c| {
            let sql = format!(
                "SELECT {WF_COLS} FROM workflows {} ORDER BY name",
                if include_deleted {
                    ""
                } else {
                    "WHERE deleted=0"
                }
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], wf_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Soft-delete — the tombstone propagates to peers.
    pub fn remove_workflow(store: &Arc<pai_storage::Store>, id_or_name: &str) -> Result<bool> {
        let n = store.with_conn(|c| {
            c.execute(
                "UPDATE workflows SET deleted=1, updated_at=?2
                 WHERE deleted=0 AND (id=?1 OR name=?1)",
                params![id_or_name, pai_storage::ts(&now())],
            )
        })?;
        Ok(n > 0)
    }

    /// Flip the sync scope (opt a definition in/out of roaming).
    pub fn set_sync_scope(
        store: &Arc<pai_storage::Store>,
        id_or_name: &str,
        scope: SyncScope,
    ) -> Result<bool> {
        let n = store.with_conn(|c| {
            c.execute(
                "UPDATE workflows SET sync_scope=?2, updated_at=?3
                 WHERE deleted=0 AND (id=?1 OR name=?1)",
                params![
                    id_or_name,
                    serde_json::to_string(&scope)
                        .unwrap_or_else(|_| "\"device_local\"".into())
                        .trim_matches('"')
                        .to_string(),
                    pai_storage::ts(&now()),
                ],
            )
        })?;
        Ok(n > 0)
    }

    // -- run records ----------------------------------------------------

    /// Open a run row (`running`, cursor at step 0).
    pub fn start_run(
        store: &Arc<pai_storage::Store>,
        workflow_id: &str,
        input: &str,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        store.with_conn(|c| {
            c.execute(
                &format!(
                    "INSERT INTO workflow_runs({RUN_COLS})
                     VALUES(?1,?2,?3,'running',0,'{{}}',NULL,?4,NULL)"
                ),
                params![id, workflow_id, input, pai_storage::ts(&now())],
            )
        })?;
        Ok(id)
    }

    /// Persist the cursor + accumulated outputs after each step — this
    /// is the crash-resume record.
    pub fn save_progress(
        store: &Arc<pai_storage::Store>,
        run_id: &str,
        step_index: usize,
        outputs: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<()> {
        store.with_conn(|c| {
            c.execute(
                "UPDATE workflow_runs SET step_index=?2, outputs_json=?3
                 WHERE id=?1",
                params![
                    run_id,
                    step_index as i64,
                    serde_json::to_string(outputs).unwrap_or_else(|_| "{}".into()),
                ],
            )
        })?;
        Ok(())
    }

    /// Close a run: `done`/`failed`/`cancelled` (+error text).
    pub fn finish_run(
        store: &Arc<pai_storage::Store>,
        run_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        store.with_conn(|c| {
            c.execute(
                "UPDATE workflow_runs SET status=?2, error=?3, finished_at=?4
                 WHERE id=?1",
                params![run_id, status, error, pai_storage::ts(&now())],
            )
        })?;
        Ok(())
    }

    pub fn get_run(
        store: &Arc<pai_storage::Store>,
        run_id: &str,
    ) -> Result<Option<WorkflowRunRow>> {
        store.with_conn(|c| {
            let mut stmt =
                c.prepare(&format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id=?1"))?;
            let mut rows = stmt.query_map(params![run_id], run_row)?;
            rows.next().transpose()
        })
    }

    /// Runs for a workflow, newest first.
    pub fn list_runs(
        store: &Arc<pai_storage::Store>,
        workflow_id: &str,
    ) -> Result<Vec<WorkflowRunRow>> {
        store.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {RUN_COLS} FROM workflow_runs
                 WHERE workflow_id=?1 ORDER BY started_at DESC"
            ))?;
            let rows = stmt.query_map(params![workflow_id], run_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }
}

// ---------------------------------------------------------------------------
// Definition
// ---------------------------------------------------------------------------

/// One step in a workflow. `id` is the handle later steps reference via
/// `{{steps.<id>.output}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowStep {
    /// Model step: render `prompt`, run it through the agent with the
    /// workflow's tool allowlist, capture the final answer.
    Prompt { id: String, prompt: String },
    /// Deterministic step: call `tool` directly with rendered `args` —
    /// still passes policy + approval gates (never a bypass).
    Tool {
        id: String,
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
}

impl WorkflowStep {
    pub fn id(&self) -> &str {
        match self {
            WorkflowStep::Prompt { id, .. } | WorkflowStep::Tool { id, .. } => id,
        }
    }
}

/// A declarative multi-step definition — the unit that persists + syncs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Tool allowlist bounding every step. Empty = NO tools (a workflow
    /// must opt in explicitly — the opposite default of AgentDefinition,
    /// where empty means all).
    #[serde(default)]
    pub tools: Vec<String>,
    pub steps: Vec<WorkflowStep>,
}

impl WorkflowDefinition {
    /// Structural checks — called on save AND on load-before-run so a
    /// synced-in definition can't smuggle an out-of-allowlist tool call.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::InvalidInput("workflow needs a name".into()));
        }
        if self.steps.is_empty() {
            return Err(Error::InvalidInput(
                "workflow needs at least one step".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for (i, s) in self.steps.iter().enumerate() {
            let id = s.id();
            if id.is_empty() || !seen.insert(id.to_string()) {
                return Err(Error::InvalidInput(format!(
                    "duplicate or empty step id {id:?}"
                )));
            }
            if let WorkflowStep::Tool { tool, .. } = s {
                if !self.tools.iter().any(|t| t == tool) {
                    return Err(Error::PermissionDenied(format!(
                        "step {id} calls {tool} — not in the workflow's tool allowlist"
                    )));
                }
            }
            // `{{steps.<X>.output}}` may only reference earlier steps —
            // a forward ref would render literally, silently corrupting
            // the step's input. Refs to non-step ids stay literal by design.
            for ref_id in step_refs(s) {
                if let Some(j) = self.steps.iter().position(|t| t.id() == ref_id) {
                    if j >= i {
                        return Err(Error::InvalidInput(format!(
                            "step {id} references {ref_id} before it has run"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// `{{steps.<id>.output}}` references inside one step's templates.
fn step_refs(step: &WorkflowStep) -> Vec<String> {
    let mut out = Vec::new();
    let mut scan = |text: &str| {
        let mut rest = text;
        while let Some(i) = rest.find("{{steps.") {
            rest = &rest[i + 8..];
            if let Some(j) = rest.find(".output}}") {
                out.push(rest[..j].to_string());
            }
        }
    };
    match step {
        WorkflowStep::Prompt { prompt, .. } => scan(prompt),
        WorkflowStep::Tool { args, .. } => scan(&args.to_string()),
    }
    out
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

/// Render `{{input}}` and `{{steps.<id>.output}}` placeholders. Unknown
/// references are left literal (visible in output, never silent).
pub fn render(
    template: &str,
    input: &str,
    outputs: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut out = template.replace("{{input}}", input);
    // Longest-key-first isn't needed — ids can't contain '.' and the
    // placeholder is exact — but substitute each known step id.
    for (id, v) in outputs {
        let placeholder = format!("{{{{steps.{id}.output}}}}");
        if out.contains(&placeholder) {
            let text = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            out = out.replace(&placeholder, &text);
        }
    }
    out
}

/// Render every string leaf of a JSON template (tool args).
pub fn render_args(
    template: &serde_json::Value,
    input: &str,
    outputs: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    match template {
        serde_json::Value::String(s) => serde_json::Value::String(render(s, input, outputs)),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(|v| render_args(v, input, outputs)).collect())
        }
        serde_json::Value::Object(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), render_args(v, input, outputs)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Executes workflow definitions against the shared agent runtime.
/// Cheap to construct per run — borrows the runtime + store.
pub struct WorkflowRunner<'a> {
    pub agent: &'a AgentRuntime,
    pub store: &'a Arc<pai_storage::Store>,
    /// Provider/model for `prompt` steps (the caller's default agent
    /// config — workflows don't pin models).
    pub provider: String,
    pub model: Option<String>,
}

pub struct WorkflowOutcome {
    pub run: store::WorkflowRunRow,
    /// Last step's output, if any — the workflow's "answer".
    pub output: Option<serde_json::Value>,
}

impl WorkflowRunner<'_> {
    /// Start a fresh run of `wf` (id or name resolves at call time).
    pub async fn run(
        &self,
        wf: &store::WorkflowRow,
        input: &str,
        approval: &dyn ApprovalHandler,
        emit: &(dyn Fn(pai_agent::AgentEvent) + Send + Sync),
    ) -> Result<WorkflowOutcome> {
        let run_id = store::start_run(self.store, &wf.id, input)?;
        self.drive(wf, &run_id, input, approval, emit).await
    }

    /// Resume a `running` run from its saved cursor.
    pub async fn resume(
        &self,
        run_id: &str,
        approval: &dyn ApprovalHandler,
        emit: &(dyn Fn(pai_agent::AgentEvent) + Send + Sync),
    ) -> Result<WorkflowOutcome> {
        let row = store::get_run(self.store, run_id)?
            .ok_or_else(|| Error::NotFound(format!("workflow run {run_id}")))?;
        if row.status != "running" {
            return Err(Error::InvalidInput(format!(
                "run {run_id} is {} — only running runs resume",
                row.status
            )));
        }
        let wf = store::get_workflow(self.store, &row.workflow_id)?
            .ok_or_else(|| Error::NotFound(format!("workflow {}", row.workflow_id)))?;
        self.drive(&wf, run_id, &row.input, approval, emit).await
    }

    async fn drive(
        &self,
        wf: &store::WorkflowRow,
        run_id: &str,
        input: &str,
        approval: &dyn ApprovalHandler,
        emit: &(dyn Fn(pai_agent::AgentEvent) + Send + Sync),
    ) -> Result<WorkflowOutcome> {
        let def = &wf.definition;
        def.validate()?;
        let mut row = store::get_run(self.store, run_id)?
            .ok_or_else(|| Error::NotFound(format!("workflow run {run_id}")))?;
        let mut outputs = row.outputs.clone();
        // One synthetic AgentRun per workflow run — audit attribution for
        // direct tool steps; prompt steps get their own agent_runs rows.
        let audit_run = AgentRun {
            id: AgentRunId::new(),
            agent: AgentId::new(),
            conversation: None,
            started_at: now(),
            ended_at: None,
            state: RunState::Running,
        };

        let mut last: Option<serde_json::Value> = None;
        let result = async {
            for i in row.step_index..def.steps.len() {
                let step = &def.steps[i];
                match step {
                    WorkflowStep::Prompt { id, prompt } => {
                        let text = render(prompt, input, &outputs);
                        // The workflow's allowlist bounds the step — empty
                        // means NO tools (AgentDefinition's empty = all, so
                        // a sentinel keeps the bound honest).
                        let tools = if def.tools.is_empty() {
                            vec!["__no_tools__".to_string()]
                        } else {
                            def.tools.clone()
                        };
                        let step_def = AgentDefinition {
                            name: format!("wf:{}:{id}", def.name),
                            description: def.description.clone(),
                            purpose: format!("workflow {} step {id}", def.name),
                            tools,
                            memory_scopes: vec![],
                            provider: self.provider.clone(),
                            model: self.model.clone(),
                        };
                        let out = self
                            .agent
                            .run(RunRequest {
                                definition: &step_def,
                                history: vec![],
                                input: text,
                                conversation: None,
                                approval,
                                cancel: CancelToken::new(),
                                emit,
                                stream: false,
                                resume_from: None,
                            })
                            .await?;
                        let answer = out.answer.ok_or_else(|| {
                            Error::Provider(format!("step {id} produced no answer"))
                        })?;
                        outputs.insert(id.clone(), serde_json::Value::String(answer.clone()));
                        last = Some(serde_json::Value::String(answer));
                    }
                    WorkflowStep::Tool { id, tool, args } => {
                        let rendered = render_args(args, input, &outputs);
                        let out = self
                            .agent
                            .invoke_tool(&audit_run, tool, rendered, approval, emit)
                            .await?;
                        outputs.insert(
                            id.clone(),
                            serde_json::json!({
                                "value": out.value,
                                "summary": out.summary,
                            }),
                        );
                        last = Some(out.value);
                    }
                }
                // Cursor advances only after the step completed — a crash
                // resumes *this* step, never skips it.
                store::save_progress(self.store, run_id, i + 1, &outputs)?;
            }
            Ok::<(), Error>(())
        }
        .await;

        match result {
            Ok(()) => {
                store::finish_run(self.store, run_id, "done", None)?;
            }
            Err(e) => {
                store::finish_run(self.store, run_id, "failed", Some(&e.to_string()))?;
                return Err(e);
            }
        }
        row = store::get_run(self.store, run_id)?.unwrap_or(row);
        Ok(WorkflowOutcome {
            run: row,
            output: last,
        })
    }
}

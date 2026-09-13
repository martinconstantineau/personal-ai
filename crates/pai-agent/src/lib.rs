//! Agent runtime — a proper execution boundary, not a fragile loop.
//!
//! One run = a bounded state machine:
//!
//! ```text
//! input → context assembly (history + memory recall)
//!       → model call → parse action
//!       → tool_call: permission check → (approval?) → execute → audit → observe → loop
//!       → final:     emit answer → audit → done
//! ```
//!
//! Hard guarantees:
//! - `max_steps` bounds the loop — a confused model cannot run forever.
//! - Every tool execution passes `PolicyEngine::decide` first.
//! - Every step lands in the audit log, including denials.
//! - Cancellation via `tokio_util`-style token; timeout via config.
//! - Model output is data; tool results are tagged untrusted.

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use pai_core::*;
use pai_inference::{AIRequest, GenerateResponse, ModelAction, StreamEvent, ToolSpec};
use pai_memory::{MemoryBackend, MemoryScopeQuery, RecallQuery};
use pai_permissions::{PermissionDecision, PolicyEngine};
use pai_tools::{validate_args, ToolContext, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub mod store;
use store::run_state_name;
pub use store::{ConversationStore, RunStore};

/// What the outside world observes while a run executes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    RunStarted {
        run: AgentRunId,
    },
    Step {
        index: u32,
    },
    ToolCallRequested {
        call: ToolCallId,
        tool: String,
        risk: String,
    },
    /// The runtime needs a human decision; run is suspended until resolved.
    ApprovalNeeded {
        call: ToolCallId,
        tool: String,
        summary: String,
        permissions: Vec<String>,
    },
    ToolExecuted {
        call: ToolCallId,
        tool: String,
        summary: String,
    },
    ToolDenied {
        call: ToolCallId,
        tool: String,
    },
    TextDelta {
        text: String,
    },
    Done {
        run: AgentRunId,
        state: RunState,
        answer: Option<String>,
    },
}

pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

/// Decides whether an approval request is granted. The app supplies this —
/// CLI prompts, the GUI shows a dialog, tests auto-decide.
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn decide(&self, req: &pai_permissions::ApprovalRequest) -> bool;
}

/// Auto-approves everything — tests only; apps must supply a real handler.
pub struct AutoApprove;
#[async_trait]
impl ApprovalHandler for AutoApprove {
    async fn decide(&self, _req: &pai_permissions::ApprovalRequest) -> bool {
        true
    }
}

/// Cancellation handle shared with a run.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Everything a single run needs — bundled so `run` stays extensible
/// without signature churn.
pub struct RunRequest<'a> {
    pub definition: &'a AgentDefinition,
    /// Conversation so far; the new user message is appended internally.
    pub history: Vec<Message>,
    /// The user's new input text. Ignored when `resume_from` is set — the
    /// original input is restored from the checkpoint.
    pub input: String,
    pub conversation: Option<ConversationId>,
    /// Who decides AskUser-gated actions.
    pub approval: &'a dyn ApprovalHandler,
    pub cancel: CancelToken,
    /// Sink for [`AgentEvent`]s — UI updates, log taps, test probes.
    pub emit: &'a (dyn Fn(AgentEvent) + Send + Sync),
    /// Stream model tokens through [`AgentEvent::TextDelta`] as they arrive
    /// (only the `final` answer's content is streamed — tool-call JSON is
    /// never tokenized to the UI).
    pub stream: bool,
    /// Resume a previously checkpointed run (requires `persistence`).
    pub resume_from: Option<AgentRunId>,
}

/// Static definition of an agent (see [`pai_core::Agent`] for the persisted form).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub purpose: String,
    /// Allowed tool names; empty = no tools.
    pub tools: Vec<String>,
    pub memory_scopes: Vec<MemoryScope>,
    /// Provider id + model slug to run on.
    pub provider: String,
    pub model: Option<String>,
}

/// Optional persistence wiring. Without it the runtime is fully functional
/// but keeps no transcripts, checkpoints, or resumable runs.
#[derive(Clone)]
pub struct Persistence {
    pub conversations: Arc<ConversationStore>,
    pub runs: Arc<RunStore>,
}

/// The runtime. Cheap to construct; holds `Arc`s into shared subsystems.
pub struct AgentRuntime {
    pub providers: pai_inference::ProviderRegistry,
    pub tools: ToolRegistry,
    pub permissions: Arc<PolicyEngine>,
    pub memory: Arc<dyn MemoryBackend>,
    pub audit: Arc<pai_audit::AuditLog>,
    pub max_steps: u32,
    pub step_timeout: Duration,
    /// Who this runtime acts for.
    pub device: DeviceId,
    pub persistence: Option<Persistence>,
}

pub struct RunOutcome {
    pub run: AgentRun,
    pub answer: Option<String>,
}

impl AgentRuntime {
    /// Memory visibility for a run: shared conversations see global + own
    /// memories; isolated ones see only their own; runs without a
    /// conversation see global memory only.
    fn memory_query(&self, conversation: Option<ConversationId>) -> MemoryScopeQuery {
        match conversation {
            None => MemoryScopeQuery::GlobalOnly,
            Some(c) => {
                let isolated = self
                    .persistence
                    .as_ref()
                    .and_then(|p| p.conversations.get(c).ok())
                    .map(|conv| conv.memory == MemoryIsolation::Isolated)
                    .unwrap_or(false);
                MemoryScopeQuery::Scoped(c, !isolated)
            }
        }
    }

    /// Assemble the model request: system protocol + recalled memories +
    /// prior messages + the new user input.
    async fn build_request(
        &self,
        def: &AgentDefinition,
        history: &[Message],
        user_text: &str,
        mem_query: MemoryScopeQuery,
    ) -> Vec<Message> {
        let mut messages: Vec<Message> = Vec::new();

        // Memory recall — inject as system-attributed context, clearly marked.
        if !user_text.is_empty() {
            if let Ok(recalled) = self
                .memory
                .recall(&RecallQuery {
                    text: Some(user_text.to_string()),
                    scopes: def.memory_scopes.clone(),
                    limit: 8,
                    memory_scope: mem_query,
                    ..Default::default()
                })
                .await
            {
                if !recalled.is_empty() {
                    let text = recalled
                        .iter()
                        .map(|s| {
                            let tag = match s.item.source {
                                MemorySource::UserStated => "memory",
                                _ => "memory (AI-inferred, unverified)",
                            };
                            format!("[{tag}] {}", s.item.content)
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    messages.push(Message {
                        id: MessageId::new(),
                        conversation: ConversationId::new(),
                        role: Role::System,
                        created_at: now(),
                        content: vec![Content::text(format!("Relevant memories:\n{text}"))],
                        trust: TrustLevel::Generated,
                    });
                }
            }
        }

        messages.extend(history.iter().cloned());
        messages
    }

    fn tool_specs(&self, def: &AgentDefinition) -> Vec<ToolSpec> {
        self.tools
            .descriptors()
            .into_iter()
            .filter(|d| def.tools.is_empty() || def.tools.contains(&d.name))
            .map(|d| ToolSpec {
                name: d.name,
                description: d.description,
                input_schema: d.input_schema,
            })
            .collect()
    }

    /// Run `req` to completion (or cancellation/timeout/step-limit),
    /// pushing [`AgentEvent`]s to `req.emit`.
    ///
    /// With `persistence` wired, every step is checkpointed into
    /// `agent_runs` and every transcript message lands in `messages` — a
    /// crash mid-run leaves a resumable checkpoint (see [`RunStore`]).
    pub async fn run(&self, req: RunRequest<'_>) -> Result<RunOutcome> {
        let RunRequest {
            definition: def,
            history,
            input: user_text,
            conversation,
            approval,
            cancel,
            emit,
            stream,
            resume_from,
        } = req;

        let provider = self
            .providers
            .get(&def.provider)
            .ok_or_else(|| Error::Provider(format!("no provider '{}'", def.provider)))?;

        // Fresh run vs. resume-from-checkpoint: both end up as
        // (run, messages, first step to execute).
        let (run, mut messages, mut step) = match resume_from {
            Some(rid) => {
                let runs = self
                    .persistence
                    .as_ref()
                    .map(|p| p.runs.clone())
                    .ok_or_else(|| Error::InvalidInput("resume requires persistence".into()))?;
                let (mut r, step, msgs, _input) = runs.load(rid)?;
                r.state = RunState::Running;
                emit(AgentEvent::RunStarted { run: r.id });
                self.audit(
                    &r,
                    AuditKind::RunStarted,
                    AuditOutcome::Ok,
                    None,
                    serde_json::json!({"provider": def.provider, "model": def.model, "resumed": true}),
                );
                (r, msgs, step)
            }
            None => {
                let run = AgentRun {
                    id: AgentRunId::new(),
                    agent: AgentId::new(),
                    conversation,
                    started_at: now(),
                    ended_at: None,
                    state: RunState::Running,
                };
                emit(AgentEvent::RunStarted { run: run.id });
                self.audit(
                    &run,
                    AuditKind::RunStarted,
                    AuditOutcome::Ok,
                    None,
                    serde_json::json!({"provider": def.provider, "model": def.model}),
                );
                if let Some(p) = &self.persistence {
                    let _ = p.runs.begin(&run, &user_text);
                }
                let conv = conversation.unwrap_or_default();
                let mut messages = self
                    .build_request(def, &history, &user_text, self.memory_query(conversation))
                    .await;
                let user_msg = Message {
                    id: MessageId::new(),
                    conversation: conv,
                    role: Role::User,
                    created_at: now(),
                    content: vec![Content::text(&user_text)],
                    trust: TrustLevel::User,
                };
                if let (Some(p), true) = (&self.persistence, conversation.is_some()) {
                    let _ = p.conversations.append(&user_msg);
                    let _ = p.conversations.set_title_if_empty(conv, &user_text);
                }
                messages.push(user_msg);
                (run, messages, 0)
            }
        };
        let conv = run.conversation.unwrap_or_default();
        // Isolated conversations tag their memory writes to themselves.
        let mem_scope = match self.memory_query(run.conversation) {
            MemoryScopeQuery::Scoped(c, false) => Some(c),
            _ => None,
        };
        let tools = self.tool_specs(def);
        let mut answer: Option<String> = None;

        while step < self.max_steps {
            if cancel.cancelled() {
                return Ok(self.finish_run(run, answer, RunState::Cancelled, emit));
            }
            emit(AgentEvent::Step { index: step });
            // Checkpoint *before* the model call: on crash we resume having
            // seen exactly these messages.
            if let Some(p) = &self.persistence {
                let _ = p.runs.checkpoint(&run, step, &messages);
            }

            let req = AIRequest {
                messages: messages.clone(),
                tools: tools.clone(),
                model: def.model.clone(),
                require_structured: true,
                ..Default::default()
            };
            let (resp, streamed) = match self.generate_step(&provider, req, stream, emit).await {
                Ok(x) => x,
                Err(e) => {
                    self.audit_error(&run, &e);
                    let state = if matches!(e, Error::Timeout) {
                        RunState::TimedOut
                    } else {
                        RunState::Failed
                    };
                    self.finish_run(run, None, state, emit);
                    return Err(e);
                }
            };
            self.audit(
                &run,
                AuditKind::ModelResponded,
                AuditOutcome::Ok,
                None,
                serde_json::json!({
                    "step": step,
                    "action": match &resp.action {
                        Some(pai_inference::ModelAction::ToolCall { name, .. }) => {
                            serde_json::json!({"type": "tool_call", "tool": name})
                        }
                        Some(pai_inference::ModelAction::Final { .. }) => {
                            serde_json::json!({"type": "final"})
                        }
                        None => serde_json::json!({"type": "unparsed"}),
                    },
                }),
            );

            match self.dispatch(def, &run, conv, resp).await? {
                Dispatch::Final { text } => {
                    answer = Some(text.clone());
                    if let (Some(p), true) = (&self.persistence, run.conversation.is_some()) {
                        let _ = p.conversations.append(&Message {
                            id: MessageId::new(),
                            conversation: conv,
                            role: Role::Assistant,
                            created_at: now(),
                            content: vec![Content::text(&text)],
                            trust: TrustLevel::Generated,
                        });
                    }
                    // When streaming, the answer was already emitted token by
                    // token; only emit the full text as a fallback.
                    if !streamed {
                        emit(AgentEvent::TextDelta { text: text.clone() });
                    }
                    return Ok(self.finish_run(run, answer, RunState::Completed, emit));
                }
                Dispatch::ToolRequested {
                    call_id,
                    name,
                    arguments,
                } => {
                    let ToolStep::Observation(msg) = self
                        .execute_tool(ToolInvocation {
                            run: &run,
                            call_id: &call_id,
                            name: &name,
                            arguments,
                            approval,
                            emit,
                            conv,
                            memory_scope: mem_scope,
                        })
                        .await;
                    if let (Some(p), true) = (&self.persistence, run.conversation.is_some()) {
                        let _ = p.conversations.append(&msg);
                    }
                    messages.push(msg);
                }
            }
            step += 1;
        }

        // Ran out of steps — fail closed.
        self.finish_run(run, answer, RunState::Failed, emit);
        Err(Error::Other("max agent steps exceeded".into()))
    }

    /// Terminal bookkeeping for a run: state, audit, `Done` event, and the
    /// checkpoint store's finish marker.
    fn finish_run(
        &self,
        mut run: AgentRun,
        answer: Option<String>,
        state: RunState,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> RunOutcome {
        run.state = state;
        run.ended_at = Some(now());
        let outcome = match state {
            RunState::Cancelled => AuditOutcome::Cancelled,
            RunState::Completed => AuditOutcome::Ok,
            _ => AuditOutcome::Error,
        };
        self.audit(
            &run,
            AuditKind::RunFinished,
            outcome,
            None,
            serde_json::json!({"state": run_state_name(state)}),
        );
        if let Some(p) = &self.persistence {
            let _ = p.runs.finish(&run);
        }
        emit(AgentEvent::Done {
            run: run.id,
            state,
            answer: answer.clone(),
        });
        RunOutcome { run, answer }
    }

    /// One model call. When `stream` is set, token deltas are decoded
    /// through [`FinalStream`] so only the *final answer's* content reaches
    /// the UI as `TextDelta` — structured tool-call JSON never leaks
    /// token-by-token. Returns the response plus whether the answer was
    /// streamed (so the caller doesn't emit the text twice).
    async fn generate_step(
        &self,
        provider: &Arc<dyn pai_inference::InferenceProvider>,
        req: AIRequest,
        stream: bool,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<(GenerateResponse, bool)> {
        if !stream {
            return match tokio::time::timeout(self.step_timeout, provider.generate(&req)).await {
                Ok(Ok(r)) => Ok((r, false)),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(Error::Timeout),
            };
        }
        let mut s = provider.stream(req);
        let mut extractor = FinalStream::new();
        let mut streamed = false;
        let mut result: Option<GenerateResponse> = None;
        let consume = async {
            while let Some(item) = s.next().await {
                match item {
                    Ok(StreamEvent::Delta(d)) => {
                        if let Some(t) = extractor.push(&d) {
                            if !t.is_empty() {
                                emit(AgentEvent::TextDelta { text: t });
                                streamed = true;
                            }
                        }
                    }
                    Ok(StreamEvent::Done(r)) => {
                        result = Some(r);
                        break;
                    }
                    Ok(StreamEvent::Error(e)) => return Err(Error::Provider(e)),
                    Err(e) => return Err(e),
                }
            }
            result.ok_or_else(|| Error::Provider("stream ended without Done".into()))
        };
        match tokio::time::timeout(self.step_timeout, consume).await {
            Ok(Ok(r)) => Ok((r, streamed)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::Timeout),
        }
    }

    async fn dispatch(
        &self,
        _def: &AgentDefinition,
        _run: &AgentRun,
        _conv: ConversationId,
        resp: GenerateResponse,
    ) -> Result<Dispatch> {
        let action = resp
            .action
            .or_else(|| pai_inference::parse_action(&resp.text))
            .unwrap_or(ModelAction::Final {
                content: resp.text.clone(),
            });
        Ok(match action {
            ModelAction::Final { content } => Dispatch::Final { text: content },
            ModelAction::ToolCall { name, arguments } => Dispatch::ToolRequested {
                call_id: ToolCallId::new(),
                name,
                arguments,
            },
        })
    }

    /// Permission check → maybe approval → execute → audit → observation msg.
    async fn execute_tool(&self, t: ToolInvocation<'_>) -> ToolStep {
        let ToolInvocation {
            run,
            call_id,
            name,
            arguments,
            approval,
            emit,
            conv,
            memory_scope,
        } = t;
        let tool = match self.tools.get(name) {
            Some(t) => t,
            None => {
                self.audit(
                    run,
                    AuditKind::ToolDenied,
                    AuditOutcome::Denied,
                    Some(name),
                    serde_json::json!({"reason": "unknown tool"}),
                );
                return ToolStep::Observation(tool_result_msg(
                    conv,
                    *call_id,
                    name,
                    serde_json::json!({"error": "unknown tool"}),
                    true,
                ));
            }
        };
        let desc = tool.descriptor();
        emit(AgentEvent::ToolCallRequested {
            call: *call_id,
            tool: name.to_string(),
            risk: format!("{:?}", desc.risk),
        });
        self.audit(
            run,
            AuditKind::ToolRequested,
            AuditOutcome::Ok,
            Some(name),
            serde_json::json!({"arguments": arguments}),
        );

        // Schema validation before anything else.
        if let Err(e) = validate_args(&desc.input_schema, &arguments) {
            self.audit(
                run,
                AuditKind::ToolDenied,
                AuditOutcome::Error,
                Some(name),
                serde_json::json!({"reason": e.to_string()}),
            );
            return ToolStep::Observation(tool_result_msg(
                conv,
                *call_id,
                name,
                serde_json::json!({"error": e.to_string()}),
                true,
            ));
        }

        // Permission gate.
        match self.permissions.decide(&desc.required_permissions) {
            PermissionDecision::Deny => {
                self.audit(
                    run,
                    AuditKind::ToolDenied,
                    AuditOutcome::Denied,
                    Some(name),
                    serde_json::json!({}),
                );
                emit(AgentEvent::ToolDenied {
                    call: *call_id,
                    tool: name.into(),
                });
                return ToolStep::Observation(tool_result_msg(
                    conv,
                    *call_id,
                    name,
                    serde_json::json!({"error": "permission denied by policy"}),
                    true,
                ));
            }
            PermissionDecision::AskUser => {
                let req = pai_permissions::ApprovalRequest {
                    id: *call_id,
                    tool: name.into(),
                    permissions: desc.required_permissions.clone(),
                    summary: format!("{name} {}", arguments),
                    risk: desc.risk,
                };
                self.audit(
                    run,
                    AuditKind::ApprovalRequested,
                    AuditOutcome::Ok,
                    Some(name),
                    serde_json::json!({}),
                );
                emit(AgentEvent::ApprovalNeeded {
                    call: *call_id,
                    tool: name.into(),
                    summary: req.summary.clone(),
                    permissions: desc
                        .required_permissions
                        .iter()
                        .map(|p| format!("{p:?}"))
                        .collect(),
                });
                let granted = approval.decide(&req).await;
                self.audit(
                    run,
                    AuditKind::ApprovalResolved,
                    if granted {
                        AuditOutcome::Ok
                    } else {
                        AuditOutcome::Denied
                    },
                    Some(name),
                    serde_json::json!({"granted": granted}),
                );
                if !granted {
                    emit(AgentEvent::ToolDenied {
                        call: *call_id,
                        tool: name.into(),
                    });
                    return ToolStep::Observation(tool_result_msg(
                        conv,
                        *call_id,
                        name,
                        serde_json::json!({"error": "user denied approval"}),
                        true,
                    ));
                }
                self.audit(
                    run,
                    AuditKind::ToolAllowed,
                    AuditOutcome::Ok,
                    Some(name),
                    serde_json::json!({}),
                );
            }
            PermissionDecision::Allow => {
                self.audit(
                    run,
                    AuditKind::ToolAllowed,
                    AuditOutcome::Ok,
                    Some(name),
                    serde_json::json!({}),
                );
            }
        }

        // Execute inside the tool context — narrow capability surface.
        let ctx = ToolContext {
            run: run.id,
            device: self.device,
            memory: Some(self.memory.as_ref()),
            memory_scope,
        };
        match tool.execute(arguments, &ctx).await {
            Ok(out) => {
                self.audit(
                    run,
                    AuditKind::ToolExecuted,
                    AuditOutcome::Ok,
                    Some(name),
                    serde_json::json!({"summary": out.summary}),
                );
                if name == "memory.remember" {
                    self.audit(
                        run,
                        AuditKind::MemoryWritten,
                        AuditOutcome::Ok,
                        Some(name),
                        out.value.clone(),
                    );
                } else if name == "memory.forget" {
                    self.audit(
                        run,
                        AuditKind::MemoryDeleted,
                        AuditOutcome::Ok,
                        Some(name),
                        out.value.clone(),
                    );
                }
                emit(AgentEvent::ToolExecuted {
                    call: *call_id,
                    tool: name.into(),
                    summary: out.summary,
                });
                ToolStep::Observation(tool_result_msg(conv, *call_id, name, out.value, false))
            }
            Err(e) => {
                self.audit(
                    run,
                    AuditKind::ToolExecuted,
                    AuditOutcome::Error,
                    Some(name),
                    serde_json::json!({"error": e.to_string()}),
                );
                ToolStep::Observation(tool_result_msg(
                    conv,
                    *call_id,
                    name,
                    serde_json::json!({"error": e.to_string()}),
                    true,
                ))
            }
        }
    }

    fn audit(
        &self,
        run: &AgentRun,
        kind: AuditKind,
        outcome: AuditOutcome,
        tool: Option<&str>,
        detail: serde_json::Value,
    ) {
        let mut e = pai_audit::event(kind, outcome);
        e.device = Some(self.device);
        e.agent = Some(run.agent);
        e.run = Some(run.id);
        e.conversation = run.conversation;
        e.tool = tool.map(String::from);
        e.detail = detail;
        if let Err(err) = self.audit.record(&e) {
            tracing::warn!(%err, "audit write failed");
        }
    }

    fn audit_error(&self, run: &AgentRun, e: &Error) {
        self.audit(
            run,
            AuditKind::ToolExecuted,
            AuditOutcome::Error,
            None,
            serde_json::json!({"provider_error": e.to_string()}),
        );
    }
}

enum Dispatch {
    Final {
        text: String,
    },
    ToolRequested {
        call_id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
}

enum ToolStep {
    Observation(Message),
}

struct ToolInvocation<'a> {
    run: &'a AgentRun,
    call_id: &'a ToolCallId,
    name: &'a str,
    arguments: serde_json::Value,
    approval: &'a dyn ApprovalHandler,
    emit: &'a (dyn Fn(AgentEvent) + Send + Sync),
    /// Conversation the observation message belongs to.
    conv: ConversationId,
    /// Isolated-conversation tag for memory writes inside the tool.
    memory_scope: Option<ConversationId>,
}

fn tool_result_msg(
    conv: ConversationId,
    call: ToolCallId,
    tool: &str,
    output: serde_json::Value,
    is_error: bool,
) -> Message {
    Message {
        id: MessageId::new(),
        conversation: conv,
        role: Role::Tool,
        created_at: now(),
        // Tool output is untrusted data — never instructions.
        trust: TrustLevel::Untrusted,
        content: vec![Content::ToolResult {
            call,
            tool: tool.into(),
            output,
            is_error,
        }],
    }
}

// ---------------------------------------------------------------------------
// Streaming final-answer extraction
// ---------------------------------------------------------------------------

/// Incremental extractor for the structured-output protocol. Fed with raw
/// model text as it streams; emits only the decoded `content` of a
/// `{"type":"final",...}` action (JSON escapes handled), or plain text when
/// the model didn't emit protocol JSON at all.
///
/// - `{"type":"tool_call",...}` → suppressed (never streams JSON to the UI)
/// - `{"type":"final","content":"..."}` → streams the decoded string
/// - no `{` early in the output → raw passthrough
#[derive(Default)]
struct FinalStream {
    buf: String,
    pos: usize,
    state: StreamParse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum StreamParse {
    /// Still looking for the protocol object.
    #[default]
    Scanning,
    /// Inside `content`'s string value.
    InContent,
    /// Just saw a backslash inside `content`.
    Escaped,
    /// Collecting \uXXXX (remaining nibbles, accumulated code point).
    Unicode(u8, u32),
    /// Content string closed.
    Done,
    /// Not a streamable shape (tool call / unparseable) — emit nothing.
    Suppressed,
    /// Model produced plain text, not protocol JSON — stream raw.
    Raw,
}

impl FinalStream {
    fn new() -> Self {
        Self::default()
    }

    /// Feed a raw token chunk; returns decoded answer text to emit, if any.
    fn push(&mut self, chunk: &str) -> Option<String> {
        self.buf.push_str(chunk);
        let mut out = String::new();
        loop {
            match self.state {
                StreamParse::Scanning => {
                    if !self.scan() {
                        return (!out.is_empty()).then_some(out);
                    }
                }
                StreamParse::Raw => {
                    out.push_str(&self.buf[self.pos..]);
                    self.pos = self.buf.len();
                    return Some(out);
                }
                StreamParse::InContent => match self.buf[self.pos..].chars().next() {
                    None => return (!out.is_empty()).then_some(out),
                    Some('"') => {
                        self.pos += 1;
                        self.state = StreamParse::Done;
                    }
                    Some('\\') => {
                        self.pos += 1;
                        self.state = StreamParse::Escaped;
                    }
                    Some(c) => {
                        out.push(c);
                        self.pos += c.len_utf8();
                    }
                },
                StreamParse::Escaped => match self.buf[self.pos..].chars().next() {
                    None => return (!out.is_empty()).then_some(out),
                    Some(c) => {
                        self.pos += c.len_utf8();
                        match c {
                            '"' => out.push('"'),
                            '\\' => out.push('\\'),
                            '/' => out.push('/'),
                            'b' => out.push('\u{0008}'),
                            'f' => out.push('\u{000C}'),
                            'n' => out.push('\n'),
                            'r' => out.push('\r'),
                            't' => out.push('\t'),
                            'u' => {
                                self.state = StreamParse::Unicode(4, 0);
                                continue;
                            }
                            other => out.push(other),
                        }
                        self.state = StreamParse::InContent;
                    }
                },
                StreamParse::Unicode(rem, acc) => match self.buf[self.pos..].chars().next() {
                    None => return (!out.is_empty()).then_some(out),
                    Some(c) => {
                        match c.to_digit(16) {
                            Some(d) => {
                                self.pos += c.len_utf8();
                                let acc = acc * 16 + d;
                                if rem == 1 {
                                    if let Some(ch) = char::from_u32(acc) {
                                        out.push(ch);
                                    }
                                    self.state = StreamParse::InContent;
                                } else {
                                    self.state = StreamParse::Unicode(rem - 1, acc);
                                }
                            }
                            // Malformed escape — skip it, keep streaming.
                            None => {
                                self.pos += c.len_utf8();
                                self.state = StreamParse::InContent;
                            }
                        }
                    }
                },
                StreamParse::Done | StreamParse::Suppressed => {
                    return (!out.is_empty()).then_some(out)
                }
            }
        }
    }

    /// Advance the Scanning state: locate the protocol object, read its
    /// `"type"`, and position `pos` inside `content`'s opening quote.
    /// Returns false when more input is needed (or the state moved to
    /// Raw/Suppressed, in which case the main loop takes over).
    fn scan(&mut self) -> bool {
        // Plain-text fast path: no '{' within the first 64 chars.
        if self.pos == 0 {
            match self.buf.find('{') {
                Some(i) => self.pos = i,
                None => {
                    if self.buf.len() >= 64 {
                        self.state = StreamParse::Raw;
                    }
                    return self.state != StreamParse::Scanning;
                }
            }
        }
        let rest = &self.buf[self.pos..];
        match find_json_string_value(rest, "type") {
            Some((value, end)) if value == "final" => {
                // Position just inside `content`'s opening quote.
                match find_key_open_quote(&rest[end..], "content") {
                    Some(open) => {
                        self.pos += end + open;
                        self.state = StreamParse::InContent;
                    }
                    None if self.buf.len() > 4096 => self.state = StreamParse::Suppressed,
                    None => return false,
                }
            }
            Some(_) => self.state = StreamParse::Suppressed,
            None => {
                if self.buf.len() > 4096 {
                    self.state = StreamParse::Suppressed;
                } else {
                    return false;
                }
            }
        }
        true
    }
}

/// Find `"key"` followed by `:` then a `"` — returns byte offset of the
/// position just after that opening quote (i.e. inside the string value).
/// Returns None when the pattern isn't complete yet.
fn find_key_open_quote(s: &str, key: &str) -> Option<usize> {
    let pat = format!("\"{key}\"");
    let kstart = s.find(&pat)?;
    let after = &s[kstart + pat.len()..];
    let colon = after.find(':')?;
    let q = after[colon + 1..].find('"')?;
    Some(kstart + pat.len() + colon + 1 + q + 1)
}

/// Find `"key": "<value>"` — returns (value, offset just past the closing
/// quote). Used for the `"type"` discriminant (values are simple strings).
fn find_json_string_value(s: &str, key: &str) -> Option<(String, usize)> {
    let open = find_key_open_quote(s, key)?;
    let rest = &s[open..];
    let close = rest.find('"')?;
    Some((rest[..close].to_string(), open + close + 1))
}

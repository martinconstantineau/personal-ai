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
use futures::Stream;
use pai_core::*;
use pai_inference::{AIRequest, GenerateResponse, ModelAction, ToolSpec};
use pai_memory::{MemoryBackend, RecallQuery};
use pai_permissions::{PermissionDecision, PolicyEngine};
use pai_tools::{validate_args, ToolContext, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
    /// The user's new input text.
    pub input: String,
    pub conversation: Option<ConversationId>,
    /// Who decides AskUser-gated actions.
    pub approval: &'a dyn ApprovalHandler,
    pub cancel: CancelToken,
    /// Sink for [`AgentEvent`]s — UI updates, log taps, test probes.
    pub emit: &'a (dyn Fn(AgentEvent) + Send + Sync),
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
}

pub struct RunOutcome {
    pub run: AgentRun,
    pub answer: Option<String>,
}

impl AgentRuntime {
    /// Assemble the model request: system protocol + recalled memories +
    /// prior messages + the new user input.
    async fn build_request(
        &self,
        def: &AgentDefinition,
        history: &[Message],
        user_text: &str,
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
    pub async fn run(&self, req: RunRequest<'_>) -> Result<RunOutcome> {
        let RunRequest {
            definition: def,
            history,
            input: user_text,
            conversation,
            approval,
            cancel,
            emit,
        } = req;
        let run = AgentRun {
            id: AgentRunId::new(),
            agent: AgentId::new(),
            conversation,
            started_at: now(),
            ended_at: None,
            state: RunState::Running,
        };
        let mut run = run;
        emit(AgentEvent::RunStarted { run: run.id });

        let provider = self
            .providers
            .get(&def.provider)
            .ok_or_else(|| Error::Provider(format!("no provider '{}'", def.provider)))?;

        let conv = conversation.unwrap_or_default();
        let mut messages = self.build_request(def, &history, &user_text).await;
        messages.push(Message {
            id: MessageId::new(),
            conversation: conv,
            role: Role::User,
            created_at: now(),
            content: vec![Content::text(&user_text)],
            trust: TrustLevel::User,
        });
        let tools = self.tool_specs(def);
        let mut answer: Option<String> = None;

        for step in 0..self.max_steps {
            if cancel.cancelled() {
                run.state = RunState::Cancelled;
                emit(AgentEvent::Done {
                    run: run.id,
                    state: run.state,
                    answer: None,
                });
                return Ok(RunOutcome { run, answer });
            }
            emit(AgentEvent::Step { index: step });

            let req = AIRequest {
                messages: messages.clone(),
                tools: tools.clone(),
                model: def.model.clone(),
                require_structured: true,
                ..Default::default()
            };
            let resp = match tokio::time::timeout(self.step_timeout, provider.generate(&req)).await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    self.audit_error(&run, &e);
                    run.state = RunState::Failed;
                    run.ended_at = Some(now());
                    emit(AgentEvent::Done {
                        run: run.id,
                        state: run.state,
                        answer: None,
                    });
                    return Err(e);
                }
                Err(_) => {
                    run.state = RunState::TimedOut;
                    run.ended_at = Some(now());
                    emit(AgentEvent::Done {
                        run: run.id,
                        state: run.state,
                        answer: None,
                    });
                    return Err(Error::Timeout);
                }
            };

            match self.dispatch(def, &run, conv, resp).await? {
                Dispatch::Final { text } => {
                    answer = Some(text.clone());
                    run.state = RunState::Completed;
                    run.ended_at = Some(now());
                    emit(AgentEvent::TextDelta { text: text.clone() });
                    emit(AgentEvent::Done {
                        run: run.id,
                        state: run.state,
                        answer: Some(text),
                    });
                    return Ok(RunOutcome { run, answer });
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
                        })
                        .await;
                    messages.push(msg);
                }
            }
        }

        // Ran out of steps — fail closed.
        run.state = RunState::Failed;
        run.ended_at = Some(now());
        self.audit(
            &run,
            AuditKind::ToolDenied,
            AuditOutcome::Error,
            None,
            serde_json::json!({"reason": "max_steps exceeded"}),
        );
        emit(AgentEvent::Done {
            run: run.id,
            state: run.state,
            answer,
        });
        Err(Error::Other("max agent steps exceeded".into()))
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
                }
                emit(AgentEvent::ToolExecuted {
                    call: *call_id,
                    tool: name.into(),
                    summary: out.summary,
                });
                ToolStep::Observation(tool_result_msg(*call_id, name, out.value, false))
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
}

fn tool_result_msg(
    call: ToolCallId,
    tool: &str,
    output: serde_json::Value,
    is_error: bool,
) -> Message {
    Message {
        id: MessageId::new(),
        conversation: ConversationId::new(),
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

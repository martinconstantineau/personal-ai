//! Persistence for conversations and agent runs.
//!
//! - [`ConversationStore`] owns the `sessions`/`conversations`/`messages`
//!   tables: history survives restarts, and each conversation carries a
//!   [`MemoryIsolation`] mode that scopes what the agent may recall.
//! - [`RunStore`] checkpoints a run's message state per step into
//!   `agent_runs`. A crashed or interrupted run keeps `ended_at IS NULL` and
//!   can be resumed exactly where it left off.

use pai_core::*;
use pai_storage::{parse_ts, ts, Store};
use rusqlite::params;
use std::sync::Arc;

fn parse_id(s: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(s).unwrap_or_else(|_| uuid::Uuid::nil())
}

pub(crate) fn run_state_name(s: RunState) -> &'static str {
    match s {
        RunState::Running => "running",
        RunState::AwaitingApproval => "awaiting_approval",
        RunState::Completed => "completed",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
        RunState::TimedOut => "timed_out",
    }
}

fn run_state_from(s: &str) -> RunState {
    match s {
        "awaiting_approval" => RunState::AwaitingApproval,
        "completed" => RunState::Completed,
        "failed" => RunState::Failed,
        "cancelled" => RunState::Cancelled,
        "timed_out" => RunState::TimedOut,
        _ => RunState::Running,
    }
}

fn memory_scope_name(m: MemoryIsolation) -> &'static str {
    match m {
        MemoryIsolation::Shared => "shared",
        MemoryIsolation::Isolated => "isolated",
    }
}

fn memory_scope_from(s: &str) -> MemoryIsolation {
    match s {
        "isolated" => MemoryIsolation::Isolated,
        _ => MemoryIsolation::Shared,
    }
}

// ---------------------------------------------------------------------------
// Conversations + messages + sessions
// ---------------------------------------------------------------------------

pub struct ConversationStore {
    store: Arc<Store>,
}

impl ConversationStore {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Reuse the user's open session on this device, or start a new one.
    pub fn get_or_create_session(&self, user: UserId, device: DeviceId) -> Result<SessionId> {
        let existing = self.store.with_conn(|c| {
            c.query_row(
                "SELECT id FROM sessions WHERE user_id=?1 AND device_id=?2
                 AND ended_at IS NULL ORDER BY started_at DESC LIMIT 1",
                params![user.to_string(), device.to_string()],
                |r| r.get::<_, String>(0),
            )
        });
        if let Ok(id) = existing {
            return Ok(SessionId(parse_id(&id)));
        }
        let s = SessionId::new();
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO sessions(id, user_id, device_id, started_at)
                 VALUES(?1,?2,?3,?4)",
                params![
                    s.to_string(),
                    user.to_string(),
                    device.to_string(),
                    ts(&now())
                ],
            )
        })?;
        Ok(s)
    }

    pub fn create(&self, session: SessionId, memory: MemoryIsolation) -> Result<Conversation> {
        let conv = Conversation {
            id: ConversationId::new(),
            session,
            title: None,
            created_at: now(),
            sync_scope: SyncScope::DeviceLocal,
            memory,
        };
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO conversations(id, session_id, title, created_at,
                    updated_at, sync_scope, memory_scope)
                 VALUES(?1,?2,?3,?4,?4,?5,?6)",
                params![
                    conv.id.to_string(),
                    conv.session.to_string(),
                    conv.title,
                    ts(&conv.created_at),
                    "device_local",
                    memory_scope_name(conv.memory),
                ],
            )
        })?;
        Ok(conv)
    }

    pub fn get(&self, id: ConversationId) -> Result<Conversation> {
        self.store
            .with_conn(|c| {
                c.query_row(
                    "SELECT id, session_id, title, created_at, sync_scope,
                            memory_scope
                     FROM conversations WHERE id=?1 AND deleted=0",
                    params![id.to_string()],
                    Self::row_to_conv,
                )
            })
            .map_err(|_| Error::NotFound(format!("conversation {id}")))
    }

    /// Newest first.
    pub fn list(&self) -> Result<Vec<Conversation>> {
        self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, session_id, title, created_at, sync_scope,
                        memory_scope
                 FROM conversations WHERE deleted=0 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], Self::row_to_conv)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    fn row_to_conv(r: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
        Ok(Conversation {
            id: ConversationId(parse_id(&r.get::<_, String>(0)?)),
            session: SessionId(parse_id(&r.get::<_, String>(1)?)),
            title: r.get(2)?,
            created_at: parse_ts(&r.get::<_, String>(3)?),
            sync_scope: if r.get::<_, String>(4)? == "synchronized" {
                SyncScope::Synchronized
            } else {
                SyncScope::DeviceLocal
            },
            memory: memory_scope_from(&r.get::<_, String>(5)?),
        })
    }

    pub fn rename(&self, id: ConversationId, title: &str) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE conversations SET title=?2, updated_at=?3 WHERE id=?1",
                params![id.to_string(), title, ts(&now())],
            )
        })?;
        Ok(())
    }

    pub fn set_memory_scope(&self, id: ConversationId, mode: MemoryIsolation) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE conversations SET memory_scope=?2 WHERE id=?1",
                params![id.to_string(), memory_scope_name(mode)],
            )
        })?;
        Ok(())
    }

    /// Mark the conversation for E2EE sync (or pull it back to this device).
    pub fn set_sync_scope(&self, id: ConversationId, scope: SyncScope) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE conversations SET sync_scope=?2, updated_at=?3 WHERE id=?1",
                params![
                    id.to_string(),
                    match scope {
                        SyncScope::Synchronized => "synchronized",
                        SyncScope::DeviceLocal => "device_local",
                    },
                    ts(&now())
                ],
            )
        })?;
        Ok(())
    }

    /// Delete the conversation and its messages. Scoped memories are kept —
    /// forgetting is a separate, permission-gated operation.
    pub fn delete(&self, id: ConversationId) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "DELETE FROM messages WHERE conversation_id=?1",
                params![id.to_string()],
            )?;
            c.execute(
                "UPDATE conversations SET deleted=1, updated_at=?2 WHERE id=?1",
                params![id.to_string(), ts(&now())],
            )
        })?;
        Ok(())
    }

    /// Set the title once, from the first user message (keeps "New chat"
    /// placeholders meaningful without a model round-trip).
    pub fn set_title_if_empty(&self, id: ConversationId, title: &str) -> Result<()> {
        let short: String = title.chars().take(60).collect();
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE conversations SET title=?2, updated_at=?3
                 WHERE id=?1 AND title IS NULL",
                params![id.to_string(), short, ts(&now())],
            )
        })?;
        Ok(())
    }

    pub fn append(&self, msg: &Message) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "INSERT OR REPLACE INTO messages(id, conversation_id, role,
                    trust, created_at, content_json)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    msg.id.to_string(),
                    msg.conversation.to_string(),
                    match msg.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => "system",
                        Role::Tool => "tool",
                    },
                    match msg.trust {
                        TrustLevel::Untrusted => "untrusted",
                        TrustLevel::Generated => "generated",
                        TrustLevel::User => "user",
                        TrustLevel::Policy => "policy",
                    },
                    ts(&msg.created_at),
                    serde_json::to_string(&msg.content).unwrap_or_else(|_| "[]".into()),
                ],
            )
        })?;
        Ok(())
    }

    /// Full transcript, oldest first.
    pub fn messages(&self, conv: ConversationId) -> Result<Vec<Message>> {
        self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, role, trust, created_at, content_json
                 FROM messages WHERE conversation_id=?1 ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map(params![conv.to_string()], |r| {
                let role = match r.get::<_, String>(1)?.as_str() {
                    "assistant" => Role::Assistant,
                    "system" => Role::System,
                    "tool" => Role::Tool,
                    _ => Role::User,
                };
                let trust = match r.get::<_, String>(2)?.as_str() {
                    "untrusted" => TrustLevel::Untrusted,
                    "generated" => TrustLevel::Generated,
                    "policy" => TrustLevel::Policy,
                    _ => TrustLevel::User,
                };
                Ok(Message {
                    id: MessageId(parse_id(&r.get::<_, String>(0)?)),
                    conversation: conv,
                    role,
                    trust,
                    created_at: parse_ts(&r.get::<_, String>(3)?),
                    content: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }
}

// ---------------------------------------------------------------------------
// Run checkpoints (crash-safe resume)
// ---------------------------------------------------------------------------

pub struct RunStore {
    store: Arc<Store>,
}

impl RunStore {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Insert a fresh run row at step 0.
    pub fn begin(&self, run: &AgentRun, input: &str) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "INSERT OR REPLACE INTO agent_runs(id, agent_id,
                    conversation_id, started_at, ended_at, state, step,
                    input, checkpoint_json)
                 VALUES(?1,?2,?3,?4,NULL,?5,0,?6,NULL)",
                params![
                    run.id.to_string(),
                    run.agent.to_string(),
                    run.conversation.map(|c| c.to_string()),
                    ts(&run.started_at),
                    run_state_name(run.state),
                    input,
                ],
            )
        })?;
        Ok(())
    }

    /// Persist the message state after step `step`. Called before each model
    /// call so a crash resumes with everything the run had seen.
    pub fn checkpoint(&self, run: &AgentRun, step: u32, messages: &[Message]) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE agent_runs SET step=?2, state=?3, checkpoint_json=?4
                 WHERE id=?1",
                params![
                    run.id.to_string(),
                    step as i64,
                    run_state_name(run.state),
                    serde_json::to_string(messages).unwrap_or_else(|_| "[]".into()),
                ],
            )
        })?;
        Ok(())
    }

    /// Terminal write: state + ended_at. The run no longer counts as open.
    pub fn finish(&self, run: &AgentRun) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE agent_runs SET state=?2, ended_at=?3 WHERE id=?1",
                params![run.id.to_string(), run_state_name(run.state), ts(&now())],
            )
        })?;
        Ok(())
    }

    /// Runs that never reached a terminal state — crash/interruption
    /// candidates that [`AgentRuntime::run`] can resume.
    pub fn interrupted(&self) -> Result<Vec<AgentRun>> {
        self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, agent_id, conversation_id, started_at, state
                 FROM agent_runs WHERE ended_at IS NULL
                 ORDER BY started_at",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(AgentRun {
                    id: AgentRunId(parse_id(&r.get::<_, String>(0)?)),
                    agent: AgentId(
                        r.get::<_, Option<String>>(1)?
                            .map(|s| parse_id(&s))
                            .unwrap_or_else(uuid::Uuid::nil),
                    ),
                    conversation: r
                        .get::<_, Option<String>>(2)?
                        .map(|s| ConversationId(parse_id(&s))),
                    started_at: parse_ts(&r.get::<_, String>(3)?),
                    ended_at: None,
                    state: run_state_from(&r.get::<_, String>(4)?),
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Load a run's checkpoint: the run row plus its step + messages.
    pub fn load(&self, id: AgentRunId) -> Result<(AgentRun, u32, Vec<Message>, String)> {
        self.store
            .with_conn(|c| {
                c.query_row(
                    "SELECT id, agent_id, conversation_id, started_at, state,
                            step, checkpoint_json, input
                     FROM agent_runs WHERE id=?1",
                    params![id.to_string()],
                    |r| {
                        Ok((
                            AgentRun {
                                id: AgentRunId(parse_id(&r.get::<_, String>(0)?)),
                                agent: AgentId(
                                    r.get::<_, Option<String>>(1)?
                                        .map(|s| parse_id(&s))
                                        .unwrap_or_else(uuid::Uuid::nil),
                                ),
                                conversation: r
                                    .get::<_, Option<String>>(2)?
                                    .map(|s| ConversationId(parse_id(&s))),
                                started_at: parse_ts(&r.get::<_, String>(3)?),
                                ended_at: None,
                                state: run_state_from(&r.get::<_, String>(4)?),
                            },
                            r.get::<_, i64>(5)? as u32,
                            serde_json::from_str::<Vec<Message>>(
                                &r.get::<_, Option<String>>(6)?
                                    .unwrap_or_else(|| "[]".into()),
                            )
                            .unwrap_or_default(),
                            r.get::<_, String>(7)?,
                        ))
                    },
                )
            })
            .map_err(|_| Error::NotFound(format!("run {id}")))
    }

    /// Give up on an open run (mark it failed without resuming).
    pub fn abandon(&self, id: AgentRunId) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE agent_runs SET state='failed', ended_at=?2
                 WHERE id=?1 AND ended_at IS NULL",
                params![id.to_string(), ts(&now())],
            )
        })?;
        Ok(())
    }
}

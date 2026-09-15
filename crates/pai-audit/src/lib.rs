//! Audit log — append-only record of meaningful platform actions.
//! Powers "what did my AI do today?" and the security review surface.

use pai_core::*;
use pai_storage::{parse_ts, ts, Store};
use rusqlite::params;
use serde::Deserialize;

pub struct AuditLog {
    store: std::sync::Arc<Store>,
}

/// Secret-ish keys whose values must never land in the audit trail.
const REDACT_KEYS: &[&str] = &["password", "token", "secret", "key", "authorization"];

/// Deep-redact a JSON value for audit display.
pub fn redact(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, val)| {
                let val = if REDACT_KEYS.iter().any(|s| k.to_lowercase().contains(s)) {
                    serde_json::Value::String("[redacted]".into())
                } else {
                    redact(val)
                };
                (k.clone(), val)
            })
            .collect::<serde_json::Map<_, _>>()
            .into(),
        serde_json::Value::Array(arr) => arr.iter().map(redact).collect(),
        other => other.clone(),
    }
}

impl AuditLog {
    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }

    pub fn record(&self, event: &AuditEvent) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO audit_events(id, at, device_id, agent_id, run_id,
                    conversation_id, kind, tool, detail_json, outcome)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    event.id.to_string(),
                    ts(&event.at),
                    event.device.map(|d| d.to_string()),
                    event.agent.map(|a| a.to_string()),
                    event.run.map(|r| r.to_string()),
                    event.conversation.map(|c| c.to_string()),
                    kind_name(event.kind),
                    event.tool,
                    serde_json::to_string(&redact(&event.detail)).unwrap(),
                    outcome_name(event.outcome),
                ],
            )
        })?;
        Ok(())
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<AuditEvent>> {
        self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, at, device_id, agent_id, run_id, conversation_id,
                        kind, tool, detail_json, outcome
                 FROM audit_events ORDER BY at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit as i64], |r| {
                Ok(AuditEvent {
                    id: AuditEventId(
                        uuid::Uuid::parse_str(&r.get::<_, String>(0)?).unwrap_or_default(),
                    ),
                    at: parse_ts(&r.get::<_, String>(1)?),
                    device: r
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                        .map(DeviceId),
                    agent: r
                        .get::<_, Option<String>>(3)?
                        .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                        .map(AgentId),
                    run: r
                        .get::<_, Option<String>>(4)?
                        .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                        .map(AgentRunId),
                    conversation: r
                        .get::<_, Option<String>>(5)?
                        .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                        .map(ConversationId),
                    kind: kind_from(&r.get::<_, String>(6)?),
                    tool: r.get(7)?,
                    detail: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
                    outcome: outcome_from(&r.get::<_, String>(9)?),
                })
            })?;
            rows.collect()
        })
    }

    /// All events for one agent run — the "what did it do" query.
    pub fn for_run(&self, run: AgentRunId) -> Result<Vec<AuditEvent>> {
        self.store.with_conn(|c| {
            let mut stmt = c.prepare("SELECT id FROM audit_events WHERE run_id=?1 ORDER BY at")?;
            let ids: Vec<String> = stmt
                .query_map(params![run.to_string()], |r| r.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(ids)
        })?;
        // Reuse recent() filter for simplicity in V1.
        Ok(self
            .recent(10_000)?
            .into_iter()
            .filter(|e| e.run == Some(run))
            .collect())
    }
}

fn kind_name(k: AuditKind) -> &'static str {
    match k {
        AuditKind::MessageSent => "message_sent",
        AuditKind::RunStarted => "run_started",
        AuditKind::RunFinished => "run_finished",
        AuditKind::ModelResponded => "model_responded",
        AuditKind::MemoryWritten => "memory_written",
        AuditKind::MemoryDeleted => "memory_deleted",
        AuditKind::ToolRequested => "tool_requested",
        AuditKind::ToolAllowed => "tool_allowed",
        AuditKind::ToolDenied => "tool_denied",
        AuditKind::ToolExecuted => "tool_executed",
        AuditKind::ModelLoaded => "model_loaded",
        AuditKind::SyncReceived => "sync_received",
        AuditKind::SyncSent => "sync_sent",
        AuditKind::PermissionChanged => "permission_changed",
        AuditKind::ApprovalRequested => "approval_requested",
        AuditKind::ApprovalResolved => "approval_resolved",
        AuditKind::AppDeployed => "app_deployed",
        AuditKind::AppRun => "app_run",
        AuditKind::AppRemoved => "app_removed",
        AuditKind::AppBackedUp => "app_backed_up",
        AuditKind::AppRestored => "app_restored",
        AuditKind::AppMigrated => "app_migrated",
    }
}

fn kind_from(s: &str) -> AuditKind {
    AuditKind::deserialize(serde_json::Value::String(s.into())).unwrap_or(AuditKind::ToolExecuted)
}

fn outcome_name(o: AuditOutcome) -> &'static str {
    match o {
        AuditOutcome::Ok => "ok",
        AuditOutcome::Denied => "denied",
        AuditOutcome::Error => "error",
        AuditOutcome::Cancelled => "cancelled",
    }
}

fn outcome_from(s: &str) -> AuditOutcome {
    match s {
        "denied" => AuditOutcome::Denied,
        "error" => AuditOutcome::Error,
        "cancelled" => AuditOutcome::Cancelled,
        _ => AuditOutcome::Ok,
    }
}

pub fn event(kind: AuditKind, outcome: AuditOutcome) -> AuditEvent {
    AuditEvent {
        id: AuditEventId::new(),
        at: now(),
        device: None,
        agent: None,
        run: None,
        conversation: None,
        kind,
        tool: None,
        detail: serde_json::json!({}),
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted() {
        let v = serde_json::json!({"user": "a", "api_token": "sk-123", "nested": {"auth_key": 1}});
        let r = redact(&v);
        assert_eq!(r["api_token"], "[redacted]");
        assert_eq!(r["nested"]["auth_key"], "[redacted]");
        assert_eq!(r["user"], "a");
    }
}

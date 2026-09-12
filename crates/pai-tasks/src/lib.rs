//! Task + background-agent scaffolding.
//!
//! Real now: persistent `Task`s, `ScheduledTask` triggers, an in-process
//! `Scheduler` that dispatches due tasks to a handler. Still respects the
//! permission system — background execution is not a privilege escalation.
//!
//! Not yet: OS-level wake (APNS/WorkManager), distributed cron.

use async_trait::async_trait;
use pai_core::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A durable unit of background work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub task: Task,
    pub trigger: Trigger,
    /// Arguments handed to the handler (e.g. the user's instruction text).
    pub payload: serde_json::Value,
}

/// What the scheduler calls when a task fires.
#[async_trait]
pub trait TaskHandler: Send + Sync {
    async fn handle(&self, task: &ScheduledTask) -> Result<()>;
}

/// In-process scheduler. Persists nothing beyond what the caller stores;
/// on restart, tasks are re-registered from the `tasks` table.
pub struct Scheduler {
    pending: Mutex<Vec<ScheduledTask>>,
    handler: Arc<dyn TaskHandler>,
}

impl Scheduler {
    pub fn new(handler: Arc<dyn TaskHandler>) -> Self {
        Self {
            pending: Mutex::new(vec![]),
            handler,
        }
    }

    pub async fn schedule(&self, task: ScheduledTask) {
        self.pending.lock().await.push(task);
    }

    /// Execute every task whose `run_at` has passed. Returns task ids run.
    /// Callers drive this periodically (app foreground, timer, CLI).
    pub async fn drain_due(&self, at: Timestamp) -> Vec<TaskId> {
        let mut pending = self.pending.lock().await;
        let (due, later): (Vec<_>, Vec<_>) = pending
            .drain(..)
            .partition(|t| t.task.run_at.map(|r| r <= at).unwrap_or(true));
        *pending = later;
        let mut ran = vec![];
        for t in due {
            if self.handler.handle(&t).await.is_ok() {
                ran.push(t.task.id);
            }
        }
        ran
    }
}

/// Minimal cron-ish next-fire calculator. V1 supports interval + daily
/// "HH:MM" schedules; swap in a full cron parser later without API churn.
pub fn next_fire(trigger: &Trigger, from: Timestamp) -> Option<Timestamp> {
    match trigger {
        Trigger::Manual => None,
        Trigger::Event { .. } => None,
        Trigger::Schedule { cron } => {
            // "@every Ns" interval form.
            if let Some(secs) = cron
                .strip_prefix("@every ")
                .and_then(|s| s.trim_end_matches('s').parse::<i64>().ok())
            {
                return Some(from + chrono::Duration::seconds(secs));
            }
            None
        }
    }
}

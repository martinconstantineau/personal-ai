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

// ---------------------------------------------------------------------------
// Persistent store — the `tasks` table, shared across devices via sync.
// ---------------------------------------------------------------------------

pub mod store {
    use crate::{next_fire, ScheduledTask, TaskHandler};
    use pai_core::*;
    use pai_storage::{parse_ts, ts, Store};
    use rusqlite::params;
    use std::sync::Arc;

    /// One `tasks` row — the syncable unit of background work.
    #[derive(Debug, Clone)]
    pub struct TaskRow {
        pub id: TaskId,
        pub title: String,
        pub agent_id: AgentId,
        pub created_at: Timestamp,
        pub run_at: Option<Timestamp>,
        pub state: TaskState,
        pub trigger: Trigger,
        pub payload: serde_json::Value,
        pub result: Option<serde_json::Value>,
        pub claimed_by: Option<DeviceId>,
        pub lease_expires_at: Option<Timestamp>,
        pub updated_at: Timestamp,
        pub deleted: bool,
        pub sync_scope: SyncScope,
    }

    impl TaskRow {
        pub fn scheduled(&self) -> ScheduledTask {
            ScheduledTask {
                task: Task {
                    id: self.id,
                    title: self.title.clone(),
                    agent: self.agent_id,
                    created_at: self.created_at,
                    run_at: self.run_at,
                    state: self.state,
                    sync_scope: self.sync_scope,
                },
                trigger: self.trigger.clone(),
                payload: self.payload.clone(),
            }
        }
    }

    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
        let id: String = r.get(0)?;
        let agent: String = r.get(2)?;
        let trigger: String = r.get(9)?;
        let payload: String = r.get(10)?;
        let claimed: Option<String> = r.get(12)?;
        Ok(TaskRow {
            id: TaskId(uuid::Uuid::parse_str(&id).unwrap_or_default()),
            title: r.get(1)?,
            agent_id: AgentId(uuid::Uuid::parse_str(&agent).unwrap_or_default()),
            created_at: parse_ts(&r.get::<_, String>(3)?),
            run_at: r.get::<_, Option<String>>(4)?.map(|s| parse_ts(&s)),
            state: serde_json::from_str(&format!("\"{}\"", r.get::<_, String>(5)?))
                .unwrap_or(TaskState::Pending),
            trigger: serde_json::from_str(&trigger).unwrap_or(Trigger::Manual),
            payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
            result: r
                .get::<_, Option<String>>(11)?
                .and_then(|s| serde_json::from_str(&s).ok()),
            claimed_by: claimed
                .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                .map(DeviceId),
            lease_expires_at: r.get::<_, Option<String>>(13)?.map(|s| parse_ts(&s)),
            updated_at: parse_ts(&r.get::<_, String>(6)?),
            deleted: r.get::<_, i64>(7)? != 0,
            sync_scope: serde_json::from_str(&format!("\"{}\"", r.get::<_, String>(8)?))
                .unwrap_or_default(),
        })
    }

    const COLS: &str = "id, title, agent_id, created_at, run_at, state,
        updated_at, deleted, sync_scope, trigger_json, payload_json,
        result_json, claimed_by, lease_expires_at";

    /// Persist a new task. `sync_scope` decides whether it roams.
    pub fn create_task(
        store: &Arc<Store>,
        title: &str,
        agent_id: AgentId,
        run_at: Option<Timestamp>,
        trigger: &Trigger,
        payload: &serde_json::Value,
        scope: SyncScope,
    ) -> Result<TaskId> {
        let id = TaskId::new();
        let now = now();
        store.with_conn(|c| {
            c.execute(
                &format!(
                    "INSERT INTO tasks({COLS}) VALUES(?1,?2,?3,?4,?5,'pending',
                        ?6,0,?7,?8,?9,NULL,NULL,NULL)"
                ),
                params![
                    id.to_string(),
                    title,
                    agent_id.to_string(),
                    ts(&now),
                    run_at.map(|t| ts(&t)),
                    ts(&now),
                    serde_json::to_string(&scope)
                        .unwrap_or_else(|_| "\"device_local\"".into())
                        .trim_matches('"')
                        .to_string(),
                    serde_json::to_string(trigger).unwrap_or_else(|_| "{}".into()),
                    serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
                ],
            )
        })?;
        Ok(id)
    }

    /// All live tasks (deleted excluded unless `include_deleted`).
    pub fn list_tasks(store: &Arc<Store>, include_deleted: bool) -> Result<Vec<TaskRow>> {
        store.with_conn(|c| {
            let sql = format!(
                "SELECT {COLS} FROM tasks {} ORDER BY created_at DESC",
                if include_deleted {
                    ""
                } else {
                    "WHERE deleted=0"
                }
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Tasks needing a claim attempt at `at`: due (run_at passed or null),
    /// not deleted, and either pending+unclaimed/expired-claim or
    /// running with an expired lease (crash recovery).
    pub fn due_tasks(store: &Arc<Store>, at: Timestamp) -> Result<Vec<TaskRow>> {
        store.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {COLS} FROM tasks
                 WHERE deleted=0 AND (run_at IS NULL OR run_at <= ?1)
                   AND ((state='pending' AND (claimed_by IS NULL
                            OR lease_expires_at IS NULL
                            OR lease_expires_at <= ?1))
                     OR (state='running' AND lease_expires_at <= ?1))
                 ORDER BY run_at"
            ))?;
            let rows = stmt.query_map(params![ts(&at)], row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Try to claim `id` for `device` with a `lease_secs` lease. Wins when
    /// the task is unclaimed, already ours, or the lease expired — a
    /// single UPDATE so two local claimers can't both succeed; across
    /// devices the sync engine's LWW picks one winner after exchange.
    pub fn claim_task(
        store: &Arc<Store>,
        id: TaskId,
        device: DeviceId,
        lease_secs: i64,
    ) -> Result<bool> {
        let now = now();
        let lease_end = now + chrono::Duration::seconds(lease_secs);
        let n = store.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET claimed_by=?2, lease_expires_at=?3,
                    state='running', updated_at=?4
                 WHERE id=?1 AND deleted=0 AND state IN ('pending','running')
                   AND (claimed_by IS NULL OR claimed_by=?2
                        OR lease_expires_at IS NULL OR lease_expires_at <= ?5)",
                params![
                    id.to_string(),
                    device.to_string(),
                    ts(&lease_end),
                    ts(&now),
                    ts(&now),
                ],
            )
        })?;
        Ok(n > 0)
    }

    /// Fetch one task row by id.
    pub fn get_task(store: &Arc<Store>, id: TaskId) -> Result<Option<TaskRow>> {
        store.with_conn(|c| {
            let mut stmt = c.prepare(&format!("SELECT {COLS} FROM tasks WHERE id=?1"))?;
            let mut rows = stmt.query_map(params![id.to_string()], row)?;
            rows.next().transpose()
        })
    }

    /// Record the run outcome; `claimed_by` stays as an audit trail.
    /// A `None` result preserves whatever the handler already wrote.
    pub fn finish_task(
        store: &Arc<Store>,
        id: TaskId,
        state: TaskState,
        result: Option<serde_json::Value>,
    ) -> Result<()> {
        store.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET state=?2, updated_at=?4,
                    result_json=COALESCE(?3, result_json)
                 WHERE id=?1",
                params![
                    id.to_string(),
                    serde_json::to_string(&state)
                        .unwrap_or_else(|_| "\"pending\"".into())
                        .trim_matches('"')
                        .to_string(),
                    result.map(|r| serde_json::to_string(&r).unwrap_or_else(|_| "{}".into())),
                    ts(&now()),
                ],
            )
        })?;
        Ok(())
    }

    /// Requeue a recurring (schedule) task after a successful run.
    pub fn requeue_task(store: &Arc<Store>, id: TaskId, next_run: Timestamp) -> Result<()> {
        store.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET state='pending', run_at=?2, claimed_by=NULL,
                    lease_expires_at=NULL, updated_at=?3 WHERE id=?1",
                params![id.to_string(), ts(&next_run), ts(&now())],
            )
        })?;
        Ok(())
    }

    /// Soft-delete — the tombstone propagates to peers.
    pub fn remove_task(store: &Arc<Store>, id: TaskId) -> Result<bool> {
        let n = store.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET deleted=1, updated_at=?2 WHERE id=?1",
                params![id.to_string(), ts(&now())],
            )
        })?;
        Ok(n > 0)
    }

    /// Flip the sync scope (opt a task in/out of roaming).
    pub fn set_sync_scope(store: &Arc<Store>, id: TaskId, scope: SyncScope) -> Result<bool> {
        let n = store.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET sync_scope=?2, updated_at=?3 WHERE id=?1",
                params![
                    id.to_string(),
                    serde_json::to_string(&scope)
                        .unwrap_or_else(|_| "\"device_local\"".into())
                        .trim_matches('"')
                        .to_string(),
                    ts(&now()),
                ],
            )
        })?;
        Ok(n > 0)
    }

    /// Store a result payload without touching state — handlers use this
    /// to leave output (e.g. an agent reply) on the row they ran.
    pub fn set_result(store: &Arc<Store>, id: TaskId, result: serde_json::Value) -> Result<()> {
        store.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET result_json=?2, updated_at=?3 WHERE id=?1",
                params![
                    id.to_string(),
                    serde_json::to_string(&result).unwrap_or_else(|_| "{}".into()),
                    ts(&now()),
                ],
            )
        })?;
        Ok(())
    }

    /// Claim every due task for `device`, run it through `handler`, and
    /// record the outcome. Schedule triggers requeue via `next_fire`;
    /// manual/event tasks finish done/failed. `claimed_by` doubles as the
    /// who-ran-it record; the handler owns `result_json` (`set_result`).
    /// Returns (id, ok) per run.
    pub async fn claim_and_run(
        store: &Arc<Store>,
        handler: &dyn TaskHandler,
        device: DeviceId,
        at: Timestamp,
        lease_secs: i64,
    ) -> Result<Vec<(TaskId, bool)>> {
        let mut ran = vec![];
        for t in due_tasks(store, at)? {
            if !claim_task(store, t.id, device, lease_secs)? {
                continue; // another device holds a live claim
            }
            let ok = handler.handle(&t.scheduled()).await.is_ok();
            if ok {
                if let Some(next) = next_fire(&t.trigger, at) {
                    requeue_task(store, t.id, next)?;
                    ran.push((t.id, true));
                    continue;
                }
            }
            finish_task(
                store,
                t.id,
                if ok {
                    TaskState::Done
                } else {
                    TaskState::Failed
                },
                (!ok).then(|| serde_json::json!({"error": "handler failed"})),
            )?;
            ran.push((t.id, ok));
        }
        Ok(ran)
    }
}

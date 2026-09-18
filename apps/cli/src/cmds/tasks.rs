//! `pai task …` — synced background tasks with claim/lease execution.

use crate::ctx::*;
use crate::util::*;
use clap::Subcommand;
use pai_agent::{AgentDefinition, DenyApprovals};
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum TaskCmd {
    /// List tasks, newest first.
    List,
    /// Create a task. With --prompt it runs through the agent on
    /// whichever device claims it.
    Add {
        title: String,
        /// When to run: RFC3339 timestamp or +N seconds from now.
        /// Omit to run on the next tick.
        #[arg(long)]
        at: Option<String>,
        /// Repeat every N seconds ("@every Ns" trigger).
        #[arg(long)]
        every: Option<u64>,
        /// Prompt text handed to the agent when the task fires.
        #[arg(long)]
        prompt: Option<String>,
        /// Publish the result to the notification inbox when it runs
        /// ("notify": true lands in the inbox; "external" also fans out
        /// to channels configured in notify.json).
        #[arg(long)]
        notify: bool,
        /// Keep the task on this device (default: synchronized).
        #[arg(long)]
        local: bool,
    },
    /// Soft-delete a task — the tombstone propagates to peers.
    Remove { id: String },
    /// Claim and run due tasks. Claims are pushed before each run when a
    /// transport is given, so peers see them before results land.
    Tick {
        /// Claim lease in seconds — a crashed runner's tasks become
        /// claimable again after this.
        #[arg(long, default_value = "300")]
        lease_secs: i64,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Change a task's sync scope (synchronized | device_local).
    Sync { id: String, mode: String },
}

/// Runs a claimed task through the agent when its payload is a prompt.
/// Approvals are denied — a background tick has nobody to ask, so
/// interactive-permission actions are refused, not silently granted.
struct PromptTaskHandler<'a> {
    ctx: &'a Ctx,
    def: &'a AgentDefinition,
}

#[async_trait::async_trait]
impl pai_tasks::TaskHandler for PromptTaskHandler<'_> {
    async fn handle(&self, t: &pai_tasks::ScheduledTask) -> Result<()> {
        if t.payload.get("kind").and_then(|k| k.as_str()) != Some("prompt") {
            return Ok(()); // marker/reminder payloads just complete
        }
        let text = t
            .payload
            .get("text")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        let out = send(self.ctx, self.def, text, None, None, &DenyApprovals).await?;
        pai_tasks::store::set_result(
            &self.ctx.store,
            t.task.id,
            serde_json::json!({"reply": out.answer}),
        )?;
        Ok(())
    }
}

/// `pai task` — synced background tasks with claim/lease execution.
///
/// Passes the resolved provider (`ctx.provider_name`/`ctx.model`), not the
/// raw `--provider` flag — "auto" is not a registry id and would fail at
/// run time with `no provider 'auto'`.
pub(crate) async fn run(cmd: &TaskCmd, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    run_task_cmds(cmd, ctx, &ctx.provider_name, ctx.model.clone(), cfg).await
}

async fn run_task_cmds(
    cmd: &TaskCmd,
    ctx: &Ctx,
    provider: &str,
    model: Option<String>,
    cfg: &pai_config::Config,
) -> Result<()> {
    use pai_tasks::{store as ts, TaskHandler};
    match cmd {
        TaskCmd::List => {
            let me = ctx.agent.device.to_string();
            for t in ts::list_tasks(&ctx.store, false)? {
                let claim = match (&t.claimed_by, &t.lease_expires_at) {
                    (Some(c), Some(l)) => format!(
                        " claimed:{}{}",
                        if c.to_string() == me {
                            "me".to_string()
                        } else {
                            c.to_string()[..8].to_string()
                        },
                        if *l > now() { "·live" } else { "·expired" }
                    ),
                    _ => String::new(),
                };
                println!(
                    "{id:.8}  {state:<8} {run:<20}{claim}  {title}",
                    id = t.id.to_string(),
                    state = serde_json::to_string(&t.state)
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string(),
                    run = t
                        .run_at
                        .map(|r| pai_storage::ts(&r))
                        .unwrap_or_else(|| "on-tick".into()),
                    claim = claim,
                    title = t.title,
                );
            }
        }
        TaskCmd::Add {
            title,
            at,
            every,
            prompt,
            notify,
            local,
        } => {
            let run_at = match at {
                Some(s) => Some(parse_at(s)?),
                None => None,
            };
            let trigger = every
                .map(|s| Trigger::Schedule {
                    cron: format!("@every {s}s"),
                })
                .unwrap_or(Trigger::Manual);
            let payload = match prompt {
                Some(p) => serde_json::json!({
                    "kind": "prompt",
                    "text": p,
                    "notify": notify,
                }),
                None => serde_json::json!({"notify": notify}),
            };
            let scope = if *local {
                SyncScope::DeviceLocal
            } else {
                SyncScope::Synchronized
            };
            let id = ts::create_task(
                &ctx.store,
                title,
                AgentId(uuid::Uuid::nil()),
                run_at,
                &trigger,
                &payload,
                scope,
            )?;
            println!(
                "task {id} created{}{}",
                if *local { " (device-local)" } else { "" },
                run_at
                    .map(|r| format!(" — runs {}", pai_storage::ts(&r)))
                    .unwrap_or_default()
            );
        }
        TaskCmd::Remove { id } => {
            if ts::remove_task(&ctx.store, TaskId(parse_uuid(id, "task")?))? {
                println!("removed task {id}");
            } else {
                println!("no such task: {id}");
            }
        }
        TaskCmd::Sync { id, mode } => {
            let scope = match mode.as_str() {
                "synchronized" | "sync" => SyncScope::Synchronized,
                "device_local" | "local" => SyncScope::DeviceLocal,
                _ => {
                    return Err(Error::InvalidInput(
                        "scope must be 'synchronized' or 'device_local'".into(),
                    ))
                }
            };
            if ts::set_sync_scope(&ctx.store, TaskId(parse_uuid(id, "task")?), scope)? {
                println!("task {id} → {mode}");
            } else {
                println!("no such task: {id}");
            }
        }
        TaskCmd::Tick {
            lease_secs,
            dir,
            relay,
            token,
        } => {
            // Optional engine to advertise claims/results over a transport.
            let engine = match (dir.is_some() || relay.is_some())
                .then(|| sync_transport(dir, relay, token))
            {
                Some(Ok(t)) => {
                    let vault = pai_sync::crypto::vault_key(&cfg.data_dir)?
                        .ok_or_else(|| Error::Sync("no vault key — pair a device first".into()))?;
                    Some(pai_sync::engine::SyncEngine::new(
                        t,
                        ctx.store.clone(),
                        vault,
                        ctx.agent.device,
                        &cfg.data_dir,
                    ))
                }
                Some(Err(e)) => return Err(e),
                None => None,
            };
            let def = agent_def(provider, model);
            let handler = PromptTaskHandler { ctx, def: &def };
            let mut ran = 0usize;
            for t in ts::due_tasks(&ctx.store, now())? {
                if !ts::claim_task(&ctx.store, t.id, ctx.agent.device, *lease_secs)? {
                    println!("{:.8}  claimed by another device — skipped", t.id);
                    continue;
                }
                // Advertise the claim before running — shrinks the window
                // where a peer could claim the same task.
                if let Some(e) = &engine {
                    e.push().await.ok();
                }
                let ok = handler.handle(&t.scheduled()).await.is_ok();
                if ok {
                    if let Some(next) = pai_tasks::next_fire(&t.trigger, now()) {
                        ts::requeue_task(&ctx.store, t.id, next)?;
                        println!("{:.8}  ran — requeued for {}", t.id, pai_storage::ts(&next));
                        ran += 1;
                        continue;
                    }
                }
                ts::finish_task(
                    &ctx.store,
                    t.id,
                    if ok {
                        TaskState::Done
                    } else {
                        TaskState::Failed
                    },
                    (!ok).then(|| serde_json::json!({"error": "handler failed"})),
                )?;
                if t.payload
                    .get("notify")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    let sink = pai_notify::StoreNotifySink {
                        store: ctx.store.clone(),
                        config: pai_notify::load_config(&cfg.data_dir)?,
                        email: ctx.email.clone(),
                    };
                    let result = ts::get_task(&ctx.store, t.id)
                        .ok()
                        .flatten()
                        .and_then(|r| r.result)
                        .map(|v| {
                            v.as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| v.to_string())
                        })
                        .unwrap_or_default();
                    pai_notify::NotifySink::publish(
                        &sink,
                        &format!("task '{}' {}", t.title, if ok { "done" } else { "FAILED" }),
                        &result,
                        &format!("task:{:.8}", t.id),
                        t.sync_scope,
                    )?;
                }
                println!("{:.8}  {}", t.id, if ok { "done" } else { "FAILED" });
                ran += 1;
            }
            if let Some(e) = &engine {
                let out = e.push().await?;
                println!("pushed {0} sync object(s)", out.pushed);
            }
            println!("{ran} task(s) ran");
        }
    }
    Ok(())
}

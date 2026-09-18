//! `pai workflow …` — declarative multi-step workflows over the agent
//! runtime: definitions, runs, resume.

use crate::ctx::*;
use clap::Subcommand;
use pai_agent::AgentEvent;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum WorkflowCmd {
    /// List workflow definitions.
    List,
    /// Add or update a workflow from a JSON file ("-" reads stdin).
    Add {
        /// Path to the workflow definition JSON.
        file: String,
        /// Keep it on this device (default: synchronized).
        #[arg(long)]
        local: bool,
    },
    /// Print a workflow's definition JSON.
    Show { id_or_name: String },
    /// Soft-delete a workflow — the tombstone propagates to peers.
    Remove { id_or_name: String },
    /// Change a workflow's sync scope (synchronized | device_local).
    Sync { id_or_name: String, mode: String },
    /// List runs of a workflow, newest first.
    Runs { id_or_name: String },
    /// Run a workflow once through the agent runtime.
    Run {
        id_or_name: String,
        /// Input text bound to {{input}} in step templates.
        #[arg(long, default_value = "")]
        input: String,
    },
    /// Resume a crashed/interrupted run from its saved step cursor.
    Resume { run_id: String },
}

/// `pai workflow ...` — declarative multi-step runs over the agent runtime.
/// `pai workflow` — declarative multi-step runs over the agent runtime.
///
/// Passes the resolved provider — see `cmds::tasks::run` for why.
pub(crate) async fn run(cmd: &WorkflowCmd, ctx: &Ctx) -> Result<()> {
    run_workflow_cmds(cmd, ctx, &ctx.provider_name, ctx.model.clone()).await
}

async fn run_workflow_cmds(
    cmd: &WorkflowCmd,
    ctx: &Ctx,
    provider: &str,
    model: Option<String>,
) -> Result<()> {
    use pai_workflows::{store as ws, WorkflowDefinition, WorkflowRunner};
    match cmd {
        WorkflowCmd::List => {
            for w in ws::list_workflows(&ctx.store, false)? {
                println!(
                    "{:.8}  {:<24} {} step{}  tools:{}  {}",
                    w.id,
                    w.name,
                    w.definition.steps.len(),
                    if w.definition.steps.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    if w.definition.tools.is_empty() {
                        "none".to_string()
                    } else {
                        w.definition.tools.join(",")
                    },
                    w.sync_scope,
                );
            }
        }
        WorkflowCmd::Add { file, local } => {
            let raw = if file == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                    .map_err(|e| Error::Other(e.to_string()))?;
                s
            } else {
                std::fs::read_to_string(file)
                    .map_err(|e| Error::InvalidInput(format!("{file}: {e}")))?
            };
            let def: WorkflowDefinition = serde_json::from_str(&raw)
                .map_err(|e| Error::InvalidInput(format!("bad workflow json: {e}")))?;
            let scope = if *local {
                pai_core::SyncScope::DeviceLocal
            } else {
                pai_core::SyncScope::Synchronized
            };
            if let Some(existing) = ws::get_workflow(&ctx.store, &def.name)? {
                ws::update_workflow(&ctx.store, &existing.id, &def)?;
                println!("updated workflow {} ({:.8})", def.name, existing.id);
            } else {
                let id = ws::create_workflow(&ctx.store, &def, scope)?;
                println!("added workflow {} ({:.8})", def.name, id);
            }
        }
        WorkflowCmd::Show { id_or_name } => {
            let w = ws::get_workflow(&ctx.store, id_or_name)?
                .ok_or_else(|| Error::NotFound(format!("workflow {id_or_name}")))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&w.definition).unwrap_or_default()
            );
            println!(
                "id: {}  scope: {}  updated: {}",
                w.id, w.sync_scope, w.updated_at
            );
        }
        WorkflowCmd::Remove { id_or_name } => {
            if ws::remove_workflow(&ctx.store, id_or_name)? {
                println!("removed {id_or_name} (tombstone propagates on sync)");
            } else {
                println!("no workflow {id_or_name}");
            }
        }
        WorkflowCmd::Sync { id_or_name, mode } => {
            let scope = match mode.as_str() {
                "synchronized" | "sync" => pai_core::SyncScope::Synchronized,
                "device_local" | "local" => pai_core::SyncScope::DeviceLocal,
                other => {
                    return Err(Error::InvalidInput(format!(
                        "bad scope {other:?} — synchronized|device_local"
                    )))
                }
            };
            if ws::set_sync_scope(&ctx.store, id_or_name, scope)? {
                println!("{id_or_name} → {mode}");
            } else {
                println!("no workflow {id_or_name}");
            }
        }
        WorkflowCmd::Runs { id_or_name } => {
            let w = ws::get_workflow(&ctx.store, id_or_name)?
                .ok_or_else(|| Error::NotFound(format!("workflow {id_or_name}")))?;
            for r in ws::list_runs(&ctx.store, &w.id)? {
                println!(
                    "{:.8}  {:<8} step {}/{}  {}{}",
                    r.id,
                    r.status,
                    r.step_index,
                    w.definition.steps.len(),
                    r.started_at,
                    r.error.map(|e| format!("  err: {e}")).unwrap_or_default(),
                );
            }
        }
        WorkflowCmd::Run { id_or_name, input } => {
            let w = ws::get_workflow(&ctx.store, id_or_name)?
                .ok_or_else(|| Error::NotFound(format!("workflow {id_or_name}")))?;
            let runner = WorkflowRunner {
                agent: &ctx.agent,
                store: &ctx.store,
                provider: provider.to_string(),
                model,
            };
            let emit = |e: AgentEvent| print_event(&e);
            let out = runner.run(&w, input, &CliApproval, &emit).await?;
            print_workflow_output(&out);
        }
        WorkflowCmd::Resume { run_id } => {
            let runner = WorkflowRunner {
                agent: &ctx.agent,
                store: &ctx.store,
                provider: provider.to_string(),
                model,
            };
            let emit = |e: AgentEvent| print_event(&e);
            let out = runner.resume(run_id, &CliApproval, &emit).await?;
            print_workflow_output(&out);
        }
    }
    Ok(())
}

/// Print a finished workflow run's final output (per-step results were
/// already streamed through `emit`).
fn print_workflow_output(out: &pai_workflows::WorkflowOutcome) {
    match &out.output {
        Some(v) => println!(
            "
{}",
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        ),
        None => println!("(no output)"),
    }
}

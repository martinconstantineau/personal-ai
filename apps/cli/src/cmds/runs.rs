//! `pai runs …` — agent run recovery: list interrupted, resume, abandon.

use crate::ctx::*;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum RunsCmd {
    /// Runs that never finished — crash/interrupt candidates.
    Interrupted,
    /// Resume an interrupted run from its last checkpoint.
    Resume { id: String },
    /// Mark an interrupted run failed (give up on it).
    Abandon { id: String },
}

pub(crate) async fn run(cmd: RunsCmd, ctx: &Ctx) -> Result<()> {
    match cmd {
        RunsCmd::Interrupted => {
            let runs = ctx.runs.interrupted()?;
            if runs.is_empty() {
                println!("(no interrupted runs)");
            }
            for r in runs {
                println!(
                    "  {}  state={:?}  started={}  conv={:?}",
                    r.id,
                    r.state,
                    r.started_at.format("%Y-%m-%d %H:%M:%S"),
                    r.conversation.map(|c| c.to_string())
                );
            }
        }
        RunsCmd::Resume { id } => {
            let rid = AgentRunId(parse_uuid(&id, "run")?);
            let def = agent_def(&ctx.provider_name, ctx.model.clone());
            send(ctx, &def, "", None, Some(rid), &CliApproval).await?;
        }
        RunsCmd::Abandon { id } => {
            ctx.runs.abandon(AgentRunId(parse_uuid(&id, "run")?))?;
        }
    }
    Ok(())
}

//! `pai memories …` — recall list, forget, federate to a circle.

use crate::ctx::Ctx;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;
use pai_memory::{MemoryScopeQuery, RecallQuery};
use pai_tools::Tool;

#[derive(Subcommand)]
pub(crate) enum MemCmd {
    /// Forget a memory by uuid, or by a text query.
    Forget { target: String },
    /// Federate a memory to a named circle (family/team) instead of the
    /// whole vault; omit --circle to move it back to vault-wide.
    Share {
        /// Memory uuid.
        target: String,
        #[arg(long)]
        circle: Option<String>,
    },
}

pub(crate) async fn run(cmd: Option<MemCmd>, ctx: &Ctx) -> Result<()> {
    match cmd {
        None => {
            let items = ctx
                .memory
                .recall(&RecallQuery {
                    text: None,
                    limit: 200,
                    memory_scope: MemoryScopeQuery::All,
                    ..Default::default()
                })
                .await?;
            if items.is_empty() {
                println!("(no memories yet — try `pai demo`)");
            }
            for s in items {
                let scope = s
                    .item
                    .conversation
                    .map(|c| format!("conv:{:.8}", c.to_string()))
                    .unwrap_or_else(|| "global".into());
                println!(
                    "  {} [{:?}|{:?}|{}] {} (conf={}, imp={})",
                    &s.item.id.to_string()[..8],
                    s.item.scope,
                    s.item.source,
                    scope,
                    s.item.content,
                    s.item.confidence,
                    s.item.importance
                );
            }
        }
        Some(MemCmd::Forget { target }) => {
            let tool = pai_tools::MemoryForget;
            let args = if uuid::Uuid::parse_str(&target).is_ok() {
                serde_json::json!({"memory_id": target})
            } else {
                serde_json::json!({"query": target})
            };
            let ctx_tool = tool_ctx(ctx);
            // CLI user is the operator — direct invocation, still audited
            // via the audit log write below.
            let out = tool.execute(args, &ctx_tool).await?;
            let mut e = pai_audit::event(AuditKind::MemoryDeleted, AuditOutcome::Ok);
            e.device = Some(ctx.agent.device);
            e.detail = out.value.clone();
            ctx.audit.record(&e)?;
            println!("{}", out.summary);
        }
        Some(MemCmd::Share { target, circle }) => {
            let mid = parse_uuid(&target, "memory")?;
            let ctx_tool = tool_ctx(ctx);
            let out = pai_tools::MemoryShare
                .execute(
                    serde_json::json!({"memory_id": mid, "circle": circle}),
                    &ctx_tool,
                )
                .await?;
            let mut e = pai_audit::event(AuditKind::MemoryWritten, AuditOutcome::Ok);
            e.device = Some(ctx.agent.device);
            e.detail = out.value.clone();
            ctx.audit.record(&e)?;
            println!("{}", out.summary);
        }
    }
    Ok(())
}

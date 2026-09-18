//! `pai policies …` — permission policy management.

use crate::ctx::Ctx;
use clap::Subcommand;
use pai_core::*;
use pai_permissions::{all_permissions, Permission};

#[derive(Subcommand)]
pub(crate) enum PoliciesCmd {
    /// Every permission and its effective policy.
    List,
    /// Set a policy: `pai policies set EMAIL_SEND ASK_USER`.
    Set { permission: String, policy: String },
}

pub(crate) fn run(cmd: PoliciesCmd, ctx: &Ctx) -> Result<()> {
    match cmd {
        PoliciesCmd::List => {
            for p in all_permissions() {
                println!(
                    "  {:<22} {:?}",
                    format!("{p:?}"),
                    ctx.agent.permissions.effective(p)
                );
            }
        }
        PoliciesCmd::Set { permission, policy } => {
            let p = serde_json::from_value::<Permission>(serde_json::json!(permission))
                .map_err(|_| Error::InvalidInput(format!("unknown permission '{permission}'")))?;
            let pol = serde_json::from_value::<ExecutionPolicy>(serde_json::json!(policy))
                .map_err(|_| Error::InvalidInput(format!("unknown policy '{policy}'")))?;
            ctx.agent.permissions.set_policy(p, pol);
            // Persist.
            let store = ctx.store.as_ref();
            store.with_conn(|c| {
                c.execute(
                    "INSERT INTO policies(permission, policy, updated_at) VALUES(?1,?2,?3)
                         ON CONFLICT(permission) DO UPDATE SET policy=excluded.policy,
                         updated_at=excluded.updated_at",
                    rusqlite::params![permission, policy, pai_storage::ts(&now()),],
                )
            })?;
            println!("{permission} → {policy}");
        }
    }
    Ok(())
}

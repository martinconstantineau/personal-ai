//! `pai circle …` — shared-memory federation scopes: create, grant to a
//! paired device, list, leave.

use crate::ctx::Base;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;
use pai_storage::Store;
use std::sync::Arc;

#[derive(Subcommand)]
pub(crate) enum CircleCmd {
    /// Create (or rejoin) a named circle — generates its key locally.
    Create { name: String },
    /// Grant circle membership to a paired device: pushes a ckg/ grant
    /// object sealed to that device's pairwise key.
    Grant {
        name: String,
        /// Target device id (see `pai pair list`).
        to: String,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// List circles this device holds keys for.
    List,
    /// Leave a circle: drops the key and re-scopes its memories to
    /// device-local. Forward-only — already-received copies on other
    /// devices are unaffected.
    Leave { name: String },
}

/// `pai circle` — shared-memory federation scopes (V3d). Runs on the
/// light path (no inference needed): create/grant/list/leave.
pub(crate) async fn run(cmd: &CircleCmd, b: &Base) -> Result<()> {
    run_circle_cmds(&b.store, b.device.id, &b.cfg, cmd).await
}

async fn run_circle_cmds(
    store: &Arc<Store>,
    device: DeviceId,
    cfg: &pai_config::Config,
    cmd: &CircleCmd,
) -> Result<()> {
    use pai_sync::{circle, crypto};
    match cmd {
        CircleCmd::Create { name } => {
            circle::create_circle(store, &cfg.data_dir, name, device)?;
            println!("circle '{name}' ready — share it: pai circle grant {name} --to <device>");
        }
        CircleCmd::Grant {
            name,
            to,
            dir,
            relay,
            token,
        } => {
            let pid = resolve_peer(store, to)?;
            let peer = pai_sync::pair::list_peers(store)?
                .into_iter()
                .find(|p| p.device_id == pid)
                .ok_or_else(|| Error::InvalidInput(format!("no paired device {to}")))?;
            let t = sync_transport(dir, relay, token)?;
            let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                Error::Sync("no vault key — pair a device first (pai pair)".into())
            })?;
            let eng =
                pai_sync::engine::SyncEngine::new(t, store.clone(), vault, device, &cfg.data_dir);
            eng.push_circle_grant(name, &peer).await?;
            println!(
                "granted circle '{name}' to {} ({:.8}) — lands on their next pull",
                peer.name,
                peer.device_id.to_string()
            );
        }
        CircleCmd::List => {
            let circles = circle::list_circles(store)?;
            if circles.is_empty() {
                println!("(no circles — `pai circle create <name>`)");
            }
            for c in circles {
                let has_key = crypto::circle_key(&cfg.data_dir, &c.name)?.is_some();
                println!(
                    "  {} (created by {:.8}, key {})",
                    c.name,
                    c.created_by,
                    if has_key { "held" } else { "MISSING" }
                );
            }
        }
        CircleCmd::Leave { name } => {
            let n = circle::leave_circle(store, &cfg.data_dir, name)?;
            println!(
                "left '{name}' — key dropped, {n} memor{} re-scoped device-local",
                if n == 1 { "y" } else { "ies" }
            );
        }
    }
    Ok(())
}

//! One module per command family: the clap subcommand enum plus its
//! handler. `run_light` covers commands that skip the inference stack;
//! `run_heavy` builds the full agent runtime first.

pub(crate) mod agent;
pub(crate) mod apps;
pub(crate) mod broker;
pub(crate) mod circle;
pub(crate) mod conversations;
pub(crate) mod docs;
pub(crate) mod email;
pub(crate) mod gitlab;
pub(crate) mod media;
pub(crate) mod memories;
pub(crate) mod mesh;
pub(crate) mod models;
pub(crate) mod notify;
pub(crate) mod pair;
pub(crate) mod policies;
pub(crate) mod runs;
pub(crate) mod serve;
pub(crate) mod sync;
pub(crate) mod tasks;
pub(crate) mod voice;
pub(crate) mod workflows;

use crate::ctx::{self, Ctx};
use crate::{Cli, Cmd};
use pai_core::*;

/// `pai pair` + `pai sync` + the other no-inference commands — store and
/// identity only, no provider probing or embedder.
pub(crate) async fn run_light(cli: &Cli) -> Result<()> {
    let b = ctx::base(cli).await?;
    match &cli.cmd {
        Cmd::Deploy { path, upgrade } => apps::deploy(path, upgrade, &b).await?,
        Cmd::Audio { cmd } => media::run_audio(cmd, &b).await?,
        Cmd::Media { cmd } => media::run(cmd, &b).await?,
        Cmd::Apps { cmd } => apps::run(cmd, &b).await?,
        Cmd::Mesh { cmd } => mesh::run(cmd, &b).await?,
        Cmd::Pair { cmd } => pair::run(cmd, &b).await?,
        Cmd::Circle { cmd } => circle::run(cmd, &b).await?,
        Cmd::Sync { cmd } => sync::run(cmd, &b).await?,
        Cmd::Broker { cmd } => broker::run(cmd, &b, cli).await?,
        Cmd::Serve {
            bind,
            dir,
            relay,
            token,
            bridge,
            bridge_token,
        } => {
            let opts = serve::ServeOpts {
                bind,
                dir,
                relay,
                token,
                bridge,
                bridge_token,
            };
            serve::run(&opts, &b, cli).await?
        }
        Cmd::Email { cmd } => email::run(cmd, &b.cfg).await?,
        Cmd::Gitlab { cmd } => gitlab::run(cmd, &b.cfg).await?,
        Cmd::Describe { image, prompt } => agent::describe(image, prompt, &b.cfg).await?,
        _ => unreachable!("run_light: not a light command"),
    }
    Ok(())
}

/// Everything that needs the agent runtime — `ctx::build` resolves the
/// provider, memory, connectors, and tool registry first.
pub(crate) async fn run_heavy(cli: Cli, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    match cli.cmd {
        Cmd::Docs { cmd } => docs::run(cmd, ctx).await?,
        Cmd::Demo => agent::demo(ctx, cfg).await?,
        Cmd::Chat {
            conversation,
            isolated,
        } => agent::chat(ctx, conversation, isolated).await?,
        Cmd::Models { cmd } => models::run(&cmd, cfg).await?,
        Cmd::Conversations { cmd } => conversations::run(cmd, ctx).await?,
        Cmd::Runs { cmd } => runs::run(cmd, ctx).await?,
        Cmd::Policies { cmd } => policies::run(cmd, ctx)?,
        Cmd::Audit { limit } => agent::audit(ctx, limit)?,
        Cmd::Memories { cmd } => memories::run(cmd, ctx).await?,
        Cmd::Voice { cmd } => voice::run(&cmd, ctx, cfg).await?,
        Cmd::Task { cmd } => tasks::run(&cmd, ctx, cfg).await?,
        Cmd::Workflow { cmd } => workflows::run(&cmd, ctx).await?,
        Cmd::Notify { cmd } => notify::run(&cmd, ctx, cfg).await?,
        // Light commands — routed away by `Cmd::is_light` before build().
        // Listing them explicitly (not `_`) means a new `Cmd` variant is a
        // compile error here until it's routed to a handler.
        Cmd::Pair { .. }
        | Cmd::Sync { .. }
        | Cmd::Broker { .. }
        | Cmd::Email { .. }
        | Cmd::Gitlab { .. }
        | Cmd::Circle { .. }
        | Cmd::Describe { .. }
        | Cmd::Deploy { .. }
        | Cmd::Apps { .. }
        | Cmd::Mesh { .. }
        | Cmd::Serve { .. }
        | Cmd::Audio { .. }
        | Cmd::Media { .. } => {
            unreachable!("handled before build")
        }
    }
    Ok(())
}

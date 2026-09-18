//! `pai conversations …` — conversation listing and lifecycle.

use crate::ctx::Ctx;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum ConvCmd {
    /// List conversations, newest first.
    List,
    /// Start a new conversation (prints its id).
    New {
        #[arg(long)]
        isolated: bool,
    },
    Rename {
        id: String,
        title: String,
    },
    Delete {
        id: String,
    },
    /// Show a conversation's transcript.
    History {
        id: String,
    },
    /// Set memory scope: shared | isolated.
    Scope {
        id: String,
        mode: String,
    },
    /// Set sync scope: synchronized | device-local (default).
    Sync {
        id: String,
        mode: String,
    },
}

pub(crate) async fn run(cmd: ConvCmd, ctx: &Ctx) -> Result<()> {
    match cmd {
        ConvCmd::List => {
            for c in ctx.conversations.list()? {
                let n = ctx.conversations.messages(c.id)?.len();
                println!(
                    "  {}  {:<9} {:<30} ({} msgs) {}",
                    &c.id.to_string()[..8],
                    match c.memory {
                        MemoryIsolation::Shared => "shared",
                        MemoryIsolation::Isolated => "isolated",
                    },
                    c.title.unwrap_or_else(|| "(untitled)".into()),
                    n,
                    c.created_at.format("%Y-%m-%d %H:%M"),
                );
            }
        }
        ConvCmd::New { isolated } => {
            let c = ctx.conversations.create(
                ctx.session,
                if isolated {
                    MemoryIsolation::Isolated
                } else {
                    MemoryIsolation::Shared
                },
            )?;
            println!("{}", c.id);
        }
        ConvCmd::Rename { id, title } => {
            ctx.conversations
                .rename(ConversationId(parse_uuid(&id, "conversation")?), &title)?;
        }
        ConvCmd::Delete { id } => {
            ctx.conversations
                .delete(ConversationId(parse_uuid(&id, "conversation")?))?;
        }
        ConvCmd::History { id } => {
            for m in ctx
                .conversations
                .messages(ConversationId(parse_uuid(&id, "conversation")?))?
            {
                let text: String = m
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(String::from))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{:>9}: {}", format!("{:?}", m.role), text);
            }
        }
        ConvCmd::Scope { id, mode } => {
            let m = match mode.as_str() {
                "isolated" => MemoryIsolation::Isolated,
                "shared" => MemoryIsolation::Shared,
                _ => return Err(Error::InvalidInput("mode: shared|isolated".into())),
            };
            ctx.conversations
                .set_memory_scope(ConversationId(parse_uuid(&id, "conversation")?), m)?;
        }
        ConvCmd::Sync { id, mode } => {
            let sc = match mode.as_str() {
                "synchronized" => SyncScope::Synchronized,
                "device-local" | "device_local" => SyncScope::DeviceLocal,
                _ => {
                    return Err(Error::InvalidInput(
                        "mode: synchronized|device-local".into(),
                    ))
                }
            };
            ctx.conversations
                .set_sync_scope(ConversationId(parse_uuid(&id, "conversation")?), sc)?;
        }
    }
    Ok(())
}

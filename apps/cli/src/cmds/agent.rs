//! Agent-facing commands: the interactive `chat` REPL, the `demo`
//! vertical slice, `describe` (vision), and the `audit` log dump.

use crate::ctx::*;
use crate::util::*;
use pai_agent::AutoApprove;
use pai_core::*;
use std::io::Write;
use std::sync::Arc;

/// `pai describe <image>` — answer a question about an image via the
/// configured vision provider (llama-server with --mmproj by default).
pub(crate) async fn describe(image: &str, prompt: &str, cfg: &pai_config::Config) -> Result<()> {
    let path = std::path::Path::new(image);
    let mime = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(pai_vision::mime_for_ext)
        .unwrap_or("image/png");
    let bytes = std::fs::read(path).map_err(|e| Error::InvalidInput(format!("{image}: {e}")))?;
    let model = cfg.inference.default_model.clone();
    let p: Arc<dyn pai_inference::ImageUnderstandingProvider> = vision_provider(cfg)
        .unwrap_or_else(|| {
            Arc::new(pai_vision::LlamaVisionProvider::new(
                &cfg.inference.local_server_url,
                model,
            ))
        });
    match p.describe(&bytes, mime, prompt).await {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("{e}");
            if p.id() == "llama-vision" {
                eprintln!(
                    "hint: serve a multimodal model — e.g. llama-server \
                             -m model.gguf --mmproj mmproj.gguf --port 8080, or \
                             configure a process adapter in vision.json"
                );
            } else {
                eprintln!(
                    "hint: check the `process` block in vision.json \
                             (command on PATH, placeholders {{image}}/{{prompt}})"
                );
            }
            return Err(e);
        }
    }
    Ok(())
}

/// `pai demo` — the vertical slice end-to-end: remember → recall → tool
/// → audit.
pub(crate) async fn demo(ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    let def = agent_def(&ctx.provider_name, ctx.model.clone());
    let conv = ctx
        .conversations
        .create(ctx.session, MemoryIsolation::Shared)?;
    println!("== Vertical slice: provider={} ==\n", ctx.provider_name);

    println!("user: Remember that I prefer local models");
    let r = send(
        ctx,
        &def,
        "Remember that I prefer local models",
        Some(conv.id),
        None,
        &AutoApprove,
    )
    .await?;
    if !r.streamed {
        println!("assistant: {}", r.answer.unwrap_or_default());
    }
    println!();

    println!("user: What do I prefer for AI models?");
    let r = send(
        ctx,
        &def,
        "What do I prefer for AI models?",
        Some(conv.id),
        None,
        &AutoApprove,
    )
    .await?;
    if !r.streamed {
        println!("assistant: {}", r.answer.unwrap_or_default());
    }
    println!();

    println!("user: What is 41 + 1?");
    let r = send(
        ctx,
        &def,
        "What is 41 + 1?",
        Some(conv.id),
        None,
        &AutoApprove,
    )
    .await?;
    if !r.streamed {
        println!("assistant: {}", r.answer.unwrap_or_default());
    }
    println!();

    println!("== Audit log (last 15) ==");
    for e in ctx.audit.recent(15)? {
        println!(
            "  {} {:?} {:?} tool={:?} outcome={:?}",
            e.at.format("%H:%M:%S"),
            e.kind,
            e.detail,
            e.tool,
            e.outcome
        );
    }
    println!("\nData dir: {}", cfg.data_dir.display());
    Ok(())
}

/// `pai chat` — interactive REPL (persistent; see `pai conversations`).
pub(crate) async fn chat(ctx: &Ctx, conversation: Option<String>, isolated: bool) -> Result<()> {
    let conv = match conversation {
        Some(id) => {
            let cid = ConversationId(parse_uuid(&id, "conversation")?);
            ctx.conversations.get(cid)?;
            cid
        }
        None => {
            ctx.conversations
                .create(
                    ctx.session,
                    if isolated {
                        MemoryIsolation::Isolated
                    } else {
                        MemoryIsolation::Shared
                    },
                )?
                .id
        }
    };
    let def = agent_def(&ctx.provider_name, ctx.model.clone());
    println!(
        "pai chat (provider={}, conversation={:.8}{}). 'quit' to exit.",
        ctx.provider_name,
        conv.to_string(),
        if isolated { ", isolated memory" } else { "" }
    );
    let stdin = std::io::stdin();
    loop {
        print!("you> ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() || line == "quit" {
            break;
        }
        match send(ctx, &def, line, Some(conv), None, &CliApproval).await {
            Ok(o) if o.streamed => println!(),
            Ok(o) => match o.answer {
                Some(a) => println!("pai> {a}"),
                None => println!("pai> (no answer)"),
            },
            Err(e) => println!("pai> error: {e}"),
        }
    }
    Ok(())
}

/// `pai audit` — dump the audit log ("what did my AI do?").
pub(crate) fn audit(ctx: &Ctx, limit: usize) -> Result<()> {
    for e in ctx.audit.recent(limit)? {
        println!(
            "{}  {:<18} tool={:<16} outcome={:?}",
            e.at.to_rfc3339(),
            format!("{:?}", e.kind),
            e.tool.unwrap_or_default(),
            e.outcome
        );
    }
    Ok(())
}

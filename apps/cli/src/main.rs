//! `pai` — Personal AI CLI. Drives the vertical slice end-to-end:
//! Flutter-quality UX is in apps/desktop; this is the same Rust core.
//!
//! Layout: `cmds/` holds one module per command family (its clap
//! subcommand enum + handler), `ctx.rs` builds the runtime context, and
//! `util.rs` has the cross-command helpers. `Cmd::is_light` splits the
//! commands that skip the inference stack (`run_light`) from those that
//! build it (`run_heavy`).

mod cmds;
mod ctx;
mod util;

use clap::{Parser, Subcommand};
use cmds::apps::AppsCmd;
use cmds::broker::BrokerCmd;
use cmds::circle::CircleCmd;
use cmds::conversations::ConvCmd;
use cmds::docs::DocsCmd;
use cmds::email::EmailCmd;
use cmds::gitlab::GitLabCmd;
use cmds::media::{AudioCmd, MediaCmd};
use cmds::memories::MemCmd;
use cmds::mesh::MeshCmd;
use cmds::models::ModelsCmd;
use cmds::notify::NotifyCmd;
use cmds::pair::PairCmd;
use cmds::policies::PoliciesCmd;
use cmds::runs::RunsCmd;
use cmds::sync::SyncCmd;
use cmds::tasks::TaskCmd;
use cmds::voice::VoiceCmd;
use cmds::workflows::WorkflowCmd;
use pai_core::Result;

#[derive(Parser)]
#[command(name = "pai", about = "Personal AI — local-first, free models only")]
struct Cli {
    /// Data directory (default: ~/.local/share/personal-ai)
    #[arg(long, global = true)]
    data_dir: Option<String>,
    /// Inference provider: echo (offline stub), llama-server, or auto
    /// (probe llama-server/Ollama/LM Studio on localhost).
    #[arg(long, global = true, default_value = "echo")]
    provider: String,
    /// Base URL for the local inference server
    #[arg(long, global = true, default_value = "http://127.0.0.1:8080")]
    server_url: String,
    /// Model slug to request from the provider
    #[arg(long, global = true)]
    model: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage the document store
    Docs {
        #[command(subcommand)]
        cmd: DocsCmd,
    },
    /// Run the vertical slice end-to-end: remember → recall → tool → audit.
    Demo,
    /// Interactive chat REPL (persistent; see `pai conversations`).
    Chat {
        /// Continue an existing conversation id.
        #[arg(long)]
        conversation: Option<String>,
        /// Isolate this conversation's memory from global recall.
        #[arg(long)]
        isolated: bool,
    },
    /// Model registry operations (catalog + Hugging Face).
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
    /// Conversation management.
    Conversations {
        #[command(subcommand)]
        cmd: ConvCmd,
    },
    /// Agent run recovery.
    Runs {
        #[command(subcommand)]
        cmd: RunsCmd,
    },
    /// Permission policy management.
    Policies {
        #[command(subcommand)]
        cmd: PoliciesCmd,
    },
    /// Dump the audit log — "what did my AI do?"
    Audit {
        #[arg(long, default_value = "50")]
        limit: usize,
    },
    /// Memory operations.
    Memories {
        #[command(subcommand)]
        cmd: Option<MemCmd>,
    },
    /// Device pairing for end-to-end encrypted sync.
    Pair {
        #[command(subcommand)]
        cmd: PairCmd,
    },
    /// Shared-memory circles — opt-in family/team scopes inside a vault.
    Circle {
        #[command(subcommand)]
        cmd: CircleCmd,
    },
    /// Cross-device sync over a shared folder.
    Sync {
        #[command(subcommand)]
        cmd: SyncCmd,
    },
    /// LAN discovery for paired devices (`pai sync serve --announce`
    /// on the other end).
    Mesh {
        #[command(subcommand)]
        cmd: MeshCmd,
    },
    /// Trusted-device compute: serve ops to paired peers or call one.
    Broker {
        #[command(subcommand)]
        cmd: BrokerCmd,
    },
    /// Stable app URLs (V5j): HTTP gateway — `GET /apps/<id>/<path>`
    /// runs the app CGI-style and streams its response. Follows
    /// `active_device`, so the same path works on every device.
    Serve {
        /// Listen address.
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: String,
        /// Shared-folder transport for forwarding to the app's device.
        #[arg(long)]
        dir: Option<String>,
        /// Relay transport (mutually exclusive with --dir).
        #[arg(long)]
        relay: Option<String>,
        /// Relay bearer token.
        #[arg(long)]
        token: Option<String>,
        /// Host a PaiRuntime in-process and answer POST /api/bridge —
        /// this is what the Flutter *web* build (the PWA) talks to.
        /// Loopback-only by default; on a non-loopback bind it requires
        /// --bridge-token.
        #[arg(long)]
        bridge: bool,
        /// Bearer token required on /api/bridge when set (mandatory for
        /// non-loopback binds). The web app passes it as
        /// `?token=` once, then keeps it in localStorage.
        #[arg(long)]
        bridge_token: Option<String>,
    },
    /// Email connector (IMAP) — configure + direct ops.
    Email {
        #[command(subcommand)]
        cmd: EmailCmd,
    },
    /// GitLab connector (REST v4) — configure + direct ops.
    Gitlab {
        #[command(subcommand)]
        cmd: GitLabCmd,
    },
    /// Voice pipeline — whisper-server STT, piper TTS, energy VAD.
    Voice {
        #[command(subcommand)]
        cmd: VoiceCmd,
    },
    /// Audio generation — text-to-music/SFX via a configured provider.
    Audio {
        #[command(subcommand)]
        cmd: AudioCmd,
    },
    /// Media generation — audio/image/video via the configured
    /// providers or a paired `media-run` device.
    Media {
        #[command(subcommand)]
        cmd: MediaCmd,
    },
    /// Background tasks — synced across devices; a due task is claimed by
    /// one device under a lease so it doesn't run everywhere at once.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Declarative multi-step workflows — prompt + tool steps with a
    /// per-workflow tool allowlist; runs persist and resume after crashes.
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
    /// Notification inbox — the proactive surface tasks/tools publish
    /// into; rows sync across paired devices.
    Notify {
        #[command(subcommand)]
        cmd: NotifyCmd,
    },
    /// Describe/answer a question about an image via a local multimodal
    /// model (llama.cpp server with --mmproj).
    Describe {
        /// Image file (png/jpg/webp/gif/bmp).
        image: String,
        /// Question or instruction about the image.
        #[arg(long, default_value = "Describe this image in detail.")]
        prompt: String,
    },
    /// Install a signed app package (see `pai apps sign` to produce one).
    /// Unsigned or unverifiable packages are refused — nothing is run.
    Deploy {
        /// Package directory containing manifest.toml.
        path: String,
        /// Replace an existing install of the same app id.
        #[arg(long)]
        upgrade: bool,
    },
    /// App package operations: sign, verify, list installed.
    Apps {
        #[command(subcommand)]
        cmd: AppsCmd,
    },
}

impl Cmd {
    /// Commands that run without the inference stack — no provider
    /// probing, no embedder, just config + store + identity.
    fn is_light(&self) -> bool {
        matches!(
            self,
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
                | Cmd::Media { .. }
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    if cli.cmd.is_light() {
        return cmds::run_light(&cli).await;
    }

    let (ctx, cfg) = ctx::build(&cli).await?;
    cmds::run_heavy(cli, &ctx, &cfg).await
}

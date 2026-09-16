//! `pai` — Personal AI CLI. Drives the vertical slice end-to-end:
//! Flutter-quality UX is in apps/desktop; this is the same Rust core.

use clap::{Parser, Subcommand};
use pai_agent::{
    AgentDefinition, AgentEvent, AgentRuntime, ApprovalHandler, AutoApprove, CancelToken,
    ConversationStore, DenyApprovals, Persistence, RunRequest, RunStore,
};
use pai_core::*;
use pai_inference::{EchoProvider, LlamaServerProvider};
use pai_memory::{Embedder, MemoryBackend, MemoryScopeQuery, RecallQuery, SqliteMemory};
use pai_permissions::{all_permissions, Permission, PolicyEngine, PolicyTable};
use pai_storage::Store;
use pai_tools::Tool;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

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
    /// Run the vertical slice end-to-end: remember → recall → tool → audit.
    /// Manage the document store
    Docs {
        #[command(subcommand)]
        cmd: DocsCmd,
    },
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
    },
    /// Email connector (IMAP) — configure + direct ops.
    Email {
        #[command(subcommand)]
        cmd: EmailCmd,
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

/// `app.user.devices` name layer: slugify a display name into the DNS
/// label the gateway recognizes (`Alice's phone` -> `alice-s-phone`).
fn name_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Host-header name route — `<app>.<user>.devices` (port ignored)
/// resolves to app id `app` when `<user>` is this device's user slug.
/// The app id may itself be dotted (`com.example.app`).
fn parse_app_name(host: &str, user_slug: &str) -> Option<String> {
    let host = host
        .split(':')
        .next()
        .unwrap_or(host)
        .trim_end_matches('.')
        .to_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 3 || *labels.last()? != "devices" {
        return None;
    }
    if labels[labels.len() - 2] != user_slug {
        return None;
    }
    let app = labels[..labels.len() - 2].join(".");
    (!app.is_empty()).then_some(app)
}

#[derive(Subcommand)]
enum AppsCmd {
    /// Scaffold a new app source project (manifest.toml + Rust wasm
    /// skeleton) — then `pai apps build` it.
    Init {
        /// App name; the app id is slugified from it.
        name: String,
        /// Parent directory (default: current dir).
        #[arg(long)]
        dir: Option<String>,
    },
    /// Build a source dir into a verifiable package: Rust crate →
    /// wasm32-wasip1, or an existing package dir → copy + validate.
    Build {
        /// Source directory (Cargo.toml and/or manifest.toml).
        dir: String,
        /// Output package dir (default: <dir>/pkg).
        #[arg(long)]
        out: Option<String>,
        /// Sign the built package with this device's key.
        #[arg(long)]
        sign: bool,
    },
    /// List installed apps.
    List,
    /// Emit hosts-file lines: `<ip> <app>.<user>.devices` for every
    /// serve-enabled app — append to /etc/hosts (or feed a Tailscale
    /// nameserver) so `http://app.user.devices` URLs resolve here.
    Names {
        /// IP the names should point at (default 127.0.0.1 — the local
        /// `pai serve` gateway). For a shared/LAN gateway pass its IP.
        #[arg(long)]
        ip: Option<String>,
    },
    /// Sign a package in place with this device's key (writes
    /// signature.bin over manifest + content digest).
    Sign {
        /// Package directory containing manifest.toml.
        path: String,
    },
    /// Verify a package's signature without installing it.
    Verify {
        /// Package directory containing manifest.toml.
        path: String,
    },
    /// Run an installed app's wasm entrypoint in the sandbox.
    Run {
        /// Installed app id (see `pai apps list`).
        id: String,
        /// Run on a paired device instead of locally — a device-id
        /// prefix, or `any` to route to a peer announcing the op.
        /// Installed apps sync to every paired device.
        #[arg(long)]
        on: Option<String>,
        /// Transport for --on: shared sync directory.
        #[arg(long)]
        dir: Option<String>,
        /// Transport for --on: relay URL.
        #[arg(long)]
        relay: Option<String>,
        /// Transport for --on: relay bearer token.
        #[arg(long)]
        token: Option<String>,
        /// Run as a guest: path to a capability token JSON file issued
        /// by `pai apps share` on the target device. The request is
        /// signed with this device's key when the grant is bound to it.
        #[arg(long)]
        cap: Option<String>,
        /// Arguments passed to the app.
        args: Vec<String>,
    },
    /// Remove an installed app.
    Remove {
        /// Installed app id.
        id: String,
    },
    /// Snapshot an installed app (package + live data) into a backup
    /// that ships to paired devices on the next `pai sync push`.
    Backup {
        /// Installed app id.
        id: String,
    },
    /// List known app backups — own snapshots and ones received from
    /// paired devices.
    Backups,
    /// Restore an app's package + data from a backup snapshot.
    Restore {
        /// Installed app id.
        id: String,
        /// Backup writer device-id prefix (default: newest backup).
        #[arg(long)]
        from: Option<String>,
    },
    /// Delete my backup for an app — ships a tombstone so paired
    /// devices drop their copy too.
    BackupDelete {
        /// Installed app id.
        id: String,
    },
    /// Move an app to a paired device: snapshot package + data into a
    /// migration-flagged backup, hand over active_device, deactivate
    /// local data. Ships on next `pai sync push`; the target restores
    /// inline on its next pull.
    Migrate {
        /// Installed app id.
        id: String,
        /// Destination device — a paired device-id prefix.
        #[arg(long)]
        to: String,
    },
    /// Rescue an app whose home device is dead/lost: claim
    /// active_device here and restore the newest backup's data.
    /// `--all --from <dev>` rescues every app that lived on that
    /// device. The claim ships on next `pai sync push`; if the old
    /// device returns, its pull parks its stale data.
    Rescue {
        /// Installed app id — or use --all --from.
        id: Option<String>,
        /// Rescue every app whose active_device is --from's device.
        #[arg(long)]
        all: bool,
        /// The dead device's id or prefix (required with --all).
        #[arg(long)]
        from: Option<String>,
    },
    /// Diagnose an installed app: install state, placement, storage,
    /// backups, share tokens, and recent audit outcomes.
    Status {
        /// Installed app id (see `pai apps list`).
        id: String,
    },
    /// Show an app's recent run logs (exit code, trap, stdout/stderr).
    Logs {
        /// Installed app id.
        id: String,
        /// How many entries to show (newest first, max 20).
        #[arg(short = 'n', long, default_value = "5")]
        limit: usize,
    },
    /// Configure an OAuth provider for an app — "add Google login".
    /// Writes the provider config (`auth.json`, synced via `app/`),
    /// runs the RFC 8628 device-authorization flow, and stores the
    /// refresh token in the OS keystore. At run time the app receives
    /// a fresh access token as `PAI_OAUTH_<PROVIDER>` — the refresh
    /// token never enters the sandbox.
    Auth {
        /// Installed app id.
        id: String,
        /// Provider preset: `google` | `microsoft` | `custom`.
        provider: Option<String>,
        /// OAuth public client_id (from your app registration).
        #[arg(long)]
        client_id: Option<String>,
        /// Scope to request — repeatable. Defaults per provider.
        #[arg(long)]
        scope: Vec<String>,
        /// Custom-IdP device-code endpoint (provider `custom`).
        #[arg(long)]
        device_url: Option<String>,
        /// Custom-IdP token endpoint (provider `custom`).
        #[arg(long)]
        token_url: Option<String>,
        /// Microsoft tenant (default `common`).
        #[arg(long)]
        tenant: Option<String>,
        /// Remove the named provider instead of adding one.
        #[arg(long)]
        remove: bool,
        /// List configured providers + local token state.
        #[arg(long)]
        status: bool,
    },
    /// Issue a capability token for an installed app — a signed,
    /// expiring grant a guest device uses with `pai apps run --cap`.
    Share {
        /// Installed app id to share.
        id: String,
        /// Action(s) granted: `exec`, `read`, `write`, `share` —
        /// repeat or comma-separate (`--action read,write`).
        #[arg(long, default_value = "exec", value_delimiter = ',')]
        action: Vec<String>,
        /// Token lifetime in days.
        #[arg(long, default_value_t = 30)]
        days: i64,
        /// Bind the grant to a paired device's id prefix — requests
        /// must then be signed by that device's key. Omit for a
        /// bearer token (anyone holding it may run the app).
        #[arg(long, name = "for")]
        for_device: Option<String>,
        /// Write the token JSON here instead of share/<app>/<id>.json.
        #[arg(long)]
        out: Option<String>,
    },
    /// Re-grant a narrower sub-token from a capability this device
    /// holds — the parent token must carry `share` and be bound to
    /// this device's key. The child embeds the parent chain.
    Delegate {
        /// Parent capability token JSON (from `pai apps share`).
        #[arg(long)]
        parent: String,
        /// Action(s) for the sub-token — must be a subset of the
        /// parent's (`--action read,exec`).
        #[arg(long, default_value = "exec", value_delimiter = ',')]
        action: Vec<String>,
        /// Sub-token lifetime in days — may not outlive the parent.
        #[arg(long)]
        days: Option<i64>,
        /// Bind the sub-token: a paired device-id prefix or a raw
        /// 64-hex Ed25519 pubkey. Omit for a bearer sub-token.
        #[arg(long, name = "for")]
        for_device: Option<String>,
        /// Write the sub-token JSON here (default: share/tokens/).
        #[arg(long)]
        out: Option<String>,
    },
    /// List capability grants this device has issued.
    Grants,
    /// Revoke a capability grant by token id — revoking a parent
    /// also kills every sub-token delegated from it.
    Revoke {
        /// Installed app id the grant belongs to.
        id: String,
        /// Token id (see `pai apps grants`).
        token: String,
    },
    /// Read a file under an installed app's `files/` or `data/` —
    /// locally, or on a host as a guest with --cap + --on.
    Read {
        /// Installed app id.
        id: String,
        /// Path relative to the app dir (e.g. data/state.txt).
        path: String,
        /// Guest capability token JSON (`share --action read`).
        #[arg(long)]
        cap: Option<String>,
        /// Host device id or paired prefix — required with --cap.
        #[arg(long)]
        on: Option<String>,
        /// Guest transport: shared sync directory.
        #[arg(long)]
        dir: Option<String>,
        /// Guest transport: relay URL.
        #[arg(long)]
        relay: Option<String>,
        /// Guest transport: relay bearer token.
        #[arg(long)]
        token: Option<String>,
        /// Write the bytes here instead of stdout.
        #[arg(long)]
        out: Option<String>,
    },
    /// Write bytes into an installed app's `data/` — locally, or on a
    /// host as a guest with --cap + --on (`share --action write`).
    Write {
        /// Installed app id.
        id: String,
        /// Path relative to the app dir; must start with data/.
        path: String,
        /// File whose bytes to write.
        #[arg(long)]
        file: Option<String>,
        /// Literal text to write.
        #[arg(long)]
        text: Option<String>,
        /// Guest capability token JSON (`share --action write`).
        #[arg(long)]
        cap: Option<String>,
        /// Host device id or paired prefix — required with --cap.
        #[arg(long)]
        on: Option<String>,
        /// Guest transport: shared sync directory.
        #[arg(long)]
        dir: Option<String>,
        /// Guest transport: relay URL.
        #[arg(long)]
        relay: Option<String>,
        /// Guest transport: relay bearer token.
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// Show the catalog + installed models (incl. hf:// installs).
    List,
    /// Download + verify + install a model — a catalog slug or an
    /// `hf://owner/repo/file.gguf` reference.
    Install {
        model: String,
        /// Install into this directory instead of the local store —
        /// e.g. a flash drive (`E:\pai-models`). The pack is
        /// self-describing: plug it into any pai device and it is
        /// adopted on scan.
        #[arg(long)]
        to: Option<String>,
    },
    /// Re-scan mounted drives for `pai-models/` packs and adopt any
    /// models found — run after plugging in a model drive.
    Scan,
    /// Remove an installed model.
    Uninstall { slug: String },
    /// Models that fit this device's hardware.
    Runnable,
    /// Probe local inference endpoints + provider binaries.
    Detect,
    /// Search Hugging Face for GGUF model repos.
    Search { query: String },
    /// List .gguf files inside a hub repo ("owner/repo").
    Files {
        repo: String,
        #[arg(long, default_value = "main")]
        revision: String,
    },
    /// Serve an installed model via a local `llama-server` binary.
    Serve {
        slug: String,
        #[arg(long, default_value = "8080")]
        port: u16,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
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

#[derive(Subcommand)]
enum WorkflowCmd {
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

#[derive(Subcommand)]
enum NotifyCmd {
    /// List notifications, newest first.
    List {
        /// Only unread rows.
        #[arg(long)]
        unread: bool,
    },
    /// Show a notification and mark it read.
    Open { id: String },
    /// Publish a notification to the inbox.
    Send {
        title: String,
        body: Option<String>,
        /// Also deliver via notify.json's external channels
        /// (email_to / webhook_url).
        #[arg(long)]
        external: bool,
    },
    /// Mark every notification read.
    Clear,
    /// Soft-delete a notification (tombstone propagates).
    Remove { id: String },
    /// Configure external channels — writes notify.json.
    Configure {
        #[arg(long)]
        email_to: Option<String>,
        #[arg(long)]
        webhook: Option<String>,
    },
    /// Deliver a test notification through the configured external
    /// channels — verifies notify.json actually reaches you.
    Test,
}

#[derive(Subcommand)]
enum ConvCmd {
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

#[derive(Subcommand)]
enum RunsCmd {
    /// Runs that never finished — crash/interrupt candidates.
    Interrupted,
    /// Resume an interrupted run from its last checkpoint.
    Resume { id: String },
    /// Mark an interrupted run failed (give up on it).
    Abandon { id: String },
}

#[derive(Subcommand)]
enum PoliciesCmd {
    /// Every permission and its effective policy.
    List,
    /// Set a policy: `pai policies set EMAIL_SEND ASK_USER`.
    Set { permission: String, policy: String },
}

#[derive(Subcommand)]
enum MemCmd {
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

#[derive(Subcommand)]
enum DocsCmd {
    /// Ingest a file (txt/md/html) into the document store.
    Ingest {
        path: String,
        /// Mark the document `synchronized` for E2EE sync.
        #[arg(long)]
        sync: bool,
    },
    /// Set sync scope: synchronized | device-local (default).
    Sync { id: String, mode: String },
    /// List ingested documents.
    List,
    /// Search document sections.
    Search { query: String },
    /// Remove a document and its sections.
    Delete { id: String },
}
#[derive(Subcommand)]
enum PairCmd {
    /// Write a signed pairing offer for another device to accept.
    Offer {
        #[arg(long)]
        out: String,
        /// Also print the offer as a QR code — the other device scans
        /// it instead of receiving the file.
        #[arg(long)]
        qr: bool,
    },
    /// Accept an offer file; writes the signed accept (carries the vault
    /// key sealed to the offerer).
    Accept {
        offer: String,
        #[arg(long)]
        out: String,
        /// Also print the accept as a QR code — the offering device
        /// scans it instead of receiving the file back.
        #[arg(long)]
        qr: bool,
    },
    /// Complete pairing from an accept file; installs the vault key.
    Complete { accept: String },
    /// List trusted peer devices.
    List,
    /// Remove a peer. Note: does NOT rotate the vault key — a removed
    /// peer may still hold it.
    Remove { id: String },
}

#[derive(Subcommand)]
enum CircleCmd {
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

#[derive(Subcommand)]
enum MeshCmd {
    /// Listen for signed LAN announcements and list the paired devices
    /// serving a sync relay right now.
    Discover {
        /// Seconds to listen.
        #[arg(long, default_value = "3")]
        timeout_secs: u64,
    },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Seal + push local changes to a shared folder or relay.
    Push {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        /// Bearer token for the relay (or PAI_SYNC_TOKEN).
        #[arg(long)]
        token: Option<String>,
        /// Find a paired peer's relay on the LAN (no --dir/--relay).
        #[arg(long)]
        lan: bool,
        /// Peer device-id prefix to pick when several announce.
        #[arg(long)]
        to: Option<String>,
    },
    /// Pull + apply remote changes from a shared folder or relay.
    Pull {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Find a paired peer's relay on the LAN (no --dir/--relay).
        #[arg(long)]
        lan: bool,
        /// Peer device-id prefix to pick when several announce.
        #[arg(long)]
        to: Option<String>,
    },
    /// Push then pull in one pass.
    Run {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Find a paired peer's relay on the LAN (no --dir/--relay).
        #[arg(long)]
        lan: bool,
        /// Peer device-id prefix to pick when several announce.
        #[arg(long)]
        to: Option<String>,
    },
    /// Rotate the vault key and push sealed rotation objects to every
    /// paired peer — they adopt on their next sync pull/run. Use after
    /// `pair remove` to actually revoke a device's access.
    Rotate {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Peers + object count at the destination.
    Status {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Find a paired peer's relay on the LAN (no --dir/--relay).
        #[arg(long)]
        lan: bool,
        /// Peer device-id prefix to pick when several announce.
        #[arg(long)]
        to: Option<String>,
    },
    /// Run a sync relay server — stores ciphertext objects under --dir.
    /// Put it behind TLS (reverse proxy) off localhost; the blobs are
    /// sealed anyway, but auth keeps it from being a free object store.
    Serve {
        #[arg(long)]
        dir: String,
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: String,
        /// Require `Authorization: Bearer <token>` (or PAI_SYNC_TOKEN).
        #[arg(long)]
        token: Option<String>,
        /// Mesh mode: broadcast a signed LAN announcement and accept
        /// peer-key bearer tokens (no --token needed). Bind 0.0.0.0 to
        /// serve other devices on the LAN.
        #[arg(long)]
        announce: bool,
    },
}

#[derive(Subcommand)]
enum BrokerCmd {
    /// List paired peer devices (potential trusted executors).
    Devices,
    /// Prefer (or deprioritize) a device when `--on any` routes a call:
    /// the weight adds to the device's announced load score. Positive
    /// prefers, negative avoids, 0 clears. Local-only — never announced.
    Prefer {
        /// Peer device id or unambiguous prefix (see `broker devices`).
        peer: String,
        /// Signed weight — e.g. 500 strongly prefers, -500 avoids.
        weight: i64,
    },
    /// Answer broker requests addressed to this device, forever.
    /// Ops are served by this device's providers: stt (whisper-server),
    /// tts (piper), infer/describe (llama-server).
    Serve {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Poll interval, seconds.
        #[arg(long, default_value = "2")]
        poll_secs: u64,
    },
    /// Send one request to a paired peer and wait for its response.
    Call {
        /// Peer device id or unambiguous prefix (see `broker devices`),
        /// or the literal `any` to route by announced capability.
        device: String,
        /// Operation: stt | tts | infer | describe.
        op: String,
        /// Stream the response — chunks print as they arrive (infer).
        #[arg(long)]
        stream: bool,
        /// UTF-8 payload (tts/infer/describe prompt) — or --file for bytes.
        #[arg(long)]
        text: Option<String>,
        /// Binary payload file (stt WAV, describe image).
        #[arg(long)]
        file: Option<String>,
        /// Write response bytes to a file instead of stdout.
        #[arg(long)]
        out: Option<String>,
        #[arg(long, default_value = "60")]
        timeout_secs: u64,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum EmailCmd {
    /// Configure the IMAP account: writes email.json; the password goes
    /// to the OS keystore (`email:<user>`), never the file.
    /// `--oauth google|microsoft` uses the device-authorization flow
    /// instead — a refresh token replaces the app password.
    Configure {
        /// OAuth2 device flow for Gmail/Outlook (needs a client_id from
        /// your own cloud app registration).
        #[arg(long)]
        oauth: Option<String>,
    },
    /// Show the configured account (never prints the password).
    Status,
    /// Search messages.
    Search {
        query: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value = "10")]
        limit: u32,
    },
    /// Read one message body by id.
    Read { id: String },
    /// Create a draft (the safe send path).
    Draft {
        #[arg(long)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        in_reply_to: Option<String>,
    },
    /// Send immediately via SMTP (needs the `smtp` block in email.json —
    /// `pai email configure` writes one). Drafts remain the default.
    Send {
        #[arg(long)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        in_reply_to: Option<String>,
    },
    /// Archive a message by id.
    Archive { id: String },
    /// Apply a label/mailbox to a message by id.
    Label { id: String, label: String },
    /// Delete a message by id.
    Delete { id: String },
}

#[derive(Subcommand)]
enum VoiceCmd {
    /// Show detected voice providers (whisper-server reachability, piper
    /// binary/model, VAD) and the effective config.
    Status,
    /// Write voice.json: whisper-server URL, piper binary + voice model.
    Configure {
        #[arg(long)]
        whisper_url: Option<String>,
        #[arg(long)]
        piper_bin: Option<String>,
        #[arg(long)]
        piper_model: Option<String>,
    },
    /// Transcribe an audio file (WAV) via whisper-server.
    Transcribe { file: String },
    /// Synthesize text to a WAV file via piper.
    Say {
        text: String,
        #[arg(long, default_value = "reply.wav")]
        out: String,
    },
    /// One conversational turn: WAV in → transcript → agent → reply WAV.
    Turn {
        /// WAV file to transcribe; omit with --mic to capture instead.
        file: Option<String>,
        /// Capture the utterance from the default microphone and play the
        /// spoken reply through the speakers.
        #[arg(long)]
        mic: bool,
        #[arg(long, default_value = "reply.wav")]
        out: String,
    },
    /// Capture one utterance from the mic (VAD-endpointed) and transcribe.
    Listen {
        /// Hard cap on capture length, seconds.
        #[arg(long, default_value = "30")]
        max_secs: u32,
        /// Stream partial transcripts — each pause-finalized segment
        /// transcribes while you keep talking.
        #[arg(long)]
        stream: bool,
    },
}

#[derive(Subcommand)]
enum AudioCmd {
    /// Show the configured audio-generation provider and reachability.
    Status,
    /// Write media.json: the audio-generation server URL.
    Configure {
        /// Base URL, e.g. http://127.0.0.1:8179 (a MusicGen/stable-audio
        /// wrapper — any server accepting POST /generate {prompt,
        /// duration_seconds} → audio bytes).
        #[arg(long)]
        audio_gen_url: Option<String>,
    },
    /// Generate audio from a text prompt → writes a WAV file.
    Gen {
        prompt: String,
        #[arg(long, default_value = "10")]
        seconds: u32,
        /// Output path; default <data_dir>/media/audio-<ts>.wav
        #[arg(long)]
        out: Option<String>,
        /// Run on a paired device (id prefix or "any" = best advertised
        /// media-run peer). Needs --dir or --relay for the transport.
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// List recent media jobs (local + executed-for-peers rows).
    Jobs,
}

#[derive(Subcommand)]
enum MediaCmd {
    /// Show configured media providers (audio/image/video) and reachability.
    Status,
    /// Write media.json: generation server URLs.
    Configure {
        /// Audio-gen base URL (POST /generate {prompt,duration_seconds}
        /// → WAV), e.g. services/media-gen or a MusicGen/stable-audio
        /// wrapper. Also PAI_AUDIO_GEN_URL.
        #[arg(long)]
        audio_gen_url: Option<String>,
        /// Image-gen base URL. Also PAI_IMAGE_GEN_URL.
        #[arg(long)]
        image_gen_url: Option<String>,
        /// Image adapter: `sdcpp` (default — stable-diffusion.cpp's
        /// OpenAI /v1/images/generations) or `onnx` (minimal
        /// POST /generate → PNG convention).
        #[arg(long)]
        image_backend: Option<String>,
        /// Video-gen base URL (async job contract: POST /generate →
        /// {job_id}, GET /jobs/{id} → {status,result_b64}).
        /// Also PAI_VIDEO_GEN_URL.
        #[arg(long)]
        video_gen_url: Option<String>,
    },
    /// Generate media from a text prompt → writes a file.
    Gen {
        /// audio | image | image-edit | upscale | video
        #[arg(long, default_value = "audio")]
        kind: String,
        prompt: String,
        /// Audio/video duration, seconds (clamped 1–300).
        #[arg(long, default_value = "10")]
        seconds: u32,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        /// Source image file (image-edit / upscale kinds).
        #[arg(long)]
        input: Option<String>,
        /// Output path; default <data_dir>/media/<kind>-<ts>.<ext>
        #[arg(long)]
        out: Option<String>,
        /// Run on a paired device (id prefix or "any" = best advertised
        /// media-run peer). Needs --dir or --relay for the transport.
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// List recent media jobs (local + executed-for-peers rows).
    Jobs,
}

struct Ctx {
    store: Arc<Store>,
    documents: Arc<pai_documents::DocumentStore>,
    email: Option<Arc<dyn pai_connector_email::EmailProvider>>,
    agent: AgentRuntime,
    memory: Arc<dyn MemoryBackend>,
    audit: Arc<pai_audit::AuditLog>,
    conversations: Arc<ConversationStore>,
    runs: Arc<RunStore>,
    session: SessionId,
    provider_name: String,
    model: Option<String>,
}

/// Config + store + identity without inference/embedder probing — used by
/// commands that don't need a model (pair, sync).
async fn base(
    cli: &Cli,
) -> Result<(
    pai_config::Config,
    Arc<Store>,
    pai_identity::IdentityStore,
    std::path::PathBuf,
    User,
    Device,
)> {
    let cfg = match &cli.data_dir {
        Some(d) => pai_config::Config {
            data_dir: d.into(),
            inference: pai_config::InferenceConfig {
                local_server_url: cli.server_url.clone(),
                ..pai_config::Config::default().inference
            },
            ..pai_config::Config::default()
        },
        None => pai_config::Config::load(None)?,
    };
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| Error::Storage(e.to_string()))?;
    let store_key = pai_identity::keystore::store_key(&cfg.data_dir);
    let store = Arc::new(Store::open(&cfg.data_dir, store_key.as_ref())?);

    // Identity: reuse existing user/device or create on first run.
    let ids = pai_identity::IdentityStore::new(store.clone());
    let key_dir = cfg.data_dir.join("keys");
    let user = match store.with_conn(|c| {
        c.query_row("SELECT id FROM users LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
    }) {
        Ok(id) => ids.get_user(UserId(uuid::Uuid::parse_str(&id).unwrap()))?,
        Err(_) => ids.create_user("local-user")?,
    };
    let device = match ids.list_devices(user.id)?.into_iter().next() {
        Some(d) => d,
        None => ids.register_device(
            user.id,
            "cli-host",
            current_platform(),
            pai_identity::probe_capabilities(),
            &key_dir,
        )?,
    };
    Ok((cfg, store, ids, key_dir, user, device))
}

async fn build(cli: &Cli) -> Result<(Ctx, pai_config::Config)> {
    let (cfg, store, _ids, _key_dir, user, device) = base(cli).await?;

    let audit = Arc::new(pai_audit::AuditLog::new(store.clone()));
    let conversations = Arc::new(ConversationStore::new(store.clone()));
    let runs = Arc::new(RunStore::new(store.clone()));
    let session = conversations.get_or_create_session(user.id, device.id)?;

    // Provider resolution. "auto" probes live endpoints (needs a runtime).
    let mut provider_name = cli.provider.clone();
    let mut model = cli.model.clone();
    let mut server_url = cfg.inference.local_server_url.clone();
    // An inference.json process adapter is an explicit on-device
    // choice — auto prefers it over probing HTTP endpoints.
    let process_adapter = pai_inference::InferenceFileConfig::load(&cfg.data_dir)?
        .and_then(|c| c.process)
        .and_then(pai_inference::ProcessInferenceProvider::detect);
    if provider_name == "auto" && process_adapter.is_some() {
        provider_name = "process".into();
        eprintln!("auto: using process adapter from inference.json");
    }
    if provider_name == "auto" {
        match pai_inference::detect_endpoints(std::time::Duration::from_secs(2)).await {
            found if !found.is_empty() => {
                let ep = &found[0];
                eprintln!("auto: using {} at {}", ep.provider, ep.base_url);
                server_url = ep.base_url.clone();
                if model.is_none() {
                    model = pai_inference::pick_chat_model(&ep.models);
                }
                provider_name = "llama-server".into();
            }
            _ => {
                eprintln!("auto: no local server found — falling back to echo");
                provider_name = "echo".into();
            }
        }
    }

    // Vector recall: probe the resolved server for an Ollama embedding
    // model (/api/tags is Ollama-only, so detection is self-gating).
    let embedder: Option<Arc<pai_memory::OllamaEmbedder>> =
        pai_memory::OllamaEmbedder::detect(&server_url, std::time::Duration::from_secs(2))
            .await
            .map(Arc::new);
    if let Some(e) = &embedder {
        eprintln!("embedder: {}", e.id());
    }
    let mut mem_impl = SqliteMemory::new(store.clone());
    let mut doc_impl = pai_documents::DocumentStore::new(store.clone());
    if let Some(e) = &embedder {
        mem_impl = mem_impl.with_embedder(e.clone());
        doc_impl = doc_impl.with_embedder(e.clone());
    }
    let memory: Arc<dyn MemoryBackend> = Arc::new(mem_impl);
    let documents = Arc::new(doc_impl);
    // Model-driven file reads are jailed to <data_dir>/inbox; user-driven
    // `pai docs ingest <path>` bypasses the jail.
    let inbox = cfg.data_dir.join("inbox");
    std::fs::create_dir_all(&inbox).ok();

    // Connectors: email provider when an account is configured.
    let email: Option<Arc<dyn pai_connector_email::EmailProvider>> =
        pai_connector_email::ImapConfig::load(&cfg.data_dir)?
            .map(|c| Arc::new(pai_connector_email::ImapProvider::new(c)) as _);

    let mut providers = pai_inference::ProviderRegistry::default();
    providers.register(Arc::new(EchoProvider));
    providers.register(Arc::new(LlamaServerProvider::new(
        &server_url,
        model
            .clone()
            .unwrap_or_else(|| cfg.inference.default_model.clone()),
    )));
    if let Some(p) = process_adapter {
        providers.register(Arc::new(p));
    }

    // Vision: a process adapter from vision.json wins when configured;
    // else llama-server (the only OpenAI-image_url provider we know).
    let vision: Option<Arc<dyn pai_inference::ImageUnderstandingProvider>> = vision_provider(&cfg)
        .or_else(|| {
            (provider_name == "llama-server").then(|| {
                Arc::new(pai_vision::LlamaVisionProvider::new(
                    &server_url,
                    model
                        .clone()
                        .unwrap_or_else(|| cfg.inference.default_model.clone()),
                )) as _
            })
        });

    let audio_gen: Option<Arc<dyn pai_inference::AudioGenerationProvider>> =
        pai_media::providers::detect(&cfg.data_dir, std::time::Duration::from_secs(2))
            .await
            .map(|p| Arc::new(p) as _);

    let agent = AgentRuntime {
        providers,
        tools: pai_tools::builtin_registry(),
        permissions: Arc::new(PolicyEngine::new(load_policies(&store))),
        memory: memory.clone(),
        audit: audit.clone(),
        max_steps: cfg.inference.max_agent_steps,
        step_timeout: std::time::Duration::from_secs(cfg.inference.request_timeout_secs),
        device: device.id,
        persistence: Some(Persistence {
            conversations: conversations.clone(),
            runs: runs.clone(),
        }),
        documents: Some(documents.clone()),
        email: email.clone(),
        vision,
        notify: Some(Arc::new(pai_notify::StoreNotifySink {
            store: store.clone(),
            config: pai_notify::load_config(&cfg.data_dir)?,
            email: email.clone(),
        })),
        apps: Some(Arc::new(pai_agent::appops::StoreAppOperator::new(
            store.clone(),
            cfg.data_dir.clone(),
            device.id,
        ))),
        audio_gen,
        media_dir: Some(cfg.data_dir.join("media")),
        allowed_roots: vec![inbox],
    };

    Ok((
        Ctx {
            agent,
            memory,
            audit,
            conversations,
            runs,
            session,
            provider_name,
            model,
            documents,
            email,
            store,
        },
        cfg,
    ))
}

fn current_platform() -> Platform {
    match std::env::consts::OS {
        "macos" => Platform::MacOs,
        "windows" => Platform::Windows,
        "ios" => Platform::Ios,
        "android" => Platform::Android,
        _ => Platform::Linux,
    }
}

/// `policies` table overlays the shipped defaults.
fn load_policies(store: &Store) -> PolicyTable {
    let mut table = PolicyTable::with_defaults();
    if let Ok(rows) = store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT permission, policy FROM policies")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }) {
        for (perm, policy) in rows {
            if let (Ok(p), Ok(pol)) = (
                serde_json::from_value::<Permission>(serde_json::json!(perm)),
                serde_json::from_value::<ExecutionPolicy>(serde_json::json!(policy)),
            ) {
                table.set(p, pol);
            }
        }
    }
    table
}

fn agent_def(provider: &str, model: Option<String>) -> AgentDefinition {
    AgentDefinition {
        name: "assistant".into(),
        description: "Default assistant".into(),
        purpose: "general assistance".into(),
        tools: vec![], // all registered
        memory_scopes: vec![MemoryScope::Semantic, MemoryScope::Episodic],
        provider: provider.into(),
        model,
    }
}

fn print_event(e: &AgentEvent) {
    match e {
        AgentEvent::RunStarted { run } => println!("  ▸ run {}", &run.to_string()[..8]),
        AgentEvent::Step { index } => println!("  ▸ step {index}"),
        AgentEvent::ToolCallRequested { tool, risk, .. } => {
            println!("  ▸ tool call: {tool} (risk: {risk})")
        }
        AgentEvent::ApprovalNeeded {
            tool,
            summary,
            permissions,
            ..
        } => {
            println!("  ▸ APPROVAL NEEDED: {tool} — {summary} [{permissions:?}]")
        }
        AgentEvent::ToolExecuted { tool, summary, .. } => {
            println!("  ▸ executed {tool}: {summary}")
        }
        AgentEvent::ToolDenied { tool, .. } => println!("  ▸ DENIED: {tool}"),
        AgentEvent::TextDelta { text } => print!("{text}"),
        AgentEvent::Done { state, .. } => println!("\n  ▸ finished: {state:?}"),
    }
}

struct SendOutcome {
    answer: Option<String>,
    /// True when the answer was already printed token-by-token.
    streamed: bool,
}

async fn send(
    ctx: &Ctx,
    def: &AgentDefinition,
    text: &str,
    conversation: Option<ConversationId>,
    resume_from: Option<AgentRunId>,
    approval: &dyn ApprovalHandler,
) -> Result<SendOutcome> {
    let streamed = Arc::new(AtomicBool::new(false));
    let flag = streamed.clone();
    let emit = move |e: AgentEvent| {
        if matches!(e, AgentEvent::TextDelta { .. }) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        print_event(&e);
    };
    let history = match conversation {
        Some(c) => ctx.conversations.messages(c).unwrap_or_default(),
        None => vec![],
    };
    let out = ctx
        .agent
        .run(RunRequest {
            definition: def,
            history,
            input: text.to_string(),
            conversation,
            approval,
            cancel: CancelToken::default(),
            emit: &emit,
            stream: true,
            resume_from,
        })
        .await?;
    Ok(SendOutcome {
        answer: out.answer,
        streamed: streamed.load(std::sync::atomic::Ordering::SeqCst),
    })
}

/// Interactive approval handler for `chat`: prints the request and reads a
/// y/n answer on stdin. Blocking inside `decide` is fine — the REPL owns
/// the loop, and a run has nothing else to do while awaiting approval.
pub struct CliApproval;

#[async_trait::async_trait]
impl ApprovalHandler for CliApproval {
    async fn decide(&self, req: &pai_permissions::ApprovalRequest) -> bool {
        println!("  ┌─ approval needed ───────────────────────────");
        println!("  │ tool:        {}", req.tool);
        println!("  │ action:      {}", req.summary);
        println!("  │ permissions: {:?}", req.permissions);
        println!("  │ risk:        {:?}", req.risk);
        print!("  └─ allow? [y/N] ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
    }
}

fn parse_uuid(s: &str, what: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|_| Error::InvalidInput(format!("invalid {what} id '{s}'")))
}

async fn run_models(cmd: &ModelsCmd, cfg: &pai_config::Config) -> Result<()> {
    let mgr = pai_models::ModelManager::new(
        Arc::new(Store::open(
            &cfg.data_dir,
            pai_identity::keystore::store_key(&cfg.data_dir).as_ref(),
        )?),
        &cfg.data_dir,
    );
    for m in pai_models::builtin_catalog() {
        mgr.register(&m)?;
    }
    match cmd {
        ModelsCmd::List => {
            let caps = pai_identity::probe_capabilities();
            for (m, installed, path) in mgr.list()? {
                let fits = pai_models::fits(&m, &caps)
                    .map(|_| "fits")
                    .unwrap_or("too large");
                println!(
                    "  {:<44} [{}MB, {}, {}] {:<10}{}",
                    m.slug,
                    m.size_bytes / 1_000_000,
                    m.quantization.clone().unwrap_or_default(),
                    m.license.clone().unwrap_or_else(|| "?".into()),
                    fits,
                    if installed {
                        match path {
                            Some(p) if p.exists() => {
                                format!("installed → {}", p.display())
                            }
                            Some(p) => {
                                format!("installed (offline — last at {})", p.display())
                            }
                            None => "installed".into(),
                        }
                    } else {
                        String::new()
                    }
                );
            }
            println!("\ninstall: pai models install <slug|hf://owner/repo/file.gguf>");
        }
        ModelsCmd::Install { model, to } => {
            let manifest = pai_models::resolve_model_arg(model).await?;
            mgr.register(&manifest)?;
            let p = match to {
                Some(d) => {
                    let dir = pai_models::pack_dir_for(std::path::Path::new(d));
                    mgr.install_to(&manifest.model.slug, &manifest, &dir)
                        .await?
                }
                None => mgr.install(&manifest.model.slug, &manifest).await?,
            };
            println!("installed: {}", p.display());
            if to.is_some() {
                println!("pack:      plug into any pai device — adopted on `pai models scan`");
            }
            println!("serve:    pai models serve {}", manifest.model.slug);
        }
        ModelsCmd::Scan => {
            let found = mgr.scan()?;
            if found.is_empty() {
                println!("no model packs found (looked for pai-models/ on mounted drives)");
            }
            for s in found {
                println!("  adopted {:<44} {}", s.slug, s.path.display());
            }
        }
        ModelsCmd::Uninstall { slug } => {
            mgr.uninstall(slug)?;
            println!("uninstalled {slug}");
        }
        ModelsCmd::Runnable => {
            let caps = pai_identity::probe_capabilities();
            for slug in mgr.runnable(&caps)? {
                println!("  {slug}");
            }
        }
        ModelsCmd::Detect => {
            println!("endpoints:");
            let eps = pai_inference::detect_endpoints(std::time::Duration::from_secs(2)).await;
            if eps.is_empty() {
                println!("  (none live)");
            }
            for ep in &eps {
                println!(
                    "  {} {} — models: {}",
                    ep.provider,
                    ep.base_url,
                    if ep.models.is_empty() {
                        "(none reported)".into()
                    } else {
                        ep.models.join(", ")
                    }
                );
            }
            println!("binaries:");
            for b in ["llama-server", "ollama", "lms"] {
                match pai_inference::find_in_path(b) {
                    Some(p) => println!("  {b:<14} {}", p.display()),
                    None => println!("  {b:<14} (not on PATH)"),
                }
            }
            if eps.is_empty() {
                println!("\nget started: pai models install <slug> && pai models serve <slug>");
            }
        }
        ModelsCmd::Search { query } => {
            let repos = pai_models::hf::HfClient::new().search(query, 15).await?;
            for r in repos {
                println!(
                    "  {:<48} ⬇ {:<10} ♥ {}",
                    r.id,
                    r.downloads.map(|d| d.to_string()).unwrap_or("?".into()),
                    r.likes.map(|l| l.to_string()).unwrap_or("?".into())
                );
            }
            println!("\nfiles: pai models files <owner/repo> — install: pai models install hf://<owner/repo>/<file.gguf>");
        }
        ModelsCmd::Files { repo, revision } => {
            let files = pai_models::hf::HfClient::new()
                .list_gguf_files(repo, revision)
                .await?;
            if files.is_empty() {
                println!("(no .gguf files in {repo}@{revision})");
            }
            for f in files {
                let size = f
                    .size
                    .map(|s| format!("{}MB", s / 1_000_000))
                    .unwrap_or_else(|| "?".into());
                println!("  {size:>8}  hf://{repo}/{}", f.path);
            }
        }
        ModelsCmd::Serve { slug, port } => {
            let path = mgr
                .locate(slug)?
                .ok_or_else(|| Error::NotFound(format!("{slug} not installed")))?;
            let bin = pai_inference::find_in_path("llama-server").ok_or_else(|| {
                Error::NotFound(
                    "llama-server not on PATH — install llama.cpp, or run Ollama/LM Studio and use --provider auto".into(),
                )
            })?;
            println!(
                "serving {} at http://127.0.0.1:{port} (ctrl-c to stop)",
                path.display()
            );
            let mut child = std::process::Command::new(&bin)
                .args([
                    "-m",
                    &path.to_string_lossy(),
                    "--port",
                    &port.to_string(),
                    "--host",
                    "127.0.0.1",
                ])
                .spawn()
                .map_err(|e| Error::Other(format!("spawn llama-server: {e}")))?;
            // Wait for readiness, then hand the process to the user.
            let url = format!("http://127.0.0.1:{port}");
            for _ in 0..60 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if pai_inference::detect_endpoints(std::time::Duration::from_millis(300))
                    .await
                    .iter()
                    .any(|e| e.base_url == url)
                {
                    println!("ready — chat with: pai chat --provider llama-server --server-url {url} --model {slug}");
                    break;
                }
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(Error::Other(format!("llama-server exited early: {status}")));
                }
            }
            let _ = child.wait();
        }
    }
    Ok(())
}

/// Resolve --dir / --relay into a transport; mutually exclusive.
fn sync_transport(
    dir: &Option<String>,
    relay: &Option<String>,
    token: &Option<String>,
) -> Result<Box<dyn pai_sync::SyncTransport>> {
    match (dir, relay) {
        (Some(d), None) => Ok(Box::new(pai_sync::FolderTransport::new(d.into())?)),
        (None, Some(r)) => Ok(Box::new(pai_sync::relay::RelayTransport::new(
            r,
            token.clone(),
        ))),
        (None, None) => Err(Error::InvalidInput("specify --dir or --relay".into())),
        (Some(_), Some(_)) => Err(Error::InvalidInput(
            "--dir and --relay are mutually exclusive".into(),
        )),
    }
}

/// Discover a paired peer's relay on the LAN and build an
/// authenticated transport to it: bearer token = hex(peer_key), the
/// pairing-derived secret — no shared token file needed.
fn lan_transport(
    to: &Option<String>,
    store: &Store,
    ids: &pai_identity::IdentityStore,
    device: &Device,
    data_dir: &std::path::Path,
) -> Result<Box<dyn pai_sync::SyncTransport>> {
    use pai_sync::crypto;
    let bind = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        pai_mesh::MULTICAST_PORT,
    );
    let sock = pai_mesh::bind_listener(bind, Some(pai_mesh::MULTICAST_GROUP))?;
    let found = pai_mesh::discover(&sock, std::time::Duration::from_secs(3));
    let paired = pai_mesh::paired_announcements(store, ids, found)?;
    let target = match to {
        Some(prefix) => paired
            .iter()
            .find(|p| p.peer.device_id.to_string().starts_with(prefix.as_str())),
        None => paired.first(),
    }
    .ok_or_else(|| {
        Error::NotFound(
            "no paired mesh peer announcing — run `pai sync serve --announce` on it".into(),
        )
    })?;
    let agree = crypto::agreement_key(device.id, data_dir)?;
    let token = pai_mesh::token_for(&agree.secret, &target.peer);
    println!(
        "mesh: {} ({}) at http://{}",
        target.peer.name, target.peer.device_id, target.relay_addr
    );
    Ok(Box::new(pai_sync::relay::RelayTransport::new(
        format!("http://{}", target.relay_addr),
        Some(token),
    )))
}

/// Match a device-id prefix against paired peers.
/// Per-device placement weight for `find_peer` — the `place_weight.<id>`
/// meta keys set by `pai broker prefer`. Missing/unparseable = 0.
fn place_weights(store: &Arc<Store>) -> Box<dyn Fn(&DeviceId) -> i64 + Send + Sync + 'static> {
    let st = store.clone();
    Box::new(move |id| {
        st.meta_get(&format!("place_weight.{id}"))
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

/// Render `payload` as a QR in the terminal — two modules per
/// character via half-blocks, 2-module quiet zone.
fn print_qr(payload: &str) -> Result<()> {
    let code = qrcode::QrCode::new(payload.as_bytes())
        .map_err(|e| Error::Other(format!("qr encode (payload {}B): {e}", payload.len())))?;
    let w = code.width();
    let colors = code.to_colors();
    let at = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= w as i32 || y >= w as i32 {
            false
        } else {
            colors[y as usize * w + x as usize] == qrcode::Color::Dark
        }
    };
    for y in ((-2)..(w as i32 + 2)).step_by(2) {
        let mut line = String::with_capacity(w + 8);
        for x in -2..(w as i32 + 2) {
            let ch = match (at(x, y), at(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            };
            line.push(ch);
        }
        println!("{line}");
    }
    Ok(())
}

fn resolve_peer(store: &Store, prefix: &str) -> Result<DeviceId> {
    use pai_sync::pair;
    let peers = pair::list_peers(store)?;
    let matches: Vec<_> = peers
        .iter()
        .filter(|p| p.device_id.to_string().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(Error::NotFound(format!(
            "no paired device matching '{prefix}' — `pai broker devices`"
        ))),
        1 => Ok(matches[0].device_id),
        _ => Err(Error::InvalidInput(format!(
            "'{prefix}' matches {} devices — be more specific",
            matches.len()
        ))),
    }
}

/// Broker op dispatch for `pai broker serve`: each op resolves through
/// whatever this device actually runs — whisper-server (stt), piper
/// (tts), llama-server (infer/describe).
/// Everything `guest_call` needs from `base()` — keeps the signature
/// under clippy's arg limit.
struct GuestCtx<'a> {
    store: &'a Store,
    ids: &'a pai_identity::IdentityStore,
    key_dir: &'a std::path::Path,
    device: &'a Device,
}

/// Guest-side request common to `apps run/read/write --cap`: load the
/// token, check it names `want_app`, resolve the target device, and
/// sign when the grant is bound to this device.
async fn guest_call(
    t: &dyn pai_sync::SyncTransport,
    cx: &GuestCtx<'_>,
    cap_path: &str,
    on: &str,
    want_app: &str,
    op: &str,
    args: &[String],
) -> Result<(DeviceId, Vec<u8>)> {
    let json = std::fs::read_to_string(cap_path)
        .map_err(|e| Error::InvalidInput(format!("{cap_path}: {e}")))?;
    let capability = pai_share::Capability::from_json(&json)
        .map_err(|e| Error::InvalidInput(format!("bad token: {e}")))?;
    if capability.app_id != want_app {
        return Err(Error::InvalidInput(format!(
            "token grants app {} not {want_app}",
            capability.app_id
        )));
    }
    // Guests can't read sealed bcap announcements — a literal device
    // id, a paired prefix, or the token's issuer (`any`) name the host.
    // For a delegated token the serving host is the ROOT issuer — the
    // child's own `issued_by` is the delegating device.
    let to = if on == "any" {
        let mut root = &capability;
        while let Some(p) = &root.parent {
            root = p;
        }
        root.issued_by
    } else {
        match uuid::Uuid::parse_str(on) {
            Ok(u) => DeviceId(u),
            Err(_) => resolve_peer(cx.store, on)?,
        }
    };
    let signer = |msg: &[u8]| {
        cx.ids
            .sign(cx.device.id, cx.key_dir, msg)
            .map_err(|e| pai_share::ShareError::InvalidInput(e.to_string()))
    };
    let resp = pai_share::guest::call_guest(
        t,
        to,
        capability,
        op,
        args,
        std::time::Duration::from_secs(120),
        Some(&signer),
    )
    .await?;
    Ok((to, resp))
}

struct BrokerOps {
    stt: Option<pai_voice::WhisperServerStt>,
    tts: Option<pai_voice::PiperTts>,
    /// Any media-generation backend reachable — the device advertises
    /// `media-run` and executes generation jobs for vault members
    /// (the op itself dispatches to audio/image/video by `kind`).
    has_media: bool,
    server_url: String,
    model: String,
    data_dir: std::path::PathBuf,
    store: Arc<Store>,
    device: DeviceId,
    /// Ops currently executing — announced as `DeviceLoad.busy` so
    /// peers can place work on the least-loaded device.
    busy: Arc<AtomicUsize>,
}

/// Decrements the busy counter when the op finishes — covers early
/// returns without restructuring `handle`'s match.
struct BusyGuard<'a>(&'a AtomicUsize);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl BrokerOps {
    fn ops(&self) -> Vec<String> {
        // app-run is announced unconditionally: installed apps sync to
        // every paired device, so any peer may serve the run.
        let mut v = vec![
            "infer".to_string(),
            "describe".to_string(),
            "app-run".to_string(),
        ];
        if self.stt.is_some() {
            v.push("stt".into());
        }
        if self.tts.is_some() {
            v.push("tts".into());
        }
        if self.has_media {
            v.push("media-run".into());
        }
        v
    }

    fn describe(&self) -> String {
        self.ops().join(", ")
    }
}

#[async_trait::async_trait]
impl pai_broker::rpc::OpHandler for BrokerOps {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        use base64::Engine as _;
        use pai_inference::{
            ImageUnderstandingProvider, InferenceProvider, SpeechToTextProvider,
            TextToSpeechProvider,
        };
        self.busy.fetch_add(1, Ordering::Relaxed);
        let _busy = BusyGuard(&self.busy);
        match op {
            "stt" => {
                let stt = self
                    .stt
                    .as_ref()
                    .ok_or_else(|| Error::Provider("no whisper-server here".into()))?;
                Ok(stt.transcribe(payload, "audio/wav").await?.into_bytes())
            }
            "tts" => {
                let tts = self
                    .tts
                    .as_ref()
                    .ok_or_else(|| Error::Provider("no piper here".into()))?;
                let text = String::from_utf8(payload.to_vec())
                    .map_err(|_| Error::InvalidInput("tts payload must be UTF-8".into()))?;
                tts.synthesize(&text, None).await
            }
            "infer" => {
                let prompt = String::from_utf8(payload.to_vec())
                    .map_err(|_| Error::InvalidInput("infer payload must be UTF-8".into()))?;
                let p = LlamaServerProvider::new(&self.server_url, self.model.clone());
                let req = pai_inference::AIRequest {
                    messages: vec![Message {
                        id: MessageId::new(),
                        conversation: ConversationId::new(),
                        role: Role::User,
                        created_at: now(),
                        content: vec![Content::Text { text: prompt }],
                        trust: TrustLevel::User,
                    }],
                    tools: vec![],
                    model: Some(self.model.clone()),
                    temperature: None,
                    max_tokens: None,
                    require_structured: false,
                };
                Ok(p.generate(&req).await?.text.into_bytes())
            }
            "app-run" => {
                #[derive(serde::Deserialize)]
                struct RunArgs {
                    id: String,
                    #[serde(default)]
                    args: Vec<String>,
                }
                let a: RunArgs = serde_json::from_slice(payload)
                    .map_err(|e| Error::InvalidInput(format!("app-run payload JSON: {e}")))?;
                if let Some(other) =
                    pai_sync::backup::active_elsewhere(&self.store, self.device, &a.id)?
                {
                    return Err(Error::InvalidInput(format!(
                        "app {} is active on {other} — not runnable here",
                        a.id
                    )));
                }
                pai_apps::app_run_op(&self.data_dir, &a.id, &a.args)
                    .await
                    .map_err(|e| Error::Other(e.to_string()))
            }
            // Stable app URLs: the gateway forwards the CGI request;
            // the manifest's `serve` flag is re-checked here (the
            // opt-in lives in the package, not the gateway).
            "app-serve" => {
                #[derive(serde::Deserialize)]
                struct ServeArgs {
                    id: String,
                    #[serde(default)]
                    args: Vec<String>,
                }
                let a: ServeArgs = serde_json::from_slice(payload)
                    .map_err(|e| Error::InvalidInput(format!("app-serve payload JSON: {e}")))?;
                pai_apps::app_serve_op(&self.data_dir, &a.id, &a.args)
                    .map_err(|e| Error::Other(e.to_string()))
            }
            // Broker-routed media generation: the requester picks this
            // device via find_peer("media-run"); execution + job record
            // live in pai_media::jobs::media_run_op.
            "media-run" => {
                pai_media::jobs::media_run_op(&self.data_dir, &self.store, self.device, payload)
                    .await
                    .map_err(|e| Error::Other(e.to_string()))
            }
            "describe" => {
                #[derive(serde::Deserialize)]
                struct DescribeArgs {
                    image_b64: String,
                    mime: String,
                    prompt: String,
                }
                let args: DescribeArgs = serde_json::from_slice(payload)
                    .map_err(|e| Error::InvalidInput(format!("describe payload JSON: {e}")))?;
                let img = base64::engine::general_purpose::STANDARD
                    .decode(&args.image_b64)
                    .map_err(|e| Error::InvalidInput(format!("describe image_b64: {e}")))?;
                let p = pai_vision::LlamaVisionProvider::new(&self.server_url, self.model.clone());
                Ok(p.describe(&img, &args.mime, &args.prompt)
                    .await?
                    .into_bytes())
            }
            other => Err(Error::InvalidInput(format!("unknown broker op {other}"))),
        }
    }

    /// Streaming infer: llama-server deltas become broker chunks.
    /// Everything else falls back to one-chunk `handle`.
    async fn handle_stream(
        &self,
        op: &str,
        payload: &[u8],
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) -> Result<()> {
        use futures::StreamExt;
        use pai_inference::InferenceProvider;
        if op != "infer" {
            let out = self.handle(op, payload).await?;
            let _ = tx.send(out).await;
            return Ok(());
        }
        self.busy.fetch_add(1, Ordering::Relaxed);
        let _busy = BusyGuard(&self.busy);
        let prompt = String::from_utf8(payload.to_vec())
            .map_err(|_| Error::InvalidInput("infer payload must be UTF-8".into()))?;
        let p = LlamaServerProvider::new(&self.server_url, self.model.clone());
        let req = pai_inference::AIRequest {
            messages: vec![Message {
                id: MessageId::new(),
                conversation: ConversationId::new(),
                role: Role::User,
                created_at: now(),
                content: vec![Content::Text { text: prompt }],
                trust: TrustLevel::User,
            }],
            tools: vec![],
            model: Some(self.model.clone()),
            temperature: None,
            max_tokens: None,
            require_structured: false,
        };
        let mut stream = p.stream(req);
        while let Some(ev) = stream.next().await {
            match ev? {
                pai_inference::StreamEvent::Delta(text) => {
                    if tx.send(text.into_bytes()).await.is_err() {
                        break;
                    }
                }
                pai_inference::StreamEvent::Done(_) => break,
                pai_inference::StreamEvent::Error(e) => {
                    return Err(Error::Provider(e));
                }
            }
        }
        Ok(())
    }
}

/// Detect this device's serveable ops (whisper/piper via voice config;
/// llama-server from inference config).
async fn broker_ops(
    cfg: &pai_config::Config,
    cli: &Cli,
    store: Arc<Store>,
    device: DeviceId,
) -> BrokerOps {
    let (stt, tts) = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2))
        .await
        .map(|v| (v.stt, v.tts))
        .unwrap_or((None, None));
    let has_media =
        pai_media::providers::any_media_backend(&cfg.data_dir, std::time::Duration::from_secs(2))
            .await;
    let model = cli
        .model
        .clone()
        .unwrap_or_else(|| cfg.inference.default_model.clone());
    BrokerOps {
        stt,
        tts,
        has_media,
        server_url: cfg.inference.local_server_url.clone(),
        model,
        data_dir: cfg.data_dir.clone(),
        store,
        device,
        busy: Default::default(),
    }
}

fn load_package(path: &str) -> Result<pai_apps::AppPackage> {
    pai_apps::AppPackage::load(std::path::Path::new(path))
        .map_err(|e| Error::InvalidInput(e.to_string()))
}

/// `pai pair` + `pai sync` — store/identity only, no inference stack.
async fn run_sync_cmds(cli: &Cli) -> Result<()> {
    use pai_sync::{crypto, engine, pair, SyncTransport};
    let (cfg, store, ids, key_dir, user, device) = base(cli).await?;
    match &cli.cmd {
        Cmd::Deploy { path, upgrade } => {
            let pkg = load_package(path)?;
            let devices = ids.list_devices(user.id)?;
            let signer = pkg
                .verify_any(&ids, &devices)
                .map_err(|e| Error::InvalidInput(e.to_string()))?;
            let signer_dev = devices
                .iter()
                .find(|d| d.id == signer)
                .expect("verify_any returned a listed device");
            let dest = pai_apps::AppRegistry::new(&cfg.data_dir)
                .install(&pkg, &ids, signer_dev, *upgrade)
                .map_err(|e| Error::Storage(e.to_string()))?;
            // Registry row: `app/<id>` sync objects are built from this
            // table — deployed packages roam to every paired device.
            let now_s = pai_storage::ts(&now());
            store.with_conn(|c| {
                c.execute(
                    "INSERT INTO apps(id, name, version, runtime, installed_at,
                        updated_at, deleted) VALUES(?1,?2,?3,?4,?5,?6,0)
                     ON CONFLICT(id) DO UPDATE SET name=excluded.name,
                        version=excluded.version, runtime=excluded.runtime,
                        updated_at=excluded.updated_at, deleted=0",
                    rusqlite::params![
                        pkg.manifest.app_id(),
                        pkg.manifest.app.name,
                        pkg.manifest.app.version,
                        format!("{:?}", pkg.manifest.app.runtime).to_lowercase(),
                        now_s,
                        now_s,
                    ],
                )?;
                Ok(())
            })?;
            let mut ev = pai_audit::event(AuditKind::AppDeployed, AuditOutcome::Ok);
            ev.device = Some(device.id);
            ev.detail = serde_json::json!({
                "app_id": pkg.manifest.app_id(),
                "version": pkg.manifest.app.version,
                "signer": signer.to_string(),
            });
            pai_audit::AuditLog::new(store.clone()).record(&ev)?;
            println!(
                "deployed {} {} ({:?}) -> {}",
                pkg.manifest.app_id(),
                pkg.manifest.app.version,
                pkg.manifest.app.runtime,
                dest.display()
            );
            println!("signed by device {:.8}", signer.to_string());
            println!("run it: `pai apps run {}`", pkg.manifest.app_id());
        }
        Cmd::Audio { cmd } => run_audio_cmds(cmd, &cfg, &store, &device).await?,
        Cmd::Media { cmd } => run_media_cmds(cmd, &cfg, &store, &device).await?,
        Cmd::Apps { cmd } => match cmd {
            AppsCmd::Init { name, dir } => {
                let base = match dir {
                    Some(d) => std::path::PathBuf::from(d),
                    None => std::env::current_dir().map_err(|e| Error::Other(e.to_string()))?,
                };
                let root = pai_apps::AppPackage::init(&base, name)
                    .map_err(|e| Error::InvalidInput(e.to_string()))?;
                println!("created {}", root.display());
                println!("next:    pai apps build {}", root.display());
            }
            AppsCmd::Build { dir, out, sign } => {
                let out_dir = out
                    .clone()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from(dir).join("pkg"));
                let pkg_dir = pai_apps::AppPackage::build(&std::path::PathBuf::from(dir), &out_dir)
                    .map_err(|e| Error::InvalidInput(e.to_string()))?;
                println!("package: {}", pkg_dir.display());
                if *sign {
                    let pkg = load_package(&pkg_dir.to_string_lossy())?;
                    pkg.sign(&ids, &device, &key_dir)
                        .map_err(|e| Error::Other(e.to_string()))?;
                    println!("signed by device {:.8}", device.id);
                }
                println!("deploy:  pai deploy {}", pkg_dir.display());
            }
            AppsCmd::Names { ip } => {
                // `app.user.devices` hosts-file lines — one per
                // serve-enabled app. Append to /etc/hosts (Linux/macOS),
                // C:\Windows\System32\drivers\etc\hosts, or the lines a
                // Tailscale/MagicDNS-style nameserver would serve.
                let ip = ip.as_deref().unwrap_or("127.0.0.1");
                let slug = name_slug(&user.display_name);
                let mut any = false;
                for (id, m) in pai_apps::AppRegistry::new(&cfg.data_dir)
                    .list()
                    .map_err(|e| Error::Storage(e.to_string()))?
                {
                    if m.app.serve {
                        println!("{ip:<15}  {id}.{slug}.devices");
                        any = true;
                    }
                }
                if !any {
                    println!("(no serve-enabled apps — mark `serve = true` in manifest.toml)");
                } else {
                    eprintln!(
                        "# append to your hosts file, then open http://<app>.{slug}.devices[:port]"
                    );
                }
            }
            AppsCmd::List => {
                let apps = pai_apps::AppRegistry::new(&cfg.data_dir)
                    .list()
                    .map_err(|e| Error::Storage(e.to_string()))?;
                if apps.is_empty() {
                    println!("(no apps installed - `pai deploy <dir>`)");
                }
                // Placement from the sync table — `apps` rows only
                // exist once the app has been synced or migrated.
                let placement: std::collections::HashMap<String, Option<String>> = store
                    .with_conn(|c| {
                        let mut s = c.prepare("SELECT id, active_device FROM apps")?;
                        let rows = s.query_map([], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                        })?;
                        rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()
                    })
                    .unwrap_or_default();
                for (id, m) in apps {
                    let where_ = match placement.get(&id).and_then(|p| p.as_deref()) {
                        Some(d) if d == device.id.to_string() => " here".to_string(),
                        Some(d) => format!(" → {:.8}", d),
                        None => " everywhere".to_string(),
                    };
                    println!(
                        "  {:<28} {:<10} {:<6} {}{}",
                        id,
                        m.app.version,
                        format!("{:?}", m.app.runtime).to_lowercase(),
                        m.app.name,
                        where_
                    );
                }
            }
            AppsCmd::Sign { path } => {
                let pkg = load_package(path)?;
                pkg.sign(&ids, &device, &key_dir)
                    .map_err(|e| Error::Other(e.to_string()))?;
                println!(
                    "signed {} with device {:.8}",
                    pkg.manifest.app_id(),
                    device.id
                );
            }
            AppsCmd::Verify { path } => {
                let pkg = load_package(path)?;
                let devices = ids.list_devices(user.id)?;
                match pkg.verify_any(&ids, &devices) {
                    Ok(signer) => println!(
                        "{} {} - signature ok (device {:.8})",
                        pkg.manifest.app_id(),
                        pkg.manifest.app.version,
                        signer.to_string()
                    ),
                    Err(e) => return Err(Error::InvalidInput(e.to_string())),
                }
            }
            AppsCmd::Run {
                id,
                on,
                dir,
                relay,
                token,
                cap,
                args,
            } if on.is_some() => {
                let dev = on.as_deref().unwrap();
                let t = sync_transport(dir, relay, token)?;
                let (to, resp) = if let Some(cap_path) = cap {
                    // Guest path: a capability token stands in for
                    // vault membership — request goes out unsealed,
                    // response seals to an ephemeral key in it.
                    let cx = GuestCtx {
                        store: &store,
                        ids: &ids,
                        key_dir: &key_dir,
                        device: &device,
                    };
                    guest_call(&*t, &cx, cap_path, dev, id, "app-run", args).await?
                } else {
                    let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                        Error::Sync("no vault key — pair a device first (pai pair)".into())
                    })?;
                    let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, device.id)
                        .with_weights(place_weights(&store));
                    let to = if dev == "any" {
                        client.find_peer("app-run").await?.ok_or_else(|| {
                            Error::NotFound(
                                "no paired device advertises app-run — `pai broker serve` running?"
                                    .into(),
                            )
                        })?
                    } else {
                        resolve_peer(&store, dev)?
                    };
                    let payload = serde_json::json!({"id": id, "args": args})
                        .to_string()
                        .into_bytes();
                    let resp = client
                        .call(to, "app-run", &payload, std::time::Duration::from_secs(120))
                        .await?;
                    (to, resp)
                };
                let v: serde_json::Value = serde_json::from_slice(&resp)
                    .map_err(|e| Error::Other(format!("bad app-run response: {e}")))?;
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD;
                for (key, err) in [("stdout_b64", false), ("stderr_b64", true)] {
                    let bytes = v[key]
                        .as_str()
                        .and_then(|x| b64.decode(x).ok())
                        .unwrap_or_default();
                    if err && !bytes.is_empty() {
                        eprint!("{}", String::from_utf8_lossy(&bytes));
                    } else if !err {
                        print!("{}", String::from_utf8_lossy(&bytes));
                    }
                }
                let mut ev = pai_audit::event(AuditKind::AppRun, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({
                    "app_id": id,
                    "on": to.to_string(),
                    "exit_code": v["exit_code"],
                });
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                let short = &to.to_string()[..8.min(to.to_string().len())];
                match v["exit_code"].as_i64() {
                    Some(c) => println!("(remote {short} — exit {c})"),
                    None => println!("(remote {short})"),
                }
            }
            AppsCmd::Run { id, args, .. } => {
                if let Some(other) = pai_sync::backup::active_elsewhere(&store, device.id, id)? {
                    return Err(Error::InvalidInput(format!(
                        "app {id} is active on {other} — run it there, or migrate it back with `pai apps migrate {id} --to <me>`"
                    )));
                }
                let out = pai_apps::run_logged(&cfg.data_dir, id, args)
                    .await
                    .map_err(|e| {
                        if e.to_string().contains("not installed") {
                            Error::NotFound(format!("app {id}"))
                        } else {
                            Error::Other(e.to_string())
                        }
                    })?;
                print!("{}", String::from_utf8_lossy(&out.stdout));
                if !out.stderr.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&out.stderr));
                }
                let mut ev = pai_audit::event(AuditKind::AppRun, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({
                    "app_id": id,
                    "exit_code": out.exit_code,
                    "fuel": out.fuel_consumed,
                });
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                if let Some(code) = out.exit_code {
                    println!("(exit {code}, fuel {})", out.fuel_consumed);
                }
            }
            AppsCmd::Remove { id } => {
                let reg = pai_apps::AppRegistry::new(&cfg.data_dir);
                if reg
                    .remove(id)
                    .map_err(|e| Error::InvalidInput(e.to_string()))?
                {
                    // Tombstone the row — the next push ships `app/<id>`
                    // as a deletion so peers remove it too.
                    store.with_conn(|c| {
                        c.execute(
                            "UPDATE apps SET deleted=1, updated_at=?2 WHERE id=?1",
                            rusqlite::params![id, pai_storage::ts(&now())],
                        )?;
                        Ok(())
                    })?;
                    let mut ev = pai_audit::event(AuditKind::AppRemoved, AuditOutcome::Ok);
                    ev.device = Some(device.id);
                    ev.detail = serde_json::json!({"app_id": id});
                    pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                    println!("removed {id}");
                } else {
                    println!("no such app: {id}");
                }
            }
            AppsCmd::Backup { id } => {
                let path = pai_sync::backup::create(&store, &cfg.data_dir, device.id, id, None)?;
                let mut ev = pai_audit::event(AuditKind::AppBackedUp, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({"app_id": id});
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                println!(
                    "backup recorded at {} — `pai sync push` ships it to paired devices",
                    path.display()
                );
            }
            AppsCmd::Backups => {
                let rows = pai_sync::backup::list(&store)?;
                if rows.is_empty() {
                    println!("(no backups — `pai apps backup <id>`)");
                }
                let me = device.id.to_string();
                for r in rows {
                    let who = if r.writer == me { "mine" } else { "peer" };
                    let state = if r.deleted { " (deleted)" } else { "" };
                    println!(
                        "  {:<28} {:.8}  {:<6} {}{}",
                        r.app_id, r.writer, who, r.created_at, state
                    );
                }
            }
            AppsCmd::Restore { id, from } => {
                let p = pai_sync::backup::restore(
                    &store,
                    &cfg.data_dir,
                    device.id,
                    id,
                    from.as_deref(),
                )?;
                let mut ev = pai_audit::event(AuditKind::AppRestored, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({
                    "app_id": id,
                    "backup_writer": p.writer,
                    "backup_created_at": p.created_at,
                });
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                println!(
                    "restored {id} from backup by {:.8} ({})",
                    p.writer, p.created_at
                );
            }
            AppsCmd::BackupDelete { id } => {
                if pai_sync::backup::delete(&store, &cfg.data_dir, device.id, id)? {
                    println!("backup deleted — tombstone ships on next push");
                } else {
                    println!("no backup of mine for {id}");
                }
            }
            AppsCmd::Migrate { id, to } => {
                let target = resolve_peer(&store, to)?;
                // Order matters: snapshot while still active here
                // (create refuses on an inactive app), then hand over
                // placement, then quiesce local data. The app/ object
                // carries active_device; the bkp/ object carries
                // migrate_to — pull applies them in that rank order.
                let pak =
                    pai_sync::backup::create(&store, &cfg.data_dir, device.id, id, Some(target))?;
                let now = pai_core::now().to_rfc3339();
                store.with_conn(|c| {
                    c.execute(
                        "UPDATE apps SET active_device=?2, updated_at=?3 WHERE id=?1",
                        rusqlite::params![id, target.to_string(), now],
                    )?;
                    Ok(())
                })?;
                let rescue = pai_apps::AppRegistry::new(&cfg.data_dir)
                    .deactivate_data(id)
                    .map_err(|e| Error::Other(e.to_string()))?;
                let mut ev = pai_audit::event(AuditKind::AppMigrated, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({
                    "app_id": id,
                    "to": target.to_string(),
                });
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                println!(
                    "migrating {id} to {:.8} — snapshot {}, local data {}",
                    target,
                    pak.display(),
                    rescue
                        .map(|p| format!("parked at {}", p.display()))
                        .unwrap_or_else(|| "was empty".into())
                );
                println!("`pai sync push` ships it; the target restores on pull");
            }
            AppsCmd::Rescue { id, all, from } => {
                let audit = |app_id: &str, outcome: &pai_sync::backup::RescueOutcome| {
                    let mut ev = pai_audit::event(AuditKind::AppRescued, AuditOutcome::Ok);
                    ev.device = Some(device.id);
                    ev.detail = serde_json::json!({
                        "app_id": app_id,
                        "outcome": format!("{outcome:?}"),
                    });
                    let _ = pai_audit::AuditLog::new(store.clone()).record(&ev);
                };
                if *all {
                    let from_id = resolve_peer(
                        &store,
                        from.as_deref().ok_or_else(|| {
                            Error::InvalidInput("--all needs --from <device>".into())
                        })?,
                    )?;
                    let results =
                        pai_sync::backup::rescue_from(&store, &cfg.data_dir, device.id, from_id)?;
                    if results.is_empty() {
                        println!("(no apps were active on {from_id})");
                    }
                    for (app_id, outcome) in &results {
                        audit(app_id, outcome);
                        match outcome {
                            pai_sync::backup::RescueOutcome::Restored { writer, created_at } => {
                                println!(
                                    "  {app_id}: claimed + restored from {:.8} ({created_at})",
                                    writer
                                )
                            }
                            pai_sync::backup::RescueOutcome::ClaimedNoBackup => {
                                println!("  {app_id}: claimed — no backup; fresh install only")
                            }
                            pai_sync::backup::RescueOutcome::ClaimedRestoreFailed(e) => {
                                println!("  {app_id}: claimed — restore failed: {e}")
                            }
                        }
                    }
                } else {
                    let id = id.as_deref().ok_or_else(|| {
                        Error::InvalidInput("pass an app id, or --all --from <device>".into())
                    })?;
                    let outcome = pai_sync::backup::rescue(&store, &cfg.data_dir, device.id, id)?;
                    audit(id, &outcome);
                    match outcome {
                        pai_sync::backup::RescueOutcome::Restored {
                            writer,
                            created_at,
                        } => println!(
                            "rescued {id}: placement claimed, data restored from {:.8} ({created_at})",
                            writer
                        ),
                        pai_sync::backup::RescueOutcome::ClaimedNoBackup => println!(
                            "rescued {id}: placement claimed — no backup found; fresh install only"
                        ),
                        pai_sync::backup::RescueOutcome::ClaimedRestoreFailed(e) => {
                            println!("rescued {id}: placement claimed — restore failed: {e}")
                        }
                    }
                }
                println!("claim ships on next `pai sync push`");
            }
            AppsCmd::Status { id } => {
                let ops = pai_agent::appops::StoreAppOperator::new(
                    store.clone(),
                    cfg.data_dir.clone(),
                    device.id,
                );
                use pai_tools::AppOperator as _;
                let v = ops.status(id)?;
                if !v["installed"].as_bool().unwrap_or(false) {
                    println!("{id}: not installed here");
                }
                let p = &v["placement"];
                let placed = p["device_id"].as_str();
                match placed {
                    None => println!("placement: unplaced (local instance)"),
                    Some(d) if p["this_device"].as_bool() == Some(true) => {
                        println!("placement: this device ({:.8})", d)
                    }
                    Some(d) => println!(
                        "placement: {} ({:.8}){}",
                        p["device_name"].as_str().unwrap_or("?"),
                        d,
                        if p["paired"].as_bool() == Some(true) {
                            ""
                        } else {
                            " (not a paired peer — stale claim?)"
                        }
                    ),
                }
                let st = &v["storage"];
                println!(
                    "storage: {} ({} bytes)",
                    if st["data_present"].as_bool() == Some(true) {
                        "data present"
                    } else {
                        "no data dir"
                    },
                    st["data_bytes"].as_u64().unwrap_or(0)
                );
                println!(
                    "backups: {} (newest {})",
                    v["backups"]["count"].as_u64().unwrap_or(0),
                    v["backups"]["newest"].as_str().unwrap_or("none")
                );
                println!(
                    "shares: {} active, {} expired, {} revoked",
                    v["shares"]["active"].as_u64().unwrap_or(0),
                    v["shares"]["expired"].as_u64().unwrap_or(0),
                    v["shares"]["revoked"].as_u64().unwrap_or(0)
                );
                let lg = &v["logs"];
                print!("run logs: {}", lg["count"].as_u64().unwrap_or(0));
                match lg["last_at"].as_str() {
                    None => println!(),
                    Some(at) => {
                        let last = match lg["last_trap"].as_str() {
                            Some(t) => format!("last {at} — trap: {t}"),
                            None => format!(
                                "last {at} — exit {}",
                                lg["last_exit_code"]
                                    .as_i64()
                                    .map(|c| c.to_string())
                                    .unwrap_or_else(|| "ok".into())
                            ),
                        };
                        println!(" ({last})");
                    }
                }
                let events = v["recent_events"].as_array().cloned().unwrap_or_default();
                if events.is_empty() {
                    println!("recent events: none");
                } else {
                    println!("recent events:");
                    for e in events {
                        println!(
                            "  {} {} — {}",
                            e["at"].as_str().unwrap_or("?"),
                            e["kind"].as_str().unwrap_or("?"),
                            e["outcome"].as_str().unwrap_or("?")
                        );
                    }
                }
            }
            AppsCmd::Logs { id, limit } => {
                let entries = pai_apps::logs::tail(&cfg.data_dir, id, (*limit).min(20));
                if entries.is_empty() {
                    println!("{id}: no run logs (apps/<id>/logs/ is empty — has it run?)");
                }
                for e in entries {
                    let outcome = match (e.trap, e.exit_code) {
                        (Some(t), _) => format!("trap: {t}"),
                        (None, Some(c)) => format!("exit {c}"),
                        (None, None) => "ok".into(),
                    };
                    println!("── {} · {outcome} · fuel {}", e.at, e.fuel);
                    if !e.stdout.is_empty() {
                        print!("{}", e.stdout);
                        if !e.stdout.ends_with('\n') {
                            println!();
                        }
                    }
                    if !e.stderr.is_empty() {
                        eprintln!("--- stderr ---\n{}", e.stderr);
                    }
                }
            }
            AppsCmd::Auth {
                id,
                provider,
                client_id,
                scope,
                device_url,
                token_url,
                tenant,
                remove,
                status,
            } => {
                use pai_apps::auth::AppAuth;
                // Config changes require the package installed locally.
                if pai_apps::AppRegistry::new(&cfg.data_dir)
                    .get(id)
                    .map_err(|e| Error::Other(e.to_string()))?
                    .is_none()
                    && !*status
                {
                    return Err(Error::InvalidInput(format!(
                        "app {id} not installed — `pai apps deploy` it first"
                    )));
                }
                let mut auth =
                    AppAuth::load(&cfg.data_dir, id).map_err(|e| Error::Other(e.to_string()))?;
                if *status {
                    if auth.providers.is_empty() {
                        println!("{id}: no oauth providers configured");
                    }
                    for (name, c) in &auth.providers {
                        let token = if pai_apps::auth::has_token(&cfg.data_dir, id, name) {
                            "stored"
                        } else {
                            "missing — run `pai apps auth` on this device"
                        };
                        println!(
                            "{name}: provider={} client_id={} env={} token={token}",
                            c.provider,
                            c.client_id,
                            pai_apps::auth::env_name(name)
                        );
                    }
                } else if *remove {
                    let name = provider.as_deref().ok_or_else(|| {
                        Error::InvalidInput("apps auth --remove needs a provider".into())
                    })?;
                    if auth.providers.remove(name).is_none() {
                        return Err(Error::NotFound(format!(
                            "no oauth provider '{name}' configured for {id}"
                        )));
                    }
                    auth.save(&cfg.data_dir, id)
                        .map_err(|e| Error::Other(e.to_string()))?;
                    println!("{id}: removed oauth provider '{name}' (syncs on next push)");
                } else {
                    let name = provider.as_deref().ok_or_else(|| {
                        Error::InvalidInput(
                            "apps auth needs a provider — google|microsoft|custom".into(),
                        )
                    })?;
                    let oc = pai_oauth::OAuthConfig {
                        provider: name.into(),
                        client_id: client_id.clone().ok_or_else(|| {
                            Error::InvalidInput(
                                "--client-id required — register an oauth app first".into(),
                            )
                        })?,
                        tenant: tenant.clone(),
                        device_url: device_url.clone(),
                        token_url: token_url.clone(),
                        scopes: (!scope.is_empty()).then_some(scope.clone()),
                    };
                    auth.providers.insert(name.into(), oc.clone());
                    auth.save(&cfg.data_dir, id)
                        .map_err(|e| Error::Other(e.to_string()))?;
                    // Device-authorization flow: print the code, poll
                    // until the user authorizes (or the grant expires).
                    let grant = pai_oauth::device_flow(&oc).await?;
                    println!();
                    println!("Go to {}", grant.verification_uri);
                    if let Some(u) = &grant.verification_uri_complete {
                        println!("  (or directly: {u})");
                    }
                    println!("and enter code: {}", grant.user_code);
                    let mut interval = grant.interval.max(1);
                    let deadline = std::time::Instant::now()
                        + std::time::Duration::from_secs(grant.expires_in.max(120));
                    let mut consecutive_pendings = 0u32;
                    let tokens = loop {
                        if std::time::Instant::now() > deadline {
                            return Err(Error::InvalidInput(
                                "device grant expired — re-run `pai apps auth`".into(),
                            ));
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                        match pai_oauth::poll_token_once(&oc, &grant.device_code).await? {
                            pai_oauth::Poll::Pending => {
                                consecutive_pendings += 1;
                                if consecutive_pendings == 5 {
                                    interval += 5; // back off gently
                                }
                                print!(".");
                                std::io::Write::flush(&mut std::io::stdout()).ok();
                            }
                            pai_oauth::Poll::Granted(t) => break t,
                        }
                    };
                    let refresh = tokens.refresh_token.ok_or_else(|| {
                        Error::Provider(
                            "oauth grant returned no refresh_token — add `offline_access` scope"
                                .into(),
                        )
                    })?;
                    if !pai_apps::auth::store_refresh_token(&cfg.data_dir, id, name, &refresh) {
                        return Err(Error::Other(
                            "cannot persist the refresh token — keystore and file fallback both failed".into(),
                        ));
                    }
                    let mut ev = pai_audit::event(AuditKind::AppAuthConfigured, AuditOutcome::Ok);
                    ev.detail = serde_json::json!({
                        "app_id": id,
                        "provider": name,
                        "env": pai_apps::auth::env_name(name),
                    });
                    pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                    let env_var = pai_apps::auth::env_name(name);
                    println!("{id}: {name} authorized — token stored; the app sees {env_var} at run time");
                }
            }
            AppsCmd::Share {
                id,
                action,
                days,
                for_device,
                out,
            } => {
                let reg = pai_apps::AppRegistry::new(&cfg.data_dir);
                if reg
                    .get(id)
                    .map_err(|e| Error::InvalidInput(e.to_string()))?
                    .is_none()
                {
                    return Err(Error::NotFound(format!(
                        "app {id} — `pai apps deploy` it first"
                    )));
                }
                let mut actions = Vec::new();
                for a in action {
                    actions.push(match a.as_str() {
                        "exec" => pai_share::Action::Exec,
                        "read" => pai_share::Action::Read,
                        "write" => pai_share::Action::Write,
                        "share" => pai_share::Action::Share,
                        other => {
                            return Err(Error::InvalidInput(format!(
                                "unknown action '{other}' — exec|read|write|share"
                            )))
                        }
                    });
                }
                // --for binds the grant to a peer's device key: guest
                // requests must then arrive signed by that key.
                let grantee_key = match for_device {
                    Some(prefix) => {
                        let peers = pai_sync::pair::list_peers(&store)?;
                        let m: Vec<_> = peers
                            .iter()
                            .filter(|p| p.device_id.to_string().starts_with(prefix.as_str()))
                            .collect();
                        match m.len() {
                            0 => {
                                return Err(Error::NotFound(format!(
                                    "no paired device matching '{prefix}'"
                                )))
                            }
                            1 => Some(m[0].ed_pubkey),
                            n => {
                                return Err(Error::InvalidInput(format!(
                                    "'{prefix}' matches {n} devices — be more specific"
                                )))
                            }
                        }
                    }
                    None => None,
                };
                let shares = pai_share::ShareStore::new(&cfg.data_dir);
                let mut spec = pai_share::GrantSpec::for_app(id.clone(), actions.clone());
                spec.grantee_key = grantee_key;
                spec.expires = Some((pai_core::now() + chrono::Duration::days(*days)).timestamp());
                let cap = shares
                    .grant(&ids, &key_dir, device.id, spec)
                    .map_err(|e| Error::Other(e.to_string()))?;
                let json = cap.to_json().map_err(|e| Error::Other(e.to_string()))?;
                let path = match out {
                    Some(p) => std::path::PathBuf::from(p),
                    None => cfg
                        .data_dir
                        .join("share")
                        .join("tokens")
                        .join(format!("{}-{}.json", cap.app_id, cap.token_id)),
                };
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&path, &json)
                    .map_err(|e| Error::Storage(format!("{path:?}: {e}")))?;
                let mut ev = pai_audit::event(AuditKind::AppShared, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({
                    "app_id": id,
                    "token_id": cap.token_id,
                    "actions": actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                    "bound": cap.grantee_key.is_some(),
                    "expires": cap.expires,
                });
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                println!("wrote {}", path.display());
                println!(
                    "token {} — {} on {}{} — expires {}",
                    &cap.token_id[..8.min(cap.token_id.len())],
                    actions
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                    cap.app_id,
                    if cap.grantee_key.is_some() {
                        " (bound to grantee)"
                    } else {
                        " (bearer — anyone holding it may run the app)"
                    },
                    chrono::DateTime::from_timestamp(cap.expires.unwrap_or(0), 0)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_else(|| "never".into())
                );
                println!(
                    "guest runs it with: pai apps run {id} --on {} --cap <file>",
                    device.id
                );
            }
            AppsCmd::Delegate {
                parent,
                action,
                days,
                for_device,
                out,
            } => {
                let raw = std::fs::read_to_string(parent)
                    .map_err(|e| Error::InvalidInput(format!("{parent}: {e}")))?;
                let parent_cap = pai_share::Capability::from_json(&raw)
                    .map_err(|e| Error::InvalidInput(format!("bad parent token: {e}")))?;
                let mut actions = Vec::new();
                for a in action {
                    actions.push(match a.as_str() {
                        "exec" => pai_share::Action::Exec,
                        "read" => pai_share::Action::Read,
                        "write" => pai_share::Action::Write,
                        "share" => pai_share::Action::Share,
                        other => {
                            return Err(Error::InvalidInput(format!(
                                "unknown action '{other}' — exec|read|write|share"
                            )))
                        }
                    });
                }
                // The chain only verifies when this device's key is the
                // key the parent was bound to — fail early otherwise.
                let my_key = hex::encode(&device.public_key);
                if parent_cap.grantee_key.as_deref() != Some(my_key.as_str()) {
                    return Err(Error::InvalidInput(
                        "parent token isn't bound to this device's key".into(),
                    ));
                }
                let grantee_key = match for_device {
                    Some(target) => match hex::decode(target) {
                        Ok(v) if v.len() == 32 => Some(<[u8; 32]>::try_from(v.as_slice()).unwrap()),
                        Ok(_) => {
                            return Err(Error::InvalidInput(
                                "--for hex key must be 32 bytes (64 hex chars)".into(),
                            ))
                        }
                        Err(_) => Some(
                            pai_sync::pair::list_peers(&store)?
                                .iter()
                                .find(|p| p.device_id.to_string().starts_with(target.as_str()))
                                .map(|p| p.ed_pubkey)
                                .ok_or_else(|| {
                                    Error::NotFound(format!("no paired device matching '{target}'"))
                                })?,
                        ),
                    },
                    None => None,
                };
                let shares = pai_share::ShareStore::new(&cfg.data_dir);
                let mut spec =
                    pai_share::GrantSpec::for_app(parent_cap.app_id.clone(), actions.clone());
                spec.grantee_key = grantee_key;
                spec.device = parent_cap.device;
                spec.expires =
                    days.map(|d| (pai_core::now() + chrono::Duration::days(d)).timestamp());
                let cap = shares
                    .delegate(&ids, &key_dir, device.id, &parent_cap, spec)
                    .map_err(|e| Error::Other(e.to_string()))?;
                let json = cap.to_json().map_err(|e| Error::Other(e.to_string()))?;
                let path = match out {
                    Some(p) => std::path::PathBuf::from(p),
                    None => cfg
                        .data_dir
                        .join("share")
                        .join("tokens")
                        .join(format!("{}-{}.json", cap.app_id, cap.token_id)),
                };
                if let Some(p) = path.parent() {
                    std::fs::create_dir_all(p).ok();
                }
                std::fs::write(&path, &json)
                    .map_err(|e| Error::Storage(format!("{path:?}: {e}")))?;
                let mut ev = pai_audit::event(AuditKind::AppShared, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({
                    "app_id": cap.app_id,
                    "token_id": cap.token_id,
                    "parent": parent_cap.token_id,
                    "delegated": true,
                    "actions": actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                    "bound": cap.grantee_key.is_some(),
                    "expires": cap.expires,
                });
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                println!("wrote {}", path.display());
                println!(
                    "sub-token {} — {} on {} — delegated from {}",
                    &cap.token_id[..8.min(cap.token_id.len())],
                    actions
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                    cap.app_id,
                    &parent_cap.token_id[..8.min(parent_cap.token_id.len())],
                );
            }
            AppsCmd::Grants => {
                let shares = pai_share::ShareStore::new(&cfg.data_dir);
                let list = shares.list().map_err(|e| Error::Other(e.to_string()))?;
                if list.is_empty() {
                    println!("no capability grants issued");
                }
                for (cap, status) in list {
                    let actions = cap
                        .actions
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let exp = cap
                        .expires
                        .and_then(|e| chrono::DateTime::from_timestamp(e, 0))
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_else(|| "never".into());
                    let parent = cap
                        .parent
                        .as_ref()
                        .map(|p| format!("  ↳ {:.8}", p.token_id))
                        .unwrap_or_default();
                    println!(
                        "{}  {:<24} {:<10} {:<8} exp {}  {}{}",
                        cap.token_id,
                        cap.app_id,
                        actions,
                        format!("{status:?}").to_lowercase(),
                        exp,
                        if cap.grantee_key.is_some() {
                            "bound"
                        } else {
                            "bearer"
                        },
                        parent,
                    );
                }
            }
            AppsCmd::Revoke { id, token } => {
                let shares = pai_share::ShareStore::new(&cfg.data_dir);
                let list = shares.list().map_err(|e| Error::Other(e.to_string()))?;
                match list.iter().find(|(c, _)| c.token_id == *token) {
                    None => {
                        return Err(Error::NotFound(format!(
                            "no grant {token} — `pai apps grants`"
                        )))
                    }
                    Some((c, _)) if c.app_id != *id => {
                        return Err(Error::InvalidInput(format!(
                            "token {token} grants app {} not {id}",
                            c.app_id
                        )))
                    }
                    _ => {}
                }
                shares
                    .revoke(token)
                    .map_err(|e| Error::Other(e.to_string()))?;
                let mut ev = pai_audit::event(AuditKind::AppShareRevoked, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({"app_id": id, "token_id": token});
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                println!("revoked {token} — guests holding it are refused on next request");
            }
            AppsCmd::Read {
                id,
                path,
                cap,
                on,
                dir,
                relay,
                token,
                out,
            } => {
                use base64::Engine as _;
                let args = vec![path.clone()];
                let resp = match cap {
                    Some(c) => {
                        let dev = on.as_deref().ok_or_else(|| {
                            Error::InvalidInput("--cap needs --on <device|any>".into())
                        })?;
                        let t = sync_transport(dir, relay, token)?;
                        let cx = GuestCtx {
                            store: &store,
                            ids: &ids,
                            key_dir: &key_dir,
                            device: &device,
                        };
                        guest_call(&*t, &cx, c, dev, id, "app-read", &args).await?.1
                    }
                    None => pai_apps::app_read_op(&cfg.data_dir, id, &args)
                        .map_err(|e| Error::InvalidInput(e.to_string()))?,
                };
                let v: serde_json::Value = serde_json::from_slice(&resp)
                    .map_err(|e| Error::Other(format!("bad app-read response: {e}")))?;
                let bytes = v["data_b64"]
                    .as_str()
                    .and_then(|x| base64::engine::general_purpose::STANDARD.decode(x).ok())
                    .ok_or_else(|| Error::Other("app-read returned no data_b64".into()))?;
                match out {
                    Some(f) => {
                        std::fs::write(f, &bytes)
                            .map_err(|e| Error::Storage(format!("{f}: {e}")))?;
                        println!("wrote {} ({} bytes)", f, bytes.len());
                    }
                    None => {
                        use std::io::Write as _;
                        std::io::stdout().write_all(&bytes).ok();
                        println!();
                    }
                }
            }
            AppsCmd::Write {
                id,
                path,
                file,
                text,
                cap,
                on,
                dir,
                relay,
                token,
            } => {
                use base64::Engine as _;
                let data = match (file, text) {
                    (Some(f), None) => {
                        std::fs::read(f).map_err(|e| Error::InvalidInput(format!("{f}: {e}")))?
                    }
                    (None, Some(t)) => t.clone().into_bytes(),
                    _ => {
                        return Err(Error::InvalidInput(
                            "pass --file <path> or --text <s>".into(),
                        ))
                    }
                };
                let args = vec![
                    path.clone(),
                    base64::engine::general_purpose::STANDARD.encode(&data),
                ];
                match cap {
                    Some(c) => {
                        let dev = on.as_deref().ok_or_else(|| {
                            Error::InvalidInput("--cap needs --on <device|any>".into())
                        })?;
                        let t = sync_transport(dir, relay, token)?;
                        let cx = GuestCtx {
                            store: &store,
                            ids: &ids,
                            key_dir: &key_dir,
                            device: &device,
                        };
                        guest_call(&*t, &cx, c, dev, id, "app-write", &args).await?;
                    }
                    None => {
                        pai_apps::app_write_op(&cfg.data_dir, id, &args)
                            .map_err(|e| Error::InvalidInput(e.to_string()))?;
                    }
                }
                let mut ev = pai_audit::event(AuditKind::AppRun, AuditOutcome::Ok);
                ev.device = Some(device.id);
                ev.detail = serde_json::json!({
                    "app_id": id,
                    "guest_write": path,
                    "bytes": data.len(),
                });
                pai_audit::AuditLog::new(store.clone()).record(&ev)?;
                println!("wrote {} bytes to {id}:{path}", data.len());
            }
        },
        Cmd::Mesh { cmd } => match cmd {
            MeshCmd::Discover { timeout_secs } => {
                let bind = std::net::SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    pai_mesh::MULTICAST_PORT,
                );
                let sock = pai_mesh::bind_listener(bind, Some(pai_mesh::MULTICAST_GROUP))?;
                let found =
                    pai_mesh::discover(&sock, std::time::Duration::from_secs(*timeout_secs));
                let paired = pai_mesh::paired_announcements(&store, &ids, found.clone())?;
                for p in &paired {
                    println!(
                        "  {}  {:<20} {:<10} relay http://{}",
                        p.peer.device_id, p.peer.name, p.peer.platform, p.relay_addr
                    );
                }
                println!(
                    "{} paired device(s) announcing ({} datagram(s) ignored)",
                    paired.len(),
                    found.len() - paired.len()
                );
                if !paired.is_empty() {
                    println!("sync now: `pai sync run --lan`");
                }
            }
        },
        Cmd::Pair { cmd } => match cmd {
            PairCmd::Offer { out, qr } => {
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let m = pair::make_offer(&device, &agree, &ids, &key_dir)?;
                pair::write_message(&m, std::path::Path::new(out))?;
                println!("offer for '{}' written to {out}", device.name);
                if *qr {
                    let payload =
                        serde_json::to_string(&m).map_err(|e| Error::Sync(e.to_string()))?;
                    print_qr(&payload)?;
                    println!("scan me with the other device's camera");
                } else {
                    println!("send it to the other device: pai pair accept {out} --out accept.pai");
                }
            }
            PairCmd::Accept { offer, out, qr } => {
                let offer = pair::read_message(std::path::Path::new(offer))?;
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let m = pair::accept_offer(
                    &store,
                    &offer,
                    &device,
                    &agree,
                    &ids,
                    &key_dir,
                    &cfg.data_dir,
                )?;
                pair::write_message(&m, std::path::Path::new(out))?;
                println!(
                    "paired with '{}' ({}…); accept written to {out}",
                    offer.name,
                    &offer.device_id[..8.min(offer.device_id.len())]
                );
                if *qr {
                    let payload =
                        serde_json::to_string(&m).map_err(|e| Error::Sync(e.to_string()))?;
                    print_qr(&payload)?;
                    println!("scan me with the offering device");
                } else {
                    println!("return it to the offering device: pai pair complete {out}");
                }
            }
            PairCmd::Complete { accept } => {
                let accept = pair::read_message(std::path::Path::new(accept))?;
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                pair::complete_pairing(&store, &accept, &agree, &cfg.data_dir)?;
                println!("paired with '{}' — vault key installed", accept.name);
            }
            PairCmd::List => {
                let peers = pair::list_peers(&store)?;
                if peers.is_empty() {
                    println!("no paired devices — see `pai pair offer`");
                }
                for p in peers {
                    println!(
                        "  {}  {:<16} {:<10} paired {}",
                        &p.device_id.to_string()[..8],
                        p.name,
                        p.platform,
                        p.paired_at.format("%Y-%m-%d %H:%M")
                    );
                }
            }
            PairCmd::Remove { id } => {
                if pair::remove_peer(&store, DeviceId(parse_uuid(id, "device")?))? {
                    println!(
                        "removed peer {id} — run `pai sync rotate` to revoke \
                         their access (they keep a stale vault key until then)"
                    );
                } else {
                    println!("no such peer: {id}");
                }
            }
        },
        Cmd::Circle { cmd } => run_circle_cmds(&store, device.id, &cfg, cmd).await?,
        Cmd::Sync { cmd } => match cmd {
            SyncCmd::Push {
                dir,
                relay,
                token,
                lan,
                to,
            }
            | SyncCmd::Pull {
                dir,
                relay,
                token,
                lan,
                to,
            }
            | SyncCmd::Run {
                dir,
                relay,
                token,
                lan,
                to,
            } => {
                let t = if *lan {
                    lan_transport(to, &store, &ids, &device, &cfg.data_dir)?
                } else {
                    sync_transport(dir, relay, token)?
                };
                let kind = t.id().to_string();
                // Adopt any pending vault rotation first — objects sealed
                // under the new vault need the new key before pull.
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let adopted = pai_sync::rotate::adopt_rotations(
                    &*t,
                    &store,
                    &cfg.data_dir,
                    &agree,
                    &device,
                    &ids,
                    &key_dir,
                )
                .await?;
                if adopted > 0 {
                    println!(
                        "adopted vault rotation (epoch {})",
                        pai_sync::rotate::vault_epoch(&cfg.data_dir)
                    );
                }
                let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                    Error::Sync("no vault key — pair a device first (pai pair)".into())
                })?;
                let eng =
                    engine::SyncEngine::new(t, store.clone(), vault, device.id, &cfg.data_dir);
                let out = match cmd {
                    SyncCmd::Push { .. } => eng.push().await?,
                    SyncCmd::Pull { .. } => eng.pull().await?,
                    _ => eng.run().await?,
                };
                println!(
                    "sync via {kind}: pushed {}, pulled {}, skipped {}",
                    out.pushed, out.pulled, out.skipped
                );
            }
            SyncCmd::Rotate { dir, relay, token } => {
                let t = sync_transport(dir, relay, token)?;
                let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                let n = pai_sync::rotate::push_rotation(
                    &*t,
                    &store,
                    &cfg.data_dir,
                    &device,
                    &agree,
                    &ids,
                    &key_dir,
                )
                .await?;
                println!(
                    "vault rotated (epoch {}) — notified {n} peer(s); \
                     they adopt on their next sync pull/run",
                    pai_sync::rotate::vault_epoch(&cfg.data_dir)
                );
            }
            SyncCmd::Status {
                dir,
                relay,
                token,
                lan,
                to,
            } => {
                let peers = pair::list_peers(&store)?;
                println!("{} paired device(s)", peers.len());
                if dir.is_some() || relay.is_some() || *lan {
                    let t = if *lan {
                        lan_transport(to, &store, &ids, &device, &cfg.data_dir)?
                    } else {
                        sync_transport(dir, relay, token)?
                    };
                    let metas = t.list().await?;
                    let tombstones = metas.iter().filter(|m| m.tombstone).count();
                    println!(
                        "{}: {} object(s) ({} tombstone(s))",
                        t.id(),
                        metas.len(),
                        tombstones
                    );
                }
            }
            SyncCmd::Serve {
                dir,
                addr,
                token,
                announce,
            } => {
                if *announce {
                    // Mesh mode: peer-key bearer auth + signed multicast
                    // announcement. No token file — only paired devices
                    // can compute hex(peer_key).
                    let agree = crypto::agreement_key(device.id, &cfg.data_dir)?;
                    let st = store.clone();
                    let tokens = std::sync::Arc::new(move || {
                        pai_mesh::relay_tokens(&st, &agree.secret).unwrap_or_default()
                    });
                    let srv = pai_sync::relay::bind_dynamic(dir.into(), addr, tokens)?;
                    let port: u16 = srv
                        .addr()
                        .rsplit(':')
                        .next()
                        .and_then(|p| p.parse().ok())
                        .ok_or_else(|| Error::Sync("bad relay addr".into()))?;
                    {
                        let (st2, dev2, kd2) = (store.clone(), device.clone(), key_dir.clone());
                        std::thread::spawn(move || {
                            let ids2 = pai_identity::IdentityStore::new(st2);
                            let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
                                return;
                            };
                            let _ = sock.set_multicast_loop_v4(true);
                            let dest = std::net::SocketAddr::new(
                                std::net::IpAddr::V4(pai_mesh::MULTICAST_GROUP),
                                pai_mesh::MULTICAST_PORT,
                            );
                            loop {
                                if let Ok(a) = pai_mesh::make_announcement(&ids2, &dev2, &kd2, port)
                                {
                                    let _ = pai_mesh::send_announcement(&sock, &a, dest);
                                }
                                std::thread::sleep(std::time::Duration::from_secs(2));
                            }
                        });
                    }
                    println!(
                        "mesh relay on http://{} storing under {dir} \
                         — announcing to paired devices (peer-key auth)",
                        srv.addr()
                    );
                    pai_sync::relay::serve(srv);
                } else {
                    let token = token
                        .clone()
                        .or_else(|| std::env::var("PAI_SYNC_TOKEN").ok());
                    let srv = pai_sync::relay::bind(dir.into(), addr, token.clone())?;
                    println!(
                        "relay on http://{} storing under {dir} {}",
                        srv.addr(),
                        if token.is_some() {
                            "(token required)"
                        } else {
                            "(NO AUTH — localhost use only)"
                        }
                    );
                    pai_sync::relay::serve(srv);
                }
            }
        },
        Cmd::Broker { cmd } => match cmd {
            BrokerCmd::Devices => {
                let peers = pair::list_peers(&store)?;
                if peers.is_empty() {
                    println!("(no paired devices — `pai pair` first)");
                }
                for p in peers {
                    let w = store
                        .meta_get(&format!("place_weight.{}", p.device_id))?
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(0);
                    let pref = if w != 0 {
                        format!("  weight={w:+}")
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}  {:<20} {:<10} paired {}{}",
                        p.device_id,
                        p.name,
                        p.platform,
                        p.paired_at.format("%Y-%m-%d"),
                        pref
                    );
                }
            }
            BrokerCmd::Prefer { peer, weight } => {
                let id = resolve_peer(&store, peer)?;
                store.meta_set(&format!("place_weight.{id}"), &weight.to_string())?;
                let how = match weight.signum() {
                    1 => "preferred",
                    -1 => "deprioritized",
                    _ => "cleared",
                };
                println!("{id} {how} — placement weight {weight:+} (local only)");
            }
            BrokerCmd::Serve {
                dir,
                relay,
                token,
                poll_secs,
            } => {
                let t = sync_transport(dir, relay, token)?;
                // Guest endpoint: greq/ requests carrying a capability
                // token minted by `pai apps share` — verified, then run
                // through the same sandboxed app_run_op. It does not
                // need a vault: the token is the guest's auth.
                let guest_data = cfg.data_dir.clone();
                let guest_handler: Box<pai_share::guest::GuestHandler<'static>> =
                    Box::new(move |op, app_id, args| match op {
                        "app-run" => pai_apps::app_run_op_guest(&guest_data, app_id, args)
                            .map_err(|e| Error::Other(e.to_string())),
                        // Guests may hit the CGI surface too — the
                        // request is theirs, and no owner tokens ride.
                        "app-serve" => pai_apps::app_serve_op(&guest_data, app_id, args)
                            .map_err(|e| Error::Other(e.to_string())),
                        "app-read" => pai_apps::app_read_op(&guest_data, app_id, args)
                            .map_err(|e| Error::Other(e.to_string())),
                        "app-write" => pai_apps::app_write_op(&guest_data, app_id, args)
                            .map_err(|e| Error::Other(e.to_string())),
                        other => Err(Error::InvalidInput(format!("unknown guest op '{other}'"))),
                    });
                let guests = pai_share::guest::GuestServer::new(
                    device.clone(),
                    store.clone(),
                    &cfg.data_dir,
                );
                match crypto::vault_key(&cfg.data_dir)? {
                    Some(vault) => {
                        let ops = broker_ops(&cfg, cli, store.clone(), device.id).await;
                        let busy = ops.busy.clone();
                        let caps = device.capabilities.clone();
                        let mut srv =
                            pai_broker::rpc::BrokerServer::new(&*t, &vault, device.id, &ops)
                                .with_ops(ops.ops())
                                .with_guest_handler(guests, guest_handler)
                                .with_load_probe(Box::new(move || {
                                    // Battery/thermal are re-sampled live
                                    // each announce; hardware fields are
                                    // registration-time.
                                    let (bat, th) = pai_identity::probe_power();
                                    pai_broker::rpc::DeviceLoad {
                                        busy: busy.load(Ordering::Relaxed) as u32,
                                        on_battery: bat.or(caps.on_battery),
                                        thermal_throttled: th.or(caps.thermal_throttled),
                                        ram_bytes: caps.ram_bytes,
                                        cpu_cores: caps.cpu_cores,
                                    }
                                }));
                        println!(
                            "broker serving {} on {} — ops: {} (+ guest endpoint)",
                            device.id,
                            t.id(),
                            ops.describe()
                        );
                        srv.serve(std::time::Duration::from_secs(*poll_secs)).await;
                    }
                    None => {
                        // No vault — this device shares to guests only.
                        let poll = std::time::Duration::from_secs(*poll_secs);
                        println!(
                            "no vault key — serving {} guest endpoint only on {}",
                            device.id,
                            t.id()
                        );
                        loop {
                            if let Err(e) = guests.serve_once(&*t, &guest_handler).await {
                                eprintln!("guest serve pass failed: {e}");
                            }
                            tokio::time::sleep(poll).await;
                        }
                    }
                }
            }
            BrokerCmd::Call {
                device: dev,
                op,
                stream,
                text,
                file,
                out,
                timeout_secs,
                dir,
                relay,
                token,
            } => {
                let t = sync_transport(dir, relay, token)?;
                let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                    Error::Sync("no vault key — pair a device first (pai pair)".into())
                })?;
                let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, device.id)
                    .with_weights(place_weights(&store));
                let to = if dev == "any" {
                    client.find_peer(op).await?.ok_or_else(|| {
                        Error::NotFound(format!(
                            "no paired device advertises '{op}' — is `pai broker serve` running?"
                        ))
                    })?
                } else {
                    resolve_peer(&store, dev)?
                };
                let payload = if let Some(f) = file {
                    std::fs::read(f).map_err(|e| Error::InvalidInput(format!("{f}: {e}")))?
                } else if let Some(t) = text {
                    t.clone().into_bytes()
                } else {
                    return Err(Error::InvalidInput("pass --text or --file".into()));
                };
                let timeout = std::time::Duration::from_secs(*timeout_secs);
                let resp = if *stream {
                    client
                        .call_stream(to, op, &payload, timeout, &mut |chunk| {
                            if let Ok(s) = std::str::from_utf8(chunk) {
                                print!("{s}");
                                std::io::Write::flush(&mut std::io::stdout()).ok();
                            }
                        })
                        .await?
                } else {
                    client.call(to, op, &payload, timeout).await?
                };
                if let Some(f) = out {
                    std::fs::write(f, &resp).map_err(|e| Error::Storage(e.to_string()))?;
                    println!("wrote {f} ({} bytes)", resp.len());
                } else {
                    match String::from_utf8(resp.clone()) {
                        Ok(s) => println!("{s}"),
                        Err(_) => println!("({} bytes, binary — use --out)", resp.len()),
                    }
                }
            }
        },
        // Stable app URLs (V5j): every device running `pai serve` is
        // an ingress for every serve-enabled app — the route follows
        // `active_device`, so the URL survives migration. CGI-style:
        // request → env vars + stdin; app prints a CGI response.
        Cmd::Serve {
            bind,
            dir,
            relay,
            token,
        } => {
            let http = tiny_http::Server::http(bind)
                .map_err(|e| Error::Other(format!("serve bind {bind}: {e}")))?;
            let user_slug = name_slug(&user.display_name);
            println!("serving apps on http://{bind}/apps/<app-id>/<path> — ctrl-c to stop");
            println!(
                "name layer: http://<app>.{user_slug}.devices[:port] — \
                 `pai apps names` emits hosts-file lines"
            );
            let cx = ServeCtx {
                data_dir: cfg.data_dir.clone(),
                store: store.clone(),
                device: device.clone(),
                dir: dir.clone(),
                relay: relay.clone(),
                token: token.clone(),
                user_slug,
            };
            let rt = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                for mut req in http.incoming_requests() {
                    let resp = serve_request(&cx, &rt, &mut req);
                    let _ = req.respond(resp);
                }
            })
            .await
            .map_err(|e| Error::Other(format!("serve loop: {e}")))?;
        }
        Cmd::Email { cmd } => run_email_cmds(cmd, &cfg).await?,
        Cmd::Describe { image, prompt } => {
            let path = std::path::Path::new(image);
            let mime = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(pai_vision::mime_for_ext)
                .unwrap_or("image/png");
            let bytes =
                std::fs::read(path).map_err(|e| Error::InvalidInput(format!("{image}: {e}")))?;
            let model = cfg.inference.default_model.clone();
            let p: Arc<dyn pai_inference::ImageUnderstandingProvider> = vision_provider(&cfg)
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
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// Vision provider selection: a `process` block in `vision.json` wins
/// (its command must resolve on PATH); otherwise None — callers fall
/// back to llama-server.
fn vision_provider(
    cfg: &pai_config::Config,
) -> Option<Arc<dyn pai_inference::ImageUnderstandingProvider>> {
    let pc = pai_vision::VisionFileConfig::load(&cfg.data_dir)
        .ok()
        .flatten()?
        .process?;
    let p = pai_vision::ProcessVisionProvider::detect(pc)?;
    Some(Arc::new(p))
}

/// Voice ops. `transcribe`/`say` use one provider each; `turn` runs the
/// full Mic→VAD→STT→Agent→TTS→Speaker pipeline (file-based I/O — live mic
/// capture is the next step, needs OS audio permissions).
async fn run_voice_cmds(cmd: &VoiceCmd, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    use pai_inference::{SpeechToTextProvider, TextToSpeechProvider};
    use pai_voice::{UtteranceHandler, VoicePipeline};
    match cmd {
        VoiceCmd::Status => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            println!(
                "whisper-server: {}",
                if s.stt.is_some() {
                    format!("reachable at {}", s.cfg.whisper_url())
                } else {
                    format!("NOT reachable ({})", s.cfg.whisper_url())
                }
            );
            println!(
                "piper: {}",
                if s.tts.is_some() {
                    "found"
                } else {
                    "NOT found (set `pai voice configure --piper-bin/--piper-model` or PAI_PIPER_MODEL)"
                }
            );
            println!("vad: energy-vad (always available)");
            println!(
                "mic: {} | speaker: {}",
                if pai_voice::mic::input_available() {
                    "default input found"
                } else {
                    "NONE"
                },
                if pai_voice::mic::output_available() {
                    "default output found"
                } else {
                    "NONE"
                }
            );
        }
        VoiceCmd::Configure {
            whisper_url,
            piper_bin,
            piper_model,
        } => {
            let mut c = pai_voice::VoiceConfig::load(&cfg.data_dir)?;
            if let Some(u) = whisper_url {
                c.whisper_url = Some(u.clone());
            }
            if let Some(b) = piper_bin {
                c.piper_bin = Some(b.into());
            }
            if let Some(m) = piper_model {
                c.piper_model = Some(m.into());
            }
            c.save(&cfg.data_dir)?;
            println!("voice.json written — `pai voice status` to verify");
        }
        VoiceCmd::Transcribe { file } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let stt = s.stt.ok_or_else(|| {
                Error::Provider(
                    "whisper-server unreachable — start it or `pai voice configure --whisper-url`"
                        .into(),
                )
            })?;
            let audio =
                std::fs::read(file).map_err(|e| Error::InvalidInput(format!("{file}: {e}")))?;
            let text = stt.transcribe(&audio, "audio/wav").await?;
            println!("{text}");
        }
        VoiceCmd::Say { text, out } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let tts = s.tts.ok_or_else(|| {
                Error::Provider(
                    "piper not found — `pai voice configure --piper-bin/--piper-model`".into(),
                )
            })?;
            let wav = tts.synthesize(text, None).await?;
            std::fs::write(out, &wav).map_err(|e| Error::Storage(e.to_string()))?;
            println!("wrote {out} ({} bytes)", wav.len());
        }
        VoiceCmd::Turn { file, mic, out } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let pipeline: VoicePipeline = s.pipeline().ok_or_else(|| {
                Error::Provider(
                    "voice turn needs both whisper-server AND piper — `pai voice status`".into(),
                )
            })?;
            let audio = if *mic {
                println!("listening… (speak, then pause)");
                let pcm = pai_voice::mic::capture_utterance(&pai_voice::EnergyVad::default(), 30)?;
                if pcm.is_empty() {
                    println!("(nothing heard)");
                    return Ok(());
                }
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                pai_voice::pcm16_to_wav(&bytes, pai_voice::mic::TARGET_RATE)
            } else {
                let f = file
                    .as_deref()
                    .ok_or_else(|| Error::InvalidInput("pass a WAV file or --mic".into()))?;
                std::fs::read(f).map_err(|e| Error::InvalidInput(format!("{f}: {e}")))?
            };
            let def = agent_def(&ctx.provider_name, ctx.model.clone());
            struct AgentVoice<'a> {
                ctx: &'a Ctx,
                def: &'a AgentDefinition,
            }
            #[async_trait::async_trait]
            impl UtteranceHandler for AgentVoice<'_> {
                async fn respond(&self, transcript: &str) -> Result<String> {
                    let out =
                        send(self.ctx, self.def, transcript, None, None, &CliApproval).await?;
                    Ok(out.answer.unwrap_or_else(|| "(no reply)".into()))
                }
            }
            let handler = AgentVoice { ctx, def: &def };
            let (transcript, speech) = pipeline.turn(&audio, "audio/wav", &handler).await?;
            println!("you said: {transcript}");
            if *mic {
                let (rate, pcm) = pai_voice::mic::wav_to_pcm16(&speech)?;
                pai_voice::mic::play(&pcm, rate)?;
                println!("reply spoken");
            } else {
                std::fs::write(out, &speech).map_err(|e| Error::Storage(e.to_string()))?;
                println!("reply → {out} ({} bytes)", speech.len());
            }
        }
        VoiceCmd::Listen { max_secs, stream } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let stt = s.stt.ok_or_else(|| {
                Error::Provider(
                    "whisper-server unreachable — start it or `pai voice configure --whisper-url`"
                        .into(),
                )
            })?;
            if *stream {
                println!("listening… (partials print as segments close)");
                let mut n = 0usize;
                let text = pai_voice::stream_transcribe(
                    &stt,
                    &pai_voice::EnergyVad::default(),
                    *max_secs,
                    &mut |part| {
                        n += 1;
                        println!("  [{n}] {part}");
                    },
                )?;
                if text.is_empty() {
                    println!("(nothing heard)");
                } else {
                    println!("final: {text}");
                }
            } else {
                println!("listening… (speak, then pause)");
                let pcm =
                    pai_voice::mic::capture_utterance(&pai_voice::EnergyVad::default(), *max_secs)?;
                if pcm.is_empty() {
                    println!("(nothing heard)");
                    return Ok(());
                }
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                let wav = pai_voice::pcm16_to_wav(&bytes, pai_voice::mic::TARGET_RATE);
                let text = stt.transcribe(&wav, "audio/wav").await?;
                println!("{text}");
            }
        }
    }
    Ok(())
}

/// `pai audio …` — audio-generation provider config + generation.
/// Needs only config + the provider endpoint (no inference stack), so it
/// lives on the light command path. Sugar over `pai media`.
async fn run_audio_cmds(
    cmd: &AudioCmd,
    cfg: &pai_config::Config,
    store: &Arc<Store>,
    device: &Device,
) -> Result<()> {
    let m = match cmd {
        AudioCmd::Status => MediaCmd::Status,
        AudioCmd::Configure { audio_gen_url } => MediaCmd::Configure {
            audio_gen_url: audio_gen_url.clone(),
            image_gen_url: None,
            image_backend: None,
            video_gen_url: None,
        },
        AudioCmd::Gen {
            prompt,
            seconds,
            out,
            device: on,
            dir,
            relay,
            token,
        } => MediaCmd::Gen {
            kind: "audio".into(),
            prompt: prompt.clone(),
            seconds: *seconds,
            width: None,
            height: None,
            input: None,
            out: out.clone(),
            device: on.clone(),
            dir: dir.clone(),
            relay: relay.clone(),
            token: token.clone(),
        },
        AudioCmd::Jobs => MediaCmd::Jobs,
    };
    run_media_cmds(&m, cfg, store, device).await
}

/// `pai media …` — media generation (audio/image/video) provider
/// config + generation. Needs only config + the provider endpoints,
/// so it lives on the light command path.
async fn run_media_cmds(
    cmd: &MediaCmd,
    cfg: &pai_config::Config,
    store: &Arc<Store>,
    device: &Device,
) -> Result<()> {
    use base64::Engine as _;
    match cmd {
        MediaCmd::Status => {
            let c = pai_media::providers::MediaConfig::load(&cfg.data_dir)?;
            let t = std::time::Duration::from_secs(2);
            let audio = pai_media::providers::detect(&cfg.data_dir, t).await;
            let image = pai_media::providers::detect_image(&cfg.data_dir, t).await;
            let video = pai_media::providers::detect_video(&cfg.data_dir, t).await;
            let report = |name: &str, url: Option<String>, up: bool| {
                let at = url.unwrap_or_else(|| "(unset)".into());
                println!(
                    "{name}: {}",
                    if up {
                        format!("reachable at {at}")
                    } else {
                        format!("not reachable ({at})")
                    }
                );
            };
            report("audio", c.audio_gen_url.clone(), audio.is_some());
            report("image", c.image_gen_url.clone(), image.is_some());
            report("video", c.video_gen_url.clone(), video.is_some());
            if audio.is_none() && image.is_none() && video.is_none() {
                println!(
                    "none reachable — `pai media configure` sets URLs (defaults from env); \
                     services/media-gen is the bundled reference backend"
                );
            }
        }
        MediaCmd::Configure {
            audio_gen_url,
            image_gen_url,
            image_backend,
            video_gen_url,
        } => {
            let mut c = pai_media::providers::MediaConfig::load(&cfg.data_dir)?;
            if let Some(u) = audio_gen_url {
                c.audio_gen_url = Some(u.clone());
            }
            if let Some(u) = image_gen_url {
                c.image_gen_url = Some(u.clone());
            }
            if let Some(b) = image_backend {
                if b != "sdcpp" && b != "onnx" {
                    return Err(Error::InvalidInput(
                        "--image-backend must be `sdcpp` or `onnx`".into(),
                    ));
                }
                c.image_backend = Some(b.clone());
            }
            if let Some(u) = video_gen_url {
                c.video_gen_url = Some(u.clone());
            }
            c.save(&cfg.data_dir)?;
            println!("media.json written — `pai media status` to verify");
        }
        MediaCmd::Gen {
            kind,
            prompt,
            seconds,
            width,
            height,
            input,
            out,
            device: on,
            dir,
            relay,
            token,
        } => {
            let k = pai_media::jobs::kind_from_str(kind)
                .map_err(|_| Error::InvalidInput(format!("unknown media kind: {kind}")))?;
            let secs = (*seconds).clamp(1, 300);
            let size = (width.unwrap_or(512), height.unwrap_or(512));
            let input_bytes = match input {
                Some(p) => {
                    let bytes = std::fs::read(p).map_err(|e| Error::Storage(e.to_string()))?;
                    let mime = match std::path::Path::new(p)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase()
                        .as_str()
                    {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "webp" => "image/webp",
                        _ => "application/octet-stream",
                    };
                    Some((bytes, mime.to_string()))
                }
                None => None,
            };
            let out_path = |data_dir: &std::path::Path, mime: &str| -> Result<std::path::PathBuf> {
                match out {
                    Some(o) => Ok(std::path::PathBuf::from(o)),
                    None => {
                        let dir = data_dir.join("media");
                        std::fs::create_dir_all(&dir).map_err(|e| Error::Storage(e.to_string()))?;
                        let ext = match mime {
                            "image/png" => "png",
                            "video/mp4" => "mp4",
                            _ => "wav",
                        };
                        Ok(dir.join(format!(
                            "{}-{}.{}",
                            kind,
                            pai_core::now().timestamp_millis(),
                            ext
                        )))
                    }
                }
            };
            let mut job = pai_media::jobs::new_job(k, prompt);
            let params = serde_json::json!({
                "kind": kind,
                "duration_seconds": secs,
                "size": [size.0, size.1],
                "has_input": input_bytes.is_some(),
            })
            .to_string();
            pai_media::jobs::record(store, &job, Some(&params), Some(device.id), None)?;

            if let Some(dev) = on {
                // Remote: dispatch media-run to a paired device.
                let result = async {
                    let t = sync_transport(dir, relay, token)?;
                    let vault = pai_sync::crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                        Error::Sync("no vault key — pair a device first (pai pair)".into())
                    })?;
                    let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, device.id)
                        .with_weights(place_weights(store));
                    let to = if dev == "any" {
                        client.find_peer("media-run").await?.ok_or_else(|| {
                            Error::NotFound(
                                "no paired device advertises media-run — \
                                 `pai broker serve` running on the worker?"
                                    .into(),
                            )
                        })?
                    } else {
                        resolve_peer(store, dev)?
                    };
                    job.state = pai_media::JobState::Running;
                    job.placement_device = Some(to);
                    pai_media::jobs::record(store, &job, Some(&params), Some(device.id), None)?;
                    let mut payload = serde_json::json!({
                        "prompt": prompt,
                        "kind": kind,
                        "duration_seconds": secs,
                        "width": size.0,
                        "height": size.1,
                    });
                    if let Some((bytes, mime)) = &input_bytes {
                        payload["input_b64"] = serde_json::Value::String(
                            base64::engine::general_purpose::STANDARD.encode(bytes),
                        );
                        payload["input_mime"] = serde_json::Value::String(mime.clone());
                    }
                    let payload = payload.to_string().into_bytes();
                    println!("generating {kind} on {to} — this can take a while…");
                    client
                        .call(
                            to,
                            "media-run",
                            &payload,
                            std::time::Duration::from_secs(660),
                        )
                        .await
                }
                .await;

                match result {
                    Ok(resp) => {
                        let v: serde_json::Value = serde_json::from_slice(&resp)
                            .map_err(|e| Error::Other(format!("bad media-run reply: {e}")))?;
                        let b64 = v["result_b64"]
                            .as_str()
                            .or_else(|| v["audio_b64"].as_str())
                            .ok_or_else(|| {
                                Error::Other("media-run reply missing result_b64".into())
                            })?;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .map_err(|e| Error::Other(e.to_string()))?;
                        let mime = v["mime"].as_str().unwrap_or("audio/wav").to_string();
                        job.state = pai_media::JobState::Done;
                        job.result_blob = Some(store.put_blob(&bytes)?);
                        pai_media::jobs::record(store, &job, Some(&params), Some(device.id), None)?;
                        let path = out_path(&cfg.data_dir, &mime)?;
                        std::fs::write(&path, &bytes).map_err(|e| Error::Storage(e.to_string()))?;
                        println!(
                            "{} → {} bytes (job {} on {})",
                            path.display(),
                            bytes.len(),
                            job.id,
                            job.placement_device.unwrap()
                        );
                    }
                    Err(e) => {
                        job.state = pai_media::JobState::Failed;
                        pai_media::jobs::record(
                            store,
                            &job,
                            Some(&params),
                            Some(device.id),
                            Some(&e.to_string()),
                        )?;
                        return Err(e);
                    }
                }
            } else {
                job.state = pai_media::JobState::Running;
                job.placement_device = Some(device.id);
                println!("generating {kind} — this can take a while…");
                match pai_media::providers::generate(
                    &cfg.data_dir,
                    k,
                    prompt,
                    secs,
                    size,
                    input_bytes,
                )
                .await
                {
                    Ok((bytes, mime)) => {
                        job.state = pai_media::JobState::Done;
                        job.result_blob = Some(store.put_blob(&bytes)?);
                        pai_media::jobs::record(store, &job, Some(&params), Some(device.id), None)?;
                        let path = out_path(&cfg.data_dir, mime)?;
                        std::fs::write(&path, &bytes).map_err(|e| Error::Storage(e.to_string()))?;
                        println!("{} → {} bytes", path.display(), bytes.len());
                    }
                    Err(e) => {
                        job.state = pai_media::JobState::Failed;
                        pai_media::jobs::record(
                            store,
                            &job,
                            Some(&params),
                            Some(device.id),
                            Some(&e.to_string()),
                        )?;
                        return Err(e);
                    }
                }
            }
        }
        MediaCmd::Jobs => {
            for j in pai_media::jobs::list(store, 20)? {
                println!(
                    "{} {} [{}] {} {}",
                    j["id"].as_str().unwrap_or("?"),
                    j["kind"].as_str().unwrap_or("?"),
                    j["prompt"].as_str().unwrap_or("?"),
                    j["state"].as_str().unwrap_or("?"),
                    j["worker"]
                        .as_str()
                        .map(|w| format!("on {:.8}", w))
                        .unwrap_or_default(),
                );
                if let Some(e) = j["error"].as_str() {
                    println!("    error: {e}");
                }
            }
        }
    }
    Ok(())
}

async fn email_provider(cfg: &pai_config::Config) -> Result<pai_connector_email::ImapProvider> {
    let c = pai_connector_email::ImapConfig::load(&cfg.data_dir)?.ok_or_else(|| {
        Error::InvalidInput("no email account — run `pai email configure`".into())
    })?;
    Ok(pai_connector_email::ImapProvider::new(c))
}

async fn run_email_cmds(cmd: &EmailCmd, cfg: &pai_config::Config) -> Result<()> {
    use pai_connector_email::EmailProvider;
    match cmd {
        EmailCmd::Configure { oauth } => {
            let read = |prompt: &str, default: &str| -> Result<String> {
                print!("{prompt} [{default}]: ");
                std::io::Write::flush(&mut std::io::stdout()).ok();
                let mut s = String::new();
                std::io::stdin()
                    .read_line(&mut s)
                    .map_err(|e| Error::Other(e.to_string()))?;
                let s = s.trim();
                Ok(if s.is_empty() {
                    default.to_string()
                } else {
                    s.to_string()
                })
            };
            let host = read("IMAP host", "imap.gmail.com")?;
            let port: u16 = read("Port", "993")?
                .parse()
                .map_err(|_| Error::InvalidInput("bad port".into()))?;
            let user = read("User (email address)", "")?;
            if user.is_empty() {
                return Err(Error::InvalidInput("user is required".into()));
            }
            let mailbox = read("Mailbox", "INBOX")?;
            let drafts = read("Drafts mailbox", "[Gmail]/Drafts")?;
            let archive = read("Archive mailbox", "[Gmail]/All Mail")?;
            // Optional SMTP submission — empty host keeps drafts-only.
            let smtp_host = read("SMTP host (empty = drafts only)", "smtp.gmail.com")?;
            let smtp = if smtp_host.is_empty() {
                None
            } else {
                let smtp_port: u16 = read("SMTP port", "465")?
                    .parse()
                    .map_err(|_| Error::InvalidInput("bad smtp port".into()))?;
                let smtp_tls = match read("SMTP TLS (tls/starttls/none)", "tls")?.as_str() {
                    "tls" => pai_connector_email::SmtpTls::Tls,
                    "starttls" => pai_connector_email::SmtpTls::StartTls,
                    "none" => pai_connector_email::SmtpTls::None,
                    other => {
                        return Err(Error::InvalidInput(format!(
                            "bad tls mode {other:?} — tls|starttls|none"
                        )))
                    }
                };
                Some(pai_connector_email::SmtpConfig {
                    host: smtp_host,
                    port: smtp_port,
                    tls: smtp_tls,
                    user: None, // same login as IMAP
                })
            };
            let oauth_cfg = match oauth.as_deref() {
                None => None,
                Some(provider) => {
                    let client_id = read("OAuth client_id (from your app registration)", "")?;
                    if client_id.is_empty() {
                        return Err(Error::InvalidInput(
                            "client_id is required — register an app first".into(),
                        ));
                    }
                    let tenant = if provider == "microsoft" {
                        Some(read("Tenant", "common")?)
                    } else {
                        None
                    };
                    Some(pai_connector_email::OAuthConfig {
                        provider: provider.to_string(),
                        client_id,
                        tenant,
                        device_url: None,
                        token_url: None,
                        scopes: None,
                    })
                }
            };
            let password = if oauth_cfg.is_none() {
                rpassword::prompt_password("Password (app password for Gmail/Outlook): ")
                    .map_err(|e| Error::Other(e.to_string()))?
            } else {
                String::new()
            };
            let c = pai_connector_email::ImapConfig {
                host,
                port,
                user: user.clone(),
                mailbox,
                drafts_mailbox: drafts,
                archive_mailbox: archive,
                smtp,
                oauth: oauth_cfg,
            };
            if let Some(oc) = &c.oauth {
                // Device-authorization flow: print the code, poll until
                // the user authorizes (or the grant expires).
                let grant = pai_connector_email::oauth::device_flow(oc).await?;
                println!();
                println!("Go to {}", grant.verification_uri);
                if let Some(u) = &grant.verification_uri_complete {
                    println!("  (or directly: {u})");
                }
                println!("and enter code: {}", grant.user_code);
                let mut interval = grant.interval.max(1);
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(grant.expires_in.max(120));
                let mut consecutive_pendings = 0u32;
                let tokens = loop {
                    if std::time::Instant::now() > deadline {
                        return Err(Error::InvalidInput(
                            "device grant expired — re-run configure".into(),
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                    match pai_connector_email::oauth::poll_token_once(oc, &grant.device_code)
                        .await?
                    {
                        pai_connector_email::oauth::Poll::Pending => {
                            consecutive_pendings += 1;
                            if consecutive_pendings == 5 {
                                interval += 5; // back off gently
                            }
                            print!(".");
                            std::io::Write::flush(&mut std::io::stdout()).ok();
                        }
                        pai_connector_email::oauth::Poll::Granted(t) => break t,
                    }
                };
                let refresh = tokens.refresh_token.ok_or_else(|| {
                    Error::Provider(
                        "oauth grant returned no refresh_token — add `offline_access` scope".into(),
                    )
                })?;
                c.save(&cfg.data_dir)?;
                if pai_connector_email::oauth::store_refresh_token(&user, &refresh) {
                    println!("\nrefresh token stored in OS keystore (email-oauth:{user})");
                } else {
                    return Err(Error::Other(
                        "keystore unavailable — cannot persist the refresh token".into(),
                    ));
                }
            } else {
                c.save(&cfg.data_dir)?;
                if password.is_empty() {
                    println!("no password stored — set PAI_EMAIL_PASSWORD at run time");
                } else if pai_connector_email::imap::store_password(&user, &password) {
                    println!("password stored in OS keystore (email:{user})");
                } else {
                    println!("keystore unavailable — set PAI_EMAIL_PASSWORD at run time");
                }
            }
            println!("account written to {}/email.json", cfg.data_dir.display());
        }
        EmailCmd::Status => match pai_connector_email::ImapConfig::load(&cfg.data_dir)? {
            Some(c) => {
                println!("imap://{}:{}/{}", c.user, c.host, c.mailbox);
                println!(
                    "drafts: {}  archive: {}",
                    c.drafts_mailbox, c.archive_mailbox
                );
                match &c.smtp {
                    Some(s) => println!("smtp: {}:{} ({:?}) — send enabled", s.host, s.port, s.tls),
                    None => println!("smtp: not configured — drafts only"),
                }
                match &c.oauth {
                    Some(o) => {
                        let has_rt =
                            pai_identity::keystore::load(&format!("email-oauth:{}", c.user))
                                .is_some();
                        println!(
                            "auth: oauth2 ({}) — refresh token {}",
                            o.provider,
                            if has_rt { "available" } else { "MISSING" }
                        );
                    }
                    None => {
                        let pw = pai_connector_email::imap::resolve_password(&c.user).is_ok();
                        println!(
                            "auth: password — {}",
                            if pw { "available" } else { "MISSING" }
                        );
                    }
                }
            }
            None => println!("not configured — run `pai email configure`"),
        },
        EmailCmd::Search {
            query,
            from,
            label,
            unread,
            limit,
        } => {
            let p = email_provider(cfg).await?;
            let hits = p
                .search(&pai_connector_email::EmailSearch {
                    query: query.clone(),
                    from: from.clone(),
                    label: label.clone(),
                    unread_only: *unread,
                    limit: *limit,
                    ..Default::default()
                })
                .await?;
            for m in &hits {
                println!(
                    "  {:>6}  {:<40} {:<45} {}",
                    m.id,
                    m.from.address,
                    m.subject,
                    m.received_at.format("%Y-%m-%d")
                );
            }
            if hits.is_empty() {
                println!("no messages");
            }
        }
        EmailCmd::Read { id } => {
            let m = email_provider(cfg).await?.read(id).await?;
            println!("from: {}", m.summary.from.address);
            println!(
                "to:   {}",
                m.summary
                    .to
                    .iter()
                    .map(|a| a.address.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("date: {}", m.summary.received_at.format("%Y-%m-%d %H:%M"));
            println!("subject: {}\n", m.summary.subject);
            println!("{}", m.body_text.unwrap_or_else(|| "(no text body)".into()));
            for a in &m.attachments {
                println!(
                    "  attachment: {} ({}, {} bytes)",
                    a.filename, a.mime, a.size_bytes
                );
            }
        }
        EmailCmd::Draft {
            to,
            cc,
            subject,
            body,
            in_reply_to,
        } => {
            let addr = |a: &String| pai_connector_email::EmailAddress {
                name: None,
                address: a.clone(),
            };
            let id = email_provider(cfg)
                .await?
                .create_draft(&pai_connector_email::Draft {
                    to: to.iter().map(addr).collect(),
                    cc: cc.iter().map(addr).collect(),
                    subject: subject.clone(),
                    body: body.clone(),
                    in_reply_to: in_reply_to.clone(),
                })
                .await?;
            println!("{id}");
        }
        EmailCmd::Send {
            to,
            cc,
            subject,
            body,
            in_reply_to,
        } => {
            let addr = |a: &String| pai_connector_email::EmailAddress {
                name: None,
                address: a.clone(),
            };
            email_provider(cfg)
                .await?
                .send(&pai_connector_email::Draft {
                    to: to.iter().map(addr).collect(),
                    cc: cc.iter().map(addr).collect(),
                    subject: subject.clone(),
                    body: body.clone(),
                    in_reply_to: in_reply_to.clone(),
                })
                .await?;
            println!("sent");
        }
        EmailCmd::Archive { id } => {
            email_provider(cfg).await?.archive(id).await?;
            println!("archived {id}");
        }
        EmailCmd::Label { id, label } => {
            email_provider(cfg).await?.label(id, label).await?;
            println!("labeled {id} → {label}");
        }
        EmailCmd::Delete { id } => {
            email_provider(cfg).await?.delete(id).await?;
            println!("deleted {id}");
        }
    }
    Ok(())
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

/// `pai notify ...` — the proactive inbox + configured delivery channels.
async fn run_notify_cmds(cmd: &NotifyCmd, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    use pai_notify::{store as ns, NotifySink, StoreNotifySink};
    let sink = StoreNotifySink {
        store: ctx.store.clone(),
        config: pai_notify::load_config(&cfg.data_dir)?,
        email: ctx.email.clone(),
    };
    match cmd {
        NotifyCmd::List { unread } => {
            for n in ns::list(&ctx.store, *unread, 50)? {
                println!(
                    "{:.8}  {} {:<40} {}",
                    n.id,
                    if n.read_at.is_some() { " " } else { "●" },
                    n.title,
                    n.created_at,
                );
            }
            let unread = ns::unread_count(&ctx.store)?;
            if unread > 0 {
                println!("{unread} unread");
            }
        }
        NotifyCmd::Open { id } => {
            let n = ns::get(&ctx.store, id)?
                .ok_or_else(|| Error::NotFound(format!("notification {id}")))?;
            println!(
                "{}

{}

— {} ({})",
                n.title, n.body, n.source, n.created_at
            );
            ns::mark_read(&ctx.store, &n.id)?;
        }
        NotifyCmd::Send {
            title,
            body,
            external,
        } => {
            let body_s = body.clone().unwrap_or_default();
            let id = sink.publish(title, &body_s, "cli", SyncScope::Synchronized)?;
            if *external {
                let fired = sink.deliver_external(title, &body_s, "cli").await?;
                println!(
                    "sent {:.8} → inbox{}",
                    id,
                    if fired.is_empty() {
                        String::new()
                    } else {
                        format!(" +{}", fired.join("+"))
                    }
                );
            } else {
                println!("sent {:.8} → inbox", id);
            }
        }
        NotifyCmd::Clear => {
            println!("{} marked read", ns::mark_all_read(&ctx.store)?);
        }
        NotifyCmd::Remove { id } => {
            if ns::remove(&ctx.store, id)? {
                println!("removed {id}");
            } else {
                println!("no notification {id}");
            }
        }
        NotifyCmd::Configure { email_to, webhook } => {
            let path = cfg.data_dir.join("notify.json");
            let mut c = pai_notify::load_config(&cfg.data_dir)?;
            if let Some(e) = email_to {
                c.email_to = if e.is_empty() { None } else { Some(e.clone()) };
            }
            if let Some(w) = webhook {
                c.webhook_url = if w.is_empty() { None } else { Some(w.clone()) };
            }
            let json =
                serde_json::to_string_pretty(&c).map_err(|e| Error::InvalidInput(e.to_string()))?;
            std::fs::write(&path, json).map_err(|e| Error::Storage(e.to_string()))?;
            println!(
                "notify.json: email_to={} webhook={}",
                c.email_to.as_deref().unwrap_or("(none)"),
                c.webhook_url.as_deref().unwrap_or("(none)")
            );
        }
        NotifyCmd::Test => {
            let fired = sink
                .deliver_external("pai test", "notification channel check", "cli:test")
                .await?;
            if fired.is_empty() {
                println!("no external channels configured — see `pai notify configure`");
            } else {
                println!("delivered via: {}", fired.join(", "));
            }
        }
    }
    Ok(())
}

/// `pai workflow ...` — declarative multi-step runs over the agent runtime.
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
        WorkflowCmd::Resume { run_id } => {
            let runner = WorkflowRunner {
                agent: &ctx.agent,
                store: &ctx.store,
                provider: provider.to_string(),
                model,
            };
            let emit = |e: AgentEvent| print_event(&e);
            let out = runner.resume(run_id, &CliApproval, &emit).await?;
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
    }
    Ok(())
}

/// `pai task …` — synced background tasks with claim/lease execution.
/// `pai circle` — shared-memory federation scopes (V3d).
/// `pai circle` — shared-memory federation scopes (V3d). Runs on the
/// light path (no inference needed): create/grant/list/leave.
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

/// "--at" accepts RFC3339 or "+N" seconds from now.
fn parse_at(s: &str) -> Result<Timestamp> {
    if let Some(secs) = s.strip_prefix('+').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(now() + chrono::Duration::seconds(secs));
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|e| Error::InvalidInput(format!("bad --at '{s}' (RFC3339 or +N secs): {e}")))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // Pair/sync/broker/email/describe need no agent — skip provider probing.
    if matches!(
        cli.cmd,
        Cmd::Pair { .. }
            | Cmd::Sync { .. }
            | Cmd::Broker { .. }
            | Cmd::Email { .. }
            | Cmd::Circle { .. }
            | Cmd::Describe { .. }
            | Cmd::Deploy { .. }
            | Cmd::Apps { .. }
            | Cmd::Mesh { .. }
            | Cmd::Serve { .. }
            | Cmd::Audio { .. }
            | Cmd::Media { .. }
    ) {
        return run_sync_cmds(&cli).await;
    }

    let (ctx, cfg) = build(&cli).await?;

    match cli.cmd {
        Cmd::Docs { cmd } => match cmd {
            DocsCmd::Ingest { path, sync } => {
                // User-initiated: no jail — they named the file.
                let canon = std::fs::canonicalize(&path)
                    .map_err(|e| Error::InvalidInput(format!("{path}: {e}")))?;
                let bytes = std::fs::read(&canon)
                    .map_err(|e| Error::InvalidInput(format!("{canon:?}: {e}")))?;
                let mime = match canon
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase()
                    .as_str()
                {
                    "md" | "markdown" => "text/markdown",
                    "html" | "htm" => "text/html",
                    _ => "text/plain",
                };
                let id = ctx
                    .documents
                    .ingest(&bytes, mime, canon.file_name().and_then(|n| n.to_str()))
                    .await?;
                if sync {
                    ctx.documents.set_sync_scope(id, SyncScope::Synchronized)?;
                }
                println!(
                    "ingested: {id}{}",
                    if sync { " (synchronized)" } else { "" }
                );
            }
            DocsCmd::Sync { id, mode } => {
                let sc = match mode.as_str() {
                    "synchronized" => SyncScope::Synchronized,
                    "device-local" | "device_local" => SyncScope::DeviceLocal,
                    _ => {
                        return Err(Error::InvalidInput(
                            "mode: synchronized|device-local".into(),
                        ))
                    }
                };
                ctx.documents
                    .set_sync_scope(DocumentId(parse_uuid(&id, "document")?), sc)?;
            }
            DocsCmd::List => {
                for (id, title, mime, at, sections) in ctx.documents.list()? {
                    println!(
                        "  {}  {:<28} {:<14} {} sections  {}",
                        &id.to_string()[..8],
                        title.unwrap_or_else(|| "untitled".into()),
                        mime,
                        sections,
                        at.format("%Y-%m-%d")
                    );
                }
            }
            DocsCmd::Search { query } => {
                for h in ctx.documents.search(&query, 10).await? {
                    println!(
                        "  {:.2}  {}  {}  {}",
                        h.score,
                        &h.document_id.to_string()[..8],
                        h.title.unwrap_or_else(|| "untitled".into()),
                        h.snippet.replace('\n', " ")
                    );
                }
            }
            DocsCmd::Delete { id } => {
                ctx.documents
                    .delete(DocumentId(parse_uuid(&id, "document")?))?;
                println!("deleted {id}");
            }
        },
        Cmd::Demo => {
            let def = agent_def(&ctx.provider_name, ctx.model.clone());
            let conv = ctx
                .conversations
                .create(ctx.session, MemoryIsolation::Shared)?;
            println!("== Vertical slice: provider={} ==\n", ctx.provider_name);

            println!("user: Remember that I prefer local models");
            let r = send(
                &ctx,
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
                &ctx,
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
                &ctx,
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
        }

        Cmd::Chat {
            conversation,
            isolated,
        } => {
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
                match send(&ctx, &def, line, Some(conv), None, &CliApproval).await {
                    Ok(o) if o.streamed => println!(),
                    Ok(o) => match o.answer {
                        Some(a) => println!("pai> {a}"),
                        None => println!("pai> (no answer)"),
                    },
                    Err(e) => println!("pai> error: {e}"),
                }
            }
        }

        Cmd::Models { cmd } => run_models(&cmd, &cfg).await?,

        Cmd::Conversations { cmd } => match cmd {
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
        },

        Cmd::Runs { cmd } => match cmd {
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
                send(&ctx, &def, "", None, Some(rid), &CliApproval).await?;
            }
            RunsCmd::Abandon { id } => {
                ctx.runs.abandon(AgentRunId(parse_uuid(&id, "run")?))?;
            }
        },

        Cmd::Policies { cmd } => match cmd {
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
                    .map_err(|_| {
                        Error::InvalidInput(format!("unknown permission '{permission}'"))
                    })?;
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
        },

        Cmd::Audit { limit } => {
            for e in ctx.audit.recent(limit)? {
                println!(
                    "{}  {:<18} tool={:<16} outcome={:?}",
                    e.at.to_rfc3339(),
                    format!("{:?}", e.kind),
                    e.tool.unwrap_or_default(),
                    e.outcome
                );
            }
        }

        Cmd::Memories { cmd } => match cmd {
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
                let ctx_tool = pai_tools::ToolContext {
                    run: AgentRunId::new(),
                    device: ctx.agent.device,
                    memory: Some(ctx.memory.as_ref()),
                    memory_scope: None,
                    documents: Some(ctx.documents.as_ref()),
                    email: ctx.email.as_deref(),
                    vision: None,
                    notify: None,
                    allowed_roots: &[],
                    apps: None,
                    audio_gen: None,
                    media_dir: None,
                };
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
                let ctx_tool = pai_tools::ToolContext {
                    run: AgentRunId::new(),
                    device: ctx.agent.device,
                    memory: Some(ctx.memory.as_ref()),
                    memory_scope: None,
                    documents: Some(ctx.documents.as_ref()),
                    email: ctx.email.as_deref(),
                    vision: None,
                    notify: None,
                    allowed_roots: &[],
                    apps: None,
                    audio_gen: None,
                    media_dir: None,
                };
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
        },
        Cmd::Circle { cmd } => run_circle_cmds(&ctx.store, ctx.agent.device, &cfg, &cmd).await?,
        Cmd::Voice { cmd } => run_voice_cmds(&cmd, &ctx, &cfg).await?,
        Cmd::Task { cmd } => {
            run_task_cmds(&cmd, &ctx, &cli.provider, cli.model.clone(), &cfg).await?
        }
        Cmd::Workflow { cmd } => {
            run_workflow_cmds(&cmd, &ctx, &cli.provider, cli.model.clone()).await?
        }
        Cmd::Notify { cmd } => run_notify_cmds(&cmd, &ctx, &cfg).await?,
        Cmd::Pair { .. }
        | Cmd::Sync { .. }
        | Cmd::Broker { .. }
        | Cmd::Email { .. }
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

// --- V5j: stable app URLs — CGI-style HTTP gateway -------------------

struct ServeCtx {
    data_dir: std::path::PathBuf,
    store: Arc<pai_storage::Store>,
    device: Device,
    dir: Option<String>,
    relay: Option<String>,
    token: Option<String>,
    /// Local user's `app.user.devices` slug — Host-header routing only
    /// answers names in this namespace.
    user_slug: String,
}

fn serve_response(
    status: u16,
    ct: &str,
    body: Vec<u8>,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_data(body)
        .with_status_code(status)
        .with_header(tiny_http::Header::from_bytes("Content-Type", ct).expect("valid header"))
}

/// One HTTP request → app run → HTTP response. Local apps run in
/// process; apps placed elsewhere are forwarded over the broker
/// `app-serve` op — either way the wire shape is the `encode_run`
/// JSON envelope and stdout is parsed as a CGI response.
fn serve_request(
    cx: &ServeCtx,
    rt: &tokio::runtime::Handle,
    req: &mut tiny_http::Request,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    use base64::Engine as _;
    use std::io::Read as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let url = req.url().to_string();
    let (mut path, query) = match url.split_once('?') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (url, String::new()),
    };
    // `app.user.devices` name layer — a Host like
    // `notes.alice.devices` routes `/x` to `/apps/notes/x`, so names
    // stay valid no matter which device the app is placed on.
    if let Some(app) = req
        .headers()
        .iter()
        .find(|h| h.field.to_string().eq_ignore_ascii_case("host"))
        .and_then(|h| parse_app_name(h.value.as_str(), &cx.user_slug))
    {
        path = format!("/apps/{app}{path}");
    }
    if path == "/" || path == "/apps" || path == "/apps/" {
        // Index: apps that opted into serving.
        let reg = pai_apps::AppRegistry::new(&cx.data_dir);
        let mut lines = String::from("serve-enabled apps:\n");
        for (id, m) in reg.list().unwrap_or_default() {
            if m.app.serve {
                lines.push_str(&format!("  /apps/{id}/\n"));
            }
        }
        return serve_response(200, "text/plain", lines.into_bytes());
    }
    let Some(rest) = path.strip_prefix("/apps/") else {
        return serve_response(404, "text/plain", b"not found\n".to_vec());
    };
    let (id, sub) = match rest.split_once('/') {
        Some((a, b)) => (a.to_string(), format!("/{b}")),
        None => (rest.to_string(), "/".to_string()),
    };
    if id.is_empty() {
        return serve_response(404, "text/plain", b"not found\n".to_vec());
    }
    let mut body = Vec::new();
    if req
        .as_reader()
        .take(pai_apps::serve::BODY_CAP as u64 + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return serve_response(400, "text/plain", b"bad request body\n".to_vec());
    }
    if body.len() > pai_apps::serve::BODY_CAP {
        return serve_response(413, "text/plain", b"body too large\n".to_vec());
    }
    let sreq = pai_apps::serve::ServeRequest {
        method: req.method().as_str().into(),
        path: sub,
        query,
        headers: req
            .headers()
            .iter()
            .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
            .collect(),
        body_b64: b64.encode(&body),
    };
    let arg = sreq.to_json();
    // Follow placement: an app active elsewhere is proxied over the
    // broker so the URL is stable across migration.
    let remote = pai_sync::backup::active_elsewhere(&cx.store, cx.device.id, &id)
        .ok()
        .flatten();
    let envelope = if let Some(ref other) = remote {
        let t = match sync_transport(&cx.dir, &cx.relay, &cx.token) {
            Ok(t) => t,
            Err(e) => {
                return serve_response(
                    502,
                    "text/plain",
                    format!("app {id} lives on {other} — transport needed: {e}\n").into_bytes(),
                )
            }
        };
        let vault = match pai_sync::crypto::vault_key(&cx.data_dir) {
            Ok(Some(v)) => v,
            _ => {
                return serve_response(
                    502,
                    "text/plain",
                    "no vault key — cannot reach the app's device\n"
                        .as_bytes()
                        .to_vec(),
                )
            }
        };
        let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, cx.device.id)
            .with_weights(place_weights(&cx.store));
        let payload = serde_json::json!({"id": id, "args": [arg]})
            .to_string()
            .into_bytes();
        let to = match uuid::Uuid::parse_str(other) {
            Ok(u) => DeviceId(u),
            Err(e) => {
                return serve_response(
                    502,
                    "text/plain",
                    format!("bad active_device id '{other}': {e}\n").into_bytes(),
                )
            }
        };
        match rt.block_on(client.call(
            to,
            "app-serve",
            &payload,
            std::time::Duration::from_secs(120),
        )) {
            Ok(r) => r,
            Err(e) => {
                return serve_response(
                    502,
                    "text/plain",
                    format!("remote serve: {e}\n").into_bytes(),
                )
            }
        }
    } else {
        match pai_apps::app_serve_op(&cx.data_dir, &id, &[arg]) {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                let status = if msg.contains("does not serve") {
                    403
                } else if msg.contains("not installed") {
                    404
                } else {
                    502
                };
                return serve_response(
                    502.min(status).max(status),
                    "text/plain",
                    format!("{msg}\n").into_bytes(),
                );
            }
        }
    };
    let v: serde_json::Value = serde_json::from_slice(&envelope).unwrap_or_default();
    let stdout = v["stdout_b64"]
        .as_str()
        .and_then(|s| b64.decode(s).ok())
        .unwrap_or_default();
    let exit = v["exit_code"].as_i64().unwrap_or(-1);
    let served = pai_apps::serve::parse_cgi(&stdout);
    let mut ev = pai_audit::event(AuditKind::AppServed, AuditOutcome::Ok);
    ev.device = Some(cx.device.id);
    ev.detail = serde_json::json!({
        "app_id": id,
        "method": req.method().as_str(),
        "status": served.status,
        "remote": remote.is_some(),
        "exit_code": exit,
    });
    let _ = pai_audit::AuditLog::new(cx.store.clone()).record(&ev);
    if exit != 0 {
        let err = v["stderr_b64"]
            .as_str()
            .and_then(|s| b64.decode(s).ok())
            .unwrap_or_default();
        return serve_response(
            502,
            "text/plain",
            format!("app exited {exit}: {}\n", String::from_utf8_lossy(&err)).into_bytes(),
        );
    }
    let mut resp = tiny_http::Response::from_data(served.body).with_status_code(served.status);
    for (k, val) in served.headers {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_str(), val.as_str()) {
            resp.add_header(h);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_slug_makes_dns_labels() {
        assert_eq!(name_slug("Alice"), "alice");
        assert_eq!(name_slug("Martin C."), "martin-c");
        assert_eq!(name_slug("Alice's phone"), "alice-s-phone");
        assert_eq!(name_slug("  spaced  out  "), "spaced-out");
    }

    #[test]
    fn app_name_resolves_own_namespace() {
        assert_eq!(
            parse_app_name("notes.martin.devices", "martin"),
            Some("notes".into())
        );
        // Port + trailing dot tolerated.
        assert_eq!(
            parse_app_name("com.example.app.martin.devices:8787", "martin"),
            Some("com.example.app".into())
        );
        // Other users' namespace isn't answered here.
        assert_eq!(parse_app_name("notes.bob.devices", "martin"), None);
        assert_eq!(parse_app_name("localhost", "martin"), None);
        assert_eq!(parse_app_name("a.b", "martin"), None);
    }
}

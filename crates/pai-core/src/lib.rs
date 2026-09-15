//! Core domain model for the Personal AI platform.
//!
//! These types are stable platform concepts. They must never depend on a
//! vendor API, a specific model, or an inference engine. Provider-specific
//! shapes live in adapter crates, never here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(UserId);
id_type!(DeviceId);
id_type!(IdentityId);
id_type!(SessionId);
id_type!(ConversationId);
id_type!(MessageId);
id_type!(ModelId);
id_type!(AgentId);
id_type!(AgentRunId);
id_type!(TaskId);
id_type!(ToolCallId);
id_type!(MemoryId);
id_type!(DocumentId);
id_type!(MediaAssetId);
id_type!(ConnectorId);
id_type!(WorkflowId);
id_type!(AuditEventId);

pub type Timestamp = chrono::DateTime<chrono::Utc>;

pub fn now() -> Timestamp {
    chrono::Utc::now()
}

// ---------------------------------------------------------------------------
// Identity / devices
// ---------------------------------------------------------------------------

/// One human. One AI identity across every device they own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub display_name: String,
    pub created_at: Timestamp,
}

/// A device that runs part of the platform (phone, desktop, home server).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub owner: UserId,
    pub name: String,
    pub platform: Platform,
    /// ed25519 public key (raw 32 bytes), used for sync identity.
    pub public_key: Vec<u8>,
    pub registered_at: Timestamp,
    pub last_seen_at: Timestamp,
    pub capabilities: DeviceCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Ios,
    Android,
    Linux,
    MacOs,
    Windows,
    Server,
}

/// What a device can compute on. Advertised to the compute broker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    pub cpu_cores: u32,
    pub ram_bytes: u64,
    /// Discrete VRAM, or unified memory visible to GPU.
    pub gpu_vram_bytes: Option<u64>,
    pub gpu_name: Option<String>,
    pub npu_available: bool,
    pub on_battery: Option<bool>,
    pub thermal_throttled: Option<bool>,
    pub network: NetworkState,
    /// Model ids currently installed and loadable on this device.
    pub available_models: Vec<ModelId>,
    /// Capabilities this device can serve (union over models + hardware).
    pub supported_capabilities: Vec<ModelCapability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkState {
    #[default]
    Unknown,
    Offline,
    Metered,
    Online,
}

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub user: UserId,
    pub device: DeviceId,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub session: SessionId,
    pub title: Option<String>,
    pub created_at: Timestamp,
    /// Sync-scoped: conversations may roam across devices.
    pub sync_scope: SyncScope,
    /// Whether memory written here is global or scoped to this conversation.
    #[serde(default)]
    pub memory: MemoryIsolation,
}

/// Per-conversation memory partitioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryIsolation {
    /// Sees global memory plus its own; writes land in global scope.
    #[default]
    Shared,
    /// Sees only its own memories; writes stay scoped to this conversation.
    /// Use for sensitive contexts that must not leak into other chats.
    Isolated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub conversation: ConversationId,
    pub role: Role,
    pub created_at: Timestamp,
    /// Multimodal content blocks — never assume text-only.
    pub content: Vec<Content>,
    /// Trust classification of this content's origin. Tool results and
    /// external documents are *never* treated as instructions.
    pub trust: TrustLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

/// Where a piece of content came from. Drives prompt-injection defenses:
/// untrusted content is data, never instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Email bodies, web pages, attachments, tool output.
    Untrusted = 0,
    /// Content the platform itself generated (e.g. prior assistant text).
    Generated = 1,
    /// Direct user input.
    User = 2,
    /// Developer/system policy — highest authority in prompts.
    Policy = 3,
}

/// A single block of multimodal content inside a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        /// Reference into blob storage — never inline bytes in the domain model.
        blob: String,
        mime: String,
    },
    Audio {
        blob: String,
        mime: String,
    },
    Video {
        blob: String,
        mime: String,
    },
    DocumentRef {
        document: DocumentId,
    },
    /// Structured tool invocation emitted by a model.
    ToolCall {
        call: ToolCallId,
        tool: String,
        arguments: serde_json::Value,
    },
    /// Result of a tool call. Always [`TrustLevel::Untrusted`] upstream.
    ToolResult {
        call: ToolCallId,
        tool: String,
        output: serde_json::Value,
        is_error: bool,
    },
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text { text: s.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text { text } => Some(text),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// What a model can do. Capability-driven selection — never select by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    TextGeneration,
    Vision,
    AudioUnderstanding,
    ToolCalling,
    Reasoning,
    Embeddings,
    SpeechToText,
    TextToSpeech,
    ImageGeneration,
    ImageEditing,
    VideoGeneration,
    VideoUnderstanding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: ModelId,
    /// Registry key, e.g. "qwen2.5-3b-instruct-q4_k_m". Not a vendor type.
    pub slug: String,
    pub family: String,
    pub provider: String,
    pub capabilities: Vec<ModelCapability>,
    pub context_length: u32,
    /// Quantization label, e.g. "q4_k_m", "fp16".
    pub quantization: Option<String>,
    pub size_bytes: u64,
    /// Minimum hardware to load this model.
    pub requirements: ModelRequirements,
    pub local: bool,
    /// SPDX id of the model's license (must permit the intended use).
    pub license: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRequirements {
    pub min_ram_bytes: Option<u64>,
    pub min_vram_bytes: Option<u64>,
    pub accelerators: Vec<Accelerator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accelerator {
    Cpu,
    Gpu,
    Npu,
}

// ---------------------------------------------------------------------------
// Agents / tools / tasks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub description: String,
    pub purpose: String,
    /// Tool names this agent may ever invoke (still subject to permissions).
    pub tools: Vec<String>,
    /// Memory scopes this agent can read/write.
    pub memory_scopes: Vec<MemoryScope>,
    pub execution_policy: ExecutionPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: AgentRunId,
    pub agent: AgentId,
    pub conversation: Option<ConversationId>,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub state: RunState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    AwaitingApproval,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub agent: AgentId,
    pub created_at: Timestamp,
    pub run_at: Option<Timestamp>,
    pub state: TaskState,
    pub sync_scope: SyncScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// How the platform treats a class of action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionPolicy {
    AlwaysAllow,
    AskUser,
    AllowWithRule,
    NeverAllow,
}

/// Whether state stays on one device or roams with the user's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncScope {
    #[default]
    DeviceLocal,
    Synchronized,
}

/// Coarse memory partition; agents get scoped access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Working,
    Episodic,
    Semantic,
    Procedural,
    Relationship,
}

// ---------------------------------------------------------------------------
// Memory / documents / media / misc domain objects
// ---------------------------------------------------------------------------

/// Provenance of a memory: did the user state it, or did the AI infer it?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    UserStated,
    AiInferred,
    Imported,
    SystemObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyLevel {
    Normal = 0,
    Sensitive = 1,
    Secret = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    pub title: Option<String>,
    pub mime: String,
    /// Blob-store reference to the raw file.
    pub blob: String,
    pub created_at: Timestamp,
    pub trust: TrustLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAsset {
    pub id: MediaAssetId,
    pub kind: MediaKind,
    pub blob: String,
    pub mime: String,
    pub created_at: Timestamp,
    pub produced_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Audio,
    Video,
}

/// An account/service connection (email, calendar, ...). Vendor-neutral.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connector {
    pub id: ConnectorId,
    pub kind: String,
    pub display_name: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: WorkflowId,
    pub name: String,
    pub trigger: Trigger,
    pub steps: Vec<WorkflowStep>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Schedule { cron: String },
    Event { source: String, event: String },
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub tool: String,
    pub arguments: serde_json::Value,
}

/// Unit of cross-device replication. Opaque to transports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncObject {
    /// Deterministic content id (device + object path + version).
    pub key: String,
    /// Encrypted payload. The sync layer cannot read plaintext.
    pub ciphertext: Vec<u8>,
    pub version: u64,
    pub writer: DeviceId,
    pub updated_at: Timestamp,
    pub tombstone: bool,
}

/// Immutable record of a meaningful platform action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: AuditEventId,
    pub at: Timestamp,
    pub device: Option<DeviceId>,
    pub agent: Option<AgentId>,
    pub run: Option<AgentRunId>,
    pub conversation: Option<ConversationId>,
    pub kind: AuditKind,
    /// Tool name when kind involves a tool.
    pub tool: Option<String>,
    /// Redacted argument summary — never raw secrets.
    pub detail: serde_json::Value,
    pub outcome: AuditOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    MessageSent,
    RunStarted,
    RunFinished,
    ModelResponded,
    MemoryWritten,
    MemoryDeleted,
    ToolRequested,
    ToolAllowed,
    ToolDenied,
    ToolExecuted,
    ModelLoaded,
    SyncReceived,
    SyncSent,
    PermissionChanged,
    ApprovalRequested,
    ApprovalResolved,
    AppDeployed,
    AppRun,
    AppRemoved,
    AppBackedUp,
    AppRestored,
    AppMigrated,
    AppRescued,
    AppShared,
    AppShareRevoked,
    AppAuthConfigured,
    AppServed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Ok,
    Denied,
    Error,
    Cancelled,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("approval required for {0}")]
    ApprovalRequired(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("sync error: {0}")]
    Sync(String),
    #[error("cancelled")]
    Cancelled,
    #[error("timeout")]
    Timeout,
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Arbitrary metadata bag used sparingly across the domain.
pub type Metadata = BTreeMap<String, serde_json::Value>;

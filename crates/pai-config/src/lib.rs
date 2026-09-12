//! Central configuration. One file, one schema, loaded once at startup.
//! Subsystems receive their section; nothing reads ad-hoc config elsewhere.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where inference may run. The user controls this globally and per task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ComputePolicy {
    /// Never leave the local device. Fully offline-capable.
    #[default]
    LocalOnly,
    /// Prefer local; may fall back to trusted devices on the LAN.
    LocalPreferred,
    /// Allow routing to trusted local-network devices (home server).
    TrustedLocalNetwork,
    /// Remote providers allowed when local cannot satisfy the request.
    CloudAllowed,
    /// Prefer remote providers (still routed through the broker/policy).
    CloudPreferred,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub data_dir: PathBuf,
    pub inference: InferenceConfig,
    pub memory: MemoryConfig,
    pub sync: SyncConfig,
    pub logging: LoggingConfig,
    pub voice: VoiceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceConfig {
    /// Registry slug of the default text model.
    pub default_model: String,
    pub fallback_model: Option<String>,
    pub compute_policy: ComputePolicy,
    /// Base URL of a local OpenAI-compatible inference server
    /// (llama.cpp `llama-server`, Ollama, LM Studio — all free/local).
    pub local_server_url: String,
    /// Hard ceiling on agent loop steps.
    pub max_agent_steps: u32,
    /// Per-request generation timeout, seconds.
    pub request_timeout_secs: u64,
    pub temperature: f32,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub enabled: bool,
    /// Max items surfaced to a single request context.
    pub recall_limit: u32,
    /// Minimum importance (0..1) for semantic memories to persist.
    pub min_importance: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub enabled: bool,
    /// Transport the sync engine uses. "folder" is the free/local default.
    pub transport: String,
    /// For the folder transport: directory shared via Syncthing/rsync/etc.
    pub folder: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// e.g. "info", "debug". Never logs message content at any level.
    pub level: String,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceConfig {
    pub enabled: bool,
    pub stt_model: Option<String>,
    pub tts_model: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            inference: InferenceConfig {
                default_model: "smollm2-360m-instruct-q4".into(),
                fallback_model: None,
                compute_policy: ComputePolicy::LocalOnly,
                local_server_url: "http://127.0.0.1:8080".into(),
                max_agent_steps: 8,
                request_timeout_secs: 120,
                temperature: 0.2,
                max_tokens: 512,
            },
            memory: MemoryConfig {
                enabled: true,
                recall_limit: 8,
                min_importance: 0.1,
            },
            sync: SyncConfig {
                enabled: false,
                transport: "folder".into(),
                folder: None,
            },
            logging: LoggingConfig {
                level: "info".into(),
                json: false,
            },
            voice: VoiceConfig {
                enabled: false,
                stt_model: None,
                tts_model: None,
            },
        }
    }
}

pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("personal-ai")
}

impl Config {
    pub fn load(path: Option<&Path>) -> pai_core::Result<Self> {
        let path = path
            .map(PathBuf::from)
            .unwrap_or_else(|| default_data_dir().join("config.toml"));
        if !path.exists() {
            let cfg = Config::default();
            cfg.save(&path)?;
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| pai_core::Error::InvalidInput(format!("read {path:?}: {e}")))?;
        toml::from_str(&raw)
            .map_err(|e| pai_core::Error::InvalidInput(format!("parse {path:?}: {e}")))
    }

    pub fn save(&self, path: &Path) -> pai_core::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| pai_core::Error::Storage(format!("mkdir {parent:?}: {e}")))?;
        }
        let raw = toml::to_string_pretty(self)
            .map_err(|e| pai_core::Error::InvalidInput(e.to_string()))?;
        std::fs::write(path, raw)
            .map_err(|e| pai_core::Error::Storage(format!("write {path:?}: {e}")))
    }
}

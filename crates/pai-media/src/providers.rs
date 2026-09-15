//! Concrete media-generation providers.
//!
//! Configuration lives in `<data_dir>/media.json` ([`MediaConfig`]) —
//! mirroring `voice.json`. Audio generation rides the same convention as
//! `whisper-server`: a local process holds the model (MusicGen,
//! stable-audio, or a remote bridge) and serves HTTP.

use async_trait::async_trait;
use pai_core::*;
use pai_inference::AudioGenerationProvider;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Default port convention: whisper.cpp takes 8178 when co-hosted with
/// llama-server; 8179 is the audio-generation server's slot.
pub const DEFAULT_AUDIO_GEN_URL: &str = "http://127.0.0.1:8179";

/// Text-to-audio over a running local HTTP server.
///
/// Protocol (deliberately minimal — any backend can front it):
/// `POST {url}/generate` with JSON `{prompt, duration_seconds}`; the
/// response body is the encoded audio (WAV expected; the `Content-Type`
/// is advisory). Generation is slow — timeout defaults to 10 min.
pub struct HttpAudioGen {
    base_url: String,
    client: reqwest::Client,
    timeout: Duration,
}

impl HttpAudioGen {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(600),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Probe the server (any HTTP response = alive).
    pub async fn detect(base_url: &str, timeout: Duration) -> Option<Self> {
        let p = Self::new(base_url);
        let resp = p
            .client
            .get(&p.base_url)
            .timeout(timeout)
            .send()
            .await
            .ok()?;
        let _ = resp.status();
        Some(p)
    }
}

#[async_trait]
impl AudioGenerationProvider for HttpAudioGen {
    fn id(&self) -> &'static str {
        "http-audio-gen"
    }

    async fn generate_audio(&self, prompt: &str, duration_secs: u32) -> Result<Vec<u8>> {
        let resp = self
            .client
            .post(format!("{}/generate", self.base_url))
            .timeout(self.timeout)
            .json(&serde_json::json!({
                "prompt": prompt,
                "duration_seconds": duration_secs,
            }))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("audio-gen: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!("audio-gen HTTP {}", resp.status())));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| Error::Provider(format!("audio-gen body: {e}")))
    }
}

/// Media wiring persisted as `<data_dir>/media.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaConfig {
    /// Base URL of the audio-generation server (None = unset; detection
    /// also checks `PAI_AUDIO_GEN_URL`).
    #[serde(default)]
    pub audio_gen_url: Option<String>,
}

impl MediaConfig {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let f = data_dir.join("media.json");
        if !f.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&f).map_err(|e| Error::Storage(e.to_string()))?;
        serde_json::from_str(&raw).map_err(|e| Error::InvalidInput(format!("bad media.json: {e}")))
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let raw =
            serde_json::to_string_pretty(self).map_err(|e| Error::InvalidInput(e.to_string()))?;
        std::fs::write(data_dir.join("media.json"), raw).map_err(|e| Error::Storage(e.to_string()))
    }
}

/// Effective audio-generation provider: configured URL (media.json) or
/// `PAI_AUDIO_GEN_URL`, probed for reachability. `None` = no backend.
pub async fn detect(data_dir: &Path, timeout: Duration) -> Option<HttpAudioGen> {
    let url = MediaConfig::load(data_dir)
        .ok()
        .and_then(|c| c.audio_gen_url)
        .or_else(|| std::env::var("PAI_AUDIO_GEN_URL").ok())
        .unwrap_or_else(|| DEFAULT_AUDIO_GEN_URL.to_string());
    HttpAudioGen::detect(&url, timeout).await
}

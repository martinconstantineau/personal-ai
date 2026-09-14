//! Concrete free/local voice providers.
//!
//! - [`WhisperServerStt`] — whisper.cpp's `whisper-server` HTTP endpoint
//!   (`POST /inference`, multipart WAV). Same local-server pattern as
//!   `llama-server`: the user runs the binary, we talk localhost HTTP.
//! - [`PiperTts`] — the `piper` CLI binary (`--output-raw` → stdout PCM).
//! - [`EnergyVad`] — dependency-free RMS energy gate on i16 frames.
//!
//! Configuration lives in `<data_dir>/voice.json` ([`VoiceConfig`]);
//! binaries are found on PATH or configured explicitly.

use async_trait::async_trait;
use pai_core::*;
use pai_inference::{
    find_in_path, SpeechToTextProvider, TextToSpeechProvider, VoiceActivityProvider,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Whisper (STT)
// ---------------------------------------------------------------------------

/// Default port convention: whisper.cpp's server defaults to 8080, which
/// collides with llama-server — 8178 is the common co-hosted choice.
pub const DEFAULT_WHISPER_URL: &str = "http://127.0.0.1:8178";

/// STT over a running `whisper-server` (whisper.cpp examples/server).
/// The server holds the model; we POST WAV bytes.
pub struct WhisperServerStt {
    base_url: String,
    client: reqwest::Client,
    timeout: Duration,
}

impl WhisperServerStt {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(120),
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
        // whisper-server's / returns the HTML UI; any reply means reachable.
        let _ = resp.status();
        Some(p)
    }
}

/// Minimal multipart/form-data body — whisper-server wants `file` (the
/// audio) and `response-format`. Hand-rolled to avoid pulling extra deps.
fn multipart_body(boundary: &str, audio: &[u8], mime: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(audio.len() + 512);
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"audio\"\r\nContent-Type: {mime}\r\n\r\n"
    );
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(audio);
    out.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\nContent-Disposition: form-data; \
             name=\"response-format\"\r\n\r\njson\r\n--{boundary}--\r\n"
        )
        .as_bytes(),
    );
    out
}

#[async_trait]
impl SpeechToTextProvider for WhisperServerStt {
    fn id(&self) -> &'static str {
        "whisper-server"
    }

    async fn transcribe(&self, audio: &[u8], mime: &str) -> Result<String> {
        let boundary = format!(
            "pai{:016x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        );
        let body = multipart_body(&boundary, audio, mime);
        let resp = self
            .client
            .post(format!("{}/inference", self.base_url))
            .timeout(self.timeout)
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("whisper-server: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "whisper-server HTTP {}",
                resp.status()
            )));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("whisper-server: {e}")))?;
        // whisper-server returns {"text": "..."} for response-format=json;
        // some builds nest segments — fall back to concatenating.
        if let Some(t) = v["text"].as_str() {
            return Ok(t.trim().to_string());
        }
        if let Some(parts) = v["segments"].as_array() {
            let t: String = parts
                .iter()
                .filter_map(|s| s["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if !t.is_empty() {
                return Ok(t.trim().to_string());
            }
        }
        Err(Error::Provider("whisper-server: empty transcript".into()))
    }
}

// ---------------------------------------------------------------------------
// Piper (TTS)
// ---------------------------------------------------------------------------

/// TTS via the `piper` binary: `piper --model voice.onnx --output-raw`
/// writes raw s16le/22050Hz/mono PCM to stdout, which we wrap in a WAV
/// header so callers get a directly playable file.
pub struct PiperTts {
    bin: PathBuf,
    model: PathBuf,
    timeout: Duration,
    /// PCM output rate — piper voices are 22050 Hz unless the model says
    /// otherwise (`--output-raw` doesn't resample).
    sample_rate: u32,
}

impl PiperTts {
    pub fn new(bin: impl Into<PathBuf>, model: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            model: model.into(),
            timeout: Duration::from_secs(60),
            sample_rate: 22_050,
        }
    }

    /// Discover piper on PATH + a model path. `model` may also come from
    /// `PAI_PIPER_MODEL`.
    pub fn detect(model: Option<PathBuf>) -> Option<Self> {
        let bin = find_in_path("piper")?;
        let model = model.or_else(|| std::env::var_os("PAI_PIPER_MODEL").map(PathBuf::from))?;
        Some(Self::new(bin, model))
    }

    pub fn with_sample_rate(mut self, hz: u32) -> Self {
        self.sample_rate = hz;
        self
    }
}

/// Wrap s16le mono PCM in a canonical 44-byte RIFF/WAVE header.
pub fn pcm16_to_wav(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[async_trait]
impl TextToSpeechProvider for PiperTts {
    fn id(&self) -> &'static str {
        "piper"
    }

    async fn synthesize(&self, text: &str, voice: Option<&str>) -> Result<Vec<u8>> {
        let bin = self.bin.clone();
        let model = self.model.clone();
        let text = text.to_string();
        let speaker = voice.and_then(|v| v.parse::<u32>().ok());
        let timeout = self.timeout;
        let rate = self.sample_rate;
        let pcm = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let mut cmd = std::process::Command::new(&bin);
            cmd.arg("--model")
                .arg(&model)
                .arg("--output-raw")
                .arg("--quiet");
            if let Some(s) = speaker {
                cmd.arg("--speaker").arg(s.to_string());
            }
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child = cmd
                .spawn()
                .map_err(|e| Error::Provider(format!("piper: {e}")))?;
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(text.as_bytes())
                    .map_err(|e| Error::Provider(format!("piper stdin: {e}")))?;
            }
            // Wait with timeout: poll rather than block forever.
            let deadline = std::time::Instant::now() + timeout;
            let output = loop {
                match child.try_wait() {
                    Ok(Some(_)) => break child.wait_with_output(),
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(25))
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        return Err(Error::Provider("piper: timed out".into()));
                    }
                    Err(e) => return Err(Error::Provider(format!("piper: {e}"))),
                }
            }
            .map_err(|e| Error::Provider(format!("piper: {e}")))?;
            if !output.status.success() {
                return Err(Error::Provider(format!(
                    "piper exited {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(output.stdout)
        })
        .await
        .map_err(|e| Error::Provider(format!("piper task: {e}")))??;
        Ok(pcm16_to_wav(&pcm, rate))
    }
}

// ---------------------------------------------------------------------------
// Energy VAD
// ---------------------------------------------------------------------------

/// Dependency-free VAD: a frame counts as speech when its RMS energy
/// exceeds `threshold` (0..32767 i16 scale). `hangover` frames of
/// sub-threshold audio still count as speech, so pauses inside an
/// utterance don't cut it short.
pub struct EnergyVad {
    threshold: f32,
    hangover: u32,
    left: std::sync::atomic::AtomicU32,
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self::new(500.0, 8)
    }
}

impl EnergyVad {
    pub fn new(threshold: f32, hangover_frames: u32) -> Self {
        Self {
            threshold,
            hangover: hangover_frames,
            left: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl VoiceActivityProvider for EnergyVad {
    fn id(&self) -> &'static str {
        "energy-vad"
    }

    async fn is_speech(&self, frame: &[i16]) -> Result<bool> {
        if frame.is_empty() {
            return Ok(false);
        }
        let mean_sq: f64 =
            frame.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / frame.len() as f64;
        let rms = mean_sq.sqrt() as f32;
        let hot = rms > self.threshold;
        use std::sync::atomic::Ordering::Relaxed;
        if hot {
            self.left.store(self.hangover, Relaxed);
            Ok(true)
        } else if self
            .left
            .fetch_update(Relaxed, Relaxed, |l| (l > 0).then(|| l - 1))
            .is_ok()
        {
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Voice wiring persisted as `<data_dir>/voice.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceConfig {
    /// whisper-server URL (`PAI_WHISPER_URL` env wins when set).
    #[serde(default)]
    pub whisper_url: Option<String>,
    /// piper binary path (PATH lookup when absent).
    #[serde(default)]
    pub piper_bin: Option<PathBuf>,
    /// piper voice model (.onnx). `PAI_PIPER_MODEL` env wins when set.
    #[serde(default)]
    pub piper_model: Option<PathBuf>,
}

impl VoiceConfig {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let f = data_dir.join("voice.json");
        if !f.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read(&f).map_err(|e| Error::Storage(e.to_string()))?;
        serde_json::from_slice(&raw)
            .map_err(|e| Error::InvalidInput(format!("bad voice.json: {e}")))
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir).map_err(|e| Error::Storage(e.to_string()))?;
        let raw = serde_json::to_vec_pretty(self).map_err(|e| Error::Storage(e.to_string()))?;
        std::fs::write(data_dir.join("voice.json"), raw).map_err(|e| Error::Storage(e.to_string()))
    }

    pub fn whisper_url(&self) -> String {
        std::env::var("PAI_WHISPER_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| self.whisper_url.clone())
            .unwrap_or_else(|| DEFAULT_WHISPER_URL.into())
    }
}

/// What `detect` found — STT and TTS are independent; a partial setup is
/// still useful (`voice say` only needs TTS).
pub struct VoiceSetup {
    pub stt: Option<WhisperServerStt>,
    pub tts: Option<PiperTts>,
    pub vad: EnergyVad,
    pub cfg: VoiceConfig,
}

impl VoiceSetup {
    /// Full pipeline — needs both ends reachable.
    pub fn pipeline(self) -> Option<crate::VoicePipeline> {
        Some(crate::VoicePipeline {
            vad: std::sync::Arc::new(self.vad),
            stt: std::sync::Arc::new(self.stt?),
            tts: std::sync::Arc::new(self.tts?),
        })
    }
}

/// Probe config + PATH for voice providers.
pub async fn detect(data_dir: &Path, probe: Duration) -> Result<VoiceSetup> {
    let cfg = VoiceConfig::load(data_dir)?;
    let stt = WhisperServerStt::detect(&cfg.whisper_url(), probe).await;
    let bin = cfg.piper_bin.clone().or_else(|| find_in_path("piper"));
    let model = cfg
        .piper_model
        .clone()
        .or_else(|| std::env::var_os("PAI_PIPER_MODEL").map(PathBuf::from));
    let tts = match (bin, model) {
        (Some(b), Some(m)) => Some(PiperTts::new(b, m)),
        _ => None,
    };
    Ok(VoiceSetup {
        stt,
        tts,
        vad: EnergyVad::default(),
        cfg,
    })
}

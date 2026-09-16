//! Concrete media-generation providers.
//!
//! Configuration lives in `<data_dir>/media.json` ([`MediaConfig`]) —
//! mirroring `voice.json`. Audio generation rides the same convention as
//! `whisper-server`: a local process holds the model (MusicGen,
//! stable-audio, or a remote bridge) and serves HTTP.

use async_trait::async_trait;
use pai_core::*;
use pai_inference::{
    AudioGenerationProvider, ImageGenerationProvider, JobStatus, VideoGenerationProvider,
};
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
    /// Base URL of the image-generation server (`PAI_IMAGE_GEN_URL`).
    #[serde(default)]
    pub image_gen_url: Option<String>,
    /// Which adapter fronts `image_gen_url`: `sdcpp` (stable-diffusion.cpp's
    /// OpenAI-images-compatible server, the default) or `onnx`
    /// (diffusers-onnx / any server taking the minimal `POST /generate`
    /// convention).
    #[serde(default)]
    pub image_backend: Option<String>,
    /// Base URL of the video-generation server (`PAI_VIDEO_GEN_URL`) —
    /// the async job contract: `POST /generate` → `{job_id}`, then
    /// `GET /jobs/{id}` → `{status, result_b64?, error?}`.
    #[serde(default)]
    pub video_gen_url: Option<String>,
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

// ---------------------------------------------------------------------------
// Image generation — stable-diffusion.cpp + diffusers-onnx adapters.
// ---------------------------------------------------------------------------

/// stable-diffusion.cpp's HTTP server speaks the OpenAI images API:
/// `POST {url}/v1/images/generations {prompt, size}` →
/// `{data: [{b64_json}]}`. Implemented by `sd --server` and the
/// sd.cpp-webui backends.
pub struct SdCppImageGen {
    base_url: String,
    client: reqwest::Client,
    timeout: Duration,
}

impl SdCppImageGen {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(600),
        }
    }

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
impl ImageGenerationProvider for SdCppImageGen {
    fn id(&self) -> &'static str {
        "sdcpp-image-gen"
    }

    async fn generate_image(
        &self,
        prompt: &str,
        size: (u32, u32),
        input: Option<(&[u8], &str)>,
    ) -> Result<Vec<u8>> {
        use base64::Engine as _;
        let mut body = serde_json::json!({
            "prompt": prompt,
            "size": format!("{}x{}", size.0, size.1),
            "response_format": "b64_json",
        });
        if let Some((bytes, mime)) = input {
            // sd.cpp edits/upscales take the source image inline.
            body["image"] = serde_json::Value::String(format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ));
        }
        let resp = self
            .client
            .post(format!("{}/v1/images/generations", self.base_url))
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("image-gen: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!("image-gen HTTP {}", resp.status())));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("image-gen JSON: {e}")))?;
        let b64 = v["data"]
            .get(0)
            .and_then(|d| d["b64_json"].as_str())
            .ok_or_else(|| Error::Provider("image-gen reply missing data[0].b64_json".into()))?;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| Error::Provider(format!("image-gen b64: {e}")))
    }
}

/// diffusers-onnx (and the reference `services/media-gen` server): the
/// same minimal convention as audio — `POST {url}/generate {prompt,
/// width, height, image_b64?}` → raw image bytes.
pub struct OnnxImageGen {
    base_url: String,
    client: reqwest::Client,
    timeout: Duration,
}

impl OnnxImageGen {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(600),
        }
    }

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
impl ImageGenerationProvider for OnnxImageGen {
    fn id(&self) -> &'static str {
        "onnx-image-gen"
    }

    async fn generate_image(
        &self,
        prompt: &str,
        size: (u32, u32),
        input: Option<(&[u8], &str)>,
    ) -> Result<Vec<u8>> {
        use base64::Engine as _;
        let mut body = serde_json::json!({
            "prompt": prompt,
            "width": size.0,
            "height": size.1,
        });
        if let Some((bytes, _mime)) = input {
            body["image_b64"] =
                serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
        let resp = self
            .client
            .post(format!("{}/generate", self.base_url))
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("image-gen: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!("image-gen HTTP {}", resp.status())));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| Error::Provider(format!("image-gen body: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Video generation — async job contract: submit returns a handle, poll
// until done.
// ---------------------------------------------------------------------------

/// diffusers-onnx video (or the reference server): `POST {url}/generate
/// {prompt}` → `{job_id}`; `GET {url}/jobs/{id}` → `{status,
/// result_b64?, error?}` (MP4 bytes in `result_b64`).
pub struct OnnxVideoGen {
    base_url: String,
    client: reqwest::Client,
    timeout: Duration,
}

impl OnnxVideoGen {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(1800),
        }
    }

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
impl VideoGenerationProvider for OnnxVideoGen {
    fn id(&self) -> &'static str {
        "onnx-video-gen"
    }

    async fn submit(&self, prompt: &str) -> Result<String> {
        let resp = self
            .client
            .post(format!("{}/generate", self.base_url))
            .timeout(Duration::from_secs(60))
            .json(&serde_json::json!({"prompt": prompt, "kind": "video"}))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("video-gen: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!("video-gen HTTP {}", resp.status())));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("video-gen JSON: {e}")))?;
        v["job_id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Provider("video-gen reply missing job_id".into()))
    }

    async fn poll(&self, job: &str) -> Result<JobStatus> {
        let resp = self
            .client
            .get(format!("{}/jobs/{}", self.base_url, job))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("video-gen poll: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "video-gen poll HTTP {}",
                resp.status()
            )));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("video-gen poll JSON: {e}")))?;
        match v["status"].as_str().unwrap_or("failed") {
            "queued" => Ok(JobStatus::Queued),
            "running" => Ok(JobStatus::Running),
            "done" => Ok(JobStatus::Done),
            s => Err(Error::Provider(format!(
                "video-gen job failed: {}",
                v["error"].as_str().unwrap_or(s)
            ))),
        }
    }
}

/// Completed video bytes — `poll` only reports status, so the result is
/// fetched separately once the job is done.
pub async fn video_result(p: &OnnxVideoGen, job: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let resp = p
        .client
        .get(format!("{}/jobs/{}", p.base_url, job))
        .timeout(p.timeout)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("video-gen result: {e}")))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("video-gen result JSON: {e}")))?;
    let b64 = v["result_b64"]
        .as_str()
        .ok_or_else(|| Error::Provider("video-gen job has no result_b64".into()))?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| Error::Provider(format!("video-gen result b64: {e}")))
}

/// Effective image-generation provider. `image_backend` picks the
/// adapter: `sdcpp` (default) or `onnx`. `None` = no backend.
pub async fn detect_image(
    data_dir: &Path,
    timeout: Duration,
) -> Option<std::sync::Arc<dyn ImageGenerationProvider>> {
    let c = MediaConfig::load(data_dir).ok()?;
    let url = c
        .image_gen_url
        .or_else(|| std::env::var("PAI_IMAGE_GEN_URL").ok())?;
    match c.image_backend.as_deref().unwrap_or("sdcpp") {
        "onnx" => OnnxImageGen::detect(&url, timeout)
            .await
            .map(|p| std::sync::Arc::new(p) as std::sync::Arc<dyn ImageGenerationProvider>),
        _ => SdCppImageGen::detect(&url, timeout)
            .await
            .map(|p| std::sync::Arc::new(p) as std::sync::Arc<dyn ImageGenerationProvider>),
    }
}

/// Effective video-generation provider. `None` = no backend.
pub async fn detect_video(data_dir: &Path, timeout: Duration) -> Option<OnnxVideoGen> {
    let url = MediaConfig::load(data_dir)
        .ok()
        .and_then(|c| c.video_gen_url)
        .or_else(|| std::env::var("PAI_VIDEO_GEN_URL").ok())?;
    OnnxVideoGen::detect(&url, timeout).await
}

/// True when at least one media-generation backend is reachable — used
/// for `media-run` advertisement (the op dispatches by `kind`).
pub async fn any_media_backend(data_dir: &Path, timeout: Duration) -> bool {
    detect(data_dir, timeout).await.is_some()
        || detect_image(data_dir, timeout).await.is_some()
        || detect_video(data_dir, timeout).await.is_some()
}

/// Reachability check for the backend `kind` would dispatch to —
/// callers use it to fail before claiming work when nothing is wired.
pub async fn detect_for_kind(
    data_dir: &Path,
    kind: crate::MediaJobKind,
    timeout: Duration,
) -> Result<()> {
    let what = match kind {
        crate::MediaJobKind::TextToAudio => "audio-gen",
        crate::MediaJobKind::TextToImage
        | crate::MediaJobKind::ImageEdit
        | crate::MediaJobKind::Upscale => "image-gen",
        crate::MediaJobKind::TextToVideo => "video-gen",
    };
    let found = match kind {
        crate::MediaJobKind::TextToAudio => detect(data_dir, timeout).await.is_some(),
        crate::MediaJobKind::TextToImage
        | crate::MediaJobKind::ImageEdit
        | crate::MediaJobKind::Upscale => detect_image(data_dir, timeout).await.is_some(),
        crate::MediaJobKind::TextToVideo => detect_video(data_dir, timeout).await.is_some(),
    };
    if found {
        Ok(())
    } else {
        Err(Error::Provider(format!("no {what} server on this device")))
    }
}

/// Run one media job locally and return `(bytes, mime)`. This is the
/// single dispatch point for `media_run_op`, `pai_media_gen`, and the
/// CLI — the job kind picks the provider.
pub async fn generate(
    data_dir: &Path,
    kind: crate::MediaJobKind,
    prompt: &str,
    duration_secs: u32,
    size: (u32, u32),
    input: Option<(Vec<u8>, String)>,
) -> Result<(Vec<u8>, &'static str)> {
    match kind {
        crate::MediaJobKind::TextToAudio => {
            let gen = detect(data_dir, Duration::from_secs(2))
                .await
                .ok_or_else(|| Error::Provider("no audio-gen server on this device".into()))?;
            gen.generate_audio(prompt, duration_secs)
                .await
                .map(|b| (b, "audio/wav"))
        }
        crate::MediaJobKind::TextToImage
        | crate::MediaJobKind::ImageEdit
        | crate::MediaJobKind::Upscale => {
            let gen = detect_image(data_dir, Duration::from_secs(2))
                .await
                .ok_or_else(|| Error::Provider("no image-gen server on this device".into()))?;
            let inp = input.as_ref().map(|(b, m)| (b.as_slice(), m.as_str()));
            gen.generate_image(prompt, size, inp)
                .await
                .map(|b| (b, "image/png"))
        }
        crate::MediaJobKind::TextToVideo => {
            let gen = detect_video(data_dir, Duration::from_secs(2))
                .await
                .ok_or_else(|| Error::Provider("no video-gen server on this device".into()))?;
            let job = gen.submit(prompt).await?;
            let deadline = std::time::Instant::now() + Duration::from_secs(1800);
            loop {
                if std::time::Instant::now() > deadline {
                    return Err(Error::Provider("video-gen timed out".into()));
                }
                match gen.poll(&job).await? {
                    JobStatus::Done => {
                        return video_result(&gen, &job).await.map(|b| (b, "video/mp4"))
                    }
                    JobStatus::Failed => {
                        return Err(Error::Provider("video-gen job failed".into()))
                    }
                    _ => tokio::time::sleep(Duration::from_secs(5)).await,
                }
            }
        }
    }
}

//! Process-based vision adapter — `ProcessVisionProvider`.
//!
//! Same external-boundary philosophy as the rest of the stack (whisper-
//! server, piper, llama-server): the VLM runtime is a user-configured
//! subprocess, not a linked library — so ONNX Runtime CLIs, MLX-VLM on
//! Apple silicon, `llama-mtmd-cli`, or any other free/open-weight runner
//! plugs in without new native deps (and without dragging ONNX's C++
//! toolchain onto windows-gnu).
//!
//! Config lives in `<data_dir>/vision.json`:
//! ```json
//! {
//!   "process": {
//!     "command": "python",
//!     "args": ["-m", "mlx_vlm.generate", "--model",
//!              "mlx-community/Qwen2-VL-2B-Instruct-4bit",
//!              "--image", "{image}", "--prompt", "{prompt}"],
//!     "timeout_secs": 300
//!   }
//! }
//! ```
//! `{image}` expands to a temp-file path holding the image bytes;
//! `{prompt}` to the prompt text. Both are single argv entries — never
//! shell-interpreted. stdout becomes the description; non-zero exit or
//! timeout surfaces stderr as the error.

use async_trait::async_trait;
use pai_core::*;
use pai_inference::{find_in_path, ImageUnderstandingProvider};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// One external VLM runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessVisionConfig {
    /// Binary to spawn — resolved against PATH at detect time.
    pub command: String,
    /// Argv after the command. `{image}` and `{prompt}` placeholders are
    /// substituted per call; args without placeholders pass through.
    #[serde(default)]
    pub args: Vec<String>,
    /// Per-call wall-clock cap. First-run model loads can be slow —
    /// default 300s, raise for big checkpoints.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    300
}

/// `vision.json` — currently just the process block; llama-server stays
/// the implicit default so old setups need no file at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VisionFileConfig {
    #[serde(default)]
    pub process: Option<ProcessVisionConfig>,
}

impl VisionFileConfig {
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let f = data_dir.join("vision.json");
        if !f.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&f).map_err(|e| Error::Storage(e.to_string()))?;
        let cfg = serde_json::from_slice(&raw)
            .map_err(|e| Error::InvalidInput(format!("bad vision.json: {e}")))?;
        Ok(Some(cfg))
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir).map_err(|e| Error::Storage(e.to_string()))?;
        let raw = serde_json::to_vec_pretty(self).map_err(|e| Error::Storage(e.to_string()))?;
        std::fs::write(data_dir.join("vision.json"), raw).map_err(|e| Error::Storage(e.to_string()))
    }
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "png",
    }
}

pub struct ProcessVisionProvider {
    cfg: ProcessVisionConfig,
    /// Resolved command path (found once at construct time).
    binary: PathBuf,
}

impl ProcessVisionProvider {
    /// Construct when `command` resolves on PATH — None otherwise.
    pub fn detect(cfg: ProcessVisionConfig) -> Option<Self> {
        find_in_path(&cfg.command).map(|binary| Self { cfg, binary })
    }

    /// Blocking describe — run inside `spawn_blocking`. Writes the image
    /// to a temp file, substitutes placeholders, and waits up to
    /// `timeout_secs` (killing the child on expiry).
    pub fn describe_blocking(&self, image: &[u8], mime: &str, prompt: &str) -> Result<String> {
        let dir = std::env::temp_dir().join(format!("pai-vision-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).map_err(|e| Error::Storage(e.to_string()))?;
        let image_path = dir.join(format!("image.{}", ext_for_mime(mime)));
        let cleanup = |dir: &PathBuf| {
            let _ = std::fs::remove_dir_all(dir);
        };
        let mut f = std::fs::File::create(&image_path).map_err(|e| {
            cleanup(&dir);
            Error::Storage(format!("vision temp: {e}"))
        })?;
        f.write_all(image).map_err(|e| {
            cleanup(&dir);
            Error::Storage(format!("vision temp: {e}"))
        })?;
        drop(f);

        let args: Vec<String> = self
            .cfg
            .args
            .iter()
            .map(|a| {
                a.replace("{image}", &image_path.to_string_lossy())
                    .replace("{prompt}", prompt)
            })
            .collect();
        let mut child = std::process::Command::new(&self.binary)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                cleanup(&dir);
                Error::Provider(format!("vision process spawn: {e}"))
            })?;

        let deadline = Instant::now() + Duration::from_secs(self.cfg.timeout_secs.max(1));
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        cleanup(&dir);
                        return Err(Error::Provider(format!(
                            "vision process timed out after {}s",
                            self.cfg.timeout_secs
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    cleanup(&dir);
                    return Err(Error::Provider(format!("vision process wait: {e}")));
                }
            }
        };
        let mut out = String::new();
        if let Some(mut so) = child.stdout.take() {
            use std::io::Read;
            let _ = so.read_to_string(&mut out);
        }
        let mut err_buf = String::new();
        if let Some(mut se) = child.stderr.take() {
            use std::io::Read;
            let _ = se.read_to_string(&mut err_buf);
        }
        cleanup(&dir);
        if !status.success() {
            let detail = err_buf.trim();
            return Err(Error::Provider(format!(
                "vision process exited {status}: {}",
                if detail.is_empty() {
                    "no stderr"
                } else {
                    detail
                }
            )));
        }
        let text = out.trim().to_string();
        if text.is_empty() {
            return Err(Error::Provider(format!(
                "vision process produced no output{}",
                if err_buf.trim().is_empty() {
                    String::new()
                } else {
                    format!(" (stderr: {})", err_buf.trim())
                }
            )));
        }
        Ok(text)
    }
}

#[async_trait]
impl ImageUnderstandingProvider for ProcessVisionProvider {
    fn id(&self) -> &'static str {
        "process-vision"
    }

    async fn describe(&self, image: &[u8], mime: &str, prompt: &str) -> Result<String> {
        let me = Self {
            cfg: self.cfg.clone(),
            binary: self.binary.clone(),
        };
        let img = image.to_vec();
        let mime = mime.to_string();
        let prompt = prompt.to_string();
        tokio::task::spawn_blocking(move || me.describe_blocking(&img, &mime, &prompt))
            .await
            .map_err(|e| Error::Provider(format!("vision task: {e}")))?
    }
}

//! Process-boundary inference — on-device backends without linked
//! runtimes.
//!
//! ExecuTorch runners, MLC-LLM CLIs, `llama-cli`, `llama-mtmd-cli`, or
//! any local generator plugs in via `inference.json` in the data dir:
//!
//! ```json
//! {"process": {"command": "llama-cli",
//!              "args": ["-m", "model.gguf", "-n", "512", "-p", "{prompt}"],
//!              "model": "qwen2.5-0.5b", "timeout_secs": 300}}
//! ```
//!
//! `{prompt}` inside `args` is substituted with the rendered prompt as a
//! *single* argv element — never shell-interpreted. When no arg carries
//! `{prompt}`, the rendered prompt is piped to stdin instead (covers
//! runners that read stdin). stdout becomes the completion text; a
//! non-zero exit surfaces stderr as the error.

use crate::{
    find_in_path, parse_action, AIRequest, EventStream, GenerateResponse, InferenceProvider,
    ModelCapability, StreamEvent,
};
use pai_core::*;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// `inference.json` — the user's on-device runner config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceFileConfig {
    pub process: Option<ProcessInferenceConfig>,
}

impl InferenceFileConfig {
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let p = data_dir.join("inference.json");
        if !p.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&p)
            .map_err(|e| Error::InvalidInput(format!("inference.json: {e}")))?;
        serde_json::from_str(&raw).map_err(|e| Error::InvalidInput(format!("inference.json: {e}")))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInferenceConfig {
    /// Binary name (resolved via PATH) or absolute path.
    pub command: String,
    /// argv template — `{prompt}` elements are substituted per request.
    #[serde(default)]
    pub args: Vec<String>,
    /// Model name reported back in responses.
    #[serde(default)]
    pub model: Option<String>,
    /// Wall-clock kill bound — first run may include a model load.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    300
}

/// Flatten the request into a single prompt string for CLI runners —
/// tool specs are embedded as a JSON preamble so capable models can
/// still emit the `{"action": ...}` protocol.
pub fn render_prompt(req: &AIRequest) -> String {
    let mut out = String::new();
    if !req.tools.is_empty() {
        let specs: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        out.push_str("tools:\n");
        out.push_str(&serde_json::to_string(&specs).unwrap_or_default());
        out.push('\n');
    }
    for m in &req.messages {
        for c in &m.content {
            if let Some(t) = c.as_text() {
                out.push_str(&format!("{:?}: {}\n", m.role, t));
            }
        }
    }
    out
}

/// Spawn the runner: `{prompt}` args substituted (single argv element,
/// never shell-interpreted); no placeholder → prompt piped to stdin.
fn spawn_process(bin: &Path, args: &[String], prompt: &str) -> Result<Child> {
    let mut cmd = Command::new(bin);
    let mut used_placeholder = false;
    for a in args {
        if a.contains("{prompt}") {
            cmd.arg(a.replace("{prompt}", prompt));
            used_placeholder = true;
        } else {
            cmd.arg(a);
        }
    }
    if !used_placeholder {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Provider(format!("{}: {e}", bin.display())))?;
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prompt.to_string();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(prompt.as_bytes());
        });
    }
    Ok(child)
}

/// Wait for the child to finish, capturing stdout/stderr, killed at
/// `timeout`. Blocking — call from `spawn_blocking` or a thread.
fn wait_capture(
    child: &mut Child,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String, String)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut out);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut err);
                }
                return Ok((status, out, err));
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::Timeout);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(Error::Other(e.to_string())),
        }
    }
}

pub struct ProcessInferenceProvider {
    cfg: ProcessInferenceConfig,
    /// Resolved binary path (PATH lookup at detect()).
    bin: PathBuf,
}

impl ProcessInferenceProvider {
    /// Build from a config whose command resolves — None when the
    /// binary isn't on PATH (detect-time gate, mirrors vision adapter).
    pub fn detect(cfg: ProcessInferenceConfig) -> Option<Self> {
        let p = Path::new(&cfg.command);
        let bin = if p.is_absolute() && p.exists() {
            p.to_path_buf()
        } else {
            find_in_path(&cfg.command)?
        };
        Some(Self { cfg, bin })
    }
}

#[async_trait::async_trait]
impl InferenceProvider for ProcessInferenceProvider {
    fn id(&self) -> &'static str {
        "process"
    }

    fn capabilities(&self) -> Vec<ModelCapability> {
        // TextGeneration only — whether the served model speaks the
        // action protocol is unknowable from the config; parse_action
        // treats free text as Final so nothing depends on it.
        vec![ModelCapability::TextGeneration]
    }

    async fn available_models(&self) -> Result<Vec<String>> {
        Ok(vec![self
            .cfg
            .model
            .clone()
            .unwrap_or_else(|| "process".to_string())])
    }

    async fn generate(&self, request: &AIRequest) -> Result<GenerateResponse> {
        let prompt = render_prompt(request);
        let timeout = Duration::from_secs(self.cfg.timeout_secs);
        let bin = self.bin.clone();
        let args = self.cfg.args.clone();
        let model = self.cfg.model.clone();
        tokio::task::spawn_blocking(move || -> Result<GenerateResponse> {
            let mut child = spawn_process(&bin, &args, &prompt)?;
            let (status, out, err) = wait_capture(&mut child, timeout)?;
            if !status.success() {
                return Err(Error::Provider(format!(
                    "process exited {status}: {}",
                    err.trim()
                )));
            }
            let text = out.trim().to_string();
            Ok(GenerateResponse {
                action: parse_action(&text),
                text,
                model,
                ..Default::default()
            })
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
    }

    /// Real CLIs stream tokens to stdout incrementally — forward each
    /// line as a Delta as it arrives; Done carries the joined text
    /// (the runtime reads the answer from Done, not the deltas).
    fn stream<'a>(&'a self, request: AIRequest) -> EventStream<'a> {
        let prompt = render_prompt(&request);
        let model = self.cfg.model.clone();
        let timeout = Duration::from_secs(self.cfg.timeout_secs);
        match spawn_process(&self.bin, &self.cfg.args, &prompt) {
            Err(e) => Box::pin(futures::stream::once(async move {
                Ok(StreamEvent::Error(e.to_string()))
            })),
            Ok(mut child) => {
                let (tx, rx) = mpsc::channel::<std::result::Result<String, String>>();
                let mut stdout = child.stdout.take().expect("stdout piped");
                let mut stderr = child.stderr.take().expect("stderr piped");
                // Reader thread: forward each stdout line as it arrives.
                // Done fires when *all* senders drop, i.e. after the
                // waiter below also finishes.
                let rtx = tx.clone();
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(&mut stdout).lines() {
                        match line {
                            Ok(l) => {
                                if rtx.send(Ok(l)).is_err() {
                                    return; // consumer dropped
                                }
                            }
                            Err(e) => {
                                let _ = rtx.send(Err(e.to_string()));
                                return;
                            }
                        }
                    }
                });
                // Waiter thread: owns the deadline. Must not block on
                // reading the pipes — a hung process keeps them open.
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + timeout;
                    loop {
                        match child.try_wait() {
                            Ok(Some(st)) if st.success() => return,
                            Ok(Some(st)) => {
                                let mut err = String::new();
                                let _ = stderr.read_to_string(&mut err);
                                let _ =
                                    tx.send(Err(format!("process exited {st}: {}", err.trim())));
                                return;
                            }
                            Ok(None) => {
                                if std::time::Instant::now() > deadline {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    let _ = tx.send(Err("timed out".into()));
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            Err(e) => {
                                let _ = tx.send(Err(e.to_string()));
                                return;
                            }
                        }
                    }
                });
                // State: (rx, accumulated text, done emitted?). rx moves
                // into the blocking recv and comes back with the result.
                let stream = futures::stream::unfold(
                    (rx, String::new(), false),
                    move |(rx, mut acc, done)| {
                        let model = model.clone();
                        async move {
                            if done {
                                return None;
                            }
                            let (next, rx) = match tokio::task::spawn_blocking(move || {
                                let v = rx.recv();
                                (v, rx)
                            })
                            .await
                            {
                                Ok(v) => v,
                                Err(_) => return None,
                            };
                            match next {
                                Ok(Ok(line)) => {
                                    acc.push_str(&line);
                                    acc.push('\n');
                                    Some((
                                        Ok(StreamEvent::Delta(format!("{line}\n"))),
                                        (rx, acc, false),
                                    ))
                                }
                                Ok(Err(e)) => Some((Ok(StreamEvent::Error(e)), (rx, acc, false))),
                                Err(_) => {
                                    let text = acc.trim_end().to_string();
                                    Some((
                                        Ok(StreamEvent::Done(GenerateResponse {
                                            action: parse_action(&text),
                                            text,
                                            model,
                                            ..Default::default()
                                        })),
                                        (rx, acc, true),
                                    ))
                                }
                            }
                        }
                    },
                );
                Box::pin(stream)
            }
        }
    }
}

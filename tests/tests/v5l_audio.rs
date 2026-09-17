//! V5l tests: audio generation — `ModelCapability::AudioGeneration`,
//! `MediaJobKind::TextToAudio`, the `AudioGenerationProvider` trait, the
//! `HttpAudioGen` local-server adapter (POST {prompt, duration_seconds}
//! → audio bytes), and the `audio.generate` agent tool that lands
//! artifacts under the media dir.

use pai_core::*;
use pai_inference::AudioGenerationProvider;
use pai_media::providers::{HttpAudioGen, MediaConfig};
use pai_media::MediaJobKind;
use pai_permissions::Permission;
use pai_tools::{AudioGenerateTool, Tool, ToolContext};
use std::path::{Path, PathBuf};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5l-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One-shot HTTP stub: accepts a request, captures the body, replies with
/// the given bytes + content-type. Returns the bound URL.
fn stub_http(tag: &str, status: &'static str, body: &'static [u8]) -> (String, PathBuf) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let capture = tmpdir(tag).join("request-body.json");
    let cap = capture.clone();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        use std::io::{Read, Write};
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        // Read until headers+body complete (Content-Length aware).
        let mut len = None;
        loop {
            let n = s.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
                if len.is_none() {
                    let head = String::from_utf8_lossy(&buf[..p]);
                    len = Some(
                        head.lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0), // no Content-Length → no body
                    );
                }
                if buf.len() >= p + len.unwrap() {
                    break;
                }
            }
        }
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
            std::fs::write(&cap, &buf[p..]).unwrap();
        }
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        s.write_all(resp.as_bytes()).unwrap();
        s.write_all(body).unwrap();
    });
    (format!("http://127.0.0.1:{port}"), capture)
}

#[test]
fn capability_and_job_kind_serde() {
    let cap: ModelCapability = serde_json::from_str("\"audio_generation\"").unwrap();
    assert_eq!(cap, ModelCapability::AudioGeneration);
    assert_eq!(
        serde_json::to_string(&MediaJobKind::TextToAudio).unwrap(),
        "\"text_to_audio\""
    );
}

#[test]
fn media_config_roundtrip() {
    let dir = tmpdir("cfg");
    assert!(MediaConfig::load(&dir).unwrap().audio_gen_url.is_none());
    let c = MediaConfig {
        audio_gen_url: Some("http://127.0.0.1:9000".into()),
        ..MediaConfig::default()
    };
    c.save(&dir).unwrap();
    let c2 = MediaConfig::load(&dir).unwrap();
    assert_eq!(c2.audio_gen_url.as_deref(), Some("http://127.0.0.1:9000"));
    // Corrupt JSON → InvalidInput, not a panic.
    std::fs::write(dir.join("media.json"), "{nope").unwrap();
    assert!(MediaConfig::load(&dir).is_err());
}

#[tokio::test]
async fn http_audio_gen_roundtrip() {
    // RIFF header + a few bytes — enough to look like a WAV.
    let wav: &'static [u8] = b"RIFF$\0\0\0WAVEfmt fake-audio-bytes";
    let (url, cap) = stub_http("gen", "200 OK", wav);
    let gen = HttpAudioGen::new(&url);
    let out = gen
        .generate_audio("lofi rain on a tin roof", 15)
        .await
        .unwrap();
    assert_eq!(out, wav);
    // The server saw the minimal protocol: prompt + duration_seconds.
    let sent: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cap).unwrap()).unwrap();
    assert_eq!(sent["prompt"], "lofi rain on a tin roof");
    assert_eq!(sent["duration_seconds"], 15);
    assert_eq!(gen.id(), "http-audio-gen");
}

#[tokio::test]
async fn http_audio_gen_http_error() {
    let (url, _) = stub_http("err", "500 Internal Server Error", b"");
    let gen = HttpAudioGen::new(&url);
    let e = gen.generate_audio("x", 5).await.unwrap_err();
    assert!(e.to_string().contains("500"), "{e}");
}

#[tokio::test]
async fn detect_dead_server_is_none() {
    // Port 1 is never listening.
    assert!(
        HttpAudioGen::detect("http://127.0.0.1:1", std::time::Duration::from_millis(300))
            .await
            .is_none()
    );
    let (url, _) = stub_http("det", "404 Not Found", b"");
    // Any HTTP response = alive (mirrors whisper-server detect).
    assert!(
        HttpAudioGen::detect(&url, std::time::Duration::from_secs(2))
            .await
            .is_some()
    );
}

// --- audio.generate tool ---------------------------------------------------

struct StubGen {
    calls: std::sync::Mutex<Vec<(String, u32)>>,
}

#[async_trait::async_trait]
impl AudioGenerationProvider for StubGen {
    fn id(&self) -> &'static str {
        "stub-gen"
    }
    async fn generate_audio(&self, prompt: &str, duration_secs: u32) -> Result<Vec<u8>> {
        self.calls
            .lock()
            .unwrap()
            .push((prompt.to_string(), duration_secs));
        Ok(b"RIFF-stub".to_vec())
    }
}

fn ctx<'a>(
    gen: Option<&'a dyn AudioGenerationProvider>,
    media_dir: Option<&'a Path>,
) -> ToolContext<'a> {
    ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: None,
        email: None,
        gitlab: None,
        vision: None,
        notify: None,
        allowed_roots: &[],
        apps: None,
        audio_gen: gen,
        media_dir,
    }
}

#[tokio::test]
async fn audio_generate_tool_writes_artifact() {
    let media = tmpdir("media");
    let gen = StubGen {
        calls: std::sync::Mutex::new(vec![]),
    };
    let cx = ctx(Some(&gen), Some(&media));
    let tool = AudioGenerateTool;
    let d = tool.descriptor();
    assert_eq!(d.name, "audio.generate");
    assert_eq!(d.required_permissions, vec![Permission::MediaGenerate]);
    assert_eq!(d.risk, pai_permissions::RiskLevel::High);

    let out = tool
        .execute(
            serde_json::json!({
                "prompt": "dark synthwave bassline",
                "duration_secs": 20,
                "filename": "../escape/../evil.wav",
            }),
            &cx,
        )
        .await
        .unwrap();
    let path = PathBuf::from(out.value["path"].as_str().unwrap());
    // Path traversal stripped — file lands inside the media dir.
    assert_eq!(path.parent().unwrap(), media.as_path());
    assert_eq!(path.file_name().unwrap(), "_escape_.._evil.wav");
    assert_eq!(std::fs::read(&path).unwrap(), b"RIFF-stub");
    assert_eq!(out.value["duration_secs"], 20);
    assert_eq!(out.value["provider"], "stub-gen");
    let calls = gen.calls.lock().unwrap();
    assert_eq!(calls.as_slice(), &[("dark synthwave bassline".into(), 20)]);
}

#[tokio::test]
async fn audio_generate_tool_unavailable_and_missing_dir() {
    let tool = AudioGenerateTool;
    let media = tmpdir("none");
    // No provider → Provider error, not a panic.
    let e = tool
        .execute(serde_json::json!({"prompt": "x"}), &ctx(None, Some(&media)))
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("no audio-generation provider"),
        "{e}"
    );
    // Provider but no media dir → InvalidInput.
    let gen = StubGen {
        calls: std::sync::Mutex::new(vec![]),
    };
    let e = tool
        .execute(serde_json::json!({"prompt": "x"}), &ctx(Some(&gen), None))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no media dir"), "{e}");
}

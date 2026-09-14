//! V2c tests: energy VAD, WAV wrapping, whisper-server STT against a mock
//! HTTP endpoint, piper error path, full VoicePipeline turn with mocks.

use pai_core::*;
use pai_inference::{SpeechToTextProvider, TextToSpeechProvider, VoiceActivityProvider};
use pai_voice::*;
use std::io::{Read, Write};
use std::sync::Arc;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v2c-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn energy_vad_silence_and_speech() {
    let vad = EnergyVad::new(500.0, 3);
    let silence = vec![0i16; 160];
    assert!(!vad.is_speech(&silence).await.unwrap());
    assert!(!vad.is_speech(&[]).await.unwrap());

    // Loud tone (~14500 RMS) counts as speech.
    let loud: Vec<i16> = (0..160)
        .map(|i| if i % 2 == 0 { 20_000 } else { -20_000 })
        .collect();
    assert!(vad.is_speech(&loud).await.unwrap());

    // Hangover: a few quiet frames still count as speech inside an utterance.
    assert!(vad.is_speech(&silence).await.unwrap());
    assert!(vad.is_speech(&silence).await.unwrap());
    assert!(vad.is_speech(&silence).await.unwrap());
    assert!(!vad.is_speech(&silence).await.unwrap()); // hangover exhausted
}

#[test]
fn wav_header_is_wellformed() {
    let pcm = vec![0x11u8; 1000];
    let wav = pcm16_to_wav(&pcm, 22_050);
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(&wav[12..16], b"fmt ");
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 22_050);
    assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
    assert_eq!(&wav[36..40], b"data");
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 1000);
    assert_eq!(wav.len(), 1044);
}

/// One-shot mock HTTP server: read the request, respond `body`.
fn mock_http(body: &'static str, content_type: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        // Read headers, then Content-Length body.
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = 0usize;
        let mut content_len = 0usize;
        loop {
            let n = s.read(&mut buf[got..]).unwrap_or(0);
            if n == 0 {
                break;
            }
            got += n;
            let text = String::from_utf8_lossy(&buf[..got]);
            if let Some(pos) = text.find("\r\n\r\n") {
                let head = &text[..pos];
                for line in head.lines() {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                if got >= pos + 4 + content_len {
                    break;
                }
            }
            if got >= buf.len() {
                break;
            }
        }
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(resp.as_bytes()).ok();
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn whisper_stt_parses_json_response() {
    let url = mock_http(r#"{"text":"  hello from whisper  "}"#, "application/json");
    let stt = WhisperServerStt::new(url);
    let wav = pcm16_to_wav(&vec![0u8; 2000], 16_000);
    let text = stt.transcribe(&wav, "audio/wav").await.unwrap();
    assert_eq!(text, "hello from whisper");
}

#[tokio::test]
async fn whisper_stt_segments_fallback() {
    let url = mock_http(
        r#"{"segments":[{"text":"first"},{"text":"second"}]}"#,
        "application/json",
    );
    let stt = WhisperServerStt::new(url);
    let text = stt.transcribe(b"wav", "audio/wav").await.unwrap();
    assert_eq!(text, "first second");
}

#[tokio::test]
async fn piper_missing_binary_errors() {
    let tts = PiperTts::new("definitely-not-piper-xyz", "missing.onnx");
    let err = tts.synthesize("hi", None).await.unwrap_err();
    assert!(err.to_string().contains("piper"));
}

struct MockStt;
#[async_trait::async_trait]
impl SpeechToTextProvider for MockStt {
    fn id(&self) -> &'static str {
        "mock-stt"
    }
    async fn transcribe(&self, _a: &[u8], _m: &str) -> Result<String> {
        Ok("what is two plus two".into())
    }
}

struct MockTts;
#[async_trait::async_trait]
impl TextToSpeechProvider for MockTts {
    fn id(&self) -> &'static str {
        "mock-tts"
    }
    async fn synthesize(&self, text: &str, _v: Option<&str>) -> Result<Vec<u8>> {
        Ok(pcm16_to_wav(text.as_bytes(), 22_050))
    }
}

struct Echo;
#[async_trait::async_trait]
impl UtteranceHandler for Echo {
    async fn respond(&self, transcript: &str) -> Result<String> {
        Ok(format!("answer to '{transcript}'"))
    }
}

#[tokio::test]
async fn pipeline_turn_runs_end_to_end() {
    let p = VoicePipeline {
        vad: Arc::new(EnergyVad::default()),
        stt: Arc::new(MockStt),
        tts: Arc::new(MockTts),
    };
    let (transcript, speech) = p.turn(b"audio", "audio/wav", &Echo).await.unwrap();
    assert_eq!(transcript, "what is two plus two");
    // Speech is a WAV wrapping "answer to 'what is two plus two'".
    assert_eq!(&speech[..4], b"RIFF");
    assert!(String::from_utf8_lossy(&speech[44..]).contains("answer to"));
}

#[test]
fn voice_config_roundtrip_and_env() {
    let dir = tmpdir("cfg");
    let cfg = VoiceConfig {
        whisper_url: Some("http://127.0.0.1:9999".into()),
        piper_bin: Some("/opt/piper".into()),
        piper_model: Some("/opt/voice.onnx".into()),
    };
    cfg.save(&dir).unwrap();
    let back = VoiceConfig::load(&dir).unwrap();
    assert_eq!(back.whisper_url(), "http://127.0.0.1:9999");

    // Env override wins over the file.
    unsafe { std::env::set_var("PAI_WHISPER_URL", "http://x:1") };
    assert_eq!(back.whisper_url(), "http://x:1");
    unsafe { std::env::remove_var("PAI_WHISPER_URL") };

    // Absent file → default URL.
    let empty = VoiceConfig::load(&tmpdir("empty")).unwrap();
    assert_eq!(empty.whisper_url(), DEFAULT_WHISPER_URL);
}

//! V2d tests: llama.cpp multimodal adapter against a mock OpenAI endpoint,
//! mime guessing, error surfacing, and vision type roundtrips.

use base64::Engine;
use pai_inference::ImageUnderstandingProvider;
use pai_vision::*;
use std::io::{Read, Write};

/// Mock OpenAI server: capture the request body for assertions, reply
/// with `body`. Returns (url, captured_request).
fn mock_chat(body: &'static str) -> (String, std::sync::Arc<std::sync::Mutex<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let cap = captured.clone();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 1024 * 1024];
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
                for line in text[..pos].lines() {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                if got >= pos + 4 + content_len {
                    *cap.lock().unwrap() = text[pos + 4..pos + 4 + content_len].to_string();
                    break;
                }
            }
            if got >= buf.len() {
                break;
            }
        }
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(resp.as_bytes()).ok();
    });
    (format!("http://127.0.0.1:{port}"), captured)
}

#[tokio::test]
async fn describe_posts_data_uri_and_parses_reply() {
    let (url, captured) =
        mock_chat(r#"{"choices":[{"message":{"content":"a red square on white"}}]}"#);
    let p = LlamaVisionProvider::new(url, "llava");
    let png = b"\x89PNG-fake-bytes".to_vec();
    let text = p
        .describe(&png, "image/png", "What is shown?")
        .await
        .unwrap();
    assert_eq!(text, "a red square on white");

    // The request carried the image as an OpenAI image_url data URI.
    let req = captured.lock().unwrap().clone();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    assert!(req.contains("image_url"), "request: {req}");
    assert!(req.contains(&format!("data:image/png;base64,{b64}")));
    assert!(req.contains("What is shown?"));
}

#[tokio::test]
async fn describe_error_mentions_mmproj() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        if s.read(&mut buf).unwrap_or(0) == 0 {
            return;
        }
        let body = "no multimodal projector";
        let resp = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(resp.as_bytes()).ok();
    });
    let p = LlamaVisionProvider::new(format!("http://127.0.0.1:{port}"), "m");
    let err = p.describe(b"x", "image/png", "describe").await.unwrap_err();
    assert!(err.to_string().contains("mmproj"), "{err}");
}

#[test]
fn mime_guessing() {
    assert_eq!(mime_for_ext("png"), Some("image/png"));
    assert_eq!(mime_for_ext("jpeg"), Some("image/jpeg"));
    assert_eq!(mime_for_ext("gif"), Some("image/gif"));
    assert_eq!(mime_for_ext("tiff"), None);
}

struct MockVision;
#[async_trait::async_trait]
impl ImageUnderstandingProvider for MockVision {
    fn id(&self) -> &'static str {
        "mock-vision"
    }
    async fn describe(&self, image: &[u8], _mime: &str, prompt: &str) -> pai_core::Result<String> {
        Ok(format!("saw {} bytes for '{prompt}'", image.len()))
    }
}

#[tokio::test]
async fn vision_describe_tool_reads_jailed_image() {
    use pai_tools::{Tool, ToolContext};
    let dir = std::env::temp_dir().join(format!("pai-v2d-jail-{}", uuid::Uuid::new_v4()));
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    std::fs::write(inbox.join("pic.png"), b"\x89PNG-data").unwrap();

    let v = MockVision;
    let ctx = ToolContext {
        run: pai_core::AgentRunId::new(),
        device: pai_core::DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: None,
        email: None,
        vision: Some(&v),
        notify: None,
        apps: None,
        audio_gen: None,
        media_dir: None,
        allowed_roots: std::slice::from_ref(&inbox),
    };
    let out = pai_tools::VisionDescribe
        .execute(
            serde_json::json!({"path": inbox.join("pic.png").to_string_lossy(), "prompt": "what?"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out.value["text"], "saw 9 bytes for 'what?'");

    // Jail: outside paths are denied.
    let err = pai_tools::VisionDescribe
        .execute(
            serde_json::json!({"path": dir.join("pic.png").to_string_lossy()}),
            &ctx,
        )
        .await;
    assert!(err.is_err());
}

#[test]
fn vision_types_roundtrip() {
    let r = VisionRequest {
        image_blob: "blob:1".into(),
        prompt: "read the text".into(),
        model: Some("llava".into()),
    };
    let s = serde_json::to_string(&r).unwrap();
    let back: VisionRequest = serde_json::from_str(&s).unwrap();
    assert_eq!(back.image_blob, "blob:1");

    let res = VisionResult {
        text: "hello".into(),
        detections: vec![Detection {
            label: "face".into(),
            confidence: 0.9,
            bbox: Some([0.1, 0.2, 0.3, 0.4]),
        }],
    };
    let back: VisionResult = serde_json::from_str(&serde_json::to_string(&res).unwrap()).unwrap();
    assert_eq!(back.detections[0].label, "face");
    assert_eq!(back.detections[0].bbox.unwrap()[2], 0.3);
}

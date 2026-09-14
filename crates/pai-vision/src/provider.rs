//! llama.cpp multimodal adapter — `LlamaVisionProvider`.
//!
//! Newer `llama-server` builds speak the OpenAI chat-completions protocol
//! with `image_url` content parts when the model has a multimodal
//! projector (`--mmproj`). Same localhost pattern as text inference: the
//! user serves a VLM, we POST a data-URI image + prompt.

use async_trait::async_trait;
use base64::Engine;
use pai_core::*;
use pai_inference::ImageUnderstandingProvider;
use std::time::Duration;

pub struct LlamaVisionProvider {
    base_url: String,
    model: String,
    client: reqwest::Client,
    timeout: Duration,
}

impl LlamaVisionProvider {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(180),
        }
    }

    /// Probe `/v1/models` — reachable means usable; whether the served
    /// model is actually multimodal is the server's config concern (the
    /// describe call will surface a clear error if not).
    pub async fn detect(base_url: &str, model: String, timeout: Duration) -> Option<Self> {
        let p = Self::new(base_url, model);
        let resp = p
            .client
            .get(format!("{}/v1/models", p.base_url))
            .timeout(timeout)
            .send()
            .await
            .ok()?;
        resp.status().is_success().then_some(p)
    }
}

/// Guess an image mime from a file extension (magic-byte sniffing is the
/// caller's problem when there's no extension).
pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

#[async_trait]
impl ImageUnderstandingProvider for LlamaVisionProvider {
    fn id(&self) -> &'static str {
        "llama-vision"
    }

    async fn describe(&self, image: &[u8], mime: &str, prompt: &str) -> Result<String> {
        let data_uri = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(image)
        );
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": data_uri}},
                    {"type": "text", "text": prompt},
                ],
            }],
            "temperature": 0.2,
            "max_tokens": 512,
            "stream": false,
        });
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("llama-vision: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!(
                "llama-vision HTTP {status}: {detail} — is the served model \
                 multimodal (started with --mmproj)?"
            )));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("llama-vision: {e}")))?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Provider("llama-vision: empty response".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_guesses() {
        assert_eq!(mime_for_ext("png"), Some("image/png"));
        assert_eq!(mime_for_ext("JPG"), Some("image/jpeg"));
        assert_eq!(mime_for_ext("webp"), Some("image/webp"));
        assert_eq!(mime_for_ext("tiff"), None);
    }
}

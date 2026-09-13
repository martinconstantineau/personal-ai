//! Hugging Face hub support — the primary free-model source.
//!
//! Any public GGUF on the hub is installable via an `hf://` reference:
//!
//! ```text
//! hf://<owner>/<repo>/<path/to/file.gguf>[@revision]
//! e.g. hf://unsloth/Qwen2.5-3B-Instruct-GGUF/qwen2.5-3b-instruct-q4_k_m.gguf
//! ```
//!
//! Downloads go through the hub's `resolve` endpoint. For LFS files the hub
//! exposes the real blob sha256 in `x-linked-etag`, which lets the install
//! path verify content the same way catalog models are verified.

use pai_core::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const API: &str = "https://huggingface.co";

/// A parsed `hf://` model reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfRef {
    /// "owner/repo"
    pub repo: String,
    /// Path of the file inside the repo, e.g. "qwen2.5-3b-instruct-q4_k_m.gguf".
    pub file: String,
    /// Git revision (branch/tag/sha). Defaults to "main".
    pub revision: String,
}

/// Parse `hf://owner/repo/file[@rev]`. Only `.gguf` files are installable —
/// the runtime backends in V1 speak llama.cpp's format.
pub fn parse_hf_ref(s: &str) -> Result<HfRef> {
    let rest = s
        .strip_prefix("hf://")
        .ok_or_else(|| Error::InvalidInput("hf ref must start with hf://".into()))?;
    let (rest, revision) = match rest.split_once('@') {
        Some((r, rev)) => (r, rev.to_string()),
        None => (rest, "main".to_string()),
    };
    // owner/repo/file... — file may itself contain slashes.
    let mut parts = rest.splitn(3, '/');
    let (owner, repo, file) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if owner.is_empty() || repo.is_empty() || file.is_empty() {
        return Err(Error::InvalidInput(format!(
            "hf ref must be hf://<owner>/<repo>/<file>, got '{s}'"
        )));
    }
    if !file.ends_with(".gguf") {
        return Err(Error::InvalidInput(format!(
            "only .gguf files are installable in V1, got '{file}'"
        )));
    }
    Ok(HfRef {
        repo: format!("{owner}/{repo}"),
        file: file.to_string(),
        revision,
    })
}

/// A GGUF file inside a hub repo, as reported by the tree API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfFile {
    pub path: String,
    pub size: Option<u64>,
}

/// A hub repo returned by search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfRepo {
    /// "owner/repo"
    pub id: String,
    pub downloads: Option<u64>,
    pub likes: Option<u64>,
    pub tags: Vec<String>,
}

/// Minimal async client over the hub's public HTTP API. No auth — public
/// repos only, which is exactly the free/open-weight surface we support.
pub struct HfClient {
    client: reqwest::Client,
}

impl Default for HfClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HfClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("personal-ai/0.1")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// The direct-download URL for a ref.
    pub fn resolve_url(r: &HfRef) -> String {
        format!("{API}/{}/resolve/{}/{}", r.repo, r.revision, r.file)
    }

    /// List `.gguf` files in a repo revision (top 1000 entries; the tree API
    /// paginates beyond that — enough for GGUF repos which ship few files).
    pub async fn list_gguf_files(&self, repo: &str, revision: &str) -> Result<Vec<HfFile>> {
        let url = format!("{API}/api/models/{repo}/tree/{revision}?recursive=true");
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("huggingface: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "huggingface tree {repo}: HTTP {}",
                resp.status()
            )));
        }
        let items: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("huggingface: {e}")))?;
        Ok(items
            .iter()
            .filter(|i| i["type"].as_str() == Some("file"))
            .filter_map(|i| {
                let path = i["path"].as_str()?.to_string();
                path.ends_with(".gguf").then(|| HfFile {
                    path,
                    size: i["size"].as_u64(),
                })
            })
            .collect())
    }

    /// Build a verifiable [`crate::ModelManifest`] for a hub file via HEAD:
    /// `x-linked-size`/`x-linked-etag` give the LFS blob's true size+sha256.
    pub async fn manifest_for(&self, r: &HfRef) -> Result<crate::ModelManifest> {
        let url = Self::resolve_url(r);
        let resp = self
            .client
            .head(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("huggingface: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "huggingface resolve {url}: HTTP {}",
                resp.status()
            )));
        }
        let h = resp.headers();
        let header = |name: &str| h.get(name).and_then(|v| v.to_str().ok());
        // LFS blobs carry the real size/hash on x-linked-*; plain git files
        // fall back to content-length/etag (etag then is not sha256).
        let size = header("x-linked-size")
            .or_else(|| header("content-length"))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let sha256 = header("x-linked-etag").map(|s| s.trim_matches('"').to_string());
        let filename = r.file.rsplit('/').next().unwrap_or(&r.file).to_string();
        let slug = format!("hf:{}:{}", r.repo, r.file);
        Ok(crate::ModelManifest {
            model: Model {
                id: ModelId::new(),
                slug: slug.clone(),
                family: r.repo.clone(),
                provider: "llama-server".into(),
                capabilities: vec![
                    ModelCapability::TextGeneration,
                    ModelCapability::ToolCalling,
                ],
                // Unknown from the hub API; the server reports the real
                // context length once the model is loaded.
                context_length: 8192,
                quantization: r
                    .file
                    .split(['-', '_', '.'])
                    .find(|p| p.to_lowercase().starts_with('q') && p.len() <= 8)
                    .map(String::from),
                size_bytes: size,
                requirements: ModelRequirements {
                    // Rule of thumb: ~1.3x file size of RAM headroom.
                    min_ram_bytes: (size > 0).then_some(size + size / 3),
                    min_vram_bytes: None,
                    accelerators: vec![Accelerator::Cpu],
                },
                local: true,
                license: None, // per-repo; surfaced by `models search` tags
            },
            url: Some(url),
            sha256,
            filename,
        })
    }

    /// Search the hub for GGUF model repos.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<HfRepo>> {
        let url = format!(
            "{API}/api/models?search={}&filter=gguf&limit={}&sort=downloads&direction=-1",
            urlencoding(query),
            limit.max(1)
        );
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("huggingface: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "huggingface search: HTTP {}",
                resp.status()
            )));
        }
        let items: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("huggingface: {e}")))?;
        Ok(items
            .iter()
            .filter_map(|i| {
                Some(HfRepo {
                    id: i["id"].as_str()?.to_string(),
                    downloads: i["downloads"].as_u64(),
                    likes: i["likes"].as_u64(),
                    tags: i["tags"]
                        .as_array()
                        .map(|t| {
                            t.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            })
            .collect())
    }
}

/// Minimal percent-encoding for a query parameter value.
fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hf_refs() {
        let r =
            parse_hf_ref("hf://unsloth/Qwen2.5-3B-Instruct-GGUF/qwen2.5-3b-instruct-q4_k_m.gguf")
                .unwrap();
        assert_eq!(r.repo, "unsloth/Qwen2.5-3B-Instruct-GGUF");
        assert_eq!(r.file, "qwen2.5-3b-instruct-q4_k_m.gguf");
        assert_eq!(r.revision, "main");

        let r = parse_hf_ref("hf://o/r/sub/dir/m.gguf@rev123").unwrap();
        assert_eq!(r.file, "sub/dir/m.gguf");
        assert_eq!(r.revision, "rev123");

        assert!(parse_hf_ref("hf://o/r/model.safetensors").is_err());
        assert!(parse_hf_ref("hf://incomplete").is_err());
        assert!(parse_hf_ref("not-a-ref").is_err());
    }

    #[test]
    fn encodes_query() {
        assert_eq!(urlencoding("qwen 2.5 +coder"), "qwen%202.5%20%2Bcoder");
    }
}

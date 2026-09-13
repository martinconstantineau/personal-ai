//! Model registry + manager.
//!
//! Models are entries in a manifest — never hard-coded families. The catalog
//! ships only free/open-weight models (Qwen, SmolLM, Gemma, Llama, Whisper,
//! Stable Diffusion...); licenses are recorded per entry. The manager
//! downloads, verifies (sha256), installs, and checks hardware fit.

use pai_core::*;
use pai_storage::{store_err, Store};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::{Path, PathBuf};

pub mod hf;

/// A downloadable model artifact in the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    pub model: Model,
    /// Download URL (e.g. Hugging Face). Optional for built-in models.
    pub url: Option<String>,
    pub sha256: Option<String>,
    pub filename: String,
}

/// The built-in catalog. Only free/open weights — paid API-only models are
/// out of scope by design (they'd go behind RemoteProvider adapters anyway).
pub fn builtin_catalog() -> Vec<ModelManifest> {
    vec![
        ModelManifest {
            model: Model {
                id: ModelId::new(),
                slug: "smollm2-135m-instruct-q4".into(),
                family: "SmolLM2".into(),
                provider: "llama-server".into(),
                capabilities: vec![
                    ModelCapability::TextGeneration,
                    ModelCapability::ToolCalling,
                ],
                context_length: 8192,
                quantization: Some("q4_0".into()),
                size_bytes: 100_000_000,
                requirements: ModelRequirements {
                    min_ram_bytes: Some(512 * 1024 * 1024),
                    min_vram_bytes: None,
                    accelerators: vec![Accelerator::Cpu],
                },
                local: true,
                license: Some("Apache-2.0".into()),
            },
            url: Some(
                "https://huggingface.co/HuggingFaceTB/smollm2-135m-instruct-gguf/resolve/main/smollm2-135m-instruct-q4_0.gguf".into(),
            ),
            sha256: None, // filled by the registry on first verified download
            filename: "smollm2-135m-instruct-q4_0.gguf".into(),
        },
        ModelManifest {
            model: Model {
                id: ModelId::new(),
                slug: "smollm2-360m-instruct-q4".into(),
                family: "SmolLM2".into(),
                provider: "llama-server".into(),
                capabilities: vec![
                    ModelCapability::TextGeneration,
                    ModelCapability::ToolCalling,
                ],
                context_length: 8192,
                quantization: Some("q4_0".into()),
                size_bytes: 220_000_000,
                requirements: ModelRequirements {
                    min_ram_bytes: Some(1024 * 1024 * 1024),
                    min_vram_bytes: None,
                    accelerators: vec![Accelerator::Cpu],
                },
                local: true,
                license: Some("Apache-2.0".into()),
            },
            url: Some(
                "https://huggingface.co/HuggingFaceTB/smollm2-360m-instruct-gguf/resolve/main/smollm2-360m-instruct-q4_0.gguf".into(),
            ),
            sha256: None,
            filename: "smollm2-360m-instruct-q4_0.gguf".into(),
        },
        ModelManifest {
            model: Model {
                id: ModelId::new(),
                slug: "qwen2.5-0.5b-instruct-q4_k_m".into(),
                family: "Qwen2.5".into(),
                provider: "llama-server".into(),
                capabilities: vec![
                    ModelCapability::TextGeneration,
                    ModelCapability::ToolCalling,
                ],
                context_length: 32768,
                quantization: Some("q4_k_m".into()),
                size_bytes: 397_000_000,
                requirements: ModelRequirements {
                    min_ram_bytes: Some(1024 * 1024 * 1024),
                    min_vram_bytes: None,
                    accelerators: vec![Accelerator::Cpu],
                },
                local: true,
                license: Some("Apache-2.0".into()),
            },
            url: Some(
                "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf".into(),
            ),
            sha256: None,
            filename: "qwen2.5-0.5b-instruct-q4_k_m.gguf".into(),
        },
        ModelManifest {
            model: Model {
                id: ModelId::new(),
                slug: "qwen2.5-1.5b-instruct-q4_k_m".into(),
                family: "Qwen2.5".into(),
                provider: "llama-server".into(),
                capabilities: vec![
                    ModelCapability::TextGeneration,
                    ModelCapability::ToolCalling,
                ],
                context_length: 32768,
                quantization: Some("q4_k_m".into()),
                size_bytes: 986_000_000,
                requirements: ModelRequirements {
                    min_ram_bytes: Some(2 * 1024 * 1024 * 1024),
                    min_vram_bytes: None,
                    accelerators: vec![Accelerator::Cpu],
                },
                local: true,
                license: Some("Apache-2.0".into()),
            },
            url: Some(
                "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf".into(),
            ),
            sha256: None,
            filename: "qwen2.5-1.5b-instruct-q4_k_m.gguf".into(),
        },
    ]
}

/// Resolve a user-supplied model argument to a manifest:
/// - `hf://owner/repo/file.gguf[@rev]` → resolved against the hub (HEAD
///   request captures size + LFS sha256)
/// - otherwise treated as a catalog slug.
pub async fn resolve_model_arg(arg: &str) -> Result<ModelManifest> {
    if arg.starts_with("hf://") {
        let r = hf::parse_hf_ref(arg)?;
        return hf::HfClient::new().manifest_for(&r).await;
    }
    builtin_catalog()
        .into_iter()
        .find(|m| m.model.slug == arg)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "model '{arg}' (try `pai models list`, `pai models search`, or an hf:// ref)"
            ))
        })
}

/// Can `caps` run `model`? Deterministic, hardware-based check.
pub fn fits(model: &Model, caps: &DeviceCapabilities) -> Result<()> {
    if let Some(min) = model.requirements.min_ram_bytes {
        if caps.ram_bytes < min {
            return Err(Error::InvalidInput(format!(
                "model {} needs {}B RAM, device has {}B",
                model.slug, min, caps.ram_bytes
            )));
        }
    }
    if let Some(min) = model.requirements.min_vram_bytes {
        let have = caps.gpu_vram_bytes.unwrap_or(0);
        if have < min {
            return Err(Error::InvalidInput(format!(
                "model {} needs {}B VRAM, device has {}B",
                model.slug, min, have
            )));
        }
    }
    Ok(())
}

/// Install/uninstall/verify lifecycle for local model files.
pub struct ModelManager {
    store: std::sync::Arc<Store>,
    model_dir: PathBuf,
}

impl ModelManager {
    pub fn new(store: std::sync::Arc<Store>, data_dir: &Path) -> Self {
        Self {
            store,
            model_dir: data_dir.join("models"),
        }
    }

    pub fn register(&self, m: &ModelManifest) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO models(id, slug, family, provider, capabilities_json,
                    context_length, quantization, size_bytes, requirements_json,
                    local, license, sha256)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                 ON CONFLICT(slug) DO NOTHING",
                rusqlite::params![
                    m.model.id.to_string(),
                    m.model.slug,
                    m.model.family,
                    m.model.provider,
                    serde_json::to_string(&m.model.capabilities).unwrap(),
                    m.model.context_length,
                    m.model.quantization,
                    m.model.size_bytes as i64,
                    serde_json::to_string(&m.model.requirements).unwrap(),
                    m.model.local as i64,
                    m.model.license,
                    m.sha256,
                ],
            )
        })?;
        Ok(())
    }

    /// Download + verify + mark installed. Skips if already installed.
    pub async fn install(&self, slug: &str, manifest: &ModelManifest) -> Result<PathBuf> {
        if let Some(path) = self.installed_path(slug)? {
            return Ok(path);
        }
        let url = manifest
            .url
            .clone()
            .ok_or_else(|| Error::InvalidInput(format!("{slug} has no download URL")))?;
        std::fs::create_dir_all(&self.model_dir).map_err(store_err)?;
        let dest = self.model_dir.join(&manifest.filename);
        let tmp = dest.with_extension("part");

        tracing::info!(%slug, %url, "downloading model");
        let bytes = reqwest::get(&url)
            .await
            .map_err(|e| Error::Provider(e.to_string()))?
            .bytes()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;
        std::fs::write(&tmp, &bytes).map_err(store_err)?;

        let digest = hex::encode(sha2::Sha256::digest(&bytes));
        if let Some(expected) = &manifest.sha256 {
            if &digest != expected {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Provider(format!(
                    "sha256 mismatch for {slug}: got {digest}"
                )));
            }
        }
        std::fs::rename(&tmp, &dest).map_err(store_err)?;

        self.store.with_conn(|c| {
            c.execute(
                "UPDATE models SET installed=1, path=?2, sha256=?3 WHERE slug=?1",
                rusqlite::params![slug, dest.to_string_lossy().to_string(), digest],
            )
        })?;
        Ok(dest)
    }

    pub fn uninstall(&self, slug: &str) -> Result<()> {
        if let Some(path) = self.installed_path(slug)? {
            let _ = std::fs::remove_file(&path);
        }
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE models SET installed=0, path=NULL WHERE slug=?1",
                rusqlite::params![slug],
            )
        })?;
        Ok(())
    }

    pub fn installed_path(&self, slug: &str) -> Result<Option<PathBuf>> {
        self.store
            .with_conn(|c| {
                c.query_row(
                    "SELECT path FROM models WHERE slug=?1 AND installed=1",
                    rusqlite::params![slug],
                    |r| r.get::<_, Option<String>>(0),
                )
            })
            .map(|p| p.map(PathBuf::from))
    }

    /// Every registered model (catalog entries + hf:// installs) with its
    /// install state and on-disk path when present.
    pub fn list(&self) -> Result<Vec<(Model, bool, Option<PathBuf>)>> {
        self.store
            .with_conn(|c| {
                let mut stmt = c.prepare(
                    "SELECT slug, family, provider, capabilities_json,
                            context_length, quantization, size_bytes,
                            requirements_json, local, license,
                            installed, path
                     FROM models ORDER BY slug",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        Model {
                            id: ModelId::new(),
                            slug: r.get(0)?,
                            family: r.get(1)?,
                            provider: r.get(2)?,
                            capabilities: serde_json::from_str(&r.get::<_, String>(3)?)
                                .unwrap_or_default(),
                            context_length: r.get(4)?,
                            quantization: r.get(5)?,
                            size_bytes: r.get::<_, i64>(6)? as u64,
                            requirements: serde_json::from_str(&r.get::<_, String>(7)?)
                                .unwrap_or_default(),
                            local: r.get::<_, i64>(8)? != 0,
                            license: r.get(9)?,
                        },
                        r.get::<_, i64>(10)? != 0,
                        r.get::<_, Option<String>>(11)?.map(PathBuf::from),
                    ))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(store_err)
    }

    /// All models whose declared requirements fit this device.
    pub fn runnable(&self, caps: &DeviceCapabilities) -> Result<Vec<String>> {
        self.store
            .with_conn(|c| {
                let mut stmt =
                    c.prepare("SELECT slug, requirements_json FROM models WHERE installed=1")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                Ok(rows
                    .filter_map(|r| r.ok())
                    .filter(|(_, req)| {
                        serde_json::from_str::<ModelRequirements>(req)
                            .map(|r| {
                                r.min_ram_bytes.map(|m| caps.ram_bytes >= m).unwrap_or(true)
                                    && r.min_vram_bytes
                                        .map(|m| caps.gpu_vram_bytes.unwrap_or(0) >= m)
                                        .unwrap_or(true)
                            })
                            .unwrap_or(false)
                    })
                    .map(|(slug, _)| slug)
                    .collect::<Vec<_>>())
            })
            .map_err(store_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_fit() {
        let m = &builtin_catalog()[1].model;
        let mut caps = DeviceCapabilities {
            ram_bytes: 8 * 1024 * 1024 * 1024,
            ..Default::default()
        };
        assert!(fits(m, &caps).is_ok());
        caps.ram_bytes = 256 * 1024 * 1024;
        assert!(fits(m, &caps).is_err());
    }
}

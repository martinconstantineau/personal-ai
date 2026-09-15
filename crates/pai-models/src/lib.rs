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

/// Index file written into every model directory (`index.json`). Makes
/// a directory self-describing: any pai device that finds it can adopt
/// the pack's models without re-downloading — this is what makes a
/// flash drive plug-and-play.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPackIndex {
    pub format: u32,
    #[serde(default)]
    pub models: Vec<ModelManifest>,
}

const PACK_INDEX_FILE: &str = "index.json";

fn read_pack_index(dir: &Path) -> Option<ModelPackIndex> {
    let bytes = std::fs::read(dir.join(PACK_INDEX_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_pack_index(dir: &Path, index: &ModelPackIndex) -> Result<()> {
    let tmp = dir.join(PACK_INDEX_FILE).with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(index).unwrap()).map_err(store_err)?;
    std::fs::rename(&tmp, dir.join(PACK_INDEX_FILE)).map_err(store_err)
}

fn upsert_pack_index(dir: &Path, m: &ModelManifest) -> Result<()> {
    let mut idx = read_pack_index(dir).unwrap_or(ModelPackIndex {
        format: 1,
        models: vec![],
    });
    idx.models.retain(|e| e.model.slug != m.model.slug);
    idx.models.push(m.clone());
    write_pack_index(dir, &idx)
}

fn remove_from_pack_index(dir: &Path, slug: &str) {
    if let Some(mut idx) = read_pack_index(dir) {
        let n = idx.models.len();
        idx.models.retain(|e| e.model.slug != slug);
        if idx.models.len() != n {
            let _ = write_pack_index(dir, &idx);
        }
    }
}

fn file_sha256(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(store_err)?;
    let mut h = sha2::Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(store_err)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Normalize a `--to` argument: filesystem roots (`E:\`, `/`) become
/// `<root>/pai-models` so the drive stays a clean single-folder pack.
pub fn pack_dir_for(dest: &Path) -> PathBuf {
    if dest.parent().is_none() {
        dest.join("pai-models")
    } else {
        dest.to_path_buf()
    }
}

/// Directories that may hold a model pack: `pai-models/` under every
/// mounted volume. Windows probes drive letters (cheap `is_dir` calls —
/// absent letters just miss); unix probes the usual mount points one
/// and two levels deep (`/media/<user>/<drive>`).
pub fn pack_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(windows)]
    for letter in b'A'..=b'Z' {
        roots.push(PathBuf::from(format!("{}:\\pai-models", letter as char)));
    }
    #[cfg(not(windows))]
    for base in ["/Volumes", "/media", "/mnt", "/run/media"] {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            roots.push(p.join("pai-models"));
            if let Ok(sub) = std::fs::read_dir(&p) {
                for s in sub.flatten() {
                    if s.path().is_dir() {
                        roots.push(s.path().join("pai-models"));
                    }
                }
            }
        }
    }
    roots.into_iter().filter(|r| r.is_dir()).collect()
}

/// A model adopted during a pack scan.
#[derive(Debug)]
pub struct ScannedModel {
    pub slug: String,
    pub path: PathBuf,
    pub pack_root: PathBuf,
}

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

    /// Download + verify + mark installed in the local model dir.
    /// Skips if the recorded file still exists.
    pub async fn install(&self, slug: &str, manifest: &ModelManifest) -> Result<PathBuf> {
        let dir = self.model_dir.clone();
        self.install_to(slug, manifest, &dir).await
    }

    /// Install into `dest_dir` — e.g. a flash drive's `pai-models/`
    /// folder. If the model is already on disk somewhere reachable, the
    /// file is *copied* (no second download). The directory's
    /// `index.json` is upserted either way so the pack stays
    /// self-describing.
    pub async fn install_to(
        &self,
        slug: &str,
        manifest: &ModelManifest,
        dest_dir: &Path,
    ) -> Result<PathBuf> {
        let dest = dest_dir.join(&manifest.filename);
        if let Some(cur) = self.installed_path(slug)? {
            if cur.exists() {
                if cur == dest {
                    return Ok(cur);
                }
                return self.copy_to(slug, manifest, &cur, dest_dir);
            }
        }
        let url = manifest
            .url
            .clone()
            .ok_or_else(|| Error::InvalidInput(format!("{slug} has no download URL")))?;
        std::fs::create_dir_all(dest_dir).map_err(store_err)?;
        tracing::info!(%slug, %url, "downloading model");
        let bytes = reqwest::get(&url)
            .await
            .map_err(|e| Error::Provider(e.to_string()))?
            .bytes()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;
        self.place(slug, manifest, &bytes, dest_dir)
    }

    /// Write `bytes` to `dest_dir`, verify sha256, mark installed, and
    /// upsert the pack index.
    fn place(
        &self,
        slug: &str,
        manifest: &ModelManifest,
        bytes: &[u8],
        dest_dir: &Path,
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(dest_dir).map_err(store_err)?;
        let dest = dest_dir.join(&manifest.filename);
        let tmp = dest.with_extension("part");
        let digest = hex::encode(sha2::Sha256::digest(bytes));
        if let Some(expected) = &manifest.sha256 {
            if &digest != expected {
                return Err(Error::Provider(format!(
                    "sha256 mismatch for {slug}: got {digest}"
                )));
            }
        }
        std::fs::write(&tmp, bytes).map_err(store_err)?;
        std::fs::rename(&tmp, &dest).map_err(store_err)?;
        self.mark_installed(slug, &dest, &digest)?;
        upsert_pack_index(dest_dir, manifest)?;
        Ok(dest)
    }

    /// Copy an existing local file into `dest_dir` (avoids a second
    /// download when moving a model to external storage).
    fn copy_to(
        &self,
        slug: &str,
        manifest: &ModelManifest,
        src: &Path,
        dest_dir: &Path,
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(dest_dir).map_err(store_err)?;
        let dest = dest_dir.join(&manifest.filename);
        let tmp = dest.with_extension("part");
        std::fs::copy(src, &tmp).map_err(store_err)?;
        let digest = file_sha256(&tmp)?;
        if let Some(expected) = &manifest.sha256 {
            if &digest != expected {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Provider(format!(
                    "sha256 mismatch copying {slug}: got {digest}"
                )));
            }
        }
        std::fs::rename(&tmp, &dest).map_err(store_err)?;
        self.mark_installed(slug, &dest, &digest)?;
        upsert_pack_index(dest_dir, manifest)?;
        Ok(dest)
    }

    fn mark_installed(&self, slug: &str, path: &Path, digest: &str) -> Result<()> {
        self.store.with_conn(|c| {
            c.execute(
                "UPDATE models SET installed=1, path=?2, sha256=?3 WHERE slug=?1",
                rusqlite::params![slug, path.to_string_lossy().to_string(), digest],
            )
        })?;
        Ok(())
    }

    /// Adopt an on-disk file found by a scan: verify the pinned sha256
    /// (when present), register the manifest, mark installed. Files
    /// that fail verification are skipped, never adopted.
    fn adopt(
        &self,
        manifest: &ModelManifest,
        path: &Path,
        pack_root: &Path,
    ) -> Result<Option<ScannedModel>> {
        if !path.is_file() {
            return Ok(None);
        }
        let digest = file_sha256(path)?;
        if let Some(expected) = &manifest.sha256 {
            if &digest != expected {
                tracing::warn!(slug = %manifest.model.slug, "pack file sha256 mismatch — skipped");
                return Ok(None);
            }
        }
        self.register(manifest)?;
        self.mark_installed(&manifest.model.slug, path, &digest)?;
        Ok(Some(ScannedModel {
            slug: manifest.model.slug.clone(),
            path: path.to_path_buf(),
            pack_root: pack_root.to_path_buf(),
        }))
    }

    /// Scan `roots` (pack dirs) and adopt every verifiable model file.
    /// With an `index.json`, each listed manifest is matched by
    /// filename; without one, bare files are matched against the
    /// built-in catalog.
    pub fn scan_roots(&self, roots: &[PathBuf]) -> Result<Vec<ScannedModel>> {
        let mut found = Vec::new();
        for root in roots {
            if !root.is_dir() {
                continue;
            }
            if let Some(idx) = read_pack_index(root) {
                for m in &idx.models {
                    if let Some(s) = self.adopt(m, &root.join(&m.filename), root)? {
                        found.push(s);
                    }
                }
            } else {
                let Ok(entries) = std::fs::read_dir(root) else {
                    continue;
                };
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if let Some(m) = builtin_catalog().into_iter().find(|m| m.filename == name) {
                        if let Some(s) = self.adopt(&m, &e.path(), root)? {
                            found.push(s);
                        }
                    }
                }
            }
        }
        Ok(found)
    }

    /// Probe mounted volumes for `pai-models/` packs and adopt
    /// everything verifiable. Run after plugging in a model drive —
    /// `locate` also calls this lazily when a recorded path went missing.
    pub fn scan(&self) -> Result<Vec<ScannedModel>> {
        self.scan_roots(&pack_roots())
    }

    /// Resolve `slug` to a usable on-disk path: the recorded location
    /// if the file is still there, else a pack-scan fallback — handles
    /// drives that arrive under different letters/mount points and
    /// packs this device has never seen.
    pub fn locate(&self, slug: &str) -> Result<Option<PathBuf>> {
        self.locate_roots(slug, &pack_roots())
    }

    fn locate_roots(&self, slug: &str, roots: &[PathBuf]) -> Result<Option<PathBuf>> {
        if let Some(p) = self.installed_path(slug)? {
            if p.exists() {
                return Ok(Some(p));
            }
        }
        if self.scan_roots(roots)?.iter().any(|s| s.slug == slug) {
            return self.installed_path(slug);
        }
        Ok(None)
    }

    pub fn uninstall(&self, slug: &str) -> Result<()> {
        if let Some(path) = self.installed_path(slug)? {
            let _ = std::fs::remove_file(&path);
            if let Some(dir) = path.parent() {
                remove_from_pack_index(dir, slug);
            }
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
        use rusqlite::OptionalExtension;
        self.store
            .with_conn(|c| {
                c.query_row(
                    "SELECT path FROM models WHERE slug=?1 AND installed=1",
                    rusqlite::params![slug],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
            })
            .map(|p| p.flatten().map(PathBuf::from))
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

    /// Models that fit this device *and* are actually on disk right
    /// now — a model on an unplugged drive is not runnable. Missing
    /// paths trigger one pack scan (a newly-inserted drive counts).
    pub fn runnable(&self, caps: &DeviceCapabilities) -> Result<Vec<String>> {
        let mut models = self.list()?;
        let missing = models
            .iter()
            .any(|(_, i, p)| *i && !p.as_ref().is_some_and(|q| q.exists()));
        if missing {
            let _ = self.scan()?;
            models = self.list()?;
        }
        Ok(models
            .into_iter()
            .filter(|(_, i, p)| *i && p.as_ref().is_some_and(|q| q.exists()))
            .filter(|(m, _, _)| fits(m, caps).is_ok())
            .map(|(m, _, _)| m.slug)
            .collect())
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

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pai-models-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mgr(dir: &Path) -> ModelManager {
        let store = std::sync::Arc::new(Store::open(dir, None).unwrap());
        ModelManager::new(store, dir)
    }

    fn manifest(slug: &str, filename: &str, sha256: Option<String>) -> ModelManifest {
        ModelManifest {
            model: Model {
                slug: slug.into(),
                ..builtin_catalog()[0].model.clone()
            },
            url: None,
            sha256,
            filename: filename.into(),
        }
    }

    #[test]
    fn place_writes_pack_index() {
        let dir = tempdir("place");
        let m = mgr(&dir);
        let bytes = b"fake weights";
        let digest = hex::encode(sha2::Sha256::digest(bytes));
        let mf = manifest("test-model", "test.gguf", Some(digest.clone()));
        m.register(&mf).unwrap();

        let pack = tempdir("pack");
        let dest = m.place("test-model", &mf, bytes, &pack).unwrap();
        assert!(dest.exists());
        assert_eq!(m.installed_path("test-model").unwrap().unwrap(), dest);

        let idx = read_pack_index(&pack).unwrap();
        assert_eq!(idx.models.len(), 1);
        assert_eq!(idx.models[0].model.slug, "test-model");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn scan_adopts_indexed_pack() {
        let dir = tempdir("scan");
        let m = mgr(&dir);
        let bytes = b"weights-on-a-drive";
        let digest = hex::encode(sha2::Sha256::digest(bytes));
        let mf = manifest("drive-model", "drive.gguf", Some(digest));

        // Build a pack on a "removable" dir as another device would.
        let pack = tempdir("pack");
        std::fs::write(pack.join("drive.gguf"), bytes).unwrap();
        upsert_pack_index(&pack, &mf).unwrap();

        // This device has never seen the model — scan adopts it.
        let found = m.scan_roots(std::slice::from_ref(&pack)).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slug, "drive-model");
        assert_eq!(
            m.installed_path("drive-model").unwrap().unwrap(),
            pack.join("drive.gguf")
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn scan_rejects_sha_mismatch() {
        let dir = tempdir("bad");
        let m = mgr(&dir);
        let mf = manifest("corrupt", "corrupt.gguf", Some("0".repeat(64)));

        let pack = tempdir("pack");
        std::fs::write(pack.join("corrupt.gguf"), b"tampered").unwrap();
        upsert_pack_index(&pack, &mf).unwrap();

        assert!(m
            .scan_roots(std::slice::from_ref(&pack))
            .unwrap()
            .is_empty());
        assert!(m.installed_path("corrupt").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn scan_matches_catalog_filename_without_index() {
        let dir = tempdir("bare");
        let m = mgr(&dir);
        let cat = builtin_catalog()[0].clone();

        let pack = tempdir("pack");
        std::fs::write(pack.join(&cat.filename), b"weights").unwrap();

        let found = m.scan_roots(std::slice::from_ref(&pack)).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slug, cat.model.slug);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn locate_rescans_when_recorded_path_missing() {
        let dir = tempdir("locate");
        let m = mgr(&dir);
        let bytes = b"roaming weights";
        let digest = hex::encode(sha2::Sha256::digest(bytes));
        let mf = manifest("roamer", "roamer.gguf", Some(digest));
        m.register(&mf).unwrap();

        // Installed on pack A (e.g. a drive that left as D:).
        let pack_a = tempdir("packa");
        m.place("roamer", &mf, bytes, &pack_a).unwrap();

        // The same content arrives on pack B (drive now at E:).
        let pack_b = tempdir("packb");
        std::fs::write(pack_b.join("roamer.gguf"), bytes).unwrap();
        upsert_pack_index(&pack_b, &mf).unwrap();

        // A's file is gone — locate should rescan and repoint to B.
        std::fs::remove_file(pack_a.join("roamer.gguf")).unwrap();
        let found = m
            .locate_roots("roamer", std::slice::from_ref(&pack_b))
            .unwrap()
            .unwrap();
        assert_eq!(found, pack_b.join("roamer.gguf"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&pack_a);
        let _ = std::fs::remove_dir_all(&pack_b);
    }

    #[test]
    fn locate_reports_missing_when_unplugged() {
        let dir = tempdir("gone");
        let m = mgr(&dir);
        let mf = manifest("away", "away.gguf", None);
        m.register(&mf).unwrap();
        let pack = tempdir("pack");
        m.place("away", &mf, b"w", &pack).unwrap();
        std::fs::remove_file(pack.join("away.gguf")).unwrap();

        assert!(m.locate_roots("away", &[]).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn copy_to_moves_without_download() {
        let dir = tempdir("copy");
        let m = mgr(&dir);
        let bytes = b"already local";
        let digest = hex::encode(sha2::Sha256::digest(bytes));
        let mf = manifest("mover", "mover.gguf", Some(digest));
        m.register(&mf).unwrap();
        let src = m.place("mover", &mf, bytes, &dir.join("models")).unwrap();

        let drive = tempdir("drive");
        let dest = m.copy_to("mover", &mf, &src, &drive).unwrap();
        assert_eq!(dest, drive.join("mover.gguf"));
        assert_eq!(m.installed_path("mover").unwrap().unwrap(), dest);
        assert_eq!(read_pack_index(&drive).unwrap().models.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&drive);
    }

    #[test]
    fn uninstall_drops_pack_index_entry() {
        let dir = tempdir("un");
        let m = mgr(&dir);
        let mf = manifest("bye", "bye.gguf", None);
        m.register(&mf).unwrap();
        let pack = tempdir("pack");
        m.place("bye", &mf, b"w", &pack).unwrap();
        assert_eq!(read_pack_index(&pack).unwrap().models.len(), 1);

        m.uninstall("bye").unwrap();
        assert!(read_pack_index(&pack).unwrap().models.is_empty());
        assert!(!pack.join("bye.gguf").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn pack_dir_for_appends_under_root() {
        assert_eq!(pack_dir_for(Path::new("/some/dir")), Path::new("/some/dir"));
        #[cfg(windows)]
        assert_eq!(
            pack_dir_for(Path::new("E:\\")),
            Path::new("E:\\").join("pai-models")
        );
        #[cfg(not(windows))]
        assert_eq!(pack_dir_for(Path::new("/")), Path::new("/pai-models"));
    }
}

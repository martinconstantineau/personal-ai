//! Signed, self-contained app packages for the Personal App Cloud.
//!
//! A package is a directory (see docs/adr/0004-app-package-format.md):
//!
//! ```text
//! MyGarageApp/
//!   manifest.toml      # [app] [storage] [permissions] [sharing] [migration]
//!   app.wasm           # required for runtime = "wasm"
//!   schema.sql         # optional — SQLite schema + migrations
//!   files/             # optional — bundled user files
//!   versions/          # optional — immutable version metadata
//!   signature.bin      # Ed25519 signature over manifest + content digest
//! ```
//!
//! The signer is a registered device key from `pai-identity`; verification
//! checks `signature.bin` against the signing device's public key.
//! Unverified packages are rejected.

mod run;

pub use run::{installed_dir, RunLimits, RunOutput};

use pai_core::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// Errors returned by package loading / verification / install.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("manifest.toml not found in {0}")]
    NoManifest(PathBuf),
    #[error("manifest.toml: {0}")]
    Manifest(String),
    #[error("package layout: {0}")]
    Layout(String),
    #[error("signature.bin missing — package is not signed")]
    NotSigned,
    #[error("signature verification failed: {0}")]
    BadSignature(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("core: {0}")]
    Core(#[from] Error),
}

pub type AppResult<T> = std::result::Result<T, AppError>;

/// `manifest.toml` schema — mirrors docs/adr/0004-app-package-format.md.
///
/// ```toml
/// [app]
/// name = "My Garage App"
/// version = "1.0.0"
/// entrypoint = "app.wasm"
/// runtime = "wasm"
///
/// [storage]
/// type = "sqlite"
/// path = "schema.sql"
///
/// [permissions]
/// files = ["files/"]
/// network = "none"
/// devices = []
///
/// [sharing]
/// default = "private"
///
/// [migration]
/// auto_migrate = true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManifest {
    pub app: AppSection,
    #[serde(default)]
    pub storage: StorageSpec,
    #[serde(default)]
    pub permissions: PermissionsSpec,
    #[serde(default)]
    pub sharing: SharingSpec,
    #[serde(default)]
    pub migration: MigrationSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSection {
    /// Human-readable name.
    pub name: String,
    /// Semver-ish version string.
    pub version: String,
    /// Entrypoint inside the package (`app.wasm` for wasm runtime).
    #[serde(default = "default_entrypoint")]
    pub entrypoint: String,
    /// Runtime the package targets.
    #[serde(default)]
    pub runtime: AppRuntime,
    /// Optional stable id, e.g. `com.martin.garage`. When absent the id is
    /// derived from `name` as a slug (`my-garage-app`).
    #[serde(default)]
    pub id: Option<String>,
}

fn default_entrypoint() -> String {
    "app.wasm".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AppRuntime {
    /// Portable sandboxed WebAssembly — the default and only runtime
    /// accepted for cross-device installs.
    #[default]
    Wasm,
    /// Native desktop binary. Only installable on the platform it was
    /// built for; carries a stronger trust requirement.
    Native,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSpec {
    /// Storage engine the app wants.
    #[serde(default)]
    pub r#type: StorageKind,
    /// Schema/data file inside the package (e.g. `schema.sql`).
    #[serde(default)]
    pub path: Option<String>,
    /// Blob quota in bytes; `0` = unlimited.
    #[serde(default)]
    pub quota_bytes: u64,
}

impl Default for StorageSpec {
    fn default() -> Self {
        Self {
            r#type: StorageKind::Sqlite,
            path: None,
            quota_bytes: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    #[default]
    Sqlite,
    /// Key-value store only.
    Kv,
    /// Content-addressed blobs only — no database.
    Files,
    /// Stateless app — no storage.
    None,
}

/// Capabilities the app requests — each must be granted explicitly by the
/// operator or the App Operator before the sandbox provides it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionsSpec {
    /// Package-relative directories the app may read/write.
    #[serde(default)]
    pub files: Vec<String>,
    /// Network access level.
    #[serde(default)]
    pub network: NetworkSpec,
    /// Other devices (by id) the app may call via the broker.
    #[serde(default)]
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkSpec {
    /// No network access — the default.
    #[default]
    None,
    /// Outbound requests only.
    Outbound,
    /// May accept inbound connections (gets a tailnet URL).
    Inbound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharingSpec {
    /// `private` = owner only; `link` = anyone with the share token;
    /// `public` = discoverable on the tailnet.
    #[serde(default)]
    pub default: SharePolicy,
    /// Devices the app is pinned to; empty = place anywhere.
    #[serde(default)]
    pub devices: Vec<String>,
}

impl Default for SharingSpec {
    fn default() -> Self {
        Self {
            default: SharePolicy::Private,
            devices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SharePolicy {
    #[default]
    Private,
    Link,
    Public,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MigrationSpec {
    /// Apply `schema.sql` migrations automatically on install/upgrade.
    #[serde(default = "default_true")]
    pub auto_migrate: bool,
}

fn default_true() -> bool {
    true
}

impl Default for MigrationSpec {
    fn default() -> Self {
        Self { auto_migrate: true }
    }
}

impl AppManifest {
    /// Parse and validate a `manifest.toml` string.
    pub fn parse(toml_text: &str) -> AppResult<Self> {
        let m: Self =
            toml::from_str(toml_text).map_err(|e| AppError::Manifest(format!("bad toml: {e}")))?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> AppResult<()> {
        if self.app.name.trim().is_empty() {
            return Err(AppError::Manifest("app.name is required".into()));
        }
        if self.app.version.trim().is_empty() {
            return Err(AppError::Manifest("app.version is required".into()));
        }
        check_rel_path(&self.app.entrypoint)?;
        if let Some(p) = &self.storage.path {
            check_rel_path(p)?;
        }
        for f in &self.permissions.files {
            check_rel_path(f)?;
        }
        if let Some(id) = &self.app.id {
            check_app_id(id)?;
        } else {
            // The derived id must still be usable as a directory name.
            check_app_id(&self.app_id())?;
        }
        Ok(())
    }

    /// Stable app identifier: explicit `app.id`, else a slug of the name.
    pub fn app_id(&self) -> String {
        if let Some(id) = &self.app.id {
            return id.clone();
        }
        let slug: String = self
            .app
            .name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        // Collapse runs of '-' and trim.
        let mut out = String::with_capacity(slug.len());
        for c in slug.chars() {
            if c == '-' && out.ends_with('-') {
                continue;
            }
            out.push(c);
        }
        out.trim_matches('-').to_string()
    }
}

fn check_app_id(id: &str) -> AppResult<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(AppError::Manifest(format!(
            "app id {id:?} must be non-empty [a-zA-Z0-9._-]"
        )));
    }
    Ok(())
}

/// A loaded package directory: manifest plus the paths inside it.
pub struct AppPackage {
    pub dir: PathBuf,
    pub manifest: AppManifest,
    /// Raw bytes of `manifest.toml` — covered by the signature together
    /// with `content_digest`.
    pub manifest_bytes: Vec<u8>,
    /// sha256 over every file except `signature.bin`, in sorted path order.
    /// Covers `app.wasm`, `schema.sql`, `files/`, `versions/` — the whole
    /// payload the ADR says the signature must cover.
    pub content_digest: [u8; 32],
    /// Files included in the digest, relative to `dir`.
    pub files: Vec<PathBuf>,
}

impl AppPackage {
    /// Load + validate a package directory. Does NOT verify the signature.
    pub fn load(dir: &Path) -> AppResult<Self> {
        let manifest_path = dir.join("manifest.toml");
        let manifest_bytes =
            std::fs::read(&manifest_path).map_err(|_| AppError::NoManifest(dir.to_path_buf()))?;
        let manifest = AppManifest::parse(
            std::str::from_utf8(&manifest_bytes)
                .map_err(|_| AppError::Manifest("manifest.toml is not utf-8".into()))?,
        )?;

        // Entrypoint must exist for wasm packages.
        let entry = dir.join(&manifest.app.entrypoint);
        if manifest.app.runtime == AppRuntime::Wasm && !entry.is_file() {
            return Err(AppError::Layout(format!(
                "entrypoint {:?} missing for wasm runtime",
                manifest.app.entrypoint
            )));
        }

        let mut files = Vec::new();
        collect_files(dir, dir, &mut files)?;
        files.retain(|f| f != Path::new("signature.bin"));
        files.sort();
        let content_digest = digest_files(dir, &files)?;

        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
            manifest_bytes,
            content_digest,
            files,
        })
    }

    /// Bytes the signature covers: manifest bytes || content digest.
    fn signing_payload(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.manifest_bytes.len() + 32);
        v.extend_from_slice(&self.manifest_bytes);
        v.extend_from_slice(&self.content_digest);
        v
    }

    /// Sign the package with `device`'s key, writing `signature.bin`.
    pub fn sign(
        &self,
        ids: &pai_identity::IdentityStore,
        device: &Device,
        key_dir: &Path,
    ) -> AppResult<()> {
        let sig = ids.sign(device.id, key_dir, &self.signing_payload())?;
        std::fs::write(self.dir.join("signature.bin"), sig)?;
        Ok(())
    }

    /// Verify `signature.bin` against `device`'s public key.
    /// `device` must be the device that signed (its pubkey is in the
    /// `devices` table). Unsigned packages are rejected.
    pub fn verify(&self, ids: &pai_identity::IdentityStore, device: &Device) -> AppResult<()> {
        let sig_path = self.dir.join("signature.bin");
        if !sig_path.is_file() {
            return Err(AppError::NotSigned);
        }
        let sig = std::fs::read(&sig_path)?;
        match ids.verify(device, &self.signing_payload(), &sig)? {
            true => Ok(()),
            false => Err(AppError::BadSignature(format!(
                "signature.bin does not match device {}",
                device.id
            ))),
        }
    }

    /// Like `verify`, but tries each candidate device and returns the id
    /// of the one that signed. Use this when the signer isn't known —
    /// e.g. a package built on another of the user's devices.
    pub fn verify_any(
        &self,
        ids: &pai_identity::IdentityStore,
        devices: &[Device],
    ) -> AppResult<DeviceId> {
        if !self.dir.join("signature.bin").is_file() {
            return Err(AppError::NotSigned);
        }
        for d in devices {
            if self.verify(ids, d).is_ok() {
                return Ok(d.id);
            }
        }
        Err(AppError::BadSignature(format!(
            "no registered device (of {}) produced this signature",
            devices.len()
        )))
    }

    /// Copy the package into `dest` (e.g. `data_dir/apps/<app-id>`).
    /// `dest` must not exist yet.
    pub fn install_to(&self, dest: &Path) -> AppResult<()> {
        if dest.exists() {
            return Err(AppError::Layout(format!("{dest:?} already exists")));
        }
        std::fs::create_dir_all(dest)?;
        for rel in &self.files {
            // `rel` was produced by walking `dir`, but re-check anyway.
            check_rel_path(rel.to_str().unwrap_or_default())?;
            let from = self.dir.join(rel);
            let to = dest.join(rel);
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&from, &to)?;
        }
        // Preserve the signature so installed apps stay verifiable.
        let sig = self.dir.join("signature.bin");
        if sig.is_file() {
            std::fs::copy(&sig, dest.join("signature.bin"))?;
        }
        Ok(())
    }
}

/// Reject absolute paths, `..`, and Windows drive prefixes.
fn check_rel_path(p: &str) -> AppResult<()> {
    let path = Path::new(p);
    if path.is_absolute() {
        return Err(AppError::Layout(format!("absolute path {p:?}")));
    }
    for c in path.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(AppError::Layout(format!("unsafe path {p:?}")))
            }
            _ => {}
        }
    }
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> AppResult<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if p.is_dir() {
            collect_files(root, &p, out)?;
        } else if p.is_file() {
            out.push(
                p.strip_prefix(root)
                    .map_err(|_| AppError::Layout(format!("{p:?} outside package")))?
                    .to_path_buf(),
            );
        }
    }
    Ok(())
}

fn digest_files(root: &Path, files: &[PathBuf]) -> AppResult<[u8; 32]> {
    let mut h = Sha256::new();
    for rel in files {
        h.update(rel.to_string_lossy().as_bytes());
        h.update([0]);
        h.update(std::fs::read(root.join(rel))?);
        h.update([0]);
    }
    Ok(h.finalize().into())
}

/// Filesystem registry: `data_dir/apps/<app-id>/` per installed app.
pub struct AppRegistry {
    root: PathBuf,
}

impl AppRegistry {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("apps"),
        }
    }

    /// Verify + install a package; returns the installed directory.
    /// `upgrade` allows replacing an existing install of the same id.
    pub fn install(
        &self,
        pkg: &AppPackage,
        ids: &pai_identity::IdentityStore,
        device: &Device,
        upgrade: bool,
    ) -> AppResult<PathBuf> {
        pkg.verify(ids, device)?;
        let dest = self.root.join(pkg.manifest.app_id());
        if dest.exists() && upgrade {
            std::fs::remove_dir_all(&dest)?;
        }
        pkg.install_to(&dest)?;
        Ok(dest)
    }

    /// Load an installed package by app id.
    pub fn get(&self, app_id: &str) -> AppResult<Option<AppPackage>> {
        let dir = self.root.join(app_id);
        if !dir.is_dir() {
            return Ok(None);
        }
        check_app_id(app_id)?; // belt-and-braces against traversal via id
        AppPackage::load(&dir).map(Some)
    }

    /// Remove an installed app. Returns false when no such id exists.
    pub fn remove(&self, app_id: &str) -> AppResult<bool> {
        check_app_id(app_id)?;
        let dir = self.root.join(app_id);
        if !dir.is_dir() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(true)
    }

    /// List installed apps as `(app_id, manifest)`.
    pub fn list(&self) -> AppResult<Vec<(String, AppManifest)>> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        for e in std::fs::read_dir(&self.root)? {
            let dir = e?.path();
            if !dir.is_dir() {
                continue;
            }
            match AppPackage::load(&dir) {
                Ok(p) => out.push((p.manifest.app_id(), p.manifest)),
                Err(e) => tracing::warn!(?dir, "skipping bad app install: {e}"),
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pai_identity::IdentityStore;
    use pai_storage::Store;
    use std::sync::Arc;

    /// Unique throwaway dir under the system temp dir (tests clean up
    /// behind themselves, matching pai-identity's convention).
    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pai-apps-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const MANIFEST: &str = r#"
[app]
name = "My Garage App"
version = "1.0.0"
entrypoint = "app.wasm"
runtime = "wasm"

[storage]
type = "sqlite"
path = "schema.sql"

[permissions]
files = ["files/"]
network = "none"
devices = []

[sharing]
default = "private"

[migration]
auto_migrate = true
"#;

    fn pkg_dir(root: &Path) -> PathBuf {
        let d = root.join("demo");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("manifest.toml"), MANIFEST).unwrap();
        std::fs::write(d.join("app.wasm"), b"\0asm\x01\0\0\0").unwrap();
        std::fs::write(d.join("schema.sql"), b"create table t(x);").unwrap();
        d
    }

    fn identity(root: &Path) -> (IdentityStore, Device, PathBuf) {
        let store = Arc::new(Store::in_memory().unwrap());
        let ids = IdentityStore::new(store);
        let user = ids.create_user("t").unwrap();
        let key_dir = root.join("keys");
        let dev = ids
            .register_device(
                user.id,
                "laptop",
                Platform::Linux,
                DeviceCapabilities {
                    cpu_cores: 4,
                    ram_bytes: 1 << 30,
                    gpu_vram_bytes: None,
                    gpu_name: None,
                    npu_available: false,
                    on_battery: None,
                    thermal_throttled: None,
                    network: NetworkState::Unknown,
                    available_models: vec![],
                    supported_capabilities: vec![],
                },
                &key_dir,
            )
            .unwrap();
        (ids, dev, key_dir)
    }

    #[test]
    fn parses_valid_manifest() {
        let m = AppManifest::parse(MANIFEST).unwrap();
        assert_eq!(m.app.name, "My Garage App");
        assert_eq!(m.app.version, "1.0.0");
        assert_eq!(m.app.runtime, AppRuntime::Wasm);
        assert_eq!(m.app_id(), "my-garage-app");
        assert_eq!(m.permissions.files, vec!["files/"]);
        assert!(matches!(m.permissions.network, NetworkSpec::None));
        assert!(matches!(m.storage.r#type, StorageKind::Sqlite));
        assert_eq!(m.storage.path.as_deref(), Some("schema.sql"));
        assert!(matches!(m.sharing.default, SharePolicy::Private));
        assert!(m.migration.auto_migrate);
    }

    #[test]
    fn explicit_id_wins_over_slug() {
        let m = AppManifest::parse(
            "[app]\nname = \"N\"\nversion = \"1\"\nid = \"com.x.y\"\nruntime = \"wasm\"",
        )
        .unwrap();
        assert_eq!(m.app_id(), "com.x.y");
    }

    #[test]
    fn rejects_bad_toml() {
        assert!(AppManifest::parse("not [toml").is_err());
    }

    #[test]
    fn rejects_missing_fields() {
        assert!(AppManifest::parse("[app]").is_err()); // no name/version
        assert!(AppManifest::parse("[app]\nname = \"\"").is_err());
        assert!(AppManifest::parse("[app]\nname = \"n\"\nversion = \"\"").is_err());
    }

    #[test]
    fn rejects_unsupported_runtime() {
        assert!(
            AppManifest::parse("[app]\nname=\"n\"\nversion=\"1\"\nruntime=\"python\"").is_err()
        );
    }

    #[test]
    fn rejects_missing_wasm_entrypoint() {
        let t = tmp();
        let d = t.join("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("manifest.toml"), MANIFEST).unwrap();
        assert!(matches!(AppPackage::load(&d), Err(AppError::Layout(_))));
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn rejects_traversal_paths() {
        assert!(AppManifest::parse(
            "[app]\nname=\"n\"\nversion=\"1\"\nentrypoint=\"../evil.wasm\""
        )
        .is_err());
        assert!(AppManifest::parse(
            "[app]\nname=\"n\"\nversion=\"1\"\nentrypoint=\"/abs/evil.wasm\""
        )
        .is_err());
        assert!(AppManifest::parse(
            "[app]\nname=\"n\"\nversion=\"1\"\n[storage]\npath=\"../../x\""
        )
        .is_err());
        assert!(AppManifest::parse(
            "[app]\nname=\"n\"\nversion=\"1\"\n[permissions]\nfiles=[\"../outside\"]"
        )
        .is_err());
    }

    #[test]
    fn unsigned_package_rejected() {
        let t = tmp();
        let (ids, dev, _kd) = identity(&t);
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        assert!(matches!(pkg.verify(&ids, &dev), Err(AppError::NotSigned)));
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let t = tmp();
        let (ids, dev, kd) = identity(&t);
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        pkg.sign(&ids, &dev, &kd).unwrap();
        // Reload so signature.bin exists on disk for verify.
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        pkg.verify(&ids, &dev).unwrap();
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn tampered_content_fails_verification() {
        let t = tmp();
        let (ids, dev, kd) = identity(&t);
        let d = pkg_dir(&t);
        let pkg = AppPackage::load(&d).unwrap();
        pkg.sign(&ids, &dev, &kd).unwrap();
        std::fs::write(d.join("app.wasm"), b"tampered").unwrap();
        let pkg = AppPackage::load(&d).unwrap();
        assert!(matches!(
            pkg.verify(&ids, &dev),
            Err(AppError::BadSignature(_))
        ));
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn registry_install_and_list() {
        let t = tmp();
        let (ids, dev, kd) = identity(&t);
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        pkg.sign(&ids, &dev, &kd).unwrap();
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        let reg = AppRegistry::new(&t);
        let dest = reg.install(&pkg, &ids, &dev, false).unwrap();
        assert!(dest.join("app.wasm").is_file());
        let apps = reg.list().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].0, "my-garage-app");
        let _ = std::fs::remove_dir_all(&t);
    }
}

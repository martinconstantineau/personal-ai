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

pub mod logs;
mod run;

pub use run::{
    app_read_op, app_run_op, app_write_op, installed_dir, run_logged, RunLimits, RunOutput,
    APP_IO_MAX,
};

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
    #[error("storage provisioning: {0}")]
    Storage(String),
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
        slugify(&self.app.name)
    }
}

/// Slug a display name into an app id: lowercase, non-alphanumeric
/// runs collapse to single `-`, trimmed at both ends.
pub fn slugify(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut out = String::with_capacity(slug.len());
    for c in slug.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
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
    /// Directories that are runtime state, not package content — excluded
    /// from the file list and content digest so signatures stay stable
    /// across install/run cycles.
    pub const RESERVED_DIRS: &'static [&'static str] = &["data"];

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
        files.retain(|f| {
            f != Path::new("signature.bin")
                && f.components().next().is_none_or(|c| {
                    !Self::RESERVED_DIRS.contains(&c.as_os_str().to_str().unwrap_or_default())
                })
        });
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

    /// Like `verify_any`, but candidates are raw `(DeviceId, ed25519
    /// pubkey)` pairs — used by sync apply, where a package may have been
    /// signed by a *peer* device (whose key lives in `sync_peers`, not
    /// the local `devices` table).
    pub fn verify_any_key(
        &self,
        ids: &pai_identity::IdentityStore,
        candidates: &[(DeviceId, [u8; 32])],
    ) -> AppResult<DeviceId> {
        if !self.dir.join("signature.bin").is_file() {
            return Err(AppError::NotSigned);
        }
        let sig = std::fs::read(self.dir.join("signature.bin"))?;
        let payload = self.signing_payload();
        for (id, key) in candidates {
            if ids.verify_with_key(key, &payload, &sig)? {
                return Ok(*id);
            }
        }
        Err(AppError::BadSignature(format!(
            "no candidate (of {}) produced this signature",
            candidates.len()
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

    /// Scaffold a new app source project under `dir` (id slugified from
    /// `name`): a `manifest.toml` to edit, plus a minimal Rust bin
    /// (`Cargo.toml` + `src/main.rs`) that `pai apps build` compiles
    /// for `wasm32-wasip1`.
    pub fn init(dir: &Path, name: &str) -> AppResult<PathBuf> {
        let id = slugify(name);
        check_app_id(&id)?;
        let root = dir.join(&id);
        if root.exists() {
            return Err(AppError::Layout(format!("{root:?} already exists")));
        }
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(
            root.join("manifest.toml"),
            format!(
                "[app]\nid = \"{id}\"\nname = \"{name}\"\nversion = \"0.1.0\"\nruntime = \"wasm\"\nentrypoint = \"app.wasm\"\n\n[permissions]\n# files = [\"files\"]\n\n[storage]\ntype = \"kv\"\npath = \"data\"\n"
            ),
        )?;
        std::fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = \"{id}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )?;
        std::fs::write(
            root.join("src/main.rs"),
            format!("fn main() {{\n    println!(\"hello from {name}\");\n}}\n"),
        )?;
        Ok(root)
    }

    /// Build `project_dir` into a loadable package under `out_dir`.
    /// - `manifest.toml` + `Cargo.toml`: cargo build for wasm32-wasip1,
    ///   manifest + payload copied, compiled wasm written to the
    ///   manifest's entrypoint.
    /// - `manifest.toml` only: already a package — copied + validated.
    /// - `Cargo.toml` only: compiled, with a manifest synthesized from
    ///   `[package]` (entrypoint `app.wasm`).
    pub fn build(project_dir: &Path, out_dir: &Path) -> AppResult<PathBuf> {
        let manifest_src = project_dir.join("manifest.toml");
        let cargo_src = project_dir.join("Cargo.toml");
        if !manifest_src.is_file() && !cargo_src.is_file() {
            return Err(AppError::Layout(format!(
                "{project_dir:?} has no manifest.toml or Cargo.toml — `pai apps init <name>` scaffolds one"
            )));
        }

        let manifest_text = if manifest_src.is_file() {
            std::fs::read_to_string(&manifest_src)?
        } else {
            let doc: toml::Value = std::fs::read_to_string(&cargo_src)?
                .parse()
                .map_err(|e: toml::de::Error| AppError::Manifest(e.to_string()))?;
            let name = doc["package"]["name"]
                .as_str()
                .ok_or_else(|| AppError::Manifest("Cargo.toml missing package.name".into()))?;
            let version = doc["package"]["version"].as_str().unwrap_or("0.1.0");
            format!(
                "[app]\nname = \"{name}\"\nversion = \"{version}\"\nruntime = \"wasm\"\nentrypoint = \"app.wasm\"\n"
            )
        };
        let manifest = AppManifest::parse(&manifest_text)?;

        if cargo_src.is_file() {
            let status = std::process::Command::new("cargo")
                .args(["build", "--target", "wasm32-wasip1", "--release"])
                .current_dir(project_dir)
                .status()
                .map_err(|e| AppError::Layout(format!("cargo build: {e}")))?;
            if !status.success() {
                return Err(AppError::Layout(format!(
                    "cargo build --target wasm32-wasip1 failed ({status})"
                )));
            }
        }

        // Copy payload: everything except build-only files and the
        // output dir itself.
        if out_dir.exists() {
            std::fs::remove_dir_all(out_dir)?;
        }
        std::fs::create_dir_all(out_dir)?;
        const SKIP_DIRS: &[&str] = &["src", "target", ".git"];
        for e in std::fs::read_dir(project_dir)? {
            let e = e?;
            let p = e.path();
            if p == out_dir {
                continue;
            }
            let name = e.file_name();
            if p.is_dir() {
                if SKIP_DIRS.contains(&name.to_str().unwrap_or_default()) {
                    continue;
                }
                copy_merge(&p, &out_dir.join(&name))?;
            } else if name != "Cargo.toml" && name != "Cargo.lock" {
                std::fs::copy(&p, out_dir.join(&name))?;
            }
        }
        std::fs::write(out_dir.join("manifest.toml"), &manifest_text)?;

        if cargo_src.is_file() {
            // cargo output name: package.name with '-' → '_'.
            let doc: toml::Value = std::fs::read_to_string(&cargo_src)?
                .parse()
                .map_err(|e: toml::de::Error| AppError::Manifest(e.to_string()))?;
            let crate_name = doc["package"]["name"].as_str().unwrap_or_default();
            let out_dir_t = project_dir.join("target/wasm32-wasip1/release");
            // Top-level artifacts keep the package spelling; dep-level
            // artifacts mangle '-' to '_'. Accept either.
            let wasm = [crate_name.to_string(), crate_name.replace('-', "_")]
                .iter()
                .map(|n| out_dir_t.join(format!("{n}.wasm")))
                .find(|p| p.is_file())
                .unwrap_or_else(|| out_dir_t.join(format!("{crate_name}.wasm")));
            if !wasm.is_file() {
                return Err(AppError::Layout(format!(
                    "{} not produced — the crate must build a wasm32-wasip1 bin",
                    wasm.display()
                )));
            }
            let entry = out_dir.join(&manifest.app.entrypoint);
            if let Some(p) = entry.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&wasm, &entry)?;
        }

        // Validate the assembled package — catches a missing entrypoint
        // for manifest-only sources and bad payload paths either way.
        Self::load(out_dir)?;
        Ok(out_dir.to_path_buf())
    }
}

/// Reject absolute paths, `..`, and Windows drive prefixes. Public so
/// sync apply can stage package files with the same safety rule.
pub fn check_rel_path(p: &str) -> AppResult<()> {
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

/// Copy `src` into `dst` recursively; existing files in `dst` are
/// overwritten. Used to restore live `data/` over package-shipped seeds.
fn copy_merge(src: &Path, dst: &Path) -> AppResult<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_merge(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// Provision per-app storage after install. `data/` is created for every
/// non-stateless app (the sandbox preopens it); `sqlite` additionally
/// creates `data/data.db` and, when `migration.auto_migrate` is set,
/// applies `storage.path` (e.g. `schema.sql`) as a batch.
fn provision_storage(app_dir: &Path, manifest: &AppManifest) -> AppResult<()> {
    let data = app_dir.join("data");
    match manifest.storage.r#type {
        StorageKind::None => return Ok(()),
        StorageKind::Sqlite => {
            std::fs::create_dir_all(&data)?;
            let db = data.join("data.db");
            let conn = rusqlite::Connection::open(&db)
                .map_err(|e| AppError::Storage(format!("open {db:?}: {e}")))?;
            // schema.sql is re-applied on every install/upgrade — it is
            // the app's migration runner, so its DDL must be idempotent
            // (`create table if not exists …`). A preserved data.db keeps
            // its rows; new statements add what v2 needs.
            if manifest.migration.auto_migrate {
                if let Some(schema) = &manifest.storage.path {
                    let sql = std::fs::read_to_string(app_dir.join(schema))?;
                    conn.execute_batch(&sql)
                        .map_err(|e| AppError::Storage(format!("apply {schema}: {e}")))?;
                }
            }
        }
        StorageKind::Kv | StorageKind::Files => {
            std::fs::create_dir_all(&data)?;
        }
    }
    Ok(())
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
    /// `upgrade` allows replacing an existing install of the same id —
    /// the live `data/` directory (app database, user files) is preserved
    /// across the upgrade and merged over any package-shipped seeds.
    pub fn install(
        &self,
        pkg: &AppPackage,
        ids: &pai_identity::IdentityStore,
        device: &Device,
        upgrade: bool,
    ) -> AppResult<PathBuf> {
        pkg.verify(ids, device)?;
        self.place(pkg, upgrade)
    }

    /// Install a package whose signature was already verified by the
    /// caller (e.g. sync apply via `verify_any_key`). Same layout +
    /// provisioning as `install`, minus the signature check — the name
    /// is deliberate: never call this on an unverified package.
    pub fn install_trusted(&self, pkg: &AppPackage, upgrade: bool) -> AppResult<PathBuf> {
        self.place(pkg, upgrade)
    }

    /// Write the package into the registry and provision storage. On
    /// upgrade the live `data/` directory is preserved and merged back
    /// over any package-shipped seeds.
    fn place(&self, pkg: &AppPackage, upgrade: bool) -> AppResult<PathBuf> {
        let dest = self.root.join(pkg.manifest.app_id());
        if dest.exists() {
            if !upgrade {
                return Err(AppError::Layout(format!("{dest:?} already exists")));
            }
            // Preserve live app data across the upgrade.
            let live = dest.join("data");
            let backup = self
                .root
                .join(format!(".{}.data.bak", pkg.manifest.app_id()));
            if live.is_dir() {
                if backup.exists() {
                    std::fs::remove_dir_all(&backup)?;
                }
                std::fs::rename(&live, &backup)?;
            }
            std::fs::remove_dir_all(&dest)?;
            let r = pkg.install_to(&dest).and_then(|_| {
                if backup.is_dir() {
                    copy_merge(&backup, &dest.join("data"))?;
                    std::fs::remove_dir_all(&backup)?;
                }
                Ok(())
            });
            if let Err(e) = r {
                // Roll the live data back if the reinstall failed midway.
                if backup.is_dir() {
                    let _ = std::fs::create_dir_all(&dest);
                    let _ = std::fs::rename(&backup, &live);
                }
                return Err(e);
            }
        } else {
            pkg.install_to(&dest)?;
        }
        provision_storage(&dest, &pkg.manifest)?;
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

    /// Read the app's live `data/` subtree as `(relative path, bytes)`
    /// pairs — the backup snapshot. Stateless apps yield an empty vec.
    /// Paths use forward slashes so snapshots are portable.
    pub fn snapshot_data(&self, app_id: &str) -> AppResult<Vec<(String, Vec<u8>)>> {
        check_app_id(app_id)?;
        let data = self.root.join(app_id).join("data");
        let mut out = Vec::new();
        if !data.is_dir() {
            return Ok(out);
        }
        let mut rels = Vec::new();
        collect_files(&data, &data, &mut rels)?;
        for rel in rels {
            out.push((
                rel.to_string_lossy().replace('\\', "/"),
                std::fs::read(data.join(&rel))?,
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Replace the app's `data/` with a backup's files — same
    /// move-aside contract as upgrades: live state is rescued first and
    /// rolled back if the write fails midway. Call after the package
    /// itself is installed; the swap is exact (no merge) because a
    /// restore means "the state at backup time", not "union".
    pub fn restore_data(&self, app_id: &str, files: &[(String, Vec<u8>)]) -> AppResult<()> {
        check_app_id(app_id)?;
        for (rel, _) in files {
            check_rel_path(rel)?;
        }
        let dest = self.root.join(app_id);
        if !dest.is_dir() {
            return Err(AppError::Layout(format!("app {app_id} not installed")));
        }
        let live = dest.join("data");
        let rescue = self.root.join(format!(".{app_id}.data.rescue"));
        if live.is_dir() {
            if rescue.exists() {
                std::fs::remove_dir_all(&rescue)?;
            }
            std::fs::rename(&live, &rescue)?;
        }
        let r = (|| -> AppResult<()> {
            std::fs::create_dir_all(&live)?;
            for (rel, bytes) in files {
                let to = live.join(rel);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&to, bytes)?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            let _ = std::fs::remove_dir_all(&live);
            if rescue.is_dir() {
                let _ = std::fs::rename(&rescue, &live);
            }
            return Err(e);
        }
        if rescue.is_dir() {
            std::fs::remove_dir_all(&rescue)?;
        }
        Ok(())
    }

    /// Move a live `data/` aside to `apps/.<id>.data.inactive-<ts>` —
    /// the source half of a migration, or the deactivation applied
    /// when a peer claims active_device. The dir is recoverable, not
    /// deleted: merge it back manually if the migrate is cancelled.
    /// Returns the rescue path when data existed.
    pub fn deactivate_data(&self, app_id: &str) -> AppResult<Option<PathBuf>> {
        check_app_id(app_id)?;
        let live = self.root.join(app_id).join("data");
        if !live.is_dir() {
            return Ok(None);
        }
        let rescue = self.root.join(format!(
            ".{app_id}.data.inactive-{}",
            pai_core::now().format("%Y%m%d%H%M%S")
        ));
        std::fs::rename(&live, &rescue)?;
        Ok(Some(rescue))
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
            // Dot-dirs are internal state (parked `.inactive-*` data),
            // never packages.
            if dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
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

    #[test]
    fn install_provisions_sqlite_and_applies_schema() {
        let t = tmp();
        let (ids, dev, kd) = identity(&t);
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        pkg.sign(&ids, &dev, &kd).unwrap();
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        let dest = AppRegistry::new(&t)
            .install(&pkg, &ids, &dev, false)
            .unwrap();
        // schema.sql was `create table t(x);` — auto_migrate applied it.
        let conn = rusqlite::Connection::open(dest.join("data/data.db")).unwrap();
        let n: i64 = conn
            .query_row(
                "select count(*) from sqlite_master where name='t'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn upgrade_preserves_live_data() {
        let t = tmp();
        let (ids, dev, kd) = identity(&t);
        let reg = AppRegistry::new(&t);
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        pkg.sign(&ids, &dev, &kd).unwrap();
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        let dest = reg.install(&pkg, &ids, &dev, false).unwrap();
        // Simulate the app having written state.
        let conn = rusqlite::Connection::open(dest.join("data/data.db")).unwrap();
        conn.execute_batch("insert into t values (42);").unwrap();
        drop(conn); // release the file handle — Windows locks open files
        std::fs::write(dest.join("data/user.txt"), b"mine").unwrap();

        // v2 package: same id, bumped version, extended schema.
        let d = pkg_dir(&t);
        std::fs::write(
            d.join("schema.sql"),
            b"create table if not exists t(x); create table t2(y);",
        )
        .unwrap();
        let pkg = AppPackage::load(&d).unwrap();
        pkg.sign(&ids, &dev, &kd).unwrap();
        let pkg = AppPackage::load(&d).unwrap();
        let dest = reg.install(&pkg, &ids, &dev, true).unwrap();

        assert_eq!(std::fs::read(dest.join("data/user.txt")).unwrap(), b"mine");
        let conn = rusqlite::Connection::open(dest.join("data/data.db")).unwrap();
        let row: i64 = conn.query_row("select x from t", [], |r| r.get(0)).unwrap();
        assert_eq!(row, 42);
        let n: i64 = conn
            .query_row(
                "select count(*) from sqlite_master where name='t2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn reinstall_without_upgrade_refused() {
        let t = tmp();
        let (ids, dev, kd) = identity(&t);
        let reg = AppRegistry::new(&t);
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        pkg.sign(&ids, &dev, &kd).unwrap();
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        reg.install(&pkg, &ids, &dev, false).unwrap();
        let pkg = AppPackage::load(&pkg_dir(&t)).unwrap();
        assert!(reg.install(&pkg, &ids, &dev, false).is_err());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn slugify_works() {
        assert_eq!(slugify("My Garage App!"), "my-garage-app");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
        assert_eq!(slugify("already-slugged"), "already-slugged");
    }

    #[test]
    fn init_scaffolds_project() {
        let t = tmp();
        let root = AppPackage::init(&t, "My Cool App").unwrap();
        assert_eq!(root, t.join("my-cool-app"));
        for f in ["manifest.toml", "Cargo.toml", "src/main.rs"] {
            assert!(root.join(f).is_file(), "missing {f}");
        }
        let m = AppManifest::parse(&std::fs::read_to_string(root.join("manifest.toml")).unwrap())
            .unwrap();
        assert_eq!(m.app_id(), "my-cool-app");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn init_refuses_existing_dir() {
        let t = tmp();
        AppPackage::init(&t, "dup").unwrap();
        assert!(AppPackage::init(&t, "dup").is_err());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn build_copies_existing_package() {
        let t = tmp();
        let src = pkg_dir(&t);
        let out = t.join("out");
        let built = AppPackage::build(&src, &out).unwrap();
        assert_eq!(built, out);
        assert!(out.join("app.wasm").is_file());
        assert!(out.join("schema.sql").is_file());
        // Output is a valid loadable package.
        let pkg = AppPackage::load(&out).unwrap();
        assert_eq!(pkg.manifest.app_id(), "my-garage-app");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn build_rejects_dir_without_sources() {
        let t = tmp();
        assert!(AppPackage::build(&t, &t.join("out")).is_err());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn build_rejects_package_missing_entrypoint() {
        let t = tmp();
        let src = t.join("broken");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("manifest.toml"), MANIFEST).unwrap();
        assert!(AppPackage::build(&src, &t.join("out")).is_err());
        let _ = std::fs::remove_dir_all(&t);
    }
}

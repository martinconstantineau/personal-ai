# Architecture: Personal App Cloud

**Status:** Draft  
**Owner:** Marti  
**Date:** 2026-09-14  
**Related:** `ARCHITECTURE.md`, `docs/PRD-personal-app-cloud.md`, `docs/adr/0004-app-package-format.md`

---

## 1. Scope

This document describes the technical architecture for the **Personal App Cloud** product. It builds on the existing `personal-ai` Rust workspace (`pai-*`) and adds a device-mesh, app-package, and sharing layer.

The Personal App Cloud treats the user's devices as a single infrastructure. An app is a signed package that can run on any device in the mesh, sync its state, and be moved or restored without reconfiguration.

## 2. Design goals

| Goal | How it is achieved |
|------|-------------------|
| **Local-first** | Apps and data run on user devices; the internet is optional. |
| **No server admin** | Devices discover each other via `pai-mesh`; no DNS or port-forwarding. |
| **App portability** | An app is a signed package (`pai-apps`) that can be moved across devices. |
| **Least privilege** | Apps declare capabilities; `pai-permissions` enforces them in a sandbox. |
| **User ownership** | The user's Ed25519 key signs packages and capabilities; the platform never holds user data. |
| **Vibe-coding native** | `pai deploy` accepts a generated project and configures the rest automatically. |

## 3. Component diagram

```
┌─────────────────────────────────────────────────────────────┐
│ Experience Layer                                            │
│  apps/desktop (Flutter)        apps/mobile (Flutter)        │
│  apps/cli (`pai`)              apps/web (optional)          │
└───────────────────────────┬─────────────────────────────────┘
                            │ C ABI: pai_init / pai_send / pai_audit
                            │ JSON in, JSON out
┌───────────────────────────▼─────────────────────────────────┐
│ Core Layer (Rust workspace)                                 │
│                                                             │
│  pai-agent      bounded agent loop (App Operator)           │
│  pai-inference  provider trait (Ollama, OpenRouter, etc.)   │
│  pai-tools      tool registry (deploy, share, backup, open) │
│  pai-permissions policy engine (app capabilities)          │
│  pai-identity   device keys, user identity, signing         │
│  pai-sync       E2EE object sync (app state)                │
│  pai-storage    per-app SQLite + blob store                 │
│  pai-broker     workload placement                         │
│  pai-config     TOML config + ComputePolicy                 │
│                                                             │
│  **pai-apps**   app package format, manifest, sandbox      │
│  **pai-mesh**   device discovery, pairing, connections      │
│  **pai-share**  capability tokens, invites, ACLs            │
│  **pai-backup** snapshot, restore, rescue mode             │
└─────────────────────────────────────────────────────────────┘
```

## 4. New crate interfaces

### 4.1 `pai-apps`

Responsible for the app package format, manifest, and sandboxed execution.

```rust
pub struct AppPackage {
    pub manifest: AppManifest,
    pub root: PathBuf,          // temp or package root
    pub signature: Signature,   // Ed25519 over manifest + files
}

pub struct AppManifest {
    pub app: AppSection,        // name, version, entrypoint, runtime, optional id
    pub storage: StorageSpec,   // "sqlite" | "kv" | "files" | "none" + schema path
    pub permissions: PermissionsSpec, // files[], network, devices[]
    pub sharing: SharingSpec,   // "private" | "link" | "public" + pinned devices
    pub migration: MigrationSpec,     // auto_migrate
}

pub enum AppRuntime {
    Wasm { module: PathBuf },
    Native { binary: PathBuf, os: OsTarget },
}

pub struct AppSandbox {
    pub instance: wasmtime::Instance,   // or native process
    pub capabilities: CapabilitySet,
    pub store: StoreHandle,             // per-app SQLite
}

impl AppPackage {
    pub fn verify(&self, ids: &IdentityStore, device: &Device) -> Result<(), AppError>;
    pub fn verify_any(&self, ids: &IdentityStore, devices: &[Device]) -> Result<DeviceId, AppError>;
    pub fn install_to(&self, dest: &Path) -> Result<(), AppError>;
    pub fn sign(&self, ids: &IdentityStore, device: &Device, key_dir: &Path) -> Result<(), AppError>;
    // Implemented: wasmi + WASI preview1, deny-by-default sandbox,
    // fuel + memory limits. Returns captured stdout/stderr + exit code.
    pub fn run(&self, app_dir: &Path, args: &[String], limits: RunLimits) -> Result<RunOutput, AppError>;
}
```

### 4.2 `pai-mesh`

Responsible for device discovery, pairing, and connection management.

```rust
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub pubkey: PublicKey,
    pub capabilities: DeviceCaps,
    pub status: DeviceStatus,   // Online | Offline | Sleeping
}

pub struct DeviceCaps {
    pub cpu_cores: u32,
    pub ram_mb: u64,
    pub storage_free_mb: u64,
    pub gpu: Option<GpuInfo>,
    pub battery: BatteryStatus,
    pub network: NetworkKind,    // Lan | Tailscale | Wan
}

pub trait DeviceDiscovery {
    fn announce(&self) -> Result<(), MeshError>;
    fn discover(&self) -> Vec<Device>;
    fn pair(&self, code: &str) -> Result<DeviceId, MeshError>;
}

pub trait MeshTransport {
    fn send(&self, to: DeviceId, obj: &SyncObject) -> Result<(), MeshError>;
    fn recv(&self) -> Result<Option<(DeviceId, SyncObject)>, MeshError>;
}

pub struct Mesh {
    pub devices: Vec<Device>,
    pub local: Device,
    pub transport: Box<dyn MeshTransport>,
}
```

### 4.3 `pai-share`

Responsible for capability-based access control and invites.

```rust
pub enum AccessLevel {
    Read,
    Write,
    Admin,
}

pub struct Capability {
    pub app_id: AppId,
    pub action: String,          // "read", "write", "exec", "share"
    pub device: Option<DeviceId>, // None = any device in mesh
    pub expires: Option<DateTime<Utc>>,
}

pub struct Invite {
    pub token: SecretToken,
    pub app_id: AppId,
    pub level: AccessLevel,
    pub created_by: UserId,
    pub expires: DateTime<Utc>,
}

impl ShareEngine {
    pub fn grant(&self, app: &AppId, to: &PublicKey, level: AccessLevel) -> Result<Capability, ShareError>;
    pub fn revoke(&self, cap: &Capability) -> Result<(), ShareError>;
    pub fn invite(&self, app: &AppId, level: AccessLevel) -> Result<Invite, ShareError>;
    pub fn accept(&self, token: &SecretToken, device: &DeviceId) -> Result<Capability, ShareError>;
}
```

### 4.4 `pai-backup`

Responsible for snapshots, restore, and rescue mode.

```rust
pub struct Backup {
    pub app_id: AppId,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub devices: Vec<DeviceId>,
    pub data: Vec<u8>,          // encrypted blob
}

pub trait BackupStore {
    fn write(&self, backup: &Backup) -> Result<BackupId, BackupError>;
    fn list(&self, app: &AppId) -> Result<Vec<BackupId>, BackupError>;
    fn read(&self, id: &BackupId) -> Result<Backup, BackupError>;
}

pub struct RescueMode {
    pub snapshot: Backup,
    pub target: DeviceId,
}

impl RescueMode {
    pub fn restore(&self, mesh: &Mesh) -> Result<(), BackupError>;
}
```

## 5. Data flow

### 5.1 Deploy

```
User                    CLI/UI                    Core                      Target Device
  │                       │                         │                            │
  │── "deploy ./app" ──▶│                         │                            │
  │                       │── pai_deploy(path) ──▶│                            │
  │                       │                         │── verify + sign pkg      │
  │                       │                         │── pai_broker::place()    │
  │                       │                         │── pai_mesh::send(pkg) ──▶│
  │                       │                         │                            │── install
  │                       │                         │── pai_sync::watch()      │
  │                       │                         │                            │── start
  │                       │◀── JSON {app_id, url} ──│                            │
  │◀── "Done. URL: ..."──│                         │                            │
```

### 5.2 Sync

```
Device A                Device B                Relay / LAN
  │                       │                          │
  │── write object ──▶ pai_sync                    │
  │                       │── encrypt + sign ───────▶│
  │                       │                          │── forward ──▶ Device B
  │                       │                          │              │── decrypt
  │                       │                          │              │── apply
  │                       │◀── ack ─────────────────│              │
```

### 5.3 Run (request)

```
User                    Mobile UI                 Core                      App
  │                       │                         │                         │
  │── "open Budget" ──▶│                         │                         │
  │                       │── pai_open(app_id) ──▶│                         │
  │                       │                         │── find app on mesh   │
  │                       │                         │── establish channel    │
  │                       │                         │── proxy request ──────▶│
  │                       │                         │                         │── handle
  │                       │◀── response ──────────│                         │
  │◀── render ───────────│                         │                         │
```

## 6. Storage layout

Each app gets its own SQLite database under the `pai-storage` workspace:

```
~/.pai/apps/<app-id>/
├── package/           # unpacked app package
├── data.db            # app SQLite (WAL)
├── blobs/             # content-addressed files
├── logs/              # app logs
└── sync/              # outgoing/incoming SyncObjects
```

The `pai-storage` `Store` handle is opened per-app and scoped to the app's capability set.

## 7. Security model

- **Package signing:** every `AppPackage` is signed by the user's Ed25519 key (`pai-identity`). The mesh verifies the signature before install.
- **Sandboxing:** apps run in a Wasmtime instance or a restricted process. The manifest declares capabilities; `pai-permissions` enforces them at runtime.
- **Sync:** all `SyncObject`s are encrypted with XChaCha20-Poly1305 and signed; the transport never sees plaintext.
- **Sharing:** access is granted by signed `Capability` tokens; tokens can be revoked and expire.
- **Transport:** device-to-device traffic uses Noise or Tailscale; there is no public HTTP endpoint unless the user explicitly enables it.

## 8. Placement policy

`pai-broker` scores devices on a weighted basis:

| Factor | Weight | Example |
|--------|--------|---------|
| Free RAM | 25% | Prefer desktop over phone for heavy apps |
| Battery | 20% | Avoid running heavy work on a phone on battery |
| Storage | 15% | Prefer device with most free space |
| Network | 15% | Prefer device on Tailscale for remote access |
| User pin | 25% | User override "run this on desktop" |

The broker can migrate an app when a better device becomes available, but only if the app's data is fully synced and the migration is safe.

## 9. Error handling

- **Deploy fails** → the package is not installed; the user gets a clear error.
- **Sync conflict** → per-field last-write-wins; conflicts are logged to `pai-audit`.
- **Device offline** → the app is marked "waiting" and the UI shows last-known state.
- **Migration fails** → the app rolls back to the original device.
- **Permission denied** → the App Operator explains what capability is missing.

## 10. Future work

- `pai-share` invite UI (QR code, deep link)
- `pai-backup` to an external S3 bucket or external drive
- `pai-mesh` relay for non-Tailscale NAT traversal
- App templates and a user-facing "Create App" flow
- Cross-app automation with a permission model

---

*Next step: implement `pai-apps` package parsing and a minimal `pai-mesh` LAN discovery spike.*

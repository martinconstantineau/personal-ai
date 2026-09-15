# ADR 0004: App package format

**Status:** Proposed  
**Date:** 2026-09-14  
**Deciders:** Marti  
**Related:** `docs/PRD-personal-app-cloud.md`, `docs/ARCHITECTURE-personal-app-cloud.md`

---

## Context

The Personal App Cloud needs a portable, signed package that can be created by any AI coding tool and run on any of the user's devices. The package must contain:

- The application code (UI + backend)
- A database or storage schema
- User files and configuration
- Declared permissions (what the app can access)
- Version history and a signature

The format must be:

- **Cross-platform** — the same package runs on Windows, macOS, Linux, iOS, and Android
- **Sandboxable** — the platform can restrict file, network, and device access
- **Self-contained** — no external dependencies to install
- **Versioned** — every deploy is immutable and can be rolled back
- **Signed** — the user's Ed25519 key proves the package came from them

## Decision

Use a **directory-based, signed package** format with a WASM runtime and a per-app SQLite database.

### Package layout

```
MyGarageApp/
├── manifest.toml          # name, version, permissions, entrypoint, runtime
├── app.wasm               # compiled WASM module (UI + logic)
├── schema.sql             # SQLite schema + migrations
├── files/                 # bundled user files (assets, templates, etc.)
├── versions/              # immutable version metadata
│   └── v1.0/
│       ├── manifest.toml
│       └── app.wasm
└── signature.bin          # Ed25519 signature over manifest + files
```

### Manifest (`manifest.toml`)

```toml
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
```

### Runtime

- **WASM** for the application layer. The app compiles to a single `app.wasm` module that runs inside `wasmtime`.
- **SQLite** for storage. Each app gets its own database file under `~/.pai/apps/<app-id>/data.db`.
- **Files** are bundled in the package and stored content-addressed in `~/.pai/apps/<app-id>/blobs/`.

### Signing

- The package is signed by the user's Ed25519 key (`pai-identity`).
- The signature covers `manifest.toml`, `app.wasm`, `schema.sql`, and the `files/` directory.
- Unverified packages are rejected by `pai-apps::AppPackage::verify`.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
|-------------|------|------|---------|
| **Raw directory** | Simple, human-readable | Not portable, no signing | Cannot verify integrity or move safely |
| **Tar/zip** | Single file, easy to move | Not executable, needs unpacking step | Same as directory, but adds a layer |
| **OCI image (Docker)** | Standard, good tooling | Requires container runtime, heavy | Too complex for consumer devices |
| **Native executable** | Fast, no runtime | Not portable, hard to sandbox | Different binaries per OS; iOS impossible |
| **Web app (HTML/JS)** | Portable | Needs a browser, not native | Limited device access and storage |
| **WASM + SQLite** | Portable, sandboxable, single file | Requires WASM runtime | Best balance of portability and safety |

## Consequences

### Positive

- One package format works on all devices.
- WASM sandboxing gives strong isolation without Docker.
- SQLite gives a real database with zero admin.
- Signing makes the package trustworthy and portable.

### Negative

- Users (or AI tools) must compile to WASM; not all code is WASM-friendly.
- WASM + SQLite has more overhead than a native binary.
- We need a `pai-apps` crate for packaging, verification, and sandboxing.

### Mitigations

- Provide a `pai build` command that wraps `cargo build --target wasm32-wasi` (or similar).
- Support a "native" runtime for desktop-only apps when WASM is too restrictive.
- Keep the manifest simple; the App Operator can fill it in for the user.

## Validation

- [x] `pai-apps` can parse `manifest.toml` and verify `signature.bin`
- [x] `pai-apps` can unpack a package into `~/.pai/apps/<app-id>/`
- [x] `pai` CLI can `pai deploy ./MyGarageApp` and `pai apps list`
- [x] A WASM module can read/write `data.db` through the sandbox — the
  `data/` preopen covers it; install provisions `data/data.db` and applies
  `schema.sql` when `migration.auto_migrate` is set
- [x] A signed package is rejected if the signature is invalid
- [x] An app package can be moved to a second device and run without
  changes — `pai sync` ships installed apps as sealed `app/<id>` objects
  carrying the whole package + `signature.bin`; the receiver re-verifies
  the signature against its own devices and `sync_peers.ed_pubkey`
  (paired devices) before `AppRegistry::install_trusted` — unverifiable
  packages are skipped, never installed. `apps remove` ships a tombstone
  so deletion propagates. Live `data/` (runtime state) is reserved and
  never travels; the recipient provisions it fresh and upgrades preserve
  it. `schema.sql` is re-applied per provision, so its DDL must be
  idempotent (`create table if not exists …`).

---

*Implementation note: start with a single-device WASM runner; add `pai-mesh` sync and `pai-share` capabilities once the package format is proven.*
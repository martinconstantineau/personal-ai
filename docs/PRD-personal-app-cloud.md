# PRD: Personal App Cloud

**Status:** Draft  
**Owner:** Marti  
**Date:** 2026-09-14  
**Related:** `ARCHITECTURE.md`, `docs/ROADMAP.md`, `docs/adr/`

---

## 1. Problem statement

Vibe coding makes it trivial to generate software, but it does not make it easy to *own* software. A person who builds 20 small apps still has to solve hosting, databases, authentication, deployment, sync, backups, and mobile access for each one. Existing personal-cloud tools (Umbrel, CasaOS) are server-first and require dedicated hardware; generic clouds (Vercel, AWS) are designed for public apps and require infrastructure work the user should not have to think about.

The gap: **there is no consumer product that turns the devices a person already owns into a personal app cloud.**

## 2. Product vision

A **Personal App Cloud** is an operating layer for software a person creates. A user installs a lightweight agent on their devices; those devices discover each other and become a single infrastructure. Every app the user builds gets a runtime, storage, identity, and network access — but the "cloud" is physically the user's own hardware.

**Principle:** Your data. Your apps. Your devices. Your cloud.

## 3. Goals

### Primary goals

- **Zero-setup app ownership.** A user can take a vibe-coded project and deploy it in under 2 minutes without knowing what Docker, a database, or a domain is.
- **Device-agnostic execution.** The platform decides where an app runs based on resources, battery, network, and offline requirements.
- **Data stays on the user's devices.** No central database for user app data; storage and sync are end-to-end encrypted.
- **App portability.** An app is a package that can be moved or restored across devices without reconfiguration.

### Non-goals

- Not a public app marketplace.
- Not a SaaS hosting provider; we do not host user apps for them.
- Not a general-purpose server OS like Umbrel.
- Not a consumer cloud-storage product (files are only part of an app package).

## 4. Target users

- **Solo builders** who use AI coding tools (Cursor, Claude Code, Lovable, Replit) to make personal utilities.
- **Privacy-conscious power users** who want local-first software but do not want to administer a server.
- **Small households** who want to share a small number of apps (e.g., a family budget) without giving a company their data.

## 5. User stories

### Core stories

1. **Add a device.** "I install the app on my iPhone and it says 'Welcome to your Personal Cloud. Add this device?' I tap yes, then install the companion on my Mac; they find each other and become one environment."
2. **Deploy an app.** "I finished a car-maintenance tracker in Cursor. I say `appcloud deploy` and it appears on my iPhone and Mac."
3. **Run anywhere.** "My desktop is asleep, so the budget app runs on my phone until the desktop wakes up; then it moves."
4. **Share safely.** "I give my partner access to the recipes app, but not my finance app. They see only what I allow."
5. **Backup and restore.** "I replace my laptop and restore my Personal Cloud from my phone in minutes."

### Advanced stories

6. **App Operator.** "I tell the AI 'Add Google login to the invoice app' and it configures auth for me."
7. **Cross-app automation.** "Take the expenses from my receipt scanner and put them into my budget app." The platform knows both apps belong to the same user and can safely connect them.

## 6. Functional requirements

### 6.1 Device mesh

- Each device has a stable Ed25519 identity (see `pai-identity`).
- Devices pair via QR code or a short code; pairing is mutual.
- The mesh uses a gossip/relay protocol for discovery and connection (see `pai-sync/relay.rs`).
- Devices expose capabilities: CPU, RAM, storage, GPU, battery status, network type.
- The platform treats the mesh as **one computer made of many devices**.

### 6.2 App packages

An app is a signed, self-contained package:

```
MyGarageApp/
├── application/          # WASM or native binary
├── database/             # SQLite + migrations
├── files/                # user files
├── configuration.toml    # env vars, secrets refs, permissions
├── permissions/          # capability list
└── versions/             # immutable history
```

- The package format is portable across OSes.
- Packages are signed by the user's identity; unverified packages are rejected.
- The package defines what the app is allowed to access (files, camera, network, other apps).

### 6.3 Placement and execution

- A **placement engine** (`pai-broker`) scores devices on load, power, offline need, and user preference.
- An app can be:
  - **Pinned** to a device (e.g., "always run on desktop").
  - **Replicated** (e.g., database synced across laptop and desktop).
  - **Migratable** (e.g., runs on phone when desktop is asleep).
- Apps run in a sandbox with the permissions declared in the package.

### 6.4 Sync

- App state is modeled as encrypted objects (`SyncObject` in `pai-sync`).
- Objects are stored on at least two devices for redundancy.
- Conflict resolution is per-field last-write-wins for simple apps; CRDTs for collaborative apps later.
- Sync works on LAN, Tailscale, or direct P2P; it does not require a public cloud relay.

### 6.5 Access and sharing

- Every app gets a stable URL on the personal cloud (`app.user.devices`).
- Authentication is based on the user's device keys; no passwords.
- Sharing is capability-based: a user can grant read/write/admin on a per-app basis.
- Remote access uses existing VPN (Tailscale) or HTTPS + device certificates; no open ports.

### 6.6 Backups

- App packages are backed up to user-selected locations: another device, external drive, or a zero-knowledge cloud bucket.
- Backups are versioned and can be restored to a new device.
- A "rescue mode" allows restoring the entire Personal Cloud from a single device.

### 6.7 Model packs

- Model weights can live on removable storage instead of the system drive: `pai models install <slug> --to E:\pai-models`.
- A pack directory is self-describing (`index.json` records each file's manifest + sha256), so **any** pai device adopts it on plug-in — no re-download, no re-registration (`pai models scan`, or lazily on first use via `locate`).
- An unplugged drive degrades gracefully: `models list` shows `installed (offline)`, and the model drops out of `runnable` until the drive returns — under any letter or mount point.

### 6.8 App Operator

- An AI agent (`pai-agent`) can inspect an app package and configure infrastructure:
  - "Add Google login" → configures OAuth.
  - "Give Sarah access" → creates a scoped capability.
  - "Back up the database" → snapshots and stores.
  - "Why is my app broken?" → inspects logs, permissions, device health.
- The App Operator must request approval for any security-sensitive action.

## 7. Non-functional requirements

| Requirement | Target |
|-------------|--------|
| Local-first | Apps work when the internet is down; sync happens when devices reconnect. |
| Privacy | No user app data leaves the device mesh unencrypted; telemetry is opt-in. |
| Security | Sandboxed apps, least-privilege permissions, E2EE sync, signed packages. |
| Performance | Sub-100 ms UI interaction; app start < 2 s on device. |
| Reliability | No single point of failure; a dead laptop does not lose data. |
| Power | Mobile devices only run apps when necessary; background work is battery-aware. |

## 8. Architecture (high-level)

The Personal App Cloud extends the existing Rust core and Flutter UI. New components are **bold**.

```
┌─────────────────────────────────────────────────────────────┐
│ Experience Layer                                            │
│  apps/desktop (Flutter)          apps/mobile (Flutter)      │
│  apps/cli (`pai`)                apps/web (optional)        │
└───────────────────────────┬─────────────────────────────────┘
                            │ C ABI: pai_init / pai_send / pai_audit
┌───────────────────────────▼─────────────────────────────────┐
│ Core Layer (Rust workspace)                                 │
│                                                             │
│  pai-agent      bounded agent loop (App Operator)          │
│  pai-inference  model provider (local Ollama / OpenRouter) │
│  pai-tools      tool registry (deploy, share, backup)      │
│  pai-permissions policy engine (app capabilities)           │
│  pai-identity   device keys, user identity                  │
│  pai-sync       E2EE object sync (app state)                │
│  pai-storage    per-app SQLite + blob store                 │
│  pai-broker     placement (local → trusted → cloud)        │
│  pai-config     TOML config + ComputePolicy                 │
│                                                             │
│  **pai-apps**   app package format, sandbox, registry      │
│  **pai-mesh**   device discovery, NAT traversal, pairing    │
│  **pai-share**  capability-based access control            │
└─────────────────────────────────────────────────────────────┘
```

### New crates

| Crate | Role |
|-------|------|
| `pai-apps` | App package format, manifest parsing, sandbox runner, version store |
| `pai-mesh` | Device discovery, pairing protocol, connection management |
| `pai-share` | Capability tokens, per-app ACLs, invite flow |
| `pai-backup` | Snapshot, restore, rescue-mode logic |

### Data flow (deploy)

1. User runs `pai deploy ./MyGarageApp` or the desktop UI accepts a folder.
2. `pai-apps` validates the package, signs it with the user's key.
3. `pai-broker` picks the best device for the workload.
4. `pai-mesh` copies the package to the target device and registers the app.
5. `pai-sync` begins syncing the app's database and files.
6. The app appears in the user's dashboard with a URL.

## 9. MVP

**Goal:** One device can run one app; the user can see it in a dashboard.

- `pai-apps` package format (WASM + SQLite + manifest).
- `pai` CLI commands: `pai init`, `pai deploy`, `pai list`, `pai open`.
- `pai-storage` per-app database.
- `pai-identity` device key.
- `pai-mesh` LAN-only discovery (no internet).
- Flutter desktop dashboard showing running apps.

**Success metric:** a user can build a "hello world" app, deploy it, and open it on the same machine in < 5 minutes.

## 10. V1 (Multi-device)

- `pai-mesh` internet pairing via Tailscale or a rendezvous server.
- `pai-sync` object sync across devices.
- `pai-broker` placement with resource scoring.
- Mobile companion app that can run lightweight apps and sync.
- Backup/restore to another device.

## 11. V2 (Sharing + Operator)

- `pai-share` capability tokens and invite flow.
- App Operator AI (`pai-agent` + `pai-tools`) for deploy/share/backup commands.
- Cross-app automation with explicit user consent.

## 12. V3 (Portability)

- App migration on device sleep/wake.
- "Rescue mode" full restore from a single device.
- App Store-like catalog of user-built apps.

## 13. Risks

| Risk | Mitigation |
|------|------------|
| iOS background limits | Keep heavy apps on desktop; mobile runs light sync + UI. |
| NAT traversal | Use Tailscale first; add WebRTC/relay fallback later. |
| App sandboxing | WASM runtime with capability tokens; no raw binary execution. |
| Sync conflicts | Last-write-wins per field for MVP; CRDT later. |
| Complexity | Keep the "Deploy to Personal Cloud" UX one-tap; hide all infrastructure. |

## 14. Open questions

- Should the platform require a paid relay for NAT traversal, or remain Tailscale-only?
- How do we handle app signing when the user generates code on many different machines?
- What is the minimal WASM ABI for database access?
- Should sharing ever expose an app to the public internet, or only to trusted devices?
- What is the business model: one-time purchase, subscription for convenience, or free/open?

## 15. Success metrics

- **Time to first app:** < 5 minutes from download to a running app.
- **Local execution:** > 80% of app compute happens on the user's devices.
- **Sync reliability:** < 1% lost writes across a device mesh of 4 devices.
- **Backup restore:** A full Personal Cloud restore completes in < 15 minutes.
- **User retention:** A user keeps at least 3 apps deployed after 30 days.

---

*This PRD assumes the Personal AI workspace (`pai-*`) becomes the runtime for the Personal App Cloud. The next step is an ADR for the app package format and a spike on `pai-mesh` pairing.*
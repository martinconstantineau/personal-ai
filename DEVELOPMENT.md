# Development

## Toolchain

- **Rust 1.97+** via rustup (edition 2021, resolver 2)
- **Flutter 3.47+** for `apps/desktop` (Linux desktop needs `clang`, `cmake`,
  `ninja`, `pkg-config`, GTK3 dev packages — `scripts/setup.sh` installs them)
- No other system deps: SQLite is bundled via `rusqlite`'s `bundled` feature.
- **Windows-gnu note:** `rust-toolchain.toml` pins
  `x86_64-pc-windows-gnu`. Crates that generate import libs at build time
  (e.g. `windows-sys` 0.60+) invoke `dlltool`, which needs GNU binutils
  (`as`) on PATH — Git-for-Windows doesn't ship them. Dependencies that
  require this (native-tls/schannel, aws-lc-rs) are intentionally avoided;
  TLS goes through rustls+ring+webpki-roots, which ship prebuilt objects.
  If a future dep reintroduces the failure (`error calling dlltool`),
  prefer a pure-Rust alternative over installing binutils.
  Note `windows-sys` 0.61 already enters the tree via `chrono` and
  generates import libs at link time — `cargo test`/`cargo build` on
  this machine therefore needs GNU binutils on PATH
  (`export PATH="$HOME/scoop/apps/mingw/current/bin:$PATH"`, Scoop's
  `mingw` package provides `as`/`dlltool`; the rust-mingw component
  ships `dlltool` but not `as`, so the bundled copy alone fails).

```bash
./scripts/setup.sh     # Debian/Ubuntu system deps + rust components
./scripts/test.sh      # cargo fmt --check, clippy -D warnings, cargo test
./scripts/build_core.sh            # build libpai_ffi for the desktop app
./scripts/run_desktop.sh           # build core + flutter run -d linux
./scripts/install_model.sh <id>    # download a model manifest + weights
./scripts/build_android_ffi.sh     # libpai_ffi.so → android jniLibs (3 ABIs)
```

## Mobile (Android)

Prereqs: Flutter 3.47+, Android SDK (platform 36, build-tools 36),
NDK r28+, `cargo-ndk`, the three `*-linux-android` rustup targets, and
prebuilt static OpenSSL per ABI (`OPENSSL_OUT` dir — see the script header;
vendored `openssl-src` can't cross-compile on Windows hosts).

```bash
OPENSSL_OUT=~/src/ossl-out ./scripts/build_android_ffi.sh   # arm64-v8a, armeabi-v7a, x86_64
cd apps/desktop && flutter build apk --release              # or appbundle
```

- `minSdk = 26`: `cpal` links AAudio, which needs API 26+.
- The APK packages `libpai_ffi.so` from `android/app/src/main/jniLibs/`
  (gitignored — it is a build artifact). `dart:ffi` resolves it by name.
- The app data dir is the app's private files dir
  (`/data/data/<applicationId>/files`); `path_provider` is intentionally
  absent (the current build host lacks symlink privilege — see
  `pubspec.yaml`).
- Manifest permissions: `INTERNET` (connectors/relay), `RECORD_AUDIO`
  (voice capture — still requires the runtime grant), cleartext traffic
  allowed for LAN sync relays.
- Release APKs sign with the debug key until a keystore is configured —
  fine for sideloading, not for Play distribution.
- iOS is not buildable from Windows/Linux hosts — needs a macOS + Xcode
  machine.

## Windows desktop build

VS 2022+ BuildTools with the C++ workload (doctor detects it), then
`cd apps/desktop && flutter build windows`. Copy `target/release/
pai_ffi.dll` next to `pai_app.exe` in the output bundle — the FFI loader
resolves it by name.

## Common commands

```bash
cargo build --workspace              # everything
cargo test --workspace               # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings
cargo audit                          # vulnerability scan (CI enforces)
cargo deny check                     # licenses/bans/sources (CI enforces)
cargo run -p pai-cli -- demo         # vertical slice
cargo run -p pai-cli -- chat --provider llama-server \
    --server-url http://127.0.0.1:8080 --model local-model
cd apps/desktop && flutter analyze && flutter test
```

## CLI surface (V1)

```bash
pai chat [--conversation <id>] [--isolated]      # persistent, scoped chat
pai models list|install|uninstall|runnable       # catalog + hf:// refs
pai models detect                                # probe local servers/binaries
pai models search <q>                            # Hugging Face GGUF repos
pai models files <owner/repo> [--revision r]     # .gguf files in a repo
pai models serve <slug> [--port 8080]            # run via llama-server
pai conversations list|new|rename|delete|history|scope
pai runs interrupted|resume|abandon              # crash-safe run recovery
pai policies list|set <PERMISSION> <POLICY>      # persisted policy edits
pai memories                                     # memory browser
pai memories forget <uuid|query>
pai audit [--limit N]
pai apps sign|verify|list|run|remove             # app package signing, registry, sandboxed run
pai deploy <dir> [--upgrade]                     # verify + install a signed app package
```

`pai deploy` also upserts the `apps` registry row — the next `pai sync
push` ships the package as a sealed `app/<id>` object and every paired
device installs it (signature re-verified against `sync_peers` before
`install_trusted`; tampered/foreign-signed packages are skipped, not
installed). `pai apps remove` tombstones the row so deletion propagates.
Live `data/` never syncs — recipients provision it fresh and upgrades
preserve it. Coverage: `cargo test -p pai-integration-tests --test
v4d_app_sync`.

App state instead travels via backups: `pai apps backup <id>` snapshots
the package + `data/` into `backups/<app>/<writer>.pak` and ships a
sealed `bkp/<app>/<writer>` object on the next push. Peers store it
(never auto-restore); `pai apps restore <id> [--from <writer>]` verifies
the embedded signature, reinstalls, and swaps `data/` with rollback —
it's also the rescue path when an install is lost. Coverage: `cargo
test -p pai-integration-tests --test v4f_backups`.

Placement: `apps.active_device` (schema v12) names the single device
running an app's live `data/` — `NULL` means legacy "runs
everywhere". `pai apps migrate <id> --to <peer>` moves an app: a
migrate-flagged backup + the placement update ship on the next push,
the target restores inline on pull, and the source's `data/` is
parked at `apps/.<id>.data.inactive-<ts>` (recoverable, not deleted).
`apps run`, `apps backup`, and `apps restore` all refuse on a device
that isn't active; `pai apps list` shows each app's placement.
Coverage: `cargo test -p pai-integration-tests --test v4j_migration`.

LAN sync needs no --relay flag: `pai sync serve --announce` broadcasts a
signed multicast announcement and authenticates callers by the
pairing-derived `hex(peer_key)` bearer token; `pai mesh discover` lists
announcing paired devices and `pai sync run|push|pull|status --lan`
picks one and syncs. Coverage: `cargo test -p pai-integration-tests
--test v4e_mesh`.

Model sources: `pai models install` accepts a catalog **slug** or an
`hf://<owner>/<repo>/<file.gguf>[@revision]` reference — resolved against
the Hugging Face hub (size + sha256 from `x-linked-*` headers, verified on
install).

## Adding a tool

1. Implement `Tool` in `pai-tools` (see `CalculatorAdd` / `MemoryRemember`).
2. Declare `required_permissions()` — the policy engine gates it automatically.
3. Register it in `builtin_registry()` and give `PolicyTable::with_defaults()`
   a sensible default for any new permission it introduces.
4. Add an integration test in `tests/` driving it through `AgentRuntime::run`
   — tools are only "real" when reached through the permission engine.

## Adding an inference provider

Implement `InferenceProvider` (`generate` + `capabilities` + `stream`):
parse model output with `parse_action`, honor `AIRequest.tools` by
appending `protocol_prompt()` or native tool calling, and tag untrusted
observations. `stream` yields raw model text as `StreamEvent::Delta`
chunks — the agent decodes the action protocol itself, so providers never
parse their own stream. Free/local providers only may be marked
`ComputePolicy::Local*`-compatible; anything remote stays behind
`CloudAllowed`/`CloudPreferred`.

## Adding a crate

- Name it `pai-<thing>`, put it under `crates/`, add it to the workspace
  `members` and the `tests` crate's dev-deps if the slice should cover it.
- Depend downward only (see ARCHITECTURE.md's layer rules). `pai-core` stays
  dependency-free.
- Every public unsafe needs a `# Safety` section; clippy runs with
  `-D warnings` in CI.

## Conventions

- Errors: `pai_core::Error` variants; crates return `pai_core::Result<T>`.
- Time: RFC 3339 UTC strings (`storage::ts` / `parse_ts`).
- Ids: typed wrappers via `id_type!` — never bare `String`.
- Audit: record `ToolRequested` *before* policy, `ToolDenied`/`ToolExecuted`
  after; secrets are redacted by `pai_audit::redact` — use it for arg blobs.
- No `unwrap()` in library code (tests/binaries may). No `unsafe` outside
  `pai-ffi`.
- Keep the FFI boundary `String -> String` JSON; never expose Rust types.

## Layout rules for docs

Architecture decisions that change scope, dependencies, security posture, or
public interfaces need an ADR (`docs/adr/NNNN-title.md`) in the same PR.

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

```bash
./scripts/setup.sh     # Debian/Ubuntu system deps + rust components
./scripts/test.sh      # cargo fmt --check, clippy -D warnings, cargo test
./scripts/build_core.sh            # build libpai_ffi for the desktop app
./scripts/run_desktop.sh           # build core + flutter run -d linux
./scripts/install_model.sh <id>    # download a model manifest + weights
```

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
```

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

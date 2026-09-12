# Cross-platform strategy

## One core, native everywhere

The Rust core compiles to every target we care about:

| Target | Core artifact | UI |
|---|---|---|
| Linux x86_64 | `libpai_ffi.so` | Flutter desktop (GTK) |
| macOS (arm64/x86_64) | `libpai_ffi.dylib` | Flutter desktop / iOS |
| Windows | `pai_ffi.dll` | Flutter desktop |
| iOS | `libpai_ffi.a` (static) | Flutter iOS |
| Android | `libpai_ffi.so` per ABI | Flutter Android |

The same `pai_*` symbols are exported everywhere; the Dart side loads via
`dart:ffi` `DynamicLibrary.open` with per-platform names.

## Why dart:ffi instead of flutter_rust_bridge

- Zero codegen in the build path — the boundary is 5 C functions.
- JSON strings cross the boundary; Dart never mirrors Rust structs, so the
  two sides evolve independently.
- Debugging is `printf`-level: every call is inspectable text.
- Cost: we hand-maintain the JSON schema (documented in `pai-ffi` docs).
  If the surface grows past ~10 calls we can adopt FRB without changing the
  core — the ADR (0005) records this as a reversible decision.

## Performance notes

- Inference is delegated to a server process (`llama-server`) — the heavy
  SIMD/quantized math happens in llama.cpp's tuned kernels, not in our code.
- `pai_send` blocks; the Flutter bridge runs it on a worker isolate so UI
  stays at 60fps during generation.
- SQLite is `bundled` — identical behavior on every OS, no system-dep drift.
- Models are selected per device via `probe_capabilities()` (RAM, cores) +
  `ModelManifest::fits()` so a phone never tries to load a 13B model.

## Platform-specific seams (deliberately narrow)

| Seam | Today | Per-platform plan |
|---|---|---|
| Secret storage | file `0600` | iOS Keychain / Android Keystore / libsecret |
| Notifications | — | platform channels per OS |
| Document pickers / share | — | Flutter plugins (UI side only) |
| Voice I/O | traits | platform audio under `pai-voice` impls |
| Model backends | llama.cpp HTTP | MLX on Apple Silicon, NNAPI delegates |

Nothing platform-specific may live in `pai-core`; each seam is a trait whose
impls are selected at runtime/compile time in the edge crates.

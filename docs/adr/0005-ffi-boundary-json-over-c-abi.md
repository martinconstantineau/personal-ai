# ADR 0005: Hand-rolled C ABI with JSON payloads (not codegen)

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

The Rust core must be callable from Flutter on all targets.

## Decision

A deliberately tiny surface in `pai-ffi`: `pai_init`, `pai_send`,
`pai_audit`, `pai_memories`, `pai_free_string`, `pai_free` — opaque handle
in, heap-allocated JSON out, caller frees. Dart side uses `dart:ffi` inside
a worker isolate (`pai_bridge.dart`).

## Alternatives considered

- **flutter_rust_bridge**: ergonomic typed APIs, but adds a codegen step to
  every build and mirrors Rust types into Dart (coupling we don't want yet).
  Reversible: FRB can wrap `libpai_ffi` later without core changes.
- **Platform channels / method channels**: async-only, per-OS glue code.
- **cap'n proto / protobuf**: schema overhead for six calls.

## Consequences

- The boundary is debuggable with a hex editor and `jq`.
- Versioning happens inside JSON (`{"v": 1, …}` style, in-band).
- Cost: hand-maintained docs of the JSON schema in `pai-ffi`; acceptable
  while the surface is ≤ ~10 calls. ADR will be revisited if it grows.

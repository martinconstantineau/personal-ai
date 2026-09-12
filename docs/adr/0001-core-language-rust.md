# ADR 0001: Rust for the platform core

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

The core must run identically on phones, laptops, and desktops for years;
hold all user state; and never delegate safety to a runtime we don't control.

## Decision

All platform logic lives in a Rust workspace (`crates/pai-*`), compiled once
per target as a cdylib/staticlib exposed over a C ABI.

## Alternatives considered

- **Kotlin Multiplatform / Swift-native**: great on one OS family, second-
  class elsewhere; doubles the audit surface.
- **Go**: easy concurrency, but GC pauses, no iOS story, weaker FFI.
- **TypeScript/Dart core**: shares a language with the UI, but the platform
  would be tied to a UI runtime and weak on embedded targets.
- **C++**: maximal control, maximal footguns; cargo's dependency story beats
  CMake/vcpkg for contributor longevity.

## Consequences

- Memory safety by construction — critical when the same process parses
  hostile model output and holds the user's private data.
- One codebase → five targets; no per-platform reimplementation of policy.
- Cost: async Rust learning curve; mitigated by keeping async at the edges
  (inference, FFI) and synchronous storage behind a mutex.

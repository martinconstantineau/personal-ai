# ADR 0002: Flutter for the experience layer

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

One UI codebase must cover iOS, Android, and desktop, talking to a Rust core.

## Decision

Flutter (stable channel), communicating with the core exclusively through
the `libpai_ffi` C ABI — JSON in, JSON out.

## Alternatives considered

- **React Native**: JS bridge overhead and per-platform inconsistencies;
  Hermes/JSI adds a second runtime we don't need.
- **Native per-OS (SwiftUI + Compose + Qt)**: best fidelity, 3× maintenance
  for a solo/small team — rejected on long-term maintainability grounds.
- **Tauri/Electron**: web content on mobile is second-class; Electron's
  footprint contradicts the efficiency requirement.
- **Dioxus/Slint (Rust UI)**: promising but immature widget ecosystems.

## Consequences

- dart:ffi gives direct synchronous calls; we run blocking `pai_send` on a
  worker isolate so the UI thread never stalls.
- Flutter desktop Linux is the first verified target; mobile needs no new
  core work — only platform glue (keystore, audio).

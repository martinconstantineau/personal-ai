# ADR 0003: Inference via OpenAI-compatible local servers

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

Inference must be free, offline-capable, and swappable across devices with
very different capabilities (GPU laptop ↔ 4GB phone).

## Decision

`InferenceProvider` targets the OpenAI-compatible HTTP API
(`/v1/chat/completions`, `/v1/models`). `LlamaServerProvider` is the
reference implementation, covering llama.cpp `llama-server`, Ollama, LM
Studio, and self-hosted vLLM. Tool use is driven by `protocol_prompt()` +
`parse_action()` (structured JSON actions) so any instruct model can
participate.

## Alternatives considered

- **Embedded llama.cpp crates (`llama-cpp-2`)**: no server to manage, but a
  cmake/C++ build in `build.rs` hurts portability and CI time; adds a native
  dep to every build. Revisit for mobile where a separate process is
  heavyweight — the trait boundary makes this a local change.
- **candle/burn in-process**: pure Rust, but quantized GGUF support and
  kernel coverage lag llama.cpp today.
- **Per-vendor SDKs** (Ollama API, etc.): fragments the provider layer;
  OpenAI-compat already covers all of them.

## Consequences

- `cargo build` stays hermetic — no model-runtime C++ in our build.
- Serving is an ops detail (`scripts/install_model.sh` + one server binary).
- Mobile path: same trait, future `ExecuTorch`/`MLC` impl — no core changes.

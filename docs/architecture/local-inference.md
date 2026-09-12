# Local inference

## Requirements

- **Free only**: every provider path must be usable at $0 — local runtimes
  (llama.cpp, Ollama, LM Studio) or self-hosted OSS servers.
- **Replaceable**: the `InferenceProvider` trait is the only contract; the
  rest of the system can't tell llama.cpp from ExecuTorch.
- **Capable of the protocol**: providers must drive the tool-call JSON
  protocol — either via native tool calling or `protocol_prompt()` +
  `parse_action()`.

## Architecture

```
pai-agent ──AIRequest──▶ InferenceProvider
                            │
              ┌─────────────┼────────────────────┐
              ▼             ▼                    ▼
     LlamaServerProvider  (future)         EchoProvider
     POST /v1/chat/…      MLX / ExecuTorch  deterministic
     llama.cpp·Ollama·    Candle / Burn     test provider
     LM Studio·vLLM
```

`LlamaServerProvider` targets the **OpenAI-compatible HTTP API** — one client
covers llama.cpp's `llama-server`, Ollama (`:11434/v1`), LM Studio
(`:1234/v1`), and self-hosted vLLM/TGI. No native build of llama.cpp is
linked in, which keeps `cargo build` fast and hermetic; running a model is
an operational concern (`scripts/install_model.sh` + any server binary).

## The tool protocol

The system prompt carries `protocol_prompt()`: the list of tool descriptors
plus the exact response schema:

```json
{"action": "tool_call", "tool": "calculator.add", "args": {"a": 2, "b": 3}}
{"action": "final", "answer": "…"}
```

`parse_action` finds the outermost `{…}` in the completion (tolerating
markdown fences and preamble), validates the schema, and returns a typed
`ModelAction`. Models that can't follow the protocol are rejected by
`capabilities()` checks at provider-registration time (a `ToolUse`
capability bit).

## Model management (`pai-models`)

- `ModelManifest`: id, name, parameters, quantization, context length,
  required/declared `ModelCapability`s, `min_ram_mb`, download `sources`
  with sha256.
- `builtin_catalog()` ships known-good free models (SmolLM2 135M/360M
  instruct GGUFs from Hugging Face) — small enough for phones.
- `ModelManager::install` downloads to `models/<id>/`, verifies sha256,
  and only then marks the model installed. `fits()` gates on device RAM.
- Serving is out of scope: `pai models runnable` prints the exact
  `llama-server` command for the installed model.

## Streaming & cancellation

`EventStream` yields tokens as they arrive; `CancelToken` is checked between
agent steps (and providers should abort in-flight HTTP on drop — tracked in
ROADMAP V1.1: stream + cancel wiring through the FFI layer).

## Roadmap for providers

| Milestone | Provider |
|---|---|
| V1 | llama.cpp HTTP (done), Ollama auto-detect |
| V1.1 | streaming to UI, speculative-draft pair support |
| V2 | MLX (Apple), ExecuTorch (mobile), embedding server for memory |
| V3 | distributed placement via `pai-broker` trusted devices |

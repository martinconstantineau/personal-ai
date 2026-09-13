# Personal AI

A local-first, multimodal **Personal AI operating layer** — a privacy-focused
alternative to cloud-centric assistants. Your conversations, memories, and
documents stay on your devices; inference runs on free, local models.

**The model is replaceable. The platform is not.**

Canonical repo: [GitLab](https://gitlab.com/martin.constantineau.ca/personal-ai)
(GitHub is a read mirror that also runs CI).

- 🖥️ **Cross-platform** — Rust core compiles to Linux, macOS, Windows, iOS,
  Android. Flutter UI shares one codebase across all of them.
- 🆓 **Free models only** — inference via `llama-server` (llama.cpp), Ollama,
  LM Studio, or any OpenAI-compatible local endpoint. No paid API required.
- 🧠 **Persistent memory** — the AI remembers what you tell it, scoped and
  searchable, stored locally in SQLite with FTS5.
- 🔐 **Permission-gated agents** — every tool call passes a least-privilege
  policy engine; side effects require approval.
- 📜 **Audited** — every decision, tool call, and memory write is recorded in
  a local, tamper-evident audit log.
- 🔄 **Sync designed, not bolted on** — end-to-end-encrypted object sync via
  pluggable transports (folder transport implemented; relays are ciphertext-only).

## Status: V1.1 — hardened local assistant

V1 delivered the day-to-day surface (streaming answers, approvals,
conversations, memory, resume, Hugging Face models). V1.1 hardens it:
**at-rest encryption (SQLCipher) keyed from the OS keystore, interactive
CLI approvals, Ollama embeddings + vector recall, document ingest with
RAG citations (`pai docs`, Flutter Documents screen), and a filesystem
jail for model-driven file reads**. See [docs/ROADMAP.md](docs/ROADMAP.md)
for what's next and [ARCHITECTURE.md](ARCHITECTURE.md) for the design.

## Quick start

```bash
# Prerequisites: Rust 1.97+ (rustup), Flutter 3.47+ (for the desktop app)
./scripts/setup.sh        # installs system deps on Debian/Ubuntu
./scripts/test.sh         # fmt + clippy + full test suite

# Run the vertical slice end-to-end (offline, deterministic):
cargo run -p pai-cli -- demo

# Install + serve a local model (serving needs llama.cpp on PATH):
cargo run -p pai-cli -- models detect                        # what's already here?
cargo run -p pai-cli -- models list                          # catalog
cargo run -p pai-cli -- models install qwen2.5-0.5b-instruct-q4_k_m
cargo run -p pai-cli -- models serve qwen2.5-0.5b-instruct-q4_k_m

# …or pick any GGUF straight from Hugging Face:
cargo run -p pai-cli -- models search "qwen 3b"
cargo run -p pai-cli -- models files Qwen/Qwen2.5-3B-Instruct-GGUF
cargo run -p pai-cli -- models install \
    hf://Qwen/Qwen2.5-3B-Instruct-GGUF/qwen2.5-3b-instruct-q4_k_m.gguf

# Chat — `auto` probes running servers, or point at one explicitly:
cargo run -p pai-cli -- chat --provider auto
cargo run -p pai-cli -- chat --provider llama-server \
    --server-url http://127.0.0.1:8080 --model my-model
```

The `demo` command exercises the whole stack: it tells the assistant to
remember a fact, asks it back (memory recall injected into the prompt), asks
an arithmetic question (routed to the `calculator.add` tool through the
permission engine), and prints the resulting audit trail.

## Repository layout

```
apps/cli        — `pai` binary: demo, chat, models (incl. Hugging Face),
                  conversations, runs, policies, audit, memories
apps/desktop    — Flutter desktop shell (dart:ffi → libpai_ffi)
connectors/     — external service connectors (email provider trait first)
crates/pai-*    — the Rust core (see ARCHITECTURE.md)
tests/          — workspace-level integration tests
docs/           — architecture deep-dives, ADRs, roadmap, threat model
scripts/        — setup, test, build, model-install helpers
```

## Design guarantees

1. **Nothing leaves the device by default.** `ComputePolicy::LocalOnly` is the
   default; cloud providers can never be used unless the user opts in and the
   model registry admits the provider.
2. **The model never touches the host directly.** Models emit a JSON action
   (`tool_call`/`final`); the agent runtime validates it, asks the permission
   engine, and only then executes the registered tool.
3. **Untrusted content is marked.** Tool results, document text, and retrieved
   memories carry a `TrustLevel`; the inference provider prefixes untrusted
   content so a malicious document can't masquerade as a user instruction.
4. **Every step is auditable.** `pai audit` shows the full event log for any
   run — what the model asked for, what policy decided, what executed.

## Documentation

| Doc | Contents |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | System design, layers, crate map |
| [DEVELOPMENT.md](DEVELOPMENT.md) | Build/test/lint, adding crates & tools |
| [docs/SECURITY.md](docs/SECURITY.md) | Security model, reporting, supply-chain & signed-commit policy |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Style, review checklist, ADR policy |
| [docs/ROADMAP.md](docs/ROADMAP.md) | V1 → V3 milestones and known debt |
| [docs/architecture/](docs/architecture/) | Per-subsystem deep dives |
| [docs/adr/](docs/adr/) | Architecture decision records |
| [docs/security/threat-model.md](docs/security/threat-model.md) | Threat model |

## License

MIT — see [LICENSE](LICENSE).

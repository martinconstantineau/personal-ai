# Personal AI

A local-first, multimodal **Personal AI operating layer** — a privacy-focused
alternative to cloud-centric assistants. Your conversations, memories, and
documents stay on your devices; inference runs on free, local models.

**The model is replaceable. The platform is not.**

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

## Status: groundwork + vertical slice

This repository is the architectural foundation plus one working end-to-end
slice: **remember → recall → use a tool → gated by permissions → recorded in
the audit log**. Everything else is a real, compilable interface — no fake
features. See [docs/ROADMAP.md](docs/ROADMAP.md) for what's next and
[ARCHITECTURE.md](ARCHITECTURE.md) for the full design.

## Quick start

```bash
# Prerequisites: Rust 1.97+ (rustup), Flutter 3.47+ (for the desktop app)
./scripts/setup.sh        # installs system deps on Debian/Ubuntu
./scripts/test.sh         # fmt + clippy + full test suite

# Run the vertical slice end-to-end (offline, deterministic):
cargo run -p pai-cli -- demo

# Chat against it:
cargo run -p pai-cli -- chat

# Use a real local model (llama.cpp llama-server, Ollama, LM Studio):
llama-server -m model.gguf --port 8080          # any OpenAI-compatible server
cargo run -p pai-cli -- chat --provider llama-server --model model.gguf
```

The `demo` command exercises the whole stack: it tells the assistant to
remember a fact, asks it back (memory recall injected into the prompt), asks
an arithmetic question (routed to the `calculator.add` tool through the
permission engine), and prints the resulting audit trail.

## Repository layout

```
apps/cli        — `pai` binary: demo, chat, models, audit, memories
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
| [SECURITY.md](SECURITY.md) | Security model, reporting, hardening |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Style, review checklist, ADR policy |
| [docs/ROADMAP.md](docs/ROADMAP.md) | V1 → V3 milestones and known debt |
| [docs/architecture/](docs/architecture/) | Per-subsystem deep dives |
| [docs/adr/](docs/adr/) | 11 architecture decision records |
| [docs/security/threat-model.md](docs/security/threat-model.md) | Threat model |

## License

MIT — see [LICENSE](LICENSE).

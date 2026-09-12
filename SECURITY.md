# Security

Personal AI's value proposition is privacy: **data stays local, the model is
untrusted, and every action is gated and logged.** This file is the summary;
the full threat model lives in `docs/security/threat-model.md`.

## Security model

- **Local-first by default.** `ComputePolicy::LocalOnly` is the default
  placement policy. Remote providers can only run when the user has
  explicitly set a cloud-allowing policy *and* the provider declares itself.
- **The model is not trusted.** Models emit a constrained JSON action
  (`{"action":"tool_call",...}` / `{"action":"final",...}`). They cannot
  execute anything directly — all effects go through the tool registry and
  the permission engine.
- **Least-privilege tools.** `PolicyTable::with_defaults()` denies unknown
  permissions and requires approval for side effects (sends, deletes,
  writes outside the data dir). Most-restrictive matching rule wins.
- **Prompt-injection defense.** Retrieved memories, tool observations, and
  document content carry `TrustLevel::Untrusted`; providers prefix such
  content so it can't impersonate user instructions. AI-inferred memories
  are stored at a lower trust tier and labeled in prompts.
- **Audit everything.** Permission decisions, tool requests/results, memory
  writes, and run lifecycle transitions are appended to a local audit table.
  Argument blobs pass through `pai_audit::redact`, which masks keys named
  like secrets (`password`, `token`, `api_key`, …).
- **Keys.** Devices get ed25519 keypairs (signing now, key agreement via
  X25519 conversion on the roadmap). Private keys are written `0600` in the
  data dir today; the target is OS-keystore backing (ADR 0011).
- **Sync is ciphertext-only.** Transports move opaque `SyncObject`s; the
  folder transport proves the contract without seeing plaintext.

## Known limitations (groundwork stage)

- SQLite file is not yet encrypted at rest (SQLCipher/`age` decision in
  ROADMAP). OS disk encryption is assumed until then.
- Device keys are file-backed, not keystore-backed.
- Approval UX in the CLI auto-approves via `AutoApprove`; the Flutter app
  will own interactive approval.
- `llama-server` traffic is localhost HTTP — do not point it at remote hosts
  without TLS in front.

## Reporting

This project is pre-release; report vulnerabilities privately to the
maintainer (open a security advisory or email the repo owner) rather than a
public issue. Do not include secrets, personal memories, or data-dir
contents in reports.

## Hardening checklist for releases

- [ ] encrypt store at rest; [ ] keystore-backed device keys; [ ] interactive
      approval UI; [ ] sync E2EE implementation; [ ] supply-chain: lockfile +
      `cargo audit` in CI; [ ] sandboxed tool execution profiles.

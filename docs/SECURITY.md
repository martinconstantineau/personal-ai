# Security policy

A local-first personal AI handles your private data, your credentials, and
— eventually — your devices. Security posture is part of the product, not a
compliance checkbox.

## Reporting a vulnerability

Please **do not** file public issues for security problems. Email the
maintainer or use GitHub's private vulnerability reporting
(“Security → Report a vulnerability”). Include a reproduction and the
affected commit. We aim to acknowledge within 72 hours.

## Supply-chain policy

- `cargo audit` and `cargo deny check advisories licenses bans sources`
  run on every PR (`.github/workflows/ci.yml`, `deny.toml`).
- Dependencies are crates.io only — no git or path sources (`deny.toml`
  `[sources]`).
- Licenses are permissive-only; see `deny.toml` `[licenses]`.
- The agent's permission engine (`pai-permissions`) is deny-by-default and
  **model output cannot change policy**: `policies` rows are written only by
  the CLI/FFI policy APIs, never by a tool. See ADR 0010 + 0012.

## Signed commits

Commits on `main` are required to be **GPG- or SSH-signed and verified**.
Contributors:

1. Configure signing: `git config commit.gpgsign true` plus either
   `user.signingkey` (GPG) or `gpg.format ssh` + `user.signingkey` pointing
   at `~/.ssh/id_*.pub`.
2. Keep `tag.gpgsign` on for release tags.
3. Maintainers enforce this via GitHub branch protection
   (“Require signed commits”) on `main`; PRs merge through the queue only
   after CI (fmt/clippy/test/audit/deny + flutter analyze/test) is green.

Emergency/manual commits by maintainers follow the same rule — no
`--no-verify`, no unsigned pushes.

## Data handling

- Everything lives in `~/.local/share/personal-ai/` (or `--data-dir`);
  nothing leaves the device unless you install a connector.
- Audit log (`pai audit`) records every tool execution, approval decision,
  memory write/delete, and policy change — check it first when something
  looks wrong.
- Known gaps being tracked on the roadmap: at-rest encryption, OS keystore
  for the device key, sandboxed tool execution (V1.1).

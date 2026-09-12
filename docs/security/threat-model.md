# Threat model

Scope: the Personal AI platform at groundwork stage. Assets are **the user's
private data** (conversations, memories, documents, credentials) and **the
device's integrity** (files, network, outbound sends).

## Actors

- **Owner** — the user; fully trusted on their own devices.
- **Devices** — owner's hardware holding keys + data; pairwise trusted.
- **Models** — untrusted probabilistic components, even when local.
- **Tools** — deterministic code paths, trusted only as far as their
  declared permissions.
- **Transports** — sync relays/folders; treated as hostile observers.
- **External content** — web pages, documents, tool output: untrusted input.
- **Remote providers** — only reachable under `CloudAllowed+` policies.

## Threats → controls

| Threat | Control | Where |
|---|---|---|
| Model issues dangerous command | constrained JSON action space; no direct exec path | `parse_action`, `Tool` registry |
| Tool called with bad args | schema validation before policy/execute | `validate_args` |
| Model escalates privileges | least-privilege `PolicyTable`, most-restrictive-wins, deny-default | `PolicyEngine::decide` |
| Side effects without consent | `RequireApproval` on risky classes; audited either way | `ApprovalHandler` |
| Prompt injection via docs/tool output | `TrustLevel` tagging; untrusted prefixing; inferred memory labeled | `Content`, provider |
| Runaway agent loop | `max_steps` + `step_timeout` + `CancelToken`, fail-closed | `AgentRuntime` |
| Secret leakage into logs/audit | `redact()` on arg blobs; no secrets in code | `pai_audit::redact` |
| Data leaves device silently | `ComputePolicy` gates; `Placement::Unavailable` ≠ fallback | `pai-broker` |
| Tampered model weights | sha256-verified install | `ModelManager::install` |
| Sync observer reads data | transports see ciphertext only (E2EE lands V2) | `SyncObject` |
| Rogue device writes | ed25519 signature per object; device revocation | `pai-identity` |
| Malicious connector | connector = `Tool`s + permissions; no special power | `connectors/*` |
| Hostile model suggests exfil via allowed tool | target-scoped rules (e.g. `FileRead` allow ≠ `NetworkEgress`) | `PolicyTable` globs |
| Audit tampering | append-style log; deletion requires `DataDelete` permission | `audit_events` |
| Local physical access | **out of scope until V1.1** at-rest encryption + keystore | ROADMAP |

## Residual risks (accepted, tracked)

1. File-backed device keys on unencrypted disks — V1.1 keystore.
2. `AutoApprove` demo path could approve risky tools if registered — the
   built-in registry is conservative; interactive approval is V1.
3. Localhost HTTP to `llama-server` — no TLS; documented to never expose
   the port remotely.
4. LWW merge can lose concurrent edits — acceptable for personal state;
   CRDT kinds in V2.
5. Side-channel/DoS via huge tool output — observations are truncated at
   injection (provider max_bytes) — budget enforcement is V1.1.

## Security invariants (test-maintained)

- No path from `ModelAction::ToolCall` to execution skips `decide`.
- Denied/approval-required calls never execute *and* are always audited.
- `NeverSync` privacy items never serialize into `SyncObject`s.
- Untrusted content never lands in the transcript without its trust tag.

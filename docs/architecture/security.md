# Security architecture

> Full analysis: `docs/security/threat-model.md`. This doc covers the
> mechanisms and where they live in code.

## Trust boundaries

```
        TRUSTED                          UNTRUSTED
 ┌──────────────────────┐   ┌──────────────────────────────┐
 │ user input           │   │ model output                 │
 │ policy table         │   │ tool observations            │
 │ secrets in keystore  │   │ document/web content         │
 │ signed device msgs   │   │ recalled AI-inferred memory  │
 └──────────────────────┘   └──────────────────────────────┘
              ▲ boundary: TrustLevel tagging + PolicyEngine
```

## Mechanisms

### 1. Constrained model protocol

Models answer with exactly one JSON action. `parse_action` extracts the
outermost JSON object; anything that isn't `tool_call`/`final` is an error,
not a fallback to free execution. There is no code path from model output to
`std::process` except through a registered `Tool` whose
`required_permissions` pass `PolicyEngine::decide`.

### 2. Permission engine

- Rules: `(Permission, target-glob) → Allow|Deny|RequireApproval`.
- Resolution: **most restrictive wins** (Deny > RequireApproval > Allow).
- `with_defaults()`: everything unlisted → Deny. Side effects (file writes
  outside data dir, sends, deletes, shell, network egress) → RequireApproval.
- Approvals flow through `ApprovalHandler`; the CLI uses `AutoApprove` for
  the demo, Flutter will render the `ApprovalRequest` interactively.

### 3. Trust-tagged context

`Content` and `Memory` carry `TrustLevel`. When building prompts:

- user messages → `User`
- verified store state (e.g. user-stated facts) → `Verified`
- tool output, documents, AI-inferred memories → `Untrusted`

`LlamaServerProvider` prefixes untrusted blocks; inferred memories are
injected as `[memory (AI-inferred, unverified)]` so the model itself can see
the distinction.

### 4. Audit with redaction

Every run lifecycle event, permission decision, tool request/result, memory
write, and sync change appends an `AuditEvent`. Argument blobs are passed
through `redact()` which masks values for keys matching
`password|token|secret|api_key|key|credential`.

### 5. Secrets & keys

- Device identity: ed25519 keypair per device (`pai-identity`), private key
  `0600` on disk now, OS keystore on the roadmap (ADR 0011).
- No secrets in the repo, in logs, or in audit payloads.
- Model weights are sha256-verified at install time.

### 6. Transport security

`SyncTransport` moves opaque ciphertext objects. `FolderTransport` proves the
contract (list/put/get/delete) without plaintext — the encryption layer is
specified in the sync doc and lands with the relay transport.

## Fail-closed defaults

- Unknown tool name → observation error to the model, audited, run continues.
- Arg validation failure → tool never executes.
- `max_steps` reached → run ends `Failed`, not "keep going forever".
- `ComputePolicy` mismatch → `Placement::Unavailable`, never silent remote.
- `pai_send` on a poisoned/disposed handle → JSON `{"error": …}`, never UB.

# ADR 0010: Least-privilege permission engine, most-restrictive-wins

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

Tools give the model real-world effects; a compromised or manipulated model
must not be able to escalate. Approvals must be a *policy output*, not a
per-tool afterthought.

## Decision

`Permission` is a closed enum (18 kinds). `PolicyTable` maps
`(permission, target-glob) → Allow | Deny | RequireApproval`;
`PolicyEngine::decide` evaluates all matching rules and returns the
**most restrictive** result (Deny > RequireApproval > Allow).
`with_defaults()` denies anything unlisted. Approvals go through an
`ApprovalHandler` trait; requests carry `RiskLevel` + human-readable reason.

## Alternatives considered

- **Per-tool boolean consent dialogs**: consent fatigue, no policy reuse.
- **OS sandboxing only (seccomp/Seatbelt)**: complementary but orthogonal —
  the policy engine is *semantic* ("may not send email") where sandboxes are
  *mechanical* ("may not open /etc"). We'll add sandbox profiles as a
  `ExecutionMode` per tool (V2).

## Consequences

- New permissions are a deliberate act — defaults deny.
- The same table drives CLI prompts, Flutter sheets, and audit labels.
- Denied requests are still audited (`ToolDenied`) — silent denial would
  hide attack attempts.

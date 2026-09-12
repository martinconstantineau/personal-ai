# ADR 0008: Bounded agent loop — "LLM proposes, code disposes"

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

The agent must be useful (multi-step tool use) while remaining the most
audited, least trusted component — it mediates between a probabilistic model
and the user's real data.

## Decision

`AgentRuntime::run` is a bounded, synchronous-in-spirit loop: every step is
`generate → validate → policy-decide → execute → audit`. Step count is
capped (`max_steps`), each step is timed out, and the loop terminates
fail-closed. Models never see code-execution surfaces — they emit JSON
actions that deterministic code disposes of. (Same principle as the team's
safety-backstop work elsewhere: open-ended text is the model's; everything
that can be a rule is a rule.)

## Alternatives considered

- **ReAct-with-free-text parsing**: brittler; a stray "Action:" in model
  output shouldn't be able to trigger execution paths. The typed
  `ModelAction` + schema validation is stricter.
- **Full planner/executor architectures**: powerful, premature — the
  single-loop with memory recall covers the V1 surface and keeps every
  decision on one auditable path.
- **Unbounded "run until done"**: rejected; a runaway loop is a
  security/cost incident even with free models (device battery, loops).

## Consequences

- Every behavior is reconstructible from the audit trail alone.
- Smarter orchestration is an `AgentDefinition` concern, layered on top.

# ADR 0012: Schema v2 — conversation-scoped memory, run checkpoints, persisted policies

- **Status**: Accepted
- **Date**: 2026-09-13

## Context

V1 ("usable local assistant") requires three durable structures the v1
schema lacked: (a) conversations must own a memory-isolation mode and
memories must be attributable to the conversation that produced them;
(b) agent runs must be resumable after a crash — the process may die
mid-loop; (c) permission-policy edits made in the UI must survive restart.

## Decision

Schema version 2 (`meta.schema_version`), applied transactionally by the
existing migration loop:

```sql
ALTER TABLE memories      ADD COLUMN conversation_id TEXT;
ALTER TABLE conversations ADD COLUMN memory_scope TEXT NOT NULL DEFAULT 'shared';

CREATE TABLE agent_runs(
  id TEXT PRIMARY KEY, agent_id TEXT, conversation_id TEXT,
  started_at TEXT NOT NULL, ended_at TEXT,
  state TEXT NOT NULL, step INTEGER NOT NULL DEFAULT 0,
  input TEXT, checkpoint_json TEXT);

CREATE TABLE policies(
  permission TEXT PRIMARY KEY, policy TEXT NOT NULL, updated_at TEXT NOT NULL);
```

- `memories.conversation_id` — NULL = global; set = scoped to one
  conversation. `MemoryScopeQuery` (`All | GlobalOnly | Scoped(c, global?)`)
  selects the visibility rule at recall time.
- `agent_runs.checkpoint_json` — the full message array *before* each model
  call. `ended_at IS NULL` marks an interrupted run; resume replays from the
  checkpoint, not from a replayed event log.
- `policies` — sparse overlay: only user-edited rows are stored, merged over
  `PolicyTable::with_defaults()` at startup.

## Alternatives considered

- **Event-sourced runs** (append every `AgentEvent`, fold on resume):
  richer history but heavier; a message-array checkpoint is sufficient
  because the loop is deterministic given (messages, step).
- **Whole-table policy rows**: storing defaults too would freeze shipped
  defaults at first-boot time; the overlay keeps them upgradable.
- **Cascade-deleting a conversation's memories**: rejected — forgetting is a
  permission-gated, audited operation (`memory.forget`), not an accident of
  chat cleanup. `ConversationStore::delete` removes messages only.

## Consequences

- `Store::open` migrates in place; `in_memory` fixtures get v2 directly.
- Resume semantics: an `AwaitingApproval` run resumes by re-asking — the
  approval is ephemeral, the checkpoint is not.
- Untrusted model output still cannot mutate `policies`: writes go through
  `pai_set_policy` / `pai policies set` only, never a tool.

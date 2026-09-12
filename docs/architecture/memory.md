# Memory

The memory subsystem is the platform's long-term advantage: it persists
across models, sessions, and devices, and it's queryable by both the agent
and the user.

## Model

```
MemoryItem {
  id, user_id,
  content,                 // the fact/text
  scope,                   // Global | Conversation | Device
  source,                  // UserStated | AiInferred | Imported | ToolResult
  trust,                   // Verified when UserStated, Untrusted when inferred
  confidence, importance,  // 0..1 — ranked decay and prompt budgeting
  privacy,                 // Normal | Sensitive | NeverSync
  entities: [String],      // people/places/things for graph queries
  embedding: Vec<f32>?,    // when an EmbeddingProvider is available
  created_at, updated_at,  // access/decay bookkeeping
}
```

`source` drives `trust`: only `UserStated` memories get `Verified` —
AI-inferred facts are recalled but labeled `[memory (AI-inferred, unverified)]`
in prompts and never silently become truth. `correct()` exists so a
correction *replaces* a memory with a new item linked to the old one — no
in-place mutation of history.

## Backend: `SqliteMemory`

- **Keyword recall**: FTS5 over content. Natural-language queries are
  tokenized to `tok1 OR tok2 OR …` (FTS5 defaults to AND, which misses most
  real questions); results ranked by FTS score × importance.
- **Vector recall**: when an `EmbeddingProvider` is registered, embeddings
  are stored alongside; recall is brute-force cosine over the table —
  correct and fine at personal scale (<100k items); swap for `sqlite-vec`
  when it matters.
- **Browse-all**: empty/`*` queries return recent items — powers
  `pai memories` and the UI memory screen.
- **Write-through triggers**: inserts/updates/deletes on `memories` mirror
  into `memories_fts` via SQLite triggers — the index can't drift.

## Working memory

`WorkingMemory` is the per-run scratch the agent writes to (`memory.remember`
with `scope=run` semantics): captured as tool observations during the run and
promoted to durable `MemoryItem`s only for content the user or policy marks
worth keeping. This keeps trivial run state out of long-term recall.

## How the agent uses it

1. Before every `generate`, `build_request` calls `recall(query = last user
   message, k=8)` and injects results as `[memory] …` lines.
2. `memory.remember` tool calls write `UserStated`/`Verified` items (only
   when the user said it — the tool requires the phrasing to originate in a
   user message via the `source` arg policy).
3. Sensitive items (`privacy=NeverSync`) never enter `SyncObject`s; the sync
  layer filters on `kind` + `privacy`.

## Deletion

`memory.forget`-style user asks → `delete()` removes the row, FTS entry, and
relationships, and emits a `MemoryDeleted` audit event. GDPR-style export is
`SELECT *` — it's a local SQLite file the user owns.

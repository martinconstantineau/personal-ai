# ADR 0006: Hybrid memory — FTS5 first, vectors optional

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

Recall must work on day one on low-end devices — before any embedding model
is installed — and improve transparently when one is.

## Decision

`MemoryBackend` with a `SqliteMemory` impl: FTS5 keyword recall (tokenized
OR queries for natural language), optional brute-force cosine when an
`EmbeddingProvider` is registered, importance/confidence scoring, and
entity links. `source` drives `trust` (UserStated → Verified).

## Alternatives considered

- **Vector-only (sqlite-vec / external vector DB)**: fails offline-first —
  a fresh install with no embedding model recalls nothing.
- **Knowledge-graph-first**: powerful but premature; entities table keeps
  the door open without buying the complexity.

## Consequences

- Works today with zero model downloads.
- AI-inferred memories are never promoted to `Verified` — corrections use
  `correct()` to append, keeping an honest history.
- Future: `sqlite-vec` ANN index + cross-encoder rerank; trait unchanged.

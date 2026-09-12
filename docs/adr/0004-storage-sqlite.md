# ADR 0004: SQLite (bundled) as the single store

- **Status**: Accepted
- **Date**: 2026-09-12

## Context

The platform needs durable, queryable, transactional storage that works
identically on every OS with zero administration.

## Decision

One `Store` (`rusqlite`, `bundled` feature) per data dir: WAL mode, foreign
keys, `SCHEMA_VERSION` migrations in a `meta` table. Blobs live in a
content-addressed file tree referenced by hash.

## Alternatives considered

- **RocksDB/sled**: key-value only — we'd reimplement FTS, joins, and
  integrity checks.
- **Per-file stores (JSON/markdown)**: human-inspectable but no atomic
  multi-table writes; sync conflicts become file conflicts.
- **Postgres via embedded server**: violates zero-admin and mobile.

## Consequences

- FTS5 covers memory + document search today; `sqlite-vec` can add ANN
  later without changing `MemoryBackend`.
- At-rest encryption is a future `SQLCipher`/`age` decision — the schema and
  `Store` API don't change (tracked in ROADMAP).
- Single-writer model fits one-user-at-a-time; the FFI serializes runs.

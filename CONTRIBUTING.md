# Contributing

## Ground rules

- **Free/local first.** Contributions must not require a paid API or a
  specific vendor. Optional remote providers are fine behind
  `ComputePolicy::CloudAllowed`, but the default path must work offline.
- **No fake functionality.** Ship the interface + real wiring, or mark it
  explicitly as a stub. Never return canned output that looks real.
- **Small, focused changes.** Match surrounding conventions; no drive-by
  refactors.

## Before you push

```bash
./scripts/test.sh    # fmt --check + clippy -D warnings + cargo test
cd apps/desktop && flutter analyze   # if you touched Dart
```

## ADR policy

Any change that picks a dependency, alters the permission model, changes
the FFI surface, adds a storage schema version, or touches the trust model
ships with an ADR in `docs/adr/` (copy `docs/adr/0000-template.md` style:
Context → Decision → Consequences → Alternatives).

## Review checklist

- [ ] Does the model ever get to execute something without the permission
      engine? (Must be no.)
- [ ] Is untrusted content (tool output, docs, recalled memory) tagged
      `TrustLevel::Untrusted` before it reaches the prompt?
- [ ] Are tool args validated before `decide`/execute?
- [ ] Are secrets redacted in audit payloads?
- [ ] New public API documented; unsafe code justified with `# Safety`.
- [ ] Tests cover the failure path (denied permission, bad args, unknown
      tool), not just the happy path.

## Commit style

Conventional-ish prefixes are welcome (`agent:`, `memory:`, `docs:`) but not
enforced; one concern per commit is enforced in review.

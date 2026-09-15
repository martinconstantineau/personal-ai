# ADR 0020: Capability sharing — signed tokens and the guest channel

- **Status**: Accepted
- **Date**: 2026-09-15

## Context

Everything before V4o shared *within* a vault: pairing gives every
device the vault key, so any paired peer may call any broker op. That
model has no way to give a **non-member** — a friend's phone, a
tenant's laptop — access to exactly one app, for a bounded time,
without handing them the vault. The sharing sketches (ADR-0004
`[sharing]`, arch §4.3) wanted capability-style grants but named no
transport or enforcement point.

The design constraint: no new network service. Guests reach the host
over the same `SyncTransport`s everything else uses (shared folder,
relay, LAN), and requests must be verifiable without a vault key.

## Decision

**Tokens** (`pai-share`): a `Capability` is a self-contained JSON
grant — `{token_id, app_id, actions, grantee_key, device, expires,
issued_by, issued_at, signature}` — Ed25519-signed by the issuing
device key over a fixed canonical payload. Verification needs only
the issuer's public key: signature, expiry, revocation tombstone
(`share/revoked/<token_id>`), and action coverage. `grantee_key`
present ⇒ the grant is bound; guest requests must additionally carry
a request signature from that key. Absent ⇒ bearer token — anyone
holding the file may run the app. The ShareStore is plain files under
`<data_dir>/share/` (no schema changes, nothing synced — grants live
and die on the issuer).

**Delegation** (`share` action): a bound grantee may mint a narrower
sub-token signed by its own key. The child embeds its `parent` inside
the token JSON and commits the parent's `token_id` in its signed
payload (tokens without a parent keep the original 8-line payload —
wire-compatible). `verify_chain` walks the chain recursively: each
child verifies against the parent's `grantee_key`, must narrow
(actions ⊆, expiry ≤, same `app_id`, device restriction preserved),
and the parent must carry `share`; only the root resolves `issued_by`
through the issuer table. A bearer parent can't delegate — there is
no bound key to verify a child against. Revocation composes:
tombstoning a parent id refuses every descendant on next request;
a child minted elsewhere can't be tombstoned on the host — revoking
its parent is the kill switch.

**Guest channel** (`pai-share::guest`): requests ride `greq/<to>/<id>`
sync objects carrying the token, args, and an ephemeral X25519 public
key — plaintext, since they contain no vault data and the token is
the auth. Replies are `gres/<eph_tag>/<id>` sealed XChaCha20-Poly1305
to the ephemeral key (request id ⇒ HKDF ⇒ response key), so response
bodies stay private even on a world-readable folder/relay. An
`expires_at_ms` on the request bounds how long a stale request sits
on the transport.

**Enforcement point**: `BrokerServer::with_guest_handler` — each
serve pass polls `greq/<me>/`, verifies, and dispatches through the
same sandboxed `app_run_op` vault members reach via `app-run`. No
vault key means the broker wasn't constructible, so `pai broker
serve` falls back to a guest-only loop — a host that shares to
guests never needs to pair. CLI: `pai apps share|grants|revoke`,
`pai apps run <id> --cap <token.json> --on <device|any>`; `any`
targets the token's `issued_by` since guests can't read sealed
`bcap/` announcements.

## Consequences

- A guest gets app execution without vault membership, pairing UX,
  or a synced identity — the token file is the whole credential.
- Revocation is a tombstone check on the serving device — offline
  for the guest is offline; a revoked token is refused the moment
  the tombstone exists.
- `exec`, `read`, `write`, and `share` are all consumed (`app-run` /
  `app-read` / `app-write` ops plus `pai apps delegate`); delegation
  chains nest — a sub-token carrying `share` may delegate further.
- Bearer tokens are replayable by anyone holding the file — bound
  tokens (`--for`) exist for the stronger case.
- `greq/` payloads are visible on the transport — they reveal which
  app was invoked and the token itself (which is usable by a snooper
  for the bearer case; bound tokens resist this since the snooper
  can't produce the request signature).

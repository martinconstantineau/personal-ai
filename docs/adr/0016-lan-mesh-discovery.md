# ADR 0016: LAN mesh discovery and pairing-derived relay auth

- **Status**: Accepted
- **Date**: 2026-09-15

## Context

V4d syncs app packages (and everything else) as sealed `SyncObject`s over
dumb transports — but LAN sync still required one device to run
`sync serve --relay-token <secret>` while the other typed a matching
`--relay <url> --relay-token <secret>`. That means out-of-band token
sharing for devices that already share pairing keys, plus manual
endpoint discovery on a network where multicast exists.

## Decision

`crates/pai-mesh` provides signed LAN announcements over UDP multicast
(`239.255.71.77:47677`). An announcement carries the device id, name,
platform, relay port, timestamp, and an Ed25519 signature over the whole
payload. `pai sync serve --announce` broadcasts it while serving;
`pai mesh discover` listens and lists only **paired, signature-verified**
devices; `pai sync run|push|pull|status --lan` discovers, authenticates,
and syncs with zero flags.

Three rules keep it honest:

1. **Source IP, not payload IP** — the announcement carries only the
   port; the endpoint IP comes from the datagram's source address, so a
   forged packet can't redirect sync to an attacker host.
2. **Freshness window** — timestamps outside ±15 min are dropped,
   bounding replay of captured datagrams.
3. **Trust at verify time** — `paired_announcements` verifies the
   signature against `sync_peers.ed_pubkey` / own device keys; anything
   unpaired or badly signed is ignored, never surfaced.

Relay authentication moves from a single shared `--relay-token` to a
per-peer bearer: `hex(peer_key)`, where `peer_key` is the pairwise
X25519 secret both sides already derive from pairing. The server side
accepts a validator closure (`bind_dynamic` builds it from the trusted
peer set, recomputed per request so newly paired devices work without a
restart); the flat-token `bind` path remains for explicit use. Only a
paired device can mint a valid token — and the objects it protects are
sealed ciphertext regardless.

## Alternatives considered

- **mDNS/DNS-SD (bonjour)**: heavier dependency, more parsing surface,
  and still needs an application-layer trust check — the signature is
  the part that matters, so we keep a minimal signed datagram instead.
- **mTLS on the relay**: correct long-term, but pulls a TLS stack +
  certificate story into a LAN path whose payload is already E2EE;
  pairing-derived bearer gets the same "only paired devices" property
  with zero new key material.
- **Payload-advertised IP**: simpler, but lets a forger redirect
  clients; source-IP derivation is strictly safer.
- **Global discovery (rendezvous server)**: contradicts local-first —
  WAN discovery stays an explicit relay address.

## Consequences

### Positive

- Zero-config LAN sync: deploy on one device, `pai sync run --lan` on
  the other — no token copying, no addresses.
- Announcements double as presence: `mesh discover` answers "which of
  my devices are up".
- Legacy flat-token relays keep working; `sync run --relay … --relay-token …`
  is unchanged.

### Negative

- Announcements leak presence on the LAN (device id/name/platform
  visible to any listener) — documented in SECURITY.md.
- The bearer token is replayable on the LAN by anyone who captures it
  — but only a paired device can mint it, and what it protects is
  ciphertext-only anyway.
- Multicast may be filtered on some networks; the explicit `--relay`
  path remains the fallback.

### Mitigations

- Signature + freshness + pairing checks all enforced before a peer is
  used; rejected datagrams are counted and dropped, never fatal.
- Source-IP binding prevents redirect attacks even with a stolen
  signing key on a foreign network.

# ADR 0021: Placement scoring — `bcap` load hints + scored `find_peer`

- **Status**: Accepted
- **Date**: 2026-09-15

## Context

Since V2m, `pai-broker` routes `--on any` calls by capability match:
every serving device announces `bcap/<device>` with its op list, and
`find_peer` picks the **lowest device id** among fresh announcers.
Deterministic, but blind — a laptop at 4% battery wins exactly as
often as an idle wall-powered desktop. The PRD asks for placement on
load, power, and user preference ("the mesh is one computer made of
many devices"), and remote app execution (V4k) made the routing
decision user-visible: a bad pick is a slow app launch on a dying
battery.

## Decision

`bcap` announcements gain an optional `load` hint:

```json
"load": {"busy": 0, "on_battery": false, "thermal_throttled": null,
         "ram_bytes": 8589934592, "cpu_cores": 4}
```

- `busy` is **live**: the serving handler counts in-flight ops
  (`BusyGuard` wraps `handle`/streaming `infer`) and a
  `BrokerServer::with_load_probe` closure samples it at each announce
  (60 s cadence, 300 s TTL — unchanged).
- `on_battery`/`thermal_throttled` are **live** too: the probe calls
  `pai_identity::probe_power()` per announce — `GetSystemPowerStatus`
  on Windows, `/sys/class/power_supply` + cpufreq heuristics on
  Linux, `None` elsewhere (neutral). Registration also uses it, so
  `DeviceCapabilities` rows now carry real power state.
- `ram_bytes`, `cpu_cores` are registration-time hardware constants.

`find_peer` scores each fresh candidate:

```text
score = ram_gb + 2*cores - 10*busy - 1000*(on_battery) - 500*(throttled)
```

highest score wins; equal scores keep the lowest-device-id tiebreak,
so all vault members still agree on the pick.

## Wire compatibility

`load` is an `Option` field — pre-V5 announcements (no `load` key)
deserialize as `None` and score 0: they lose to any idle capable peer
but beat nothing-left-else. Old peers ignore the field when *they*
read new announcements; sealed envelope, AAD, TTL, and reannounce
cadence are untouched.

## Consequences

- `pai broker serve` now publishes load with every announce; all
  `find_peer` callers (`apps run --on any`, `broker call --any`)
  get scored placement with no call-site changes.
- Load hints are **self-reported and unauthenticated beyond the vault
  seal** — fine inside the trust domain (vault members); a malicious
  peer could inflate its score, same as it could already advertise
  any op.
- RAM/cores stay registration-time constants (they don't move);
  power state went live with `probe_power` — a laptop that unplugs
  stops attracting `--on any` work within one announce cycle (≤60 s).
- User preference landed as `BrokerClient::with_weights` — a local
  `place_weight.<device>` meta value (`pai broker prefer <peer> <w>`,
  `broker devices` shows it) added client-side to each candidate's
  score. Local-only by design: preferences are *this* user's routing
  choice, not the announcer's claim, and nothing crosses the wire.

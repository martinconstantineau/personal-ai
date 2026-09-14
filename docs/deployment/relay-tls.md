# Relay behind TLS — deployment notes

`pai sync serve` is deliberately plain HTTP bound to `127.0.0.1`. Every
object it stores is already XChaCha20-Poly1305 sealed, so the relay (and
anyone who reaches it) only ever sees ciphertext — but off-LAN use
should still put it behind a TLS-terminating reverse proxy. This doc is
the recommended setup.

## Why TLS still matters when everything is ciphertext

- **The bearer token is a shared secret in a header.** Over plain HTTP
  it is sniffable; once it leaks, the relay becomes an anonymous object
  store for whoever has it. TLS keeps the token confidential.
- **Replay/injection resistance.** A MITM on plain HTTP can replay old
  ciphertext objects or inject blobs under arbitrary keys. The vault
  can't be *read*, but garbage objects waste storage and replayed
  tombstones/claims can confuse convergence. TLS removes the MITM.
- **Metadata narrowing.** Object keys (`memory/<id>`, `breq/<to>/<id>`,
  `vrot/...`) are visible on the wire for routing. TLS hides the sync
  pattern (which devices sync, how often, what kinds of objects) from
  passive observers — the relay operator still sees keys by design.

## Server side

Keep the relay on loopback and terminate TLS one hop in front:

```bash
pai sync serve --dir /var/lib/pai/relay --addr 127.0.0.1:8787 \
    --token "$(head -c 32 /dev/urandom | base64)"
```

Always set `--token` on anything reachable by other hosts (or export
`PAI_SYNC_TOKEN`). The token guards *availability*, not confidentiality —
ciphertext is the confidentiality layer; the token is what stops the
relay becoming a free blob store for strangers.

### Caddy (simplest)

```caddyfile
sync.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Caddy obtains and renews the certificate automatically. Done.

### nginx

```nginx
server {
    listen 443 ssl;
    server_name sync.example.com;

    ssl_certificate     /etc/letsencrypt/live/sync.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sync.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8787;
        proxy_set_header Authorization $http_authorization;
        client_max_body_size 64m;   # document blobs can be large
    }
}
```

`Authorization` must be forwarded explicitly — that's the bearer token.

## Client side

Devices just point at the HTTPS URL — the transport is already reqwest
+ rustls with Mozilla roots:

```bash
pai sync run --relay https://sync.example.com --token "$TOKEN"
# same flags on: push, pull, status, rotate, task tick,
#                broker serve/call, conv sync, docs sync
```

`PAI_SYNC_TOKEN` is honored in place of `--token` everywhere.

## Hardening options (paranoid tier)

- **IP allowlist** at the proxy (`allow`/`deny` in nginx) when device
  IPs are stable.
- **mTLS client certs** — the proxy verifies a device certificate; the
  relay token stays as second factor. Combine with `ssl_verify_client
  on;` and a small internal CA.
- **Separate vhosts per device** if you want per-device token scoping
  (one relay process per vhost upstream, or path-prefix routing — the
  relay has no tenancy model itself).
- **Rate limiting** at the proxy (`limit_req`) — the relay is a dumb
  store; flooding is the main abuse vector a token doesn't stop once it
  leaks.

## What TLS does *not* fix

- A removed peer keeps the old vault key — `pai sync rotate` after
  `pair remove`, always (see SECURITY.md).
- The relay operator sees object *keys* and traffic timing. That's the
  designed metadata floor; don't host your own sync on infrastructure
  you consider an adversary for metadata.

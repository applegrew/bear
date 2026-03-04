# bear-turn

TURN relay server for Bear WebRTC connections, powered by [turn-rs](https://github.com/mycrl/turn-rs).

## Overview

`bear-turn` provides a TURN relay so that WebRTC connections succeed even on devices behind symmetric NATs (e.g. certain iPhones). It uses a shared secret with `bear-relay` for time-windowed HMAC-SHA1 credential validation (TURN REST API / RFC 5389 §10.2).

### Supported transports

| Transport | Port | Description |
|---|---|---|
| UDP | 3478 | Standard TURN |
| TCP | 3478 | TURN over TCP (firewall fallback) |
| TCP | 443 | TURN over TCP on 443 (mobile network fallback) |
| TLS | 5349 | TURNS — TURN over TLS (not yet supported in v3.4.0) |

## Quick start

### Docker

```bash
# Edit turn.toml — set static_auth_secret and external IP
docker compose up -d
```

### From source

Requires Rust 1.88+ (for the `time` crate dependency):

```bash
cargo install turn-server --version 3.4.0 --features tcp
turn-server --config turn.toml
```

## Configuration

All configuration is in `turn.toml` (TOML format). Key settings:

### Auth

```toml
[auth]
static_auth_secret = "your-shared-secret"
```

This **must match** the `TURN_SECRET` environment variable configured in `bear-relay`. The relay mints time-windowed credentials using HMAC-SHA1 over this secret, and the TURN server validates them.

### Interfaces

Each `[[turn.interfaces]]` block defines a listener (v3.4.0 format):

```toml
# UDP
[[turn.interfaces]]
transport = "udp"
bind = "0.0.0.0:3478"
external = "YOUR_PUBLIC_IP:3478"

# TCP (requires --features tcp at install time)
[[turn.interfaces]]
transport = "tcp"
bind = "0.0.0.0:3478"
external = "YOUR_PUBLIC_IP:3478"
```

The `external` address must be your server's **public IP** — this is the address reported to clients in relay candidates.

> **Note:** TLS/TURNS on port 5349 is not supported in v3.4.0. It requires the v4.x
> series which is currently in beta (`4.0.0-beta.4`).

## Credential flow

```
bear-relay                  turn-rs
(mints creds)            (validates creds)
     │                        │
     │    TURN_SECRET (shared) │
     └────────────────────────┘

1. bear-relay generates credentials:
   username  = <expiry_unix_timestamp>
   credential = base64(HMAC-SHA1(TURN_SECRET, username))

2. Credentials are served to:
   - bear-server (via GET offer response)
   - browser/bear.js (via BEAR_ICE_SERVERS page global)

3. Clients use credentials in ICE configuration
4. turn-rs validates using the same shared secret
```

Default credential TTL: 24 hours (configurable via `TURN_CREDENTIAL_TTL` in bear-relay).

## Deployment with bear-relay

Both services share the same `TURN_SECRET`. Example setup:

```
bear-relay:  TURN_SECRET=mysecret  TURN_URLS=turn:TURN_LB_IP:3478
bear-turn:   static_auth_secret = "mysecret"  (in turn.toml)
```

See `bear-relay/README.md` for relay-side TURN configuration.

## Known limitations

- **No TURNS (TLS) support** — turn-rs v3.4.0 doesn't support it. The v4.x series (`4.0.0-beta.4`) adds TLS but is still in beta. Can revisit when v4.0.0 stable is released.

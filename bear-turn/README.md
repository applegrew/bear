# bear-turn

TURN relay server for Bear WebRTC connections, powered by [turn-rs](https://github.com/mycrl/turn-rs).

## Overview

`bear-turn` provides a TURN relay so that WebRTC connections succeed even on devices behind symmetric NATs (e.g. certain iPhones). It uses a shared secret with `bear-relay` for time-windowed HMAC-SHA1 credential validation (TURN REST API / RFC 5389 §10.2).

### Supported transports

| Transport | Port | Description |
|---|---|---|
| UDP | 3478 | Standard TURN |
| TCP | 3478 | TURN over TCP (firewall fallback) |
| TLS | 5349 | TURNS — TURN over TLS (requires certs) |

## Quick start

### Docker

```bash
# Edit turn.toml — set static-auth-secret and external IP
docker compose up -d
```

### From source

Requires Rust 1.85+:

```bash
cargo install turn-server --version 4.0.0
turn-server --config turn.toml
```

## Configuration

All configuration is in `turn.toml` (TOML format). Key settings:

### Auth

```toml
[auth]
static-auth-secret = "your-shared-secret"
```

This **must match** the `TURN_SECRET` environment variable configured in `bear-relay`. The relay mints time-windowed credentials using HMAC-SHA1 over this secret, and the TURN server validates them.

### Interfaces

Each `[[server.interfaces]]` block defines a listener:

```toml
# UDP
[[server.interfaces]]
transport = "udp"
listen = "0.0.0.0:3478"
external = "YOUR_PUBLIC_IP:3478"

# TCP
[[server.interfaces]]
transport = "tcp"
listen = "0.0.0.0:3478"
external = "YOUR_PUBLIC_IP:3478"

# TLS (TURNS)
[[server.interfaces]]
transport = "tcp"
listen = "0.0.0.0:5349"
external = "YOUR_PUBLIC_IP:5349"

[server.interfaces.ssl]
private-key = "/etc/turn/key.pem"
certificate-chain = "/etc/turn/cert.pem"
```

The `external` address must be your server's **public IP** — this is the address reported to clients in relay candidates.

### Port range

```toml
[server]
port-range = { min = 49152, max = 65535 }
```

Relay allocations use ports in this range. Ensure these ports are open in your firewall/security group.

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
bear-relay:  TURN_SECRET=mysecret  TURN_URLS=turn:turn.example.com:3478,turns:turn.example.com:5349
bear-turn:   static-auth-secret = "mysecret"  (in turn.toml)
```

See `bear-relay/README.md` for relay-side TURN configuration.

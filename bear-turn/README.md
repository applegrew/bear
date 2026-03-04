# bear-turn

TURN relay server for Bear WebRTC connections, powered by [coturn](https://github.com/coturn/coturn).

## Overview

`bear-turn` provides a TURN relay so that WebRTC connections succeed even on devices behind symmetric NATs (e.g. certain iPhones on mobile networks). It uses a shared secret with `bear-relay` for time-windowed HMAC-SHA1 credential validation (TURN REST API / RFC 5389 §10.2).

### Supported transports

| Transport | Port | Description |
|---|---|---|
| UDP | 443 | Primary TURN — preferred by all clients. Port 443 passes through mobile carrier firewalls. |
| TCP | 443 | TURN over TCP — fallback when UDP is blocked. |

> **Why port 443?** Mobile carriers commonly block non-standard UDP ports (like 3478) but never block 443. Using 443 for UDP TURN allows mobile clients to use UDP instead of TCP. This is critical because TCP TURN is fragile on mobile — brief network hiccups cause TCP resets which kill the TURN allocation instantly, while UDP survives them.

## Quick start

### Docker

```bash
# Edit turnserver.conf — set static-auth-secret and external-ip
docker build -t bear-turn .
docker run -d -p 443:443/udp -p 443:443/tcp bear-turn
```

## Configuration

All configuration is in `turnserver.conf` (coturn format). Key settings:

### Auth

```
use-auth-secret
static-auth-secret=your-shared-secret
```

This **must match** the `TURN_SECRET` environment variable configured in `bear-relay`. The relay mints time-windowed credentials using HMAC-SHA1 over this secret, and the TURN server validates them.

### Listening port

```
listening-port=443
alt-listening-port=0
```

Coturn listens on both UDP and TCP on the configured `listening-port`. Port 443 is used because it passes through mobile carrier firewalls, enabling UDP TURN for mobile clients.

### External IP mapping

```
external-ip=YOUR_PUBLIC_IP/YOUR_INTERNAL_IP
```

The `external-ip` must use the `EXTERNAL/INTERNAL` format. This maps the public-facing IP to the server's internal IP. **This is critical:** without the `/INTERNAL` part, coturn blocks `CREATE_PERMISSION` for its own external IP, preventing relay↔relay on the same server (both peers relaying through the same TURN server).

In Kubernetes, the init container patches this with the LoadBalancer IP and the pod IP.

### TCP resilience

```
channel-lifetime=1200
permission-lifetime=600
```

Longer lifetimes (20 min channels, 10 min permissions vs defaults of 10 min / 5 min) improve TCP TURN resilience on mobile. If a TCP connection briefly drops and reconnects, the channel binding and permissions are more likely to still be valid.

### Peer IP access

```
allow-loopback-peers
no-multicast-peers
allowed-peer-ip=0.0.0.0-255.255.255.255
no-dynamic-ip-list
```

In GKE, kube-proxy SNAT makes client addresses appear as internal IPs (10.128.x.x), which coturn blocks by default. These settings allow all peer IPs.

## Credential flow

```
bear-relay                  coturn
(mints creds)            (validates creds)
     │                        │
     │    TURN_SECRET (shared) │
     └────────────────────────┘

1. bear-relay generates credentials:
   username  = <expiry_unix_timestamp>
   credential = base64(HMAC-SHA1(TURN_SECRET, username))

2. Credentials are served to:
   - bear-server (via GET offer response)
   - browser/bear.js (via GET /api/signal/turn-credentials)

3. Clients use credentials in ICE configuration
4. coturn validates using the same shared secret
```

Default credential TTL: 24 hours (configurable via `TURN_CREDENTIAL_TTL` in bear-relay).

## Deployment with bear-relay

Both services share the same `TURN_SECRET`. Example setup:

```
bear-relay:  TURN_SECRET=mysecret  TURN_URLS=turn:TURN_LB_IP:443,turn:TURN_TCP_LB_IP:443?transport=tcp
bear-turn:   static-auth-secret=mysecret  (in turnserver.conf)
```

See `bear-relay/README.md` for relay-side TURN configuration.

## Kubernetes deployment

The GKE deployment uses:
- **ConfigMap** (`bear-turn-config`) — coturn config template with placeholders
- **Init container** — patches `PLACEHOLDER` (secret), `TURN_UDP_EXTERNAL_IP` (LB IP), and `TURN_POD_IP` (from Downward API) into the config
- **Two LoadBalancer Services:**
  - `bear-turn-udp` — UDP 443 (`externalTrafficPolicy: Cluster`)
  - `bear-turn-tcp` — TCP 443 (`externalTrafficPolicy: Local` to avoid SNAT conntrack expiry)

See `bear-site/gke/turn/` for the full Kubernetes manifests.

## Known limitations

- **No TURNS (TLS) support** — not currently needed since we use plain TCP on port 443 as the fallback transport. Can add DTLS/TLS if required in the future.
- **`webrtc-rs` does not support TURN over TCP** — `bear-server` automatically filters out `?transport=tcp` TURN URLs and uses only UDP TURN. The browser client (Safari/Chrome) fully supports TURN TCP.

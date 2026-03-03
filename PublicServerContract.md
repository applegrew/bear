# Public Server Contract

The public server is an **external dependency** not built in this repository. It sits between the browser (`bear.js`) and the relay, providing user authentication, signaling proxy, and serving the browser client.

## Architecture overview

```
                         Public Internet
                              │
Browser ◄──login──► Public Server ◄──HTTPS──► bear-server
  (bear.js)        (session auth +            (user's machine)
                    signaling proxy)
                        │                          │
                   internal net              HTTPS (JWT-gated)
                        │                          │
                        └─────► Relay ◄──────────┘
                              (Docker)
                           SQLite + HTTP
                           mailbox
```

The public server communicates with the relay via its **internal API** (default port `8081`, no auth). It never needs the relay JWT — that is only used between `bear-server` and the relay's external API.

## Responsibilities

### 1. User authentication

- Implement user accounts, login, and session management.
- Protect all relay-facing endpoints behind session authentication (e.g. session cookies).
- The browser client (`bear.js`) sends `credentials: 'same-origin'` on all signaling requests.

### 2. Invite code management

- Generate invite codes for users who want to pair a `bear-server` with the relay.
- SHA-256 hash each code and push the hashes to the relay:

```
POST <relay_internal>/internal/invites
Content-Type: application/json

{ "codes": ["<sha256-hex-hash>", ...] }
```

- Each code has a **10-minute TTL** on the relay and is burned (deleted) on first use.
- When pairing succeeds, the relay stores the invite code hash on the room as `invite_code_hash`. This lets the public server look up which rooms belong to which user.
- Display the plaintext invite code to the user; it is never sent to the relay.

### 3. Signaling proxy

The public server proxies WebRTC signaling between the browser and the relay's internal API. This is the **recommended signaling path** — the browser never talks to the relay directly during offer/answer exchange.

#### Offer (browser → relay)

```
Browser:   POST <public_server>/relay/<room_id>/offer
           { "sdp": "...", "offer_hash_enc": "..." }

Public Server → POST <relay_internal>/internal/room/<room_id>/offer
                { "sdp": "...", "offer_hash_enc": "..." }

Response:  { "conn_id": "<uuid>" }
```

#### Answer (relay → browser)

```
Browser:   GET <public_server>/relay/<room_id>/answer/<conn_id>

Public Server → GET <relay_internal>/internal/room/<room_id>/answer/<conn_id>

Response:  204 (pending) or 200:
           { "sdp": "...", "client_jwt": "...", "offer_hash": "...", "signature": "..." }
```

**Critical:** All metadata fields (`offer_hash_enc`, `offer_hash`, `signature`, `client_jwt`) must be passed through **unchanged**. Do not strip, rename, or transform any fields in either direction.

### 4. Expose room public key to browser

Fetch the room's RSA public key from the relay and inject it as a JavaScript global so `bear.js` can verify answer signatures:

```
GET <relay_internal>/internal/room/<room_id>
→ { "room_id": "...", "signing_key": "<RSA public key PEM>", ... }
```

Inject `signing_key` as `BEAR_ROOM_KEY` in the page serving `bear.js`.

### 5. Serve `bear.js` with injected globals

When serving the browser client, inject the following JavaScript globals before `bear.js` loads:

| Global | Type | Description |
|---|---|---|
| `BEAR_RELAY_URL` | `string` | Relay external API URL (e.g. `https://relay.example.com`). Used by `bear.js` only as a configuration presence check — `bear.js` never contacts the relay directly; all traffic goes through the public server. |
| `BEAR_ROOM_ID` | `string` | Room UUID for this user's paired `bear-server` |
| `BEAR_PUBLIC_URL` | `string` | Public server origin for signaling proxy (empty string if same-origin) |
| `BEAR_ROOM_KEY` | `string` | RSA public key PEM (`signing_key` from relay) for signature verification |

Example injection:

```html
<script>
  const BEAR_RELAY_URL = "https://relay.example.com";
  const BEAR_ROOM_ID = "550e8400-e29b-41d4-a716-446655440000";
  const BEAR_PUBLIC_URL = "";
  const BEAR_ROOM_KEY = `-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhki...
-----END PUBLIC KEY-----`;
</script>
<script src="/bear.js"></script>
```

`bear.js` requires `BEAR_RELAY_URL` and `BEAR_ROOM_ID` to be set (used as a boot guard). All signaling traffic (offer, answer, ICE) flows exclusively through the public server — `bear.js` never communicates with the relay directly.

### 6. Map rooms to users

After a `bear-server` pairs with the relay, the room retains the `invite_code_hash` from the invite code used during pairing. The public server knows which invite codes it issued to which user, so it can use this to identify room ownership:

1. List rooms via `GET /internal/rooms` or fetch a specific room via `GET /internal/room/:room_id`
2. Match the `invite_code_hash` field to the user who was issued that invite code
3. Optionally clear the hash after recording the mapping:

```
PATCH <relay_internal>/internal/room/<room_id>
Content-Type: application/json

{ "invite_code_hash": null }
```

The `PATCH` endpoint accepts a JSON body with updatable fields. Currently the only supported field is `invite_code_hash`.

### 7. Pairing status and management UI

- Show the user whether their `bear-server` is currently paired (room exists on relay).
- Provide UI for:
  - Generating new invite codes
  - Viewing pairing status
  - Revoking a pairing (optional — users can also revoke via `bear --relay-revoke`)

## Relay internal API endpoints used

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/internal/invites` | Push invite code hashes |
| `GET` | `/internal/invites` | List active invite codes |
| `GET` | `/internal/rooms` | List all rooms (with pagination) |
| `GET` | `/internal/room/:room_id` | Get room details (including `signing_key`, `invite_code_hash`) |
| `PATCH` | `/internal/room/:room_id` | Update room fields (e.g. `{ "invite_code_hash": null }`) |
| `DELETE` | `/internal/room/:room_id` | Revoke a room (admin) |
| `POST` | `/internal/room/:room_id/offer` | Proxy browser SDP offer |
| `GET` | `/internal/room/:room_id/answer/:conn_id` | Proxy answer poll |

All internal endpoints run on the relay's `INTERNAL_PORT` (default `8081`) and require **no authentication**. The internal port must only be accessible from the public server's network — never exposed to the internet.

### Relay external API endpoints used (for ICE proxy)

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/room/:room_id/ice/:conn_id/client` | Proxy browser ICE candidates to relay |
| `GET` | `/room/:room_id/ice/:conn_id/server` | Proxy server ICE candidates to browser |

These run on the relay's external port (default `8080`) and require `Authorization: Bearer <client_jwt>`. The public server must attach the `client_jwt` obtained from the answer response when proxying these requests.

## Signaling integrity

The bear ecosystem uses cryptographic signaling integrity to prevent the relay from tampering with SDP offers and answers. The public server's role is purely to **pass through** these fields without modification.

### Fields in the offer (browser → relay)

| Field | Type | Description |
|---|---|---|
| `sdp` | `string` | SDP offer (plaintext) |
| `offer_hash_enc` | `string` (base64url) | SHA-256 hash of the SDP, RSA-OAEP encrypted with the room public key. Only `bear-server` can decrypt this to verify the offer wasn't tampered with. |

### Fields in the answer (relay → browser)

| Field | Type | Description |
|---|---|---|
| `sdp` | `string` | SDP answer (plaintext) |
| `client_jwt` | `string` | Short-lived JWT (5 min) minted by `bear-server`; used by the public server when proxying ICE requests to the relay external API |
| `offer_hash` | `string` (hex) | SHA-256 hash of the offer SDP as received by `bear-server` |
| `signature` | `string` (base64url) | RSA-PKCS1v15-SHA256 signature over `offer_hash + ":" + answer_sdp`, signed by `bear-server`'s private key |

The browser verifies:
1. `offer_hash` matches the SHA-256 it computed locally before sending the offer
2. `signature` is valid over `offer_hash:sdp` using `BEAR_ROOM_KEY`

If either check fails, the browser aborts the connection.

## ICE candidate exchange

After signaling completes, the browser exchanges ICE candidates through the **public server**, which proxies requests to the relay's external API using the `client_jwt` received in the answer. The browser never communicates with the relay directly.

The public server must store the `client_jwt` from the answer response (per `conn_id`) and attach it as a `Bearer` token when proxying ICE requests to the relay.

#### POST client ICE candidates (browser → relay)

```
Browser:   POST <public_server>/relay/<room_id>/ice/<conn_id>/client
           { "candidates": [{ "candidate": "...", "sdpMid": "...", "sdpMLineIndex": 0 }] }

Public Server → POST <relay_external>/room/<room_id>/ice/<conn_id>/client
                Authorization: Bearer <client_jwt>
                { "candidates": [...] }

Response:  { "ok": true }
```

#### GET server ICE candidates (relay → browser)

```
Browser:   GET <public_server>/relay/<room_id>/ice/<conn_id>/server

Public Server → GET <relay_external>/room/<room_id>/ice/<conn_id>/server
                Authorization: Bearer <client_jwt>

Response:  { "candidates": ["candidate:..."] }
```

**Note:** ICE requests go to the relay's **external** API (JWT-gated), not the internal API. The public server must forward the `client_jwt` as the `Authorization` header.

## Room ownership model

- A single `bear-server` can be paired to only **one room** at a time.
- A user on the public server can own **multiple rooms** (multiple paired `bear-server` instances).
- `bear-server` enforces a configurable `max_clients` limit (default 10). When at capacity, new relay offers are rejected.

## Security notes

- The public server should use HTTPS in production.
- Session cookies should have `Secure`, `HttpOnly`, and `SameSite` attributes.
- The relay internal port must **never** be exposed to the public internet.
- The public server should not log or store SDP content, `client_jwt`, or signaling integrity fields beyond transient proxying.
- Invite codes should be treated as secrets and displayed only to the authenticated user who generated them.

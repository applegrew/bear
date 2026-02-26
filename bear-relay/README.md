# bear-relay

Relay signaling server for Bear (Deno + SQLite).

## Technical design

`bear-relay` is a signaling mailbox for WebRTC setup between browser clients and `bear-server`.
It does **not** proxy session traffic after connection establishment.

### Responsibilities

- Persist room credentials (`room_id`, RSA public key PEM) and invite code hashes in SQLite.
- Accept pairing requests from `bear-server` using invite codes.
- Store short-lived SDP/ICE signaling messages in memory.
- Expose two HTTP surfaces:
  - **External API** (`PORT`, default `8080`) for browser + `bear-server` (JWT-gated per room).
  - **Internal API** (`INTERNAL_PORT`, default `8081`) for trusted control-plane/admin usage.

### Runtime architecture

Two listeners run in one process:

- `Deno.serve({ port: PORT }, handleExternal)`
- `Deno.serve({ port: INTERNAL_PORT }, handleInternal)`

Signaling state is in-memory only:

- `offers: Map<room_id, [{ conn_id, sdp, created_at }]>`
- `answers: Map<conn_id, { sdp, created_at }>`
- `ice: Map<"conn_id:side", [{ candidate, created_at }]>`

This state is TTL-pruned every 10s (`SIGNALING_TTL_MS = 60_000`).

### Persistence model (SQLite)

Database path: `DB_PATH` (default `/data/relay.db`), WAL mode enabled.

Tables:

1. `rooms`
   - `room_id TEXT PRIMARY KEY`
   - `signing_key TEXT NOT NULL` — RSA public key in SPKI PEM format
   - `created_at INTEGER NOT NULL`
   - `last_poll INTEGER`
2. `invite_codes`
   - `code_hash TEXT PRIMARY KEY` — SHA-256 hex hash of the invite code
   - `created_at INTEGER NOT NULL`
   - `expires_at INTEGER NOT NULL` — Unix timestamp; code is invalid after this time

### Auth and security model

- External room routes require `Authorization: Bearer <jwt>`.
- JWT verification (RS256):
  1. look up RSA public key PEM from `rooms` by `room_id`
  2. import as SPKI key, verify RS256 signature via `crypto.subtle`
  3. require token claim `room_id` to match route room
- Per-IP auth-failure rate limiting:
  - window: 60s
  - max failures: 5
  - then `429 rate limited`
- **Invite code security:**
  - codes are stored as SHA-256 hashes (plaintext never reaches the relay)
  - each code has a 10-minute TTL (`expires_at`)
  - codes are burned on use (deleted from the DB in the pairing transaction)

### Pairing and signaling flows

#### Pairing (`POST /pair`)

Request body:

```json
{ "room_id": "<uuid>", "signing_key": "<RSA public key PEM>", "invite_code": "<SHA-256 hex hash>" }
```

Flow:

1. Validate `invite_code` hash exists in `invite_codes` and `expires_at > now`.
2. Transaction:
   - burn (delete) the invite code row
   - insert/replace room with provided `room_id` and public key PEM
3. Return `{ "ok": true }`.

#### Offer/answer/ICE

- Browser posts offer: `POST /room/:room_id/offer` -> `{ conn_id }`
- `bear-server` polls: `GET /room/:room_id/offer`
  - `200` with oldest pending offer
  - `204` if none pending
- `bear-server` posts answer: `POST /room/:room_id/answer/:conn_id`
- Browser polls answer: `GET /room/:room_id/answer/:conn_id`
  - `200` with SDP or `204` if not ready
- Both sides exchange ICE via:
  - `POST /room/:room_id/ice/:conn_id/:side`
  - `GET /room/:room_id/ice/:conn_id/:side`

ICE candidates are consumed on read (`GET` clears returned candidates).

### Background maintenance

- Signaling TTL cleanup every 10 seconds.
- Expired invite code cleanup every 60 seconds (deletes rows where `expires_at < now`).
- Room pruning every hour:
  - delete rooms with `last_poll` older than 30 days.

## API summary

### External API (JWT-gated)

- `POST /pair`
- `DELETE /room/:room_id`
- `POST /room/:room_id/offer`
- `GET /room/:room_id/offer`
- `POST /room/:room_id/answer/:conn_id`
- `GET /room/:room_id/answer/:conn_id`
- `POST /room/:room_id/ice/:conn_id/:side`
- `GET /room/:room_id/ice/:conn_id/:side`

### Internal API (trusted network only)

- `GET /internal/rooms`
- `GET /internal/room/:room_id`
- `DELETE /internal/room/:room_id`
- `POST /internal/invites`
  - accepts `{ "codes": ["<sha256-hex-hash>", ...] }`
  - each code is stored with a 10-minute TTL
- `GET /internal/invites`
  - returns `[{ code_hash, created_at, expires_at }, ...]`

## Podman build

```bash
# From repo root:
podman build -t bear-relay:latest -f bear-relay/Dockerfile bear-relay
```

## Podman run

```bash
podman run --rm \
  -p 8090:8080 \
  -p 8091:8081 \
  -v /path/on/host:/data \
  -e PORT=8080 \
  -e INTERNAL_PORT=8081 \
  -e DB_PATH=/data/relay.db \
  localhost/bear-relay:latest
```

## Operational notes

- Keep `INTERNAL_PORT` inaccessible from the public internet.
- Mount persistent storage to `/data` to retain rooms/invites across restarts.

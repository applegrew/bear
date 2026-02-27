// bear-relay — Deno signaling relay with SQLite persistence
// Two HTTP listeners: external (JWT-gated) and internal (no auth)

import { DB } from "https://deno.land/x/sqlite@v3.9.1/mod.ts";
// djwt removed — RS256 verification uses native crypto.subtle

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const PORT = parseInt(Deno.env.get("PORT") ?? "8080");
const INTERNAL_PORT = parseInt(Deno.env.get("INTERNAL_PORT") ?? "8081");
const DB_PATH = Deno.env.get("DB_PATH") ?? "/data/relay.db";
const SIGNALING_TTL_MS = 60_000; // 60s for offers/answers/ICE
const ROOM_PRUNE_DAYS = 30;
const RATE_LIMIT_WINDOW_MS = 60_000;
const RATE_LIMIT_MAX_FAILURES = 5;

// ---------------------------------------------------------------------------
// SQLite setup
// ---------------------------------------------------------------------------

const db = new DB(DB_PATH);
db.execute("PRAGMA journal_mode=WAL");
db.execute(`
  CREATE TABLE IF NOT EXISTS rooms (
    room_id          TEXT PRIMARY KEY,
    signing_key      TEXT NOT NULL,
    created_at       INTEGER NOT NULL,
    last_poll        INTEGER,
    invite_code_hash TEXT
  )
`);
// Migration: add invite_code_hash column if missing (existing DBs)
try {
  db.execute("ALTER TABLE rooms ADD COLUMN invite_code_hash TEXT");
} catch { /* column already exists */ }
db.execute(`
  CREATE TABLE IF NOT EXISTS invite_codes (
    code_hash   TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
  )
`);

// ---------------------------------------------------------------------------
// In-memory signaling store (ephemeral, TTL-based)
// ---------------------------------------------------------------------------

// offers: Map<room_id, Array<{ conn_id, sdp, created_at }>>
const offers = new Map();
// answers: Map<conn_id, { sdp, created_at }>
const answers = new Map();
// ice: Map<`${conn_id}:${side}`, Array<{ candidate, created_at }>>
const ice = new Map();

let connIdCounter = 0;
function nextConnId() {
  return `conn_${++connIdCounter}_${Date.now().toString(36)}`;
}

// Periodic cleanup of expired signaling data
setInterval(() => {
  const now = Date.now();
  for (const [roomId, roomOffers] of offers) {
    const filtered = roomOffers.filter((o) => now - o.created_at < SIGNALING_TTL_MS);
    if (filtered.length === 0) offers.delete(roomId);
    else offers.set(roomId, filtered);
  }
  for (const [connId, ans] of answers) {
    if (now - ans.created_at >= SIGNALING_TTL_MS) answers.delete(connId);
  }
  for (const [key, candidates] of ice) {
    const filtered = candidates.filter((c) => now - c.created_at < SIGNALING_TTL_MS);
    if (filtered.length === 0) ice.delete(key);
    else ice.set(key, filtered);
  }
}, 10_000);

// Periodic cleanup of expired invite codes
setInterval(() => {
  const now = Math.floor(Date.now() / 1000);
  db.query("DELETE FROM invite_codes WHERE expires_at < ?", [now]);
}, 60_000);

// Periodic room pruning (30 days no poll)
setInterval(() => {
  const cutoff = Math.floor(Date.now() / 1000) - ROOM_PRUNE_DAYS * 86400;
  db.query("DELETE FROM rooms WHERE last_poll IS NOT NULL AND last_poll < ?", [cutoff]);
}, 3600_000); // every hour

// ---------------------------------------------------------------------------
// Rate limiting (per-IP auth failures)
// ---------------------------------------------------------------------------

// Map<ip, Array<timestamp>>
const authFailures = new Map();

function checkRateLimit(ip) {
  const now = Date.now();
  const failures = (authFailures.get(ip) ?? []).filter(
    (t) => now - t < RATE_LIMIT_WINDOW_MS
  );
  authFailures.set(ip, failures);
  return failures.length < RATE_LIMIT_MAX_FAILURES;
}

function recordAuthFailure(ip) {
  const failures = authFailures.get(ip) ?? [];
  failures.push(Date.now());
  authFailures.set(ip, failures);
}

// ---------------------------------------------------------------------------
// JWT helpers
// ---------------------------------------------------------------------------

async function importPublicKey(publicKeyPem) {
  const pemBody = publicKeyPem
    .replace(/-----BEGIN PUBLIC KEY-----/, "")
    .replace(/-----END PUBLIC KEY-----/, "")
    .replace(/\s/g, "");
  const der = Uint8Array.from(atob(pemBody), (c) => c.charCodeAt(0));
  return crypto.subtle.importKey(
    "spki",
    der,
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["verify"]
  );
}

function base64UrlDecode(str) {
  const padded = str.replace(/-/g, "+").replace(/_/g, "/");
  return Uint8Array.from(atob(padded), (c) => c.charCodeAt(0));
}

async function verifyJwt(authHeader, roomId) {
  if (!authHeader || !authHeader.startsWith("Bearer ")) return null;
  const token = authHeader.slice(7);
  const parts = token.split(".");
  if (parts.length !== 3) return null;

  // Look up room
  const rows = db.query("SELECT signing_key FROM rooms WHERE room_id = ?", [roomId]);
  if (rows.length === 0) return null;
  const publicKeyPem = rows[0][0];

  try {
    const key = await importPublicKey(publicKeyPem);
    const data = new TextEncoder().encode(`${parts[0]}.${parts[1]}`);
    const sig = base64UrlDecode(parts[2]);
    const valid = await crypto.subtle.verify("RSASSA-PKCS1-v1_5", key, sig, data);
    if (!valid) return null;
    const payload = JSON.parse(new TextDecoder().decode(base64UrlDecode(parts[1])));
    if (payload.room_id !== roomId) return null;
    // Reject expired tokens
    if (payload.exp && Math.floor(Date.now() / 1000) > payload.exp) return null;
    return payload;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

function json(data, status = 200) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function text(msg, status = 200) {
  return new Response(msg, { status });
}

function getIp(req, connInfo) {
  return req.headers.get("x-forwarded-for")?.split(",")[0]?.trim() ??
    connInfo?.remoteAddr?.hostname ?? "unknown";
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

function matchRoute(method, pathname) {
  // POST /pair
  if (method === "POST" && pathname === "/pair") return { handler: "pair" };

  // Room routes: /room/:room_id/...
  const roomMatch = pathname.match(/^\/room\/([^/]+)(\/.*)?$/);
  if (roomMatch) {
    const roomId = roomMatch[1];
    const rest = roomMatch[2] ?? "";

    if (method === "DELETE" && rest === "") return { handler: "revoke", roomId };
    if (method === "POST" && rest === "/offer") return { handler: "postOffer", roomId };
    if (method === "GET" && rest === "/offer") return { handler: "getOffer", roomId };

    const answerMatch = rest.match(/^\/answer\/([^/]+)$/);
    if (answerMatch) {
      const connId = answerMatch[1];
      if (method === "POST") return { handler: "postAnswer", roomId, connId };
      if (method === "GET") return { handler: "getAnswer", roomId, connId };
    }

    const iceMatch = rest.match(/^\/ice\/([^/]+)\/(server|client)$/);
    if (iceMatch) {
      const connId = iceMatch[1];
      const side = iceMatch[2];
      if (method === "POST") return { handler: "postIce", roomId, connId, side };
      if (method === "GET") return { handler: "getIce", roomId, connId, side };
    }
  }

  return null;
}

function matchInternalRoute(method, pathname) {
  if (method === "GET" && pathname === "/internal/rooms") return { handler: "listRooms" };
  if (method === "GET" && pathname === "/internal/invites") return { handler: "listInvites" };
  if (method === "POST" && pathname === "/internal/invites") return { handler: "pushInvites" };

  const roomMatch = pathname.match(/^\/internal\/room\/([^/]+)(\/.*)?$/);
  if (roomMatch) {
    const roomId = roomMatch[1];
    const rest = roomMatch[2] ?? "";
    if (method === "GET" && rest === "") return { handler: "getRoom", roomId };
    if (method === "PATCH" && rest === "") return { handler: "updateRoom", roomId };
    if (method === "DELETE" && rest === "") return { handler: "deleteRoom", roomId };
    if (method === "POST" && rest === "/offer") return { handler: "internalPostOffer", roomId };

    const answerMatch = rest.match(/^\/answer\/([^/]+)$/);
    if (answerMatch) {
      const connId = answerMatch[1];
      if (method === "GET") return { handler: "internalGetAnswer", roomId, connId };
    }
  }

  return null;
}

// ---------------------------------------------------------------------------
// External route handlers
// ---------------------------------------------------------------------------

async function handlePair(req) {
  let body;
  try {
    body = await req.json();
  } catch {
    return text("invalid JSON", 400);
  }

  const { room_id, signing_key, invite_code } = body;
  if (!room_id || !signing_key || !invite_code) {
    return text("missing required fields: room_id, signing_key, invite_code", 400);
  }

  // Validate invite code (must exist and not expired)
  const now = Math.floor(Date.now() / 1000);
  const rows = db.query(
    "SELECT 1 FROM invite_codes WHERE code_hash = ? AND expires_at > ?",
    [invite_code, now]
  );
  if (rows.length === 0) return json({ error: "invalid or expired invite code" }, 403);

  // Burn invite code and create room, keeping the code hash on the room
  db.execute("BEGIN");
  try {
    db.query("DELETE FROM invite_codes WHERE code_hash = ?", [invite_code]);
    db.query(
      "INSERT OR REPLACE INTO rooms (room_id, signing_key, created_at, invite_code_hash) VALUES (?, ?, ?, ?)",
      [room_id, signing_key, now, invite_code]
    );
    db.execute("COMMIT");
  } catch (e) {
    db.execute("ROLLBACK");
    return json({ error: "pairing failed: " + e.message }, 500);
  }

  return json({ ok: true });
}

async function handleRevoke(req, roomId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    return text("unauthorized", 401);
  }

  db.query("DELETE FROM rooms WHERE room_id = ?", [roomId]);
  // Clean up in-memory signaling data for this room
  offers.delete(roomId);
  return json({ ok: true });
}

async function handlePostOffer(req, roomId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    // Check if room exists to distinguish 401 vs 404
    const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  let body;
  try {
    body = await req.json();
  } catch {
    return text("invalid JSON", 400);
  }

  const connId = nextConnId();
  const roomOffers = offers.get(roomId) ?? [];
  roomOffers.push({ conn_id: connId, sdp: body.sdp, created_at: Date.now() });
  offers.set(roomId, roomOffers);

  return json({ conn_id: connId });
}

async function handleGetOffer(req, roomId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  // Update last_poll
  db.query("UPDATE rooms SET last_poll = ? WHERE room_id = ?", [
    Math.floor(Date.now() / 1000),
    roomId,
  ]);

  const roomOffers = offers.get(roomId) ?? [];
  // Return oldest pending offer (FIFO)
  const now = Date.now();
  const validOffers = roomOffers.filter((o) => now - o.created_at < SIGNALING_TTL_MS);
  offers.set(roomId, validOffers);

  if (validOffers.length === 0) {
    return new Response(null, {
      status: 204,
      headers: { "X-Next-Poll": String(Math.floor(Date.now() / 1000) + 5) },
    });
  }

  const offer = validOffers.shift();
  offers.set(roomId, validOffers);

  return new Response(JSON.stringify({ conn_id: offer.conn_id, sdp: offer.sdp }), {
    status: 200,
    headers: {
      "Content-Type": "application/json",
      "X-Next-Poll": String(Math.floor(Date.now() / 1000) + 5),
    },
  });
}

async function handlePostAnswer(req, roomId, connId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  let body;
  try {
    body = await req.json();
  } catch {
    return text("invalid JSON", 400);
  }

  answers.set(connId, { sdp: body.sdp, client_jwt: body.client_jwt || null, created_at: Date.now() });
  return json({ ok: true });
}

async function handleGetAnswer(req, roomId, connId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  const ans = answers.get(connId);
  if (!ans || Date.now() - ans.created_at >= SIGNALING_TTL_MS) {
    return new Response(null, { status: 204 });
  }

  answers.delete(connId);
  const result = { sdp: ans.sdp };
  if (ans.client_jwt) result.client_jwt = ans.client_jwt;
  return json(result);
}

async function handlePostIce(req, roomId, connId, side, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  let body;
  try {
    body = await req.json();
  } catch {
    return text("invalid JSON", 400);
  }

  const key = `${connId}:${side}`;
  const candidates = ice.get(key) ?? [];
  const newCandidates = Array.isArray(body.candidates) ? body.candidates : [body.candidate];
  for (const c of newCandidates) {
    if (c) candidates.push({ candidate: c, created_at: Date.now() });
  }
  ice.set(key, candidates);

  return json({ ok: true });
}

async function handleGetIce(req, roomId, connId, side, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  const key = `${connId}:${side}`;
  const candidates = (ice.get(key) ?? []).filter(
    (c) => Date.now() - c.created_at < SIGNALING_TTL_MS
  );
  ice.delete(key); // consume on read

  return json({ candidates: candidates.map((c) => c.candidate) });
}

// ---------------------------------------------------------------------------
// Internal route handlers (no auth)
// ---------------------------------------------------------------------------

function handleListRooms(url) {
  const limit = parseInt(url.searchParams.get("limit") ?? "100");
  const offset = parseInt(url.searchParams.get("offset") ?? "0");
  const rows = db.query(
    "SELECT room_id, created_at, last_poll, invite_code_hash FROM rooms ORDER BY created_at DESC LIMIT ? OFFSET ?",
    [limit, offset]
  );
  const rooms = rows.map(([room_id, created_at, last_poll, invite_code_hash]) => ({
    room_id,
    created_at,
    last_poll,
    invite_code_hash: invite_code_hash ?? null,
  }));
  return json(rooms);
}

function handleGetRoom(roomId) {
  const rows = db.query(
    "SELECT room_id, signing_key, created_at, last_poll, invite_code_hash FROM rooms WHERE room_id = ?",
    [roomId]
  );
  if (rows.length === 0) return text("not found", 404);
  const [rid, signing_key, created_at, last_poll, invite_code_hash] = rows[0];
  return json({ room_id: rid, signing_key, created_at, last_poll, invite_code_hash: invite_code_hash ?? null });
}

async function handleUpdateRoom(req, roomId) {
  const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
  if (rows.length === 0) return text("not found", 404);

  let body;
  try {
    body = await req.json();
  } catch {
    return text("invalid JSON", 400);
  }

  // Build SET clause from allowed fields
  const allowed = ["invite_code_hash"];
  const setClauses = [];
  const params = [];
  for (const field of allowed) {
    if (field in body) {
      setClauses.push(`${field} = ?`);
      params.push(body[field] ?? null);
    }
  }
  if (setClauses.length === 0) return text("no updatable fields provided", 400);

  params.push(roomId);
  db.query(`UPDATE rooms SET ${setClauses.join(", ")} WHERE room_id = ?`, params);
  return json({ ok: true });
}

function handleDeleteRoom(roomId) {
  db.query("DELETE FROM rooms WHERE room_id = ?", [roomId]);
  offers.delete(roomId);
  return json({ ok: true });
}

async function handlePushInvites(req) {
  let body;
  try {
    body = await req.json();
  } catch {
    return text("invalid JSON", 400);
  }

  const codes = body.codes;
  if (!Array.isArray(codes) || codes.length === 0) {
    return text("missing or empty codes array", 400);
  }

  const now = Math.floor(Date.now() / 1000);
  const expires_at = now + 600; // 10 minutes TTL
  db.execute("BEGIN");
  try {
    for (const code of codes) {
      db.query(
        "INSERT OR IGNORE INTO invite_codes (code_hash, created_at, expires_at) VALUES (?, ?, ?)",
        [String(code), now, expires_at]
      );
    }
    db.execute("COMMIT");
  } catch (e) {
    db.execute("ROLLBACK");
    return json({ error: e.message }, 500);
  }

  return json({ ok: true, count: codes.length });
}

function handleListInvites(url) {
  const limit = parseInt(url.searchParams.get("limit") ?? "100");
  const offset = parseInt(url.searchParams.get("offset") ?? "0");
  const rows = db.query(
    "SELECT code_hash, created_at, expires_at FROM invite_codes ORDER BY created_at DESC LIMIT ? OFFSET ?",
    [limit, offset]
  );
  const invites = rows.map(([code_hash, created_at, expires_at]) => ({
    code_hash,
    created_at,
    expires_at,
  }));
  return json(invites);
}

// ---------------------------------------------------------------------------
// External server
// ---------------------------------------------------------------------------

async function handleExternal(req, connInfo) {
  const url = new URL(req.url);
  const ip = getIp(req, connInfo);
  const route = matchRoute(req.method, url.pathname);

  if (!route) return text("not found", 404);

  switch (route.handler) {
    case "pair":
      return handlePair(req);
    case "revoke":
      return handleRevoke(req, route.roomId, ip);
    case "postOffer":
      return handlePostOffer(req, route.roomId, ip);
    case "getOffer":
      return handleGetOffer(req, route.roomId, ip);
    case "postAnswer":
      return handlePostAnswer(req, route.roomId, route.connId, ip);
    case "getAnswer":
      return handleGetAnswer(req, route.roomId, route.connId, ip);
    case "postIce":
      return handlePostIce(req, route.roomId, route.connId, route.side, ip);
    case "getIce":
      return handleGetIce(req, route.roomId, route.connId, route.side, ip);
    default:
      return text("not found", 404);
  }
}

// ---------------------------------------------------------------------------
// Internal server
// ---------------------------------------------------------------------------

async function handleInternal(req) {
  const url = new URL(req.url);
  const route = matchInternalRoute(req.method, url.pathname);

  if (!route) return text("not found", 404);

  switch (route.handler) {
    case "listRooms":
      return handleListRooms(url);
    case "getRoom":
      return handleGetRoom(route.roomId);
    case "updateRoom":
      return handleUpdateRoom(req, route.roomId);
    case "deleteRoom":
      return handleDeleteRoom(route.roomId);
    case "pushInvites":
      return handlePushInvites(req);
    case "listInvites":
      return handleListInvites(url);
    case "internalPostOffer":
      return handleInternalPostOffer(req, route.roomId);
    case "internalGetAnswer":
      return handleInternalGetAnswer(route.roomId, route.connId);
    default:
      return text("not found", 404);
  }
}

async function handleInternalPostOffer(req, roomId) {
  // Verify room exists
  const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
  if (rows.length === 0) return text("not found", 404);

  let body;
  try {
    body = await req.json();
  } catch {
    return text("invalid JSON", 400);
  }

  const connId = nextConnId();
  const roomOffers = offers.get(roomId) ?? [];
  roomOffers.push({ conn_id: connId, sdp: body.sdp, created_at: Date.now() });
  offers.set(roomId, roomOffers);

  return json({ conn_id: connId });
}

function handleInternalGetAnswer(roomId, connId) {
  // Verify room exists
  const rows = db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
  if (rows.length === 0) return text("not found", 404);

  const ans = answers.get(connId);
  if (!ans || Date.now() - ans.created_at >= SIGNALING_TTL_MS) {
    return new Response(null, { status: 204 });
  }

  answers.delete(connId);
  const result = { sdp: ans.sdp };
  if (ans.client_jwt) result.client_jwt = ans.client_jwt;
  return json(result);
}

// ---------------------------------------------------------------------------
// Start servers
// ---------------------------------------------------------------------------

console.log(`bear-relay starting...`);
console.log(`  External API: http://0.0.0.0:${PORT}`);
console.log(`  Internal API: http://0.0.0.0:${INTERNAL_PORT}`);
console.log(`  Database: ${DB_PATH}`);

Deno.serve({ port: PORT, hostname: "0.0.0.0" }, handleExternal);
Deno.serve({ port: INTERNAL_PORT, hostname: "0.0.0.0" }, handleInternal);

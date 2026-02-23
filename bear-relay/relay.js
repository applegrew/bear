// bear-relay — Deno signaling relay with SQLite persistence
// Two HTTP listeners: external (JWT-gated) and internal (no auth)

import { DB } from "https://deno.land/x/sqlite@v3.9.1/mod.ts";
import { create, verify } from "https://deno.land/x/djwt@v3.0.2/mod.ts";
import { decode } from "https://deno.land/x/djwt@v3.0.2/mod.ts";

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
    room_id     TEXT PRIMARY KEY,
    signing_key TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    last_poll   INTEGER
  )
`);
db.execute(`
  CREATE TABLE IF NOT EXISTS invite_codes (
    code        TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    used        INTEGER NOT NULL DEFAULT 0
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

async function importKey(signingKeyBase64) {
  const raw = Uint8Array.from(atob(signingKeyBase64), (c) => c.charCodeAt(0));
  return await crypto.subtle.importKey(
    "raw",
    raw,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"]
  );
}

async function verifyJwt(authHeader, roomId) {
  if (!authHeader || !authHeader.startsWith("Bearer ")) return null;
  const token = authHeader.slice(7);

  // Look up room
  const rows = db.query("SELECT signing_key FROM rooms WHERE room_id = ?", [roomId]);
  if (rows.length === 0) return null;
  const signingKeyBase64 = rows[0][0];

  try {
    const key = await importKey(signingKeyBase64);
    const payload = await verify(token, key);
    if (payload.room_id !== roomId) return null;
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

  const roomMatch = pathname.match(/^\/internal\/room\/([^/]+)$/);
  if (roomMatch) {
    const roomId = roomMatch[1];
    if (method === "GET") return { handler: "getRoom", roomId };
    if (method === "DELETE") return { handler: "deleteRoom", roomId };
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

  // Validate invite code
  const rows = db.query(
    "SELECT used FROM invite_codes WHERE code = ?",
    [invite_code]
  );
  if (rows.length === 0) return json({ error: "invalid invite code" }, 403);
  if (rows[0][0] === 1) return json({ error: "invite code already used" }, 403);

  // Consume invite code and create room (transaction)
  db.execute("BEGIN");
  try {
    db.query("UPDATE invite_codes SET used = 1 WHERE code = ?", [invite_code]);
    db.query(
      "INSERT OR REPLACE INTO rooms (room_id, signing_key, created_at) VALUES (?, ?, ?)",
      [room_id, signing_key, Math.floor(Date.now() / 1000)]
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

  answers.set(connId, { sdp: body.sdp, created_at: Date.now() });
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
  return json({ sdp: ans.sdp });
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
    "SELECT room_id, created_at, last_poll FROM rooms ORDER BY created_at DESC LIMIT ? OFFSET ?",
    [limit, offset]
  );
  const rooms = rows.map(([room_id, created_at, last_poll]) => ({
    room_id,
    created_at,
    last_poll,
  }));
  return json(rooms);
}

function handleGetRoom(roomId) {
  const rows = db.query(
    "SELECT room_id, signing_key, created_at, last_poll FROM rooms WHERE room_id = ?",
    [roomId]
  );
  if (rows.length === 0) return text("not found", 404);
  const [rid, signing_key, created_at, last_poll] = rows[0];
  return json({ room_id: rid, signing_key, created_at, last_poll });
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
  db.execute("BEGIN");
  try {
    for (const code of codes) {
      db.query(
        "INSERT OR IGNORE INTO invite_codes (code, created_at) VALUES (?, ?)",
        [String(code), now]
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
    "SELECT code, created_at, used FROM invite_codes ORDER BY created_at DESC LIMIT ? OFFSET ?",
    [limit, offset]
  );
  const invites = rows.map(([code, created_at, used]) => ({
    code,
    created_at,
    used: used === 1,
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
    case "deleteRoom":
      return handleDeleteRoom(route.roomId);
    case "pushInvites":
      return handlePushInvites(req);
    case "listInvites":
      return handleListInvites(url);
    default:
      return text("not found", 404);
  }
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

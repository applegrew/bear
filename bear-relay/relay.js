// bear-relay — Deno signaling relay with pluggable database backend
// Two HTTP listeners: external (JWT-gated) and internal (no auth)
// DB backend selected via DB_BACKEND env var: "sqlite" (default), "postgres", "mysql"

import { createDatabase } from "./db.js";
// djwt removed — RS256 verification uses native crypto.subtle

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const PORT = parseInt(Deno.env.get("PORT") ?? "8080");
const INTERNAL_PORT = parseInt(Deno.env.get("INTERNAL_PORT") ?? "8081");
const RELAY_VERSION = "0.3.1";
const SIGNALING_TTL_MS = 60_000; // 60s for offers/answers/ICE
const ROOM_PRUNE_DAYS = 30;
const RATE_LIMIT_WINDOW_MS = 60_000;
const RATE_LIMIT_MAX_FAILURES = 5;
const TRUST_PROXY = Deno.env.get("TRUST_PROXY") === "true";
const TURN_SECRET = Deno.env.get("TURN_SECRET") || "";
const TURN_URLS = (Deno.env.get("TURN_URLS") || Deno.env.get("TURN_URL") || "").split(",").map(u => u.trim()).filter(Boolean);
const TURN_REALM = Deno.env.get("TURN_REALM") || "bear";
const TURN_CREDENTIAL_TTL = parseInt(Deno.env.get("TURN_CREDENTIAL_TTL") ?? "86400"); // 24h default

// ---------------------------------------------------------------------------
// Database setup (initialized at startup)
// ---------------------------------------------------------------------------

let db;

// ---------------------------------------------------------------------------
// In-memory signaling store (ephemeral, TTL-based)
// ---------------------------------------------------------------------------

// offers: Map<room_id, Array<{ conn_id, sdp, created_at }>>
const offers = new Map();
// answers: Map<conn_id, { sdp, created_at }>
const answers = new Map();
// ice: Map<`${conn_id}:${side}`, Array<{ candidate, created_at }>>
const ice = new Map();

// ---------------------------------------------------------------------------
// TURN credential minting (RFC 5389 long-term credentials)
// ---------------------------------------------------------------------------

async function generateTurnCredentials(ttlSeconds) {
  if (!TURN_SECRET || TURN_URLS.length === 0) return null;
  const expiry = Math.floor(Date.now() / 1000) + ttlSeconds;
  const username = String(expiry);
  // HMAC-SHA1 of username with shared secret (matches turn crate LongTermAuthHandler)
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(TURN_SECRET),
    { name: "HMAC", hash: "SHA-1" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(username));
  const credential = btoa(String.fromCharCode(...new Uint8Array(sig)));
  return {
    urls: TURN_URLS,
    username,
    credential,
  };
}

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
setInterval(async () => {
  const now = Math.floor(Date.now() / 1000);
  await db.execute("DELETE FROM invite_codes WHERE expires_at < ?", [now]);
}, 60_000);

// Periodic room pruning (30 days no poll)
setInterval(async () => {
  const cutoff = Math.floor(Date.now() / 1000) - ROOM_PRUNE_DAYS * 86400;
  await db.execute("DELETE FROM rooms WHERE last_poll IS NOT NULL AND last_poll < ?", [cutoff]);
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
  const rows = await db.query("SELECT signing_key FROM rooms WHERE room_id = ?", [roomId]);
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
    // Reject tokens without exp or with expired exp
    if (!payload.exp || Math.floor(Date.now() / 1000) > payload.exp) return null;
    return payload;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

const CORS_HEADERS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type, Authorization",
  "Access-Control-Max-Age": "86400",
};

function json(data, status = 200) {
  return new Response(JSON.stringify(data, (_k, v) => typeof v === "bigint" ? Number(v) : v), {
    status,
    headers: { "Content-Type": "application/json", ...CORS_HEADERS },
  });
}

function text(msg, status = 200) {
  return new Response(msg, { status, headers: { ...CORS_HEADERS } });
}

function getIp(req, connInfo) {
  if (TRUST_PROXY) {
    const xff = req.headers.get("x-forwarded-for");
    if (xff) return xff.split(",")[0].trim();
  }
  return connInfo?.remoteAddr?.hostname ?? "unknown";
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
    if (method === "POST" && rest === "/status") return { handler: "postStatus", roomId };
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
  if (method === "GET" && pathname === "/internal/health") return { handler: "health" };
  if (method === "GET" && pathname === "/internal/turn-credentials") return { handler: "turnCredentials" };
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

    const iceMatch = rest.match(/^\/ice\/([^/]+)\/(server|client)$/);
    if (iceMatch) {
      const connId = iceMatch[1];
      const side = iceMatch[2];
      if (method === "POST") return { handler: "internalPostIce", roomId, connId, side };
      if (method === "GET") return { handler: "internalGetIce", roomId, connId, side };
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

  const { room_id, signing_key, invite_code, jwt_expires_at } = body;
  if (!room_id || !signing_key || !invite_code) {
    return text("missing required fields: room_id, signing_key, invite_code", 400);
  }

  // Validate invite code (must exist and not expired)
  const now = Math.floor(Date.now() / 1000);
  const rows = await db.query(
    "SELECT 1 FROM invite_codes WHERE code_hash = ? AND expires_at > ?",
    [invite_code, now]
  );
  if (rows.length === 0) return json({ error: "invalid or expired invite code" }, 403);

  // Burn invite code and create room, keeping the code hash on the room
  try {
    await db.transaction(async (tx) => {
      await tx.execute("DELETE FROM invite_codes WHERE code_hash = ?", [invite_code]);
      await tx.upsert(
        "rooms",
        ["room_id", "signing_key", "created_at", "invite_code_hash", "jwt_expires_at"],
        [room_id, signing_key, now, invite_code, jwt_expires_at ?? null]
      );
    });
  } catch (e) {
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

  await db.execute("DELETE FROM rooms WHERE room_id = ?", [roomId]);
  // Clean up in-memory signaling data for this room
  offers.delete(roomId);
  return json({ ok: true });
}

async function handlePostStatus(req, roomId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  let body;
  try { body = await req.json(); } catch { return text("invalid JSON", 400); }

  const serverVersion = req.headers.get("x-bear-server-version") || null;

  if (body.online === false) {
    // Server is shutting down — set last_poll to 0 to signal offline
    await db.execute("UPDATE rooms SET last_poll = 0, server_version = ? WHERE room_id = ?", [serverVersion, roomId]);
    // Clear any pending signaling data for this room
    offers.delete(roomId);
  } else {
    // Treat as a heartbeat
    await db.execute("UPDATE rooms SET last_poll = ?, server_version = ? WHERE room_id = ?", [
      Math.floor(Date.now() / 1000), serverVersion, roomId,
    ]);
  }

  return json({ ok: true });
}

async function handlePostOffer(req, roomId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    // Check if room exists to distinguish 401 vs 404
    const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
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
    const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  // Update last_poll and server_version
  const serverVersion = req.headers.get("x-bear-server-version") || null;
  await db.execute("UPDATE rooms SET last_poll = ?, server_version = ? WHERE room_id = ?", [
    Math.floor(Date.now() / 1000),
    serverVersion,
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
      headers: { "X-Next-Poll": String(Math.floor(Date.now() / 1000) + 5), ...CORS_HEADERS },
    });
  }

  const offer = validOffers.shift();
  offers.set(roomId, validOffers);

  const offerResult = { conn_id: offer.conn_id, sdp: offer.sdp };
  const turnCreds = await generateTurnCredentials(TURN_CREDENTIAL_TTL);
  if (turnCreds) offerResult.turn_servers = [turnCreds];

  return new Response(JSON.stringify(offerResult), {
    status: 200,
    headers: {
      "Content-Type": "application/json",
      "X-Next-Poll": String(Math.floor(Date.now() / 1000) + 5),
      ...CORS_HEADERS,
    },
  });
}

async function handlePostAnswer(req, roomId, connId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  let body;
  try {
    body = await req.json();
  } catch {
    return text("invalid JSON", 400);
  }

  answers.set(connId, { sdp: body.sdp, client_jwt: body.client_jwt || null, offer_hash: body.offer_hash || null, signature: body.signature || null, created_at: Date.now() });
  return json({ ok: true });
}

async function handleGetAnswer(req, roomId, connId, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  const ans = answers.get(connId);
  if (!ans || Date.now() - ans.created_at >= SIGNALING_TTL_MS) {
    return new Response(null, { status: 204, headers: { ...CORS_HEADERS } });
  }

  answers.delete(connId);
  const result = { sdp: ans.sdp };
  if (ans.client_jwt) result.client_jwt = ans.client_jwt;
  if (ans.offer_hash) result.offer_hash = ans.offer_hash;
  if (ans.signature) result.signature = ans.signature;
  const turnCreds = await generateTurnCredentials(TURN_CREDENTIAL_TTL);
  if (turnCreds) result.turn_servers = [turnCreds];
  return json(result);
}

async function handlePostIce(req, roomId, connId, side, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
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
  console.log(`[EXT POST ICE] key=${key} added=${newCandidates.length} total=${candidates.length}`);

  return json({ ok: true });
}

async function handleGetIce(req, roomId, connId, side, ip) {
  if (!checkRateLimit(ip)) return text("rate limited", 429);

  const payload = await verifyJwt(req.headers.get("authorization"), roomId);
  if (!payload) {
    recordAuthFailure(ip);
    const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
    return text(rows.length === 0 ? "not found" : "unauthorized", rows.length === 0 ? 404 : 401);
  }

  const key = `${connId}:${side}`;
  const candidates = (ice.get(key) ?? []).filter(
    (c) => Date.now() - c.created_at < SIGNALING_TTL_MS
  );
  ice.delete(key); // consume on read
  console.log(`[EXT GET ICE] key=${key} returning=${candidates.length}`);

  return json({ candidates: candidates.map((c) => c.candidate) });
}

// ---------------------------------------------------------------------------
// Internal route handlers (no auth)
// ---------------------------------------------------------------------------

function handleHealth() {
  return json({
    status: "ok",
    version: RELAY_VERSION,
    db_backend: db.name,
    uptime_seconds: Math.floor(performance.now() / 1000),
  });
}

async function handleListRooms(url) {
  const limit = parseInt(url.searchParams.get("limit") ?? "100");
  const offset = parseInt(url.searchParams.get("offset") ?? "0");
  const rows = await db.query(
    "SELECT room_id, created_at, last_poll, invite_code_hash, server_version, jwt_expires_at FROM rooms ORDER BY created_at DESC LIMIT ? OFFSET ?",
    [limit, offset]
  );
  const rooms = rows.map(([room_id, created_at, last_poll, invite_code_hash, server_version, jwt_expires_at]) => ({
    room_id,
    created_at,
    last_poll,
    invite_code_hash: invite_code_hash ?? null,
    server_version: server_version ?? null,
    jwt_expires_at: jwt_expires_at ?? null,
  }));
  return json(rooms);
}

async function handleGetRoom(roomId) {
  const rows = await db.query(
    "SELECT room_id, signing_key, created_at, last_poll, invite_code_hash, server_version, jwt_expires_at FROM rooms WHERE room_id = ?",
    [roomId]
  );
  if (rows.length === 0) return text("not found", 404);
  const [rid, signing_key, created_at, last_poll, invite_code_hash, server_version, jwt_expires_at] = rows[0];
  return json({ room_id: rid, signing_key, created_at, last_poll, invite_code_hash: invite_code_hash ?? null, server_version: server_version ?? null, jwt_expires_at: jwt_expires_at ?? null });
}

async function handleUpdateRoom(req, roomId) {
  const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
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
  await db.execute(`UPDATE rooms SET ${setClauses.join(", ")} WHERE room_id = ?`, params);
  return json({ ok: true });
}

async function handleDeleteRoom(roomId) {
  await db.execute("DELETE FROM rooms WHERE room_id = ?", [roomId]);
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
  try {
    await db.transaction(async (tx) => {
      for (const code of codes) {
        await tx.insertIgnore(
          "invite_codes",
          ["code_hash", "created_at", "expires_at"],
          [String(code), now, expires_at]
        );
      }
    });
  } catch (e) {
    return json({ error: e.message }, 500);
  }

  return json({ ok: true, count: codes.length });
}

async function handleListInvites(url) {
  const limit = parseInt(url.searchParams.get("limit") ?? "100");
  const offset = parseInt(url.searchParams.get("offset") ?? "0");
  const rows = await db.query(
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
  // Handle CORS preflight
  if (req.method === "OPTIONS") {
    return new Response(null, { status: 204, headers: CORS_HEADERS });
  }

  const url = new URL(req.url);
  const ip = getIp(req, connInfo);
  const route = matchRoute(req.method, url.pathname);

  if (!route) return text("not found", 404);

  switch (route.handler) {
    case "pair":
      if (!checkRateLimit(ip)) return text("rate limited", 429);
      return handlePair(req);
    case "revoke":
      return handleRevoke(req, route.roomId, ip);
    case "postOffer":
      return handlePostOffer(req, route.roomId, ip);
    case "postStatus":
      return handlePostStatus(req, route.roomId, ip);
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
    case "health":
      return handleHealth();
    case "turnCredentials":
      return handleTurnCredentials();
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
    case "internalPostIce":
      return handleInternalPostIce(req, route.roomId, route.connId, route.side);
    case "internalGetIce":
      return handleInternalGetIce(route.roomId, route.connId, route.side);
    default:
      return text("not found", 404);
  }
}

async function handleInternalPostOffer(req, roomId) {
  // Verify room exists
  const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
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

async function handleInternalGetAnswer(roomId, connId) {
  // Verify room exists
  const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
  if (rows.length === 0) return text("not found", 404);

  const ans = answers.get(connId);
  if (!ans || Date.now() - ans.created_at >= SIGNALING_TTL_MS) {
    return new Response(null, { status: 204 });
  }

  answers.delete(connId);
  const result = { sdp: ans.sdp };
  if (ans.client_jwt) result.client_jwt = ans.client_jwt;
  if (ans.offer_hash) result.offer_hash = ans.offer_hash;
  if (ans.signature) result.signature = ans.signature;
  return json(result);
}

async function handleTurnCredentials() {
  const turnCreds = await generateTurnCredentials(TURN_CREDENTIAL_TTL);
  return json({ turn_servers: turnCreds ? [turnCreds] : [] });
}

async function handleInternalPostIce(req, roomId, connId, side) {
  const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
  if (rows.length === 0) return text("not found", 404);

  let body;
  try { body = await req.json(); } catch { return text("invalid JSON", 400); }

  const key = `${connId}:${side}`;
  const candidates = ice.get(key) ?? [];
  const newCandidates = Array.isArray(body.candidates) ? body.candidates : [body.candidate];
  for (const c of newCandidates) {
    if (c) candidates.push({ candidate: c, created_at: Date.now() });
  }
  ice.set(key, candidates);
  console.log(`[INT POST ICE] key=${key} added=${newCandidates.length} total=${candidates.length}`);
  return json({ ok: true });
}

async function handleInternalGetIce(roomId, connId, side) {
  const rows = await db.query("SELECT 1 FROM rooms WHERE room_id = ?", [roomId]);
  if (rows.length === 0) return text("not found", 404);

  const key = `${connId}:${side}`;
  const candidates = (ice.get(key) ?? []).filter(
    (c) => Date.now() - c.created_at < SIGNALING_TTL_MS
  );
  ice.delete(key); // consume on read
  console.log(`[INT GET ICE] key=${key} returning=${candidates.length}`);
  return json({ candidates: candidates.map((c) => c.candidate) });
}

// ---------------------------------------------------------------------------
// Start servers
// ---------------------------------------------------------------------------

async function main() {
  console.log(`bear-relay v${RELAY_VERSION} starting...`);

  db = await createDatabase();

  console.log(`  External API: http://0.0.0.0:${PORT}`);
  console.log(`  Internal API: http://0.0.0.0:${INTERNAL_PORT}`);

  Deno.serve({ port: PORT, hostname: "0.0.0.0" }, handleExternal);
  Deno.serve({ port: INTERNAL_PORT, hostname: "0.0.0.0" }, handleInternal);
}

main();

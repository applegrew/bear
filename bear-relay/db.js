// bear-relay database abstraction layer
// Supports SQLite (default, local dev), PostgreSQL, and MySQL backends.
//
// Environment variables:
//   DB_BACKEND   — "sqlite" (default), "postgres", or "mysql"
//   DATABASE_URL — connection string for postgres/mysql
//                  e.g. postgres://user:pass@host:5432/dbname
//                  e.g. mysql://user:pass@host:3306/dbname
//   DB_PATH      — SQLite file path (default: /data/relay.db)

import { DB as SqliteDB } from "https://deno.land/x/sqlite@v3.9.1/mod.ts";
import { Client as PgClient } from "https://deno.land/x/postgres@v0.19.3/mod.ts";
import { Client as MysqlClient } from "https://deno.land/x/mysql@v2.12.1/mod.ts";

// ---------------------------------------------------------------------------
// Schema SQL (dialect-specific)
// ---------------------------------------------------------------------------

const ROOMS_TABLE_SQLITE = `
  CREATE TABLE IF NOT EXISTS rooms (
    room_id          TEXT PRIMARY KEY,
    signing_key      TEXT NOT NULL,
    created_at       INTEGER NOT NULL,
    last_poll        INTEGER,
    invite_code_hash TEXT,
    server_version   TEXT
  )
`;

const ROOMS_TABLE_POSTGRES = `
  CREATE TABLE IF NOT EXISTS rooms (
    room_id          TEXT PRIMARY KEY,
    signing_key      TEXT NOT NULL,
    created_at       BIGINT NOT NULL,
    last_poll        BIGINT,
    invite_code_hash TEXT,
    server_version   TEXT
  )
`;

const ROOMS_TABLE_MYSQL = `
  CREATE TABLE IF NOT EXISTS rooms (
    room_id          VARCHAR(255) PRIMARY KEY,
    signing_key      TEXT NOT NULL,
    created_at       BIGINT NOT NULL,
    last_poll        BIGINT,
    invite_code_hash VARCHAR(255),
    server_version   VARCHAR(64)
  )
`;

const INVITE_CODES_TABLE_SQLITE = `
  CREATE TABLE IF NOT EXISTS invite_codes (
    code_hash   TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
  )
`;

const INVITE_CODES_TABLE_POSTGRES = `
  CREATE TABLE IF NOT EXISTS invite_codes (
    code_hash   TEXT PRIMARY KEY,
    created_at  BIGINT NOT NULL,
    expires_at  BIGINT NOT NULL
  )
`;

const INVITE_CODES_TABLE_MYSQL = `
  CREATE TABLE IF NOT EXISTS invite_codes (
    code_hash   VARCHAR(255) PRIMARY KEY,
    created_at  BIGINT NOT NULL,
    expires_at  BIGINT NOT NULL
  )
`;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// Convert `?` placeholders to `$1, $2, ...` for Postgres
function toPgParams(sql) {
  let i = 0;
  return sql.replace(/\?/g, () => `$${++i}`);
}

// ---------------------------------------------------------------------------
// SQLite backend
// ---------------------------------------------------------------------------

class SqliteBackend {
  constructor(dbPath) {
    this._db = new SqliteDB(dbPath);
    this._db.execute("PRAGMA journal_mode=WAL");
  }

  get name() { return "sqlite"; }

  async init() {
    this._db.execute(ROOMS_TABLE_SQLITE);
    // Migration: add columns if missing (existing DBs)
    try {
      this._db.execute("ALTER TABLE rooms ADD COLUMN invite_code_hash TEXT");
    } catch { /* column already exists */ }
    try {
      this._db.execute("ALTER TABLE rooms ADD COLUMN server_version TEXT");
    } catch { /* column already exists */ }
    this._db.execute(INVITE_CODES_TABLE_SQLITE);
  }

  // Returns array of row-arrays, e.g. [[val1, val2], [val1, val2]]
  async query(sql, params = []) {
    return this._db.query(sql, params);
  }

  // Execute statement with no return value
  async execute(sql, params = []) {
    if (params.length > 0) {
      this._db.query(sql, params);
    } else {
      this._db.execute(sql);
    }
  }

  // Transaction helper — fn receives this backend instance
  async transaction(fn) {
    this._db.execute("BEGIN");
    try {
      await fn(this);
      this._db.execute("COMMIT");
    } catch (e) {
      this._db.execute("ROLLBACK");
      throw e;
    }
  }

  // Upsert: INSERT OR REPLACE (SQLite native)
  async upsert(table, columns, values) {
    const placeholders = columns.map(() => "?").join(", ");
    const sql = `INSERT OR REPLACE INTO ${table} (${columns.join(", ")}) VALUES (${placeholders})`;
    return this.execute(sql, values);
  }

  // Insert ignore: INSERT OR IGNORE (SQLite native)
  async insertIgnore(table, columns, values) {
    const placeholders = columns.map(() => "?").join(", ");
    const sql = `INSERT OR IGNORE INTO ${table} (${columns.join(", ")}) VALUES (${placeholders})`;
    return this.execute(sql, values);
  }

  close() {
    this._db.close();
  }
}

// ---------------------------------------------------------------------------
// PostgreSQL backend
// ---------------------------------------------------------------------------

class PostgresBackend {
  constructor(url) {
    this._client = new PgClient(url);
  }

  get name() { return "postgres"; }

  async init() {
    await this._client.connect();
    await this._client.queryArray(ROOMS_TABLE_POSTGRES);
    await this._client.queryArray(INVITE_CODES_TABLE_POSTGRES);
  }

  async query(sql, params = []) {
    const pgSql = toPgParams(sql);
    const result = await this._client.queryArray(pgSql, params);
    return result.rows;
  }

  async execute(sql, params = []) {
    const pgSql = toPgParams(sql);
    await this._client.queryArray(pgSql, params);
  }

  async transaction(fn) {
    const tx = await this._client.createTransaction("relay_tx");
    await tx.begin();
    // Create a wrapper that uses the transaction for queries
    const txBackend = {
      query: async (sql, params = []) => {
        const pgSql = toPgParams(sql);
        const result = await tx.queryArray(pgSql, params);
        return result.rows;
      },
      execute: async (sql, params = []) => {
        const pgSql = toPgParams(sql);
        await tx.queryArray(pgSql, params);
      },
    };
    try {
      await fn(txBackend);
      await tx.commit();
    } catch (e) {
      await tx.rollback();
      throw e;
    }
  }

  // Upsert: INSERT ... ON CONFLICT (pk) DO UPDATE SET ...
  async upsert(table, columns, values) {
    const pk = columns[0]; // assumes first column is the primary key
    const placeholders = columns.map((_, i) => `$${i + 1}`).join(", ");
    const updates = columns.slice(1).map((c, i) => `${c} = $${i + 2}`).join(", ");
    const sql = `INSERT INTO ${table} (${columns.join(", ")}) VALUES (${placeholders}) ON CONFLICT (${pk}) DO UPDATE SET ${updates}`;
    await this._client.queryArray(sql, values);
  }

  // Insert ignore: INSERT ... ON CONFLICT DO NOTHING
  async insertIgnore(table, columns, values) {
    const placeholders = columns.map((_, i) => `$${i + 1}`).join(", ");
    const sql = `INSERT INTO ${table} (${columns.join(", ")}) VALUES (${placeholders}) ON CONFLICT DO NOTHING`;
    await this._client.queryArray(sql, values);
  }

  close() {
    this._client.end();
  }
}

// ---------------------------------------------------------------------------
// MySQL backend
// ---------------------------------------------------------------------------

class MysqlBackend {
  constructor(url) {
    this._url = new URL(url);
    this._client = null;
  }

  get name() { return "mysql"; }

  async init() {
    this._client = await new MysqlClient().connect({
      hostname: this._url.hostname,
      port: parseInt(this._url.port || "3306"),
      username: this._url.username,
      password: this._url.password,
      db: this._url.pathname.replace(/^\//, ""),
    });
    await this._client.execute(ROOMS_TABLE_MYSQL);
    await this._client.execute(INVITE_CODES_TABLE_MYSQL);
  }

  async query(sql, params = []) {
    const result = await this._client.execute(sql, params);
    return (result.rows ?? []).map((row) => Object.values(row));
  }

  async execute(sql, params = []) {
    await this._client.execute(sql, params);
  }

  async transaction(fn) {
    await this._client.transaction(async (conn) => {
      const txBackend = {
        query: async (sql, params = []) => {
          const result = await conn.execute(sql, params);
          return (result.rows ?? []).map((row) => Object.values(row));
        },
        execute: async (sql, params = []) => {
          await conn.execute(sql, params);
        },
      };
      await fn(txBackend);
    });
  }

  // Upsert: REPLACE INTO (MySQL native)
  async upsert(table, columns, values) {
    const placeholders = columns.map(() => "?").join(", ");
    const sql = `REPLACE INTO ${table} (${columns.join(", ")}) VALUES (${placeholders})`;
    await this._client.execute(sql, values);
  }

  // Insert ignore: INSERT IGNORE (MySQL native)
  async insertIgnore(table, columns, values) {
    const placeholders = columns.map(() => "?").join(", ");
    const sql = `INSERT IGNORE INTO ${table} (${columns.join(", ")}) VALUES (${placeholders})`;
    await this._client.execute(sql, values);
  }

  close() {
    this._client.close();
  }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

export async function createDatabase() {
  const backend = (Deno.env.get("DB_BACKEND") ?? "sqlite").toLowerCase();
  const url = Deno.env.get("DATABASE_URL") ?? "";

  let db;
  switch (backend) {
    case "postgres":
    case "postgresql":
      if (!url) throw new Error("DATABASE_URL is required for postgres backend");
      db = new PostgresBackend(url);
      break;
    case "mysql":
    case "mariadb":
      if (!url) throw new Error("DATABASE_URL is required for mysql backend");
      db = new MysqlBackend(url);
      break;
    case "sqlite":
    default: {
      const dbPath = Deno.env.get("DB_PATH") ?? "/data/relay.db";
      db = new SqliteBackend(dbPath);
      break;
    }
  }

  await db.init();
  console.log(`  Database backend: ${db.name}`);
  return db;
}

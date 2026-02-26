# Bear

<p align="center">
  <img src="Logo.png" alt="Bear Logo" width="360" height="360">
</p>

Bear is a Rust-based "claude code"-style coding assistant with persistent sessions backed by a single `bear-server` process. Multiple clients (native terminal and browser) can connect to the same session simultaneously, and sessions persist even after client terminals close.

## Features

### Core
- **Persistent sessions** with a single shared `bear-server`
- **Native terminal client** (`bear`) and **browser client** (`bearjs/bear.js` + `html/index.html`)
- **Multi-client sync** — multiple clients can connect to the same session; prompts are broadcast to all clients and dismissed everywhere when any client responds
- **Interactive session picker** on connect (create new or resume existing)

### Tools (25)
- **File I/O** — `read_file`, `write_file`, `edit_file`, `patch_file` (unified diff), `list_files`, `search_text`
- **LSP-powered** — `read_symbol`, `patch_symbol`, `lsp_diagnostics`, `lsp_hover`, `lsp_references`, `lsp_symbols`
- **Shell** — `run_command` (with live streaming output)
- **Git** — `git_commit` (stages all changes, commits with co-author trailer)
- **Web** — `web_fetch`, `web_search` (DDG → Google → Brave fallback chain)
- **Computation** — `js_eval` (sandboxed JavaScript REPL via boa_engine)
- **Reusable scripts** — `js_script_save`, `js_script_list`, `js_script` (LLM-authored workspace scripts persisted in `.bear/`)
- **Session** — `session_workdir`, `undo` (up to 10 steps)
- **Planning** — `todo_write`, `todo_read`
- **User interaction** — `user_prompt_options`

### Workspace persistence (`.bear/` folder)
- A `.bear/` folder per working directory stores persistent workspace state
- **Auto-approved tools/commands** — "Always approve" choices are saved to `.bear/auto_approved.json` and restored when a new session starts in the same directory
- **Reusable scripts** — The LLM can save JS utility scripts to `.bear/scripts/` and invoke them later via `js_script`
- On `session_workdir` change, the session loads fresh state from the new directory's `.bear/`
- The `.bear/` directory is **protected** — the LLM cannot read, modify, or access it via file tools or shell commands
- Write serialization per directory prevents race conditions across concurrent sessions

### Tool confirmations
- **Picker-based** — Approve / Deny / Always approve for each tool call
- **Auto-approve** — server-side per-session allowlist; "Always approve" applies to all agents in the session and persists to `.bear/`
- Unified diff output shown for file mutations (`write_file`, `edit_file`, `patch_file`, `patch_symbol`)

### Task plans & subagents
- The LLM can propose **task plans** splitting work into read-only and write tasks
- **Read-only tasks** run as concurrent subagents (configurable max via `/session max_subagents`)
- **Write tasks** run sequentially via the main agent
- All prompts (tool confirmations, user prompts, depth-limit continuations) from any agent are **serialized through a prompt queue** — only one prompt is active at a time

### LSP integration
- Language servers are **lazily spawned** per language per workspace
- Built-in support: **Rust** (rust-analyzer), **TypeScript/JavaScript** (typescript-language-server), **Python** (pyright), **Go** (gopls), **C/C++** (clangd), **Java** (jdtls), **Zig** (zls)
- Override any LSP command via `BEAR_LSP_<LANG>` env vars (e.g. `BEAR_LSP_RUST=my-rust-analyzer`)

### UI
- **Mouse/trackpad scroll** for viewport scrolling (native and browser clients)
- **Arrow Up/Down** for command history
- **PageUp/PageDown** for viewport scrolling
- **Slash command autocomplete** with dropdown (Tab to accept)
- **Esc** during streaming shows a hint to interrupt
- **Tool-depth guard** with continuation prompt (continue / continue for next N / stop)

### Session commands
| Command | Description |
|---|---|
| `/ps` | List running background processes |
| `/kill <pid>` | Kill a background process |
| `/send <pid> <text>` | Send stdin to a process |
| `/session name <n>` | Name the current session |
| `/session workdir <path>` | Set session working directory |
| `/session max_subagents <n>` | Set max concurrent subagents |
| `/allowed` | Show auto-approved commands |
| `/exit` | Disconnect, keep session alive |
| `/end` | End session, pick another |
| `/help` | Show help |

## Quick start

```bash
# Run the server (one per machine)
cargo run -p bear-server

# Run a native terminal client
cargo run -p bear
```

By default, `bear` connects to `http://127.0.0.1:49321` and will:
- Auto-create a session if none exist
- Prompt to select an existing session or create a new one
- Switch to the session working directory

## Configuration

Bear is configured via environment variables on the **server**:

| Variable | Default | Description |
|---|---|---|
| `BEAR_LLM_PROVIDER` | `ollama` | LLM provider (`ollama` or `openai`) |
| `BEAR_OPENAI_API_KEY` | *(none)* | OpenAI-compatible API key |
| `BEAR_OPENAI_MODEL` | `gpt-4` | OpenAI model name |
| `BEAR_OPENAI_URL` | `https://api.openai.com` | OpenAI-compatible API base URL |
| `BEAR_OLLAMA_URL` | `http://127.0.0.1:11434` | Ollama API base URL |
| `BEAR_OLLAMA_MODEL` | `llama3.1` | Ollama model name |
| `BEAR_MAX_TOOL_DEPTH` | `100` | Max consecutive tool calls before prompting to continue |
| `BEAR_MAX_TOOL_OUTPUT_CHARS` | `32000` | Truncation limit for tool output |
| `BEAR_CONTEXT_BUDGET` | `16000` | Context window budget (characters) for conversation history |
| `BEAR_KEEP_RECENT` | `20` | Number of recent messages always kept in context |
| `BEAR_GOOGLE_API_KEY` | *(none)* | Google Custom Search API key (web_search fallback) |
| `BEAR_GOOGLE_CX` | *(none)* | Google Custom Search engine ID |
| `BEAR_BRAVE_API_KEY` | *(none)* | Brave Search API key (web_search fallback) |
| `BEAR_LSP_<LANG>` | *(per-language defaults)* | Override LSP server command for a language |

Examples:

```bash
# Ollama (default)
BEAR_OLLAMA_MODEL=qwen2.5-coder BEAR_MAX_TOOL_DEPTH=50 cargo run -p bear-server

# OpenAI
BEAR_LLM_PROVIDER=openai BEAR_OPENAI_API_KEY=sk-... cargo run -p bear-server

# OpenAI-compatible provider (e.g. LM Studio, vLLM)
BEAR_LLM_PROVIDER=openai BEAR_OPENAI_URL=http://localhost:1234 BEAR_OPENAI_MODEL=my-model cargo run -p bear-server
```

## Browser client

Open `html/index.html` in a browser to use the xterm.js-based terminal client (`bearjs/bear.js`). It connects to `bear-server` via WebRTC DataChannels with HTTP signaling, enabling NAT traversal.

## Remote access (relay)

Bear supports remote browser access via a three-tier signaling architecture:

```
                         Public Internet
                              │
Browser ◄──login──► Public Server ◄──HTTPS──► bear-server
  (bear.js)        (auth + JWT)               (user's machine)
                        │                          │
                   internal net              HTTPS (JWT-gated)
                        │                          │
                        └─────► Relay ◄──────────┘
                              (Docker)
                           SQLite + HTTP
                           mailbox
```

**Three tiers:**
1. **Relay** (`bear-relay/`, built by us) — stateless HTTP signaling mailbox with SQLite persistence for rooms/public keys. Dockerized.
2. **Public server** (external, not built here) — user auth, invite codes, serves `bear.js`, mints JWTs.
3. **Bear ecosystem** — native client (`bear`), `bear-server`, `bear.js`.

Once signaling completes, the WebRTC DataChannel is **peer-to-peer** (browser ↔ bear-server) — the relay is only involved during signaling + ICE exchange.

### Security model

- **Asymmetric JWTs (RS256)** — `bear-server` generates an RSA-2048 keypair at pairing time. The public key is sent to the relay; the private key stays local. JWTs are signed with the private key and verified by the relay using the public key.
- **Hashed invite codes** — invite codes are SHA-256 hashed before transmission. The relay only ever stores hashes, never plaintext codes.
- **Credential lifecycle** — invite codes have a 10-minute TTL and are burned (deleted) on first use.
- **TLS SPKI pinning** — during pairing, `bear-server` captures the relay's TLS certificate SPKI fingerprint and saves it. On every subsequent poll, the pin is enforced. A mismatch is treated as a fatal security event: polling stops and the user is notified.
- **Connection notifications** — when a remote browser connects via relay, a notice is broadcast to all native clients.
- **WebRTC fingerprint verification** — both `bear-server` and the browser compute a 6-character SAS verification code from the DTLS fingerprints in the SDP offer/answer. Users can visually compare these codes to confirm the connection is not intercepted.

### Relay deployment

```bash
cd bear-relay

# Docker Compose (recommended)
docker compose up -d

# Or manual Docker
docker build -t bear-relay .
docker run -d \
  -p 8080:8080 \
  -p 8081:8081 \
  -v /path/on/host:/data \
  -e PORT=8080 \
  -e INTERNAL_PORT=8081 \
  bear-relay
```

| Env var | Default | Description |
|---|---|---|
| `PORT` | `8080` | External API port (internet-facing, JWT-gated) |
| `INTERNAL_PORT` | `8081` | Internal API port (no auth, internal network only) |
| `DB_PATH` | `/data/relay.db` | SQLite database path |

**Important:** The internal port (`8081`) must only be accessible from your internal network. The public server uses it to query signing keys and manage rooms.

### Relay API reference

**External routes** (JWT-gated, internet-facing on `PORT`):

| Method | Path | Description |
|---|---|---|
| `POST` | `/pair` | Register a new room: `{ room_id, signing_key (RSA public key PEM), invite_code (SHA-256 hex hash) }` |
| `DELETE` | `/room/:room_id` | Revoke a room (requires Bearer JWT) |
| `POST` | `/room/:room_id/offer` | Browser posts SDP offer → returns `{ conn_id }` |
| `GET` | `/room/:room_id/offer` | Bear-server polls for pending offers |
| `POST` | `/room/:room_id/answer/:conn_id` | Bear-server posts SDP answer |
| `GET` | `/room/:room_id/answer/:conn_id` | Browser polls for the answer |
| `POST` | `/room/:room_id/ice/:conn_id/:side` | Post ICE candidates (`side` = `server` or `client`) |
| `GET` | `/room/:room_id/ice/:conn_id/:side` | Poll ICE candidates |

**Internal routes** (no auth, on `INTERNAL_PORT`):

| Method | Path | Description |
|---|---|---|
| `GET` | `/internal/rooms` | List all rooms (with pagination) |
| `GET` | `/internal/room/:room_id` | Get room details including public key PEM |
| `DELETE` | `/internal/room/:room_id` | Revoke a room (admin) |
| `POST` | `/internal/invites` | Push invite code hashes: `{ codes: ["<sha256-hex>", ...] }` (10-min TTL) |
| `GET` | `/internal/invites` | List invite codes `[{ code_hash, created_at, expires_at }]` |

### Public server contract

The public server is an **external dependency** not built in this repo. It must:

1. **Authenticate users** (accounts, login, sessions)
2. **Generate invite codes**, SHA-256 hash them, and push the hashes to the relay via `POST /internal/invites`
3. **Mint JWTs** for authenticated browser sessions by querying `GET /internal/room/:room_id` for the public key PEM, then signing a JWT with `{ room_id, iat }` using **RS256** (the room's RSA public key)
4. **Serve `bear.js`** with relay config injected (e.g. `BEAR_RELAY_URL`, `BEAR_RELAY_JWT`, `BEAR_ROOM_ID` globals)
5. **Provide a UI** for pairing status, invite code generation, and revocation

### Remote access setup

```bash
# 1. Get an invite code from the public server
# 2. Pair your bear-server with the relay (default: https://bear.applegrew.com)
bear --relay-pair <invite_code>

# Override relay URL via env var
BEAR_RELAY_URL=https://my-relay.example.com bear --relay-pair <invite_code>

# Subsequent starts: relay polling is automatic

# Manage relay
bear --disable-relay    # Persistently disable relay
bear --enable-relay     # Re-enable relay
bear --relay-revoke     # Revoke pairing
```

Pairing generates an RSA-2048 keypair, hashes the invite code, registers with the relay, captures the relay's TLS SPKI pin, and saves credentials to `~/.bear/relay.json`.

### Server control

```bash
bear --stop             # Stop the running bear-server
bear --restart          # Restart the bear-server
```

| Flag | Stops server? | Starts server? | Launches client? |
|---|---|---|---|
| `--stop` | Yes | No | No |
| `--restart` | Yes (if running) | Yes | No |
| `--disable-relay` | Prompts if running | No | No |
| `--enable-relay` | Prompts if running | No | No |
| *(no flag)* | No | Auto-launch if needed | Yes |

## Project structure

```
bear/
├── bear-core/      # Core logic: LLM, tools, prompts, config, shared types
├── bear-server/    # Server: session management, LLM, tools, LSP, WebRTC, relay polling
├── bear/           # Native terminal client (crossterm TUI)
├── bear-relay/     # Relay server (Deno + SQLite, Dockerized)
├── bearjs/         # Browser client (xterm.js TUI, dual-mode signaling)
└── html/           # Browser client HTML entry point
```

## Misc

Releasing a new version:
```bash
git tag v0.1.0 && git push origin v0.1.0
```

This will trigger the GitHub Actions workflow to build and release the new version.

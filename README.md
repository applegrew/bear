# Bear

Bear is a Rust-based "claude code"-style coding assistant with persistent sessions backed by a single `bear-server` process. Multiple clients (native terminal and browser) can connect to the same session simultaneously, and sessions persist even after client terminals close.

## Features

### Core
- **Persistent sessions** with a single shared `bear-server`
- **Native terminal client** (`bear`) and **browser client** (`bearjs/bear.js` + `html/index.html`)
- **Multi-client sync** — multiple clients can connect to the same session; prompts are broadcast to all clients and dismissed everywhere when any client responds
- **Interactive session picker** on connect (create new or resume existing)

### Tools (20)
- **File I/O** — `read_file`, `write_file`, `edit_file`, `patch_file` (unified diff), `list_files`, `search_text`
- **LSP-powered** — `read_symbol`, `patch_symbol`, `lsp_diagnostics`, `lsp_hover`, `lsp_references`, `lsp_symbols`
- **Shell** — `run_command` (with live streaming output)
- **Web** — `web_fetch`, `web_search`
- **Session** — `session_workdir`, `undo` (up to 10 steps)
- **Planning** — `todo_write`, `todo_read`
- **User interaction** — `user_prompt_options`

### Tool confirmations
- **Picker-based** — Approve / Deny / Always approve for each tool call
- **Auto-approve** — server-side per-session allowlist; "Always approve" applies to all agents in the session
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

## Project structure

```
bear/
├── bear-core/      # Shared types (ClientMessage, ServerMessage, etc.)
├── bear-server/    # Server: session management, LLM, tools, LSP, WebRTC
├── bear/           # Native terminal client (crossterm TUI)
├── bearjs/         # Browser client (xterm.js TUI)
└── html/           # Browser client HTML entry point
```

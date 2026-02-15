# Bear

Bear is a Rust-based "claude code"-style CLI with persistent sessions backed by a single `bear-server` process. Multiple `bear` clients can connect to the same server, and sessions persist even after client terminals close.

## Features

- Persistent sessions with a single shared `bear-server`
- Native and browser clients
- Interactive session picker and command help
- Picker-based tool call confirmations (Approve / Deny / Always approve)
- Auto-handled `user_prompt_options` prompts (no tool confirmation step)
- Unified diff output for file mutations (`write_file`, `edit_file`, `patch_file`)
- Esc during streaming shows a prompt to interrupt and send a new request
- Tool-depth guard with a continuation prompt (continue / continue for next N / stop)
- Session commands: `/ps`, `/kill`, `/send`, `/session name`, `/session workdir`, `/allowed`, `/exit`, `/end`, `/help`

## Quick start

```bash
# Run the server (one per machine)
cargo run -p bear-server

# Run a client
cargo run -p bear
```

By default, `bear` connects to `http://127.0.0.1:49321` and will:
- Auto-create a session if none exist
- Prompt to select an existing session or create a new one
- Switch to the session working directory and inform you

## Ollama integration

Bear connects to Ollama via `/api/chat`.

Environment variables:
- `BEAR_OLLAMA_URL` (default: `http://127.0.0.1:11434`)
- `BEAR_OLLAMA_MODEL` (default: `llama3.1`)

Example:

```bash
BEAR_OLLAMA_MODEL=llama3.1 BEAR_OLLAMA_URL=http://127.0.0.1:11434 cargo run -p bear-server
```

## Browser client

Open `html/index.html` in a browser to use the xterm.js-based terminal client (`bearjs/bear.js`). It connects to `bear-server` via WebRTC DataChannels with HTTP signaling, enabling NAT traversal.

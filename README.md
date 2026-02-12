# Bear

Bear is a Rust-based "claude code"-style CLI with persistent sessions backed by a single `bear-server` process. Multiple `bear` clients can connect to the same server, and sessions persist even after client terminals close.

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

See [web/README.md](web/README.md) for wasm build and browser demo instructions.

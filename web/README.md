# Bear Browser Client

## Build the wasm client

```bash
cargo install wasm-pack
wasm-pack build ../bear-wasm --target web
```

## Serve the example page

```bash
python3 -m http.server 8080
```

Open http://localhost:8080/web/ in your browser, update the server URL if needed, and connect.

## Proxy transport

Enable **Use proxy transport** to queue outbound messages. Read the queued JSON from the UI, send it to your proxy API, and feed responses back into the client using the inbound panel.

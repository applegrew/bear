# bear-relay

Relay server for Bear (Deno + SQLite).

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

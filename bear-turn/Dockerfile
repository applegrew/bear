# bear-turn — Dockerfile for turn-rs TURN server
#
# Builds turn-rs from crates.io and runs with the provided config.
#
# Build:
#   docker build -t bear-turn .
#
# Run:
#   docker run -d \
#     -p 3478:3478/udp -p 3478:3478/tcp -p 5349:5349/tcp \
#     -v ./turn.toml:/etc/turn/turn.toml:ro \
#     -v /path/to/certs:/etc/turn/certs:ro \
#     bear-turn

FROM rust:latest AS builder

RUN cargo install turn-server --version 3.4.0 --features tcp

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/cargo/bin/turn-server /usr/local/bin/turn-server

RUN mkdir -p /etc/turn

COPY turn.toml /etc/turn/turn.toml

EXPOSE 3478/udp 3478/tcp 5349/tcp

CMD ["turn-server", "--config", "/etc/turn/turn.toml"]

# bear-turn — Dockerfile for coturn TURN server
#
# Uses the official coturn image with our configuration.
#
# Build:
#   docker build -t bear-turn .
#
# Run:
#   docker run -d \
#     -p 3478:3478/udp -p 3478:3478/tcp -p 443:443/tcp \
#     -v ./turnserver.conf:/etc/turnserver.conf:ro \
#     bear-turn

FROM coturn/coturn:latest

COPY turnserver.conf /etc/turnserver.conf

EXPOSE 3478/udp 3478/tcp 443/tcp 49152-65535/udp

CMD ["turnserver", "-c", "/etc/turnserver.conf"]

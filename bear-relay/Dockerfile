FROM denoland/deno:2.1.4

WORKDIR /app

# Copy source files
COPY relay.js db.js ./

# Create data directory for SQLite (used when DB_BACKEND=sqlite)
RUN mkdir -p /data

ARG RELAY_VERSION=0.1.4
ENV RELAY_VERSION=${RELAY_VERSION}

EXPOSE 8080 8081

# --allow-net covers both HTTP listeners and remote DB connections (postgres/mysql)
CMD ["deno", "run", "--allow-net", "--allow-read=/data,/app", "--allow-write=/data", "--allow-env", "relay.js"]

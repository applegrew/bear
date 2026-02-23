FROM denoland/deno:2.1.4

WORKDIR /app

# Cache dependencies by copying just the source file
COPY relay.js .

# Create data directory for SQLite
RUN mkdir -p /data

EXPOSE 8080 8081

# Run with necessary permissions
CMD ["deno", "run", "--allow-net", "--allow-read=/data", "--allow-write=/data", "--allow-env", "relay.js"]

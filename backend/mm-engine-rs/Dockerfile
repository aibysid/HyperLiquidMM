# ─── Builder Stage ─────────────────────────────────────────────────────────────
FROM rust:1.85-slim as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# ── Layer 1: cache dependencies by building a dummy binary ──────────────────
# Only Cargo files are copied first so dep layers are cached on code changes
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm src/main.rs

# ── Layer 2: real source code ────────────────────────────────────────────────
COPY src ./src
# Touch main.rs to bust the build cache for the app binary only (not deps)
RUN touch src/main.rs
RUN cargo build --release

# ─── Runtime Stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# Install runtime dependencies (OpenSSL + TLS roots for Hyperliquid HTTPS/WSS)
RUN apt-get update && apt-get install -y \
    libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the compiled binary
COPY --from=builder /app/target/release/mm-engine-rs /app/mm-engine-rs

# Pre-create runtime directories.
# data/ticks is the tick harvester output — mount a host volume here in production.
RUN mkdir -p /app/data/ticks /app/logs

# Defaults: shadow mode ON, tick harvesting ON, info logging.
# All of these can be overridden via docker-compose environment or --env-file.
ENV RUST_LOG=info
ENV MM_SHADOW_MODE=true
ENV MM_HARVEST_TICKS=true

ENTRYPOINT ["/app/mm-engine-rs"]

# ─────────────────────────────────────────────
# Stage 1: Builder — compile release binary
# ─────────────────────────────────────────────
FROM rust:1.88-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy manifests first to leverage Docker layer cache for deps
COPY Cargo.toml Cargo.lock ./
COPY crates/engine-core/Cargo.toml crates/engine-core/Cargo.toml
COPY crates/engine-server/Cargo.toml crates/engine-server/Cargo.toml
COPY loadtest/Cargo.toml loadtest/Cargo.toml

# Stub out sources so cargo can resolve the workspace and cache deps
RUN mkdir -p crates/engine-core/src crates/engine-server/src loadtest/src \
    && echo "fn main() {}" > crates/engine-server/src/main.rs \
    && echo "fn main() {}" > loadtest/src/main.rs \
    && echo "" > crates/engine-core/src/lib.rs \
    && cargo build --release -p engine-server || true

# Now copy real sources and build for real
COPY crates ./crates
COPY migrations ./migrations

# Touch main.rs so cargo sees a fresh mtime and rebuilds with real sources
RUN touch crates/engine-server/src/main.rs crates/engine-core/src/lib.rs \
    && cargo build --release -p engine-server \
    && strip target/release/engine-server

# ─────────────────────────────────────────────
# Stage 2: Runtime — minimal image
# ─────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    wget \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --shell /bin/false engine

WORKDIR /app

COPY --from=builder /build/target/release/engine-server /usr/local/bin/engine-server
COPY --from=builder /build/migrations /app/migrations

USER engine

ENV RUST_LOG=info \
    PORT=8000 \
    BIND_ADDR=0.0.0.0:8000

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget --quiet --tries=1 --spider http://127.0.0.1:8000/api/v1/healthz || exit 1

CMD ["/usr/local/bin/engine-server"]

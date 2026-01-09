# PolyRustArb Bot - Multi-stage Dockerfile for Coolify deployment
# Build stage
FROM rust:1.75-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies (including tools needed by ethers/ring crates)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    build-essential \
    cmake \
    perl \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests and source code
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# Build the release binary
ENV RUST_BACKTRACE=full
RUN cargo build --release 2>&1 || { \
    echo ""; \
    echo "========================================"; \
    echo "BUILD FAILED - Rust compilation error:"; \
    echo "========================================"; \
    cargo build --release 2>&1; \
    exit 1; \
    }

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /app/target/release/polyarb /app/polyarb

# Copy config file
COPY config.toml /app/config.toml

# Create directories for logs
RUN mkdir -p /app/logs

# Set environment variables
ENV RUST_LOG=info
ENV POLY_MODE=test
ENV POLY_LOG_LEVEL=info

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD pgrep polyarb || exit 1

# Run the bot
ENTRYPOINT ["/app/polyarb"]
CMD ["--config", "/app/config.toml"]

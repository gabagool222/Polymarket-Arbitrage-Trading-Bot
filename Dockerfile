# PolyRustArb Bot - Multi-stage Dockerfile for Coolify deployment
# Build stage
FROM rust:1.75-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for better caching
COPY Cargo.toml Cargo.lock* ./

# Create dummy src to cache dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release && rm -rf src

# Copy actual source code
COPY src ./src

# Build the actual binary
RUN touch src/main.rs && cargo build --release

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

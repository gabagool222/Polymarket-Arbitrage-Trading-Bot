# PolyRustArb Bot - Optimized Dockerfile with Cargo Chef

# ---------------------------------------------------
# 1. Base Stage: Install common build dependencies
# ---------------------------------------------------
FROM rust:bookworm AS chef 
WORKDIR /app
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    build-essential \
    cmake \
    perl \
    git \
    clang \
    llvm \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef

# ---------------------------------------------------
# 2. Planner Stage: Create a recipe from Cargo.lock
# ---------------------------------------------------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------
# 3. Cacher Stage: Build dependencies ONLY
#    This layer is cached as long as Cargo.lock/Cargo.toml doesn't change
# ---------------------------------------------------
FROM chef AS cacher
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - this is the slow part, but it's now cached!
RUN cargo chef cook --release --recipe-path recipe.json

# ---------------------------------------------------
# 4. Builder Stage: Build the actual application
# ---------------------------------------------------
FROM chef AS builder
COPY . .
# Copy compiled dependencies from cacher
COPY --from=cacher /app/target target
COPY --from=cacher $CARGO_HOME $CARGO_HOME
# Build the actual binary (fast!)
ENV RUST_BACKTRACE=1
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
RUN cargo build --release

# ---------------------------------------------------
# 5. Runtime Stage: The final lightweight image
# ---------------------------------------------------
FROM debian:bookworm-slim AS runtime

WORKDIR /app

# Install runtime dependencies including Python
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    python3 \
    python3-pip \
    python3-venv \
    && rm -rf /var/lib/apt/lists/*

# Create virtual environment and install Python dependencies
# We do this before copying the binary to leverage cache
RUN python3 -m venv /app/venv
ENV PATH="/app/venv/bin:$PATH"
RUN pip install --no-cache-dir py-clob-client python-dotenv

# Copy binary from builder
COPY --from=builder /app/target/release/polyarb /app/polyarb

# Copy config and scripts
COPY config.toml /app/config.toml
COPY scripts/ /app/scripts/

# Create directories for logs
RUN mkdir -p /app/logs

# Set environment variables
ENV RUST_LOG=info
ENV POLY_MODE=real

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD pgrep polyarb || exit 1

# Run the bot
ENTRYPOINT ["/app/polyarb"]
CMD ["--config", "/app/config.toml"]

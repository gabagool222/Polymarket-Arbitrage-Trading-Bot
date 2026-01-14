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

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS cacher
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

FROM chef AS builder
COPY . .
COPY --from=cacher /app/target target
COPY --from=cacher $CARGO_HOME $CARGO_HOME
ENV RUST_BACKTRACE=1
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
RUN cargo build --release

FROM debian:bookworm-slim AS runtime

WORKDIR /app

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    python3 \
    python3-pip \
    python3-venv \
    && rm -rf /var/lib/apt/lists/*

RUN python3 -m venv /app/venv
ENV PATH="/app/venv/bin:$PATH"
RUN pip install --no-cache-dir py-clob-client python-dotenv

COPY --from=builder /app/target/release/polyarb /app/polyarb

COPY config.toml /app/config.toml
COPY scripts/ /app/scripts/

RUN mkdir -p /app/logs

ENV RUST_LOG=info
ENV POLY_MODE=real

HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD pgrep polyarb || exit 1

ENTRYPOINT ["/app/polyarb"]
CMD ["--config", "/app/config.toml"]

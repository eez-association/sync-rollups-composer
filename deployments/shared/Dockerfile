# Stage 1: Install cargo-chef for dependency caching
FROM rust:1.93-bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /app

# Stage 2: Analyze dependencies and generate recipe.json for caching
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Build the application (dependencies cached via cargo-chef)
FROM chef AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    libclang-dev \
    pkg-config \
    libssl-dev \
    cmake \
    && rm -rf /var/lib/apt/lists/*

# Cook dependencies (cached until Cargo.toml/Cargo.lock change)
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --recipe-path recipe.json

# Build the application (only recompiles our crate)
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release \
    && strip target/release/based-rollup

# ── Runtime image: minimal, no build tools ──
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/based-rollup /usr/local/bin/based-rollup
COPY deployments/shared/genesis.json /etc/based-rollup/genesis.json

RUN useradd -r -m -s /bin/false rollup && mkdir -p /data && chown rollup:rollup /data
USER rollup

ENTRYPOINT ["based-rollup"]

# ── Deploy image: runtime + forge/cast for contract deployment ──
FROM runtime AS deploy

USER root
COPY --from=ghcr.io/foundry-rs/foundry:v1.6.0-rc1 /usr/local/bin/forge /usr/local/bin/forge
COPY --from=ghcr.io/foundry-rs/foundry:v1.6.0-rc1 /usr/local/bin/cast /usr/local/bin/cast

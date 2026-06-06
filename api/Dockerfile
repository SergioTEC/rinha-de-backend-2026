# Multi-stage build for API (Rust + C AVX2)
# NOTE: C AVX2 is skipped in cross-platform builds (QEMU issues).
# On native x86_64 (Rinha environment), build.rs auto-detects and compiles AVX2.
FROM rust:1.78-slim-bookworm AS builder

WORKDIR /app

# Add Rust target
RUN rustup target add x86_64-unknown-linux-gnu

# Copy workspace first
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock

# Copy manifests first (for layer caching)
COPY api/Cargo.toml api/Cargo.toml
COPY api/build.rs api/build.rs
COPY lb/Cargo.toml lb/Cargo.toml
COPY build-index/Cargo.toml build-index/Cargo.toml

# Copy source code
COPY api/src api/src
COPY lb/src lb/src
COPY build-index/src build-index/src
COPY native api/native
COPY resources /app/resources

# Build the API in release mode for x86_64 linux
RUN cargo build --target x86_64-unknown-linux-gnu --release -p rinha-api

# Runtime stage: minimal Debian image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y libssl3 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the compiled binary
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/api /app/api

# Copy resources (index + mcc_risk)
COPY --from=builder /app/resources /app/resources

# Default: listen on TCP port 9999 (overridden by docker-compose)
EXPOSE 9999

CMD ["/app/api"]
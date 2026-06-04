<<<<<<< HEAD
# Dockerfile único para Rinha Backend 2026
# Builda API + LB em uma única imagem
# Baseado no padrão do top 1 (dalvorsn)

FROM rust:1.78-slim-bookworm AS builder

WORKDIR /app

# Add Rust target
RUN rustup target add x86_64-unknown-linux-gnu

# Copy workspace first (for layer caching)
COPY Cargo.toml Cargo.lock ./
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

# Build everything in release mode
# Skip C AVX2 on cross-build; native x86_64 auto-detects
ENV RINHA_SKIP_C_AVX2=1
RUN cargo build --target x86_64-unknown-linux-gnu --release

# Runtime: minimal Debian image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y libssl3 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy both binaries
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/api /app/api
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/lb /app/lb

# Copy resources
COPY --from=builder /app/resources /app/resources

# Default expose
EXPOSE 9999

# Default entrypoint
ENTRYPOINT ["/app/api"]
CMD ["9999"]
=======
FROM debian:bookworm-slim

WORKDIR /app

COPY target/x86_64-unknown-linux-gnu/release/api /app/api
COPY target/x86_64-unknown-linux-gnu/release/lb /app/lb
COPY resources /app/resources
>>>>>>> submission

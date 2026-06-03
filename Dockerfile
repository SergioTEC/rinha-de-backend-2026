# Rinha de Backend 2026
# Multi-stage: binários cross-compilados externamente via cargo-zigbuild

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy pre-built binaries (cross-compiled with cargo-zigbuild)
COPY target/x86_64-unknown-linux-gnu/release/api /app/api
COPY target/x86_64-unknown-linux-gnu/release/lb /app/lb
COPY resources /app/resources

# Não há ENTRYPOINT/CMD aqui - o docker-compose.yml define o comando por serviço

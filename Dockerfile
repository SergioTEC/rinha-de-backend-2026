FROM debian:bookworm-slim

WORKDIR /app

COPY target/x86_64-unknown-linux-gnu/release/api /app/api
COPY target/x86_64-unknown-linux-gnu/release/lb /app/lb
COPY resources /app/resources

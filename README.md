# Rinha de Backend 2026 — Submission (Rust + C)

## Architecture

- **Load Balancer**: SCM_RIGHTS fd passing via Unix sockets (zero-copy proxy)
- **API**: Rust single-threaded + epoll, TCP or Unix socket mode
- **Busca Vetorial**: 3-phase pipeline (fast path heuristics → decision tree → IVF adaptive)
- **SIMD**: AVX2 in C for L2 distance computation
- **Quantização**: int16 (×10000) for half memory + 8× SIMD throughput

## Project Structure

```
rinha-de-backend-2026/
├── Cargo.toml              # Workspace manifest
├── docker-compose.yml      # Submission file (1 LB + 2 APIs)
├── resources/
│   ├── references.json.gz  # 3M vectors (downloaded)
│   ├── mcc_risk.json      # Merchant risk table
│   └── normalization.json # Constants
├── api/
│   ├── Cargo.toml
│   ├── build.rs            # Compile C AVX2 when targeting x86_64
│   ├── Dockerfile          # Multi-stage build for linux/amd64
│   └── src/
│       ├── main.rs         # Entry point (TCP or Unix socket)
│       ├── http.rs         # Minimal HTTP/1.1 server (zero-alloc)
│       ├── json_parser.rs  # Custom JSON parser (no serde)
│       ├── vectorize.rs    # 14-dimension vectorization
│       ├── dataset.rs      # mmap + gunzip + SoA layout
│       ├── fastpath.rs     # Heuristics + decision tree
│       └── ivf.rs          # IVF index (k-means, adaptive probe)
├── lb/
│   ├── Cargo.toml
│   ├── Dockerfile
│   └── src/main.rs         # SCM_RIGHTS fd passing load balancer
├── build-index/
│   └── src/main.rs         # Offline training tool (placeholder)
└── native/
    ├── distance_avx2.h     # C header for AVX2 distance
    └── distance_avx2.c     # AVX2 implementation (vpaddd, madd_epi16)
```

## 3-Phase Detection Pipeline

| Phase | Method |
|-------|--------|
| 1 | Fast path heuristics |
| 2 | Decision tree |
| 3 | IVF adaptive |

## Key Optimizations

- **SCM_RIGHTS**: Zero-copy load balancing
- **Quantização int16**: Half memory, AVX2 processes 8 vectors at once
- **SoA layout**: Dimensions stored separately for SIMD efficiency
- **Zero-allocation parser**: No heap allocations during request processing
- **Warm-up**: Synthetic queries before /ready to heat caches

## Build & Run

### Local development (Mac ARM64, scalar fallback)
```bash
cargo run -p rinha-api -- 9999
```

### Docker (linux/amd64 with AVX2)
```bash
docker build --platform linux/amd64 -t rinha-api ./api
docker compose up
```

## Resource Limits (within Rinha constraints)

| Service | CPU | Memory |
|---------|-----|--------|
| LB | 0.10 | 20 MB |
| API 1 | 0.40 | 160 MB |
| API 2 | 0.40 | 160 MB |
| **Total** | **0.90** | **340 MB** |

## Notes

- The decision tree is currently hardcoded (simple). For top performance, it needs offline training on the full 3M dataset.
- IVF k-means is single-pass (not converged). A few more iterations would improve cell quality.
- AVX2 is only compiled for x86_64; ARM64 uses scalar fallback (slower, but good for dev).
- SCM_RIGHTS is Linux-only. Mac development uses TCP mode.

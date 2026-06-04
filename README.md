# Rinha de Backend 2026

Submissão para a Rinha de Backend 2026.

## Stack

- **Linguagem**: Rust
- **Load Balancer**: Custom
- **API**: Rust (HTTP)
- **Busca vetorial**: IVF (Inverted File Index)
- **Dataset**: 3M vetores de 14 dimensões
- **Métrica**: k-NN com k=5 (distância euclidiana)

## Branches

- `main` — código-fonte completo
- `submission` — arquivos de runtime (docker-compose, Dockerfile, info.json)

## Como rodar localmente

```bash
# Cross-compile (Mac ARM → Linux x86_64)
cd api && RINHA_SKIP_C_AVX2=1 cargo zigbuild --target x86_64-unknown-linux-gnu --release --bin api
cd ../lb && RINHA_SKIP_C_AVX2=1 cargo zigbuild --target x86_64-unknown-linux-gnu --release --bin lb

# Build imagem
docker build --platform linux/amd64 -t ghcr.io/sergiotec/rinha-2026:latest .

# Subir stack
docker compose up -d

# Testar
curl http://localhost:9999/ready
curl -X POST http://localhost:9999/fraud-score -H 'Content-Type: application/json' -d @request.json
```

## Licença

MIT — ver [LICENSE](LICENSE).

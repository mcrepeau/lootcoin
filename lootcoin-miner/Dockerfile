# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1 AS builder

# cmake and perl are required by aws-lc-rs (the crypto backend for rustls).
RUN apt-get update && apt-get install -y --no-install-recommends cmake perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY lootcoin-miner ./lootcoin-miner
COPY Cargo.lock     ./
# Minimal workspace so cargo can resolve the path without the other members.
RUN printf '[workspace]\nmembers = ["lootcoin-miner"]\nresolver = "2"\n\n[workspace.package]\nversion = "0.0.0"\nedition = "2021"\nauthors = []\nlicense = "AGPL-3.0-only"\n' > Cargo.toml
RUN cargo build --release -p lootcoin-miner \
    && strip target/release/lootcoin-miner

# ── Runtime stage ─────────────────────────────────────────────────────────────
# distroless/cc: ~20 MB — glibc, libstdc++, CA certs. No shell, no package manager.
# rustls handles TLS in pure Rust so no libssl is needed at runtime.
FROM gcr.io/distroless/cc-debian12

WORKDIR /app
COPY --from=builder /app/target/release/lootcoin-miner /usr/local/bin/lootcoin-miner

# ── Environment variables ─────────────────────────────────────────────────────
# NODE_URLS    – comma-separated node API URLs (default: http://127.0.0.1:3000)
# MINER_ADDRESS – bech32m wallet address to receive coinbase rewards. Required.
# RUST_LOG     – tracing log filter (default: info)
ENV RUST_LOG=info

CMD ["lootcoin-miner"]

# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1 AS builder

# cmake and perl are required by aws-lc-rs (the crypto backend for rustls).
RUN apt-get update && apt-get install -y --no-install-recommends cmake perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY lootcoin-faucet ./lootcoin-faucet
COPY Cargo.lock      ./
# Minimal workspace so cargo can resolve the path without the other members.
RUN printf '[workspace]\nmembers = ["lootcoin-faucet"]\nresolver = "2"\n\n[workspace.package]\nversion = "0.0.0"\nedition = "2021"\nauthors = []\nlicense = "AGPL-3.0-only"\n' > Cargo.toml
RUN cargo build --release -p lootcoin-faucet \
    && strip target/release/lootcoin-faucet

# ── Runtime stage ─────────────────────────────────────────────────────────────
# distroless/cc: ~20 MB — glibc, libstdc++, CA certs. No shell, no package manager.
# rustls handles TLS in pure Rust so no libssl is needed at runtime.
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /app/target/release/lootcoin-faucet /usr/local/bin/lootcoin-faucet

# ── Environment variables ─────────────────────────────────────────────────────
# FAUCET_SECRET_KEY  – 64 hex chars (32-byte Ed25519 seed). Required — no default.
# NODE_URL           – node to submit transactions to (default: http://127.0.0.1:3000)
# PORT               – HTTP listen port (default: 3030)
# DISPENSE_AMOUNT    – coins sent per request (default: 500)
# DISPENSE_FEE       – transaction fee paid by the faucet (default: 1)
# COOLDOWN_SECS      – seconds between dispenses per address (default: 86400 = 24 h)
# RUST_LOG           – log filter (default: info)
ENV PORT=3030
ENV RUST_LOG=info

EXPOSE 3030

CMD ["lootcoin-faucet"]

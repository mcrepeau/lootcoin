# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1 AS builder

# cmake and perl are required by aws-lc-rs (the crypto backend for rustls).
RUN apt-get update && apt-get install -y --no-install-recommends cmake perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml    ./workspace-Cargo.toml
COPY lootcoin-node ./lootcoin-node
COPY Cargo.lock    ./
# Minimal workspace so cargo can resolve the path without the other members.
# Version is extracted from the root workspace manifest — single source of truth.
RUN VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' workspace-Cargo.toml | head -1) && \
    printf '[workspace]\nmembers = ["lootcoin-node"]\nresolver = "2"\n\n[workspace.package]\nversion = "%s"\nedition = "2021"\nauthors = []\nlicense = "AGPL-3.0-only"\n' "$VERSION" > Cargo.toml
RUN cargo build --release -p lootcoin-node \
    && strip target/release/lootcoin-node

# ── Runtime stage ─────────────────────────────────────────────────────────────
# distroless/cc: ~20 MB — glibc, libstdc++, CA certs. No shell, no package manager.
# rustls handles TLS in pure Rust so no libssl is needed at runtime.
FROM gcr.io/distroless/cc-debian12

WORKDIR /app
COPY --from=builder /app/target/release/lootcoin-node /usr/local/bin/lootcoin-node

# Persist blockchain data here; mount a named volume or bind-mount to keep data
# across container restarts.
VOLUME ["/app/data"]

# ── Environment variables ─────────────────────────────────────────────────────
# PORT          – TCP port the API listens on (default: 3000)
# PEERS         – comma-separated list of peer URLs to seed from on startup
# NODE_URL      – this node's publicly reachable URL; used for self-announcement
# ARCHIVE       – set to 1 to force full genesis replay, skipping peer snapshot sync
# RUST_LOG      – tracing log filter (default: info)
ENV PORT=3000
ENV RUST_LOG=info

EXPOSE 3000

HEALTHCHECK --interval=5s --timeout=3s --retries=12 --start-period=10s \
    CMD ["/usr/local/bin/lootcoin-node", "health"]

CMD ["lootcoin-node"]

# ── Stage 1: build WASM ───────────────────────────────────────────────────────
FROM rust:slim AS wasm-builder

RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY lootcoin-wallet ./lootcoin-wallet
COPY Cargo.lock      ./
# Minimal workspace so cargo can resolve the path without the other members.
RUN printf '[workspace]\nmembers = ["lootcoin-wallet"]\nresolver = "2"\n\n[workspace.package]\nversion = "0.0.0"\nedition = "2021"\nauthors = []\nlicense = "AGPL-3.0-only"\n' > Cargo.toml
RUN wasm-pack build lootcoin-wallet --target web

# ── Stage 2: serve static files ───────────────────────────────────────────────
FROM nginx:alpine-slim

# Copy only the web assets — Rust source never enters this stage.
COPY lootcoin-web/*.html lootcoin-web/*.css lootcoin-web/*.js lootcoin-web/favicon.ico \
     /usr/share/nginx/html/

# Explicit revalidation headers so browsers never serve a stale module
# after an image rebuild (see the comment in nginx.conf).
COPY lootcoin-web/nginx.conf /etc/nginx/conf.d/default.conf

# Overlay the compiled WASM package from the builder.
COPY --from=wasm-builder /build/lootcoin-wallet/pkg/ \
     /usr/share/nginx/html/lootcoin-wallet/pkg/

EXPOSE 80

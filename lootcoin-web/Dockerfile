# ── Stage 1: build WASM ───────────────────────────────────────────────────────
FROM rust:slim AS wasm-builder

# binaryen provides wasm-opt for reliable post-build size reduction.
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates binaryen \
    && curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY lootcoin-wallet ./lootcoin-wallet
COPY Cargo.lock      ./
# Minimal workspace so cargo can resolve the path without the other members.
RUN printf '[workspace]\nmembers = ["lootcoin-wallet"]\nresolver = "2"\n' > Cargo.toml
RUN wasm-pack build lootcoin-wallet --target web \
    && wasm-opt -Oz \
         -o lootcoin-wallet/pkg/lootcoin_wallet_bg.wasm \
            lootcoin-wallet/pkg/lootcoin_wallet_bg.wasm

# ── Stage 2: serve static files ───────────────────────────────────────────────
FROM nginx:alpine-slim

# Copy only the web assets — Rust source never enters this stage.
COPY lootcoin-web/*.html lootcoin-web/*.css lootcoin-web/*.js lootcoin-web/favicon.ico \
     /usr/share/nginx/html/

# Overlay the compiled WASM package from the builder.
COPY --from=wasm-builder /build/lootcoin-wallet/pkg/ \
     /usr/share/nginx/html/lootcoin-wallet/pkg/

EXPOSE 80

# Lootcoin

A proof-of-work blockchain with a **lottery-based fee mechanism**: instead of transaction fees going directly to the miner who includes them, every fee accumulates in a shared pot. Every mined block enters the miner in a deferred lottery, awarding the miner a random fraction of that pot.

---

## Repository layout

This is a Cargo workspace. `lootcoin-core` is a separate library published on [crates.io](https://crates.io/crates/lootcoin-core).

| Directory | What it is |
|---|---|
| `lootcoin-node` | Full node — consensus engine, HTTP API, peer-to-peer sync |
| `lootcoin-miner` | CPU miner — assembles blocks and searches for a valid PoW nonce |
| `lootcoin-faucet` | Faucet service — dispenses testnet coins to any valid address |
| `lootcoin-web` | Web UI — home page, wallet interface, block explorer, faucet page |
| `lootcoin-web/lootcoin-wallet` | WebAssembly wallet — compiled to WASM and served by `lootcoin-web` |

---

## Quick start with Docker

```bash
cp .env.example .env
# Edit .env — set FAUCET_SECRET_KEY and MINER_ADDRESS
docker compose up --build
```

This starts:
- 3 nodes on ports 3001, 3002, 3003
- 1 miner pointed at all three nodes
- Faucet on port 3030
- Web UI on port 8888
- Persistent storage via named Docker volumes

The web UI defaults to connecting to `http://127.0.0.1:3001`. To point it at a different node, mount a custom `config.js` (see [Web UI configuration](#web-ui-configuration)).

---

## Running components individually

### Node

```bash
cargo run -p lootcoin-node
# Listens on http://127.0.0.1:3000 by default

# With peers and a public URL
PEERS=http://node1.example.com:3000,http://node2.example.com:3000 \
NODE_URL=http://mynode.example.com:3000 \
PORT=3000 \
cargo run -p lootcoin-node
```

### Miner

```bash
MINER_ADDRESS=loot1… cargo run -p lootcoin-miner

# Multiple nodes
NODE_URLS=http://127.0.0.1:3000,http://127.0.0.1:3001 \
MINER_ADDRESS=loot1… \
cargo run -p lootcoin-miner
```

`MINER_ADDRESS` is required and must be a bech32m lootcoin address (`loot1…`). `NODE_URLS` defaults to `http://127.0.0.1:3000`.

### Faucet

```bash
FAUCET_SECRET_KEY=<64-char hex seed> cargo run -p lootcoin-faucet
# Listens on http://127.0.0.1:3030 by default
```

See [`lootcoin-faucet/README.md`](lootcoin-faucet/README.md) for the full environment variable reference.

### Web UI

```bash
cd lootcoin-web
python3 -m http.server 8000
# Visit http://localhost:8000
```

The web UI requires a running node. By default it points at `http://127.0.0.1:3001`.

---

## Web UI configuration

The node and faucet URLs are set at runtime via `config.js`. The default bundled in the Docker image works for local docker-compose. For any other deployment, mount your own:

```js
// config.js
window.LOOTCOIN_NODE_URL   = "https://api.mynode.example.com";
window.LOOTCOIN_FAUCET_URL = "https://faucet.mynode.example.com";
```

```yaml
# docker-compose.yml
services:
  web:
    image: mcrepeau79/lootcoin-web:latest
    volumes:
      - ./config.js:/usr/share/nginx/html/config.js:ro
```

---

## Key parameters

| Parameter | Value |
|---|---|
| Hash function | CubeHash-256 |
| Signing algorithm | Ed25519 |
| Address format | bech32m (`loot1…`) |
| Block time target | 60 seconds |
| Retarget interval | 100 blocks |
| Difficulty granularity | Fractional bits (sub-bit precision) |
| Fork selection | Most accumulated work (Σ 2^bits) |
| Coinbase reward | 1 coin per block |
| Max non-coinbase txs per block | 200 |
| Ticket maturity | 100 blocks |
| Reveal window | 10 blocks |

---

## Further reading

- [`lootcoin-node/README.md`](lootcoin-node/README.md) — node configuration, API reference, consensus details
- [`lootcoin-miner/README.md`](lootcoin-miner/README.md) — miner configuration and transaction selection
- [`lootcoin-faucet/README.md`](lootcoin-faucet/README.md) — faucet API and configuration
- [`lootcoin-web/README.md`](lootcoin-web/README.md) — building and serving the web UI

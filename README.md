# Lootcoin

A simple proof-of-work blockchain with a **lottery-based reward mechanism**

---

## Concept

In Bitcoin, miners collect transaction fees directly. In Lootcoin, every time a miner mines a block that includes transactions they receive a **lottery ticket** in addition to a share of the transaction fees. After a 100-block reveal window, each ticket is settled against the accumulated block-hash entropy as provably-fair randomness. The payout is a fraction of the pot — small wins are common, jackpots are rare.

This creates a different incentive structure: miners are rewarded not just for the block they mine, but for a delayed probabilistic payout that depends on future miners' work, making the system self-reinforcing.

---

## Hash function and key parameters

Lootcoin uses [CubeHash-256](https://en.wikipedia.org/wiki/CubeHash) instead of SHA-256. CubeHash is a NIST SHA-3 finalist designed to be simple, parallelisable, and resistant to length-extension attacks.

| Parameter                          | Value |
|------------------------------------|---|
| **Hash function**                  | CubeHash-256 |
| **Signing algorithm**              | Ed25519 |
| **Address format**                 | bech32m (`loot1…`) |
| **Block time target**              | 60 seconds |
| **Difficulty algorithm**           | ASERT (per-block, 1-hour halflife) |
| **Difficulty granularity**         | Fractional bits (sub-bit precision) |
| **Fork selection**                 | Most accumulated work (Σ 2^bits) |
| **Coinbase reward**                | 1 coin per block |
| **Max non-coinbase txs per block** | 240 |
| **Reveal window**                  | 100 blocks |

---

## How the lottery works

1. **One ticket per block** — every block that contains at least one non-coinbase transaction earns the miner a lottery ticket.

2. **Reveal window** — settlement fires at `H + REVEAL_BLOCKS` (H+100), using the hashes of all 100 blocks in `[H, H+100)` as entropy. The reveal window serves as both the maturity delay and the randomness source — an attacker must control all 100 consecutive blocks to steer the outcome; at 30% hashrate that probability is 0.3^100 ≈ 10^-52.

3. **Single draw** — at block H+100 the ticket is settled against the pot *at that moment* using one probabilistic draw:

| Probability | Tier | Payout formula | Expected frequency |
|---|---|---|---|
| 62.00% | no-win | — | — |
| 36.25% | `small` | `pot / 400,000` | every ~3 blocks |
| 1.67% | `medium` | `pot / 30,000` | every ~60 blocks (~1 h) |
| 0.07% | `large` | `pot / 2,000` | every ~1,440 blocks (~1 day) |
| 0.01% | `jackpot` | `pot / 500` | every ~10,080 blocks (~1 week) |

Payouts are a flat fraction of the current pot — independent of how many transactions were in the block. No-win tickets produce no entry in `lottery_payouts`.

4. **Fee split** — each block's transaction fees are split 50/50: half goes directly to the block's miner as immediate income, half accumulates in the lottery pot. This is the per-transaction incentive for miners; the lottery rewards the block itself.

5. **Pot funding** — seeded at genesis with 99,000,000 coins; replenished by 50% of every transaction fee thereafter. Payouts are fractions of the pot so it never fully drains. The pot naturally trends from its genesis level toward a long-run equilibrium determined by network activity.

---

## Repository layout

This is a Cargo workspace. [`lootcoin-core`](https://github.com/mcrepeau/lootcoin-core) is a separate library published on [crates.io](https://crates.io/crates/lootcoin-core).

| Directory | What it is |
|---|---|
| `lootcoin-node` | Full node — consensus engine, HTTP API, peer-to-peer sync |
| `lootcoin-miner` | CPU miner — assembles blocks and searches for a valid PoW nonce |
| `lootcoin-faucet` | Faucet service — dispenses testnet coins to any valid address |
| `lootcoin-web` | Web UI — home page, wallet interface, block explorer, faucet page |
| `lootcoin-wallet` | Wallet crate — CLI tool (`lc`) and WebAssembly library for `lootcoin-web` |

---

## Quick start with Docker

See [QUICKSTART Guide](QUICKSTART.md) to get a local chain running, mine your first block, and send coins in ~15 minutes

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

### CLI wallet

```bash
cargo build --release -p lootcoin-wallet --bin lc
# Binary at target/release/lc (lc.exe on Windows)

lc new                              # generate wallet, print recovery phrase
lc import "word1 word2 … word12"   # restore from phrase
lc balance                          # confirmed + spendable balance
lc send loot1… 100 --fee 12        # sign and broadcast a transaction
lc history                          # recent transaction history
lc status                           # chain height, difficulty, pot
```

By default `lc` connects to `http://127.0.0.1:3000`. Override with `--node <URL>` or the `LOOTCOIN_NODE` environment variable. The wallet file is stored at `~/.lootcoin/wallet.json` by default (`LOOTCOIN_WALLET` to override).

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

## Further reading

- [`ECONOMY.md`](ECONOMY.md) — simulated economics: pot dynamics, supply, inflation, fee flow, and gaming resistance (with charts)
- [`lootcoin-node/README.md`](lootcoin-node/README.md) — node configuration, API reference, consensus details
- [`lootcoin-miner/README.md`](lootcoin-miner/README.md) — miner configuration and transaction selection
- [`lootcoin-faucet/README.md`](lootcoin-faucet/README.md) — faucet API and configuration
- [`lootcoin-wallet/README.md`](lootcoin-wallet/README.md) — CLI wallet commands, WASM build instructions, key derivation
- [`lootcoin-web/README.md`](lootcoin-web/README.md) — building and serving the web UI

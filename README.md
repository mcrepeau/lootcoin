# Lootcoin

A proof-of-work blockchain with a **lottery-based fee mechanism**: half of every transaction fee goes directly to the miner who includes it; the other half accumulates in a shared pot. Every block that contains at least one transaction earns the miner a deferred lottery ticket, awarding a random fraction of that pot.

---

## Concept

In Bitcoin, miners collect transaction fees directly. In Lootcoin, every fee paid by a sender is added to a shared pot. Every time a miner mines a block they receive a **lottery ticket**. After a maturity period, each ticket is settled against a window of future block hashes used as provably-fair randomness. The payout is a fraction of the pot — small wins are common, jackpots are rare.

This creates a different incentive structure: miners are rewarded not just for the block they mine, but for a delayed probabilistic payout that depends on future miners' work, making the system self-reinforcing.

---

## How the lottery works

1. **One ticket per block** — every block that contains at least one non-coinbase transaction earns the miner a lottery ticket.

2. **Maturity** — the ticket becomes eligible for settlement after `TICKET_MATURITY = 100` blocks.

3. **Reveal window** — settlement uses the hashes of the 10 blocks following maturity (`REVEAL_BLOCKS = 10`) as entropy. An attacker would need to control all 10 consecutive blocks to steer the outcome.

4. **Single draw** — at block H+110 the ticket is settled against the pot *at that moment* using one probabilistic draw:

| Probability | Tier | Payout formula | Expected frequency |
|---|---|---|---|
| 62.00% | no-win | — | — |
| 36.25% | `small` | `pot / 400,000` | every ~3 blocks |
| 1.67% | `medium` | `pot / 30,000` | every ~60 blocks (~1 h) |
| 0.07% | `large` | `pot / 2,000` | every ~1,440 blocks (~1 day) |
| 0.01% | `jackpot` | `pot / 500` | every ~10,080 blocks (~1 week) |

Payouts are a flat fraction of the current pot — independent of how many transactions were in the block. No-win tickets produce no entry in `lottery_payouts`.

5. **Fee split** — each block's transaction fees are split 50/50: half goes directly to the block's miner as immediate income, half accumulates in the lottery pot. This is the per-transaction incentive for miners; the lottery rewards the block itself.

6. **Pot funding** — seeded at genesis with 99,000,000 coins; replenished by 50% of every transaction fee thereafter. Payouts are fractions of the pot so it never fully drains. The pot naturally trends from its genesis level toward a long-run equilibrium determined by network activity.

---

## Repository layout

This is a Cargo workspace. `lootcoin-core` is a separate library published on [crates.io](https://crates.io/crates/lootcoin-core).

| Directory | What it is |
|---|---|
| `lootcoin-node` | Full node — consensus engine, HTTP API, peer-to-peer sync |
| `lootcoin-miner` | CPU miner — assembles blocks and searches for a valid PoW nonce |
| `lootcoin-faucet` | Faucet service — dispenses testnet coins to any valid address |
| `lootcoin-web` | Web UI — home page, wallet interface, block explorer, faucet page |
| `lootcoin-wallet` | Wallet crate — CLI tool (`lc`) and WebAssembly library for `lootcoin-web` |

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

## Monitoring with Prometheus

Every node exposes a Prometheus metrics endpoint at `GET /metrics`. Add a Prometheus instance to your `docker-compose.yml` to scrape it:

```yaml
services:
  prometheus:
    image: prom/prometheus:latest
    ports:
      - "9090:9090"
    volumes:
      - prometheus-data:/prometheus
    command:
      - --config.file=/etc/prometheus/prometheus.yml
      - --storage.tsdb.retention.time=30d
    configs:
      - source: prometheus_config
        target: /etc/prometheus/prometheus.yml

configs:
  prometheus_config:
    content: |
      global:
        scrape_interval: 15s

      scrape_configs:
        - job_name: lootcoin-node
          static_configs:
            - targets:
                - node:3000

volumes:
  prometheus-data:
```

Then open `http://localhost:9090` to query metrics. Key metrics exposed:

| Metric | Type | Description |
|---|---|---|
| `lootcoin_chain_height` | Gauge | Current chain height in blocks |
| `lootcoin_chain_difficulty` | Gauge | Current mining difficulty in fractional bits |
| `lootcoin_avg_block_time_secs` | Gauge | Rolling average block time over the last 10 blocks |
| `lootcoin_pot_coins` | Gauge | Current lottery pot balance |
| `lootcoin_circulating_coins` | Gauge | Coins in circulation |
| `lootcoin_mempool_size` | Gauge | Pending transactions |
| `lootcoin_peer_count` | Gauge | Known peers |
| `lootcoin_fees_collected_total` | Counter | Cumulative transaction fees collected |
| `lootcoin_lottery_wins_total` | Counter | Lottery wins by tier (`small`, `medium`, `large`, `jackpot`) |
| `lootcoin_blocks_total` | Counter | Cumulative blocks applied to the chain |

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
| Difficulty algorithm | ASERT (per-block, 1-hour halflife) |
| Difficulty granularity | Fractional bits (sub-bit precision) |
| Fork selection | Most accumulated work (Σ 2^bits) |
| Coinbase reward | 1 coin per block |
| Max non-coinbase txs per block | 240 |
| Ticket maturity | 100 blocks |
| Reveal window | 10 blocks |

---

## Further reading

- [`lootcoin-node/README.md`](lootcoin-node/README.md) — node configuration, API reference, consensus details
- [`lootcoin-miner/README.md`](lootcoin-miner/README.md) — miner configuration and transaction selection
- [`lootcoin-faucet/README.md`](lootcoin-faucet/README.md) — faucet API and configuration
- [`lootcoin-wallet/README.md`](lootcoin-wallet/README.md) — CLI wallet commands, WASM build instructions, key derivation
- [`lootcoin-web/README.md`](lootcoin-web/README.md) — building and serving the web UI

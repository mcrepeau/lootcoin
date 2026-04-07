# lootcoin-miner

Miner for Lootcoin. Fetches pending transactions from a node, assembles a block, and searches for a proof-of-work nonce that satisfies the current difficulty target. Supports both CPU and NVIDIA GPU mining.

---

## How it works

1. **Fetch work** — queries `/chain/head` for the current height, difficulty, and tip hash, then `/mempool` for pending transactions.
2. **Select transactions** — idle network (≤ 200 pending): include everything sorted by fee descending. Busy network (> 200 pending): gate by fee-tier eligibility so high-fee senders are included first and low-fee senders age in over time. A transaction with fee `f` becomes eligible after `(120 / f) - 1` blocks.
3. **Mine** — increments a nonce and hashes with CubeHash-256 until the block hash meets the current difficulty target. Difficulty is a fractional bit count (e.g. `26.47`) — see `lootcoin-core` for how the threshold is evaluated. Runs in a blocking thread so the async runtime stays free.
4. **Cancel on new block** — a background task subscribes to the node's `/events` SSE stream and sets a cancel flag the moment a `block` event arrives, discarding the stale job immediately. Falls back to polling `/chain/head` every 2 s if SSE is unavailable.
5. **Submit** — POSTs the completed block to `/blocks`. On rejection the miner logs a warning and fetches fresh work.

The miner proceeds even when the mempool is empty: coinbase-only blocks still earn the block reward and advance the chain. They do not earn a lottery ticket (tickets require at least one non-coinbase transaction), so the miner only waits when the mempool is non-empty but every pending transaction is still within its fee-eligibility delay.

---

## GPU mining (NVIDIA)

The miner includes a CUDA kernel that runs the CubeHash-256 proof-of-work search on an NVIDIA GPU. Each kernel launch tests ~4M nonces in parallel. On an NVIDIA GeForce RTX 5070Ti, we get a 600x speedup over a single Intel Core Ultra 7 265K CPU core.

**Requirements:**

- NVIDIA GPU with CUDA support
- CUDA Toolkit installed (`nvcc` must be on `PATH` at build time)
- CUDA driver compatible with your toolkit version

**Building with GPU support:**

```bash
cargo build --release -p lootcoin-miner --features gpu
```

**Running with GPU:**

```bash
USE_GPU=1 MINER_ADDRESS=loot1… ./target/release/lootcoin-miner
```

If `USE_GPU=1` is set but the GPU fails to initialise (wrong driver, no device, etc.) the miner logs a warning and falls back to CPU automatically. If the binary was built without `--features gpu`, `USE_GPU` is ignored.

Only device 0 is used. Multi-GPU support is not currently implemented.

---

## Configuration

All configuration is via environment variables.

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MINER_ADDRESS` | Yes | — | bech32m payout address (`loot1…`) — generate one with the wallet UI |
| `NODE_URLS` | No | `http://127.0.0.1:3000` | Comma-separated node API URLs |
| `USE_GPU` | No | `0` | Set to `1` to enable GPU mining (requires build with `--features gpu`) |
| `RUST_LOG` | No | `info` | Tracing log filter |

The miner tries each URL in order and uses the first that responds. If the active node goes down, the next attempt picks another.

---

## Running

```bash
# CPU mining
MINER_ADDRESS=loot1… cargo run -p lootcoin-miner

# CPU mining — multiple nodes
NODE_URLS=http://127.0.0.1:3000,http://127.0.0.1:3001 \
MINER_ADDRESS=loot1… \
cargo run -p lootcoin-miner

# GPU mining (requires --features gpu at build time)
USE_GPU=1 MINER_ADDRESS=loot1… cargo run -p lootcoin-miner --features gpu
```

The miner loops continuously. Each successful block earns a lottery ticket (see `lootcoin-node` README for lottery details).

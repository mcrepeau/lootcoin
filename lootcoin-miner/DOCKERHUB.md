# lootcoin-miner

CPU miner for the [Lootcoin](https://github.com/mcrepeau/lootcoin) proof-of-work blockchain. Continuously assembles blocks, selects pending transactions by fee priority, and searches for a CubeHash-256 nonce that meets the current difficulty target.

Each mined block containing at least one transaction earns the miner a **lottery ticket** — a deferred probabilistic payout from the shared fee pot settled 110 blocks later.

## Quick start

```bash
docker run -d \
  -e MINER_ADDRESS=loot1… \
  -e NODE_URLS=http://your-node:3000 \
  mcrepeau79/lootcoin-miner:latest
```

`MINER_ADDRESS` is required. Generate an address with the [CLI wallet](https://github.com/mcrepeau/lootcoin/tree/main/lootcoin-wallet) (`lc new`) or the [web UI](https://github.com/mcrepeau/lootcoin/tree/main/lootcoin-web).

## Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `MINER_ADDRESS` | **Yes** | — | bech32m payout address (`loot1…`) |
| `NODE_URLS` | No | `http://127.0.0.1:3000` | Comma-separated node API URLs |
| `RUST_LOG` | No | `info` | Log filter |

When multiple node URLs are provided the miner tries each in order and uses the first that responds, giving automatic failover.

## How transaction selection works

- **Idle network** (≤ 200 pending transactions): include all transactions, sorted by fee descending.
- **Busy network** (> 200 pending transactions): gate by eligibility — a transaction with fee `f` becomes eligible after `(120 / f) − 1` blocks, so higher fees are included sooner.

The miner mines coinbase-only blocks when the mempool is empty, keeping the chain advancing and earning the block reward even without transactions.

## Cancellation

A background task subscribes to the node's `/events` SSE stream. The moment a new block arrives from another miner the current job is cancelled and fresh work is fetched, eliminating wasted hashing on stale blocks.

## Docker Compose

A ready-to-use setup (nodes + miner + faucet + web UI) is available in the [GitHub repository](https://github.com/mcrepeau/lootcoin).

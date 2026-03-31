# lootcoin-node

Full node for the [Lootcoin](https://github.com/mcrepeau/lootcoin) proof-of-work blockchain. Runs the consensus engine, stores the chain, serves the HTTP API, and synchronises with peers over Server-Sent Events.

## Quick start

```bash
docker run -d \
  -p 3000:3000 \
  -v lootcoin-data:/app/data \
  mcrepeau79/lootcoin-node:latest
```

On first boot the node creates a fresh chain from the hardcoded genesis block. On subsequent boots it replays from storage and syncs any missing blocks from peers.

## Connecting to an existing network

```bash
docker run -d \
  -p 3000:3000 \
  -v lootcoin-data:/app/data \
  -e PEERS=http://node1.example.com:3000,http://node2.example.com:3000 \
  -e NODE_URL=http://mynode.example.com:3000 \
  mcrepeau79/lootcoin-node:latest
```

`NODE_URL` is optional but recommended — it lets the node announce itself to peers so they can subscribe to its event stream.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `PORT` | `3000` | HTTP listen port |
| `PEERS` | _(none)_ | Comma-separated bootstrap peer URLs |
| `NODE_URL` | _(none)_ | This node's public URL, announced to peers on startup |
| `RUST_LOG` | `info` | Log filter (`debug`, `lootcoin_node=trace`, etc.) |

## Ports

| Port | Protocol | Description |
|---|---|---|
| `3000` | TCP | HTTP API and SSE event stream |

## Volumes

| Path | Description |
|---|---|
| `/app/data` | redb database — chain, balances, mempool, peers |

## Key API endpoints

| Endpoint | Description |
|---|---|
| `GET /chain/head` | Current height, difficulty, pot balance |
| `GET /balance/:address` | Confirmed and spendable balance |
| `GET /blocks?from=N&limit=N` | Fetch a range of blocks |
| `POST /transactions` | Submit a signed transaction |
| `POST /blocks` | Submit a mined block |
| `GET /events` | SSE stream of new blocks and transactions |
| `GET /metrics` | Prometheus metrics |

## Docker Compose

A ready-to-use multi-node setup (3 nodes + miner + faucet + web UI) is available in the [GitHub repository](https://github.com/mcrepeau/lootcoin).

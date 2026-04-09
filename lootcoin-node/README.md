# lootcoin-node

The node is at the heart of the Lootcoin network. It receives transactions from the wallet, blocks from miners, and synchronizes with other nodes. It implement the logic that governs the chain.

## Features

### Genesis

The chain has a single hardcoded genesis block shared by all nodes:

- **Timestamp** `1,748,000,000` (2025-05-23) — fixed so every node produces an identical genesis hash
- **Genesis address** `9bbec16bcab5f2d447eead5964d8e427aa9e35db490ca1ecd5ec872b35471f32`
- **Genesis wallet** receives 1,000,000 coins; the corresponding secret key is held by the chain operator and is never embedded in the binary
- **Lottery pot** seeded with 99,000,000 coins at genesis

---

### Gossip protocol

Nodes propagate blocks and transactions over **Server-Sent Events (SSE)** rather than a push-based peer protocol. When a node receives a new block or transaction it publishes a JSON event on an internal broadcast channel; every open `/events` connection receives it immediately.

**Peer subscriptions** — on startup, each node opens a persistent SSE connection to every peer in its list (`GET /peer-url/events`). Incoming events are validated and applied locally, then re-published to the node's own subscribers, propagating the event across the network. Reconnection uses exponential backoff (5 → 10 → 20 → 40 → 60 s, capped).

**Deduplication** — every node keeps a fixed-capacity seen-cache (10,000 entries, FIFO eviction) keyed by block hash and transaction ID (txid). An event already in the cache is silently dropped, preventing relay loops.

**Peer discovery** — nodes announce themselves to peers via `POST /peers` at startup. Peers are health-checked hourly with a `GET /chain/head` ping; peers that fail to respond are evicted after the grace period. The peer list is capped at 50 to bound the number of outbound connections.

**Sync on reconnect** — whenever a peer subscription reconnects after a drop, the node pulls any missing blocks from that peer before resuming the live stream, closing gaps that opened while the connection was down.

---

### Difficulty adjustment

Difficulty is adjusted **every block** using ASERT (Absolutely Scheduled Exponentially weighted Rolling Target). The target block time is `60 s`.

**Formula:**

```
new_difficulty = anchor_difficulty + (ideal_elapsed − actual_elapsed) / halflife
```

Where:
- `anchor_difficulty` — the difficulty recorded at block 1 (the first real mined block)
- `ideal_elapsed` — `(current_height − 1) × 60 s` (how long mining *should* have taken)
- `actual_elapsed` — `current_timestamp − anchor_timestamp` (how long it actually took)
- `halflife` — `3600 s` (1 hour): a sustained one-halflife deviation shifts difficulty by exactly 1 bit

Difficulty is expressed in **fractional bits** (e.g. `26.47`). A 0.65% deviation from target nudges difficulty by ~0.009 bits rather than forcing a full 1-bit step, so it converges to the exact value that produces the target block time for the current hashrate.

---

### Checkpoints and fast sync

As the chain grows, replaying every block from genesis on each restart becomes expensive. Lootcoin avoids this with a two-layer checkpoint system.

#### Local checkpoints

Every 1,000 blocks the node snapshots its full derived state — balances, pot, chain work, difficulty, pending lottery tickets — and writes it to the local database. On restart, the node loads the most recent snapshot and replays only the blocks that followed it, reducing startup time from O(chain length) to O(tail length).

The snapshot's block hash is verified against the live database on every load. If it doesn't match (e.g. after a deep reorg), the snapshot is discarded and the node falls back to a full replay.

#### Peer snapshot sync

A fresh node with no local history can bootstrap from a peer instead of replaying the entire chain. The trust model works as follows:

**Trust anchors** — `src/checkpoints.rs` contains a hardcoded list of `(height, block_hash)` pairs, attested by the chain operator. These are compiled into the binary and cannot be changed at runtime, the same model Bitcoin uses.

**Sync flow** — on first boot, the node queries every configured peer for their available snapshots (`GET /snapshots`). It finds the highest height where the peer's advertised hash matches a hardcoded trust anchor, downloads the full state payload (`GET /snapshot/{height}`), verifies the hash a second time, and applies it. Blocks from that height onward are then fetched normally via peer sync.

**Archive nodes** — set `ARCHIVE=1` to skip peer snapshot sync entirely and replay from genesis. Archive nodes serve the complete transaction history for any address, including history that predates any checkpoint.

**`history_start`** — the `/node/info` endpoint exposes the lowest block height for which this node has complete data. `0` means full archive; any other value means the node synced from a snapshot and cannot serve history before that height.

#### Adding a trust anchor

Once a block height is considered irreversible, add it to `src/checkpoints.rs`:

```rust
pub const TRUSTED_CHECKPOINTS: &[(u64, &str)] = &[
    (100_000, "0000a3f1..."),
];
```

Rebuild and redeploy. Nodes running this binary will now advertise and serve a snapshot at that height, and fresh peers will use it automatically.

---

### Transaction replay protection

Lootcoin uses **signature-based replay protection** rather than sequential per-sender nonces.

When `Transaction::new_signed` is called, a cryptographically random 64-bit nonce is generated and included in the signed message. Because the nonce is part of the signed payload, two calls with identical sender, receiver, amount, and fee still produce **distinct signatures**. The node tracks every confirmed transaction's Ed25519 signature in a `confirmed_signatures` set; a second submission of the same signature bytes is rejected as a replay.

**What this means for clients:**

- No coordination between clients is needed — the CLI and the browser wallet can both submit transactions simultaneously without fetching or tracking a `next_nonce`
- No "nonce conflict" errors when submitting from multiple devices
- The `GET /balance/:address` response only includes `balance` and `spendable_balance`; there is no `next_nonce` field

**Security properties:**

- The signed message includes a `CHAIN_ID` constant (`b"lootcoin-mainnet-1"`), so a valid signature produced for mainnet is invalid on any other chain
- Ed25519 signatures are deterministic given the same key and message; the random nonce ensures the signed message is unique even for otherwise identical transactions

---

### Hash function



The block hash covers `(index, previous_hash, timestamp, nonce, tx_root)` serialised with `bincode`, where `tx_root` is a CubeHash-256 digest of the serialised transaction list. Committing to `tx_root` rather than the full transaction list keeps the mined header fixed-size regardless of how many transactions are included.

---

### API

**GET /node/info**

Node-specific metadata. Unlike `/chain/head`, this response varies per node.

```json
{
  "version": "1.3.3",
  "history_start": 0,
  "node_url": "http://mynode.example.com:3000"
}
```

| Field | Description |
|---|---|
| `version` | Binary version compiled into the node |
| `history_start` | Lowest block height available on this node. `0` means the node has the full chain from genesis (archive node). A non-zero value indicates the node synced from a checkpoint and cannot serve blocks before that height |
| `node_url` | This node's public URL as set by `NODE_URL`, or `null` if not configured |

**GET /chain/head**

Current chain state.

```json
{
  "height": 142,
  "difficulty": 18.47,
  "latest_hash_hex": "0000a3f1...",
  "mempool_size": 3,
  "avg_block_time_secs": 61.4,
  "chain_work_hex": "00000000000000000000000000057a3c",
  "pot": 99901234
}
```

**GET /blocks?from=N&limit=N**

Returns up to `limit` blocks starting at height `from`. Each block includes a `lottery_payouts` field listing any pot payouts settled at that height.

**GET /balance/:address**

```json
{
  "balance": 10000,
  "spendable_balance": 9500
}
```

`balance` is the confirmed on-chain balance. `spendable_balance` subtracts any pending outgoing transactions currently in the mempool, giving the amount safely available to spend without risking a double-spend rejection.

**GET /address/:address/transactions?offset=N&limit=N**

Paginated transaction history for an address. Lottery payouts appear as entries with `sender: "lottery"`.

**GET /mempool**

All pending (unconfirmed) transactions.

**GET /mempool/fee-estimate?target_blocks=N**

Fee recommendation for getting a transaction mined within `target_blocks` blocks. `target_blocks` defaults to `0` (include in the very next block).

```json
{
  "target_blocks": 0,
  "utilization": 1.3,
  "recommended_fee": 145,
  "median_fee": 87
}
```

`utilization` is `pending_count / MAX_BLOCK_TXS`. Below `1.0` the network has spare capacity and any fee ≥ the minimum (`2`) gets included in the next block. At or above `1.0` the block is full and fee-tier eligibility gates inclusion.

`recommended_fee` is `MIN_TX_FEE` (`2`) when `utilization < 1.0`. When at or above capacity it is `max(⌈120 / (target_blocks + 1)⌉, cutoff_fee + 1)`, where `cutoff_fee` is the lowest fee among the top 240 pending transactions.

`median_fee` is the median fee across all pending transactions. `null` when the pool is empty.

**GET /lottery/recent-payouts?tier=<tier>&limit=N**

Most recent lottery payouts, newest first. Both parameters are optional.

`tier` filters by outcome tier: `small`, `medium`, `large`, or `jackpot`. Omit to return all tiers.
`limit` defaults to `10`, maximum `100`.

```json
[
  {
    "block_index": 1523,
    "block_timestamp": 1748123456,
    "receiver": "33693c36...",
    "amount": 499500,
    "tier": "jackpot"
  }
]
```

**GET /events**

Server-Sent Events stream. Emits a `block` event whenever a new block is accepted and a `transaction` event whenever a new transaction enters the mempool. Useful for real-time UIs and for miners that need to cancel stale work immediately.

**POST /blocks**

Submit a mined block (JSON-encoded `Block`). Returns `200` on acceptance, `400` on rejection with a reason string.

**POST /transactions**

Submit a signed transaction. Returns `200` on acceptance, `400` on rejection.

**GET /snapshots**

Lists checkpoint heights this node can serve as snapshots. Only heights that appear in the hardcoded trust anchors (`src/checkpoints.rs`) and are available in the local database are advertised.

```json
[
  {"height": 100000, "block_hash_hex": "0000a3f1..."}
]
```

**GET /snapshot/{height}**

Returns the full snapshot payload for a trusted checkpoint height. Used by fresh nodes to bootstrap without replaying from genesis. Returns `404` if the height is not a trusted checkpoint or has not yet been reached.

```json
{
  "height": 100000,
  "block_hash_hex": "0000a3f1...",
  "balances": {"loot1...": 50000},
  "pot": 98750000,
  "chain_work_hex": "00000000000000000000000000057a3c",
  "current_difficulty": 26.47,
  "asert_anchor": [1, 1748000060, 25.0],
  "tickets": [{"miner": "loot1...", "created_height": 99890}]
}
```

**GET /chain/block-hash/{height}**

Returns the hex-encoded block hash at a given height. Intended for chain operators who need to add a new entry to `src/checkpoints.rs`.

```json
{"height": 1000, "block_hash_hex": "0000a3f1..."}
```

Returns `404` if the height has not yet been reached or is not available on this node.

**GET /peers**

Known peer URLs.

**POST /peers**

Add a peer URL to the known-peers list.

---

## Running a node

```bash
# defaults: port 3000, no peers
cargo run -p lootcoin-node

# with peers and a public URL for peer discovery
PEERS=http://node1.example.com:3000,http://node2.example.com:3000 \
NODE_URL=http://mynode.example.com:3000 \
PORT=3000 \
cargo run -p lootcoin-node
```

On first boot the node creates `./data/` and initialises the database. Subsequent boots replay the chain from storage and sync any missing blocks from peers.

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `PORT` | `3000` | HTTP listen port |
| `PEERS` | _(none)_ | Comma-separated bootstrap peer URLs |
| `NODE_URL` | _(none)_ | This node's public URL, announced to peers |
| `ARCHIVE` | `0` | Set to `1` to force a full replay from genesis, skipping peer snapshot sync. Use this to run a complete archive node |
| `RUST_LOG` | `info` | Log filter (e.g. `debug`, `lootcoin_node=trace`) |

---

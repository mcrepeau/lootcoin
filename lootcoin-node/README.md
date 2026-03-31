# lootcoin-node

A proof-of-work blockchain where transaction fees don't go to miners — they accumulate in a **lottery pot** that pays out randomly to past miners.

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

## Gossip protocol

Nodes propagate blocks and transactions over **Server-Sent Events (SSE)** rather than a push-based peer protocol. When a node receives a new block or transaction it publishes a JSON event on an internal broadcast channel; every open `/events` connection receives it immediately.

**Peer subscriptions** — on startup, each node opens a persistent SSE connection to every peer in its list (`GET /peer-url/events`). Incoming events are validated and applied locally, then re-published to the node's own subscribers, propagating the event across the network. Reconnection uses exponential backoff (5 → 10 → 20 → 40 → 60 s, capped).

**Deduplication** — every node keeps a fixed-capacity seen-cache (10,000 entries, FIFO eviction) keyed by block hash and transaction signature. An event already in the cache is silently dropped, preventing relay loops.

**Peer discovery** — nodes announce themselves to peers via `POST /peers` at startup. Peers are health-checked hourly with a `GET /chain/head` ping; peers that fail to respond are evicted after the grace period. The peer list is capped at 50 to bound the number of outbound connections.

**Sync on reconnect** — whenever a peer subscription reconnects after a drop, the node pulls any missing blocks from that peer before resuming the live stream, closing gaps that opened while the connection was down.


## Difficulty adjustment

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

## Hash function

Lootcoin uses [CubeHash-256](https://en.wikipedia.org/wiki/CubeHash) instead of SHA-256. CubeHash is a NIST SHA-3 finalist designed to be simple, parallelisable, and resistant to length-extension attacks.

The block hash covers `(index, previous_hash, timestamp, nonce, tx_root)` serialised with `bincode`, where `tx_root` is a CubeHash-256 digest of the serialised transaction list. Committing to `tx_root` rather than the full transaction list keeps the mined header fixed-size regardless of how many transactions are included.

---

## Genesis

The chain has a single hardcoded genesis block shared by all nodes:

- **Timestamp** `1,748,000,000` (2025-05-23) — fixed so every node produces an identical genesis hash
- **Genesis address** `9bbec16bcab5f2d447eead5964d8e427aa9e35db490ca1ecd5ec872b35471f32`
- **Genesis wallet** receives 1,000,000 coins; the corresponding secret key is held by the chain operator and is never embedded in the binary
- **Lottery pot** seeded with 99,000,000 coins at genesis

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
| `RUST_LOG` | `info` | Log filter (e.g. `debug`, `lootcoin_node=trace`) |

---

## API

### `GET /chain/head`

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

### `GET /blocks?from=N&limit=N`

Returns up to `limit` blocks starting at height `from`. Each block includes a `lottery_payouts` field listing any pot payouts settled at that height.

### `GET /balance/:address`

```json
{
  "balance": 10000,
  "spendable_balance": 9500
}
```

`balance` is the confirmed on-chain balance. `spendable_balance` subtracts any pending outgoing transactions currently in the mempool, giving the amount safely available to spend without risking a double-spend rejection.

### `GET /address/:address/transactions?offset=N&limit=N`

Paginated transaction history for an address. Lottery payouts appear as entries with `sender: "lottery"`.

### `GET /mempool`

All pending (unconfirmed) transactions.

### `GET /mempool/fees`

Fee distribution across pending transactions.

```json
{
  "count": 42,
  "min": 1,
  "max": 120,
  "median": 12,
  "p25": 5,
  "p75": 60
}
```

All fields except `count` are `null` when the mempool is empty. Useful for wallets to detect whether the network is idle (`count ≤ 200`, in which case any fee gets in immediately) and to show relevant context when it is busy.

### `GET /lottery/recent-payouts?tier=<tier>&limit=N`

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

### `GET /events`

Server-Sent Events stream. Emits a `block` event whenever a new block is accepted and a `transaction` event whenever a new transaction enters the mempool. Useful for real-time UIs and for miners that need to cancel stale work immediately.

### `POST /blocks`

Submit a mined block (JSON-encoded `Block`). Returns `200` on acceptance, `400` on rejection with a reason string.

### `POST /transactions`

Submit a signed transaction. Returns `200` on acceptance, `400` on rejection.

### `GET /peers`

Known peer URLs.

### `POST /peers`

Add a peer URL to the known-peers list.

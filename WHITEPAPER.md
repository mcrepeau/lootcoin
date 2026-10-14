# Lootcoin: A Peer-to-Peer Electronic Cash System with a Provably-Fair Block Lottery

**Abstract.** A purely peer-to-peer version of electronic cash would allow payments to be sent
directly from one party to another without going through a financial institution. This paper
describes Lootcoin, a proof-of-work blockchain that extends the Bitcoin incentive model with a
**delayed lottery**: every block that carries transactions grants its miner a ticket, settled one
hundred blocks later against the accumulated entropy of the block hashes that followed it. Each
ticket draws against a communal pot funded by half of every transaction fee, with payouts that are
flat fractions of the pot — small wins are common, jackpots are rare. The reveal window doubles
as both the maturity delay and the randomness source, so an attacker must control one hundred
consecutive blocks to steer a single draw; at thirty percent of the network hashrate the
probability of doing so is approximately 10^-52. The lottery's purpose is bootstrapping: the pot
is deliberately seeded large at genesis so that early mining is richly rewarded, tapering
continuously as the pot equilibrates, with a tiered payout structure — frequent small awards,
rare large ones — that keeps participant rewards tangible and legible. We weigh the
design against Bitcoin's deterministic fee payout honestly: it buys an inclusion incentive
decoupled from fee size and a smoothed early subsidy, at the cost of reward variance, a blunted
fee market, and a permanently inflationary coinbase. The system uses CubeHash-256 in place of
SHA-256, per-block ASERT difficulty retargeting with fractional-bit precision, and
most-accumulated-work fork selection.

---

## 1. Introduction

Commerce on the Internet has come to rely almost exclusively on financial institutions serving as
trusted third parties. Bitcoin removed the need for a trusted third party to process payments,
using proof-of-work to replace trust with computation. Lootcoin builds on that foundation and
asks a further question: beyond the block subsidy, *how should transaction fees be paid out?*

In Bitcoin, the miner of a block collects all of its transaction fees directly. The reward is
immediate, local, and finished — the miner has no stake in what the network does next. In
Lootcoin, each block's fees are split evenly: half is paid to the miner immediately, and half
accrues to a communal lottery pot. In return, any block that includes at least one transaction
earns its miner a ticket that is settled 100 blocks later against the hashes of the hundred blocks
that followed it. The payout is a probabilistic fraction of the pot at settlement time.

The design's intent is bootstrapping and attention, and it is worth stating plainly. A young
chain is a fragile thing: it needs hashrate before it has users, and a sober fee schedule pays
almost nothing until it has both. The seeded pot solves the cold-start problem by paying early
miners generously, with payouts that taper as the pot equilibrates. And the lottery's shape —
frequent small payouts, rare large ones, provably unriggable draws — turns mining's abstract
statistical reward into something a participant can watch settle, block by block, on any
explorer. The expected payout is modest; the engagement is not. We
examine this trade against Bitcoin's reward structure honestly in section 7.4.

Lootcoin is implemented as a small set of Rust services (node, miner, faucet, wallet, web
explorer) over a shared `lootcoin-core` library. This paper describes the protocol as implemented
by that code.

## 2. Transactions

We define an electronic coin as a chain of transfers of a global balance state. Formally, a
Lootcoin transaction is a signed message:

```
Transaction = (sender, receiver, amount, fee, nonce, public_key, signature)
```

Each transfer is authenticated by an **Ed25519 signature** over the tuple
`(CHAIN_ID, sender, receiver, amount, fee, nonce)`, where `CHAIN_ID` is the constant
`"lootcoin-mainnet-1"` and `nonce` is a fresh random value generated at signing time (a 53-bit
value, so it round-trips losslessly through JSON).
Two properties follow directly from this construction:

1. **No replay across chains.** The chain identifier is inside the signed message, so a valid
   mainnet signature is invalid on any other chain.
2. **No coordination for replay protection.** Because the nonce is random and signed, two
   otherwise identical transfers produce distinct signatures. The node identifies every confirmed
   transaction by its signature bytes and rejects any resubmission of a signature it has already
   seen. There are no sequential per-sender account nonces, so multiple devices (a CLI wallet and
   a browser wallet) can spend from the same address concurrently without fetching a "next
   nonce" from anywhere.

Addresses are **bech32m** encodings of a 32-byte digest of the public key, prefixed `loot1…`,
giving checksummed, typo-resistant identifiers. Balances are integer **coins** held by address;
there is no account nonce and no partial spends — a transaction debits `amount + fee` from the
sender and credits `amount` to the receiver in one step. The minimum fee is 2 coins.

Wallet keys are derived from a 12-word **BIP-39** mnemonic via **SLIP-0010** for Ed25519, along the
BIP-44 path `m/44'/4103'/0'/0'/0'` (coin type 4103 is Lootcoin's registered SLIP-0044 type), so a
single recovery phrase restores the wallet on any device.

## 3. Blocks and the Timestamp Server

The block is the unit of commitment. A block commits to a fixed-size header:

```
Block = (index, previous_hash, timestamp, nonce, tx_root, transactions[], hash)
```

`tx_root` is a CubeHash-256 digest of the serialized transaction list, computed once when the
block is assembled. Because the mined header commits to `tx_root` rather than the transaction
list, the inner loop of the proof-of-work search hashes the same small header regardless of how
many transactions the block carries — mining speed is independent of block size, exactly the
role the Merkle root plays in Bitcoin. A block is valid only if `hash` equals the CubeHash-256
digest of the bincode-serialized `(index, previous_hash, timestamp, nonce, tx_root)` header and
`tx_root` matches the transaction list.

The chain of hashes forms a timestamp server in the Bitcoin sense: each block's hash commits to
its predecessor, making it computationally infeasible to alter history without redoing the work.
Two timestamp rules bound a miner's freedom:

- **Median Time Past.** A block's timestamp must exceed the median of the last 11 block
  timestamps, preventing backdating attacks while tolerating clock skew.
- **Future bound.** Blocks more than 120 seconds ahead of wall-clock time are rejected outright.

Exactly one transaction per block — the first — may have an empty sender. This is the
**coinbase**, and it mints exactly 1 coin to the miner's address (or 0). Any later transaction
with an empty sender is rejected, so no one can mint coins from thin air. A block carries at most
240 non-coinbase transactions, for a sustained throughput of 4 transactions per second at the
target block time.

## 4. Proof-of-Work

To implement a distributed timestamp server, we use a proof-of-work system similar to Hashcash.
Lootcoin replaces SHA-256 with **CubeHash-256**, a NIST SHA-3 finalist chosen for its simplicity,
parallelisability, and resistance to length-extension attacks.

A block satisfies difficulty *d* — a **fractional** number of leading zero bits — if its hash,
read as a big-endian integer, is less than 2^(256−d). Fractional bits matter: whole-bit
difficulty granularity forces a 2× jump in expected work per adjustment, producing a permanent
oscillation around the target block time. Lootcoin instead compares the boundary byte against a
real-valued threshold `2^(8−remainder)`, descending into the fractional part byte-by-byte when
the hash exactly equals the threshold's floor. The comparison uses only IEEE-754-exact
operations on modern platforms (non-transcendental arithmetic plus a pinned `2^x`), so every node
reaches the same accept/reject decision bit-for-bit.

### 4.1 Per-block difficulty: ASERT

Difficulty is not retargeted in windows; it is recomputed **every block** with ASERT
(Absolutely Scheduled Exponentially weighted Rolling Target):

```
new_difficulty = anchor_difficulty + (ideal_elapsed − actual_elapsed) / halflife
```

- `anchor` = (height, timestamp, difficulty) of block 1 — the first real mined block, chosen so
  that the synthetic genesis timestamp can never poison the calculation;
- `ideal_elapsed` = (height − 1) × 60 s;
- `actual_elapsed` = timestamp − anchor timestamp;
- `halflife` = 3600 s.

The formula is linear in **log₂ space**: difficulty is expressed in bits, so a sustained
deviation of one halflife (one hour) moves difficulty by exactly 1 bit — a doubling of expected
hashes. If hashrate doubles, difficulty catches up within an hour of mining; if the network goes
dark, difficulty falls one bit per hour of silence. There is no retarget window to exploit and no
difficulty oscillation between windows. Difficulty is clamped to [8, 127] bits and starts at 25
bits at genesis.

### 4.2 Chain selection

The honest chain is the one with the most **accumulated work**, Σ 2^(difficultyᵢ), where each
block's contribution is taken from the *actual* leading zero bits of its hash rather than the
target — a block that happens to find extra zeros credits the extra work it proves. Work-based
selection prevents a peer from manipulating fork choice by lying about block counts. Competing
fork blocks are held in a bounded orphan pool (at most 10 heights deep, 3 orphans per height)
and admitted only if they carry more accumulated work than the main chain from the fork point.
A reorg reverts every state change — balances, the pot, and pending lottery tickets — and
replays the winning fork from the divergence point, re-adding still-valid displaced transactions
to the mempool.

## 5. Network

The network runs a deliberately simple propagation model. Every node exposes an HTTP API and a
**Server-Sent Events (SSE)** stream, `/events`, on which it publishes every accepted block and
every relayed transaction as a JSON event. Nodes form a gossip mesh:

1. New transactions are submitted to any node's `/transactions` endpoint. They are validated
   (signature, fee ≥ 2, replay check, spendable balance), admitted to the mempool, and
   published on the event stream.
2. Every node holds a persistent SSE subscription to each of its peers and applies incoming
   events locally, re-publishing them to its own subscribers. Reconnection uses exponential
   backoff, and a node that reconnects pulls the blocks it missed before resuming the live
   stream.
3. Every event passes through a fixed-capacity (10,000-entry, FIFO) seen-cache keyed by block
   hash and transaction id; duplicates are dropped, which severs relay loops without letting an
   attacker flush the cache and re-inject old messages.
4. To reach peers that cannot hold inbound SSE connections (NAT), nodes also fire-and-forget
   every accepted block to each peer's `/blocks` endpoint.

Nodes announce themselves to peers at startup, ping peers hourly, evict peers that stay silent,
and cap the peer list at 50. Miners subscribe to the same event stream: the moment a block event
arrives, every miner still working on the previous height cancels its search and fetches new
work, so stale work is abandoned at once rather than after a poll interval.

The mempool is bounded and adversarially sized: at most 10,000 transactions, at most 25 pending
per sender, and pending transactions expire after 100 blocks. When demand exceeds the 240-transaction
block, miners include highest-fee-first, gated by a fee-tier rule: a transaction paying fee `f`
becomes includable after `⌈120/f⌉ − 1` blocks — a fee of 120 is includable immediately; a fee
of 2 waits about an hour. This is a lightweight fee market with a deterministic aging rule —
no auctions, no side agreements, no out-of-band negotiation.

## 6. Incentive

The Bitcoin incentive framework applies: the first transaction in a block mints 1 new coin to
the miner, and transaction fees flow to whoever includes them. Nodes are incentivized to stay
honest because block rewards are only realizable on the chain the network keeps.

Lootcoin splits this in two. For every block, the sum of its transaction fees is divided evenly:
half is paid to the block's miner as immediate income, and half is added to the **lottery pot**.
The pot is seeded with 99,000,000 coins at genesis (alongside a 1,000,000-coin genesis wallet, for
a 100,000,000-coin initial supply). From then on the pot is replenished exclusively by fee
halves.

This 50/50 split is the load-bearing choice. It preserves the direct fee incentive — a miner
still keeps half of every fee it includes, so fee maximisation is unchanged — while converting
the other half into a delayed, shared, probabilistic payout described next.

## 7. The Lottery

The lottery is Lootcoin's contribution to the incentive design. It needs three things to be sound:
a **ticket** that cannot be forged, a **randomness source** that cannot be steered, and a
**payout schedule** that cannot drain the pot. We address each in turn.

### 7.1 Tickets

When a block containing at least one non-coinbase transaction is applied, the protocol issues
the block's miner a ticket `(miner, created_height)`. Tickets are protocol state, not
transactions: they are carried in the consensus state, snapshotted in checkpoints, and reverted
on reorgs like any other state. Blocks with only a coinbase earn no ticket — a block that adds
nothing to the pot has no claim on it.

### 7.2 Randomness: the reveal window

At block height `H + 100`, the ticket from block `H` settles. The draw uses as entropy the
concatenated hashes of all 100 blocks `[H, H+100)` — every block mined *after* the ticket was
earned and *before* the settle block:

```
digest  = CubeHash256( entropy ‖ serialize(created_height, miner) )
bucket  = u64_le(digest[0..8]) mod 1,000,000
```

The reveal window serves double duty as both maturity delay and randomness source, and this is
what makes the draw **provably fair**. To steer the outcome of a ticket minted at block H, an
attacker must control *all one hundred* consecutive blocks H…H+99 — including the blocks mined
by everyone else in the hundred blocks after the attack begins. At 30% hashrate this has
probability 0.3^100 ≈ 10^-52. The settling miner of block H+100 controls nothing: every entropy
hash was committed before its turn, and the modulus draw adds the miner's own address to the
digest so different miners' tickets at the same height settle independently.

Eight bytes of the digest are reduced modulo 1,000,000. With a 64-bit reduction the modular bias
is about 3 × 10^-11 — negligible — whereas a 32-bit reduction would leave a ~0.023% tilt toward
the bottom bucket.

### 7.3 Payout schedule

Each ticket makes one draw against the pot *at settlement time*:

| Bucket (mod 10^6) | Probability | Tier | Payout |
|---|---|---|---|
| 0 – 619,999 | 62.00% | none | — |
| 620,000 – 982,499 | 36.25% | small | pot / 400,000 |
| 982,500 – 999,199 | 1.67% | medium | pot / 30,000 |
| 999,200 – 999,899 | 0.07% | large | pot / 2,000 |
| 999,900 – 999,999 | 0.01% | jackpot | pot / 500 |

Two structural properties follow. First, payouts are **flat fractions of the current pot**,
independent of the ticket's block contents, so the pot can never be drained by a single draw and
trends toward an equilibrium set by fee inflow versus payout outflow. Second, the schedule is
deliberately skewed: a small win (pot/400,000 — about 247 coins at the genesis-seeded level)
arrives every few blocks, while a jackpot (pot/500 — 198,000 coins) arrives about weekly. The
expected drain per ticket is pot × (0.3625/400000 + 0.0167/30000 + 0.0007/2000 + 0.0001/500)
≈ pot × 2.0 × 10^-6 — about 199 coins per ticket at the seeded level, roughly matching the pot's
fee inflow in an active network.

Winning payouts are credited to the miner's balance as part of applying the settle block, so they
inherit full reorg safety; a reorg that displaces the settle block reverts the payout and the
ticket returns to the pending set. No-win draws produce no record at all.

### 7.4 Comparison with Bitcoin's reward structure

Bitcoin pays its miners deterministically: a block subsidy plus 100 percent of the block's
transaction fees, collectable immediately. Lootcoin divides roughly the same aggregate income
into a deterministic part (the coinbase plus half of the fees) and a probabilistic part (the
lottery, funded by the other half of the fees plus the genesis pot). The lottery is therefore
not free value — it is the fee stream redistributed with delay and noise added, combined with a
slow release of the pot.

The lottery's intent is not purely economic. Mining a proof-of-work
chain already delivers a statistical, deferred reward, but an abstract one: a small miner sees
nothing for weeks and then a lump sum. The lottery makes the statistics legible: every
transaction-bearing block earns a ticket with a tiered payoff — frequent small awards, rare
large ones — settled by committed entropy no one can rig. The mechanic is the "loot drop" of
multiplayer games, and the name follows: a Lootcoin miner receives loot for defeating a block,
in a rarity tier determined by committed entropy rather than a publisher's drop table. The expected
payout is modest, but this visible, ongoing reward cadence is the mechanism by which Lootcoin hopes
to recruit hashrate that a flat fee schedule would not.


## 8. Reclaiming Disk Space

Once the latest transaction in a block is buried under enough blocks, the spent transactions can
be discarded to save disk space. Lootcoin keeps only a 100-block sliding window of full blocks
in memory — exactly the size of the reveal window, so settlement always finds its entropy
in-window. Older blocks live in an embedded local database (redb).

To avoid replaying the entire chain on restart, the node snapshots its full derived state —
balances, the pot, chain work, difficulty, the ASERT anchor, and all pending tickets — every
1,000 blocks. On restart it verifies the snapshot's block hash against the stored chain, loads
the snapshot, and replays only the tail. Boot thus costs O(tail), not O(chain).

## 9. Simplified Payment Verification and Fast Sync

A fresh node without history can bootstrap from peers. The trust model mirrors Bitcoin's
hardcoded checkpoints: a compiled-in list of `(height, block_hash)` pairs attested by the chain
operator. On first boot a node asks its peers for available state snapshots, selects the highest
height whose advertised hash matches one of its hardcoded trust anchors, downloads the state
payload, re-verifies the hash, and applies it. The blocks from that height forward arrive by
normal peer sync. Nodes that synced this way advertise their `history_start` height so light
clients know where complete history begins; **archive nodes** (`ARCHIVE=1`) skip snapshot sync
entirely and serve the full history from genesis.

Without that assurance, an SPV-style client would be adequate: a payment is verified by checking
the chain of headers — each carrying the accumulated-work figure and the ASERT difficulty — and
confirming that the transaction's block is buried under enough subsequent headers, without
needing the block's contents.

## 10. Privacy

The Bitcoin privacy model applies unchanged: the public is the witness. All transactions are
broadcast publicly, so the traditional banking model of limited-information disclosure cannot
be replicated. Addresses are hashes of public keys and users should use a fresh address per
payment; because Lootcoin has no per-address nonces and no account chains, nothing about an
address's transaction *ordering* is ever disclosed — the random signed nonce that prevents replay
reveals nothing about sequence. As with Bitcoin, some information leakage is unavoidable:
multi-input transactions reveal a common owner, and value flows can be traced.

## 11. Calculations

The lottery's security rests on the reveal window. Consider an attacker holding fraction *q* of
the network hashrate who tries to steer a ticket minted at height H. The draw uses the hashes of
blocks H through H+99. The attacker must be the miner of *all* of them — any block mined by an
honest miner injects entropy outside the attacker's control. The probability of the attacker
mining 100 consecutive blocks is q^100:

| Attacker hashrate q | q^100 |
|---|---|
| 10% | 10^-100 |
| 30% | ~5 × 10^-53 |
| 50% | ~8 × 10^-31 |

At any hashrate below majority, steering even a *single* draw is infeasible; the lottery's
integrity does not depend on attacker honesty, only on attacker hash. Contrast this with a
shorter window: at 20 blocks, a 30% attacker steers one draw in 3.5 × 10^-11; at 100 blocks, one
in 2 × 10^52.

The fee-market guarantee follows from the eligibility rule. A transaction paying fee `f` becomes
includable after `⌈120/f⌉ − 1` blocks and, under normal load, lands within `⌈120/f⌉`, since blocks hold 240
transactions and each pending transaction's eligibility is a pure function of its fee and age —
every miner computes the same eligible set and includes highest-fee-first, so no coordination
is required.

The pot's expected outflow per ticket is:

```
E[payout] = pot × ( 0.3625/400000 + 0.0167/30000 + 0.0007/2000 + 0.0001/500 )
          ≈ pot × 2.0 × 10^-6
```

At the seeded level (99,000,000 coins) this is ~199 coins per settled ticket, against an inflow
of half of all fees — one ticket is minted per transaction-bearing block, so the pot reaches
equilibrium where the average fee halve-flow per block matches the average expected payout per
ticket.

## 12. Conclusion

Lootcoin is a proof-of-work electronic cash system that keeps Bitcoin's decentralised timestamp
server and replaces its fee settlement with a delayed, provably-fair lottery. Fees are split:
half now to the miner, half into a pot drawn one hundred blocks later by every transaction-bearing
block, against entropy that no one miner can influence. CubeHash-256 hashing, per-block ASERT
difficulty in fractional bits, most-accumulated-work fork selection, signature-based replay
protection, and state checkpoints keep the machinery small, deterministic, and fast to sync. The
result is a chain where the incentive to mine is not merely to collect the fees in front of you,
but to keep the chain itself alive — because your own winnings are written by the hundred blocks
that come after you.

---

*Protocol parameters as implemented in `lootcoin-core` 3.2 and the `lootcoin` workspace (node,
miner, faucet, wallet, web). See the repository READMEs for configuration and API references.*

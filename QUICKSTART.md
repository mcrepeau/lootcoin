# Quickstart

Get a local Lootcoin chain running, mine your first block, and send coins in under 15 minutes.

**Prerequisites:** [Docker](https://docs.docker.com/get-docker/)

**Optional:** [Rust](https://www.rust-lang.org/tools/install) for the `lc` CLI.

> **First build takes ~10 minutes** (Rust compilation). Subsequent starts take ~30 seconds.

## 1. Generate a wallet

You need a `loot1…` address before starting. The `lc` CLI handles this locally — no node required.

**If you have Rust installed:**

```bash
cargo install lootcoin-wallet --bin lc
lc new
```

Save the recovery phrase somewhere safe. Note the address printed (`loot1…`).

Then get the secret key from the wallet file:

```bash
# macOS / Linux
cat ~/.lootcoin/wallet.json | python3 -c "import sys, json; print(json.load(sys.stdin)['secret_key_hex'])"
```

**If you don't have Rust:** skip to step 3, start the stack with a placeholder, then visit
`http://localhost:8888` → Wallet → New Wallet to generate an address in the browser. Stop the
stack, update `.env`, and restart. Come back to this step once the stack is running.

---

## 2. Configure

```bash
cp .env.example .env
```

Edit `.env` and fill in both values:

```
FAUCET_SECRET_KEY=<paste the secret_key_hex from step 1>
MINER_ADDRESS=<paste your loot1… address from step 1>
```

Using the **same wallet** for both miner and faucet means that every coinbase reward and
lottery prize mined goes straight into the faucet's balance — no manual seeding needed.

---

## 3. Start the stack

```bash
docker compose up --build -d
```

Wait for the node to initialise (~30 seconds after the build finishes):

```bash
docker compose logs -f node1 | grep -m1 "chain initialised\|listening"
```

Press `Ctrl+C` once you see it. The chain is now live.

---

### What's running

| Service | URL | Description |
|---|---|---|
| node1 | http://localhost:3001 | Primary node (API + chain) |
| node2 | http://localhost:3002 | Peer node |
| node3 | http://localhost:3003 | Archive node |
| Web UI | http://localhost:8888 | Wallet, explorer, faucet page |
| Faucet | http://localhost:3030 | Coin dispenser |
| Prometheus | http://localhost:9090 | Raw metrics |
| Grafana | http://localhost:3100 | Metrics dashboard |

---

## 4. Check the chain

```bash
lc status --node http://localhost:3001
```

```
Height:         1
Difficulty:     18.00 bits
Avg block time: 0.0 s
Mempool:        0 pending tx(s)
Lottery pot:    99000000 coins
```

Or open the web UI at **http://localhost:8888**.

---

## 5. Wait for your first block

The miner starts automatically and earns **1 coin per block** (coinbase) plus random
**lottery prizes** from the 99M-coin pot. A small prize (≈247 coins) arrives on average
every 3 blocks.

Watch blocks arrive in real time:

```bash
docker compose logs -f miner | grep "Block accepted\|Found nonce"
```

After 2–3 blocks (~2–3 minutes) your miner address has coins. Check the balance:

```bash
lc balance --node http://localhost:3001
```

---

## 6. Use the faucet

Once the faucet wallet has a spendable balance, you can request coins for any address.
Generate a second wallet to send to:

```bash
lc new --wallet /tmp/wallet2.json
ADDR2=$(lc address --wallet /tmp/wallet2.json)
```

Request coins from the faucet:

```bash
curl -s -X POST http://localhost:3030/faucet \
  -H "Content-Type: application/json" \
  -d "{\"address\": \"$ADDR2\"}" | python3 -m json.tool
```

If the faucet responds with `"insufficient balance"`, wait another minute for the next
lottery prize and try again. Check faucet status at any time:

```bash
curl -s http://localhost:3030/status | python3 -m json.tool
```

---

## 7. Send a transaction

Once the faucet has dispensed to `$ADDR2`, send some coins back:

```bash
lc send $(lc address) 50 --fee 12 --node http://localhost:3001 --wallet /tmp/wallet2.json
```

A fee of 12 means the transaction becomes eligible for the next block (~9 minutes wait).
Use `--fee 120` to include it in the very next block.

Check the history once it confirms:

```bash
lc history --node http://localhost:3001
```

---

## 8. Open the Grafana dashboard

Visit **http://localhost:3100** for live chain metrics: pot balance, block time, fee flow,
mempool size, and lottery wins by tier.

No login required.

---

## Stopping and restarting

```bash
# Stop everything (data is preserved in Docker volumes)
docker compose down

# Restart (fast — no rebuild)
docker compose up -d

# Full reset (deletes all chain data)
docker compose down -v
```

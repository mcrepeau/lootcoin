# lootcoin-faucet

Testnet faucet for the [Lootcoin](https://github.com/mcrepeau/lootcoin) blockchain. Holds a funded wallet and dispenses a fixed amount of coins to any valid address, with a per-address cooldown to prevent abuse.

## Quick start

```bash
docker run -d \
  -p 3030:3030 \
  -e FAUCET_SECRET_KEY=<64-char-hex-seed> \
  -e NODE_URL=http://your-node:3000 \
  mcrepeau79/lootcoin-faucet:latest
```

The faucet's address is derived from `FAUCET_SECRET_KEY` and logged at startup. Fund that address before the faucet can dispense coins.

## Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `FAUCET_SECRET_KEY` | **Yes** | — | 32-byte wallet seed as 64 hex characters |
| `NODE_URL` | No | `http://127.0.0.1:3000` | Base URL of a lootcoin node |
| `PORT` | No | `3030` | HTTP listen port |
| `DISPENSE_AMOUNT` | No | `500` | Coins sent per request |
| `DISPENSE_FEE` | No | `2` | Transaction fee (minimum is 2) |
| `COOLDOWN_SECS` | No | `86400` | Seconds between dispenses per address |

## Ports

| Port | Protocol | Description |
|---|---|---|
| `3030` | TCP | HTTP API |

## API

### `POST /faucet`

```bash
curl -X POST http://localhost:3030/faucet \
  -H "Content-Type: application/json" \
  -d '{"address": "loot1…"}'
```

```json
{ "message": "Sent 500 coins to your address.", "amount": 500 }
```

| Status | Meaning |
|---|---|
| `200` | Coins sent |
| `400` | Invalid address |
| `429` | Cooldown active — response includes minutes remaining |
| `503` | Node unreachable or faucet balance too low |

### `GET /status`

Returns the faucet address, spendable balance, and current configuration.

## Docker Compose

A ready-to-use setup (nodes + miner + faucet + web UI) is available in the [GitHub repository](https://github.com/mcrepeau/lootcoin).

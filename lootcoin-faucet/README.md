# lootcoin-faucet

HTTP faucet service for the [Lootcoin](https://github.com/mcrepeau/lootcoin) testnet. It holds a funded wallet and dispenses a fixed amount of coins to any valid address, with a per-address cooldown to prevent abuse.

## API

**GET /status**

Returns the faucet's current configuration and balance.

```json
{
  "faucet_address": "loot1…",
  "spendable_balance": 12500,
  "dispense_amount": 500,
  "fee": 2,
  "cooldown_hours": 24
}
```

`spendable_balance` is `null` when the upstream node is unreachable.

---

**POST /faucet**

Request coins for an address.

**Request body:**

```json
{ "address": "loot1…" }
```

**Response (200 OK):**

```json
{ "message": "Sent 500 coins to your address.", "amount": 500 }
```

**Error responses:**

| Status | Reason |
|--------|--------|
| `400 Bad Request` | Address is not a valid bech32m lootcoin address |
| `429 Too Many Requests` | Address was funded recently; response includes minutes remaining |
| `503 Service Unavailable` | Node unreachable or faucet balance too low |

## Configuration

All configuration is via environment variables.

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `FAUCET_SECRET_KEY` | Yes | — | 32-byte faucet wallet seed as 64 hex characters |
| `NODE_URL` | No | `http://127.0.0.1:3000` | Base URL of a lootcoin node |
| `PORT` | No | `3030` | Port to listen on |
| `DISPENSE_AMOUNT` | No | `500` | Coins sent per request |
| `DISPENSE_FEE` | No | `2` | Transaction fee deducted from faucet balance (minimum is 2) |
| `COOLDOWN_SECS` | No | `86400` | Cooldown between dispenses per address (seconds) |

The faucet wallet's address is derived from `FAUCET_SECRET_KEY` and logged at startup. Fund that address to make the faucet operational.

## Running

### With Docker Compose (recommended)

The faucet is included in the root `docker-compose.yml`:

```yaml
faucet:
  build: ./lootcoin-faucet
  ports:
    - "3030:3030"
  environment:
    NODE_URL: "http://node1:3000"
    FAUCET_SECRET_KEY: "<64-char hex seed>"
    DISPENSE_AMOUNT: "500"
    DISPENSE_FEE: "2"
    COOLDOWN_SECS: "86400"
```

### Standalone

```bash
FAUCET_SECRET_KEY=<hex> NODE_URL=http://127.0.0.1:3000 cargo run --release
```

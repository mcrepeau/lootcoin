# lootcoin-wallet

Rust crate with two outputs:

- **`lc`** — CLI wallet for sending transactions, checking balances, and managing keys from the terminal.
- **WASM library** — compiled to WebAssembly for use in the browser (`lootcoin-web`).

Both share the same key generation, BIP-39 mnemonic support, address derivation, and transaction signing logic from `lootcoin-core`. Private keys never leave the machine — all cryptographic operations happen locally.

---

## CLI wallet (`lc`)

### Building

```bash
cargo build --release --bin lc
```

The binary is placed at `target/release/lc` (or `lc.exe` on Windows). Copy it anywhere on your `PATH`.

### Configuration

| Method | Variable | Default |
|---|---|---|
| `--node <URL>` or env | `LOOTCOIN_NODE` | `http://127.0.0.1:3000` |
| `--wallet <PATH>` or env | `LOOTCOIN_WALLET` | `~/.lootcoin/wallet.json` |

The wallet file stores the secret key and mnemonic as JSON. Keep it private.

### Commands

#### `lc new`

Generate a new wallet and print the 12-word recovery phrase.

```
$ lc new
Wallet saved to /home/alice/.lootcoin/wallet.json

Address:  loot1...

Recovery phrase — write this down and keep it safe:

  word1 word2 word3 word4 word5 word6 word7 word8 word9 word10 word11 word12

Anyone with this phrase can spend your coins.
```

#### `lc import [PHRASE]`

Restore a wallet from a 12-word recovery phrase. If the phrase is omitted, you are prompted for it.

```bash
lc import "word1 word2 ... word12"
```

#### `lc address`

Print the wallet address.

```
$ lc address
loot1...
```

#### `lc balance [ADDRESS]`

Show confirmed and spendable balance. Omit the address to use the wallet address.

```
$ lc balance
Address:   loot1...
Balance:   1500 coins
Spendable: 1388 coins  (112 pending in mempool)
```

#### `lc send <RECEIVER> <AMOUNT> [--fee <FEE>]`

Sign and broadcast a transaction. Prompts for confirmation before submitting.

```
$ lc send loot1... 100 --fee 12
From:   loot1...
To:     loot1...
Amount: 100 coins
Fee:    12 coins  (total debit: 112)
Wait:   ~9 blocks (~9 min) before miners can include this tx

Confirm? [y/N] y
Transaction submitted.
```

The `--fee` flag defaults to `2` (the network minimum). Higher fees reduce the number of blocks miners must wait before including the transaction:

| Fee | Eligible after |
|---|---|
| 2 | ~59 blocks (~1 hour) |
| 12 | ~9 blocks (~9 min) |
| 120+ | immediately |

#### `lc history [ADDRESS] [--limit N]`

Show transaction history (default 20 most recent). Omit the address to use the wallet address.

```
$ lc history --limit 5
BLOCK     TYPE     AMOUNT        FEE  COUNTERPART
────────────────────────────────────────────────────────────────────────
4821      OUT          -100        12  loot1abcd…ef012345
4800      LOTTERY    +3300         0
4750      IN           +50         2  loot1wxyz…ab678901
4700      REWARD        +1         0
4650      OUT          -200         5  loot1qrst…cd234567
```

Row types: `REWARD` (coinbase), `LOTTERY` (prize payout), `IN` (received), `OUT` (sent).

#### `lc status`

Show current chain state from the connected node.

```
$ lc status
Height:         5102
Difficulty:     26.34 bits
Avg block time: 61.2 s
Mempool:        3 pending tx(s)
Lottery pot:    94823110 coins
```

---

## WASM library

### Prerequisites

Install [`wasm-pack`](https://rustwasm.github.io/wasm-pack/installer/):

```bash
cargo install wasm-pack
```

### Building

From the `lootcoin-wallet` directory:

```bash
wasm-pack build --target web
```

This generates a `pkg/` directory containing:

| File | Description |
|---|---|
| `lootcoin_wallet.js` | JavaScript glue code (ES module) |
| `lootcoin_wallet_bg.wasm` | Compiled WebAssembly binary |
| `lootcoin_wallet.d.ts` | TypeScript type definitions |

The `lootcoin-web` Docker image builds this automatically. For local development, copy the output to `lootcoin-web/`:

```bash
cp pkg/lootcoin_wallet.js pkg/lootcoin_wallet_bg.wasm ../lootcoin-web/
```

### JavaScript API

```js
import init, { Wallet } from "./lootcoin_wallet.js";
await init();

// Create a new wallet
const wallet = new Wallet();
wallet.mnemonic_phrase();    // "word1 word2 … word12"
wallet.address();            // bech32m address (loot1…)
wallet.secret_key_hex();     // 32-byte secret key as hex

// Restore from mnemonic
const wallet = Wallet.from_mnemonic("word1 word2 … word12");

// Sign a transaction (returns JSON ready to POST to /transactions)
const txJson = wallet.sign_transaction(receiverAddress, amount, fee);
```

`mnemonic_phrase()` returns `undefined` for wallets imported via `from_secret_key_hex` — there is no phrase to recover in that case.

---

## Key derivation

| Step | Detail |
|---|---|
| Entropy | 16 bytes from OS CSPRNG (WebCrypto in browser, OsRng in CLI) |
| Mnemonic | BIP-39, 12 words |
| Seed | PBKDF2-HMAC-SHA512, no passphrase, 2048 rounds |
| Master key | SLIP-0010: HMAC-SHA512(Key=`"ed25519 seed"`, Data=seed) |
| Derivation path | `m/44'/4103'/0'/0'/0'` — all components hardened (SLIP-0010 ed25519 requirement) |
| Coin type | 4103 (`0x1007`) — Lootcoin's SLIP-0044 registered coin type |
| Key | IL (first 32 bytes of final HMAC) → Ed25519 keypair |
| Address | CubeHash-256(public key) → bech32m (`loot1…`) |

A wallet created with `lc new` and restored with `lc import` using the same phrase always produces the same address.

> **Note for hardware wallet integrators:** use path `m/44'/4103'/0'/0'/0'` with SLIP-0010 ed25519 derivation (all hardened). This is compatible with any SLIP-0010-compliant ed25519 implementation.

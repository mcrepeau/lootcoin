# lootcoin-wallet

Rust crate that compiles to WebAssembly. Provides key generation, BIP-39 mnemonic support, address derivation, and transaction signing for use in the browser (`lootcoin-web`).

Private keys never leave the browser — all cryptographic operations happen client-side inside the WASM module.

---

## Prerequisites

Install [`wasm-pack`](https://rustwasm.github.io/wasm-pack/installer/) if you haven't already:

```bash
cargo install wasm-pack
```

---

## Building

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

Copy (or symlink) the output files to `lootcoin-web/` to use them in the UI:

```bash
cp pkg/lootcoin_wallet.js pkg/lootcoin_wallet_bg.wasm ../lootcoin-web/
```

---

## API

### Creating a wallet

```js
import init, { Wallet } from "./lootcoin_wallet.js";
await init();

const wallet = new Wallet();            // new random wallet
wallet.mnemonic_phrase();               // "word1 word2 … word12" (BIP-39, 12 words)
wallet.address();                       // 64-char hex address
wallet.secret_key_hex();               // 64-char hex seed (for advanced export)
```

### Restoring a wallet

```js
const wallet = Wallet.from_mnemonic("word1 word2 … word12");
// or
const wallet = Wallet.from_secret_key_hex("64-char-hex-seed");
```

### Signing a transaction

```js
const txJson = wallet.sign_transaction(receiverAddress, amount, fee);
// Returns a JSON string ready to POST to the node's /transactions endpoint
```

---

## Key derivation

| Step | Detail |
|---|---|
| Entropy | 16 bytes from OS CSPRNG (WebCrypto in browser) |
| Mnemonic | BIP-39 12-word phrase |
| Seed | PBKDF2-HMAC-SHA512, no passphrase, 2048 rounds |
| Key | First 32 bytes of seed → Ed25519 keypair |
| Address | CubeHash-256(public key), hex-encoded |

Wallets created from a mnemonic and wallets restored from the same mnemonic produce identical keypairs and addresses.

---

## Notes

- The WASM module requires a secure context (HTTPS or `localhost`) because it uses the WebCrypto API for entropy generation.
- `mnemonic_phrase()` returns `undefined` for wallets imported via `from_secret_key_hex` — there is no mnemonic to recover in that case.

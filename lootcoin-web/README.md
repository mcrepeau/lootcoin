# lootcoin-web

Static web front-end for Lootcoin. Four pages served as plain HTML + JS + WASM — no bundler required.

---

## Pages

| File | URL | Description |
|---|---|---|
| `index.html` | `/` | Home page — live pot display, lottery explainer, fee schedule |
| `wallet.html` | `/wallet.html` | Wallet — create/open wallet, send transactions, view history |
| `explorer.html` | `/explorer.html` | Block explorer — live network stats, infinite-scroll block feed |
| `faucet.html` | `/faucet.html` | Faucet — request testnet coins |

---

## Prerequisites

The wallet page uses a WebAssembly module compiled from `lootcoin-wallet/`. To compile it, install [wasm-pack](https://rustwasm.github.io/wasm-pack/) and run:

```bash
wasm-pack build lootcoin-wallet --target web
# output: lootcoin-wallet/pkg/
```

---

## Configuration

Node and faucet URLs are set at runtime via `config.js`, which is loaded by every page before the app scripts. The default file bundled in the repo points at localhost:

```js
// config.js
window.LOOTCOIN_NODE_URL   = "http://127.0.0.1:3001";
window.LOOTCOIN_FAUCET_URL = "http://127.0.0.1:3030";
```

Edit this file to point at a different node/faucet without rebuilding anything. When running via Docker, mount your own `config.js` over the default:

```bash
docker run -v /path/to/config.js:/usr/share/nginx/html/config.js:ro mcrepeau79/lootcoin-web
```

---

## Running locally

Build the WASM first (see Prerequisites), then serve the directory:

```bash
python3 -m http.server 8000
# Open http://localhost:8000
```

Any static file server works (`nginx`, `npx serve`, etc.). Browsers block ES modules and WASM loaded from `file://` URLs, so serving over HTTP is required.

# lootcoin-web

Web UI for the [Lootcoin](https://github.com/mcrepeau/lootcoin) blockchain. Serves a static site via nginx containing a wallet, block explorer, faucet page, and home page. The wallet runs entirely in the browser using a compiled WebAssembly module — private keys never leave the device.

## Quick start

```bash
docker run -d \
  -p 8080:80 \
  mcrepeau79/lootcoin-web:latest
```

Then open `http://localhost:8080`. By default the UI connects to `http://127.0.0.1:3001` for the node and `http://127.0.0.1:3030` for the faucet.

## Configuring node and faucet URLs

Mount a custom `config.js` to point the UI at your own infrastructure:

```js
// config.js
window.LOOTCOIN_NODE_URL   = "https://api.mynode.example.com";
window.LOOTCOIN_FAUCET_URL = "https://faucet.mynode.example.com";
```

```bash
docker run -d \
  -p 8080:80 \
  -v ./config.js:/usr/share/nginx/html/config.js:ro \
  mcrepeau79/lootcoin-web:latest
```

## Ports

| Port | Protocol | Description |
|---|---|---|
| `80` | TCP | HTTP (nginx) |

## Pages

| Path | Description |
|---|---|
| `/` | Home — overview of the lottery mechanism |
| `/wallet.html` | Browser wallet — generate keys, check balance, send transactions |
| `/explorer.html` | Block explorer — blocks, addresses, lottery payouts |
| `/faucet.html` | Testnet faucet — request coins for any address |

## Docker Compose

A ready-to-use setup (nodes + miner + faucet + web UI) is available in the [GitHub repository](https://github.com/mcrepeau/lootcoin).

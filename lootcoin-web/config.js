// Runtime configuration for the Lootcoin web UI.
//
// To override, mount your own config.js at container start:
//   docker run -v /path/to/config.js:/usr/share/nginx/html/config.js:ro ...
//
// Or in docker-compose:
//   volumes:
//     - ./config.js:/usr/share/nginx/html/config.js:ro

window.LOOTCOIN_NODE_URLS  = ["https://node1.lootcoin.org","https://node2.rimba-net.com"];
window.LOOTCOIN_FAUCET_URL = "https://faucet.lootcoin.org";

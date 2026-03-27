// Runtime configuration for the Lootcoin web UI.
//
// To override, mount your own config.js at container start:
//   docker run -v /path/to/config.js:/usr/share/nginx/html/config.js:ro ...
//
// Or in docker-compose:
//   volumes:
//     - ./config.js:/usr/share/nginx/html/config.js:ro

window.LOOTCOIN_NODE_URL   = "http://127.0.0.1:3001";
window.LOOTCOIN_FAUCET_URL = "http://127.0.0.1:3030";

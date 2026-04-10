// Node failover helper.
//
// Reads the ordered list from config.js (LOOTCOIN_NODE_URLS).
// Each call to nodeFetch() tries the last-known-good node first, then
// cycles through the rest on network errors or 5xx responses.
// 2xx and 4xx are returned immediately — they are valid node answers.

const NODES = (window.LOOTCOIN_NODE_URLS ?? [window.LOOTCOIN_NODE_URL])
  .map(u => u.replace(/\/+$/, ""));

let activeIdx = 0;

/**
 * Drop-in replacement for fetch() against the configured node list.
 * @param {string} path  — e.g. "/chain/head"
 * @param {RequestInit} [init]
 * @returns {Promise<Response>}
 */
export async function nodeFetch(path, init) {
  for (let i = 0; i < NODES.length; i++) {
    const idx = (activeIdx + i) % NODES.length;
    try {
      const res = await fetch(NODES[idx] + path, init);
      if (res.status < 500) {
        activeIdx = idx;
        return res;
      }
    } catch {
      // Network-level failure — try next node.
    }
  }
  throw new Error("All nodes unreachable");
}

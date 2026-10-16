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

// ── Live events (SSE) ─────────────────────────────────────────────────────
//
// Subscribes to the node's /events stream, which carries one JSON event per
// accepted block and per transaction entering the mempool:
//   {"type": "block", "data": {...Block}}
//   {"type": "transaction", "data": {...Transaction}}
//
// Events signal THAT something changed — callers re-fetch the state they
// care about. Anything missed during a disconnect is covered by the callers'
// own polling fallback. EventSource reconnects on its own for blips; on a
// hard failure we rotate to the next configured node and resubscribe, so a
// dead node doesn't leave pages stale.

const EVENT_RETRY_MS = 30_000;
let eventSource = null;

/**
 * Subscribe to live node events.
 * @param {{ onBlock?: () => void, onTransaction?: (tx: any) => void }} handlers
 */
export function subscribeEvents({ onBlock, onTransaction } = {}) {
  if (eventSource) eventSource.close();
  const es = new EventSource(NODES[activeIdx] + "/events");
  eventSource = es;
  es.onmessage = (ev) => {
    try {
      const event = JSON.parse(ev.data);
      if (event.type === "block" && onBlock) onBlock();
      else if (event.type === "transaction" && onTransaction) onTransaction(event.data);
    } catch { /* malformed event — ignore */ }
  };
  es.onerror = () => {
    es.close();
    if (eventSource === es) eventSource = null;
    // Rotate to the next node and resubscribe after a pause.
    activeIdx = (activeIdx + 1) % NODES.length;
    setTimeout(() => subscribeEvents({ onBlock, onTransaction }), EVENT_RETRY_MS);
  };
}

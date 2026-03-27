use lootcoin_core::{block::Block, transaction::Transaction};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::info;

const MAX_SEEN: usize = 10_000;
/// Maximum number of peers tracked simultaneously. Prevents the hourly ping
/// task from making unbounded outbound requests if an attacker floods POST /peers.
pub const MAX_PEERS: usize = 50;
/// Broadcast channel capacity.  Any subscriber that falls more than this many
/// events behind will receive a `RecvError::Lagged` and skip those events — the
/// node won't crash or block other subscribers.
const EVENT_CHANNEL_CAP: usize = 1_024;

/// Fixed-capacity deduplication set with FIFO eviction.
///
/// When full, the oldest inserted key is evicted before the new one is inserted.
/// This prevents the "full clear" behaviour that previously allowed an attacker
/// to flush the cache with junk and then re-relay old gossip messages.
struct SeenCache {
    set: HashSet<Vec<u8>>,
    order: VecDeque<Vec<u8>>,
    capacity: usize,
}

impl SeenCache {
    fn new(capacity: usize) -> Self {
        Self {
            set: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Insert `key`. Returns `false` without inserting if already present.
    fn insert(&mut self, key: Vec<u8>) -> bool {
        if self.set.contains(&key) {
            return false;
        }
        if self.set.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.set.insert(key);
        true
    }
}

/// Events that the node broadcasts to SSE subscribers (both local clients and
/// peer nodes that have subscribed to our /events stream).
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub enum NodeEvent {
    Block(Block),
    Transaction(Transaction),
}

pub struct Gossip {
    /// peer URL → time we last successfully reached that peer.
    /// Initialized to `Instant::now()` so newly-added peers get a grace period.
    peers: RwLock<HashMap<String, Instant>>,
    seen_blocks: Mutex<SeenCache>,
    seen_txs: Mutex<SeenCache>,
    client: reqwest::Client,
    event_tx: broadcast::Sender<NodeEvent>,
    /// URLs for which an SSE subscription task is currently running.
    /// Guards against duplicate subscription tasks being spawned for the same peer
    /// (e.g. when announce_self causes a peer to POST /peers with a URL we already know).
    active_subscriptions: Mutex<HashSet<String>>,
}

impl Gossip {
    pub fn new(initial_peers: Vec<String>) -> Self {
        let now = Instant::now();
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        Self {
            peers: RwLock::new(initial_peers.into_iter().map(|p| (p, now)).collect()),
            seen_blocks: Mutex::new(SeenCache::new(MAX_SEEN)),
            seen_txs: Mutex::new(SeenCache::new(MAX_SEEN)),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("Failed to build gossip HTTP client"),
            event_tx,
            active_subscriptions: Mutex::new(HashSet::new()),
        }
    }

    /// Create a new receiver for the node event stream.
    /// Each call returns an independent receiver starting from the next event.
    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.event_tx.subscribe()
    }

    /// Add a peer. Returns `false` (without adding) if the peer list is already
    /// at MAX_PEERS and this URL is not already known.
    pub async fn add_peer(&self, url: String) -> bool {
        let mut peers = self.peers.write().await;
        if peers.len() >= MAX_PEERS && !peers.contains_key(&url) {
            return false;
        }
        peers.entry(url).or_insert(Instant::now());
        true
    }

    /// Register `url` as actively subscribed.
    /// Returns `false` if a subscription task for this URL is already running,
    /// so the caller can skip spawning a duplicate.
    pub async fn try_start_subscription(&self, url: &str) -> bool {
        self.active_subscriptions
            .lock()
            .await
            .insert(url.to_string())
    }

    /// Deregister `url` when its subscription task exits, so a future call to
    /// `try_start_subscription` for the same URL will succeed again.
    pub async fn end_subscription(&self, url: &str) {
        self.active_subscriptions.lock().await.remove(url);
    }

    /// Returns a snapshot of all known peer URLs.
    pub async fn peer_urls(&self) -> Vec<String> {
        self.peers.read().await.keys().cloned().collect()
    }

    /// Ping every known peer and refresh the last-seen timestamp for each one
    /// that responds successfully. Called by the background health-check task.
    pub async fn ping_peers(&self) {
        let urls: Vec<String> = self.peers.read().await.keys().cloned().collect();
        for url in urls {
            match self.client.get(format!("{}/chain/head", url)).send().await {
                Ok(r) if r.status().is_success() => {
                    let mut peers = self.peers.write().await;
                    if let Some(ts) = peers.get_mut(&url) {
                        *ts = Instant::now();
                    }
                }
                _ => {}
            }
        }
    }

    /// Remove peers whose last-seen time exceeds `max_age`.
    /// Returns the list of evicted URLs so the caller can clean up the DB.
    pub async fn evict_stale(&self, max_age: Duration) -> Vec<String> {
        let mut peers = self.peers.write().await;
        let mut evicted = Vec::new();
        peers.retain(|url, last_seen| {
            if last_seen.elapsed() > max_age {
                evicted.push(url.clone());
                false
            } else {
                true
            }
        });
        if !evicted.is_empty() {
            info!("Evicted {} stale peer(s): {:?}", evicted.len(), evicted);
        }
        evicted
    }

    /// Mark a block as seen and push it to all SSE subscribers.
    /// Returns `false` (no-op) if the block was already seen.
    pub async fn publish_block(&self, block: &Block) -> bool {
        if !self.seen_blocks.lock().await.insert(block.hash.clone()) {
            return false;
        }
        // No receivers → send returns Err(SendError) which we safely ignore.
        let _ = self.event_tx.send(NodeEvent::Block(block.clone()));
        true
    }

    /// Mark a transaction as seen and push it to all SSE subscribers.
    /// Returns `false` (no-op) if the transaction was already seen.
    pub async fn publish_transaction(&self, tx: &Transaction) -> bool {
        if !self.seen_txs.lock().await.insert(tx.signature.clone()) {
            return false;
        }
        let _ = self.event_tx.send(NodeEvent::Transaction(tx.clone()));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SeenCache ─────────────────────────────────────────────────────────────

    #[test]
    fn seen_cache_insert_new_key_returns_true() {
        let mut cache = SeenCache::new(4);
        assert!(cache.insert(vec![1, 2, 3]));
    }

    #[test]
    fn seen_cache_insert_duplicate_returns_false() {
        let mut cache = SeenCache::new(4);
        cache.insert(vec![1]);
        assert!(!cache.insert(vec![1])); // same key → false
    }

    #[test]
    fn seen_cache_evicts_oldest_when_full() {
        let mut cache = SeenCache::new(3);
        cache.insert(vec![1]); // oldest
        cache.insert(vec![2]);
        cache.insert(vec![3]); // cache is now full
                               // Inserting a 4th key must evict [1] (FIFO) and succeed
        assert!(cache.insert(vec![4]));
        // [1] was evicted: inserting it again should return true (treated as new)
        assert!(cache.insert(vec![1]));
        // [2] should also have been evicted by now (two inserts above into a cap-3 cache)
        assert!(cache.insert(vec![2]));
    }

    #[test]
    fn seen_cache_never_exceeds_capacity() {
        let cap = 5;
        let mut cache = SeenCache::new(cap);
        for i in 0u8..20 {
            cache.insert(vec![i]);
            assert!(cache.set.len() <= cap);
            assert!(cache.order.len() <= cap);
        }
    }

    // ── Gossip publish deduplication ─────────────────────────────────────────

    #[tokio::test]
    async fn publish_block_deduplicates() {
        use lootcoin_core::block::Block;
        let gossip = Gossip::new(vec![]);
        let block = Block {
            index: 1,
            previous_hash: vec![0u8; 32],
            timestamp: 0,
            nonce: 0,
            tx_root: vec![],
            transactions: vec![],
            hash: vec![0xAA],
        };
        assert!(gossip.publish_block(&block).await); // first: accepted
        assert!(!gossip.publish_block(&block).await); // duplicate: rejected
    }

    #[tokio::test]
    async fn add_peer_enforces_max_peers() {
        let gossip = Gossip::new(vec![]);
        for i in 0..MAX_PEERS {
            assert!(
                gossip
                    .add_peer(format!("http://peer{}.example.com", i))
                    .await
            );
        }
        // One more should be rejected
        assert!(
            !gossip
                .add_peer("http://overflow.example.com".to_string())
                .await
        );
        // But a URL already in the list is accepted (idempotent)
        assert!(
            gossip
                .add_peer("http://peer0.example.com".to_string())
                .await
        );
    }
}

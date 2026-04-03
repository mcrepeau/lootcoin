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

    /// Fire-and-forget: push `block` to every known peer's POST /blocks endpoint.
    ///
    /// Called after a block is successfully applied so that peers which cannot
    /// subscribe to our SSE stream (e.g. nodes behind NAT with no public URL)
    /// still propagate their blocks across the network.  Each push is spawned
    /// as an independent task so a slow or unreachable peer never delays the caller.
    pub async fn push_block_to_peers(&self, block: &Block) {
        let peers: Vec<String> = self.peers.read().await.keys().cloned().collect();
        for peer_url in peers {
            let block_clone = block.clone();
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client
                    .post(format!("{}/blocks", peer_url))
                    .json(&block_clone)
                    .send()
                    .await;
            });
        }
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

    // ── peer_urls ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn peer_urls_returns_initial_peers() {
        let gossip = Gossip::new(vec![
            "http://a.example.com".to_string(),
            "http://b.example.com".to_string(),
        ]);
        let mut urls = gossip.peer_urls().await;
        urls.sort();
        assert_eq!(
            urls,
            vec!["http://a.example.com", "http://b.example.com"]
        );
    }

    #[tokio::test]
    async fn peer_urls_reflects_added_peers() {
        let gossip = Gossip::new(vec![]);
        gossip.add_peer("http://p1.example.com".to_string()).await;
        gossip.add_peer("http://p2.example.com".to_string()).await;
        let mut urls = gossip.peer_urls().await;
        urls.sort();
        assert_eq!(urls, vec!["http://p1.example.com", "http://p2.example.com"]);
    }

    // ── evict_stale ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn evict_stale_removes_all_with_zero_max_age() {
        let gossip = Gossip::new(vec![
            "http://old1.example.com".to_string(),
            "http://old2.example.string".to_string(),
        ]);
        // Any positive elapsed time satisfies elapsed() > Duration::ZERO
        let evicted = gossip.evict_stale(Duration::ZERO).await;
        assert_eq!(evicted.len(), 2);
        assert!(gossip.peer_urls().await.is_empty());
    }

    #[tokio::test]
    async fn evict_stale_keeps_peers_within_max_age() {
        let gossip = Gossip::new(vec!["http://fresh.example.com".to_string()]);
        // One hour max age — a peer added just now is definitely within it
        let evicted = gossip.evict_stale(Duration::from_secs(3600)).await;
        assert!(evicted.is_empty());
        assert_eq!(gossip.peer_urls().await.len(), 1);
    }

    #[tokio::test]
    async fn evict_stale_returns_evicted_urls() {
        let gossip = Gossip::new(vec!["http://x.example.com".to_string()]);
        let evicted = gossip.evict_stale(Duration::ZERO).await;
        assert!(evicted.contains(&"http://x.example.com".to_string()));
    }

    // ── publish_transaction ───────────────────────────────────────────────────

    #[tokio::test]
    async fn publish_transaction_deduplicates() {
        use lootcoin_core::transaction::Transaction;
        let gossip = Gossip::new(vec![]);
        let tx = Transaction {
            sender: "alice".to_string(),
            receiver: "bob".to_string(),
            amount: 1,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![0xBB],
        };
        assert!(gossip.publish_transaction(&tx).await);
        assert!(!gossip.publish_transaction(&tx).await);
    }

    #[tokio::test]
    async fn publish_transaction_distinct_signatures_both_accepted() {
        use lootcoin_core::transaction::Transaction;
        let gossip = Gossip::new(vec![]);
        let make_tx = |sig: u8| Transaction {
            sender: "alice".to_string(),
            receiver: "bob".to_string(),
            amount: 1,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![sig],
        };
        assert!(gossip.publish_transaction(&make_tx(1)).await);
        assert!(gossip.publish_transaction(&make_tx(2)).await);
    }

    // ── subscribe / event delivery ────────────────────────────────────────────

    #[tokio::test]
    async fn subscribe_receives_published_block() {
        use lootcoin_core::block::Block;
        let gossip = Gossip::new(vec![]);
        let mut rx = gossip.subscribe();
        let block = Block {
            index: 1,
            previous_hash: vec![0u8; 32],
            timestamp: 0,
            nonce: 0,
            tx_root: vec![],
            transactions: vec![],
            hash: vec![0xCC],
        };
        gossip.publish_block(&block).await;
        match rx.try_recv().unwrap() {
            NodeEvent::Block(b) => assert_eq!(b.hash, vec![0xCC]),
            _ => panic!("expected Block event"),
        }
    }

    #[tokio::test]
    async fn subscribe_receives_published_transaction() {
        use lootcoin_core::transaction::Transaction;
        let gossip = Gossip::new(vec![]);
        let mut rx = gossip.subscribe();
        let tx = Transaction {
            sender: "a".to_string(),
            receiver: "b".to_string(),
            amount: 5,
            fee: 1,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![0xDD],
        };
        gossip.publish_transaction(&tx).await;
        match rx.try_recv().unwrap() {
            NodeEvent::Transaction(t) => assert_eq!(t.signature, vec![0xDD]),
            _ => panic!("expected Transaction event"),
        }
    }

    #[tokio::test]
    async fn subscribe_does_not_receive_duplicate_block() {
        use lootcoin_core::block::Block;
        let gossip = Gossip::new(vec![]);
        let block = Block {
            index: 1,
            previous_hash: vec![0u8; 32],
            timestamp: 0,
            nonce: 0,
            tx_root: vec![],
            transactions: vec![],
            hash: vec![0xEE],
        };
        gossip.publish_block(&block).await; // first publish — no subscriber yet
        let mut rx = gossip.subscribe();
        gossip.publish_block(&block).await; // duplicate — must be dropped
        assert!(rx.try_recv().is_err());
    }

    // ── try_start_subscription / end_subscription ─────────────────────────────

    #[tokio::test]
    async fn try_start_subscription_first_call_returns_true() {
        let gossip = Gossip::new(vec![]);
        assert!(gossip.try_start_subscription("http://peer.example.com").await);
    }

    #[tokio::test]
    async fn try_start_subscription_duplicate_returns_false() {
        let gossip = Gossip::new(vec![]);
        gossip.try_start_subscription("http://peer.example.com").await;
        assert!(!gossip.try_start_subscription("http://peer.example.com").await);
    }

    #[tokio::test]
    async fn end_subscription_allows_restart() {
        let gossip = Gossip::new(vec![]);
        gossip.try_start_subscription("http://peer.example.com").await;
        gossip.end_subscription("http://peer.example.com").await;
        // After ending, a new subscription for the same URL should succeed
        assert!(gossip.try_start_subscription("http://peer.example.com").await);
    }

    #[tokio::test]
    async fn end_subscription_different_urls_are_independent() {
        let gossip = Gossip::new(vec![]);
        gossip.try_start_subscription("http://a.example.com").await;
        gossip.try_start_subscription("http://b.example.com").await;
        gossip.end_subscription("http://a.example.com").await;
        // a is freed, b is still active
        assert!(gossip.try_start_subscription("http://a.example.com").await);
        assert!(!gossip.try_start_subscription("http://b.example.com").await);
    }
}

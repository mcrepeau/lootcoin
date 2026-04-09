use crate::db::Db;
use lootcoin_core::transaction::Transaction;
use std::collections::HashMap;
use std::sync::Arc;

pub const MAX_MEMPOOL_SIZE: usize = 10_000;
/// Transactions added more than this many blocks ago are evicted.
pub const TX_EXPIRY_BLOCKS: u64 = 100;
/// Maximum number of pending transactions per sender address.
const MAX_PENDING_PER_SENDER: usize = 25;

struct MempoolEntry {
    tx: Transaction,
    added_height: u64,
}

/// Pending transactions keyed by Ed25519 signature bytes.
/// Each transaction's random nonce guarantees a unique signature, so the
/// signature is a stable, collision-free identity that also serves as the
/// replay-protection key.
pub struct Mempool {
    entries: HashMap<Vec<u8>, MempoolEntry>,
    db: Option<Arc<Db>>,
}

impl Mempool {
    pub fn new(db: Option<Arc<Db>>) -> Self {
        Self {
            entries: HashMap::new(),
            db,
        }
    }

    /// Populate the in-memory map from persisted entries without triggering
    /// write-through (the data is already in the DB).  Coinbase entries are
    /// skipped.  Per-sender and global limits are enforced.
    pub fn restore(&mut self, entries: Vec<(Transaction, u64)>) {
        for (tx, added_height) in entries {
            if tx.sender.is_empty() {
                continue; // skip coinbase
            }
            if self.len() >= MAX_MEMPOOL_SIZE {
                break;
            }
            // Skip duplicates by signature (first one wins on restore).
            if self.entries.contains_key(&tx.signature) {
                continue;
            }
            let sender_count = self.pending_count(&tx.sender);
            if sender_count >= MAX_PENDING_PER_SENDER {
                continue;
            }
            self.entries
                .insert(tx.signature.clone(), MempoolEntry { tx, added_height });
        }
    }

    /// Total number of pending transactions across all senders.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of pending transactions for `sender`.
    pub fn pending_count(&self, sender: &str) -> usize {
        self.entries
            .values()
            .filter(|e| e.tx.sender == sender)
            .count()
    }

    /// Sum of `amount + fee` across all pending transactions for `sender`.
    pub fn pending_debit(&self, sender: &str) -> u64 {
        self.entries
            .values()
            .filter(|e| e.tx.sender == sender)
            .map(|e| e.tx.amount.saturating_add(e.tx.fee))
            .fold(0u64, |acc, x| acc.saturating_add(x))
    }

    /// Insert a transaction. Returns `false` if the pool is at capacity,
    /// the sender queue is full, or this exact signature is already present.
    pub fn add_transaction(&mut self, tx: Transaction, current_height: u64) -> bool {
        if tx.sender.is_empty() {
            return false; // coinbase not allowed in mempool
        }
        // Exact duplicate (same signature already pending).
        if self.entries.contains_key(&tx.signature) {
            return false;
        }
        // Per-sender cap.
        if self.pending_count(&tx.sender) >= MAX_PENDING_PER_SENDER {
            return false;
        }
        // Global cap.
        if self.len() >= MAX_MEMPOOL_SIZE {
            return false;
        }

        if let Some(db) = &self.db {
            if let Err(e) = db.save_mempool_tx(&tx, current_height) {
                tracing::warn!("Failed to persist mempool tx: {}", e);
            }
        }
        self.entries.insert(
            tx.signature.clone(),
            MempoolEntry {
                tx,
                added_height: current_height,
            },
        );
        true
    }

    /// Remove every transaction that was included in a confirmed block.
    pub fn remove_included(&mut self, included: &[Transaction]) {
        let mut removed_txids: Vec<Vec<u8>> = Vec::new();
        for tx in included {
            if tx.sender.is_empty() {
                continue; // coinbase has no mempool slot
            }
            if let Some(entry) = self.entries.remove(&tx.signature) {
                removed_txids.push(entry.tx.txid().to_vec());
            }
        }
        if let Some(db) = &self.db {
            if let Err(e) = db.remove_mempool_txs(&removed_txids) {
                tracing::warn!(
                    "Failed to remove confirmed txs from persisted mempool: {}",
                    e
                );
            }
        }
    }

    /// Drop transactions that have been sitting in the pool for more than
    /// TX_EXPIRY_BLOCKS blocks without being included.
    pub fn evict_expired(&mut self, current_height: u64) {
        let mut evicted_txids: Vec<Vec<u8>> = Vec::new();
        self.entries.retain(|_, entry| {
            if current_height.saturating_sub(entry.added_height) <= TX_EXPIRY_BLOCKS {
                true
            } else {
                evicted_txids.push(entry.tx.txid().to_vec());
                false
            }
        });
        if let Some(db) = &self.db {
            if let Err(e) = db.remove_mempool_txs(&evicted_txids) {
                tracing::warn!("Failed to remove evicted txs from persisted mempool: {}", e);
            }
        }
    }

    /// Returns every pending transaction together with the chain height at
    /// which it was first seen.
    pub fn all_transactions_with_height(&self) -> Vec<(Transaction, u64)> {
        self.entries
            .values()
            .map(|e| (e.tx.clone(), e.added_height))
            .collect()
    }

    /// Re-add transactions from a displaced (reorged-away) block that are still
    /// valid under the new chain state.  A tx is skipped if its signature is
    /// already pending, the sender's queue is full, or the sender cannot afford it.
    pub fn readd_displaced(
        &mut self,
        txs: &[lootcoin_core::transaction::Transaction],
        get_balance: impl Fn(&str) -> u64,
        current_height: u64,
    ) {
        for tx in txs {
            if tx.sender.is_empty() {
                continue; // skip coinbase
            }
            // Skip if this exact tx is already pending.
            if self.entries.contains_key(&tx.signature) {
                continue;
            }
            if self.pending_count(&tx.sender) >= MAX_PENDING_PER_SENDER {
                continue;
            }
            if self.len() >= MAX_MEMPOOL_SIZE {
                break;
            }
            let pending_d = self.pending_debit(&tx.sender);
            let balance = get_balance(&tx.sender);
            let cost = tx.amount.saturating_add(tx.fee);
            if balance.saturating_sub(pending_d) < cost {
                continue; // can't afford
            }
            if let Some(db) = &self.db {
                if let Err(e) = db.save_mempool_tx(tx, current_height) {
                    tracing::warn!("Failed to persist re-added displaced tx: {}", e);
                }
            }
            self.entries.insert(
                tx.signature.clone(),
                MempoolEntry {
                    tx: tx.clone(),
                    added_height: current_height,
                },
            );
        }
    }

    /// Median fee of all pending transactions.
    /// `None` only when the pool is empty.
    pub fn median_fee(&self) -> Option<u64> {
        let pending = self.entries.len();
        if pending == 0 {
            return None;
        }
        let mut fees: Vec<u64> = self.entries.values().map(|e| e.tx.fee).collect();
        fees.sort_unstable_by(|a, b| b.cmp(a));
        Some(fees[pending / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lootcoin_core::{block::MAX_BLOCK_TXS, transaction::Transaction};

    fn make_tx(sig: u8, sender: &str, amount: u64, fee: u64) -> Transaction {
        Transaction {
            sender: sender.to_string(),
            receiver: "recv".to_string(),
            amount,
            fee,
            nonce: sig as u64,
            public_key: [0u8; 32],
            signature: vec![sig],
        }
    }

    /// Returns a pool with exactly MAX_BLOCK_TXS entries (the boundary for busy mode).
    /// Uses sig values 0..MAX_BLOCK_TXS-1 (as u8, wrapping), so callers can safely
    /// use sig=255 for an additional entry without collision.
    fn make_busy_pool() -> Mempool {
        let mut pool = Mempool::new(None);
        for i in 0..MAX_BLOCK_TXS as u8 {
            pool.add_transaction(make_tx(i, &format!("s{i}"), 1, 50), 0);
        }
        pool
    }

    fn coinbase() -> Transaction {
        Transaction {
            sender: String::new(),
            receiver: "miner".to_string(),
            amount: 1,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![0],
        }
    }

    #[test]
    fn new_pool_is_empty() {
        let pool = Mempool::new(None);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn add_transaction_increases_len() {
        let mut pool = Mempool::new(None);
        assert!(pool.add_transaction(make_tx(1, "alice", 100, 10), 0));
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn multiple_txs_same_sender_different_sigs_all_accepted() {
        let mut pool = Mempool::new(None);
        assert!(pool.add_transaction(make_tx(1, "alice", 100, 10), 0));
        assert!(pool.add_transaction(make_tx(2, "alice", 100, 10), 0));
        assert!(pool.add_transaction(make_tx(3, "alice", 100, 10), 0));
        assert_eq!(pool.len(), 3);
        assert_eq!(pool.pending_count("alice"), 3);
    }

    #[test]
    fn multiple_clients_same_sender_coexist() {
        // UI submits one tx, CLI submits another — both should be accepted.
        let mut pool = Mempool::new(None);
        assert!(pool.add_transaction(make_tx(10, "alice", 100, 10), 0));
        assert!(pool.add_transaction(make_tx(20, "alice", 100, 10), 0));
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn duplicate_signature_not_added_twice() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(5, "alice", 100, 10);
        assert!(pool.add_transaction(tx.clone(), 0));
        assert!(!pool.add_transaction(tx, 1)); // same sig — rejected
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn pending_debit_sums_all_pending_txs() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0); // 110
        pool.add_transaction(make_tx(2, "alice", 200, 20), 0); // 220
        assert_eq!(pool.pending_debit("alice"), 330);
    }

    #[test]
    fn remove_included_removes_matching_sig() {
        let mut pool = Mempool::new(None);
        let tx1 = make_tx(1, "alice", 100, 10);
        let tx2 = make_tx(2, "alice", 100, 10);
        pool.add_transaction(tx1.clone(), 0);
        pool.add_transaction(tx2, 0);
        pool.remove_included(&[tx1]);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.pending_count("alice"), 1);
    }

    #[test]
    fn remove_included_removes_matching_txs() {
        let mut pool = Mempool::new(None);
        let tx1 = make_tx(1, "alice", 100, 10);
        let tx2 = make_tx(2, "bob", 50, 5);
        pool.add_transaction(tx1.clone(), 0);
        pool.add_transaction(tx2, 0);
        pool.remove_included(&[tx1]);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn remove_included_noop_for_unknown_sig() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0);
        pool.remove_included(&[make_tx(99, "nobody", 0, 0)]);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn evict_expired_removes_too_old() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0); // added at 0
        pool.add_transaction(make_tx(2, "bob", 100, 10), 50); // added at 50
        pool.evict_expired(101); // alice's tx expires (age 101 > 100)
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn evict_expired_keeps_exactly_at_boundary() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0);
        pool.evict_expired(100); // age == TX_EXPIRY_BLOCKS: retained (<=)
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn evict_expired_keeps_all_when_current_height_low() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0);
        pool.add_transaction(make_tx(2, "bob", 50, 5), 0);
        pool.evict_expired(10);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn evict_expired_removes_individual_entries() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0); // old
        pool.add_transaction(make_tx(2, "alice", 100, 10), 90); // recent
        pool.evict_expired(101); // sig 1 expires, sig 2 stays
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.pending_count("alice"), 1);
    }

    #[test]
    fn pending_debit_returns_zero_for_unknown_sender() {
        let pool = Mempool::new(None);
        assert_eq!(pool.pending_debit("nobody"), 0);
    }

    #[test]
    fn pending_count_returns_zero_for_unknown_sender() {
        let pool = Mempool::new(None);
        assert_eq!(pool.pending_count("nobody"), 0);
    }

    #[test]
    fn readd_displaced_skips_coinbase() {
        let mut pool = Mempool::new(None);
        pool.readd_displaced(&[coinbase()], |_| 1000, 10);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn readd_displaced_skips_already_present_sig() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(5, "alice", 100, 10);
        pool.add_transaction(tx.clone(), 0);
        pool.readd_displaced(&[tx], |_| 1000, 10); // same sig already pending — skipped
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn readd_displaced_skips_unaffordable() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(1, "alice", 100, 10); // cost = 110
        pool.readd_displaced(&[tx], |_| 50, 10); // balance 50 < 110
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn readd_displaced_adds_affordable_tx() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(42, "alice", 100, 10);
        pool.readd_displaced(&[tx], |_| 200, 10);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn readd_displaced_adds_alongside_existing_pending() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(9, "alice", 50, 5), 0); // sig 9 already pending
        let tx = make_tx(42, "alice", 50, 5); // different sig — should be added
        pool.readd_displaced(&[tx], |_| 200, 10);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn all_transactions_with_height_returns_correct_count() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 5);
        pool.add_transaction(make_tx(2, "bob", 50, 5), 10);
        let all = pool.all_transactions_with_height();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn all_transactions_with_height_preserves_added_height() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 42);
        let all = pool.all_transactions_with_height();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1, 42);
    }

    // ── median_fee ────────────────────────────────────────────────────────────

    #[test]
    fn median_fee_empty_pool_is_none() {
        let pool = Mempool::new(None);
        assert!(pool.median_fee().is_none());
    }

    #[test]
    fn median_fee_odd_count_picks_middle() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 1, 30), 0);
        pool.add_transaction(make_tx(2, "bob", 1, 10), 0);
        pool.add_transaction(make_tx(3, "carol", 1, 20), 0);
        // sorted descending: [30, 20, 10]; median index 1 → 20
        assert_eq!(pool.median_fee(), Some(20));
    }

    #[test]
    fn median_fee_at_capacity_all_same_fee() {
        let pool = make_busy_pool();
        assert_eq!(pool.median_fee(), Some(50));
    }

    #[test]
    fn median_fee_reflects_middle_of_mixed_fees() {
        let mut pool = Mempool::new(None);
        // 240 txs with fee=100, 1 extra with fee=1 → total 241
        // Sorted descending: [100 ×240, 1]; median index=120 → fee=100.
        for i in 0..MAX_BLOCK_TXS as u8 {
            pool.add_transaction(make_tx(i, &format!("s{i}"), 1, 100), 0);
        }
        pool.add_transaction(make_tx(255, "extra", 1, 1), 0);
        assert_eq!(pool.median_fee(), Some(100));
    }
}

use lootcoin_core::transaction::Transaction;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use crate::db::Db;

#[derive(Serialize)]
pub struct FeeStats {
    pub count: usize,
    pub min: Option<u64>,
    pub max: Option<u64>,
    pub median: Option<u64>,
    pub p25: Option<u64>,
    pub p75: Option<u64>,
}

pub const MAX_MEMPOOL_SIZE: usize = 10_000;
/// Transactions added more than this many blocks ago are evicted.
pub const TX_EXPIRY_BLOCKS: u64 = 100;

struct MempoolEntry {
    tx: Transaction,
    added_height: u64,
}

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
    /// write-through (the data is already in the DB).
    pub fn restore(&mut self, entries: Vec<(Transaction, u64)>) {
        for (tx, added_height) in entries {
            if self.entries.len() >= MAX_MEMPOOL_SIZE {
                break;
            }
            let sig = tx.signature.clone();
            self.entries
                .entry(sig)
                .or_insert(MempoolEntry { tx, added_height });
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Add a transaction. Returns `false` only if the mempool is full and the
    /// tx is not already present (idempotent re-submission is always accepted).
    pub fn add_transaction(&mut self, tx: Transaction, current_height: u64) -> bool {
        if self.entries.len() >= MAX_MEMPOOL_SIZE && !self.entries.contains_key(&tx.signature) {
            return false;
        }
        let sig = tx.signature.clone();
        let is_new = !self.entries.contains_key(&sig);
        self.entries.entry(sig.clone()).or_insert(MempoolEntry {
            tx: tx.clone(),
            added_height: current_height,
        });
        if is_new {
            if let Some(db) = &self.db {
                if let Err(e) = db.save_mempool_tx(&sig, &tx, current_height) {
                    tracing::warn!("Failed to persist mempool tx: {}", e);
                }
            }
        }
        true
    }

    /// Remove every transaction that was included in a block.
    pub fn remove_included(&mut self, included: &[Transaction]) {
        let mut removed_sigs: Vec<Vec<u8>> = Vec::new();
        for tx in included {
            if self.entries.remove(&tx.signature).is_some() {
                removed_sigs.push(tx.signature.clone());
            }
        }
        if let Some(db) = &self.db {
            if let Err(e) = db.remove_mempool_txs(&removed_sigs) {
                tracing::warn!(
                    "Failed to remove confirmed txs from persisted mempool: {}",
                    e
                );
            }
        }
    }

    /// Drop transactions whose nonce will never be valid again because they
    /// have been sitting in the pool for more than TX_EXPIRY_BLOCKS blocks.
    pub fn evict_expired(&mut self, current_height: u64) {
        let mut evicted_sigs: Vec<Vec<u8>> = Vec::new();
        self.entries.retain(|sig, entry| {
            if current_height.saturating_sub(entry.added_height) <= TX_EXPIRY_BLOCKS {
                true
            } else {
                evicted_sigs.push(sig.clone());
                false
            }
        });
        if let Some(db) = &self.db {
            if let Err(e) = db.remove_mempool_txs(&evicted_sigs) {
                tracing::warn!("Failed to remove evicted txs from persisted mempool: {}", e);
            }
        }
    }

    /// Returns every pending transaction together with the chain height at
    /// which it was first seen. Used by the mempool API endpoint so miners
    /// can implement fee-based priority with age-based eligibility guarantees.
    pub fn all_transactions_with_height(&self) -> Vec<(Transaction, u64)> {
        self.entries
            .values()
            .map(|e| (e.tx.clone(), e.added_height))
            .collect()
    }

    /// Re-add transactions from a displaced (reorged-away) block that are still
    /// valid under the new chain state. Skips coinbase txs, txs already in the
    /// pool, and txs whose sender can no longer afford them.
    pub fn readd_displaced(
        &mut self,
        txs: &[lootcoin_core::transaction::Transaction],
        get_balance: impl Fn(&str) -> u64,
        current_height: u64,
    ) {
        for tx in txs {
            if tx.sender.is_empty() {
                continue;
            } // skip coinbase
            if self.entries.contains_key(&tx.signature) {
                continue;
            } // already present
            let cost = tx.amount.saturating_add(tx.fee);
            let already_pending = self.pending_debit(&tx.sender);
            let balance = get_balance(&tx.sender);
            if balance.saturating_sub(already_pending) < cost {
                continue;
            } // can't afford
            self.entries.insert(
                tx.signature.clone(),
                MempoolEntry {
                    tx: tx.clone(),
                    added_height: current_height,
                },
            );
        }
    }

    /// Sum of `amount + fee` for all pending txs from `sender`.
    /// Used to compute the sender's effective spendable balance.
    pub fn pending_debit(&self, sender: &str) -> u64 {
        self.entries
            .values()
            .filter(|e| e.tx.sender == sender)
            .fold(0u64, |acc, e| {
                acc.saturating_add(e.tx.amount.saturating_add(e.tx.fee))
            })
    }

    /// Returns fee distribution statistics across all pending transactions.
    pub fn fee_stats(&self) -> FeeStats {
        let mut fees: Vec<u64> = self.entries.values().map(|e| e.tx.fee).collect();
        let count = fees.len();
        if count == 0 {
            return FeeStats {
                count: 0,
                min: None,
                max: None,
                median: None,
                p25: None,
                p75: None,
            };
        }
        fees.sort_unstable();
        FeeStats {
            count,
            min: Some(fees[0]),
            max: Some(fees[count - 1]),
            median: Some(fees[count / 2]),
            p25: Some(fees[count / 4]),
            p75: Some(fees[count * 3 / 4]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lootcoin_core::transaction::Transaction;

    fn make_tx(sig_byte: u8, sender: &str, amount: u64, fee: u64) -> Transaction {
        Transaction {
            sender: sender.to_string(),
            receiver: "recv".to_string(),
            amount,
            fee,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![sig_byte],
        }
    }

    fn coinbase(sig_byte: u8) -> Transaction {
        Transaction {
            sender: String::new(),
            receiver: "miner".to_string(),
            amount: 1,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![sig_byte],
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
    fn add_transaction_is_idempotent() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(1, "alice", 100, 10);
        pool.add_transaction(tx.clone(), 0);
        pool.add_transaction(tx, 5); // same signature — should not duplicate
        assert_eq!(pool.len(), 1);
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
    fn remove_included_noop_for_unknown_tx() {
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
                                                              // At height 101: age of tx1 = 101 > TX_EXPIRY_BLOCKS (100) → evicted
        pool.evict_expired(101);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn evict_expired_keeps_exactly_at_boundary() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0);
        // age = 100 == TX_EXPIRY_BLOCKS: retained (<=)
        pool.evict_expired(100);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn evict_expired_keeps_all_when_current_height_low() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0);
        pool.add_transaction(make_tx(2, "bob", 50, 5), 0);
        pool.evict_expired(10); // only 10 blocks have passed
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn pending_debit_sums_amount_plus_fee_per_sender() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0); // debit 110
        pool.add_transaction(make_tx(2, "alice", 50, 5), 0); // debit 55
        pool.add_transaction(make_tx(3, "bob", 200, 20), 0); // not alice
        assert_eq!(pool.pending_debit("alice"), 165);
        assert_eq!(pool.pending_debit("bob"), 220);
        assert_eq!(pool.pending_debit("carol"), 0);
    }

    #[test]
    fn pending_debit_zero_for_unknown_sender() {
        let pool = Mempool::new(None);
        assert_eq!(pool.pending_debit("nobody"), 0);
    }

    #[test]
    fn readd_displaced_skips_coinbase() {
        let mut pool = Mempool::new(None);
        pool.readd_displaced(&[coinbase(1)], |_| 1000, 10);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn readd_displaced_skips_already_present() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(1, "alice", 100, 10);
        pool.add_transaction(tx.clone(), 0);
        pool.readd_displaced(&[tx], |_| 1000, 10);
        assert_eq!(pool.len(), 1); // still 1, not 2
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
        let tx = make_tx(1, "alice", 100, 10); // cost = 110
        pool.readd_displaced(&[tx], |_| 200, 10); // balance 200 >= 110
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn readd_displaced_accounts_for_existing_pending_debit() {
        let mut pool = Mempool::new(None);
        // alice already has a pending tx costing 150
        pool.add_transaction(make_tx(9, "alice", 140, 10), 0); // pending debit = 150
                                                               // try to re-add a tx costing 60; balance=200, effective=200-150=50 < 60 → skip
        let tx = make_tx(1, "alice", 55, 5); // cost = 60
        pool.readd_displaced(&[tx], |_| 200, 10);
        assert_eq!(pool.len(), 1); // only the original pending tx
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

    // ── fee_stats ─────────────────────────────────────────────────────────────

    #[test]
    fn fee_stats_empty_pool_returns_zero_count_and_none_fields() {
        let pool = Mempool::new(None);
        let s = pool.fee_stats();
        assert_eq!(s.count, 0);
        assert!(s.min.is_none());
        assert!(s.max.is_none());
        assert!(s.median.is_none());
        assert!(s.p25.is_none());
        assert!(s.p75.is_none());
    }

    #[test]
    fn fee_stats_single_tx_all_fields_equal_its_fee() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 42), 0);
        let s = pool.fee_stats();
        assert_eq!(s.count, 1);
        assert_eq!(s.min, Some(42));
        assert_eq!(s.max, Some(42));
        assert_eq!(s.median, Some(42));
        assert_eq!(s.p25, Some(42));
        assert_eq!(s.p75, Some(42));
    }

    #[test]
    fn fee_stats_correct_min_max_median() {
        let mut pool = Mempool::new(None);
        // fees: 1, 5, 10, 20, 100 → sorted: [1, 5, 10, 20, 100]
        pool.add_transaction(make_tx(1, "a", 0, 10), 0);
        pool.add_transaction(make_tx(2, "b", 0, 1), 0);
        pool.add_transaction(make_tx(3, "c", 0, 100), 0);
        pool.add_transaction(make_tx(4, "d", 0, 20), 0);
        pool.add_transaction(make_tx(5, "e", 0, 5), 0);
        let s = pool.fee_stats();
        assert_eq!(s.count, 5);
        assert_eq!(s.min, Some(1));
        assert_eq!(s.max, Some(100));
        assert_eq!(s.median, Some(10)); // index 5/2 = 2 → fees[2] = 10
        assert_eq!(s.p25, Some(5)); // index 5/4 = 1 → fees[1] = 5
        assert_eq!(s.p75, Some(20)); // index 5*3/4 = 3 → fees[3] = 20
    }
}

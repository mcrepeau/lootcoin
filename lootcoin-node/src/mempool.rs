use crate::db::Db;
use lootcoin_core::transaction::Transaction;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

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

/// One pending transaction per sender address (Ethereum-style). Enforcing a
/// single pending slot per sender keeps nonce validation simple: the chain
/// always knows exactly which nonce to expect next, and the mempool tracks at
/// most one in-flight transaction that consumes that slot.
pub struct Mempool {
    entries: HashMap<String, MempoolEntry>, // keyed by sender address
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
    /// write-through (the data is already in the DB). When two persisted
    /// entries share the same sender, the one with the higher added_height is
    /// kept (most recently submitted). Coinbase entries are skipped.
    pub fn restore(&mut self, entries: Vec<(Transaction, u64)>) {
        for (tx, added_height) in entries {
            if tx.sender.is_empty() {
                continue; // skip coinbase
            }
            if self.entries.len() >= MAX_MEMPOOL_SIZE {
                break;
            }
            let sender = tx.sender.clone();
            self.entries
                .entry(sender)
                .and_modify(|existing| {
                    if added_height > existing.added_height {
                        *existing = MempoolEntry {
                            tx: tx.clone(),
                            added_height,
                        };
                    }
                })
                .or_insert(MempoolEntry { tx, added_height });
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Add a transaction. Returns `false` only if the mempool is full and the
    /// sender has no existing pending slot. Re-submission by the same sender
    /// replaces the existing entry (nonce bump or fee replacement).
    pub fn add_transaction(&mut self, tx: Transaction, current_height: u64) -> bool {
        if tx.sender.is_empty() {
            return false; // coinbase not allowed in mempool
        }
        let sender = tx.sender.clone();
        if self.entries.len() >= MAX_MEMPOOL_SIZE && !self.entries.contains_key(&sender) {
            return false;
        }
        let old_txid = self.entries.get(&sender).map(|e| e.tx.txid().to_vec());
        let is_replace = old_txid.is_some();
        self.entries.insert(
            sender,
            MempoolEntry {
                tx: tx.clone(),
                added_height: current_height,
            },
        );
        if let Some(db) = &self.db {
            if is_replace {
                if let Some(txid) = old_txid {
                    let _ = db.remove_mempool_txs(&[txid]);
                }
            }
            if let Err(e) = db.save_mempool_tx(&tx, current_height) {
                tracing::warn!("Failed to persist mempool tx: {}", e);
            }
        }
        true
    }

    /// Remove every transaction that was included in a block.
    pub fn remove_included(&mut self, included: &[Transaction]) {
        let mut removed_txids: Vec<Vec<u8>> = Vec::new();
        for tx in included {
            if tx.sender.is_empty() {
                continue; // coinbase has no mempool slot
            }
            if let Some(entry) = self.entries.remove(&tx.sender) {
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
    /// which it was first seen. Used by the mempool API endpoint so miners
    /// can implement fee-based priority with age-based eligibility guarantees.
    pub fn all_transactions_with_height(&self) -> Vec<(Transaction, u64)> {
        self.entries
            .values()
            .map(|e| (e.tx.clone(), e.added_height))
            .collect()
    }

    /// Re-add transactions from a displaced (reorged-away) block that are still
    /// valid under the new chain state. Skips coinbase txs, senders that already
    /// have a pending slot, and txs whose nonce no longer matches the chain.
    pub fn readd_displaced(
        &mut self,
        txs: &[lootcoin_core::transaction::Transaction],
        get_balance: impl Fn(&str) -> u64,
        get_nonce: impl Fn(&str) -> u64,
        current_height: u64,
    ) {
        for tx in txs {
            if tx.sender.is_empty() {
                continue; // skip coinbase
            }
            if self.entries.contains_key(&tx.sender) {
                continue; // sender already has a pending slot
            }
            // Nonce must match current chain expectation.
            if tx.nonce != get_nonce(&tx.sender) {
                continue;
            }
            let cost = tx.amount.saturating_add(tx.fee);
            let balance = get_balance(&tx.sender);
            if balance < cost {
                continue; // can't afford
            }
            self.entries.insert(
                tx.sender.clone(),
                MempoolEntry {
                    tx: tx.clone(),
                    added_height: current_height,
                },
            );
            if let Some(db) = &self.db {
                if let Err(e) = db.save_mempool_tx(tx, current_height) {
                    tracing::warn!("Failed to persist re-added displaced tx: {}", e);
                }
            }
        }
    }

    /// The nonce of the pending transaction for `sender`, or `None` if they
    /// have no pending slot.  Used by `get_balance_handler` to return a
    /// mempool-aware `next_nonce` so that two rapid submissions from the same
    /// address don't both receive the same confirmed nonce and clobber each
    /// other.
    pub fn pending_nonce(&self, sender: &str) -> Option<u64> {
        self.entries.get(sender).map(|e| e.tx.nonce)
    }

    /// The pending debit for `sender`: `amount + fee` of their single pending
    /// transaction, or 0 if they have no pending slot.
    pub fn pending_debit(&self, sender: &str) -> u64 {
        self.entries
            .get(sender)
            .map(|e| e.tx.amount.saturating_add(e.tx.fee))
            .unwrap_or(0)
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
        pool.add_transaction(tx, 5); // same sender — replaces, still 1 entry
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn add_transaction_replaces_existing_for_same_sender() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0);
        pool.add_transaction(make_tx(2, "alice", 200, 20), 1); // replaces
        assert_eq!(pool.len(), 1);
        // The new tx's fee should be reflected.
        assert_eq!(pool.pending_debit("alice"), 220);
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
    fn pending_debit_returns_amount_plus_fee_for_sender() {
        let mut pool = Mempool::new(None);
        pool.add_transaction(make_tx(1, "alice", 100, 10), 0); // debit 110
        pool.add_transaction(make_tx(2, "bob", 200, 20), 0); // not alice
        assert_eq!(pool.pending_debit("alice"), 110);
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
        pool.readd_displaced(&[coinbase(1)], |_| 1000, |_| 0, 10);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn readd_displaced_skips_already_present() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(1, "alice", 100, 10);
        pool.add_transaction(tx.clone(), 0);
        pool.readd_displaced(&[tx], |_| 1000, |_| 0, 10);
        assert_eq!(pool.len(), 1); // still 1, not 2
    }

    #[test]
    fn readd_displaced_skips_unaffordable() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(1, "alice", 100, 10); // cost = 110
        pool.readd_displaced(&[tx], |_| 50, |_| 0, 10); // balance 50 < 110
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn readd_displaced_skips_stale_nonce() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(1, "alice", 100, 10); // nonce = 0
                                               // chain nonce is now 1 (tx was already confirmed), so nonce 0 is stale
        pool.readd_displaced(&[tx], |_| 1000, |_| 1, 10);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn readd_displaced_adds_affordable_tx() {
        let mut pool = Mempool::new(None);
        let tx = make_tx(1, "alice", 100, 10); // cost = 110, nonce = 0
        pool.readd_displaced(&[tx], |_| 200, |_| 0, 10); // balance 200 >= 110, nonce matches
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn readd_displaced_accounts_for_existing_pending_slot() {
        let mut pool = Mempool::new(None);
        // alice already has a pending tx — displaced tx for same sender is skipped
        pool.add_transaction(make_tx(9, "alice", 140, 10), 0);
        let tx = make_tx(1, "alice", 55, 5);
        pool.readd_displaced(&[tx], |_| 200, |_| 0, 10);
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

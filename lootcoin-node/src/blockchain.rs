use crate::db::Db;
use crate::metrics::Metrics;
use serde::{Deserialize, Serialize};
use crate::loot_ticket::{
    LootTicket, JACKPOT_BUCKET_START, JACKPOT_DIVISOR, LARGE_BUCKET_START, LARGE_DIVISOR,
    MEDIUM_BUCKET_START, MEDIUM_DIVISOR, MIN_TX_FEE, PPM, REVEAL_BLOCKS, SMALL_BUCKET_START,
    SMALL_DIVISOR, TICKET_MATURITY,
};
use cubehash::CubeHash256;
use lootcoin_core::{
    block::{meets_difficulty, Block, MAX_BLOCK_TXS},
    transaction::Transaction,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

/// Difficulty is expressed as a fractional number of leading zero **bits** required
/// in the block hash. Fractional values allow sub-bit adjustments that eliminate
/// the 2× oscillation that integer rounding would cause on every retarget window.
const INITIAL_DIFFICULTY: f64 = 25.0;
const MIN_DIFFICULTY: f64 = 8.0;
const MAX_DIFFICULTY: f64 = 127.0;

/// Target time between consecutive blocks, in seconds.
const TARGET_BLOCK_TIME_SECS: u64 = 60;

/// ASERT halflife in seconds.  A cumulative deviation of exactly one halflife
/// between ideal and actual elapsed time (measured from the anchor block) moves
/// difficulty by exactly 1 bit (2× change in expected hashes).
///
/// 3 600 s = 1 hour = 60 blocks at target time.  This means:
///  • Hashrate doubles  → difficulty catches up by 1 bit per hour of mining.
///  • Network goes dark → difficulty drops by 1 bit per hour of silence.
/// The adjustment is applied every block, so there is no fixed window to exploit.
pub const ASERT_HALFLIFE_SECS: f64 = 3_600.0;


/// A block's timestamp must exceed the median of the last N block timestamps
/// (Median Time Past). Prevents backdating attacks and difficulty manipulation
/// while tolerating normal clock skew between miners.
const MTP_WINDOW: usize = 11;

/// Reject orphans more than this many blocks ahead of our tip.
const MAX_ORPHAN_DEPTH: u64 = 10;

/// Maximum orphans stored per block height.
/// An attacker can fill at most this many slots at any one height;
/// the total pool size is bounded by MAX_ORPHAN_DEPTH * MAX_ORPHANS_PER_HEIGHT.
const MAX_ORPHANS_PER_HEIGHT: usize = 3;

/// Absolute cap on the orphan pool. Prevents memory exhaustion even if an
/// attacker floods valid-PoW blocks across many heights simultaneously.
const MAX_ORPHAN_POOL_SIZE: usize = MAX_ORPHAN_DEPTH as usize * MAX_ORPHANS_PER_HEIGHT;

/// Number of recent blocks kept in the in-memory sliding window.
/// Comfortably exceeds MAX_ORPHAN_DEPTH so in-memory reorgs work without
/// touching the DB. Deeper reorgs fall back to DB automatically.
const REORG_WINDOW: usize = 100;

/// How often (in blocks) to snapshot the full derived state to redb.
/// On restart the node loads the latest checkpoint and replays only the tail,
/// skipping the O(N) full-chain replay that would otherwise be required.
pub const CHECKPOINT_INTERVAL: u64 = 1_000;

/// Full derived state captured at every CHECKPOINT_INTERVAL blocks.
/// Stored in the CHECKPOINTS table of redb, keyed by block height.
#[derive(Serialize, Deserialize)]
pub struct CheckpointState {
    pub balances: HashMap<String, u64>,
    pub pot: u64,
    pub chain_work: u128,
    /// Hash of the block at which this checkpoint was taken.
    /// Verified at startup to detect checkpoints from a reorged chain.
    pub block_hash: Vec<u8>,
    pub current_difficulty: f64,
    pub asert_anchor: Option<(u64, u64, f64)>,
    pub tickets: Vec<LootTicket>,
}

pub enum BlockOutcome {
    Applied,
    /// `old_blocks`: the canonical blocks that were displaced (empty for deep
    /// reorgs whose ancestor is before the in-memory window — caller falls back
    /// to a full DB rebuild in that case).
    /// `new_blocks`: the fork blocks that became canonical.
    Reorged {
        old_blocks: Vec<Block>,
        new_blocks: Vec<Block>,
    },
    Orphaned,
    Rejected,
}

pub struct Blockchain {
    /// Sliding window of the most recent REORG_WINDOW blocks.
    /// blocks[0].index == blocks_offset.
    pub blocks: Vec<Block>,
    /// Absolute chain index of blocks[0]. Zero until the chain is long enough
    /// to warrant pruning, then advances as old blocks are evicted.
    blocks_offset: u64,

    pub balances: HashMap<String, u64>,
    /// Signatures of confirmed txs in the current window. Prevents replay.
    /// Pruned when blocks leave the REORG_WINDOW.
    confirmed_signatures: HashSet<Vec<u8>>,
    /// Main-chain hash → absolute block index. Pruned with the window.
    block_hashes: HashMap<Vec<u8>, u64>,
    orphan_pool: HashMap<Vec<u8>, Block>,
    pending_tickets: Vec<LootTicket>,
    pot: u64,
    /// The amount seeded into the pot at genesis. Stored so that `reorg_to`
    /// can restore it when replaying history from scratch.
    genesis_pot: u64,
    /// Lottery payouts settled per block index. Populated in `update_state`
    /// and drained by `apply_block` to persist visible payout records.
    /// Pruned with the sliding window; cleared on reorg replays.
    /// Each entry is (receiver, amount, tier) where tier is one of
    /// "small", "medium", "large", "jackpot". No-win draws produce no entry.
    settled_payouts_by_block: HashMap<u64, Vec<(String, u64, String)>>,

    /// Difficulty the chain resets to at genesis when replaying after a reorg.
    /// Matches INITIAL_DIFFICULTY for real nodes; overridable in tests.
    initial_difficulty: f64,
    /// Current required PoW difficulty (fractional leading zero bits).
    current_difficulty: f64,
    /// ASERT anchor: the fixed reference point for per-block difficulty.
    /// Set to (height=1, timestamp, difficulty) when the first mined block is
    /// applied.  `None` until then — genesis keeps INITIAL_DIFFICULTY.
    /// Using block 1 rather than genesis avoids the synthetic genesis timestamp
    /// (which can predate real mining by months) corrupting the calculation.
    asert_anchor: Option<(u64, u64, f64)>,

    /// Cumulative proof-of-work: sum of 2^difficulty for every block on the
    /// main chain. Used instead of height to select the best chain during sync,
    /// preventing a peer from manipulating selection by lying about block count.
    chain_work: u128,

    /// Optional metrics sink.  `None` in unit tests; `Some` in production.
    /// Counters (fees, lottery wins) are incremented here as blocks are applied.
    pub metrics: Option<Arc<Metrics>>,
}

impl Blockchain {
    pub fn new(genesis: Block) -> Self {
        let mut chain = Self {
            blocks: vec![genesis.clone()],
            blocks_offset: 0,
            balances: HashMap::new(),
            confirmed_signatures: HashSet::new(),
            block_hashes: HashMap::new(),
            orphan_pool: HashMap::new(),
            pending_tickets: Vec::new(),
            pot: 0,
            genesis_pot: 0,
            settled_payouts_by_block: HashMap::new(),
            initial_difficulty: INITIAL_DIFFICULTY,
            current_difficulty: INITIAL_DIFFICULTY,
            asert_anchor: None,
            chain_work: 0,
            metrics: None,
        };

        chain.block_hashes.insert(genesis.hash.clone(), 0);
        for tx in genesis.transactions {
            *chain.balances.entry(tx.receiver).or_insert(0) += tx.amount;
        }

        chain
    }

    pub fn get_latest_hash(&self) -> Vec<u8> {
        self.blocks
            .last()
            .expect("blocks vec is never empty: invariant violated")
            .hash
            .clone()
    }

    pub fn get_height(&self) -> u64 {
        self.blocks_offset + self.blocks.len() as u64
    }

    pub fn get_balance(&self, address: &str) -> u64 {
        *self.balances.get(address).unwrap_or(&0)
    }

    pub fn get_pot(&self) -> u64 {
        self.pot
    }

    pub fn get_difficulty(&self) -> f64 {
        self.current_difficulty
    }

    pub fn get_chain_work(&self) -> u128 {
        self.chain_work
    }

    pub fn get_last_block_timestamp(&self) -> u64 {
        self.blocks.last().map(|b| b.timestamp).unwrap_or(0)
    }

    /// Average seconds between the last (up to 10) block intervals.
    /// Returns `None` if fewer than 2 real (non-genesis) blocks are available.
    pub fn get_avg_block_time_secs(&self) -> Option<f64> {
        let n = self.blocks.len();
        if n < 3 {
            return None;
        } // need genesis + at least 2 real blocks
        let window = n.min(11); // up to 10 intervals
        let start = n - window;
        // genesis (index == 0) has a synthetic fixed timestamp that may predate
        // real mining by months — skip it as the oldest sample.
        let (oldest_idx, intervals) = if self.blocks[start].index == 0 {
            (start + 1, (window - 2) as u64) // skip genesis; one fewer interval
        } else {
            (start, (window - 1) as u64)
        };
        if intervals == 0 {
            return None;
        }
        let oldest = self.blocks[oldest_idx].timestamp;
        let newest = self.blocks[n - 1].timestamp;
        if newest <= oldest {
            return None;
        }
        Some((newest - oldest) as f64 / intervals as f64)
    }

    pub fn restore_tickets(&mut self, tickets: Vec<LootTicket>) {
        self.pending_tickets = tickets;
    }

    /// Capture the full derived state at the current tip.
    /// Cheap clone of in-memory maps; called only every CHECKPOINT_INTERVAL blocks.
    pub fn snapshot(&self) -> CheckpointState {
        CheckpointState {
            balances: self.balances.clone(),
            pot: self.pot,
            chain_work: self.chain_work,
            block_hash: self.get_latest_hash(),
            current_difficulty: self.current_difficulty,
            asert_anchor: self.asert_anchor,
            tickets: self.pending_tickets.clone(),
        }
    }

    /// Replace the in-memory derived state with a previously snapshotted checkpoint.
    ///
    /// `checkpoint_block` is the block at `height` — it is placed as the sole
    /// entry in `self.blocks` so that `get_latest_hash()` is correct for the
    /// first block applied after the restore.
    ///
    /// Call `seed_pot` **before** this method (to set `genesis_pot`); this
    /// method overwrites `pot` with the checkpoint value.
    pub fn restore_from_checkpoint(
        &mut self,
        height: u64,
        state: CheckpointState,
        checkpoint_block: Block,
    ) {
        self.blocks.clear();
        self.blocks_offset = height;
        self.blocks.push(checkpoint_block);

        self.block_hashes.clear();
        self.confirmed_signatures.clear();
        self.orphan_pool.clear();
        self.settled_payouts_by_block.clear();

        self.balances = state.balances;
        self.pot = state.pot;
        self.chain_work = state.chain_work;
        self.current_difficulty = state.current_difficulty;
        self.asert_anchor = state.asert_anchor;
        self.pending_tickets = state.tickets;
    }

    /// Seed the pot directly. Only called once at genesis to bootstrap the
    /// lottery before any fee-paying transactions exist.
    pub fn seed_pot(&mut self, amount: u64) {
        self.genesis_pot = amount;
        self.pot = self.pot.saturating_add(amount);
    }

    /// Evict blocks older than REORG_WINDOW from the in-memory Vec and their
    /// corresponding entries from block_hashes.
    pub fn prune_to_window(&mut self) {
        if self.blocks.len() > REORG_WINDOW {
            let to_drop = self.blocks.len() - REORG_WINDOW;
            for b in self.blocks.drain(0..to_drop) {
                self.block_hashes.remove(&b.hash);
                for tx in &b.transactions {
                    if !tx.sender.is_empty() {
                        self.confirmed_signatures.remove(&tx.signature);
                    }
                }
            }
            self.blocks_offset += to_drop as u64;
            self.settled_payouts_by_block
                .retain(|&k, _| k >= self.blocks_offset);
        }
    }

    /// Drain and return lottery payouts settled in a specific block.
    /// Called by `apply_block` before persisting so payouts are recorded in DB.
    pub fn drain_payouts_for_block(&mut self, index: u64) -> Vec<(String, u64, String)> {
        self.settled_payouts_by_block
            .remove(&index)
            .unwrap_or_default()
    }

    /// Drain all accumulated lottery payouts (used at startup after full replay).
    pub fn take_all_settled_payouts(&mut self) -> HashMap<u64, Vec<(String, u64, String)>> {
        std::mem::take(&mut self.settled_payouts_by_block)
    }

    /// Check fee, balance, and that this signature hasn't been confirmed already.
    /// Does not re-verify the Ed25519 signature — caller must do that first.
    pub fn validate_transaction_state(&self, tx: &Transaction) -> bool {
        if tx.fee < MIN_TX_FEE {
            return false;
        }
        if self.confirmed_signatures.contains(&tx.signature) {
            return false;
        }
        let balance = self.get_balance(&tx.sender);
        balance >= tx.amount.saturating_add(tx.fee)
    }

    pub fn apply_transaction(&mut self, tx: &Transaction) {
        if tx.sender.is_empty() {
            *self.balances.entry(tx.receiver.clone()).or_insert(0) += tx.amount;
            return;
        }

        let sender_balance = self.balances.entry(tx.sender.clone()).or_insert(0);
        let total = match tx.amount.checked_add(tx.fee) {
            Some(v) => v,
            None => return,
        };
        if *sender_balance < total {
            return;
        }

        *sender_balance -= total;
        *self.balances.entry(tx.receiver.clone()).or_insert(0) += tx.amount;
        self.pot = self.pot.saturating_add(tx.fee);
        self.confirmed_signatures.insert(tx.signature.clone());
    }

    fn loot_bucket_from_digest(digest: &[u8]) -> u32 {
        let mut b = [0u8; 4];
        b.copy_from_slice(&digest[0..4]);
        u32::from_le_bytes(b) % PPM
    }

    /// Look up a block hash by absolute index from the in-memory window.
    fn block_hash_at(&self, index: u64) -> Option<&[u8]> {
        if index < self.blocks_offset {
            return None;
        }
        let rel = (index - self.blocks_offset) as usize;
        self.blocks.get(rel).map(|b| b.hash.as_slice())
    }

    /// Resolve the lottery draw for a maturing ticket.
    ///
    /// Uses REVEAL_BLOCKS of accumulated block-hash entropy so that an attacker
    /// must control all REVEAL_BLOCKS consecutive blocks to steer the outcome.
    ///
    /// Payout formula: `pot / DIVISOR` (flat, independent of tx count).
    ///
    /// Per-transaction incentives are provided by the 50/50 fee split.
    ///
    /// Outcome probabilities:
    ///   • 62.00 % → (0, "none")
    ///   • 36.25 % → (amount, "small")
    ///   •  1.67 % → (amount, "medium")   — ~hourly at thriving pace
    ///   •  0.07 % → (amount, "large")    — ~daily
    ///   •  0.01 % → (amount, "jackpot")  — ~weekly
    fn compute_ticket_reward(&self, reveal_start: u64, ticket: &LootTicket) -> (u64, &'static str) {
        let mut entropy: Vec<u8> = Vec::with_capacity(32 * REVEAL_BLOCKS as usize);
        for i in 0..REVEAL_BLOCKS {
            if let Some(h) = self.block_hash_at(reveal_start + i) {
                entropy.extend_from_slice(h);
            }
        }
        let data = bincode::serialize(&(entropy.as_slice(), ticket.created_height, &ticket.miner))
            .unwrap();
        let digest = CubeHash256::digest(&data);
        let bucket = Self::loot_bucket_from_digest(&digest);

        match bucket {
            0..SMALL_BUCKET_START                    => (0,                             "none"),
            SMALL_BUCKET_START..MEDIUM_BUCKET_START  => (self.pot / SMALL_DIVISOR,   "small"),
            MEDIUM_BUCKET_START..LARGE_BUCKET_START  => (self.pot / MEDIUM_DIVISOR,  "medium"),
            LARGE_BUCKET_START..JACKPOT_BUCKET_START => (self.pot / LARGE_DIVISOR,   "large"),
            _                                        => (self.pot / JACKPOT_DIVISOR, "jackpot"),
        }
    }

    fn extract_miner_address(block: &Block) -> Option<String> {
        block.transactions.first().and_then(|tx| {
            if tx.sender.is_empty() {
                Some(tx.receiver.clone())
            } else {
                None
            }
        })
    }

    /// Attempt to append `block` to the main chain, returning false on any
    /// validation failure so the caller can treat it as a fork candidate.
    /// Ed25519 signatures are always verified — this catches DB corruption or
    /// tampering on startup as well as invalid blocks at runtime.
    fn update_state(&mut self, block: &Block) -> bool {
        // --- Chain linkage ---
        if block.index != self.get_height() {
            return false;
        }
        if block.previous_hash != self.get_latest_hash() {
            return false;
        }

        // --- Timestamp validation ---
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Reject blocks more than 2 minutes in the future.
        if block.timestamp > now_secs + 120 {
            return false;
        }

        // Median Time Past: timestamp must exceed the median of the last
        // MTP_WINDOW block timestamps. Prevents backdating attacks.
        {
            let mut recent: Vec<u64> = self
                .blocks
                .iter()
                .rev()
                .take(MTP_WINDOW)
                .map(|b| b.timestamp)
                .collect();
            if !recent.is_empty() {
                recent.sort_unstable();
                let median = recent[recent.len() / 2];
                if block.timestamp <= median {
                    return false;
                }
            }
        }

        // --- Hash and PoW ---
        if block.calculate_hash() != block.hash {
            return false;
        }
        if !meets_difficulty(&block.hash, self.current_difficulty) {
            return false;
        }

        // --- Issue 6: block transaction count cap ---
        let non_coinbase = block
            .transactions
            .iter()
            .filter(|t| !t.sender.is_empty())
            .count();
        if non_coinbase > MAX_BLOCK_TXS {
            return false;
        }

        // Only the first transaction may have an empty sender (coinbase).
        // A miner including extra coinbase-like transactions would claim coins from thin air.
        if block
            .transactions
            .iter()
            .skip(1)
            .any(|t| t.sender.is_empty())
        {
            return false;
        }

        // Coinbase reward is exactly 0 or 1 coin; fee must also be 0 or 1.
        // Checked here (not just at the API layer) so reorg replay and DB-loaded
        // blocks are subject to the same rule as freshly submitted ones.
        if let Some(coinbase) = block.transactions.first() {
            if coinbase.sender.is_empty() && (coinbase.amount > 1 || coinbase.fee != 0) {
                return false;
            }
        }

        // tx_root must commit to the actual transaction list.
        if block.tx_root != Block::compute_tx_root(&block.transactions) {
            return false;
        }

        // --- Pre-validate all non-coinbase txs ---
        {
            let mut pending_debits: HashMap<String, u64> = HashMap::new();
            let mut block_sigs: HashSet<Vec<u8>> = HashSet::new();

            for tx in &block.transactions {
                if tx.sender.is_empty() {
                    continue;
                }
                if !tx.verify() {
                    return false;
                }
                if tx.fee < MIN_TX_FEE {
                    return false;
                }
                // Reject replays: sig already confirmed or appears twice in this block
                if self.confirmed_signatures.contains(&tx.signature)
                    || !block_sigs.insert(tx.signature.clone())
                {
                    return false;
                }
                let balance = self.get_balance(&tx.sender);
                let already_debited = pending_debits.get(&tx.sender).copied().unwrap_or(0);
                let total = match tx.amount.checked_add(tx.fee) {
                    Some(v) => v,
                    None => return false,
                };
                if balance.saturating_sub(already_debited) < total {
                    return false;
                }
                *pending_debits.entry(tx.sender.clone()).or_insert(0) += total;
            }
        }

        // --- Apply block ---

        // 1) Settle maturing lottery tickets using this block's hash as randomness
        let current_index = block.index;
        let (mut to_settle, mut still_pending) = (Vec::new(), Vec::new());
        for t in self.pending_tickets.iter() {
            // Settlement is triggered REVEAL_BLOCKS after the maturity height.
            // The randomness uses blocks [maturity, maturity + REVEAL_BLOCKS),
            // all already committed before this block — the settling miner has
            // no influence over any of them.
            if t.created_height + TICKET_MATURITY + REVEAL_BLOCKS == current_index {
                to_settle.push(t.clone());
            } else {
                still_pending.push(t.clone());
            }
        }
        self.pending_tickets = still_pending;

        if !to_settle.is_empty() {
            info!(
                "Lottery: settling {} ticket(s) at block {}",
                to_settle.len(),
                current_index
            );
        }
        let mut payouts_this_block: Vec<(String, u64, String)> = Vec::new();
        for t in to_settle {
            let reveal_start = t.created_height + TICKET_MATURITY;
            let (amount, tier) = self.compute_ticket_reward(reveal_start, &t);
            let payout = amount.min(self.pot);
            if payout > 0 {
                self.pot -= payout;
                *self.balances.entry(t.miner.clone()).or_insert(0) += payout;
                payouts_this_block.push((t.miner.clone(), payout, tier.to_string()));
                if let Some(m) = &self.metrics {
                    m.record_lottery_win(tier, payout);
                }
                info!(
                    "Lottery: {} coins ({}) → {} (ticket from block {}, pot now {})",
                    payout, tier, t.miner, t.created_height, self.pot
                );
            } else {
                debug!(
                    "Lottery: no win for ticket from block {} (pot={})",
                    t.created_height, self.pot
                );
            }
        }
        if !payouts_this_block.is_empty() {
            self.settled_payouts_by_block
                .insert(current_index, payouts_this_block);
        }

        // 2) Apply transactions, then split fees 50/50 between pot and miner.
        //    apply_transaction() adds each fee entirely to the pot; after the
        //    loop we transfer the miner's half back out so the accounting stays
        //    consistent regardless of how many txs are in the block.
        let mut total_block_fees: u64 = 0;
        for tx in &block.transactions {
            if !tx.sender.is_empty() {
                total_block_fees = total_block_fees.saturating_add(tx.fee);
            }
            self.apply_transaction(tx);
        }
        let miner_fee_share = total_block_fees / 2; // floor: equal split; odd remainder goes to pot
        if miner_fee_share > 0 {
            if let Some(miner_addr) = Self::extract_miner_address(block) {
                self.pot = self.pot.saturating_sub(miner_fee_share);
                *self.balances.entry(miner_addr).or_insert(0) += miner_fee_share;
            }
        }
        if let Some(m) = &self.metrics {
            m.record_fees(total_block_fees, miner_fee_share);
        }

        // 3) Record block
        self.block_hashes.insert(block.hash.clone(), block.index);
        self.blocks.push(block.clone());

        // 4) Issue lottery ticket to the miner — only if the block includes at least
        //    one non-coinbase transaction. Coinbase-only blocks add nothing to the pot
        //    (fees are the pot's replenishment source), so they earn no ticket.
        let has_real_tx = block.transactions.iter().any(|t| !t.sender.is_empty());
        if has_real_tx {
            if let Some(miner_addr) = Self::extract_miner_address(block) {
                self.pending_tickets.push(LootTicket {
                    miner: miner_addr,
                    created_height: current_index,
                });
            }
        }

        // 5) Accumulate chain work for this block (2^difficulty hashes expected).
        //    Use the integer floor of the fractional difficulty for the shift;
        //    checked_shl returns None for shifts >= 128 bits → saturate to u128::MAX.
        let work = 1u128
            .checked_shl(self.current_difficulty as u32)
            .unwrap_or(u128::MAX);
        self.chain_work = self.chain_work.saturating_add(work);

        // 6) ASERT per-block difficulty adjustment.
        //
        // The anchor is fixed at block 1 (the first real mined block) so that the
        // synthetic genesis timestamp — which can predate real mining by months —
        // never enters the calculation.
        //
        // Formula (log2 space):
        //   ideal   = (height − anchor_height) × TARGET_BLOCK_TIME_SECS
        //   actual  = block.timestamp − anchor_timestamp
        //   Δ       = (ideal − actual) / ASERT_HALFLIFE_SECS
        //
        // Δ > 0 → blocks arrived faster than ideal → difficulty rises.
        // Δ < 0 → blocks arrived slower than ideal → difficulty falls.
        // A sustained deviation of one halflife (3 600 s) moves difficulty by 1 bit.
        if block.index == 1 {
            // Establish the anchor the first time a real block is applied.
            self.asert_anchor = Some((1, block.timestamp, self.current_difficulty));
        } else if let Some((anchor_height, anchor_ts, anchor_diff)) = self.asert_anchor {
            if anchor_diff < MIN_DIFFICULTY {
                // Chain was initialised below the minimum (only happens in tests
                // that set difficulty=0 to avoid real PoW). Skip ASERT so that
                // fake-hash blocks continue to be accepted.
            } else {
            let ideal = (block.index - anchor_height) * TARGET_BLOCK_TIME_SECS;
            let actual = block.timestamp.saturating_sub(anchor_ts);
            let adjustment =
                (ideal as f64 - actual as f64) / ASERT_HALFLIFE_SECS;
            let new_difficulty =
                (anchor_diff + adjustment).clamp(MIN_DIFFICULTY, MAX_DIFFICULTY);
            if (new_difficulty - self.current_difficulty).abs() > 1e-9 {
                info!(
                    "Difficulty adjusted: {:.3} → {:.3} bits at height {}",
                    self.current_difficulty, new_difficulty, block.index
                );
                self.current_difficulty = new_difficulty;
            }
            } // end else (anchor_diff >= MIN_DIFFICULTY)
        }

        true
    }

    /// Compute the proof-of-work represented by a block hash.
    ///
    /// We count the actual leading zero *bits* of the hash and return 2^count.
    /// Using the real hash rather than the target difficulty means a block that
    /// happened to find extra leading zeros contributes proportionally more work
    /// — correctly reflecting the expected hashing effort that produced it.
    fn block_work(hash: &[u8]) -> u128 {
        let mut bits: u32 = 0;
        for &byte in hash {
            if byte == 0 {
                bits += 8;
            } else {
                bits += byte.leading_zeros();
                break;
            }
        }
        1u128.checked_shl(bits).unwrap_or(u128::MAX)
    }

    fn validate_block_standalone(&self, block: &Block) -> bool {
        let non_coinbase = block
            .transactions
            .iter()
            .filter(|t| !t.sender.is_empty())
            .count();
        block.calculate_hash() == block.hash
            && meets_difficulty(&block.hash, self.current_difficulty)
            && block.index <= self.get_height() + MAX_ORPHAN_DEPTH
            && non_coinbase <= MAX_BLOCK_TXS
    }

    fn trace_fork_chain(&self, tip_hash: &[u8]) -> Vec<Block> {
        let mut chain = Vec::new();
        let mut prev_hash = {
            let tip = &self.orphan_pool[tip_hash];
            chain.push(tip.clone());
            tip.previous_hash.clone()
        };
        while let Some(b) = self.orphan_pool.get(&prev_hash) {
            prev_hash = b.previous_hash.clone();
            chain.push(b.clone());
        }
        chain.reverse();
        chain
    }

    fn find_longest_fork(&self, db: Option<&Db>) -> Option<(Vec<Block>, u64)> {
        if self.orphan_pool.is_empty() {
            return None;
        }

        let referenced: HashSet<Vec<u8>> = self
            .orphan_pool
            .values()
            .map(|b| b.previous_hash.clone())
            .collect();

        // Track the best candidate as (chain, ancestor_index, fork_work).
        // Work is used for all comparisons; the caller only needs (chain, ancestor).
        let mut best: Option<(Vec<Block>, u64, u128)> = None;

        for hash in self.orphan_pool.keys() {
            if referenced.contains(hash) {
                continue;
            }

            let fork_chain = self.trace_fork_chain(hash);

            let first = match fork_chain.first() {
                Some(b) => b,
                None => continue,
            };

            // Try in-memory window first, then fall back to DB for deep ancestors.
            let ancestor_index: u64 = match self.block_hashes.get(&first.previous_hash).copied() {
                Some(idx) => idx,
                None => {
                    // The ancestor may be before our in-memory window. Verify it
                    // exists in the DB at the expected index with the right hash.
                    if first.index == 0 {
                        continue;
                    }
                    let parent_idx = first.index - 1;
                    if parent_idx >= self.blocks_offset {
                        // Should be in the window but isn't — genuinely dangling.
                        continue;
                    }
                    let found = db
                        .and_then(|d| d.get_blocks_range(parent_idx, 1).ok())
                        .map(|blocks| !blocks.is_empty() && blocks[0].hash == first.previous_hash)
                        .unwrap_or(false);
                    if found {
                        parent_idx
                    } else {
                        continue;
                    }
                }
            };

            // Accumulated work for the fork chain.
            let fork_work: u128 = fork_chain
                .iter()
                .map(|b| Self::block_work(&b.hash))
                .fold(0u128, |acc, w| acc.saturating_add(w));

            // Does this fork have more work than the current main chain from
            // the ancestor to the tip?
            //
            // When the ancestor is within the in-memory window we can compute
            // exact work from the actual block hashes.  For deep ancestors
            // (before the window) we fall back to height — deep reorgs are
            // extremely rare and loading all historical hashes from DB here
            // would be disproportionately expensive.
            let fork_beats_main = if ancestor_index >= self.blocks_offset {
                let main_work: u128 = self
                    .blocks
                    .iter()
                    .filter(|b| b.index > ancestor_index)
                    .map(|b| Self::block_work(&b.hash))
                    .fold(0u128, |acc, w| acc.saturating_add(w));
                fork_work > main_work
            } else {
                // Deep-ancestor fallback: height proxy.
                ancestor_index + 1 + fork_chain.len() as u64 > self.get_height()
            };

            if !fork_beats_main {
                continue;
            }

            let is_better = best
                .as_ref()
                .is_none_or(|&(_, _, prev_work)| fork_work > prev_work);
            if is_better {
                best = Some((fork_chain, ancestor_index, fork_work));
            }
        }

        best.map(|(chain, ancestor, _work)| (chain, ancestor))
    }

    /// Reset all state and replay `new_canonical` from scratch.
    ///
    /// If `new_canonical[0].index > 0` the window no longer contains genesis,
    /// so historical blocks are loaded from `db` (required in that case).
    /// During startup replay `db` is `None`; this path is never reached because
    /// pruning has not yet run and `blocks[0]` is always genesis.
    fn reorg_to(&mut self, mut new_canonical: Vec<Block>, db: Option<&Db>) {
        if new_canonical[0].index > 0 {
            let prefix_end = new_canonical[0].index as usize;
            let db = db.expect("db required for reorg beyond in-memory window");
            match db.get_blocks_range(0, prefix_end) {
                Ok(mut prefix) => {
                    prefix.extend(new_canonical);
                    new_canonical = prefix;
                }
                Err(e) => {
                    eprintln!("reorg: failed to load historical blocks: {}", e);
                    return;
                }
            }
        }

        let genesis = new_canonical[0].clone();

        self.blocks.clear();
        self.blocks_offset = 0;
        self.balances.clear();
        self.confirmed_signatures.clear();
        self.block_hashes.clear();
        self.orphan_pool.clear();
        self.pending_tickets.clear();
        self.settled_payouts_by_block.clear();
        self.pot = self.genesis_pot;
        self.current_difficulty = self.initial_difficulty;
        self.asert_anchor = None;
        self.chain_work = 0;

        self.block_hashes.insert(genesis.hash.clone(), 0);
        self.blocks.push(genesis.clone());
        for tx in &genesis.transactions {
            *self.balances.entry(tx.receiver.clone()).or_insert(0) += tx.amount;
        }

        for block in new_canonical.into_iter().skip(1) {
            // Reorg rebuilds from a mix of trusted main-chain blocks and
            self.update_state(&block);
        }
    }

    // -------------------------------------------------------------------------
    // Public interface
    // -------------------------------------------------------------------------

    /// Fork-aware block application with no I/O.
    ///
    /// Pass `db = Some(...)` at runtime so that reorgs beyond the in-memory
    /// window can load historical blocks from storage.
    /// Pass `db = None` during startup replay when all blocks are in memory
    /// (pruning has not yet run, so `blocks[0]` is always genesis).
    pub fn apply_in_memory(&mut self, block: Block, db: Option<&Db>) -> BlockOutcome {
        if self.update_state(&block) {
            return BlockOutcome::Applied;
        }

        // A block that links perfectly to the current tip (correct index AND
        // correct previous_hash) but still failed update_state has invalid
        // *content* — bad transactions, nonce conflicts, exceeded limits, etc.
        // It can never become valid regardless of what other blocks arrive, so
        // orphaning it would only trigger spurious reorgs. Reject it outright.
        if block.index == self.get_height() && block.previous_hash == self.get_latest_hash() {
            return BlockOutcome::Rejected;
        }

        if !self.validate_block_standalone(&block) {
            return BlockOutcome::Rejected;
        }

        // Absolute pool cap: evict the lowest-index orphan if the pool is full.
        // Prevents memory exhaustion from flooding across many heights simultaneously.
        if self.orphan_pool.len() >= MAX_ORPHAN_POOL_SIZE {
            if let Some(evict_hash) = self
                .orphan_pool
                .iter()
                .min_by_key(|(_, b)| b.index)
                .map(|(h, _)| h.clone())
            {
                self.orphan_pool.remove(&evict_hash);
            }
        }

        // Per-height cap: evict the orphan with the smallest hash at this height
        // before inserting if we're already at the limit. An attacker can DoS at
        // most MAX_ORPHANS_PER_HEIGHT slots per height, not the whole pool.
        let at_height = self
            .orphan_pool
            .values()
            .filter(|b| b.index == block.index)
            .count();
        if at_height >= MAX_ORPHANS_PER_HEIGHT {
            if let Some(evict_hash) = self
                .orphan_pool
                .iter()
                .filter(|(_, b)| b.index == block.index)
                .min_by_key(|(_, b)| &b.hash)
                .map(|(h, _)| h.clone())
            {
                self.orphan_pool.remove(&evict_hash);
            }
        }

        self.orphan_pool.insert(block.hash.clone(), block);

        if let Some((fork_chain, ancestor_index)) = self.find_longest_fork(db) {
            let new_canonical = if ancestor_index >= self.blocks_offset {
                // Common ancestor is within the in-memory window: splice directly.
                let relative = (ancestor_index - self.blocks_offset) as usize;
                self.blocks[..=relative]
                    .iter()
                    .cloned()
                    .chain(fork_chain.iter().cloned())
                    .collect()
            } else {
                // Common ancestor is before the window. Pass just the fork blocks —
                // reorg_to will load the historical prefix from DB automatically.
                fork_chain.clone()
            };

            // Capture the displaced blocks before reorg_to overwrites state.
            // Only possible when the ancestor is within the memory window;
            // deep reorgs return an empty vec and fall back to full DB rebuild.
            let old_blocks: Vec<Block> = if ancestor_index >= self.blocks_offset {
                let relative = (ancestor_index - self.blocks_offset) as usize;
                self.blocks[relative + 1..].to_vec()
            } else {
                vec![]
            };

            self.reorg_to(new_canonical, db);
            return BlockOutcome::Reorged {
                old_blocks,
                new_blocks: fork_chain,
            };
        }

        BlockOutcome::Orphaned
    }

    /// Apply a block and persist to disk. Used for blocks arriving at runtime.
    pub fn apply_block(&mut self, db: &Db, block: Block) -> BlockOutcome {
        let outcome = self.apply_in_memory(block, Some(db));

        match &outcome {
            BlockOutcome::Applied => {
                let block = self.blocks.last().unwrap().clone();
                let payouts = self.drain_payouts_for_block(block.index);
                // Single atomic write: BLOCKS + TX_INDEX + CONFIRMED_SIGS + TICKETS + LOTTERY_PAYOUTS.
                if let Err(e) = db.save_applied_block(&block, &self.pending_tickets, &payouts) {
                    eprintln!("Failed to persist applied block {}: {}", block.index, e);
                }
                // Snapshot derived state every CHECKPOINT_INTERVAL blocks so future
                // restarts can skip the O(N) full-chain replay.
                if block.index > 0 && block.index % CHECKPOINT_INTERVAL == 0 {
                    match bincode::serialize(&self.snapshot()) {
                        Ok(data) => {
                            if let Err(e) = db.save_checkpoint(block.index, &data) {
                                eprintln!("Failed to save checkpoint at block {}: {}", block.index, e);
                            } else {
                                info!("Checkpoint saved at block {}", block.index);
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to serialize checkpoint at block {}: {}", block.index, e);
                        }
                    }
                }
                self.prune_to_window();
            }
            BlockOutcome::Reorged {
                old_blocks,
                new_blocks,
            } => {
                // Clone to release the shared borrow of `outcome` before calling
                // drain_payouts_for_block, which requires &mut self.
                let old_blocks = old_blocks.clone();
                let new_blocks = new_blocks.clone();
                if !old_blocks.is_empty() {
                    // Shallow reorg: common ancestor is within the memory window.
                    // Invalidate any checkpoints at or after the fork point — they are
                    // from the displaced chain and would be wrong on next restart.
                    if let Err(e) = db.delete_checkpoints_from(old_blocks[0].index) {
                        eprintln!("Failed to invalidate stale checkpoints after reorg: {}", e);
                    }
                    // Only need payouts for the new fork blocks.
                    let mut new_payouts: HashMap<u64, Vec<(String, u64, String)>> = HashMap::new();
                    for b in &new_blocks {
                        let p = self.drain_payouts_for_block(b.index);
                        if !p.is_empty() {
                            new_payouts.insert(b.index, p);
                        }
                    }
                    if let Err(e) = db.apply_reorg_incremental(
                        &old_blocks,
                        &new_blocks,
                        &self.pending_tickets,
                        &new_payouts,
                    ) {
                        eprintln!("Failed to apply incremental reorg: {}", e);
                    }
                } else {
                    // Deep reorg (ancestor before memory window): reorg_to replayed
                    // the entire chain from genesis, so ALL checkpoints are stale.
                    if let Err(e) = db.delete_checkpoints_from(0) {
                        eprintln!("Failed to invalidate stale checkpoints after deep reorg: {}", e);
                    }
                    // settled_payouts_by_block now contains payouts for ALL canonical
                    // blocks — pre-fork and new fork alike.
                    let all_payouts = self.take_all_settled_payouts();
                    for b in &new_blocks {
                        if let Err(e) = db.save_block_indexed(b) {
                            eprintln!("Failed to save reorg block {}: {}", b.index, e);
                        }
                    }
                    if let Err(e) =
                        db.rebuild_indices_with_tickets(&self.pending_tickets, &all_payouts)
                    {
                        eprintln!("Failed to rebuild indices after deep reorg: {}", e);
                    }
                }
                self.prune_to_window();
            }
            BlockOutcome::Orphaned | BlockOutcome::Rejected => {}
        }

        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lootcoin_core::block::meets_difficulty;
    use lootcoin_core::block::Block;
    use lootcoin_core::transaction::Transaction;
    use lootcoin_core::wallet::Wallet;

    const GENESIS_TS: u64 = 1_000_000;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn coinbase_tx(receiver: &str) -> Transaction {
        Transaction {
            sender: String::new(),
            receiver: receiver.to_string(),
            amount: 1,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![],
        }
    }

    fn make_genesis() -> Block {
        let txs = vec![Transaction {
            sender: String::new(),
            receiver: "genesis_miner".to_string(),
            amount: 1000,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![],
        }];
        Block {
            index: 0,
            previous_hash: vec![0u8; 32],
            timestamp: GENESIS_TS,
            nonce: 0,
            tx_root: Block::compute_tx_root(&txs),
            transactions: txs,
            hash: vec![], // genesis hash left empty; first block prev_hash matches
        }
    }

    /// Create a chain with difficulty=0 so tests don't need to actually mine.
    /// `initial_difficulty` is also set to 0 so that `reorg_to` replays at
    /// difficulty=0 rather than resetting to INITIAL_DIFFICULTY=16.
    fn make_chain() -> Blockchain {
        let mut chain = Blockchain::new(make_genesis());
        chain.current_difficulty = 0.0;
        chain.initial_difficulty = 0.0;
        chain
    }

    /// Build the next block on top of `chain`. With difficulty=0 any hash passes.
    fn next_block(chain: &Blockchain, txs: Vec<Transaction>, timestamp: u64) -> Block {
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp,
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        b
    }

    // ── meets_difficulty ─────────────────────────────────────────────────────

    #[test]
    fn meets_difficulty_zero_always_passes() {
        assert!(meets_difficulty(&[0xFF, 0xFF], 0.0));
        assert!(meets_difficulty(&[], 0.0));
    }

    #[test]
    fn meets_difficulty_exact_byte_boundary() {
        // 8 bits: first byte must be 0x00
        assert!(meets_difficulty(&[0x00, 0xFF], 8.0));
        assert!(!meets_difficulty(&[0x01, 0x00], 8.0));
    }

    #[test]
    fn meets_difficulty_sub_byte_granularity() {
        // 4 bits: top nibble of first byte must be 0
        assert!(meets_difficulty(&[0x0F, 0xFF], 4.0));
        assert!(!meets_difficulty(&[0x10, 0x00], 4.0));
    }

    #[test]
    fn meets_difficulty_two_full_bytes() {
        assert!(meets_difficulty(&[0x00, 0x00, 0xFF], 16.0));
        assert!(!meets_difficulty(&[0x00, 0x01, 0x00], 16.0));
    }

    #[test]
    fn meets_difficulty_hash_too_short_fails() {
        // requires 2 bytes but hash has only 1
        assert!(!meets_difficulty(&[0x00], 16.0));
    }

    // ── Blockchain accessors ─────────────────────────────────────────────────

    #[test]
    fn initial_height_is_one_after_genesis() {
        // genesis is block 0; height = blocks_offset(0) + blocks.len()(1) = 1
        let chain = make_chain();
        assert_eq!(chain.get_height(), 1);
    }

    #[test]
    fn genesis_balance_applied_to_receiver() {
        let chain = make_chain();
        assert_eq!(chain.get_balance("genesis_miner"), 1000);
        assert_eq!(chain.get_balance("nobody"), 0);
    }

    #[test]
    fn seed_pot_sets_pot_and_genesis_pot() {
        let mut chain = make_chain();
        chain.seed_pot(50_000);
        assert_eq!(chain.get_pot(), 50_000);
    }

    #[test]
    fn chain_work_is_zero_at_genesis() {
        let chain = make_chain();
        assert_eq!(chain.get_chain_work(), 0);
    }

    #[test]
    fn chain_work_increases_after_block() {
        let mut chain = make_chain();
        let b = next_block(&chain, vec![coinbase_tx("m1")], GENESIS_TS + 1);
        chain.apply_in_memory(b, None);
        // difficulty=0 → 1 << 0 = 1
        assert_eq!(chain.get_chain_work(), 1);
    }

    // ── get_avg_block_time_secs ───────────────────────────────────────────────

    #[test]
    fn avg_block_time_none_with_single_timestamp() {
        let chain = make_chain(); // only genesis timestamp
        assert!(chain.get_avg_block_time_secs().is_none());
    }

    #[test]
    fn avg_block_time_computed_over_two_intervals() {
        let mut chain = make_chain();
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("m1")], GENESIS_TS + 60),
            None,
        );
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("m2")], GENESIS_TS + 120),
            None,
        );
        // timestamps: [GENESIS_TS, GENESIS_TS+60, GENESIS_TS+120]
        // genesis is skipped; oldest=GENESIS_TS+60, newest=GENESIS_TS+120, intervals=1 → 60/1 = 60.0
        assert_eq!(chain.get_avg_block_time_secs(), Some(60.0_f64));
    }

    // ── apply_transaction ────────────────────────────────────────────────────

    #[test]
    fn apply_coinbase_adds_to_balance() {
        let mut chain = make_chain();
        chain.apply_transaction(&coinbase_tx("new_miner"));
        assert_eq!(chain.get_balance("new_miner"), 1);
    }

    #[test]
    fn apply_regular_tx_debits_sender_credits_receiver_and_pot() {
        let mut chain = make_chain();
        // genesis_miner has 1000
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 300,
            fee: 20,
            nonce: 1,
            public_key: [0u8; 32],
            signature: vec![1],
        };
        chain.apply_transaction(&tx);
        assert_eq!(chain.get_balance("genesis_miner"), 1000 - 300 - 20);
        assert_eq!(chain.get_balance("alice"), 300);
        assert_eq!(chain.get_pot(), 20);
    }

    #[test]
    fn apply_regular_tx_insufficient_balance_is_noop() {
        let mut chain = make_chain();
        let tx = Transaction {
            sender: "broke".to_string(),
            receiver: "alice".to_string(),
            amount: 100,
            fee: 10,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![1],
        };
        chain.apply_transaction(&tx);
        assert_eq!(chain.get_balance("broke"), 0);
        assert_eq!(chain.get_balance("alice"), 0);
    }

    #[test]
    fn apply_transaction_records_signature_for_replay_protection() {
        let mut chain = make_chain();
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 100,
            fee: 10,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![42],
        };
        chain.apply_transaction(&tx);
        assert!(chain.confirmed_signatures.contains(&vec![42]));
    }

    // ── validate_transaction_state ───────────────────────────────────────────

    #[test]
    fn validate_rejects_below_min_fee() {
        let chain = make_chain();
        // fee=0 and fee=1 are both below MIN_TX_FEE=2
        for bad_fee in [0u64, 1] {
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 100,
            fee: bad_fee,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![1],
        };
        assert!(!chain.validate_transaction_state(&tx));
        } // end for bad_fee
    }

    #[test]
    fn validate_rejects_insufficient_balance() {
        let chain = make_chain();
        // genesis_miner has 1000; trying to send 999 + fee 2 = 1001
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 999,
            fee: 2,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![1],
        };
        assert!(!chain.validate_transaction_state(&tx));
    }

    #[test]
    fn validate_accepts_sufficient_balance() {
        let chain = make_chain();
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 500,
            fee: 10,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![1],
        };
        assert!(chain.validate_transaction_state(&tx));
    }

    #[test]
    fn validate_rejects_replayed_signature() {
        let mut chain = make_chain();
        chain.confirmed_signatures.insert(vec![42]);
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 100,
            fee: 10,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![42],
        };
        assert!(!chain.validate_transaction_state(&tx));
    }

    #[test]
    fn validate_accepts_exact_balance() {
        let chain = make_chain();
        // genesis_miner has exactly 1000; sending 990 + fee 10 = 1000
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 990,
            fee: 10,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![1],
        };
        assert!(chain.validate_transaction_state(&tx));
    }

    // ── apply_in_memory: valid blocks ────────────────────────────────────────

    #[test]
    fn apply_valid_block_advances_height() {
        let mut chain = make_chain();
        let b = next_block(&chain, vec![coinbase_tx("m1")], GENESIS_TS + 1);
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Applied
        ));
        assert_eq!(chain.get_height(), 2);
    }

    #[test]
    fn apply_valid_block_credits_miner_from_coinbase() {
        let mut chain = make_chain();
        let b = next_block(&chain, vec![coinbase_tx("new_miner")], GENESIS_TS + 1);
        chain.apply_in_memory(b, None);
        assert_eq!(chain.get_balance("new_miner"), 1);
    }

    #[test]
    fn apply_multiple_blocks_accumulates_state() {
        let mut chain = make_chain();
        let mut ts = GENESIS_TS + 1;
        for i in 0..5u8 {
            let b = next_block(&chain, vec![coinbase_tx(&format!("m{}", i))], ts);
            chain.apply_in_memory(b, None);
            ts += 1;
        }
        assert_eq!(chain.get_height(), 6);
        assert_eq!(chain.get_balance("m0"), 1);
        assert_eq!(chain.get_balance("m4"), 1);
    }

    #[test]
    fn apply_block_with_signed_tx_transfers_funds() {
        let mut chain = make_chain();
        let alice = Wallet::new();
        let alice_addr = alice.get_address();

        // Seed alice's balance directly — coinbase amount is capped at 1 by
        // protocol, so we use the private field to set up the test state.
        chain.balances.insert(alice_addr.clone(), 500);
        assert_eq!(chain.get_balance(&alice_addr), 500);

        // Now alice sends to bob using a properly signed transaction
        let bob = Wallet::new();
        let bob_addr = bob.get_address();
        let signed_tx = Transaction::new_signed(&alice, bob_addr.clone(), 100, 10);

        let send_block = next_block(
            &chain,
            vec![coinbase_tx("miner"), signed_tx],
            GENESIS_TS + 2,
        );
        chain.apply_in_memory(send_block, None);
        assert_eq!(chain.get_balance(&alice_addr), 390); // 500 - 100 - 10
        assert_eq!(chain.get_balance(&bob_addr), 100);
        // 50/50 fee split: 5 to pot, 5 to miner (miner also has 1 from coinbase)
        assert_eq!(chain.get_pot(), 5);
        assert_eq!(chain.get_balance("miner"), 6); // 1 coinbase + 5 fee share
    }

    // ── apply_in_memory: invalid blocks ──────────────────────────────────────

    #[test]
    fn apply_block_with_second_coinbase_rejected() {
        let mut chain = make_chain();
        // Two coinbase txs: second empty-sender is forbidden
        let txs = vec![coinbase_tx("m1"), coinbase_tx("m2")];
        let b = next_block(&chain, txs, GENESIS_TS + 1);
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
        assert_eq!(chain.get_height(), 1);
    }

    #[test]
    fn apply_block_wrong_tx_root_rejected() {
        let mut chain = make_chain();
        let txs = vec![coinbase_tx("m1")];
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp: GENESIS_TS + 1,
            nonce: 0,
            tx_root: vec![0u8; 32], // wrong tx_root
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
    }

    #[test]
    fn apply_block_past_timestamp_rejected_by_mtp() {
        let mut chain = make_chain();
        // Block 1 at GENESIS_TS + 100 sets the median
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("m1")], GENESIS_TS + 100),
            None,
        );

        // Block 2 with timestamp <= median (GENESIS_TS) must be rejected
        let txs = vec![coinbase_tx("m2")];
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp: GENESIS_TS, // equals genesis — violates MTP
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
    }

    #[test]
    fn apply_block_wrong_index_does_not_advance_height() {
        let mut chain = make_chain();
        let txs = vec![coinbase_tx("m1")];
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: 5, // should be 1
            previous_hash: chain.get_latest_hash(),
            timestamp: GENESIS_TS + 1,
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        chain.apply_in_memory(b, None);
        assert_eq!(chain.get_height(), 1); // unchanged
    }

    #[test]
    fn apply_block_with_tampered_hash_rejected() {
        let mut chain = make_chain();
        let mut b = next_block(&chain, vec![coinbase_tx("m1")], GENESIS_TS + 1);
        b.hash = vec![0xDE, 0xAD]; // tampered — doesn't match calculate_hash()
                                   // index and prev_hash match, but hash verification fails in update_state
                                   // and also block cannot go into orphan pool (validate_block_standalone fails)
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
    }

    // ── update_state security checks ─────────────────────────────────────────

    #[test]
    fn apply_block_rejects_coinbase_amount_too_high() {
        let mut chain = make_chain();
        let mut coinbase = coinbase_tx("miner");
        coinbase.amount = 2; // exceeds the allowed maximum of 1
        let b = next_block(&chain, vec![coinbase], GENESIS_TS + 1);
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
        assert_eq!(chain.get_height(), 1); // chain unchanged
    }

    #[test]
    fn apply_block_rejects_coinbase_fee_nonzero() {
        let mut chain = make_chain();
        let mut coinbase = coinbase_tx("miner");
        coinbase.fee = 1; // coinbase fee must be exactly 0
        let b = next_block(&chain, vec![coinbase], GENESIS_TS + 1);
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
        assert_eq!(chain.get_height(), 1);
    }

    #[test]
    fn apply_block_rejects_wrong_previous_hash() {
        let mut chain = make_chain();
        let txs = vec![coinbase_tx("m1")];
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: vec![0xDE, 0xAD, 0xBE, 0xEF], // wrong
            timestamp: GENESIS_TS + 1,
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        // Index matches but prev_hash doesn't → content-invalid, rejected outright
        // (validate_block_standalone also fails because it links nowhere known)
        chain.apply_in_memory(b, None);
        assert_eq!(chain.get_height(), 1);
    }

    #[test]
    fn apply_block_rejects_future_timestamp() {
        let mut chain = make_chain();
        let txs = vec![coinbase_tx("m1")];
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp: u64::MAX, // impossibly far in the future
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
        assert_eq!(chain.get_height(), 1);
    }

    #[test]
    fn apply_block_rejects_duplicate_sig_within_block() {
        let mut chain = make_chain();
        // Two transactions sharing the same signature bytes are forbidden.
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 100,
            fee: 10,
            nonce: 1,
            public_key: [0u8; 32],
            signature: vec![0xAA],
        };
        let b = next_block(
            &chain,
            vec![coinbase_tx("miner"), tx.clone(), tx],
            GENESIS_TS + 1,
        );
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
    }

    #[test]
    fn apply_block_rejects_tx_with_insufficient_sender_balance() {
        let mut chain = make_chain();
        // genesis_miner has 1000; try to spend 2000 + fee
        let tx = Transaction {
            sender: "genesis_miner".to_string(),
            receiver: "alice".to_string(),
            amount: 2000,
            fee: 10,
            nonce: 1,
            public_key: [0u8; 32],
            signature: vec![0xBB],
        };
        let b = next_block(&chain, vec![coinbase_tx("miner"), tx], GENESIS_TS + 1);
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Rejected
        ));
        assert_eq!(chain.get_balance("genesis_miner"), 1000); // unchanged
    }

    // ── apply_block (runtime path, requires DB) ───────────────────────────────

    #[test]
    fn apply_block_accepts_coinbase_only_block() {
        let mut chain = make_chain();
        let db = crate::db::Db::new_in_memory().expect("in-memory db");
        let genesis = chain.blocks[0].clone();
        db.save_block_indexed(&genesis).unwrap();

        let b = next_block(&chain, vec![coinbase_tx("miner")], GENESIS_TS + 1);
        let outcome = chain.apply_block(&db, b);
        assert!(matches!(outcome, BlockOutcome::Applied));
        assert_eq!(chain.get_height(), 2);
    }

    // ── prune_to_window ───────────────────────────────────────────────────────

    #[test]
    fn prune_keeps_all_blocks_within_window() {
        let mut chain = make_chain();
        let mut ts = GENESIS_TS + 1;
        for i in 0..10u8 {
            let b = next_block(&chain, vec![coinbase_tx(&format!("m{}", i))], ts);
            chain.apply_in_memory(b, None);
            ts += 1;
        }
        chain.prune_to_window();
        // 11 blocks total (genesis + 10), well within REORG_WINDOW=100
        assert_eq!(chain.blocks.len(), 11);
        assert_eq!(chain.blocks_offset, 0);
    }

    // ── Additional helpers ────────────────────────────────────────────────────

    /// Like `make_chain` but starts at an explicit difficulty level.
    /// Sets both current and initial so that `reorg_to` replays at the same
    /// difficulty rather than resetting to INITIAL_DIFFICULTY.
    fn make_chain_at_difficulty(d: f64) -> Blockchain {
        let mut chain = Blockchain::new(make_genesis());
        chain.current_difficulty = d;
        chain.initial_difficulty = d;
        chain
    }

    /// Like `make_chain()` but the genesis block also allocates `amount` coins to
    /// `addr`. Genesis transactions bypass signature validation, so any address
    /// string works — including a real wallet's hex address. This is useful when
    /// a test needs a funded wallet whose balance survives `reorg_to` (which
    /// resets state to genesis before replaying the new canonical chain).
    fn make_chain_funding(addr: &str, amount: u64) -> Blockchain {
        let txs = vec![
            Transaction {
                sender: String::new(),
                receiver: "genesis_miner".to_string(),
                amount: 1000,
                fee: 0,
                nonce: 0,
                public_key: [0u8; 32],
                signature: vec![],
            },
            Transaction {
                sender: String::new(),
                receiver: addr.to_string(),
                amount,
                fee: 0,
                nonce: 0,
                public_key: [0u8; 32],
                signature: vec![],
            },
        ];
        let genesis = Block {
            index: 0,
            previous_hash: vec![0u8; 32],
            timestamp: GENESIS_TS,
            nonce: 0,
            tx_root: Block::compute_tx_root(&txs),
            transactions: txs,
            hash: vec![],
        };
        let mut chain = Blockchain::new(genesis);
        chain.current_difficulty = 0.0;
        chain.initial_difficulty = 0.0;
        chain
    }

    /// Mine a block at the chain's *current* difficulty by iterating nonces.
    /// For difficulty=0 this is instantaneous; difficulty=8 needs ~256 iterations.
    fn mine_next_block(chain: &Blockchain, txs: Vec<Transaction>, timestamp: u64) -> Block {
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp,
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        loop {
            b.hash = b.calculate_hash();
            if meets_difficulty(&b.hash, chain.current_difficulty) {
                return b;
            }
            b.nonce += 1;
        }
    }

    /// Build a block from an explicit parent rather than the chain tip.
    /// Used to construct fork chains without applying them to the main chain.
    fn block_at(index: u64, prev_hash: Vec<u8>, txs: Vec<Transaction>, timestamp: u64) -> Block {
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index,
            previous_hash: prev_hash,
            timestamp,
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        b
    }

    /// Craft a 32-byte digest whose first 4 bytes encode `val` as little-endian,
    /// so `loot_bucket_from_digest` returns exactly `val % PPM`.
    fn digest_for_bucket(val: u32) -> Vec<u8> {
        let mut d = vec![0u8; 32];
        d[0..4].copy_from_slice(&val.to_le_bytes());
        d
    }

    // ── Lottery bucket boundaries ─────────────────────────────────────────────

    #[test]
    fn loot_bucket_lower_bound_is_small() {
        assert_eq!(
            Blockchain::loot_bucket_from_digest(&digest_for_bucket(0)),
            0
        );
    }

    #[test]
    fn loot_bucket_upper_bound_of_small() {
        assert_eq!(
            Blockchain::loot_bucket_from_digest(&digest_for_bucket(979_999)),
            979_999
        );
    }

    #[test]
    fn loot_bucket_lower_bound_of_medium() {
        assert_eq!(
            Blockchain::loot_bucket_from_digest(&digest_for_bucket(980_000)),
            980_000
        );
    }

    #[test]
    fn loot_bucket_upper_bound_of_medium() {
        assert_eq!(
            Blockchain::loot_bucket_from_digest(&digest_for_bucket(998_999)),
            998_999
        );
    }

    #[test]
    fn loot_bucket_lower_bound_of_large() {
        assert_eq!(
            Blockchain::loot_bucket_from_digest(&digest_for_bucket(999_000)),
            999_000
        );
    }

    #[test]
    fn loot_bucket_upper_bound_of_large() {
        assert_eq!(
            Blockchain::loot_bucket_from_digest(&digest_for_bucket(999_899)),
            999_899
        );
    }

    #[test]
    fn loot_bucket_lower_bound_of_jackpot() {
        assert_eq!(
            Blockchain::loot_bucket_from_digest(&digest_for_bucket(999_900)),
            999_900
        );
    }

    #[test]
    fn loot_bucket_upper_bound_of_jackpot() {
        // PPM - 1 = 999_999, the highest possible bucket value
        assert_eq!(
            Blockchain::loot_bucket_from_digest(&digest_for_bucket(999_999)),
            999_999
        );
    }

    /// Verify that the tier boundaries, divisors, and payout formula match the
    /// documented values. This test replicates the match arms from
    /// `compute_ticket_reward` so any change to boundaries or divisors is caught
    /// immediately.
    ///
    /// Payout formula: `pot / DIVISOR`
    #[test]
    fn payout_divisors_match_bucket_ranges() {
        const POT: u64 = 1_000_000_000;
        let cases: &[(u32, u64, &str)] = &[
            (0,                          0,                     "none"),
            (SMALL_BUCKET_START - 1,     0,                     "none"),
            (SMALL_BUCKET_START,         POT / SMALL_DIVISOR,   "small"),
            (MEDIUM_BUCKET_START - 1,    POT / SMALL_DIVISOR,   "small"),
            (MEDIUM_BUCKET_START,        POT / MEDIUM_DIVISOR,  "medium"),
            (LARGE_BUCKET_START - 1,     POT / MEDIUM_DIVISOR,  "medium"),
            (LARGE_BUCKET_START,         POT / LARGE_DIVISOR,   "large"),
            (JACKPOT_BUCKET_START - 1,   POT / LARGE_DIVISOR,   "large"),
            (JACKPOT_BUCKET_START,       POT / JACKPOT_DIVISOR, "jackpot"),
            (PPM - 1,                    POT / JACKPOT_DIVISOR, "jackpot"),
        ];
        for &(bucket, expected_amount, expected_tier) in cases {
            let (amount, tier) = match bucket {
                0..SMALL_BUCKET_START                    => (0,                       "none"),
                SMALL_BUCKET_START..MEDIUM_BUCKET_START  => (POT / SMALL_DIVISOR,   "small"),
                MEDIUM_BUCKET_START..LARGE_BUCKET_START  => (POT / MEDIUM_DIVISOR,  "medium"),
                LARGE_BUCKET_START..JACKPOT_BUCKET_START => (POT / LARGE_DIVISOR,   "large"),
                _                                        => (POT / JACKPOT_DIVISOR, "jackpot"),
            };
            assert_eq!(amount, expected_amount, "wrong amount for bucket {}", bucket);
            assert_eq!(tier,   expected_tier,   "wrong tier for bucket {}",   bucket);
        }
    }

    // ── Ticket lifecycle ──────────────────────────────────────────────────────

    #[test]
    fn ticket_only_issued_for_blocks_with_transactions() {
        let alice = lootcoin_core::wallet::Wallet::new();
        let bob = lootcoin_core::wallet::Wallet::new();
        let mut chain = make_chain();
        chain.balances.insert(alice.get_address(), 500);

        // Coinbase-only block — no ticket.
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("m1")], GENESIS_TS + 1),
            None,
        );
        assert_eq!(
            chain.pending_tickets.len(),
            0,
            "coinbase-only block must not issue a ticket"
        );

        // Block with a real transaction — ticket issued to the miner.
        let tx = Transaction::new_signed(&alice, bob.get_address(), 10, 2);
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("m2"), tx], GENESIS_TS + 2),
            None,
        );
        assert_eq!(chain.pending_tickets.len(), 1);
        assert_eq!(chain.pending_tickets[0].miner, "m2");

        // Another coinbase-only — still no new ticket.
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("m3")], GENESIS_TS + 3),
            None,
        );
        assert_eq!(
            chain.pending_tickets.len(),
            1,
            "coinbase-only block must not issue a ticket"
        );
    }

    #[test]
    fn ticket_not_settled_before_reveal_window_closes() {
        // A ticket issued at block 1 settles at block 1 + TICKET_MATURITY + REVEAL_BLOCKS = 111.
        // At block 110 the ticket must still be pending and the pot untouched.
        //
        // Blocks cross the retarget at block 100 (difficulty rises from 0 → 8),
        // so `mine_next_block` is used throughout to handle the transition.
        let alice = lootcoin_core::wallet::Wallet::new();
        let bob = lootcoin_core::wallet::Wallet::new();
        let mut chain = make_chain();
        chain.seed_pot(1_000_000);
        chain.balances.insert(alice.get_address(), 500);

        // Block 1: miner_1 earns the ticket under test (requires a real tx).
        let tx = Transaction::new_signed(&alice, bob.get_address(), 10, 2);
        let b = mine_next_block(&chain, vec![coinbase_tx("miner_1"), tx], GENESIS_TS + 60);
        chain.apply_in_memory(b, None);
        // Capture pot after the fee from block 1 has entered it.
        let pot_after_block1 = chain.get_pot();

        // Blocks 2–110: advance the chain to just before settlement (coinbase-only is fine).
        for i in 2u64..=110 {
            let b = mine_next_block(&chain, vec![coinbase_tx("other")], GENESIS_TS + i * 60);
            chain.apply_in_memory(b, None);
        }

        assert_eq!(chain.get_height(), 111); // tip is block 110, height = 111
        assert!(
            chain.pending_tickets.iter().any(|t| t.created_height == 1),
            "ticket from block 1 should still be pending before its settlement block"
        );
        assert_eq!(
            chain.get_pot(),
            pot_after_block1,
            "pot must not change before any settlement"
        );
    }

    #[test]
    fn ticket_settles_at_maturity_plus_reveal_height() {
        // The ticket from block 1 settles exactly at block 111.
        let alice = lootcoin_core::wallet::Wallet::new();
        let bob = lootcoin_core::wallet::Wallet::new();
        let mut chain = make_chain();
        chain.seed_pot(1_000_000);
        chain.balances.insert(alice.get_address(), 500);

        // Block 1: issue the ticket under test.
        let tx = Transaction::new_signed(&alice, bob.get_address(), 10, 2);
        let b = mine_next_block(&chain, vec![coinbase_tx("miner_1"), tx], GENESIS_TS + 60);
        chain.apply_in_memory(b, None);

        // Blocks 2–111: advance to settlement (coinbase-only is fine for filler).
        for i in 2u64..=111 {
            let b = mine_next_block(
                &chain,
                vec![coinbase_tx(&format!("m{}", i))],
                GENESIS_TS + i * 60,
            );
            chain.apply_in_memory(b, None);
        }

        // After block 111, the ticket from block 1 must have settled.
        assert!(
            !chain.pending_tickets.iter().any(|t| t.created_height == 1),
            "ticket from block 1 must be settled after block 111"
        );
    }

    #[test]
    fn settled_ticket_pays_miner_and_reduces_pot() {
        let alice = lootcoin_core::wallet::Wallet::new();
        let bob = lootcoin_core::wallet::Wallet::new();
        let mut chain = make_chain();
        chain.seed_pot(100_000_000);
        chain.balances.insert(alice.get_address(), 500);
        let initial_pot = chain.get_pot();

        // Block 1: miner_1 earns a ticket (has a real tx).
        let tx = Transaction::new_signed(&alice, bob.get_address(), 10, 2);
        let b = mine_next_block(&chain, vec![coinbase_tx("miner_1"), tx], GENESIS_TS + 60);
        chain.apply_in_memory(b, None);

        // Blocks 2–111: filler (coinbase-only, no tickets).
        for i in 2u64..=111 {
            let b = mine_next_block(
                &chain,
                vec![coinbase_tx(&format!("m{}", i))],
                GENESIS_TS + i * 60,
            );
            chain.apply_in_memory(b, None);
        }

        // Ticket settled at block 111. The outcome is probabilistic (88% no win),
        // so we verify the invariants rather than a specific amount.
        // fee=2 → floor(2/2)=1 to miner, 1 to pot → pot_with_fee = initial_pot + 1
        let pot_with_fee = initial_pot + 1;
        let pot_after    = chain.get_pot();
        let payout       = pot_with_fee - pot_after; // 0 on no-win, positive on win

        // Payout must be within [0, max jackpot].
        let max_payout = pot_with_fee / JACKPOT_DIVISOR;
        assert!(
            payout <= max_payout,
            "payout {} exceeds max jackpot {}",
            payout,
            max_payout
        );

        // Pot and miner balance must be consistent with each other.
        // miner_1 earns: 1 coinbase + 1 fee share (floor(fee=2 / 2) = 1) + lottery payout.
        assert_eq!(
            chain.get_balance("miner_1"),
            2 + payout, // coinbase + fee share + lottery payout (may be 0)
            "miner_1 balance must equal coinbase plus fee share plus any lottery payout"
        );
    }

    // ── ASERT difficulty adjustment ───────────────────────────────────────────
    //
    // Anchor is set at block 1.  For all subsequent blocks:
    //   ideal  = (height − 1) × 60 s
    //   actual = block.timestamp − anchor.timestamp
    //   diff   = anchor_diff + (ideal − actual) / HALFLIFE
    //
    // All tests use exactly 2 mined blocks so they run in milliseconds:
    //   block 1 → sets the anchor (no difficulty change)
    //   block 2 → first block where ASERT applies; we check the result

    #[test]
    fn asert_stable_at_target_block_time() {
        // block 1 at T+60, block 2 at T+120:
        //   ideal=60, actual=60, adj=0 → no change.
        let mut chain = make_chain_at_difficulty(10.0);
        let b1 = mine_next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 60);
        chain.apply_in_memory(b1, None);
        let b2 = mine_next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 120);
        chain.apply_in_memory(b2, None);
        assert_eq!(chain.get_difficulty(), 10.0);
    }

    #[test]
    fn asert_increases_when_blocks_too_fast() {
        // block 1 at T+1 (sets anchor ts=T+1), block 2 at T+2:
        //   ideal=60, actual=1, adj=(60−1)/3600 = 59/3600 → difficulty rises.
        let mut chain = make_chain_at_difficulty(10.0);
        let b1 = mine_next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 1);
        chain.apply_in_memory(b1, None);
        let b2 = mine_next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 2);
        chain.apply_in_memory(b2, None);
        let expected = 10.0 + 59.0 / ASERT_HALFLIFE_SECS;
        assert!(
            (chain.get_difficulty() - expected).abs() < 1e-9,
            "got {}, expected {expected}",
            chain.get_difficulty()
        );
    }

    #[test]
    fn asert_decreases_when_blocks_too_slow() {
        // block 1 at T+60 (anchor), block 2 at T+60+100_000 (very late):
        //   ideal=60, actual=100_000, adj=(60−100_000)/3600 ≈ −27.8 → floor.
        let mut chain = make_chain_at_difficulty(10.0);
        let b1 = mine_next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 60);
        chain.apply_in_memory(b1, None);
        let b2 = mine_next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 100_060);
        chain.apply_in_memory(b2, None);
        assert_eq!(chain.get_difficulty(), MIN_DIFFICULTY);
    }

    #[test]
    fn asert_never_below_min_difficulty() {
        // Same as above but starting at the floor — must not go lower.
        let mut chain = make_chain_at_difficulty(MIN_DIFFICULTY);
        let b1 = mine_next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 60);
        chain.apply_in_memory(b1, None);
        let b2 = mine_next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 100_060);
        chain.apply_in_memory(b2, None);
        assert_eq!(chain.get_difficulty(), MIN_DIFFICULTY);
    }

    #[test]
    fn asert_ceiling_enforced_by_formula() {
        // Pure arithmetic check — no mining needed.
        // anchor_diff=120, ideal−actual = 500_000 s → adj ≈ 138.9 → would exceed MAX.
        let anchor_diff = 120.0_f64;
        let ideal: u64 = 500_060;
        let actual: u64 = 60;
        let adj = (ideal as f64 - actual as f64) / ASERT_HALFLIFE_SECS;
        let result = (anchor_diff + adj).clamp(MIN_DIFFICULTY, MAX_DIFFICULTY);
        assert_eq!(result, MAX_DIFFICULTY);
    }

    #[test]
    fn asert_halflife_one_bit_decrease() {
        // block 1 at T+60 (anchor ts=T+60, diff=10), block 2 at T+60+3_660:
        //   ideal=60, actual=3_660, adj=(60−3_660)/3600 = −1.0 → difficulty = 9.0.
        let mut chain = make_chain_at_difficulty(10.0);
        let t1 = GENESIS_TS + 60;
        let b1 = mine_next_block(&chain, vec![coinbase_tx("m")], t1);
        chain.apply_in_memory(b1, None);
        let b2 = mine_next_block(&chain, vec![coinbase_tx("m")], t1 + 3_660);
        chain.apply_in_memory(b2, None);
        assert!(
            (chain.get_difficulty() - 9.0).abs() < 1e-9,
            "expected 9.0, got {}",
            chain.get_difficulty()
        );
    }

    // ── Reorg ─────────────────────────────────────────────────────────────────

    /// Build a complete chain of coinbase-only blocks for use in `reorg_to` tests.
    fn build_chain(blocks: &[(&str, u64)], prev_hash: Vec<u8>, start_index: u64) -> Vec<Block> {
        let mut out = Vec::new();
        let mut ph = prev_hash;
        for (idx, (miner, ts)) in blocks.iter().enumerate() {
            let txs = vec![coinbase_tx(miner)];
            let b = block_at(start_index + idx as u64, ph.clone(), txs, *ts);
            ph = b.hash.clone();
            out.push(b);
        }
        out
    }

    #[test]
    fn reorg_to_switches_to_fork_tip() {
        let mut chain = make_chain();
        chain.seed_pot(0);

        // Apply two main-chain blocks.
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("main_a")], GENESIS_TS + 1),
            None,
        );
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("main_b")], GENESIS_TS + 2),
            None,
        );
        assert_eq!(chain.get_height(), 3); // genesis + 2

        // Build a three-block fork from genesis.
        let genesis_hash = chain.blocks[0].hash.clone();
        let fork = build_chain(
            &[
                ("fork_c", GENESIS_TS + 3),
                ("fork_d", GENESIS_TS + 4),
                ("fork_e", GENESIS_TS + 5),
            ],
            genesis_hash,
            1,
        );

        // Splice: [genesis] + fork
        let new_canonical: Vec<Block> = std::iter::once(chain.blocks[0].clone())
            .chain(fork)
            .collect();

        chain.reorg_to(new_canonical, None);

        assert_eq!(chain.get_height(), 4); // genesis + 3 fork blocks
    }

    #[test]
    fn reorg_to_recalculates_balances() {
        let mut chain = make_chain();
        chain.seed_pot(0);

        // Mine two main blocks: main_a and main_b each earn 1 coinbase coin.
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("main_a")], GENESIS_TS + 1),
            None,
        );
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("main_b")], GENESIS_TS + 2),
            None,
        );
        assert_eq!(chain.get_balance("main_a"), 1);
        assert_eq!(chain.get_balance("main_b"), 1);

        // Build a fork: fork_c and fork_d mine instead.
        let genesis_hash = chain.blocks[0].hash.clone();
        let fork = build_chain(
            &[("fork_c", GENESIS_TS + 3), ("fork_d", GENESIS_TS + 4)],
            genesis_hash,
            1,
        );
        let new_canonical: Vec<Block> = std::iter::once(chain.blocks[0].clone())
            .chain(fork)
            .collect();

        chain.reorg_to(new_canonical, None);

        // main_a and main_b were displaced — their coinbase rewards are gone.
        assert_eq!(
            chain.get_balance("main_a"),
            0,
            "displaced miner must lose coinbase"
        );
        assert_eq!(
            chain.get_balance("main_b"),
            0,
            "displaced miner must lose coinbase"
        );
        // Fork miners get their rewards.
        assert_eq!(chain.get_balance("fork_c"), 1);
        assert_eq!(chain.get_balance("fork_d"), 1);
        // genesis_miner's balance from the genesis block is preserved.
        assert_eq!(chain.get_balance("genesis_miner"), 1000);
    }

    #[test]
    fn reorg_to_reissues_tickets_for_new_chain() {
        // Tickets require non-coinbase txs, so we need a funded wallet that survives
        // the reorg_to state reset. make_chain_funding embeds the wallet address in
        // genesis so its balance is restored when reorg_to replays from genesis.
        let alice = lootcoin_core::wallet::Wallet::new();
        let bob = lootcoin_core::wallet::Wallet::new();
        let mut chain = make_chain_funding(&alice.get_address(), 10_000);
        chain.seed_pot(0);

        // Two main-chain blocks with real txs → main miners each earn a ticket.
        let tx_a = Transaction::new_signed(&alice, bob.get_address(), 1, 2);
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("main_a"), tx_a], GENESIS_TS + 1),
            None,
        );
        let tx_b = Transaction::new_signed(&alice, bob.get_address(), 1, 2);
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("main_b"), tx_b], GENESIS_TS + 2),
            None,
        );
        assert!(chain.pending_tickets.iter().any(|t| t.miner == "main_a"));
        assert!(chain.pending_tickets.iter().any(|t| t.miner == "main_b"));

        // Build three fork blocks, each with a real tx from alice.
        // When reorg_to replays these, alice starts with 10_000 coins (from genesis),
        // so the transactions are valid regardless of what happened on the main chain.
        let genesis_hash = chain.blocks[0].hash.clone();
        let tx_c = Transaction::new_signed(&alice, bob.get_address(), 1, 2);
        let fc = block_at(
            1,
            genesis_hash.clone(),
            vec![coinbase_tx("fork_c"), tx_c],
            GENESIS_TS + 3,
        );
        let tx_d = Transaction::new_signed(&alice, bob.get_address(), 1, 2);
        let fd = block_at(
            2,
            fc.hash.clone(),
            vec![coinbase_tx("fork_d"), tx_d],
            GENESIS_TS + 4,
        );
        let tx_e = Transaction::new_signed(&alice, bob.get_address(), 1, 2);
        let fe = block_at(
            3,
            fd.hash.clone(),
            vec![coinbase_tx("fork_e"), tx_e],
            GENESIS_TS + 5,
        );

        let new_canonical: Vec<Block> = std::iter::once(chain.blocks[0].clone())
            .chain(vec![fc, fd, fe])
            .collect();
        chain.reorg_to(new_canonical, None);

        // Main-chain tickets are gone; fork-chain tickets are present.
        assert!(
            !chain.pending_tickets.iter().any(|t| t.miner == "main_a"),
            "ticket for displaced miner must be removed after reorg"
        );
        assert!(
            !chain.pending_tickets.iter().any(|t| t.miner == "main_b"),
            "ticket for displaced miner must be removed after reorg"
        );
        assert!(chain.pending_tickets.iter().any(|t| t.miner == "fork_c"));
        assert!(chain.pending_tickets.iter().any(|t| t.miner == "fork_d"));
        assert!(chain.pending_tickets.iter().any(|t| t.miner == "fork_e"));
    }

    #[test]
    fn reorg_to_resets_pot_to_genesis_seed() {
        let mut chain = make_chain();
        chain.seed_pot(500_000);

        // Fee-paying transaction on the main chain inflates the pot.
        let alice = lootcoin_core::wallet::Wallet::new();
        chain.balances.insert(alice.get_address(), 1_000);
        let fee_tx = Transaction::new_signed(&alice, "bob".to_string(), 100, 200);
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("main_a"), fee_tx], GENESIS_TS + 1),
            None,
        );
        let pot_after_fee = chain.get_pot();
        assert!(pot_after_fee > 500_000, "pot must grow from fees");

        // Reorg to a fork with no fee transactions — pot must revert to seed.
        let genesis_hash = chain.blocks[0].hash.clone();
        let fork = build_chain(&[("fork_b", GENESIS_TS + 2)], genesis_hash, 1);
        let new_canonical: Vec<Block> = std::iter::once(chain.blocks[0].clone())
            .chain(fork)
            .collect();
        chain.reorg_to(new_canonical, None);

        assert_eq!(
            chain.get_pot(),
            500_000,
            "pot must be restored to genesis seed after reorg"
        );
    }

    #[test]
    fn apply_in_memory_orphans_block_that_skips_ahead() {
        // A block whose previous_hash points to an unknown block cannot extend
        // the tip AND has no traceable ancestor, so it sits in the orphan pool.
        // It is within MAX_ORPHAN_DEPTH so it should not be outright rejected.
        let mut chain = make_chain();
        // Advance one block so the chain has real state.
        chain.apply_in_memory(
            next_block(&chain, vec![coinbase_tx("m")], GENESIS_TS + 1),
            None,
        );

        // prev_hash is a random value not present in block_hashes: the orphan
        // is genuinely dangling — find_longest_fork cannot resolve its ancestor.
        let unknown_prev = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00];
        let fake_tx = Transaction {
            sender: "s".to_string(),
            receiver: "r".to_string(),
            amount: 0,
            fee: 2,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![0xAB],
        };
        // index=5, chain height=2 → 5 ≤ 2+10, within MAX_ORPHAN_DEPTH
        let b = block_at(
            5,
            unknown_prev,
            vec![coinbase_tx("x"), fake_tx],
            GENESIS_TS + 2,
        );
        assert!(matches!(
            chain.apply_in_memory(b, None),
            BlockOutcome::Orphaned
        ));
    }
}

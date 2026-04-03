use std::collections::HashMap;
use std::fs;

type LotteryPayoutsMap = HashMap<u64, Vec<(String, u64, String)>>;

use lootcoin_core::block::Block;
use redb::{Database as RedbDatabase, ReadableTable, TableDefinition};
use serde::Serialize;

use crate::loot_ticket::LootTicket;

// key:   address_bytes + 0x00 + block_index (8 bytes BE) + tx_pos (4 bytes BE)
// value: bincode of (sender, receiver, amount, fee)
const TX_INDEX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx_index");
const PEERS: TableDefinition<&str, &[u8]> = TableDefinition::new("peers");
/// key: created_height (u64), value: bincode-encoded miner: String
const TICKETS: TableDefinition<u64, &[u8]> = TableDefinition::new("tickets");
/// key: block index (u64), value: bincode-encoded Block
const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
/// key: tx signature bytes, value: block index — permanent replay-protection log
const CONFIRMED_SIGS: TableDefinition<&[u8], u64> = TableDefinition::new("confirmed_sigs");
/// key: block index (u64), value: bincode-encoded Vec<(receiver, amount)>
/// Stores lottery payouts per block so they can be surfaced in the TX_INDEX as
/// synthetic "sender=lottery" entries visible in the explorer and wallet history.
const LOTTERY_PAYOUTS: TableDefinition<u64, &[u8]> = TableDefinition::new("lottery_payouts");
/// key: tx signature bytes, value: bincode-encoded (Transaction, added_height: u64)
/// Survives node restarts; entries are removed when the tx is confirmed or evicted.
const MEMPOOL: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mempool");
/// key: block height (u64), value: bincode-encoded CheckpointState.
/// One entry every CHECKPOINT_INTERVAL blocks; stale entries are pruned on reorg.
const CHECKPOINTS: TableDefinition<u64, &[u8]> = TableDefinition::new("checkpoints");

#[derive(Serialize)]
pub struct TxRecord {
    pub block_index: u64,
    pub sender: String,
    pub receiver: String,
    pub amount: u64,
    pub fee: u64,
}

#[derive(Serialize)]
pub struct PayoutRecord {
    pub block_index: u64,
    pub block_timestamp: Option<u64>,
    pub receiver: String,
    pub amount: u64,
    pub tier: String,
}

pub struct Db {
    db: RedbDatabase,
}

fn make_tx_key(address: &str, block_index: u64, tx_pos: usize) -> Vec<u8> {
    let mut key = address.as_bytes().to_vec();
    key.push(0x00);
    key.extend_from_slice(&block_index.to_be_bytes());
    key.extend_from_slice(&(tx_pos as u32).to_be_bytes());
    key
}

impl Db {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        fs::create_dir_all("./data")?;
        let db = RedbDatabase::create("./data/node.redb")?;
        {
            let wtxn = db.begin_write()?;
            wtxn.open_table(TX_INDEX)?;
            wtxn.open_table(PEERS)?;
            wtxn.open_table(TICKETS)?;
            wtxn.open_table(BLOCKS)?;
            wtxn.open_table(CONFIRMED_SIGS)?;
            wtxn.open_table(LOTTERY_PAYOUTS)?;
            wtxn.open_table(MEMPOOL)?;
            wtxn.open_table(CHECKPOINTS)?;
            wtxn.commit()?;
        }
        Ok(Self { db })
    }

    /// In-memory database for unit tests. Identical schema to the on-disk DB
    /// but data lives only in RAM and is discarded when dropped.
    #[cfg(test)]
    pub fn new_in_memory() -> Result<Self, Box<dyn std::error::Error>> {
        use redb::backends::InMemoryBackend;
        let db = RedbDatabase::builder().create_with_backend(InMemoryBackend::new())?;
        {
            let wtxn = db.begin_write()?;
            wtxn.open_table(TX_INDEX)?;
            wtxn.open_table(PEERS)?;
            wtxn.open_table(TICKETS)?;
            wtxn.open_table(BLOCKS)?;
            wtxn.open_table(CONFIRMED_SIGS)?;
            wtxn.open_table(LOTTERY_PAYOUTS)?;
            wtxn.open_table(MEMPOOL)?;
            wtxn.open_table(CHECKPOINTS)?;
            wtxn.commit()?;
        }
        Ok(Self { db })
    }

    // -------------------------------------------------------------------------
    // Block storage
    // -------------------------------------------------------------------------

    /// Persist a block to the BLOCKS table, keyed by its index.
    /// Overwrites any existing entry at that index (used after reorgs).
    pub fn save_block_indexed(&self, block: &Block) -> Result<(), Box<dyn std::error::Error>> {
        let data = bincode::serialize(block)?;
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(BLOCKS)?;
            table.insert(block.index, data.as_slice())?;
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Return up to `limit` blocks starting at absolute index `from`.
    pub fn get_blocks_range(
        &self,
        from: u64,
        limit: usize,
    ) -> Result<Vec<Block>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(BLOCKS)?;
        let end = from.saturating_add(limit as u64);
        let mut blocks = Vec::with_capacity(limit.min(1024));
        for entry in table.range(from..end)? {
            let (_, v) = entry?;
            blocks.push(bincode::deserialize(v.value())?);
        }
        Ok(blocks)
    }

    /// Load every block in the BLOCKS table in index order.
    /// Used at startup to replay the canonical chain.
    pub fn load_canonical_chain(&self) -> Result<Vec<Block>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(BLOCKS)?;
        let mut blocks = Vec::new();
        for entry in table.range::<u64>(..)? {
            let (_, v) = entry?;
            blocks.push(bincode::deserialize(v.value())?);
        }
        Ok(blocks)
    }

    /// Persist a newly applied block atomically: BLOCKS + TX_INDEX +
    /// CONFIRMED_SIGS + TICKETS all in a single write transaction.
    /// If the process crashes mid-write, redb rolls back the whole transaction,
    /// leaving the DB in the state before this block was applied.
    pub fn save_applied_block(
        &self,
        block: &Block,
        tickets: &[LootTicket],
        payouts: &[(String, u64, String)],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let wtxn = self.db.begin_write()?;
        {
            let mut blocks_table = wtxn.open_table(BLOCKS)?;
            let mut tx_table = wtxn.open_table(TX_INDEX)?;
            let mut sigs_table = wtxn.open_table(CONFIRMED_SIGS)?;
            let mut tickets_table = wtxn.open_table(TICKETS)?;
            let mut lp_table = wtxn.open_table(LOTTERY_PAYOUTS)?;

            // 1. Block
            let block_data = bincode::serialize(block)?;
            blocks_table.insert(block.index, block_data.as_slice())?;

            // 2. TX index (sender + receiver keys)
            for (tx_pos, tx) in block.transactions.iter().enumerate() {
                let value = bincode::serialize(&(&tx.sender, &tx.receiver, &tx.amount, &tx.fee))?;
                let receiver_key = make_tx_key(&tx.receiver, block.index, tx_pos);
                tx_table.insert(receiver_key.as_slice(), value.as_slice())?;
                if !tx.sender.is_empty() {
                    let sender_key = make_tx_key(&tx.sender, block.index, tx_pos);
                    tx_table.insert(sender_key.as_slice(), value.as_slice())?;
                }
            }

            // 3. Confirmed signatures (for replay protection)
            for tx in block.transactions.iter().filter(|tx| !tx.sender.is_empty()) {
                sigs_table.insert(tx.signature.as_slice(), block.index)?;
            }

            // 4. Pending lottery tickets (full replace)
            let ticket_keys: Vec<u64> = tickets_table
                .range::<u64>(..)?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in ticket_keys {
                tickets_table.remove(k)?;
            }
            for t in tickets {
                let ticket_data = bincode::serialize(&t.miner)?;
                tickets_table.insert(t.created_height, ticket_data.as_slice())?;
            }

            // 5. Lottery payouts: persist and add receiver TX_INDEX entries
            if !payouts.is_empty() {
                let payout_data = bincode::serialize(payouts)?;
                lp_table.insert(block.index, payout_data.as_slice())?;
                for (i, (receiver, amount, _tier)) in payouts.iter().enumerate() {
                    let key = make_tx_key(receiver, block.index, 0xFFFF_0000 + i);
                    let value = bincode::serialize(&(
                        "lottery".to_string(),
                        receiver.clone(),
                        *amount,
                        0u64,
                    ))?;
                    tx_table.insert(key.as_slice(), value.as_slice())?;
                }
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Transaction index
    // -------------------------------------------------------------------------

    pub fn get_transactions_for_address(
        &self,
        address: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TxRecord>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(TX_INDEX)?;

        let prefix_len = address.len() + 1;
        let mut start_key = address.as_bytes().to_vec();
        start_key.push(0x00);
        let mut end_key = address.as_bytes().to_vec();
        end_key.push(0x01);

        let mut records = Vec::with_capacity(limit.min(512));
        for entry in table
            .range(start_key.as_slice()..end_key.as_slice())?
            .rev()
            .skip(offset)
            .take(limit)
        {
            let (key, value) = entry?;
            let key_bytes = key.value();
            let block_index = u64::from_be_bytes(key_bytes[prefix_len..prefix_len + 8].try_into()?);
            let (sender, receiver, amount, fee): (String, String, u64, u64) =
                bincode::deserialize(value.value())?;
            records.push(TxRecord {
                block_index,
                sender,
                receiver,
                amount,
                fee,
            });
        }
        Ok(records)
    }

    /// Clear TX_INDEX and LOTTERY_PAYOUTS, then rebuild from the given canonical
    /// chain and the provided lottery payouts map. Used at startup after full
    /// chain replay (before the sliding-window prune runs).
    pub fn rebuild_tx_index(
        &self,
        blocks: &[Block],
        payouts_by_block: &HashMap<u64, Vec<(String, u64, String)>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(TX_INDEX)?;
            let mut lp_table = wtxn.open_table(LOTTERY_PAYOUTS)?;

            // Clear TX_INDEX
            let keys: Vec<Vec<u8>> = table
                .range::<&[u8]>(..)?
                .map(|e| e.map(|(k, _)| k.value().to_vec()))
                .collect::<Result<_, _>>()?;
            for key in keys {
                table.remove(key.as_slice())?;
            }

            // Clear LOTTERY_PAYOUTS
            let lp_keys: Vec<u64> = lp_table
                .range::<u64>(..)?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in lp_keys {
                lp_table.remove(k)?;
            }

            // Rebuild TX_INDEX from blocks
            for block in blocks {
                for (tx_pos, tx) in block.transactions.iter().enumerate() {
                    let value =
                        bincode::serialize(&(&tx.sender, &tx.receiver, &tx.amount, &tx.fee))?;
                    let receiver_key = make_tx_key(&tx.receiver, block.index, tx_pos);
                    table.insert(receiver_key.as_slice(), value.as_slice())?;
                    if !tx.sender.is_empty() {
                        let sender_key = make_tx_key(&tx.sender, block.index, tx_pos);
                        table.insert(sender_key.as_slice(), value.as_slice())?;
                    }
                }

                // Add lottery payout entries for this block
                if let Some(payouts) = payouts_by_block.get(&block.index) {
                    if !payouts.is_empty() {
                        let data = bincode::serialize(payouts)?;
                        lp_table.insert(block.index, data.as_slice())?;
                        for (i, (receiver, amount, _tier)) in payouts.iter().enumerate() {
                            let key = make_tx_key(receiver, block.index, 0xFFFF_0000 + i);
                            let value = bincode::serialize(&(
                                "lottery".to_string(),
                                receiver.clone(),
                                *amount,
                                0u64,
                            ))?;
                            table.insert(key.as_slice(), value.as_slice())?;
                        }
                    }
                }
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Incremental reorg: in a single write transaction, remove old fork blocks
    /// from all indices, insert new fork blocks, and update lottery tickets.
    /// O(reorg_depth × txs_per_block) instead of O(chain_length).
    pub fn apply_reorg_incremental(
        &self,
        old_blocks: &[Block],
        new_blocks: &[Block],
        tickets: &[LootTicket],
        new_payouts_by_block: &HashMap<u64, Vec<(String, u64, String)>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let wtxn = self.db.begin_write()?;
        {
            let mut blocks_table = wtxn.open_table(BLOCKS)?;
            let mut tx_table = wtxn.open_table(TX_INDEX)?;
            let mut sigs_table = wtxn.open_table(CONFIRMED_SIGS)?;
            let mut tickets_table = wtxn.open_table(TICKETS)?;
            let mut lp_table = wtxn.open_table(LOTTERY_PAYOUTS)?;

            // Remove displaced blocks from all indices.
            for block in old_blocks {
                // Remove the block itself. This is critical when the new fork is
                // shorter than the old one: without this, stale BLOCKS entries at
                // higher indices would be reloaded on restart and corrupt the replay.
                blocks_table.remove(block.index)?;

                // Remove lottery TX_INDEX entries for this displaced block.
                if let Some(stored) = lp_table.get(&block.index)? {
                    let old_payouts: Vec<(String, u64)> = bincode::deserialize(stored.value())?;
                    for (i, (receiver, _)) in old_payouts.iter().enumerate() {
                        let key = make_tx_key(receiver, block.index, 0xFFFF_0000 + i);
                        tx_table.remove(key.as_slice())?;
                    }
                }
                lp_table.remove(block.index)?;

                for (tx_pos, tx) in block.transactions.iter().enumerate() {
                    let receiver_key = make_tx_key(&tx.receiver, block.index, tx_pos);
                    tx_table.remove(receiver_key.as_slice())?;
                    if !tx.sender.is_empty() {
                        let sender_key = make_tx_key(&tx.sender, block.index, tx_pos);
                        tx_table.remove(sender_key.as_slice())?;
                        sigs_table.remove(tx.signature.as_slice())?;
                    }
                }
            }

            // Insert new canonical blocks into all tables.
            for block in new_blocks {
                let block_data = bincode::serialize(block)?;
                blocks_table.insert(block.index, block_data.as_slice())?;

                for (tx_pos, tx) in block.transactions.iter().enumerate() {
                    let value =
                        bincode::serialize(&(&tx.sender, &tx.receiver, &tx.amount, &tx.fee))?;
                    let receiver_key = make_tx_key(&tx.receiver, block.index, tx_pos);
                    tx_table.insert(receiver_key.as_slice(), value.as_slice())?;
                    if !tx.sender.is_empty() {
                        let sender_key = make_tx_key(&tx.sender, block.index, tx_pos);
                        tx_table.insert(sender_key.as_slice(), value.as_slice())?;
                        sigs_table.insert(tx.signature.as_slice(), block.index)?;
                    }
                }

                // Add lottery payouts for this new block.
                if let Some(payouts) = new_payouts_by_block.get(&block.index) {
                    if !payouts.is_empty() {
                        let data = bincode::serialize(payouts)?;
                        lp_table.insert(block.index, data.as_slice())?;
                        for (i, (receiver, amount, _tier)) in payouts.iter().enumerate() {
                            let key = make_tx_key(receiver, block.index, 0xFFFF_0000 + i);
                            let value = bincode::serialize(&(
                                "lottery".to_string(),
                                receiver.clone(),
                                *amount,
                                0u64,
                            ))?;
                            tx_table.insert(key.as_slice(), value.as_slice())?;
                        }
                    }
                }
            }

            // Replace lottery tickets atomically with the rest of the reorg.
            let ticket_keys: Vec<u64> = tickets_table
                .range::<u64>(..)?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in ticket_keys {
                tickets_table.remove(k)?;
            }
            for t in tickets {
                let ticket_data = bincode::serialize(&t.miner)?;
                tickets_table.insert(t.created_height, ticket_data.as_slice())?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Deep-reorg fallback: rebuild TX_INDEX + CONFIRMED_SIGS + TICKETS from
    /// scratch in a single write transaction. Called after reorgs whose common
    /// ancestor lies before the in-memory window.
    pub fn rebuild_indices_with_tickets(
        &self,
        tickets: &[LootTicket],
        new_payouts_by_block: &HashMap<u64, Vec<(String, u64, String)>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let all_blocks = self.load_canonical_chain()?;
        let wtxn = self.db.begin_write()?;
        {
            let mut tx_table = wtxn.open_table(TX_INDEX)?;
            let mut sigs_table = wtxn.open_table(CONFIRMED_SIGS)?;
            let mut tickets_table = wtxn.open_table(TICKETS)?;
            let mut lp_table = wtxn.open_table(LOTTERY_PAYOUTS)?;

            // Clear TX_INDEX
            let tx_keys: Vec<Vec<u8>> = tx_table
                .range::<&[u8]>(..)?
                .map(|e| e.map(|(k, _)| k.value().to_vec()))
                .collect::<Result<_, _>>()?;
            for k in tx_keys {
                tx_table.remove(k.as_slice())?;
            }

            // Clear CONFIRMED_SIGS
            let sig_keys: Vec<Vec<u8>> = sigs_table
                .range::<&[u8]>(..)?
                .map(|e| e.map(|(k, _)| k.value().to_vec()))
                .collect::<Result<_, _>>()?;
            for k in sig_keys {
                sigs_table.remove(k.as_slice())?;
            }

            // Clear LOTTERY_PAYOUTS
            let lp_keys: Vec<u64> = lp_table
                .range::<u64>(..)?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in lp_keys {
                lp_table.remove(k)?;
            }

            // Rebuild TX_INDEX + CONFIRMED_SIGS from canonical chain
            for block in &all_blocks {
                for (tx_pos, tx) in block.transactions.iter().enumerate() {
                    let value =
                        bincode::serialize(&(&tx.sender, &tx.receiver, &tx.amount, &tx.fee))?;
                    let receiver_key = make_tx_key(&tx.receiver, block.index, tx_pos);
                    tx_table.insert(receiver_key.as_slice(), value.as_slice())?;
                    if !tx.sender.is_empty() {
                        let sender_key = make_tx_key(&tx.sender, block.index, tx_pos);
                        tx_table.insert(sender_key.as_slice(), value.as_slice())?;
                        sigs_table.insert(tx.signature.as_slice(), block.index)?;
                    }
                }
            }

            // Write lottery payouts and their TX_INDEX entries
            for (block_index, payouts) in new_payouts_by_block {
                if !payouts.is_empty() {
                    let data = bincode::serialize(payouts)?;
                    lp_table.insert(*block_index, data.as_slice())?;
                    for (i, (receiver, amount, _tier)) in payouts.iter().enumerate() {
                        let key = make_tx_key(receiver, *block_index, 0xFFFF_0000 + i);
                        let value = bincode::serialize(&(
                            "lottery".to_string(),
                            receiver.clone(),
                            *amount,
                            0u64,
                        ))?;
                        tx_table.insert(key.as_slice(), value.as_slice())?;
                    }
                }
            }

            // Replace tickets
            let ticket_keys: Vec<u64> = tickets_table
                .range::<u64>(..)?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in ticket_keys {
                tickets_table.remove(k)?;
            }
            for t in tickets {
                let ticket_data = bincode::serialize(&t.miner)?;
                tickets_table.insert(t.created_height, ticket_data.as_slice())?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Lottery payouts query
    // -------------------------------------------------------------------------

    /// Return lottery payouts for block indices in [from, from + limit).
    pub fn get_lottery_payouts_range(
        &self,
        from: u64,
        limit: usize,
    ) -> Result<LotteryPayoutsMap, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(LOTTERY_PAYOUTS)?;
        let end = from.saturating_add(limit as u64);
        let mut map = HashMap::new();
        for entry in table.range(from..end)? {
            let (k, v) = entry?;
            let payouts: Vec<(String, u64, String)> = bincode::deserialize(v.value())?;
            map.insert(k.value(), payouts);
        }
        Ok(map)
    }

    /// Return the most recent lottery payouts, optionally filtered by tier
    /// ("small", "medium", "large", "jackpot"). Results are newest-first.
    pub fn get_recent_lottery_payouts(
        &self,
        tier: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PayoutRecord>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let lp_table = rtxn.open_table(LOTTERY_PAYOUTS)?;
        let blocks_table = rtxn.open_table(BLOCKS)?;
        let mut results = Vec::new();
        'outer: for entry in lp_table.range::<u64>(..)?.rev() {
            let (k, v) = entry?;
            let block_index = k.value();
            let payouts: Vec<(String, u64, String)> = bincode::deserialize(v.value())?;
            let block_timestamp = blocks_table
                .get(&block_index)?
                .and_then(|b| bincode::deserialize::<Block>(b.value()).ok())
                .map(|b| b.timestamp);
            for (receiver, amount, payout_tier) in payouts {
                if tier.is_none_or(|t| t == payout_tier) {
                    results.push(PayoutRecord {
                        block_index,
                        block_timestamp,
                        receiver,
                        amount,
                        tier: payout_tier,
                    });
                    if results.len() >= limit {
                        break 'outer;
                    }
                }
            }
        }
        Ok(results)
    }

    // -------------------------------------------------------------------------
    // Lottery tickets
    // -------------------------------------------------------------------------

    pub fn load_tickets(&self) -> Result<Vec<LootTicket>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(TICKETS)?;
        let mut tickets = Vec::new();
        for entry in table.range::<u64>(..)? {
            let (k, v) = entry?;
            let miner: String = bincode::deserialize(v.value())?;
            tickets.push(LootTicket {
                created_height: k.value(),
                miner,
            });
        }
        Ok(tickets)
    }

    // -------------------------------------------------------------------------
    // Confirmed-signature log (replay protection)
    // -------------------------------------------------------------------------

    /// Returns true if this signature has ever been confirmed on-chain.
    pub fn is_confirmed_signature(&self, sig: &[u8]) -> Result<bool, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(CONFIRMED_SIGS)?;
        Ok(table.get(sig)?.is_some())
    }

    // -------------------------------------------------------------------------
    // Peer list
    // -------------------------------------------------------------------------

    pub fn save_peer(&self, url: &str) -> Result<(), Box<dyn std::error::Error>> {
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(PEERS)?;
            table.insert(url, &[] as &[u8])?;
        }
        wtxn.commit()?;
        Ok(())
    }

    pub fn delete_peer(&self, url: &str) -> Result<(), Box<dyn std::error::Error>> {
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(PEERS)?;
            table.remove(url)?;
        }
        wtxn.commit()?;
        Ok(())
    }

    // ── Metrics seeding ───────────────────────────────────────────────────────

    /// Scan every persisted block and return cumulative fee totals.
    /// Returns `(total_fees_collected, miner_share_total)`.
    /// Used once at startup to restore counter state after a restart.
    pub fn scan_fee_totals(&self) -> Result<(u64, u64), Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(BLOCKS)?;
        let mut total_fees = 0u64;
        let mut miner_share_total = 0u64;
        for entry in table.range::<u64>(..)? {
            let (_, v) = entry?;
            let block: Block = bincode::deserialize(v.value())?;
            let block_fees: u64 = block
                .transactions
                .iter()
                .filter(|tx| !tx.sender.is_empty())
                .map(|tx| tx.fee)
                .sum();
            total_fees = total_fees.saturating_add(block_fees);
            miner_share_total = miner_share_total.saturating_add(block_fees / 2);
        }
        Ok((total_fees, miner_share_total))
    }

    /// Scan every persisted lottery payout and return per-tier totals.
    /// Returns a map of `tier → (win_count, total_coins_paid)`.
    /// Used once at startup to restore counter state after a restart.
    pub fn scan_lottery_payout_totals(
        &self,
    ) -> Result<HashMap<String, (u64, u64)>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(LOTTERY_PAYOUTS)?;
        let mut totals: HashMap<String, (u64, u64)> = HashMap::new();
        for entry in table.range::<u64>(..)? {
            let (_, v) = entry?;
            let payouts: Vec<(String, u64, String)> = bincode::deserialize(v.value())?;
            for (_receiver, amount, tier) in payouts {
                let e = totals.entry(tier).or_insert((0, 0));
                e.0 += 1;
                e.1 = e.1.saturating_add(amount);
            }
        }
        Ok(totals)
    }

    pub fn load_peers(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(PEERS)?;
        let peers = table
            .range::<&str>(..)?
            .map(|e| e.map(|(k, _)| k.value().to_string()))
            .collect::<Result<_, _>>()?;
        Ok(peers)
    }

    // -------------------------------------------------------------------------
    // Mempool persistence
    // -------------------------------------------------------------------------

    /// Persist a single pending transaction. Called write-through from Mempool::add_transaction.
    pub fn save_mempool_tx(
        &self,
        sig: &[u8],
        tx: &lootcoin_core::transaction::Transaction,
        added_height: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let data = bincode::serialize(&(tx, added_height))?;
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(MEMPOOL)?;
            table.insert(sig, data.as_slice())?;
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Remove multiple pending transactions in a single write transaction.
    pub fn remove_mempool_txs(&self, sigs: &[Vec<u8>]) -> Result<(), Box<dyn std::error::Error>> {
        if sigs.is_empty() {
            return Ok(());
        }
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(MEMPOOL)?;
            for sig in sigs {
                table.remove(sig.as_slice())?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Load all persisted mempool entries. Returns `(tx, added_height)` pairs.
    pub fn load_mempool(
        &self,
    ) -> Result<Vec<(lootcoin_core::transaction::Transaction, u64)>, Box<dyn std::error::Error>>
    {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(MEMPOOL)?;
        let mut entries = Vec::new();
        for entry in table.range::<&[u8]>(..)? {
            let (_, v) = entry?;
            let (tx, added_height): (lootcoin_core::transaction::Transaction, u64) =
                bincode::deserialize(v.value())?;
            entries.push((tx, added_height));
        }
        Ok(entries)
    }

    // -------------------------------------------------------------------------
    // Checkpoint snapshots
    // -------------------------------------------------------------------------

    /// Persist a serialized CheckpointState at the given block height.
    /// Overwrites any existing entry at that height.
    pub fn save_checkpoint(&self, height: u64, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(CHECKPOINTS)?;
            table.insert(height, data)?;
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Return the raw bincode bytes for a specific checkpoint height, or `None`
    /// if no checkpoint exists at that height.
    pub fn load_checkpoint(
        &self,
        height: u64,
    ) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(CHECKPOINTS)?;
        match table.get(&height)? {
            Some(v) => Ok(Some(v.value().to_vec())),
            None => Ok(None),
        }
    }

    /// Return the highest stored checkpoint as `(height, raw_bytes)`, or `None`
    /// if no checkpoints exist yet.
    #[allow(clippy::type_complexity)]
    pub fn load_latest_checkpoint(
        &self,
    ) -> Result<Option<(u64, Vec<u8>)>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(CHECKPOINTS)?;
        match table.range::<u64>(..)?.next_back() {
            Some(Ok((k, v))) => Ok(Some((k.value(), v.value().to_vec()))),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Delete all checkpoint entries with key >= `from_height`.
    /// Called after a reorg to purge checkpoints from the displaced chain.
    pub fn delete_checkpoints_from(
        &self,
        from_height: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(CHECKPOINTS)?;
            let keys: Vec<u64> = table
                .range(from_height..)?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in keys {
                table.remove(k)?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Load every block with index >= `from` in ascending order.
    /// Used at startup to replay only the tail after restoring a checkpoint.
    pub fn load_blocks_from(&self, from: u64) -> Result<Vec<Block>, Box<dyn std::error::Error>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(BLOCKS)?;
        let mut blocks = Vec::new();
        for entry in table.range(from..)? {
            let (_, v) = entry?;
            blocks.push(bincode::deserialize(v.value())?);
        }
        Ok(blocks)
    }
}

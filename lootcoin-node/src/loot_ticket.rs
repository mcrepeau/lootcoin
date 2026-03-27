// Protocol constants live in lootcoin-core so every crate shares the same values.
pub(crate) use lootcoin_core::lottery::{
    JACKPOT_DIVISOR, LARGE_DIVISOR, MEDIUM_DIVISOR, PPM, REVEAL_BLOCKS, SMALL_DIVISOR,
    TICKET_MATURITY, TX_MULTIPLIER_CAP,
};

/// System-generated ticket issued to the miner of each block.
/// Matures after TICKET_MATURITY blocks, then settled using REVEAL_BLOCKS
/// of accumulated entropy.
///
/// `tx_count` is the number of non-coinbase transactions in the block that
/// earned this ticket. The final payout is scaled by
/// `min(tx_count, TX_MULTIPLIER_CAP) / TX_MULTIPLIER_CAP`, giving miners
/// a continuous per-transaction incentive up to the cap.
#[derive(Clone)]
pub struct LootTicket {
    pub miner: String,
    pub created_height: u64,
    /// Non-coinbase transaction count in the block that earned this ticket.
    pub tx_count: u64,
}

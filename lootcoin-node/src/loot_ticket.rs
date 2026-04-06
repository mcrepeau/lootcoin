// Protocol constants live in lootcoin-core so every crate shares the same values.
pub(crate) use lootcoin_core::lottery::{
    JACKPOT_BUCKET_START, JACKPOT_DIVISOR, LARGE_BUCKET_START, LARGE_DIVISOR, MEDIUM_BUCKET_START,
    MEDIUM_DIVISOR, MIN_TX_FEE, PPM, REVEAL_BLOCKS, SMALL_BUCKET_START, SMALL_DIVISOR,
};

/// System-generated ticket issued to the miner of each block that contains
/// at least one non-coinbase transaction.
/// Settled at `created_height + REVEAL_BLOCKS` using the hashes of blocks
/// `[created_height, created_height + REVEAL_BLOCKS)` as entropy.
/// Payout is a flat fraction of the pot (`pot / DIVISOR`);
/// per-transaction incentives come from the 50/50 fee split instead.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LootTicket {
    pub miner: String,
    pub created_height: u64,
}

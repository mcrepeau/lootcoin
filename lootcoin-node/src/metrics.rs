use prometheus_client::{
    encoding::{text::encode, EncodeLabelSet},
    metrics::{counter::Counter, family::Family, gauge::Gauge},
    registry::Registry,
};
use std::sync::atomic::AtomicU64;

/// Label set used to split lottery metrics by tier.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TierLabel {
    pub tier: String,
}

/// All Prometheus metrics exposed on `GET /metrics`.
///
/// Gauges are updated lazily in the handler by reading current chain state.
/// Counters (fees, lottery wins) are incremented eagerly as blocks are applied
/// so the values remain accurate even between `/metrics` scrapes.
pub struct Metrics {
    pub registry: Registry,

    // ── Chain state (gauges) ──────────────────────────────────────────────────
    pub chain_height: Gauge<f64, AtomicU64>,
    pub chain_difficulty: Gauge<f64, AtomicU64>,
    pub avg_block_time_secs: Gauge<f64, AtomicU64>,
    pub secs_since_last_block: Gauge<f64, AtomicU64>,

    // ── Economics (gauges) ────────────────────────────────────────────────────
    pub pot_coins: Gauge<f64, AtomicU64>,
    pub circulating_coins: Gauge<f64, AtomicU64>,
    pub total_supply: Gauge<f64, AtomicU64>,

    // ── Lottery (counters by tier) ────────────────────────────────────────────
    pub lottery_wins_total: Family<TierLabel, Counter<u64, AtomicU64>>,
    pub lottery_payouts_coins_total: Family<TierLabel, Counter<u64, AtomicU64>>,

    // ── Fees (counters) ───────────────────────────────────────────────────────
    pub fees_collected_total: Counter<u64, AtomicU64>,
    pub fees_to_miners_total: Counter<u64, AtomicU64>,
    pub fees_to_pot_total: Counter<u64, AtomicU64>,

    // ── Network (gauges) ──────────────────────────────────────────────────────
    pub mempool_size: Gauge<f64, AtomicU64>,
    pub peer_count: Gauge<f64, AtomicU64>,

    // ── Blocks (counter) ──────────────────────────────────────────────────────
    pub blocks_total: Counter<u64, AtomicU64>,
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let chain_height: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_chain_height",
            "Current chain height in blocks",
            chain_height.clone(),
        );

        let chain_difficulty: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_chain_difficulty",
            "Current mining difficulty in fractional bits",
            chain_difficulty.clone(),
        );

        let avg_block_time_secs: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_avg_block_time_secs",
            "Rolling average block time over the last 10 blocks",
            avg_block_time_secs.clone(),
        );

        let secs_since_last_block: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_secs_since_last_block",
            "Seconds elapsed since the most recent block",
            secs_since_last_block.clone(),
        );

        let pot_coins: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_pot_coins",
            "Current lottery pot balance in coins",
            pot_coins.clone(),
        );

        let circulating_coins: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_circulating_coins",
            "Coins in circulation (total supply minus pot)",
            circulating_coins.clone(),
        );

        let total_supply: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_total_supply",
            "Total coin supply (circulating plus pot)",
            total_supply.clone(),
        );

        let lottery_wins_total: Family<TierLabel, Counter<u64, AtomicU64>> = Family::default();
        registry.register(
            "lootcoin_lottery_wins",
            "Cumulative lottery ticket wins by tier",
            lottery_wins_total.clone(),
        );

        let lottery_payouts_coins_total: Family<TierLabel, Counter<u64, AtomicU64>> =
            Family::default();
        registry.register(
            "lootcoin_lottery_payouts_coins",
            "Cumulative lottery coins paid out by tier",
            lottery_payouts_coins_total.clone(),
        );

        let fees_collected_total: Counter<u64, AtomicU64> = Counter::default();
        registry.register(
            "lootcoin_fees_collected",
            "Cumulative transaction fees collected by the network",
            fees_collected_total.clone(),
        );

        let fees_to_miners_total: Counter<u64, AtomicU64> = Counter::default();
        registry.register(
            "lootcoin_fees_to_miners",
            "Cumulative fees paid directly to block miners (50% of collected)",
            fees_to_miners_total.clone(),
        );

        let fees_to_pot_total: Counter<u64, AtomicU64> = Counter::default();
        registry.register(
            "lootcoin_fees_to_pot",
            "Cumulative fees added to the lottery pot (50% of collected)",
            fees_to_pot_total.clone(),
        );

        let mempool_size: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_mempool_size",
            "Number of transactions currently in the mempool",
            mempool_size.clone(),
        );

        let peer_count: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "lootcoin_peer_count",
            "Number of known peers",
            peer_count.clone(),
        );

        let blocks_total: Counter<u64, AtomicU64> = Counter::default();
        registry.register(
            "lootcoin_blocks",
            "Cumulative number of blocks applied to the main chain",
            blocks_total.clone(),
        );

        Self {
            registry,
            chain_height,
            chain_difficulty,
            avg_block_time_secs,
            secs_since_last_block,
            pot_coins,
            circulating_coins,
            total_supply,
            lottery_wins_total,
            lottery_payouts_coins_total,
            fees_collected_total,
            fees_to_miners_total,
            fees_to_pot_total,
            mempool_size,
            peer_count,
            blocks_total,
        }
    }

    /// Restore fee counters from persisted history on node startup.
    /// `blocks` is the number of blocks already on-chain (excluding genesis).
    pub fn seed_fees(&self, total_fees: u64, miner_share: u64, blocks: u64) {
        self.fees_collected_total.inc_by(total_fees);
        self.fees_to_miners_total.inc_by(miner_share);
        self.fees_to_pot_total
            .inc_by(total_fees.saturating_sub(miner_share));
        self.blocks_total.inc_by(blocks);
    }

    /// Restore lottery counters from persisted history on node startup.
    pub fn seed_lottery(&self, tier: &str, wins: u64, coins: u64) {
        let label = TierLabel {
            tier: tier.to_string(),
        };
        self.lottery_wins_total.get_or_create(&label).inc_by(wins);
        if coins > 0 {
            self.lottery_payouts_coins_total
                .get_or_create(&label)
                .inc_by(coins);
        }
    }

    /// Record a settled lottery ticket.  Called from `Blockchain::update_state`.
    pub fn record_lottery_win(&self, tier: &str, amount: u64) {
        let label = TierLabel {
            tier: tier.to_string(),
        };
        self.lottery_wins_total.get_or_create(&label).inc();
        if amount > 0 {
            self.lottery_payouts_coins_total
                .get_or_create(&label)
                .inc_by(amount);
        }
    }

    /// Record the fee split for one block.  Called from `Blockchain::update_state`.
    pub fn record_fees(&self, total_fees: u64, miner_share: u64) {
        self.fees_collected_total.inc_by(total_fees);
        self.fees_to_miners_total.inc_by(miner_share);
        self.fees_to_pot_total.inc_by(total_fees - miner_share);
        self.blocks_total.inc();
    }

    /// Render all metrics in Prometheus text exposition format.
    pub fn encode_to_string(&self) -> String {
        let mut buf = String::new();
        encode(&mut buf, &self.registry).expect("metrics encoding failed");
        buf
    }
}

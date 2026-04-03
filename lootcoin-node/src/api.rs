use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use hex::FromHex;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tokio_stream::{wrappers::BroadcastStream, Stream};
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing::{info, warn};

use crate::blockchain::{BlockOutcome, Blockchain, CheckpointState};
use crate::db::Db;
use crate::gossip::{Gossip, NodeEvent};
use crate::loot_ticket::LootTicket;
use crate::mempool::{FeeStats, Mempool};
use crate::metrics::Metrics;
use lootcoin_core::block::MAX_BLOCK_TXS;
use lootcoin_core::{
    block::{meets_difficulty, Block},
    transaction::Transaction,
};
use std::collections::HashMap;

/// Maximum number of concurrent SSE subscribers. Prevents memory/CPU exhaustion
/// from an attacker opening thousands of long-lived event stream connections.
const MAX_SSE_SUBSCRIBERS: usize = 100;

pub use crate::checkpoints::TRUSTED_CHECKPOINTS;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub chain: Arc<RwLock<Blockchain>>,
    pub mempool: Arc<RwLock<Mempool>>,
    pub gossip: Arc<Gossip>,
    pub shutdown_rx: watch::Receiver<bool>,
    pub sse_subscribers: Arc<AtomicUsize>,
    pub metrics: Arc<Metrics>,
    /// Lowest block height for which this node has complete block and TX-index
    /// data. 0 for archive nodes (full replay from genesis); set to the
    /// checkpoint height for nodes that bootstrapped from a snapshot.
    pub history_start: u64,
}

/// RAII guard: decrements the SSE subscriber counter when dropped (i.e. when
/// the stream is closed or the connection is dropped by the client).
struct SseGuard(Arc<AtomicUsize>);
impl Drop for SseGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Deserialize)]
pub struct SubmitTransactionRequest {
    pub sender: String,
    pub receiver: String,
    pub amount: u64,
    pub fee: u64,
    pub nonce: u64,
    // Accept hex strings from clients for better ergonomics
    pub public_key_hex: String,
    pub signature_hex: String,
}

#[derive(Serialize)]
pub struct SubmitTransactionResponse {
    pub sender: String,
    pub receiver: String,
    pub amount: u64,
    pub fee: u64,
}

#[derive(Serialize)]
pub struct BalanceResponse {
    pub address: String,
    /// Confirmed on-chain balance.
    pub balance: u64,
    /// Balance minus pending mempool debits — the amount safely spendable right now.
    pub spendable_balance: u64,
}

#[derive(Serialize, Deserialize)]
pub struct ChainHeadResponse {
    pub height: u64,
    pub latest_hash_hex: String,
    pub difficulty: f64,
    pub mempool_size: usize,
    pub avg_block_time_secs: Option<f64>,
    /// Hex-encoded u128: cumulative sum of 2^difficulty across all main-chain
    /// blocks. Used by peers to select the best chain without trusting height.
    pub chain_work_hex: String,
    /// Current lottery pot balance in coins.
    pub pot: u64,
}

#[derive(Serialize)]
pub struct NodeInfoResponse {
    /// Semver version of the running node binary.
    pub version: &'static str,
    /// Lowest block height for which this node has complete history.
    /// 0 means full archive (replayed from genesis).
    pub history_start: u64,
    /// This node's publicly reachable URL, if configured via NODE_URL.
    pub node_url: Option<String>,
}

/// Entry in the GET /snapshots list.
#[derive(Serialize)]
pub struct SnapshotInfo {
    pub height: u64,
    /// Lowercase hex block hash — matches the corresponding TRUSTED_CHECKPOINTS entry.
    pub block_hash_hex: String,
}

/// Full snapshot payload returned by GET /snapshot/{height}.
///
/// Contains all derived state a bootstrapping node needs to resume from this
/// height without replaying blocks from genesis. The block hash is verified
/// against TRUSTED_CHECKPOINTS before the payload is accepted.
///
/// `chain_work` is hex-encoded (u128 doesn't round-trip through JSON f64).
#[derive(Serialize, Deserialize)]
pub struct SnapshotPayload {
    pub height: u64,
    pub block_hash_hex: String,
    pub balances: HashMap<String, u64>,
    pub pot: u64,
    pub chain_work_hex: String,
    pub current_difficulty: f64,
    /// ASERT anchor: (height, timestamp, difficulty) for block 1.
    pub asert_anchor: Option<(u64, u64, f64)>,
    pub tickets: Vec<LootTicket>,
}

#[derive(Deserialize)]
pub struct BlockRangeQuery {
    pub from: u64,
    pub limit: Option<usize>,
}

/// A lottery payout entry as returned by GET /blocks.
#[derive(Serialize)]
pub struct LotteryPayoutView {
    pub receiver: String,
    pub amount: u64,
}

/// Block as returned by GET /blocks. Identical to `Block` plus a
/// `lottery_payouts` field carrying any lottery settlements that were
/// triggered by this block. Peers syncing via this endpoint deserialize
/// it as `Block` — serde ignores the extra field automatically.
#[derive(Serialize)]
pub struct BlockView {
    pub index: u64,
    pub previous_hash: Vec<u8>,
    pub timestamp: u64,
    pub nonce: u64,
    pub transactions: Vec<Transaction>,
    pub tx_root: Vec<u8>,
    pub hash: Vec<u8>,
    pub lottery_payouts: Vec<LotteryPayoutView>,
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

/// Core block-application logic shared by the HTTP handler and the SSE peer
/// relay.  Validates the block, applies it to the chain, updates the mempool,
/// and publishes the event so all SSE subscribers are notified.
///
/// Returns:
///   `Ok(true)`  — Applied or Reorged (chain tip advanced, event published)
///   `Ok(false)` — Orphaned (valid PoW but not on the main chain)
///   `Err(msg)`  — Invalid block; `msg` is a short static description
async fn apply_incoming_block(state: &AppState, block: Block) -> Result<bool, &'static str> {
    let tx_count = block.transactions.len();

    if tx_count > MAX_BLOCK_TXS + 1 {
        return Err("too many transactions in block");
    }

    let coinbase = &block.transactions[0];
    if !coinbase.sender.is_empty() {
        return Err("first transaction must be coinbase");
    }
    if coinbase.amount > 1 {
        return Err("coinbase amount too high");
    }
    if coinbase.fee != 0 {
        return Err("coinbase fee must be zero");
    }
    // Only the first transaction may be a coinbase. Extra empty-sender transactions
    // would let a miner mint coins from thin air.
    if block
        .transactions
        .iter()
        .skip(1)
        .any(|t| t.sender.is_empty())
    {
        return Err("extra coinbase transaction");
    }

    let recomputed = block.calculate_hash();
    if recomputed != block.hash {
        return Err("invalid block hash");
    }

    let candidate = block.clone();
    let mut chain = state.chain.write().await;

    let diff = chain.get_difficulty();
    if !meets_difficulty(&block.hash, diff) {
        return Err("block does not meet difficulty");
    }

    let outcome = chain.apply_block(&state.db, block);
    let height = chain.get_height();

    match outcome {
        BlockOutcome::Applied => {
            info!(target: "api", "Block applied: index={} txs={} pot={}", candidate.index, tx_count, chain.get_pot());
            let mut pool = state.mempool.write().await;
            pool.remove_included(&candidate.transactions);
            pool.evict_expired(height);
            drop(pool);
            drop(chain);
            if state.gossip.publish_block(&candidate).await {
                state.gossip.push_block_to_peers(&candidate).await;
            }
            Ok(true)
        }
        BlockOutcome::Reorged {
            old_blocks,
            new_blocks,
        } => {
            info!(target: "api", "Block triggered reorg: index={} new height={}", candidate.index, height);
            let mut pool = state.mempool.write().await;
            for old_block in &old_blocks {
                pool.readd_displaced(
                    &old_block.transactions,
                    |addr| chain.get_balance(addr),
                    height,
                );
            }
            // Remove every transaction confirmed on the new canonical fork, not just
            // the triggering block. In a multi-block reorg, transactions confirmed in
            // earlier new blocks would otherwise remain in the mempool while their
            // signatures are in confirmed_signatures, causing every subsequent mined
            // block to be permanently rejected.
            for new_block in &new_blocks {
                pool.remove_included(&new_block.transactions);
            }
            pool.evict_expired(height);
            drop(pool);
            drop(chain);
            if state.gossip.publish_block(&candidate).await {
                state.gossip.push_block_to_peers(&candidate).await;
            }
            Ok(true)
        }
        BlockOutcome::Orphaned => {
            info!(target: "api", "Block orphaned (valid fork candidate): index={}", candidate.index);
            Ok(false)
        }
        BlockOutcome::Rejected => {
            warn!(target: "api", "Block rejected: index={} txs={}", candidate.index, tx_count);
            Err("block rejected")
        }
    }
}

/// Core transaction-relay logic shared by the HTTP relay handler and the SSE
/// peer relay.  Validates the transaction, adds it to the mempool, and
/// publishes the event.
///
/// Returns `true` if the transaction was new and accepted.
async fn relay_tx_inner(state: &AppState, tx: Transaction) -> bool {
    if tx.sender == tx.receiver {
        warn!(target: "api", "relayed tx rejected: sender == receiver");
        return false;
    }
    if !tx.verify() {
        warn!(target: "api", "relayed tx failed signature check");
        return false;
    }
    if state
        .db
        .is_confirmed_signature(&tx.signature)
        .unwrap_or(false)
    {
        warn!(target: "api", "relay tx replay attempt rejected");
        return false;
    }
    let (chain_ok, height, confirmed_balance) = {
        let chain = state.chain.read().await;
        let ok = chain.validate_transaction_state(&tx);
        let height = chain.get_height();
        let confirmed = chain.get_balance(&tx.sender);
        (ok, height, confirmed)
    };
    if !chain_ok {
        return false;
    }
    // Check effective balance (confirmed − pending) and insert under the same
    // write lock — same TOCTOU protection as submit_transaction_handler.
    {
        let mut pool = state.mempool.write().await;
        let already_pending = pool.pending_debit(&tx.sender);
        let cost = tx.amount.saturating_add(tx.fee);
        if confirmed_balance.saturating_sub(already_pending) < cost {
            warn!(target: "api", "relayed tx rejected: insufficient effective balance");
            return false;
        }
        if !pool.add_transaction(tx.clone(), height) {
            return false; // full or duplicate
        }
    }
    state.gossip.publish_transaction(&tx).await;
    true
}

// ─── HTTP handlers ────────────────────────────────────────────────────────────

// Accepts a fully signed transaction (client signs). Adds to mempool if valid.
pub async fn submit_transaction_handler(
    State(state): State<AppState>,
    Json(req): Json<SubmitTransactionRequest>,
) -> Result<Json<SubmitTransactionResponse>, (axum::http::StatusCode, String)> {
    info!(
        target: "api",
        "Submit tx request: sender={}, receiver={}, amount={}, fee={}",
        req.sender, req.receiver, req.amount, req.fee
    );

    if req.sender == req.receiver {
        warn!(target: "api", "invalid tx: sender == receiver");
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "the receiver must be different from the sender".to_string(),
        ));
    }

    // Decode hex public key
    let pk_bytes_vec = match <Vec<u8>>::from_hex(&req.public_key_hex) {
        Ok(v) => v,
        Err(_) => {
            warn!(target: "api", "invalid public_key_hex");
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "invalid public_key_hex".to_string(),
            ));
        }
    };
    if pk_bytes_vec.len() != 32 {
        warn!(target: "api", "public_key length != 32");
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "public_key must be 32 bytes".to_string(),
        ));
    }
    let mut pk_bytes = [0u8; 32];
    pk_bytes.copy_from_slice(&pk_bytes_vec);

    // Decode hex signature (64 bytes for ed25519)
    let sig_bytes = match <Vec<u8>>::from_hex(&req.signature_hex) {
        Ok(v) => v,
        Err(_) => {
            warn!(target: "api", "invalid signature_hex");
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "invalid signature_hex".to_string(),
            ));
        }
    };

    let tx = Transaction {
        sender: req.sender,
        receiver: req.receiver,
        amount: req.amount,
        fee: req.fee,
        nonce: req.nonce,
        public_key: pk_bytes,
        signature: sig_bytes,
    };

    if !tx.verify() {
        warn!(target: "api", "tx signature verification failed");
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "invalid signature".into(),
        ));
    }

    // Replay protection: reject signatures already confirmed beyond the in-memory window
    if state
        .db
        .is_confirmed_signature(&tx.signature)
        .unwrap_or(false)
    {
        warn!(target: "api", "tx replay attempt rejected: sig already confirmed");
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "transaction already confirmed".into(),
        ));
    }

    // Validate fee, replay, and confirmed balance under the chain read lock.
    // We also capture the confirmed balance here so we can re-check effective
    // balance (confirmed − pending) under the mempool write lock below.
    let (chain_ok, current_height, confirmed_balance) = {
        let chain = state.chain.read().await;
        let ok = chain.validate_transaction_state(&tx);
        let height = chain.get_height();
        let confirmed = chain.get_balance(&tx.sender);
        (ok, height, confirmed)
    };
    if !chain_ok {
        warn!(target: "api", "tx validation failed (balance/fee/replay)");
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "transaction not valid (balance or fee)".into(),
        ));
    }

    // Check effective balance AND insert under the same write lock to eliminate
    // the TOCTOU race: two concurrent requests for the same sender both passing
    // the check before either has been inserted.
    let (accepted, mempool_len) = {
        let mut pool = state.mempool.write().await;
        let already_pending = pool.pending_debit(&tx.sender);
        let cost = tx.amount.saturating_add(tx.fee);
        if confirmed_balance.saturating_sub(already_pending) < cost {
            warn!(target: "api", "tx validation failed (effective balance)");
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "transaction not valid (balance or fee)".into(),
            ));
        }
        let ok = pool.add_transaction(tx.clone(), current_height);
        (ok, pool.len())
    };
    if !accepted {
        warn!(target: "api", "mempool full, rejecting tx from {}", tx.sender);
        return Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "mempool full".into(),
        ));
    }

    info!(
        target: "api",
        "Tx accepted into mempool (size now {}) sender={} receiver={} amount={} fee={}",
        mempool_len, tx.sender, tx.receiver, tx.amount, tx.fee
    );
    state.gossip.publish_transaction(&tx).await;
    Ok(Json(SubmitTransactionResponse {
        sender: tx.sender,
        receiver: tx.receiver,
        amount: tx.amount,
        fee: tx.fee,
    }))
}

#[derive(Deserialize)]
pub struct TxPageQuery {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub async fn get_address_transactions_handler(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(query): Query<TxPageQuery>,
) -> Result<Json<Vec<crate::db::TxRecord>>, (axum::http::StatusCode, String)> {
    const MAX_LIMIT: usize = 500;
    let limit = query.limit.unwrap_or(50).min(MAX_LIMIT);
    let offset = query.offset.unwrap_or(0);
    state
        .db
        .get_transactions_for_address(&address, offset, limit)
        .map(Json)
        .map_err(|e| {
            warn!(target: "api", "tx index query failed for {}: {}", address, e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "index query failed".into(),
            )
        })
}

pub async fn get_balance_handler(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> Json<BalanceResponse> {
    let balance = state.chain.read().await.get_balance(&address);
    let pending = state.mempool.read().await.pending_debit(&address);
    let spendable_balance = balance.saturating_sub(pending);
    Json(BalanceResponse {
        address,
        balance,
        spendable_balance,
    })
}

#[derive(Serialize)]
pub struct MempoolEntry {
    pub transaction: Transaction,
    pub added_height: u64,
}

pub async fn list_mempool_handler(State(state): State<AppState>) -> Json<Vec<MempoolEntry>> {
    let pool = state.mempool.read().await;
    Json(
        pool.all_transactions_with_height()
            .into_iter()
            .map(|(transaction, added_height)| MempoolEntry {
                transaction,
                added_height,
            })
            .collect(),
    )
}

#[derive(Deserialize)]
pub struct RecentPayoutsQuery {
    pub tier: Option<String>,
    pub limit: Option<usize>,
}

pub async fn get_recent_lottery_payouts_handler(
    State(state): State<AppState>,
    Query(query): Query<RecentPayoutsQuery>,
) -> Result<Json<Vec<crate::db::PayoutRecord>>, (axum::http::StatusCode, String)> {
    const MAX_LIMIT: usize = 100;
    let limit = query.limit.unwrap_or(10).min(MAX_LIMIT);
    let tier = query.tier.as_deref();
    state
        .db
        .get_recent_lottery_payouts(tier, limit)
        .map(Json)
        .map_err(|e| {
            warn!(target: "api", "lottery payouts query failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "query failed".into(),
            )
        })
}

pub async fn mempool_fees_handler(State(state): State<AppState>) -> Json<FeeStats> {
    let pool = state.mempool.read().await;
    Json(pool.fee_stats())
}

pub async fn get_blocks_handler(
    State(state): State<AppState>,
    Query(query): Query<BlockRangeQuery>,
) -> Json<Vec<BlockView>> {
    const MAX_LIMIT: usize = 500;
    let limit = query.limit.unwrap_or(MAX_LIMIT).min(MAX_LIMIT);
    let blocks = state
        .db
        .get_blocks_range(query.from, limit)
        .unwrap_or_default();
    let payouts = state
        .db
        .get_lottery_payouts_range(query.from, limit)
        .unwrap_or_default();

    let views = blocks
        .into_iter()
        .map(|block| {
            let lottery_payouts = payouts
                .get(&block.index)
                .map(|ps| {
                    ps.iter()
                        .map(|(r, a, _tier)| LotteryPayoutView {
                            receiver: r.clone(),
                            amount: *a,
                        })
                        .collect()
                })
                .unwrap_or_default();

            BlockView {
                index: block.index,
                previous_hash: block.previous_hash,
                timestamp: block.timestamp,
                nonce: block.nonce,
                tx_root: block.tx_root,
                transactions: block.transactions,
                hash: block.hash,
                lottery_payouts,
            }
        })
        .collect();

    Json(views)
}

// New: expose chain head for miners
pub async fn chain_head_handler(State(state): State<AppState>) -> Json<ChainHeadResponse> {
    let (height, latest_hash_hex, difficulty, avg_block_time_secs, chain_work_hex, pot) = {
        let chain = state.chain.read().await;
        let h = chain.get_height();
        let hash_hex = hex::encode(chain.get_latest_hash());
        let diff = chain.get_difficulty();
        let avg = chain.get_avg_block_time_secs();
        let work = format!("{:x}", chain.get_chain_work());
        let pot = chain.get_pot();
        (h, hash_hex, diff, avg, work, pot)
    };
    let mempool_size = state.mempool.read().await.len();
    Json(ChainHeadResponse {
        height,
        latest_hash_hex,
        difficulty,
        mempool_size,
        avg_block_time_secs,
        chain_work_hex,
        pot,
    })
}

pub async fn node_info_handler(State(state): State<AppState>) -> Json<NodeInfoResponse> {
    Json(NodeInfoResponse {
        version: env!("CARGO_PKG_VERSION"),
        history_start: state.history_start,
        node_url: std::env::var("NODE_URL").ok(),
    })
}

/// Accept an externally mined block and try to append it.
pub async fn submit_block_handler(
    State(state): State<AppState>,
    Json(block): Json<Block>,
) -> Result<Json<Block>, (axum::http::StatusCode, String)> {
    let index = block.index;
    match apply_incoming_block(&state, block.clone()).await {
        Ok(true) => Ok(Json(block)),
        Ok(false) => {
            // Valid proof-of-work but not on the main chain (stale / duplicate).
            Err((axum::http::StatusCode::CONFLICT, "orphaned".to_string()))
        }
        Err(msg) => {
            warn!(target: "api", "Block rejected at index={}: {}", index, msg);
            Err((axum::http::StatusCode::BAD_REQUEST, msg.to_string()))
        }
    }
}

/// Relay endpoint: peers forward transactions here.
pub async fn relay_transaction_handler(
    State(state): State<AppState>,
    Json(tx): Json<Transaction>,
) -> Result<axum::http::StatusCode, (axum::http::StatusCode, String)> {
    if relay_tx_inner(&state, tx).await {
        Ok(axum::http::StatusCode::OK)
    } else {
        Err((
            axum::http::StatusCode::BAD_REQUEST,
            "transaction not valid".into(),
        ))
    }
}

// ─── SSE ─────────────────────────────────────────────────────────────────────

/// Server-Sent Events stream.  Each event is a JSON-encoded `NodeEvent`:
///   data: {"type":"block","data":{...}}
///   data: {"type":"transaction","data":{...}}
///
/// Peers and miners subscribe here instead of being pushed to via HTTP POST.
pub async fn events_handler(
    State(state): State<AppState>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, axum::http::StatusCode> {
    // Reject if at the subscriber limit.
    let prev = state.sse_subscribers.fetch_add(1, Ordering::Relaxed);
    if prev >= MAX_SSE_SUBSCRIBERS {
        state.sse_subscribers.fetch_sub(1, Ordering::Relaxed);
        warn!(target: "api", "SSE subscriber limit reached ({MAX_SSE_SUBSCRIBERS}), rejecting connection");
        return Err(axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    let guard = SseGuard(Arc::clone(&state.sse_subscribers));
    let rx = state.gossip.subscribe();
    let mut shutdown_rx = state.shutdown_rx.clone();
    let stream = BroadcastStream::new(rx)
        .take_until(async move {
            shutdown_rx.wait_for(|v| *v).await.ok();
        })
        .map(move |result| {
            // `guard` is captured here and lives as long as the stream does.
            // When the stream is dropped (client disconnects or server shuts
            // down), the guard's Drop impl decrements the subscriber counter.
            let _guard = &guard;
            let event: NodeEvent = match result {
                Ok(e) => e,
                // Lagged: send a no-data comment so the client doesn't time out.
                Err(_) => return Ok(Event::default().comment("lag")),
            };
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok(Event::default().data(data))
        });
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("ping"),
    ))
}

/// Spawn a long-running task that subscribes to `peer_url/events` and applies
/// any blocks or transactions received over the stream.  Automatically
/// reconnects on disconnect.
pub fn spawn_peer_subscription(state: AppState, peer_url: String) {
    tokio::spawn(async move {
        // Guard against duplicate subscription tasks for the same peer.
        // This happens when announce_self causes a peer to POST /peers with a URL
        // we already subscribed to at startup — without this check we'd spawn a
        // second task and process every event from that peer twice.
        if !state.gossip.try_start_subscription(&peer_url).await {
            return; // another task is already handling this peer
        }

        // No overall timeout — SSE connections are long-lived by design and the
        // keep-alive pings (every 30 s) will surface a dead connection naturally.
        // A connect timeout still guards against hung TCP handshakes.
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("SSE: failed to build client for {}: {}", peer_url, e);
                state.gossip.end_subscription(&peer_url).await;
                return;
            }
        };

        // Exponential backoff: 5 → 10 → 20 → 40 → 60s (capped).
        // Reset to 5s on a clean stream end (normal node restart / graceful close).
        // Keeps doubling on repeated errors so dead peers don't generate log spam
        // or waste connection attempts at full rate.
        let mut delay_secs: u64 = 5;
        loop {
            // Sync before every connection attempt — catches up on blocks missed
            // while the stream was down, and also covers the case where this peer
            // was unreachable during the initial startup sync in main.
            {
                let mut peer_list = vec![peer_url.clone()];
                crate::sync_from_peers(&state.db, Arc::clone(&state.chain), &mut peer_list).await;
            }

            info!("SSE: subscribing to {}/events", peer_url);
            match subscribe_to_peer(&client, &state, &peer_url).await {
                Ok(()) => {
                    info!("SSE: stream from {} ended cleanly, reconnecting", peer_url);
                    delay_secs = 5;
                }
                Err(e) => {
                    info!(
                        "SSE: stream from {} error: {} — reconnecting in {}s",
                        peer_url, e, delay_secs
                    );
                    delay_secs = (delay_secs * 2).min(60);
                }
            }
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        }
        // Note: end_subscription is not called here because the loop runs forever
        // until the task is cancelled (on shutdown). If the task ever exits the
        // loop, the subscription slot will be released when the task drops.
    });
}

/// Maximum number of bytes buffered between newlines in an SSE stream from a
/// peer. A legitimate event is at most a JSON block (~80 KB for 200 txs).
/// If a peer streams bytes without ever sending a newline the buffer would
/// otherwise grow without bound, exhausting memory.
const SSE_LINE_BUF_LIMIT: usize = 256 * 1024; // 256 KB

async fn subscribe_to_peer(
    client: &reqwest::Client,
    state: &AppState,
    peer_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .get(format!("{}/events", peer_url))
        .header("Accept", "text/event-stream")
        .send()
        .await?;

    let mut byte_stream = resp.bytes_stream();
    let mut line_buf = String::new();

    while let Some(chunk) = byte_stream.next().await {
        let bytes = chunk?;
        line_buf.push_str(&String::from_utf8_lossy(&bytes));

        // Disconnect if a peer sends an oversized line with no newline —
        // no legitimate SSE event exceeds SSE_LINE_BUF_LIMIT bytes.
        if line_buf.len() > SSE_LINE_BUF_LIMIT {
            return Err(format!(
                "SSE line buffer overflow from {} ({} bytes without newline)",
                peer_url,
                line_buf.len()
            )
            .into());
        }

        // Drain complete SSE lines from the buffer.
        loop {
            match line_buf.find('\n') {
                None => break,
                Some(pos) => {
                    let line = line_buf[..pos].trim_end_matches('\r').to_string();
                    line_buf.drain(..=pos);

                    if let Some(json) = line.strip_prefix("data: ") {
                        if let Ok(event) = serde_json::from_str::<NodeEvent>(json) {
                            handle_peer_event(state, event).await;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_peer_event(state: &AppState, event: NodeEvent) {
    match event {
        NodeEvent::Block(block) => {
            // apply_incoming_block handles dedup via chain.apply_block returning
            // Orphaned/Rejected for blocks we have already seen.
            if let Err(e) = apply_incoming_block(state, block).await {
                // Most rejections here are normal (already have the block, stale
                // fork candidate, etc.) — log at debug level to avoid spam.
                info!(target: "api", "SSE block from peer rejected: {}", e);
            }
        }
        NodeEvent::Transaction(tx) => {
            relay_tx_inner(state, tx).await;
        }
    }
}

// ─── Peers ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct PeersResponse {
    pub peers: Vec<String>,
}

#[derive(Deserialize)]
pub struct AddPeerRequest {
    pub url: String,
}

pub async fn get_peers_handler(State(state): State<AppState>) -> Json<PeersResponse> {
    let peers = state.gossip.peer_urls().await;
    Json(PeersResponse { peers })
}

/// Returns `true` if `raw_url` is a safe peer URL to connect to.
///
/// Rejects:
/// - non-http/https schemes (file://, ftp://, etc.)
/// - loopback addresses (127.x.x.x, ::1, "localhost")
/// - private RFC-1918 ranges (10.x, 172.16-31.x, 192.168.x)
/// - link-local / AWS metadata range (169.254.x.x)
/// - IPv6 private (fc00::/7) and link-local (fe80::/10)
pub fn is_safe_peer_url(raw_url: &str) -> bool {
    let parsed = match url::Url::parse(raw_url) {
        Ok(u) => u,
        Err(_) => return false,
    };

    // Only allow plain HTTP and HTTPS.
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }

    let host = match parsed.host() {
        Some(h) => h,
        None => return false,
    };

    match host {
        url::Host::Domain(name) => {
            // Block obvious loopback hostnames. We can't resolve arbitrary
            // domain names at validation time, so we block known aliases only;
            // DNS-rebinding attacks require a separate mitigation (e.g. a
            // proper egress firewall on the host machine).
            let lower = name.to_lowercase();
            if lower == "localhost" || lower.ends_with(".localhost") {
                return false;
            }
        }
        url::Host::Ipv4(addr) => {
            let octets = addr.octets();
            let is_private = octets[0] == 127                                                    // 127.0.0.0/8 loopback
                || octets[0] == 10                                                  // 10.0.0.0/8
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))            // 172.16.0.0/12
                || (octets[0] == 192 && octets[1] == 168)                          // 192.168.0.0/16
                || (octets[0] == 169 && octets[1] == 254); // 169.254.0.0/16 (link-local / AWS metadata)
            if is_private {
                return false;
            }
        }
        url::Host::Ipv6(addr) => {
            let ip = IpAddr::V6(addr);
            if ip.is_loopback() {
                return false;
            }
            // fc00::/7 (Unique Local) and fe80::/10 (Link-Local)
            let segments = addr.segments();
            let is_private = (segments[0] & 0xfe00) == 0xfc00    // fc00::/7
                || (segments[0] & 0xffc0) == 0xfe80; // fe80::/10
            if is_private {
                return false;
            }
        }
    }

    true
}

pub async fn add_peer_handler(
    State(state): State<AppState>,
    Json(req): Json<AddPeerRequest>,
) -> axum::http::StatusCode {
    let url = req.url.trim().to_string();
    if !is_safe_peer_url(&url) {
        warn!(target: "api", "rejected unsafe peer URL: {}", url);
        return axum::http::StatusCode::BAD_REQUEST;
    }
    if std::env::var("NODE_URL").ok().as_deref() == Some(url.as_str()) {
        return axum::http::StatusCode::BAD_REQUEST; // refuse to add ourselves as a peer
    }
    if !state.gossip.add_peer(url.clone()).await {
        warn!(target: "api", "peer list full, rejecting {}", url);
        return axum::http::StatusCode::TOO_MANY_REQUESTS;
    }
    if let Err(e) = state.db.save_peer(&url) {
        warn!(target: "api", "failed to persist peer {}: {}", url, e);
    }
    // Subscribe to the new peer's SSE event stream so we receive their blocks
    // and transactions immediately rather than waiting for them to push to us.
    spawn_peer_subscription(state, url);
    axum::http::StatusCode::OK
}

// ─── Metrics ──────────────────────────────────────────────────────────────────

/// `GET /metrics` — Prometheus text exposition format.
///
/// Gauges (chain state, economics) are refreshed on every scrape by reading
/// the current chain state.  Counters (fees, lottery wins) are maintained
/// incrementally as blocks are applied and are simply read here.
async fn metrics_handler(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    use std::time::{SystemTime, UNIX_EPOCH};

    let m = &state.metrics;

    // Update gauges from current chain state
    {
        let chain = state.chain.read().await;
        let height = chain.get_height();
        let pot = chain.get_pot();
        // Genesis supply is 100 M (1 M circulating + 99 M pot); coinbase adds 1/block.
        let total = 100_000_000u64.saturating_add(height);

        m.chain_height.set(height as f64);
        m.chain_difficulty.set(chain.get_difficulty());
        if let Some(avg) = chain.get_avg_block_time_secs() {
            m.avg_block_time_secs.set(avg);
        }

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        m.secs_since_last_block
            .set(now_secs.saturating_sub(chain.get_last_block_timestamp()) as f64);

        m.pot_coins.set(pot as f64);
        m.total_supply.set(total as f64);
        m.circulating_coins.set(total.saturating_sub(pot) as f64);
    }

    m.mempool_size.set(state.mempool.read().await.len() as f64);
    m.peer_count
        .set(state.gossip.peer_urls().await.len() as f64);

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        m.encode_to_string(),
    )
}

// ─── Snapshot endpoints ───────────────────────────────────────────────────────

/// GET /snapshots
///
/// Lists checkpoint heights this node can serve as snapshots. Only heights
/// that appear in TRUSTED_CHECKPOINTS and are present in the local DB are
/// advertised — so the list is always a subset of the hardcoded trust anchors.
pub async fn get_snapshots_handler(State(state): State<AppState>) -> Json<Vec<SnapshotInfo>> {
    let mut available = Vec::new();
    for &(height, trusted_hash) in TRUSTED_CHECKPOINTS {
        if let Ok(Some(data)) = state.db.load_checkpoint(height) {
            if let Ok(cp) = bincode::deserialize::<CheckpointState>(&data) {
                let local_hash = hex::encode(&cp.block_hash);
                if local_hash == trusted_hash.trim_start_matches("0x") {
                    available.push(SnapshotInfo {
                        height,
                        block_hash_hex: local_hash,
                    });
                }
            }
        }
    }
    Json(available)
}

/// GET /snapshot/{height}
///
/// Returns the full snapshot payload for a trusted checkpoint height.
/// Returns 404 if the height is not in TRUSTED_CHECKPOINTS or is not yet
/// available in this node's local DB.
pub async fn get_snapshot_handler(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<Json<SnapshotPayload>, axum::http::StatusCode> {
    // Only serve heights explicitly listed in the trust anchors.
    let trusted_hash = TRUSTED_CHECKPOINTS
        .iter()
        .find(|(h, _)| *h == height)
        .map(|(_, hash)| *hash)
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;

    let data = state
        .db
        .load_checkpoint(height)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;

    let cp = bincode::deserialize::<CheckpointState>(&data)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    // Sanity-check: local data must still match the trusted hash.
    let local_hash = hex::encode(&cp.block_hash);
    if local_hash != trusted_hash.trim_start_matches("0x") {
        warn!(target: "api", "Checkpoint at {} has unexpected hash — DB may be inconsistent", height);
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok(Json(SnapshotPayload {
        height,
        block_hash_hex: local_hash,
        balances: cp.balances,
        pot: cp.pot,
        chain_work_hex: format!("{:032x}", cp.chain_work),
        current_difficulty: cp.current_difficulty,
        asert_anchor: cp.asert_anchor,
        tickets: cp.tickets,
    }))
}

// ─── Router ───────────────────────────────────────────────────────────────────

/// Build the application router.
///
/// GET /chain/block-hash/{height}
///
/// Returns the hex-encoded block hash at the given height. Intended for
/// operators who need to add a new entry to TRUSTED_CHECKPOINTS.
pub async fn block_hash_handler(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let blocks = state
        .db
        .get_blocks_range(height, 1)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    match blocks.into_iter().next() {
        Some(b) => Ok(Json(
            serde_json::json!({ "height": height, "block_hash_hex": hex::encode(&b.hash) }),
        )),
        None => Err(axum::http::StatusCode::NOT_FOUND),
    }
}

/// Rate limiting is applied only to POST /transactions, POST /transactions/relay,
/// and POST /peers.  POST /blocks is intentionally exempt: valid blocks require
/// proof-of-work (natural rate limiter), invalid blocks are rejected cheaply,
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/transactions", post(submit_transaction_handler))
        .route("/transactions/relay", post(relay_transaction_handler))
        .route("/peers", post(add_peer_handler))
        .route("/blocks", post(submit_block_handler))
        .route("/balance/{address}", get(get_balance_handler))
        .route(
            "/address/{address}/transactions",
            get(get_address_transactions_handler),
        )
        .route("/mempool", get(list_mempool_handler))
        .route("/mempool/fees", get(mempool_fees_handler))
        .route(
            "/lottery/recent-payouts",
            get(get_recent_lottery_payouts_handler),
        )
        .route("/blocks", get(get_blocks_handler))
        .route("/chain/head", get(chain_head_handler))
        .route("/chain/block-hash/{height}", get(block_hash_handler))
        .route("/node/info", get(node_info_handler))
        .route("/snapshots", get(get_snapshots_handler))
        .route("/snapshot/{height}", get(get_snapshot_handler))
        .route("/peers", get(get_peers_handler))
        .route("/events", get(events_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
        .layer(DefaultBodyLimit::max(256 * 1024)) // 256 KB — covers max block (~54 KB) with headroom
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockchain::Blockchain;
    use crate::db::Db;
    use crate::gossip::Gossip;
    use crate::mempool::Mempool;
    use crate::metrics::Metrics;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use lootcoin_core::{block::Block, transaction::Transaction, wallet::Wallet};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::{watch, RwLock};
    use tower::ServiceExt;

    // ── state helpers ─────────────────────────────────────────────────────────

    fn test_wallet() -> Wallet {
        // Fixed key so tests are deterministic
        Wallet::from_secret_key_bytes([1u8; 32])
    }

    fn make_state(wallet: &Wallet) -> AppState {
        let db = Arc::new(Db::new_in_memory().unwrap());
        // Genesis credits the wallet address so submit-tx tests have a balance
        let genesis_txs = vec![Transaction {
            sender: String::new(),
            receiver: wallet.get_address(),
            amount: 10_000,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![],
        }];
        let genesis = Block {
            index: 0,
            previous_hash: vec![0u8; 32],
            timestamp: 1_700_000_000,
            nonce: 0,
            tx_root: Block::compute_tx_root(&genesis_txs),
            transactions: genesis_txs,
            hash: vec![],
        };
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        AppState {
            db,
            chain: Arc::new(RwLock::new(Blockchain::new_for_test(genesis))),
            mempool: Arc::new(RwLock::new(Mempool::new(None))),
            gossip: Arc::new(Gossip::new(vec![])),
            shutdown_rx,
            sse_subscribers: Arc::new(AtomicUsize::new(0)),
            metrics: Arc::new(Metrics::new()),
            history_start: 0,
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Build the next valid coinbase-only block on the current chain tip.
    /// With difficulty=0 any hash passes, so no real PoW is needed.
    async fn next_valid_block(state: &AppState) -> Block {
        let chain = state.chain.read().await;
        let txs = vec![Transaction {
            sender: String::new(),
            receiver: "miner".to_string(),
            amount: 1,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![],
        }];
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp: now_secs(),
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        b
    }

    async fn get_req(state: AppState, uri: &str) -> axum::http::Response<Body> {
        router(state)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn post_json(
        state: AppState,
        uri: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<Body> {
        router(state)
            .oneshot(
                Request::post(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn json_body(resp: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── GET /balance/{address} ────────────────────────────────────────────────

    #[tokio::test]
    async fn balance_unknown_address_returns_zero() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let resp = get_req(state, "/balance/nobody").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["balance"], 0);
        assert_eq!(body["spendable_balance"], 0);
    }

    #[tokio::test]
    async fn balance_funded_address_returns_genesis_amount() {
        let wallet = test_wallet();
        let addr = wallet.get_address();
        let state = make_state(&wallet);
        let resp = get_req(state, &format!("/balance/{}", addr)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["balance"], 10_000);
        assert_eq!(body["spendable_balance"], 10_000);
        assert_eq!(body["address"], addr);
    }

    #[tokio::test]
    async fn balance_spendable_decreases_when_tx_in_mempool() {
        let wallet = test_wallet();
        let addr = wallet.get_address();
        let state = make_state(&wallet);
        // Insert a pending tx for the wallet address directly into mempool
        let pending = Transaction {
            sender: addr.clone(),
            receiver: "bob".to_string(),
            amount: 500,
            fee: 2,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![0xAA],
        };
        state.mempool.write().await.add_transaction(pending, 1);
        let resp = get_req(state, &format!("/balance/{}", addr)).await;
        let body = json_body(resp).await;
        assert_eq!(body["balance"], 10_000);
        assert_eq!(body["spendable_balance"], 10_000 - 502); // 500 + 2
    }

    // ── GET /chain/head ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn chain_head_returns_correct_shape() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let resp = get_req(state, "/chain/head").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["height"], 1); // genesis counts as block 0, height=1
        assert_eq!(body["mempool_size"], 0);
        assert!(body["difficulty"].is_number());
        assert!(body["chain_work_hex"].is_string());
        assert!(body["pot"].is_number());
    }

    // ── GET /mempool ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn mempool_empty_returns_empty_array() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/mempool").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await, serde_json::json!([]));
    }

    #[tokio::test]
    async fn mempool_returns_pending_transactions() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let tx = Transaction {
            sender: "alice".to_string(),
            receiver: "bob".to_string(),
            amount: 100,
            fee: 2,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![0x01],
        };
        state.mempool.write().await.add_transaction(tx, 1);
        let resp = get_req(state, "/mempool").await;
        let body = json_body(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["transaction"]["amount"], 100);
        assert_eq!(body[0]["added_height"], 1);
    }

    // ── GET /mempool/fees ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn mempool_fees_empty_pool() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/mempool/fees").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["count"], 0);
        assert!(body["min"].is_null());
        assert!(body["max"].is_null());
    }

    #[tokio::test]
    async fn mempool_fees_non_empty_pool() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        for (i, fee) in [5u64, 10, 20].iter().enumerate() {
            state.mempool.write().await.add_transaction(
                Transaction {
                    sender: "a".to_string(),
                    receiver: "b".to_string(),
                    amount: 1,
                    fee: *fee,
                    nonce: 0,
                    public_key: [0u8; 32],
                    signature: vec![i as u8],
                },
                0,
            );
        }
        let body = json_body(get_req(state, "/mempool/fees").await).await;
        assert_eq!(body["count"], 3);
        assert_eq!(body["min"], 5);
        assert_eq!(body["max"], 20);
    }

    // ── GET /blocks ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_blocks_empty_db_returns_empty_array() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/blocks?from=0&limit=10").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await, serde_json::json!([]));
    }

    #[tokio::test]
    async fn get_blocks_returns_stored_block() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let block = Block {
            index: 0,
            previous_hash: vec![],
            timestamp: 1_700_000_000,
            nonce: 0,
            tx_root: vec![],
            transactions: vec![],
            hash: vec![1, 2, 3],
        };
        state.db.save_block_indexed(&block).unwrap();
        let body = json_body(get_req(state, "/blocks?from=0&limit=1").await).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["index"], 0);
    }

    // ── GET /address/{address}/transactions ───────────────────────────────────

    #[tokio::test]
    async fn address_transactions_empty_for_unknown() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/address/nobody/transactions").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await, serde_json::json!([]));
    }

    #[tokio::test]
    async fn address_transactions_returns_results_from_db() {
        let wallet = test_wallet();
        let addr = wallet.get_address();
        let state = make_state(&wallet);
        let g = Block {
            index: 0,
            previous_hash: vec![0u8; 32],
            timestamp: 1_700_000_000,
            nonce: 0,
            tx_root: vec![],
            transactions: vec![Transaction {
                sender: String::new(),
                receiver: addr.clone(),
                amount: 50,
                fee: 0,
                nonce: 0,
                public_key: [0u8; 32],
                signature: vec![],
            }],
            hash: vec![],
        };
        state.db.save_applied_block(&g, &[], &[]).unwrap();
        let body = json_body(get_req(state, &format!("/address/{}/transactions", addr)).await).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["receiver"], addr);
    }

    #[tokio::test]
    async fn address_transactions_limit_param_respected() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        // Save 3 blocks each containing a tx to "alice"
        for i in 0u64..3 {
            let b = Block {
                index: i,
                previous_hash: vec![],
                timestamp: 1_700_000_000 + i,
                nonce: 0,
                tx_root: vec![],
                transactions: vec![Transaction {
                    sender: String::new(),
                    receiver: "alice".to_string(),
                    amount: 1,
                    fee: 0,
                    nonce: 0,
                    public_key: [0u8; 32],
                    signature: vec![],
                }],
                hash: vec![i as u8],
            };
            state.db.save_applied_block(&b, &[], &[]).unwrap();
        }
        let body = json_body(
            get_req(state, "/address/alice/transactions?limit=2").await,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    // ── GET /chain/block-hash/{height} ────────────────────────────────────────

    #[tokio::test]
    async fn block_hash_missing_height_returns_404() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/chain/block-hash/999").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn block_hash_existing_height_returns_hex() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let block = Block {
            index: 7,
            previous_hash: vec![],
            timestamp: 0,
            nonce: 0,
            tx_root: vec![],
            transactions: vec![],
            hash: vec![0xab, 0xcd],
        };
        state.db.save_block_indexed(&block).unwrap();
        let body = json_body(get_req(state, "/chain/block-hash/7").await).await;
        assert_eq!(body["height"], 7);
        assert_eq!(body["block_hash_hex"], "abcd");
    }

    // ── GET /node/info ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn node_info_returns_version_and_history_start() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/node/info").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert!(body["version"].is_string());
        assert_eq!(body["history_start"], 0);
    }

    // ── GET /snapshots ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn snapshots_empty_when_no_trusted_checkpoints() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/snapshots").await;
        assert_eq!(resp.status(), StatusCode::OK);
        // TRUSTED_CHECKPOINTS is empty in this codebase → no entries served
        assert_eq!(json_body(resp).await, serde_json::json!([]));
    }

    // ── GET /snapshot/{height} ────────────────────────────────────────────────

    #[tokio::test]
    async fn snapshot_unknown_height_returns_404() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/snapshot/1000").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── GET /peers ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_peers_empty_initially() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/peers").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["peers"], serde_json::json!([]));
    }

    // ── GET /lottery/recent-payouts ───────────────────────────────────────────

    #[tokio::test]
    async fn recent_payouts_empty_initially() {
        let wallet = test_wallet();
        let resp = get_req(make_state(&wallet), "/lottery/recent-payouts").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await, serde_json::json!([]));
    }

    #[tokio::test]
    async fn recent_payouts_tier_filter_applied() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        // Store a block with mixed-tier payouts
        let b = Block {
            index: 1,
            previous_hash: vec![],
            timestamp: 1_700_000_010,
            nonce: 0,
            tx_root: vec![],
            transactions: vec![],
            hash: vec![1],
        };
        state
            .db
            .save_applied_block(
                &b,
                &[],
                &[
                    ("alice".to_string(), 100u64, "small".to_string()),
                    ("bob".to_string(), 500u64, "jackpot".to_string()),
                ],
            )
            .unwrap();
        let body = json_body(
            get_req(state, "/lottery/recent-payouts?tier=jackpot").await,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["tier"], "jackpot");
    }

    // ── POST /peers ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn add_peer_loopback_address_returns_400() {
        let wallet = test_wallet();
        let resp = post_json(
            make_state(&wallet),
            "/peers",
            serde_json::json!({"url": "http://127.0.0.1:3000"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn add_peer_private_range_returns_400() {
        let wallet = test_wallet();
        let resp = post_json(
            make_state(&wallet),
            "/peers",
            serde_json::json!({"url": "http://192.168.1.1:3000"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn add_peer_valid_public_url_returns_200() {
        let wallet = test_wallet();
        let resp = post_json(
            make_state(&wallet),
            "/peers",
            serde_json::json!({"url": "http://8.8.8.8:3000"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── POST /transactions ────────────────────────────────────────────────────

    #[tokio::test]
    async fn submit_tx_sender_equals_receiver_returns_400() {
        let wallet = test_wallet();
        let addr = wallet.get_address();
        let resp = post_json(
            make_state(&wallet),
            "/transactions",
            serde_json::json!({
                "sender": addr, "receiver": addr,
                "amount": 100, "fee": 2, "nonce": 1,
                "public_key_hex": "00".repeat(32),
                "signature_hex": "00".repeat(64),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_tx_invalid_public_key_hex_returns_400() {
        let wallet = test_wallet();
        let resp = post_json(
            make_state(&wallet),
            "/transactions",
            serde_json::json!({
                "sender": "alice", "receiver": "bob",
                "amount": 100, "fee": 2, "nonce": 1,
                "public_key_hex": "not-hex",
                "signature_hex": "00".repeat(64),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_tx_wrong_length_public_key_returns_400() {
        let wallet = test_wallet();
        let resp = post_json(
            make_state(&wallet),
            "/transactions",
            serde_json::json!({
                "sender": "alice", "receiver": "bob",
                "amount": 100, "fee": 2, "nonce": 1,
                "public_key_hex": "aabb", // 2 bytes, not 32
                "signature_hex": "00".repeat(64),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_tx_invalid_signature_hex_returns_400() {
        let wallet = test_wallet();
        let resp = post_json(
            make_state(&wallet),
            "/transactions",
            serde_json::json!({
                "sender": "alice", "receiver": "bob",
                "amount": 100, "fee": 2, "nonce": 1,
                "public_key_hex": "00".repeat(32),
                "signature_hex": "not-hex",
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_tx_wrong_signature_returns_400() {
        let wallet = test_wallet();
        // Correctly formatted but cryptographically invalid signature
        let resp = post_json(
            make_state(&wallet),
            "/transactions",
            serde_json::json!({
                "sender": "alice", "receiver": "bob",
                "amount": 100, "fee": 2, "nonce": 1,
                "public_key_hex": "00".repeat(32),
                "signature_hex": "00".repeat(64),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_tx_valid_signed_transaction_accepted() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let tx = Transaction::new_signed(&wallet, "bob".to_string(), 100, 2);
        let resp = post_json(
            state,
            "/transactions",
            serde_json::json!({
                "sender": tx.sender,
                "receiver": tx.receiver,
                "amount": tx.amount,
                "fee": tx.fee,
                "nonce": tx.nonce,
                "public_key_hex": hex::encode(tx.public_key),
                "signature_hex": hex::encode(&tx.signature),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["receiver"], "bob");
        assert_eq!(body["amount"], 100);
    }

    #[tokio::test]
    async fn submit_tx_insufficient_balance_returns_400() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        // Wallet only has 10_000; try to spend far more
        let tx = Transaction::new_signed(&wallet, "bob".to_string(), 999_999, 2);
        let resp = post_json(
            state,
            "/transactions",
            serde_json::json!({
                "sender": tx.sender,
                "receiver": tx.receiver,
                "amount": tx.amount,
                "fee": tx.fee,
                "nonce": tx.nonce,
                "public_key_hex": hex::encode(tx.public_key),
                "signature_hex": hex::encode(&tx.signature),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_tx_fee_below_minimum_returns_400() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        // Fee = 1, below MIN_TX_FEE = 2
        let tx = Transaction::new_signed(&wallet, "bob".to_string(), 10, 1);
        let resp = post_json(
            state,
            "/transactions",
            serde_json::json!({
                "sender": tx.sender,
                "receiver": tx.receiver,
                "amount": tx.amount,
                "fee": tx.fee,
                "nonce": tx.nonce,
                "public_key_hex": hex::encode(tx.public_key),
                "signature_hex": hex::encode(&tx.signature),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── POST /blocks ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn submit_block_wrong_hash_returns_400() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let chain = state.chain.read().await;
        let txs = vec![Transaction {
            sender: String::new(),
            receiver: "miner".to_string(),
            amount: 1,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![],
        }];
        let b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp: now_secs(),
            nonce: 0,
            tx_root: Block::compute_tx_root(&txs),
            transactions: txs,
            hash: vec![0xFF; 32], // deliberately wrong
        };
        drop(chain);
        let resp = post_json(state, "/blocks", serde_json::json!(b)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_block_coinbase_amount_too_high_returns_400() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let chain = state.chain.read().await;
        let txs = vec![Transaction {
            sender: String::new(),
            receiver: "miner".to_string(),
            amount: 100, // > 1, rejected by apply_incoming_block
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![],
        }];
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp: now_secs(),
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        drop(chain);
        let resp = post_json(state, "/blocks", serde_json::json!(b)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_block_no_coinbase_returns_400() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let chain = state.chain.read().await;
        // Non-empty sender on first tx — rejected as non-coinbase
        let txs = vec![Transaction {
            sender: "alice".to_string(),
            receiver: "bob".to_string(),
            amount: 1,
            fee: 2,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![],
        }];
        let tx_root = Block::compute_tx_root(&txs);
        let mut b = Block {
            index: chain.get_height(),
            previous_hash: chain.get_latest_hash(),
            timestamp: now_secs(),
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        };
        b.hash = b.calculate_hash();
        drop(chain);
        let resp = post_json(state, "/blocks", serde_json::json!(b)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_block_valid_block_accepted() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let block = next_valid_block(&state).await;
        let resp = post_json(state, "/blocks", serde_json::json!(block)).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn submit_block_advances_chain_height() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let block = next_valid_block(&state).await;
        post_json(state.clone(), "/blocks", serde_json::json!(block)).await;
        let body = json_body(get_req(state, "/chain/head").await).await;
        assert_eq!(body["height"], 2);
    }

    #[tokio::test]
    async fn submit_block_already_applied_returns_non_ok() {
        let wallet = test_wallet();
        let state = make_state(&wallet);
        let block = next_valid_block(&state).await;
        // First submission succeeds
        post_json(state.clone(), "/blocks", serde_json::json!(&block)).await;
        // Second submission of the same block is rejected (wrong index now)
        let resp = post_json(state, "/blocks", serde_json::json!(&block)).await;
        assert_ne!(resp.status(), StatusCode::OK);
    }

    // ── is_safe_peer_url ──────────────────────────────────────────────────────

    // ── is_safe_peer_url ──────────────────────────────────────────────────────

    #[test]
    fn safe_url_accepts_public_http() {
        assert!(is_safe_peer_url("http://node.example.com:3000"));
    }

    #[test]
    fn safe_url_accepts_public_https() {
        assert!(is_safe_peer_url("https://node.example.com"));
    }

    #[test]
    fn safe_url_rejects_non_http_scheme() {
        assert!(!is_safe_peer_url("file:///etc/passwd"));
        assert!(!is_safe_peer_url("ftp://files.example.com"));
        assert!(!is_safe_peer_url("ws://node.example.com"));
    }

    #[test]
    fn safe_url_rejects_unparseable_url() {
        assert!(!is_safe_peer_url("not a url"));
        assert!(!is_safe_peer_url(""));
    }

    #[test]
    fn safe_url_rejects_localhost_hostname() {
        assert!(!is_safe_peer_url("http://localhost:3000"));
        assert!(!is_safe_peer_url("http://localhost"));
        assert!(!is_safe_peer_url("http://sub.localhost/path"));
    }

    #[test]
    fn safe_url_rejects_loopback_ipv4() {
        assert!(!is_safe_peer_url("http://127.0.0.1:3000"));
        assert!(!is_safe_peer_url("http://127.1.2.3"));
    }

    #[test]
    fn safe_url_rejects_private_10_block() {
        assert!(!is_safe_peer_url("http://10.0.0.1"));
        assert!(!is_safe_peer_url("http://10.255.255.255:8080"));
    }

    #[test]
    fn safe_url_rejects_private_172_16_block() {
        assert!(!is_safe_peer_url("http://172.16.0.1"));
        assert!(!is_safe_peer_url("http://172.31.255.255"));
        // 172.15 and 172.32 are public
        assert!(is_safe_peer_url("http://172.15.0.1"));
        assert!(is_safe_peer_url("http://172.32.0.1"));
    }

    #[test]
    fn safe_url_rejects_private_192_168_block() {
        assert!(!is_safe_peer_url("http://192.168.1.1"));
        assert!(!is_safe_peer_url("http://192.168.0.0"));
    }

    #[test]
    fn safe_url_rejects_link_local_ipv4() {
        assert!(!is_safe_peer_url("http://169.254.0.1")); // AWS metadata etc.
        assert!(!is_safe_peer_url("http://169.254.169.254"));
    }

    #[test]
    fn safe_url_rejects_ipv6_loopback() {
        assert!(!is_safe_peer_url("http://[::1]:3000"));
    }

    #[test]
    fn safe_url_rejects_ipv6_unique_local() {
        assert!(!is_safe_peer_url("http://[fc00::1]"));
        assert!(!is_safe_peer_url("http://[fd12:3456:789a::1]"));
    }

    #[test]
    fn safe_url_rejects_ipv6_link_local() {
        assert!(!is_safe_peer_url("http://[fe80::1]"));
        assert!(!is_safe_peer_url("http://[fe80::1%25eth0]")); // zone ID variant
    }

    #[test]
    fn safe_url_accepts_public_ipv4() {
        assert!(is_safe_peer_url("http://8.8.8.8:3000"));
        assert!(is_safe_peer_url("http://1.2.3.4"));
    }
}

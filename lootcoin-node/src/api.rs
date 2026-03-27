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

use crate::blockchain::{BlockOutcome, Blockchain, MAX_BLOCK_TXS};
use crate::db::Db;
use crate::gossip::{Gossip, NodeEvent};
use crate::mempool::{FeeStats, Mempool};
use lootcoin_core::{
    block::{meets_difficulty, Block},
    transaction::Transaction,
};

/// Maximum number of concurrent SSE subscribers. Prevents memory/CPU exhaustion
/// from an attacker opening thousands of long-lived event stream connections.
const MAX_SSE_SUBSCRIBERS: usize = 100;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub chain: Arc<RwLock<Blockchain>>,
    pub mempool: Arc<RwLock<Mempool>>,
    pub gossip: Arc<Gossip>,
    pub shutdown_rx: watch::Receiver<bool>,
    pub sse_subscribers: Arc<AtomicUsize>,
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
    pub transactions: Vec<lootcoin_core::transaction::Transaction>,
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
            state.gossip.publish_block(&candidate).await;
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
            state.gossip.publish_block(&candidate).await;
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

// ─── Router ───────────────────────────────────────────────────────────────────

/// Build the application router.
///
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
        .route("/peers", get(get_peers_handler))
        .route("/events", get(events_handler))
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
    use super::is_safe_peer_url;

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

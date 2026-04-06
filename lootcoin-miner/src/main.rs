#[cfg(feature = "gpu")]
mod gpu;

use anyhow::Context;
use rand::Rng;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_stream::StreamExt as _;
use tracing::{info, warn};
use tracing_subscriber::{fmt, EnvFilter};

use lootcoin_core::block::{meets_difficulty, Block, MAX_BLOCK_TXS};
use lootcoin_core::lottery::GUARANTEE_AFTER;
use lootcoin_core::transaction::Transaction;

#[derive(Deserialize, Clone)]
struct HeadResp {
    height: u64,
    latest_hash_hex: String,
    difficulty: f64,
}

#[derive(Deserialize, Clone)]
struct MempoolEntry {
    transaction: Transaction,
    added_height: u64,
}

fn is_eligible(entry: &MempoolEntry, current_height: u64) -> bool {
    let fee = entry.transaction.fee;
    if fee == 0 {
        return false;
    }
    let age = current_height.saturating_sub(entry.added_height);
    // eligible_after = (GUARANTEE_AFTER / fee).saturating_sub(1)
    // Subtracting 1 means fee >= GUARANTEE_AFTER is eligible immediately (age 0),
    // so the "Immediate" preset is truly immediate.
    let eligible_after = (GUARANTEE_AFTER / fee).saturating_sub(1);
    age >= eligible_after
}

#[derive(Debug, Clone)]
struct MinerConfig {
    /// One or more node API base URLs (e.g., "http://127.0.0.1:3000")
    node_urls: Vec<String>,
    /// Your miner payout address
    miner_address: String,
}

fn load_config() -> anyhow::Result<MinerConfig> {
    let node_urls: Vec<String> = std::env::var("NODE_URLS")
        .unwrap_or_else(|_| "http://127.0.0.1:3000".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .collect();

    if node_urls.is_empty() {
        anyhow::bail!("NODE_URLS must contain at least one URL");
    }

    let miner_address =
        std::env::var("MINER_ADDRESS").context("MINER_ADDRESS environment variable must be set")?;

    if miner_address.is_empty() {
        anyhow::bail!("MINER_ADDRESS must not be empty");
    }

    Ok(MinerConfig {
        node_urls,
        miner_address,
    })
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Mine until a valid hash is found or `cancel` or `shutdown` is set.
/// Returns `Some(tries)` on success, `None` if cancelled.
fn mine(
    block: &mut Block,
    difficulty: f64,
    cancel: &AtomicBool,
    shutdown: &AtomicBool,
) -> Option<u64> {
    let mut tries = 0u64;
    let mut hash = block.calculate_hash().expect("infallible");
    while !meets_difficulty(&hash, difficulty) {
        // Check cancel/shutdown flags every 10 000 iterations to keep overhead negligible
        if tries.is_multiple_of(10_000)
            && (cancel.load(Ordering::Relaxed) || shutdown.load(Ordering::Relaxed))
        {
            return None;
        }
        block.nonce = block.nonce.wrapping_add(1);
        hash = block.calculate_hash().expect("infallible");
        tries = tries.wrapping_add(1);
    }
    block.hash = hash;
    Some(tries)
}

/// Try every node URL in order. Returns the first one that responds along with
/// the chain head and pending mempool entries. Returns `None` if all nodes fail.
async fn try_fetch_work(
    client: &reqwest::Client,
    node_urls: &[String],
) -> Option<(String, HeadResp, Vec<MempoolEntry>)> {
    for url in node_urls {
        let head = match client.get(format!("{}/chain/head", url)).send().await {
            Err(e) => {
                warn!("Node {} unreachable (head): {}", url, e);
                continue;
            }
            Ok(r) => match r.error_for_status() {
                Err(e) => {
                    warn!("Node {} returned error (head): {}", url, e);
                    continue;
                }
                Ok(r) => match r.json::<HeadResp>().await {
                    Err(e) => {
                        warn!("Node {} bad head response: {}", url, e);
                        continue;
                    }
                    Ok(h) => h,
                },
            },
        };

        let entries = match client.get(format!("{}/mempool", url)).send().await {
            Err(e) => {
                warn!("Node {} unreachable (mempool): {}", url, e);
                continue;
            }
            Ok(r) => match r.error_for_status() {
                Err(e) => {
                    warn!("Node {} returned error (mempool): {}", url, e);
                    continue;
                }
                Ok(r) => match r.json::<Vec<MempoolEntry>>().await {
                    Err(e) => {
                        warn!("Node {} bad mempool response: {}", url, e);
                        continue;
                    }
                    Ok(t) => t,
                },
            },
        };

        return Some((url.clone(), head, entries));
    }
    None
}

/// Connects to a node's SSE `/events` stream and sets `cancel` as soon as a
/// `block` event arrives, meaning the chain tip has advanced and the current
/// mining job is stale.
///
/// Returns when the cancel flag is set or the stream ends/errors.
async fn watch_sse_for_block(
    client: &reqwest::Client,
    base_url: &str,
    cancel: &AtomicBool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .get(format!("{}/events", base_url))
        .header("Accept", "text/event-stream")
        .send()
        .await?;

    let mut byte_stream = resp.bytes_stream();
    let mut line_buf = String::new();

    while let Some(chunk) = byte_stream.next().await {
        let bytes = chunk?;
        line_buf.push_str(&String::from_utf8_lossy(&bytes));

        loop {
            match line_buf.find('\n') {
                None => break,
                Some(pos) => {
                    let line = line_buf[..pos].trim_end_matches('\r').to_string();
                    line_buf.drain(..=pos);

                    if let Some(json) = line.strip_prefix("data: ") {
                        #[derive(serde::Deserialize)]
                        struct TypeOnly {
                            #[serde(rename = "type")]
                            event_type: String,
                        }
                        if let Ok(ev) = serde_json::from_str::<TypeOnly>(json) {
                            if ev.event_type == "block" {
                                info!("SSE: new block on chain — cancelling stale mining job");
                                cancel.store(true, Ordering::Relaxed);
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();

    let cfg = load_config()?;
    info!(
        "Loaded miner config: {} node(s), miner_address={}",
        cfg.node_urls.len(),
        cfg.miner_address
    );

    // Initialise the GPU miner when the binary was compiled with --features gpu
    // and the environment variable USE_GPU=1 is set.
    #[cfg(feature = "gpu")]
    let gpu_miner: Option<Arc<gpu::GpuMiner>> = if std::env::var("USE_GPU").as_deref() == Ok("1") {
        match gpu::GpuMiner::new() {
            Ok(m) => {
                info!("GPU miner ready on device 0");
                Some(Arc::new(m))
            }
            Err(e) => {
                warn!("GPU miner init failed: {e} — falling back to CPU");
                None
            }
        }
    } else {
        None
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            let ctrl_c = async {
                tokio::signal::ctrl_c()
                    .await
                    .expect("failed to install Ctrl+C handler");
            };
            #[cfg(unix)]
            let terminate = async {
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler")
                    .recv()
                    .await;
            };
            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();
            tokio::select! {
                _ = ctrl_c => {},
                _ = terminate => {},
            }
            info!("Shutdown signal received — stopping miner");
            shutdown.store(true, Ordering::Relaxed);
        });
    }

    let mut panic_streak: u32 = 0;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        // 1) Fetch work from the first reachable node
        let (base_url, head, entries) = match try_fetch_work(&client, &cfg.node_urls).await {
            Some(w) => w,
            None => {
                warn!("All nodes unreachable — retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // 2) Select transactions for the block.
        //
        //    - IDLE  (pending ≤ MAX_BLOCK_TXS): include everything — no reason
        //      to delay anyone when there is spare block space.
        //    - BUSY  (pending >  MAX_BLOCK_TXS): apply fee-tier eligibility so
        //      high-fee senders are prioritised and low-fee senders wait their
        //      turn rather than being crowded out silently.
        //
        //    In both cases transactions are sorted highest-fee-first so miners
        //    always maximise their fee income.
        let pending = entries.len();

        let mut txs: Vec<Transaction> = if pending <= MAX_BLOCK_TXS {
            // Spare capacity — take everything.
            let mut all: Vec<Transaction> = entries.iter().map(|e| e.transaction.clone()).collect();
            all.sort_by(|a, b| b.fee.cmp(&a.fee));
            all
        } else {
            // Block would be full — gate by fee-tier eligibility.
            let mut eligible: Vec<Transaction> = entries
                .iter()
                .filter(|e| is_eligible(e, head.height))
                .map(|e| e.transaction.clone())
                .collect();
            eligible.sort_by(|a, b| b.fee.cmp(&a.fee));
            eligible.truncate(MAX_BLOCK_TXS);
            eligible
        };

        if txs.is_empty() && pending > 0 {
            // Block is full but nothing has aged enough yet — wait for eligibility.
            let min_wait = entries
                .iter()
                .filter(|e| e.transaction.fee > 0)
                .map(|e| {
                    let eligible_after = (GUARANTEE_AFTER / e.transaction.fee).saturating_sub(1);
                    let age = head.height.saturating_sub(e.added_height);
                    eligible_after.saturating_sub(age)
                })
                .min()
                .unwrap_or(0);
            info!(
                "{} pending tx(s) at height {} (block full), none eligible yet \
                 (earliest in ~{} block(s)) — waiting",
                pending, head.height, min_wait
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // Empty mempool is fine — mine a coinbase-only block.

        // 3) Prepend coinbase
        let coinbase = Transaction {
            sender: String::new(),
            receiver: cfg.miner_address.clone(),
            amount: 1,
            fee: 0,
            nonce: 0,
            public_key: [0u8; 32],
            signature: vec![],
        };
        let mut block_txs = Vec::with_capacity(txs.len() + 1);
        block_txs.push(coinbase);
        block_txs.append(&mut txs);

        // 4) Build candidate block.
        //    tx_root commits to the full transaction list once so that the
        //    mining loop only hashes the small fixed-size header per attempt.
        let prev_hash = hex::decode(&head.latest_hash_hex).unwrap_or_default();
        let tx_root = Block::compute_tx_root(&block_txs).expect("infallible");
        let mut block = Block {
            index: head.height,
            previous_hash: prev_hash,
            timestamp: now_ts(),
            nonce: rand::rngs::OsRng.gen::<u64>(),
            tx_root,
            transactions: block_txs,
            hash: vec![],
        };

        // 5) Mine — runs in a blocking thread so the async runtime stays free.
        //    A separate task subscribes to the node's SSE stream and sets `cancel`
        //    when a new block arrives, making the miner abandon stale work.
        info!(
            "Mining block at height {} with {} txs (diff={}, node={})",
            block.index,
            block.transactions.len(),
            head.difficulty,
            base_url
        );

        let cancel = Arc::new(AtomicBool::new(false));

        // Head-watch task: first tries SSE (instant notification), falls back
        // to polling /chain/head every 2 s if the SSE connection fails.
        let watch_cancel = Arc::clone(&cancel);
        let watch_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120)) // long timeout for SSE keep-alive
            .build()
            .unwrap_or_else(|_| client.clone());
        let watch_url = base_url.clone();
        let known_hash = head.latest_hash_hex.clone();
        let poll_handle = tokio::spawn(async move {
            // Try SSE first — sets cancel on block event.
            match watch_sse_for_block(&watch_client, &watch_url, &watch_cancel).await {
                Ok(()) => return, // cancel was set or stream ended cleanly
                Err(e) => warn!("SSE watch failed: {} — falling back to 2s polling", e),
            }
            // Fallback: poll /chain/head every 2 s.
            let poll_client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Ok(r) = poll_client
                    .get(format!("{}/chain/head", watch_url))
                    .send()
                    .await
                {
                    if let Ok(h) = r.json::<HeadResp>().await {
                        if h.latest_hash_hex != known_hash {
                            info!("Chain head advanced — cancelling stale mining job");
                            watch_cancel.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                } // transient error — keep polling
            }
        });

        let mine_cancel = Arc::clone(&cancel);
        let mine_shutdown = Arc::clone(&shutdown);
        let difficulty = head.difficulty;
        let start = std::time::Instant::now();

        #[cfg(feature = "gpu")]
        let mine_result = if let Some(ref miner) = gpu_miner {
            let miner = Arc::clone(miner);
            tokio::task::spawn_blocking(move || {
                let tmpl = gpu::make_header_template(&block);
                match miner.mine(&tmpl, block.nonce, difficulty, &mine_cancel, &mine_shutdown) {
                    Ok(Some((winning_nonce, tries))) => {
                        block.nonce = winning_nonce;
                        block.hash = block.calculate_hash().expect("infallible");
                        (block, Some(tries))
                    }
                    Ok(None) => (block, None),
                    Err(e) => {
                        warn!("GPU error: {e} — discarding result");
                        (block, None)
                    }
                }
            })
            .await
        } else {
            tokio::task::spawn_blocking(move || {
                let result = mine(&mut block, difficulty, &mine_cancel, &mine_shutdown);
                (block, result)
            })
            .await
        };
        #[cfg(not(feature = "gpu"))]
        let mine_result = tokio::task::spawn_blocking(move || {
            let result = mine(&mut block, difficulty, &mine_cancel, &mine_shutdown);
            (block, result)
        })
        .await;

        poll_handle.abort();

        let (block, maybe_tries) = match mine_result {
            Ok(pair) => {
                panic_streak = 0;
                pair
            }
            Err(_) => {
                panic_streak += 1;
                let delay = Duration::from_secs((1u64 << panic_streak.min(5)).min(30));
                warn!(
                    "Mining task panicked (streak {}) — retrying in {:?}",
                    panic_streak, delay
                );
                tokio::time::sleep(delay).await;
                continue;
            }
        };

        let tries = match maybe_tries {
            Some(t) => t,
            None => {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                info!("Mining cancelled — fetching new work");
                continue;
            }
        };

        let elapsed = start.elapsed();
        let secs = elapsed.as_secs_f64().max(1e-9);
        info!(
            "Found nonce after {} tries in {:.3}s ({:.2} H/s); hash={}",
            tries,
            secs,
            (tries as f64) / secs,
            hex::encode(&block.hash)
        );

        // 6) Submit block — failure is non-fatal
        match client
            .post(format!("{}/blocks", base_url))
            .json(&block)
            .send()
            .await
        {
            Err(e) => {
                warn!("Failed to submit block to {}: {}", base_url, e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    info!("Block accepted at height {}", block.index);
                } else if status == reqwest::StatusCode::CONFLICT {
                    info!(
                        "Block orphaned at height {} (stale — another miner was faster)",
                        block.index
                    );
                } else {
                    let text = resp.text().await.unwrap_or_default();
                    warn!("Block rejected by {} ({}): {}", base_url, status, text);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    info!("Miner stopped cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lootcoin_core::block::Block;
    use lootcoin_core::lottery::GUARANTEE_AFTER;
    use lootcoin_core::transaction::Transaction;
    use std::sync::atomic::AtomicBool;

    fn make_entry(fee: u64, added_height: u64) -> MempoolEntry {
        MempoolEntry {
            transaction: Transaction {
                sender: "sender".to_string(),
                receiver: "receiver".to_string(),
                amount: 100,
                fee,
                nonce: 0,
                public_key: [0u8; 32],
                signature: vec![],
            },
            added_height,
        }
    }

    fn empty_block() -> Block {
        let txs: Vec<Transaction> = vec![];
        let tx_root = Block::compute_tx_root(&txs).expect("infallible");
        Block {
            index: 1,
            previous_hash: vec![0u8; 32],
            timestamp: 1_000_000,
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        }
    }

    // ── is_eligible ───────────────────────────────────────────────────────────

    #[test]
    fn is_eligible_zero_fee_always_false() {
        assert!(!is_eligible(&make_entry(0, 0), 0));
        assert!(!is_eligible(&make_entry(0, 0), 1000));
    }

    #[test]
    fn is_eligible_fee_at_guarantee_after_is_immediate() {
        // fee == GUARANTEE_AFTER: eligible_after = (120/120) - 1 = 0
        let entry = make_entry(GUARANTEE_AFTER, 50);
        assert!(is_eligible(&entry, 50)); // age = 0
    }

    #[test]
    fn is_eligible_fee_above_guarantee_after_is_immediate() {
        // fee > GUARANTEE_AFTER: eligible_after = 0, immediate
        let entry = make_entry(GUARANTEE_AFTER + 1, 50);
        assert!(is_eligible(&entry, 50));
    }

    #[test]
    fn is_eligible_fee_12_needs_9_blocks() {
        // fee=12: eligible_after = (120/12) - 1 = 10 - 1 = 9
        let entry = make_entry(12, 100);
        assert!(!is_eligible(&entry, 108)); // age = 8, not yet
        assert!(is_eligible(&entry, 109)); // age = 9, eligible
    }

    #[test]
    fn is_eligible_fee_1_needs_119_blocks() {
        // fee=1: eligible_after = (120/1) - 1 = 119
        let entry = make_entry(1, 0);
        assert!(!is_eligible(&entry, 118)); // age = 118
        assert!(is_eligible(&entry, 119)); // age = 119
    }

    #[test]
    fn is_eligible_current_height_before_added_height_is_false() {
        // saturating_sub → age = 0; eligible_after for fee=1 is 119 > 0
        let entry = make_entry(1, 500);
        assert!(!is_eligible(&entry, 100));
    }

    #[test]
    fn is_eligible_fee_60_needs_1_block() {
        // fee=60: eligible_after = (120/60) - 1 = 2 - 1 = 1
        let entry = make_entry(60, 10);
        assert!(!is_eligible(&entry, 10)); // age = 0
        assert!(is_eligible(&entry, 11)); // age = 1
    }

    // ── mine ──────────────────────────────────────────────────────────────────

    #[test]
    fn mine_succeeds_immediately_with_zero_difficulty() {
        // difficulty=0: meets_difficulty always returns true, loop is skipped
        let mut block = empty_block();
        let cancel = AtomicBool::new(false);
        let shutdown = AtomicBool::new(false);
        let result = mine(&mut block, 0.0, &cancel, &shutdown);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 0); // found on very first hash
        assert!(!block.hash.is_empty());
    }

    #[test]
    fn mine_sets_block_hash_on_success() {
        let mut block = empty_block();
        let cancel = AtomicBool::new(false);
        let shutdown = AtomicBool::new(false);
        mine(&mut block, 0.0, &cancel, &shutdown);
        assert_eq!(block.hash, block.calculate_hash().expect("infallible"));
    }

    #[test]
    fn mine_returns_none_when_cancel_is_preset() {
        // cancel flag already true — mine checks at tries=0 and returns None
        let mut block = empty_block();
        let cancel = AtomicBool::new(true);
        let shutdown = AtomicBool::new(false);
        // Very high difficulty so we never find a valid hash, but cancel fires first
        let result = mine(&mut block, 255.0, &cancel, &shutdown);
        assert!(result.is_none());
    }

    #[test]
    fn mine_returns_none_when_shutdown_is_preset() {
        let mut block = empty_block();
        let cancel = AtomicBool::new(false);
        let shutdown = AtomicBool::new(true);
        let result = mine(&mut block, 255.0, &cancel, &shutdown);
        assert!(result.is_none());
    }
}

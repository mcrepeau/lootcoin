mod api;
mod blockchain;
mod checkpoints;
mod db;
mod gossip;
mod loot_ticket;
mod mempool;
mod metrics;

use api::{router, AppState};
use blockchain::Blockchain;
use lootcoin_core::block;
use lootcoin_core::transaction::Transaction;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tracing::{debug, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

// Minimal peer response — only the fields we need for sync decisions.
#[derive(Deserialize)]
struct PeerHead {
    height: u64,
    chain_work_hex: String,
}

/// Downloads and applies any blocks the peer has that we don't.
/// Queries all peers for their chain height, sorts them best-first, and tries
/// each in order. On any failure (network error, bad block) it falls back to
/// the next candidate, continuing from the current chain height so that blocks
/// already applied are not re-downloaded.
/// Downloads and applies any blocks the peer has that we don't.
/// Queries all peers for their chain height, sorts them best-first, and tries
/// each in order. On any failure (network error, bad block) it falls back to
/// the next candidate, continuing from the current chain height so that blocks
/// already applied are not re-downloaded.
///
/// The chain lock is held only for the duration of each individual block
/// application — never across network I/O — so concurrent requests continue
/// to be served while syncing is in progress.
async fn sync_from_peers(
    db: &db::Db,
    chain: Arc<RwLock<Blockchain>>,
    known_peers: &mut Vec<String>,
) {
    if known_peers.is_empty() {
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");

    // Ask every peer for their head; collect all that have more chain work than us.
    // Chain work (sum of 2^difficulty per block) is used instead of height so a
    // peer cannot win selection by lying about block count — they must back any
    // claimed work with actual proof-of-work when we fetch and validate blocks.
    let our_work = chain.read().await.get_chain_work();
    let mut candidates: Vec<(String, u64, u128)> = Vec::new(); // (url, height, chain_work)

    for peer in known_peers.iter() {
        match client.get(format!("{}/chain/head", peer)).send().await {
            Ok(resp) => match resp.json::<PeerHead>().await {
                Ok(head) => {
                    let work =
                        u128::from_str_radix(head.chain_work_hex.trim_start_matches("0x"), 16)
                            .unwrap_or(0);
                    if work > our_work {
                        candidates.push((peer.clone(), head.height, work));
                    }
                }
                Err(e) => warn!("Peer {} returned bad head: {}", peer, e),
            },
            Err(e) => warn!("Peer {} unreachable: {}", peer, e),
        }
    }

    if candidates.is_empty() {
        debug!("No peer has more chain work (ours: {:x})", our_work);
        return;
    }

    // Most accumulated work first.
    candidates.sort_by(|a, b| b.2.cmp(&a.2));

    const BATCH: u64 = 100;
    let mut synced_from: Option<String> = None;

    'peer: for (peer_url, peer_height, _peer_work) in &candidates {
        info!(
            "Syncing from {} — their height: {}, ours: {}",
            peer_url,
            peer_height,
            chain.read().await.get_height()
        );

        loop {
            let from = chain.read().await.get_height();
            if from >= *peer_height {
                break;
            }

            let url = format!("{}/blocks?from={}&limit={}", peer_url, from, BATCH);
            let blocks: Vec<block::Block> = match client.get(&url).send().await {
                Ok(resp) => match resp.json().await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!("Bad block data from {}: {} — trying next peer", peer_url, e);
                        continue 'peer;
                    }
                },
                Err(e) => {
                    warn!("Fetch failed from {}: {} — trying next peer", peer_url, e);
                    continue 'peer;
                }
            };

            if blocks.is_empty() {
                break;
            }

            for b in blocks {
                // Acquire the write lock only for the duration of block application.
                // Releases before the next network request so the API stays responsive.
                let (expected, accepted) = {
                    let mut c = chain.write().await;
                    let expected = c.get_height();
                    c.apply_block(db, b);
                    let accepted = c.get_height() > expected;
                    (expected, accepted)
                };
                if !accepted {
                    warn!(
                        "Block {} rejected from {} — trying next peer",
                        expected, peer_url
                    );
                    continue 'peer;
                }
            }
        }

        info!(
            "Sync complete via {}. Height: {}",
            peer_url,
            chain.read().await.get_height()
        );
        synced_from = Some(peer_url.clone());
        break;
    }

    // Peer discovery: ask the peer we synced from (or the best candidate) for
    // its peer list and merge any new entries into ours.
    let discovery_url = synced_from
        .as_deref()
        .or_else(|| candidates.first().map(|(u, _, _)| u.as_str()));

    let my_url = std::env::var("NODE_URL").ok();
    #[derive(serde::Deserialize)]
    struct PeersResp {
        peers: Vec<String>,
    }
    if let Some(peer_url) = discovery_url {
        if let Ok(resp) = client.get(format!("{}/peers", peer_url)).send().await {
            if let Ok(pr) = resp.json::<PeersResp>().await {
                for p in pr.peers {
                    if known_peers.len() >= gossip::MAX_PEERS {
                        break;
                    }
                    if my_url.as_deref() == Some(p.as_str()) {
                        continue; // don't add our own URL
                    }
                    if !api::is_safe_peer_url(&p) {
                        warn!("Ignoring unsafe peer URL from discovery: {}", p);
                        continue;
                    }
                    if !known_peers.contains(&p) {
                        info!("Discovered new peer: {}", p);
                        if let Err(e) = db.save_peer(&p) {
                            warn!("Failed to persist peer {}: {}", p, e);
                        }
                        known_peers.push(p);
                    }
                }
            }
        }
    }
}

/// POST our own URL to every peer's /peers endpoint so they learn about us
/// without manual configuration. Only runs if NODE_URL env var is set.
async fn announce_self(peers: &[String], my_url: &str) {
    if peers.is_empty() {
        return;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build HTTP client");
    for peer in peers {
        match client
            .post(format!("{}/peers", peer))
            .json(&serde_json::json!({"url": my_url}))
            .send()
            .await
        {
            Ok(_) => info!("Announced self ({}) to peer {}", my_url, peer),
            Err(e) => warn!("Failed to announce to peer {}: {}", peer, e),
        }
    }
}

/// Query peers for available snapshots and download the highest one that
/// matches a hardcoded trusted checkpoint. Returns the restored state and
/// the checkpoint block (needed to seed the in-memory window) on success.
///
/// Side effects on success: saves the checkpoint block and the serialized
/// CheckpointState to the local DB so the next restart uses the fast path.
async fn fetch_peer_snapshot(
    peers: &[String],
    db: &db::Db,
) -> Option<(u64, blockchain::CheckpointState, block::Block)> {
    use api::SnapshotPayload;
    use checkpoints::TRUSTED_CHECKPOINTS;

    if TRUSTED_CHECKPOINTS.is_empty() || peers.is_empty() {
        return None;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");

    #[derive(serde::Deserialize)]
    struct SnapshotInfo {
        height: u64,
        block_hash_hex: String,
    }

    // Gather every (peer, height, hash) triple that matches a trusted checkpoint.
    let mut candidates: Vec<(String, u64, String)> = Vec::new();
    for peer in peers {
        match client.get(format!("{}/snapshots", peer)).send().await {
            Ok(resp) => match resp.json::<Vec<SnapshotInfo>>().await {
                Ok(list) => {
                    for s in list {
                        let trusted = TRUSTED_CHECKPOINTS.iter().find(|(h, _)| *h == s.height);
                        if let Some((_, trusted_hash)) = trusted {
                            if s.block_hash_hex.trim_start_matches("0x")
                                == trusted_hash.trim_start_matches("0x")
                            {
                                candidates.push((peer.clone(), s.height, s.block_hash_hex));
                            }
                        }
                    }
                }
                Err(e) => warn!("Could not parse snapshots from {}: {}", peer, e),
            },
            Err(e) => warn!("Could not reach {} for snapshots: {}", peer, e),
        }
    }

    if candidates.is_empty() {
        debug!("No trusted snapshots found from peers");
        return None;
    }

    // Prefer the highest checkpoint so we skip the most replay.
    candidates.sort_by_key(|(_, h, _)| std::cmp::Reverse(*h));

    for (peer, height, expected_hash) in candidates {
        info!(
            "Attempting snapshot sync from {} at height {}",
            peer, height
        );

        // Download the snapshot payload.
        let payload: SnapshotPayload = match client
            .get(format!("{}/snapshot/{}", peer, height))
            .send()
            .await
        {
            Ok(resp) => match resp.json::<SnapshotPayload>().await {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        "Failed to parse snapshot from {} at {}: {}",
                        peer, height, e
                    );
                    continue;
                }
            },
            Err(e) => {
                warn!(
                    "Failed to download snapshot from {} at {}: {}",
                    peer, height, e
                );
                continue;
            }
        };

        // Verify the payload hash against our trusted anchor.
        let expected = expected_hash.trim_start_matches("0x");
        if payload.block_hash_hex.trim_start_matches("0x") != expected {
            warn!("Snapshot from {} has wrong block hash — skipping", peer);
            continue;
        }

        // Download the checkpoint block so we can seed the in-memory window.
        let cp_blocks: Vec<block::Block> = match client
            .get(format!("{}/blocks?from={}&limit=1", peer, height))
            .send()
            .await
        {
            Ok(resp) => match resp.json::<Vec<block::Block>>().await {
                Ok(b) => b,
                Err(e) => {
                    warn!("Failed to parse checkpoint block from {}: {}", peer, e);
                    continue;
                }
            },
            Err(e) => {
                warn!("Failed to download checkpoint block from {}: {}", peer, e);
                continue;
            }
        };

        let cp_block = match cp_blocks.into_iter().next() {
            Some(b) => b,
            None => {
                warn!("No block at height {} from {}", height, peer);
                continue;
            }
        };

        // Final integrity check: the block's own hash must match the snapshot.
        if hex::encode(&cp_block.hash) != expected {
            warn!("Checkpoint block hash mismatch from {} — skipping", peer);
            continue;
        }

        // Parse chain_work from hex string.
        let chain_work =
            u128::from_str_radix(payload.chain_work_hex.trim_start_matches("0x"), 16).unwrap_or(0);

        let state = blockchain::CheckpointState {
            balances: payload.balances,
            account_nonces: payload.account_nonces,
            pot: payload.pot,
            chain_work,
            block_hash: cp_block.hash.clone(),
            current_difficulty: payload.current_difficulty,
            asert_anchor: payload.asert_anchor,
            tickets: payload.tickets,
        };

        // Persist block and snapshot so the next restart uses the fast path.
        if let Err(e) = db.save_block_indexed(&cp_block) {
            warn!("Failed to persist checkpoint block: {}", e);
            continue;
        }
        match bincode::serialize(&state) {
            Ok(data) => {
                if let Err(e) = db.save_checkpoint(height, &data) {
                    warn!("Failed to persist local checkpoint: {}", e);
                    continue;
                }
            }
            Err(e) => {
                warn!("Failed to serialize checkpoint state: {}", e);
                continue;
            }
        }

        info!("Snapshot sync complete: height={} from {}", height, peer);
        return Some((height, state, cp_block));
    }

    None
}

/// CubeHash-256 digest of the genesis recipient's public key (hex-encoded).
/// The actual address is the bech32m encoding of these bytes, derived at
/// startup via `lootcoin_core::wallet::encode_address`.  The secret key
/// is held by whoever bootstrapped the chain and is never embedded here.
const GENESIS_ADDRESS: &str = "loot1075w8yehjl4rvrdsuws6yn2f3sa8e7hhymr5q8ps9sa7gvulzpjq5dvlt3";
/// Fixed Unix timestamp for the genesis block so the block contents — and
/// therefore its hash — are identical regardless of when a node first boots.
const GENESIS_TIMESTAMP: u64 = 1_748_000_000; // 2025-05-23
const GENESIS_WALLET_AMOUNT: u64 = 1_000_000;
const GENESIS_POT_AMOUNT: u64 = 99_000_000;

/// Resolves when SIGINT (Ctrl-C) or SIGTERM is received.
async fn shutdown_signal() {
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

    // On non-Unix platforms (Windows) SIGTERM doesn't exist; only Ctrl-C.
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received — stopping server");
}

/// Path written once the API listener is bound. Used by the Docker HEALTHCHECK.
const HEALTHY_FILE: &str = "/app/healthy";

#[tokio::main]
async fn main() {
    // Health-check subcommand: exit 0 if the ready file exists, 1 otherwise.
    // Invoked by the Docker HEALTHCHECK — no logging needed.
    if std::env::args().nth(1).as_deref() == Some("health") {
        std::process::exit(if std::path::Path::new(HEALTHY_FILE).exists() {
            0
        } else {
            1
        });
    }

    // Remove any stale ready file from a previous run so health checks don't
    // pass before this boot's listener is actually bound.
    let _ = std::fs::remove_file(HEALTHY_FILE);

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();

    // 1. Open the database
    let db = db::Db::new().expect("Failed to init DB");

    // 2. Load the canonical chain from the BLOCKS table and replay it.
    //    If a checkpoint exists, only the tail (blocks after the checkpoint) is
    //    replayed in memory — the rest is loaded directly from the snapshot.
    let stored_blocks = db
        .load_canonical_chain()
        .expect("Failed to load chain from DB");

    let maybe_checkpoint = db
        .load_latest_checkpoint()
        .expect("Failed to read checkpoints");

    // Pre-build the env peer list — needed for peer snapshot sync on fresh nodes
    // before the full peer list (env + DB) is assembled later in the startup sequence.
    let env_peers: Vec<String> = std::env::var("PEERS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    // true  → came from a checkpoint; TX index is already up to date in DB.
    // false → did a full in-memory replay; TX index must be rebuilt from scratch.
    let used_checkpoint: bool;
    // Lowest block height for which this node has data. 0 = full archive.
    // Set to the snapshot height when the node bootstraps from a peer snapshot.
    let history_start: u64;

    let mut chain = if stored_blocks.is_empty() {
        // Fresh node — try to fast-bootstrap from a peer snapshot before
        // falling back to the full genesis → replay path.
        // Set ARCHIVE=1 to force a full replay from genesis instead.
        let force_archive = std::env::var("ARCHIVE").map(|v| v == "1").unwrap_or(false);
        let snapshot = if force_archive {
            info!("ARCHIVE=1: skipping peer snapshot sync, replaying from genesis");
            None
        } else {
            fetch_peer_snapshot(&env_peers, &db).await
        };
        if let Some((snap_height, snap_state, cp_block)) = snapshot {
            // Snapshot sync succeeded. Use a dummy genesis just to construct
            // Blockchain (which requires a Block); restore_from_checkpoint will
            // overwrite every field. genesis_pot is set so deep-reorg replay works.
            let dummy_genesis = block::Block {
                index: 0,
                previous_hash: vec![0u8; 32],
                timestamp: GENESIS_TIMESTAMP,
                nonce: 0,
                tx_root: vec![],
                transactions: vec![],
                hash: vec![],
            };
            let mut chain = Blockchain::new(dummy_genesis);
            chain.seed_pot(GENESIS_POT_AMOUNT); // sets genesis_pot; pot is overwritten below
            chain.restore_from_checkpoint(snap_height, snap_state, cp_block);
            used_checkpoint = true;
            history_start = snap_height;
            chain
        } else {
            // No trusted peer snapshot available — create the deterministic genesis block.
            // All nodes use the same fixed address and timestamp so they produce an
            // identical genesis block and can interoperate without manual coordination.
            info!("No history found. Initializing new chain with Genesis.");

            let genesis_txs = vec![Transaction {
                sender: String::new(),
                receiver: GENESIS_ADDRESS.parse().unwrap(),
                amount: GENESIS_WALLET_AMOUNT,
                fee: 0,
                nonce: 0,
                public_key: [0u8; 32],
                signature: vec![],
            }];
            let genesis = block::Block {
                index: 0,
                previous_hash: vec![0u8; 32],
                timestamp: GENESIS_TIMESTAMP,
                nonce: 0,
                tx_root: block::Block::compute_tx_root(&genesis_txs),
                transactions: genesis_txs,
                hash: vec![],
            };

            info!("Genesis address:        {}", GENESIS_ADDRESS);
            info!("Genesis wallet coins:   {}", GENESIS_WALLET_AMOUNT);
            info!("Lottery pot seeded:     {}", GENESIS_POT_AMOUNT);

            db.save_block_indexed(&genesis)
                .expect("Failed to persist genesis");
            let mut chain = Blockchain::new(genesis);
            chain.seed_pot(GENESIS_POT_AMOUNT);
            used_checkpoint = false;
            history_start = 0;
            chain
        }
    } else if let Some((cp_height, cp_data)) = maybe_checkpoint {
        // Checkpoint found — deserialize and validate it against the BLOCKS table.
        let state: blockchain::CheckpointState =
            bincode::deserialize(&cp_data).expect("Failed to deserialize checkpoint");

        let cp_block = db
            .get_blocks_range(cp_height, 1)
            .expect("Failed to load checkpoint block")
            .into_iter()
            .next()
            .expect("Checkpoint block missing from BLOCKS table");

        if cp_block.hash == state.block_hash {
            info!(
                "Found checkpoint at block {}; replaying only the tail...",
                cp_height
            );
            // seed_pot sets genesis_pot (needed if a later reorg forces a full replay)
            // and also sets pot — restore_from_checkpoint will overwrite pot with the
            // checkpoint value.
            let genesis = stored_blocks.into_iter().next().unwrap();
            let mut chain = Blockchain::new(genesis);
            chain.seed_pot(GENESIS_POT_AMOUNT);
            chain.restore_from_checkpoint(cp_height, state, cp_block);

            let tail_blocks = db
                .load_blocks_from(cp_height + 1)
                .expect("Failed to load post-checkpoint blocks");
            let tail_len = tail_blocks.len();
            for block in tail_blocks {
                chain.apply_in_memory(block, None);
            }
            info!(
                "Blockchain state rebuilt from checkpoint + {} block(s). Height: {}",
                tail_len,
                chain.get_height()
            );
            used_checkpoint = true;
            history_start = 0; // archive node — has full history in BLOCKS table
            chain
        } else {
            // Hash mismatch: this checkpoint is from a reorged (displaced) chain.
            // Fall back to a full replay so we derive correct state from the DB.
            warn!(
                "Checkpoint at block {} is stale (hash mismatch) — falling back to full replay",
                cp_height
            );
            if let Err(e) = db.delete_checkpoints_from(0) {
                warn!("Failed to purge stale checkpoints: {}", e);
            }
            let mut iter = stored_blocks.into_iter();
            let genesis = iter.next().unwrap();
            let mut chain = Blockchain::new(genesis);
            chain.seed_pot(GENESIS_POT_AMOUNT);
            for block in iter {
                chain.apply_in_memory(block, None);
            }
            info!("Blockchain state rebuilt. Height: {}", chain.get_height());
            used_checkpoint = false;
            history_start = 0;
            chain
        }
    } else {
        // No checkpoint yet — full replay from genesis.
        info!("Replaying {} block(s) from storage...", stored_blocks.len());
        let mut iter = stored_blocks.into_iter();
        let genesis = iter.next().unwrap();
        let mut chain = Blockchain::new(genesis);
        // Re-seed the pot from genesis. All subsequent pot changes (fees in,
        // lottery payouts out) are replayed through apply_in_memory below.
        chain.seed_pot(GENESIS_POT_AMOUNT);
        for block in iter {
            chain.apply_in_memory(block, None);
        }
        info!("Blockchain state rebuilt. Height: {}", chain.get_height());
        used_checkpoint = false;
        history_start = 0;
        chain
    };

    // 3. Rebuild the TX index from the full in-memory chain (before pruning),
    //    including any lottery payouts accumulated during replay.
    //    Skipped when we started from a checkpoint — the TX index was maintained
    //    by save_applied_block and is already consistent with the BLOCKS table.
    let all_payouts = chain.take_all_settled_payouts();
    if !used_checkpoint {
        db.rebuild_tx_index(&chain.blocks, &all_payouts)
            .expect("Failed to build tx index");
    }

    // 4. Prune in-memory blocks to the sliding window — historical blocks are
    //    now served exclusively from the BLOCKS table in redb.
    chain.prune_to_window();

    // 5. Restore persisted lottery tickets
    match db.load_tickets() {
        Ok(tickets) => {
            info!("Restored {} pending lottery ticket(s)", tickets.len());
            chain.restore_tickets(tickets);
        }
        Err(e) => warn!("Failed to load persisted tickets: {}", e),
    }

    // 6. Build peer list: env var + persisted peers (deduplicated)
    let mut peers: Vec<String> = std::env::var("PEERS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let my_url = std::env::var("NODE_URL").ok();
    match db.load_peers() {
        Ok(stored) => {
            for p in stored {
                if my_url.as_deref() == Some(p.as_str()) {
                    continue; // don't re-add our own URL if it was persisted
                }
                if !api::is_safe_peer_url(&p) {
                    warn!("Skipping persisted peer with unsafe URL: {}", p);
                    continue;
                }
                if !peers.contains(&p) {
                    peers.push(p);
                }
            }
        }
        Err(e) => warn!("Failed to load persisted peers: {}", e),
    }
    for p in &peers {
        let _ = db.save_peer(p);
    }

    // Wrap chain in Arc<RwLock> now so sync_from_peers can hold the lock
    // only per-block rather than across all network I/O.
    let chain = Arc::new(RwLock::new(chain));

    // 7. Sync any blocks we missed from peers (also does peer discovery)
    sync_from_peers(&db, Arc::clone(&chain), &mut peers).await;

    // 8. Announce our own URL to all known peers (optional — requires NODE_URL env var)
    if let Ok(my_url) = std::env::var("NODE_URL") {
        announce_self(&peers, &my_url).await;
    }

    let gossip = gossip::Gossip::new(peers);

    let db = Arc::new(db);
    let gossip = Arc::new(gossip);

    // 9a. Restore persisted mempool entries, dropping any with stale nonces
    //     (already confirmed) or that have expired beyond TX_EXPIRY_BLOCKS.
    let mut mempool = mempool::Mempool::new(Some(Arc::clone(&db)));
    match db.load_mempool() {
        Ok(entries) => {
            let current_height = chain.read().await.get_height();
            let confirmed_filtered: Vec<_> = {
                let c = chain.read().await;
                entries
                    .into_iter()
                    .filter(|(tx, added_height)| {
                        // Drop txs whose nonce has already been consumed on-chain.
                        let is_stale = c.get_nonce(&tx.sender) > tx.nonce;
                        let is_expired = current_height.saturating_sub(*added_height)
                            > mempool::TX_EXPIRY_BLOCKS;
                        !is_stale && !is_expired
                    })
                    .collect()
            };
            let restored = confirmed_filtered.len();
            mempool.restore(confirmed_filtered);
            info!("Restored {} pending mempool transaction(s)", restored);
        }
        Err(e) => warn!("Failed to load persisted mempool: {}", e),
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let metrics = Arc::new(metrics::Metrics::new());

    // Seed counters from persisted history so they survive node restarts.
    let chain_height = chain.read().await.get_height();
    match db.scan_fee_totals() {
        Ok((total_fees, miner_share)) => {
            metrics.seed_fees(total_fees, miner_share, chain_height.saturating_sub(1));
        }
        Err(e) => warn!("failed to seed fee metrics: {}", e),
    }
    match db.scan_lottery_payout_totals() {
        Ok(totals) => {
            for (tier, (wins, coins)) in totals {
                metrics.seed_lottery(&tier, wins, coins);
            }
        }
        Err(e) => warn!("failed to seed lottery metrics: {}", e),
    }

    chain.write().await.metrics = Some(Arc::clone(&metrics));

    let state = AppState {
        db: Arc::clone(&db),
        chain,
        mempool: Arc::new(RwLock::new(mempool)),
        gossip: Arc::clone(&gossip),
        shutdown_rx,
        sse_subscribers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        metrics,
        history_start,
    };

    // 9. Subscribe to the SSE event streams of all known peers so we receive
    //    new blocks and transactions immediately without fire-and-forget polling.
    {
        let initial_peers = state.gossip.peer_urls().await;
        for peer_url in initial_peers {
            api::spawn_peer_subscription(state.clone(), peer_url);
        }
    }

    // 10. Background task: ping peers every hour, evict those silent for 24 h
    {
        let gossip_bg = Arc::clone(&gossip);
        let db_bg = Arc::clone(&db);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                gossip_bg.ping_peers().await;
                let evicted = gossip_bg.evict_stale(Duration::from_secs(24 * 3600)).await;
                for url in &evicted {
                    if let Err(e) = db_bg.delete_peer(url) {
                        warn!("Failed to remove evicted peer {} from DB: {}", url, e);
                    }
                }
            }
        });
    }

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!("API listening on http://{}", addr);
    let _ = std::fs::write(HEALTHY_FILE, b"1");

    let app = router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        // Signal all SSE streams to close so axum can drain connections.
        let _ = shutdown_tx.send(true);
    })
    .await
    .unwrap();

    info!("Server stopped cleanly");
}

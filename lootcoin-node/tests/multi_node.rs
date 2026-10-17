//! Multi-node integration tests: spawn real `lootcoin-node` processes and make
//! them talk to each other over HTTP.
//!
//! Coverage that unit tests cannot provide, because the interesting code is
//! between processes and across the network:
//!   - gossip propagation of blocks between peers
//!   - duplicate block submission (the miner fan-out path) does not
//!     double-issue or cancel lottery tickets
//!   - fork resolution: a longer chain reorgs the node
//!   - transaction relay into peer mempools and settlement
//!   - lottery ticket lifecycle consistency across nodes
//!   - a restarted node catches up from its peers
//!
//! Nodes are spawned from the compiled binary with:
//!   - `LOOTCOIN_DATA_DIR` — a per-test temp directory
//!   - `LOOTCOIN_INITIAL_DIFFICULTY=2` — PoW is a few hashes per block
//!   - `PORT` — a pre-allocated free port
//!   - `PEERS` — loopback peer URLs (operator-configured peers may be
//!     loopback; the public POST /peers endpoint stays strict)
//!
//! Crafted block timestamps are spaced 60s apart (`GENESIS_TS + 60·index`) so
//! ASERT keeps the difficulty flat while the test mines instantly. The
//! timestamps are far in the past, which the validator accepts (only future
//! timestamps are rejected).

use lootcoin_core::block::{meets_difficulty, Block};
use lootcoin_core::transaction::Transaction;
use lootcoin_core::wallet::Wallet;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Must match `GENESIS_TIMESTAMP` in lootcoin-node/src/main.rs.
const GENESIS_TIMESTAMP: u64 = 1_748_000_000;
const TEST_DIFFICULTY: &str = "2";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(60);

// ─── node process harness ─────────────────────────────────────────────────────

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0 for a free port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A running node process. Killed on drop so a failing test never leaks
/// processes; the log file is kept for post-mortem inspection.
struct Node {
    url: String,
    log: PathBuf,
    child: Option<std::process::Child>,
}

impl Node {
    /// Spawn a node at `port`, storing state in `dir` (an existing dir is
    /// reused as-is — used by the restart test).
    fn spawn_at(port: u16, dir: PathBuf, peers: &[String]) -> Node {
        std::fs::create_dir_all(&dir).expect("create node data dir");
        let log = dir.join("node.log");
        let stdout = std::fs::File::create(&log).expect("create log file");
        let stderr = stdout.try_clone().expect("clone log handle");

        let child = std::process::Command::new(env!("CARGO_BIN_EXE_lootcoin-node"))
            .env("PORT", port.to_string())
            .env("LOOTCOIN_DATA_DIR", &dir)
            .env("LOOTCOIN_INITIAL_DIFFICULTY", TEST_DIFFICULTY)
            .env("RUST_LOG", "info")
            .env("PEERS", peers.join(","))
            .current_dir(&dir)
            .stdout(std::process::Stdio::from(stdout))
            .stderr(std::process::Stdio::from(stderr))
            .spawn()
            .expect("spawn lootcoin-node");
        Node {
            url: format!("http://127.0.0.1:{port}"),
            log,
            child: Some(child),
        }
    }

    /// Spawn a node at `port` with a fresh temp data dir.
    fn spawn(port: u16, peers: &[String]) -> Node {
        let dir = std::env::temp_dir().join(format!("loot-it-{}-{port}", std::process::id()));
        Self::spawn_at(port, dir, peers)
    }

    /// Kill the process (redb is crash-safe; the node relies on that anyway).
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.kill();
    }
}

// ─── async API helpers ────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, PartialEq)]
struct Head {
    height: u64,
    latest_hash_hex: String,
    difficulty: f64,
    pot: u64,
}

async fn head(url: &str) -> Head {
    reqwest::get(format!("{url}/chain/head"))
        .await
        .expect("GET /chain/head")
        .json()
        .await
        .expect("parse chain head")
}

async fn get_json(url: &str, path: &str) -> serde_json::Value {
    reqwest::get(format!("{url}{path}"))
        .await
        .expect("GET json")
        .json::<serde_json::Value>()
        .await
        .expect("parse json")
}

async fn wait_ready(node: &Node) {
    let url = &node.url;
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(resp) = reqwest::get(format!("{url}/node/info")).await {
            if resp.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!(
        "node {url} never became ready (log: {})",
        node.log.display()
    );
}

/// Poll until the node's head reaches `height`, with a generous deadline:
/// peer SSE subscriptions start with a 5s reconnect backoff, so propagation
/// can legitimately take a few seconds after boot.
async fn wait_for_height(url: &str, height: u64) {
    let deadline = Instant::now() + CONVERGENCE_TIMEOUT;
    while Instant::now() < deadline {
        if head(url).await.height >= height {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("node {url} never reached height {height}");
}

async fn submit_block(url: &str, block: &Block) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/blocks"))
        .json(block)
        .send()
        .await
        .expect("POST /blocks")
}

async fn balance(url: &str, address: &str) -> u64 {
    #[derive(Deserialize)]
    struct BalanceResp {
        balance: u64,
    }
    reqwest::get(format!("{url}/balance/{address}"))
        .await
        .expect("GET /balance")
        .json::<BalanceResp>()
        .await
        .expect("parse balance")
        .balance
}

// ─── block crafting (the "miner") ────────────────────────────────────────────

fn coinbase(receiver: &str) -> Transaction {
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

/// Build a valid block extending `prev_hash_hex` with proof-of-work at
/// `difficulty`. Timestamps are 60s apart so ASERT keeps the difficulty flat.
fn craft_block(
    index: u64,
    prev_hash_hex: &str,
    miner: &str,
    txs: Vec<Transaction>,
    difficulty: f64,
) -> Block {
    let mut all_txs = vec![coinbase(miner)];
    all_txs.extend(txs);
    let tx_root = Block::compute_tx_root(&all_txs).expect("infallible");
    let mut block = Block {
        index,
        previous_hash: hex::decode(prev_hash_hex).expect("prev hash hex"),
        timestamp: GENESIS_TIMESTAMP + 60 * index,
        nonce: 0,
        tx_root,
        transactions: all_txs,
        hash: vec![],
    };
    let mut hash = block.calculate_hash().expect("infallible");
    while !meets_difficulty(&hash, difficulty) {
        block.nonce += 1;
        hash = block.calculate_hash().expect("infallible");
    }
    block.hash = hash;
    block
}

/// Craft the next block on the node's current tip and return it (not submitted).
async fn mine_on(url: &str, miner: &str, txs: Vec<Transaction>) -> Block {
    let h = head(url).await;
    craft_block(h.height, &h.latest_hash_hex, miner, txs, h.difficulty)
}

/// Submit a block to the node and assert acceptance.
async fn submit_ok(url: &str, block: &Block) {
    let resp = submit_block(url, block).await;
    let status = resp.status();
    assert!(
        status.is_success(),
        "block at index {} rejected by {url}: {status}",
        block.index
    );
}

// ─── scenarios ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn blocks_propagate_across_peers() {
    let ports = [free_port(), free_port(), free_port()];
    let urls: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    let others = |i: usize| {
        urls.iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, u)| u.clone())
            .collect::<Vec<_>>()
    };
    let nodes = [
        Node::spawn(ports[0], &others(0)),
        Node::spawn(ports[1], &others(1)),
        Node::spawn(ports[2], &others(2)),
    ];
    for n in &nodes {
        wait_ready(n).await;
    }

    let miner = Wallet::from_secret_key_bytes([0x11u8; 32]).get_address();
    let block = mine_on(&urls[0], &miner, vec![]).await;
    submit_ok(&urls[0], &block).await;

    for u in &urls[1..] {
        wait_for_height(u, 2).await;
        let h = head(u).await;
        assert_eq!(
            h.latest_hash_hex,
            hex::encode(&block.hash),
            "peer {u} on a different chain"
        );
    }
}

#[tokio::test]
async fn duplicate_block_submission_does_not_double_issue_tickets() {
    let ports = [free_port(), free_port()];
    let urls: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    let nodes = [
        Node::spawn(ports[0], &[urls[1].clone()]),
        Node::spawn(ports[1], &[urls[0].clone()]),
    ];
    for n in &nodes {
        wait_ready(n).await;
    }

    // Fund a wallet with 3 coins so the ticket-triggering block can carry a
    // real transaction (MIN_TX_FEE is 2; amount 1 + fee 2 needs 3 coins).
    let spender = Wallet::from_secret_key_bytes([0x22u8; 32]);
    let miner = Wallet::from_secret_key_bytes([0x33u8; 32]).get_address();
    let receiver = Wallet::from_secret_key_bytes([0x44u8; 32]).get_address();
    for _ in 0..3 {
        let b = mine_on(&urls[0], &spender.get_address(), vec![]).await;
        submit_ok(&urls[0], &b).await;
    }
    assert_eq!(balance(&urls[0], &spender.get_address()).await, 3);

    // Block 4 contains a real tx → the miner earns a ticket.
    let tx = Transaction::new_signed(&spender, receiver.clone(), 1, 2);
    let block = mine_on(&urls[0], &miner, vec![tx]).await;
    submit_ok(&urls[0], &block).await;
    wait_for_height(&urls[1], 5).await;

    // Duplicate submission (the fan-out path): both nodes already have it.
    for u in &urls {
        let resp = submit_block(u, &block).await;
        assert_eq!(
            resp.status().as_u16(),
            409,
            "duplicate not a conflict on {u}"
        );
        assert_eq!(
            head(u).await.height,
            5,
            "duplicate changed the height on {u}"
        );
    }

    // The ticket exists exactly once per node, identically on both.
    let p1 = get_json(&urls[0], "/lottery/pending-tickets").await;
    let p2 = get_json(&urls[1], "/lottery/pending-tickets").await;
    assert_eq!(p1, p2, "pending tickets diverge between peers");
    assert_eq!(
        p1.as_array().expect("array").len(),
        1,
        "duplicate delivery changed the ticket count"
    );
    assert_eq!(p1[0]["miner"], serde_json::json!(miner));
    assert_eq!(p1[0]["created_height"], serde_json::json!(4));
    assert_eq!(p1[0]["settle_height"], serde_json::json!(104));
}

#[tokio::test]
async fn longer_fork_reorgs_the_chain() {
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let node = Node::spawn(port, &[]);
    wait_ready(&node).await;

    let genesis = head(&url).await;
    let difficulty = genesis.difficulty;
    let miner_a = Wallet::from_secret_key_bytes([0x55u8; 32]).get_address();
    let miner_b = Wallet::from_secret_key_bytes([0x66u8; 32]).get_address();

    // Block A on the main chain.
    let a = craft_block(1, &genesis.latest_hash_hex, &miner_a, vec![], difficulty);
    submit_ok(&url, &a).await;

    // Competing block B at the same height: valid work, links to genesis —
    // kept as an orphan, answered with a conflict.
    let b = craft_block(1, &genesis.latest_hash_hex, &miner_b, vec![], difficulty);
    let resp = submit_block(&url, &b).await;
    assert_eq!(
        resp.status().as_u16(),
        409,
        "competing block should conflict"
    );
    assert_eq!(balance(&url, &miner_b).await, 0);

    // B's child carries more work than the main chain → reorg to B+C.
    let c = craft_block(2, &hex::encode(&b.hash), &miner_b, vec![], difficulty);
    submit_ok(&url, &c).await;

    let h = head(&url).await;
    assert_eq!(h.height, 3, "height after reorg");
    assert_eq!(
        h.latest_hash_hex,
        hex::encode(&c.hash),
        "head is not the fork tip"
    );
    assert_eq!(
        balance(&url, &miner_b).await,
        2,
        "fork coinbase not credited"
    );
    assert_eq!(
        balance(&url, &miner_a).await,
        0,
        "displaced coinbase not unwound"
    );

    // The displaced block can never come back through a duplicate delivery.
    let resp = submit_block(&url, &a).await;
    assert_eq!(resp.status().as_u16(), 409);
    assert_eq!(head(&url).await.height, 3);
}

#[tokio::test]
async fn transactions_relay_to_peers_and_settle() {
    let ports = [free_port(), free_port()];
    let urls: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    let nodes = [
        Node::spawn(ports[0], &[urls[1].clone()]),
        Node::spawn(ports[1], &[urls[0].clone()]),
    ];
    for n in &nodes {
        wait_ready(n).await;
    }

    // Fund the spender with 15 coins (15 quick coinbase blocks) so several
    // relay attempts can each spend 3 coins (amount 1 + fee 2).
    let spender = Wallet::from_secret_key_bytes([0x77u8; 32]);
    let miner = Wallet::from_secret_key_bytes([0x88u8; 32]).get_address();
    let receiver = Wallet::from_secret_key_bytes([0x99u8; 32]).get_address();
    for _ in 0..15 {
        let b = mine_on(&urls[0], &spender.get_address(), vec![]).await;
        submit_ok(&urls[0], &b).await;
    }

    // Submit a signed transaction to node 1 and wait for it in the PEER's
    // mempool. Relay rides the peer's SSE subscription, which connects with
    // a 5s backoff after boot — a tx submitted before the stream is live is
    // never re-sent. Fresh txs (new random nonce each) retry until one rides
    // a live stream; each attempt spends 3 coins.
    let mut submitted: Vec<Transaction> = Vec::new();
    let deadline = Instant::now() + CONVERGENCE_TIMEOUT;
    while Instant::now() < deadline {
        let tx = Transaction::new_signed(&spender, receiver.clone(), 1, 2);
        let resp = reqwest::Client::new()
            .post(format!("{}/transactions", urls[0]))
            .json(&serde_json::json!({
                "sender": tx.sender,
                "receiver": tx.receiver,
                "amount": tx.amount,
                "fee": tx.fee,
                "nonce": tx.nonce,
                "public_key_hex": hex::encode(tx.public_key),
                "signature_hex": hex::encode(&tx.signature),
            }))
            .send()
            .await
            .expect("POST /transactions");
        assert!(resp.status().is_success(), "tx rejected by submitter node");
        submitted.push(tx);

        let attempt_deadline = Instant::now() + Duration::from_secs(7);
        let mut relayed = false;
        while Instant::now() < attempt_deadline {
            let mempool = get_json(&urls[1], "/mempool").await;
            if mempool
                .as_array()
                .expect("array")
                .iter()
                .any(|e| e["transaction"]["sender"] == serde_json::json!(spender.get_address()))
            {
                relayed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if relayed {
            break;
        }
    }
    assert!(!submitted.is_empty(), "no tx submitted");
    let n_txs = submitted.len() as u64;

    // Mine every submitted tx, and balances must agree on both nodes.
    let block = mine_on(&urls[0], &miner, submitted).await;
    submit_ok(&urls[0], &block).await;
    wait_for_height(&urls[1], head(&urls[0]).await.height).await;

    for u in &urls {
        assert_eq!(
            balance(u, &spender.get_address()).await,
            15 - 3 * n_txs,
            "spender on {u}"
        );
        assert_eq!(balance(u, &receiver).await, n_txs, "receiver on {u}");
    }
}

#[tokio::test]
async fn ticket_settlement_is_consistent_across_nodes() {
    let ports = [free_port(), free_port()];
    let urls: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    let nodes = [
        Node::spawn(ports[0], &[urls[1].clone()]),
        Node::spawn(ports[1], &[urls[0].clone()]),
    ];
    for n in &nodes {
        wait_ready(n).await;
    }

    // Blocks 1..=3 fund the spender; block 4 carries a real tx → ticket at 4.
    let spender = Wallet::from_secret_key_bytes([0xAAu8; 32]);
    let miner = Wallet::from_secret_key_bytes([0xBBu8; 32]).get_address();
    let receiver = Wallet::from_secret_key_bytes([0xCCu8; 32]).get_address();
    for _ in 0..3 {
        let b = mine_on(&urls[0], &spender.get_address(), vec![]).await;
        submit_ok(&urls[0], &b).await;
    }
    let tx = Transaction::new_signed(&spender, receiver, 1, 2);
    let b4 = mine_on(&urls[0], &miner, vec![tx]).await;
    submit_ok(&urls[0], &b4).await;
    wait_for_height(&urls[1], 5).await;

    // Both nodes hold the identical pending ticket.
    let p1 = get_json(&urls[0], "/lottery/pending-tickets").await;
    let p2 = get_json(&urls[1], "/lottery/pending-tickets").await;
    assert_eq!(p1, p2);
    assert_eq!(p1.as_array().expect("array").len(), 1);

    // Mine blocks 5..=104 — the ticket settles when block 104 is applied.
    for _ in 0..100 {
        let b = mine_on(&urls[0], &miner, vec![]).await;
        submit_ok(&urls[0], &b).await;
    }
    wait_for_height(&urls[1], 105).await;

    for u in &urls {
        let pending = get_json(u, "/lottery/pending-tickets").await;
        assert_eq!(
            pending.as_array().expect("array").len(),
            0,
            "ticket still pending on {u} after settlement"
        );
    }

    // Whatever the draw produced (payout or "none"), both nodes must agree on
    // the settling block, the pot, and the head.
    let settle1 = get_json(&urls[0], "/blocks?from=104&limit=1").await;
    let settle2 = get_json(&urls[1], "/blocks?from=104&limit=1").await;
    assert_eq!(settle1, settle2, "settling block diverges between peers");
    assert_eq!(head(&urls[0]).await, head(&urls[1]).await);
}

#[tokio::test]
async fn restarted_node_catches_up_from_peers() {
    let ports = [free_port(), free_port()];
    let urls: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    let _n1 = Node::spawn(ports[0], &[urls[1].clone()]);
    let dir2 = std::env::temp_dir().join(format!("loot-it-{}-{}", std::process::id(), ports[1]));
    let mut n2 = Node::spawn_at(ports[1], dir2.clone(), &[urls[0].clone()]);
    wait_ready(&_n1).await;
    wait_ready(&n2).await;

    let miner = Wallet::from_secret_key_bytes([0xDDu8; 32]).get_address();
    for _ in 0..2 {
        let b = mine_on(&urls[0], &miner, vec![]).await;
        submit_ok(&urls[0], &b).await;
    }
    wait_for_height(&urls[1], 3).await;

    // Take node 2 down, extend the chain, bring it back on the same data dir.
    n2.kill();
    for _ in 0..3 {
        let b = mine_on(&urls[0], &miner, vec![]).await;
        submit_ok(&urls[0], &b).await;
    }
    drop(n2);
    let n2b = Node::spawn_at(ports[1], dir2, &[urls[0].clone()]);
    wait_ready(&n2b).await;

    // sync_from_peers at boot must close the gap.
    wait_for_height(&urls[1], 6).await;
    assert_eq!(head(&urls[0]).await, head(&urls[1]).await);
}

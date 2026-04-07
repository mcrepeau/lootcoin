use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use lootcoin_core::{transaction::Transaction, wallet::Wallet};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

// ── Cooldown persistence ───────────────────────────────────────────────────────

/// redb table: bech32m address → unix timestamp (seconds) of last dispense.
const COOLDOWNS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("cooldowns");

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Write a cooldown entry to the database.  Logs a warning on failure but
/// never panics — the in-memory map remains the authoritative state for the
/// current process lifetime.
fn persist_cooldown(db: &Database, address: &str) {
    let ts = now_unix();
    let result = (|| -> Result<(), redb::Error> {
        let wtxn = db.begin_write()?;
        {
            let mut table = wtxn.open_table(COOLDOWNS_TABLE)?;
            table.insert(address, ts)?;
        }
        wtxn.commit()?;
        Ok(())
    })();
    if let Err(e) = result {
        warn!(address, "Failed to persist cooldown: {e}");
    }
}

/// Load all non-expired cooldown entries from the database and convert them
/// to `Instant` values so they slot directly into the existing in-memory map.
/// Entries older than `cooldown` are skipped — no point restoring them.
fn load_cooldowns(db: &Database, cooldown: Duration) -> HashMap<String, Instant> {
    let mut map = HashMap::new();
    let now_inst = Instant::now();
    let now_secs = now_unix();
    let cooldown_secs = cooldown.as_secs();

    let result = (|| -> Result<(), redb::Error> {
        let rtxn = db.begin_read()?;
        let table = match rtxn.open_table(COOLDOWNS_TABLE) {
            Ok(t) => t,
            // Table absent on a fresh database — treat as empty.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in table.iter()? {
            let (key, value): (redb::AccessGuard<&str>, redb::AccessGuard<u64>) = entry?;
            let addr = key.value().to_string();
            let stored_ts = value.value();
            let age_secs = now_secs.saturating_sub(stored_ts);
            if age_secs >= cooldown_secs {
                continue; // already expired — skip
            }
            // Reconstruct an Instant that is `age_secs` in the past.
            let last_instant = now_inst
                .checked_sub(Duration::from_secs(age_secs))
                .unwrap_or(now_inst);
            map.insert(addr, last_instant);
        }
        Ok(())
    })();

    if let Err(e) = result {
        warn!("Failed to load cooldowns from database: {e}");
    }

    map
}

// ── State ─────────────────────────────────────────────────────────────────────

struct AppState {
    wallet: Wallet,
    node_url: String,
    dispense_amount: u64,
    fee: u64,
    cooldown: Duration,
    cooldowns: Mutex<HashMap<String, Instant>>,
    cooldown_db: Database,
    client: reqwest::Client,
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct DispenseRequest {
    address: String,
}

#[derive(Serialize)]
struct DispenseResponse {
    message: String,
    txid: String,
    amount: u64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Deserialize)]
struct NodeBalanceResponse {
    spendable_balance: u64,
}

#[derive(Serialize)]
struct StatusResponse {
    faucet_address: String,
    /// None when the node is unreachable.
    spendable_balance: Option<u64>,
    dispense_amount: u64,
    fee: u64,
    cooldown_hours: u64,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_valid_address(addr: &str) -> bool {
    lootcoin_core::wallet::decode_address(addr).is_some()
}

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (code, Json(ErrorResponse { error: msg.into() }))
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// POST /faucet  { "address": "<bech32m address>" }
///
/// Sends `dispense_amount` coins to the requested address.  Rate-limited to
/// one dispense per address per cooldown period (default 24 h).
async fn handle_dispense(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DispenseRequest>,
) -> Result<Json<DispenseResponse>, (StatusCode, Json<ErrorResponse>)> {
    info!(address = %req.address, "Dispense request received");

    // 1. Validate address format.
    if !is_valid_address(&req.address) {
        warn!(address = %req.address, "Dispense denied: invalid address");
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Invalid address: must be a lootcoin bech32m address (loot1…).",
        ));
    }

    // 2. Cooldown check.
    {
        let cooldowns = state.cooldowns.lock().await;
        if let Some(&last) = cooldowns.get(&req.address) {
            let elapsed = last.elapsed();
            if elapsed < state.cooldown {
                let remaining_mins = (state.cooldown - elapsed).as_secs() / 60 + 1;
                warn!(
                    address = %req.address,
                    remaining_mins,
                    "Dispense denied: cooldown active"
                );
                return Err(err(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!(
                        "Address already funded. Try again in {} minutes.",
                        remaining_mins
                    ),
                ));
            }
        }
    }

    // 3. Verify the faucet has enough spendable balance.
    let faucet_addr = state.wallet.get_address();
    let balance: NodeBalanceResponse = state
        .client
        .get(format!("{}/balance/{}", state.node_url, faucet_addr))
        .send()
        .await
        .map_err(|e| {
            warn!("Dispense denied: could not reach node to check balance: {e}");
            err(StatusCode::SERVICE_UNAVAILABLE, "Could not reach node.")
        })?
        .json()
        .await
        .map_err(|e| {
            warn!("Dispense denied: unexpected balance response from node: {e}");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unexpected response from node.",
            )
        })?;

    if balance.spendable_balance < state.dispense_amount + state.fee {
        warn!(
            address = %req.address,
            have = balance.spendable_balance,
            need = state.dispense_amount + state.fee,
            "Dispense denied: insufficient faucet balance"
        );
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "Faucet is empty."));
    }

    // 4. Sign and submit the transaction. The random nonce is generated
    // internally by new_signed, ensuring each dispense has a unique signature.
    let tx = Transaction::new_signed(
        &state.wallet,
        req.address.clone(),
        state.dispense_amount,
        state.fee,
    );
    let txid = hex::encode(tx.txid());

    let resp = state
        .client
        .post(format!("{}/transactions/relay", state.node_url))
        .json(&tx)
        .send()
        .await
        .map_err(|e| {
            warn!(address = %req.address, "Dispense denied: could not reach node to submit tx: {e}");
            err(StatusCode::SERVICE_UNAVAILABLE, "Could not reach node.")
        })?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!(
            address = %req.address,
            %txid,
            "Dispense denied: node rejected transaction: {body}"
        );
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Node rejected transaction: {}", body),
        ));
    }

    // 5. Record cooldown only after confirmed submission, then persist.
    state
        .cooldowns
        .lock()
        .await
        .insert(req.address.clone(), Instant::now());
    persist_cooldown(&state.cooldown_db, &req.address);

    info!(
        address = %req.address,
        amount = state.dispense_amount,
        %txid,
        "Dispense sent"
    );

    Ok(Json(DispenseResponse {
        message: format!("Sent {} coins to your address.", state.dispense_amount),
        txid,
        amount: state.dispense_amount,
    }))
}

/// GET /status
///
/// Returns the faucet's configuration and current balance.  Useful for
/// monitoring and for the web UI to show whether the faucet is operational.
async fn handle_status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let faucet_addr = state.wallet.get_address();

    let spendable_balance = match state
        .client
        .get(format!("{}/balance/{}", state.node_url, faucet_addr))
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<NodeBalanceResponse>()
            .await
            .ok()
            .map(|b| b.spendable_balance),
        Err(_) => None,
    };

    Json(StatusResponse {
        faucet_address: faucet_addr,
        spendable_balance,
        dispense_amount: state.dispense_amount,
        fee: state.fee,
        cooldown_hours: state.cooldown.as_secs() / 3600,
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lootcoin_faucet=info".parse().unwrap()),
        )
        .init();

    // ── Config from environment variables ────────────────────────────────────

    let secret_key_hex = std::env::var("FAUCET_SECRET_KEY").expect(
        "FAUCET_SECRET_KEY must be set (64 hex chars representing the 32-byte wallet seed)",
    );

    let node_url = std::env::var("NODE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3000".to_string())
        .trim_end_matches('/')
        .to_string();

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3030".to_string())
        .parse()
        .expect("PORT must be a valid port number");

    let dispense_amount: u64 = std::env::var("DISPENSE_AMOUNT")
        .unwrap_or_else(|_| "500".to_string())
        .parse()
        .expect("DISPENSE_AMOUNT must be a positive integer");

    let fee: u64 = std::env::var("DISPENSE_FEE")
        .unwrap_or_else(|_| "2".to_string())
        .parse()
        .expect("DISPENSE_FEE must be a positive integer");

    let cooldown_secs: u64 = std::env::var("COOLDOWN_SECS")
        .unwrap_or_else(|_| "86400".to_string())
        .parse()
        .expect("COOLDOWN_SECS must be a positive integer");

    let cooldown_db_path =
        std::env::var("COOLDOWN_DB_PATH").unwrap_or_else(|_| "faucet_cooldowns.redb".to_string());

    // ── Restore faucet wallet from secret key ─────────────────────────────────

    let secret_bytes: [u8; 32] = hex::decode(&secret_key_hex)
        .expect("FAUCET_SECRET_KEY must be valid hex")
        .try_into()
        .expect("FAUCET_SECRET_KEY must be exactly 32 bytes (64 hex chars)");

    let wallet = Wallet::from_secret_key_bytes(secret_bytes);

    info!("Faucet address:   {}", wallet.get_address());
    info!("Node URL:         {}", node_url);
    info!("Dispense amount:  {} coins (+{} fee)", dispense_amount, fee);
    info!("Cooldown:         {} hours", cooldown_secs / 3600);
    info!("Cooldown DB:      {}", cooldown_db_path);

    // ── Open (or create) the cooldown database ────────────────────────────────

    let cooldown = Duration::from_secs(cooldown_secs);
    let cooldown_db =
        Database::create(&cooldown_db_path).expect("Failed to open cooldown database");

    let initial_cooldowns = load_cooldowns(&cooldown_db, cooldown);
    if !initial_cooldowns.is_empty() {
        info!(
            "Restored {} active cooldown(s) from database",
            initial_cooldowns.len()
        );
    }

    let state = Arc::new(AppState {
        wallet,
        node_url,
        dispense_amount,
        fee,
        cooldown,
        cooldowns: Mutex::new(initial_cooldowns),
        cooldown_db,
        client: reqwest::Client::new(),
    });

    let app = Router::new()
        .route("/faucet", post(handle_dispense))
        .route("/status", get(handle_status))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("Listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");
    info!("Server stopped cleanly");
}

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

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received — stopping server");
}

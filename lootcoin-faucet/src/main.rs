use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use lootcoin_core::{transaction::Transaction, wallet::Wallet};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

// ── State ─────────────────────────────────────────────────────────────────────

struct AppState {
    wallet: Wallet,
    node_url: String,
    dispense_amount: u64,
    fee: u64,
    cooldown: Duration,
    cooldowns: Mutex<HashMap<String, Instant>>,
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

/// POST /faucet  { "address": "<64-char hex>" }
///
/// Sends `dispense_amount` coins to the requested address.  Rate-limited to
/// one dispense per address per cooldown period (default 24 h).
async fn handle_dispense(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DispenseRequest>,
) -> Result<Json<DispenseResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 1. Validate address format.
    if !is_valid_address(&req.address) {
        warn!("Dispense rejected: invalid address '{}'", req.address);
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
                    "Dispense rejected: cooldown active for {} ({} min remaining)",
                    req.address, remaining_mins
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
            warn!("Could not reach node to check balance: {}", e);
            err(StatusCode::SERVICE_UNAVAILABLE, "Could not reach node.")
        })?
        .json()
        .await
        .map_err(|e| {
            warn!("Unexpected balance response from node: {}", e);
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unexpected response from node.",
            )
        })?;

    if balance.spendable_balance < state.dispense_amount + state.fee {
        warn!(
            "Dispense rejected: insufficient balance (have {}, need {})",
            balance.spendable_balance,
            state.dispense_amount + state.fee
        );
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "Faucet is empty."));
    }

    // 4. Sign and submit the transaction.
    let tx = Transaction::new_signed(
        &state.wallet,
        req.address.clone(),
        state.dispense_amount,
        state.fee,
    );

    let resp = state
        .client
        .post(format!("{}/transactions/relay", state.node_url))
        .json(&tx)
        .send()
        .await
        .map_err(|e| {
            warn!("Could not reach node to submit transaction: {}", e);
            err(StatusCode::SERVICE_UNAVAILABLE, "Could not reach node.")
        })?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!("Node rejected transaction for {}: {}", req.address, body);
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Node rejected transaction: {}", body),
        ));
    }

    // 5. Record cooldown only after confirmed submission.
    state
        .cooldowns
        .lock()
        .await
        .insert(req.address.clone(), Instant::now());
    info!(
        "Dispensed {} coins → {}",
        state.dispense_amount, req.address
    );

    Ok(Json(DispenseResponse {
        message: format!("Sent {} coins to your address.", state.dispense_amount),
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

    let state = Arc::new(AppState {
        wallet,
        node_url,
        dispense_amount,
        fee,
        cooldown: Duration::from_secs(cooldown_secs),
        cooldowns: Mutex::new(HashMap::new()),
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

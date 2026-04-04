use bip39::Mnemonic;
use clap::{Parser, Subcommand};
use lootcoin_core::{transaction::Transaction, wallet::Wallet};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "lc", about = "Lootcoin CLI wallet", version)]
struct Cli {
    /// Node base URL
    #[arg(long, env = "LOOTCOIN_NODE", default_value = "http://127.0.0.1:3000")]
    node: String,

    /// Path to wallet file
    #[arg(long, env = "LOOTCOIN_WALLET")]
    wallet: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate a new wallet and display the recovery phrase
    New,
    /// Import a wallet from a 12-word recovery phrase
    Import {
        /// Recovery phrase (12 words). Prompted if omitted.
        phrase: Option<String>,
    },
    /// Print the wallet address
    Address,
    /// Show confirmed and spendable balance
    Balance {
        /// Address to query (defaults to wallet address)
        address: Option<String>,
    },
    /// Sign and broadcast a transaction
    Send {
        /// Recipient address
        receiver: String,
        /// Amount in coins
        amount: u64,
        /// Transaction fee in coins (minimum 2)
        #[arg(long, default_value_t = 2)]
        fee: u64,
    },
    /// Show transaction history
    History {
        /// Address to query (defaults to wallet address)
        address: Option<String>,
        /// Number of transactions to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show current chain status
    Status,
}

// ── Wallet file ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct WalletFile {
    /// BIP-39 mnemonic; absent for wallets imported from raw hex.
    mnemonic: Option<String>,
    secret_key_hex: String,
}

fn default_wallet_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lootcoin")
        .join("wallet.json")
}

fn load_wallet_file(path: &PathBuf) -> Result<WalletFile, String> {
    let data = fs::read_to_string(path).map_err(|_| {
        format!(
            "No wallet found at {}.\nRun `lc new` or `lc import` first.",
            path.display()
        )
    })?;
    serde_json::from_str(&data).map_err(|e| format!("Corrupt wallet file: {}", e))
}

fn save_wallet_file(path: &PathBuf, wf: &WalletFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Cannot create wallet directory: {}", e))?;
    }
    fs::write(path, serde_json::to_string_pretty(wf).unwrap())
        .map_err(|e| format!("Cannot write wallet file: {}", e))
}

fn wallet_from_file(wf: &WalletFile) -> Result<Wallet, String> {
    let bytes = hex::decode(&wf.secret_key_hex)
        .map_err(|_| "Wallet file contains invalid hex".to_string())?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "Wallet file: secret key must be 32 bytes".to_string())?;
    Ok(Wallet::from_secret_key_bytes(arr))
}

fn key_from_mnemonic(m: &Mnemonic) -> [u8; 32] {
    let seed = m.to_seed("");
    seed[..32].try_into().expect("seed is at least 32 bytes")
}

// ── Node API types ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BalanceResponse {
    balance: u64,
    spendable_balance: u64,
    next_nonce: u64,
}

#[derive(Serialize)]
struct TxSubmission {
    sender: String,
    receiver: String,
    amount: u64,
    fee: u64,
    nonce: u64,
    public_key_hex: String,
    signature_hex: String,
}

#[derive(Deserialize)]
struct TxRecord {
    block_index: u64,
    sender: String,
    receiver: String,
    amount: u64,
    fee: u64,
}

#[derive(Deserialize)]
struct ChainHead {
    height: u64,
    difficulty: f64,
    avg_block_time_secs: f64,
    mempool_size: usize,
    pot: u64,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    let wallet_path = cli.wallet.unwrap_or_else(default_wallet_path);

    if let Err(e) = run(cli.command, &cli.node, &wallet_path) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn run(cmd: Commands, node: &str, wallet_path: &PathBuf) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    match cmd {
        Commands::New => cmd_new(wallet_path),
        Commands::Import { phrase } => cmd_import(wallet_path, phrase),
        Commands::Address => cmd_address(wallet_path),
        Commands::Balance { address } => cmd_balance(&client, node, wallet_path, address),
        Commands::Send {
            receiver,
            amount,
            fee,
        } => cmd_send(&client, node, wallet_path, receiver, amount, fee),
        Commands::History { address, limit } => {
            cmd_history(&client, node, wallet_path, address, limit)
        }
        Commands::Status => cmd_status(&client, node),
    }
}

// ── Command implementations ───────────────────────────────────────────────────

fn cmd_new(wallet_path: &PathBuf) -> Result<(), String> {
    let mut entropy = [0u8; 16]; // 128 bits → 12 words
    OsRng.fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy(&entropy).map_err(|e| format!("BIP-39 error: {}", e))?;
    let wallet = Wallet::from_secret_key_bytes(key_from_mnemonic(&mnemonic));

    save_wallet_file(
        wallet_path,
        &WalletFile {
            mnemonic: Some(mnemonic.to_string()),
            secret_key_hex: hex::encode(wallet.secret_key_bytes()),
        },
    )?;

    println!("Wallet saved to {}", wallet_path.display());
    println!();
    println!("Address:  {}", wallet.get_address());
    println!();
    println!("Recovery phrase — write this down and keep it safe:");
    println!();
    println!("  {}", mnemonic);
    println!();
    println!("Anyone with this phrase can spend your coins.");
    Ok(())
}

fn cmd_import(wallet_path: &PathBuf, phrase_arg: Option<String>) -> Result<(), String> {
    let phrase = match phrase_arg {
        Some(p) => p,
        None => {
            eprint!("Recovery phrase: ");
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            line.trim().to_string()
        }
    };

    let mnemonic =
        Mnemonic::parse(phrase.trim()).map_err(|e| format!("Invalid recovery phrase: {}", e))?;
    let wallet = Wallet::from_secret_key_bytes(key_from_mnemonic(&mnemonic));

    save_wallet_file(
        wallet_path,
        &WalletFile {
            mnemonic: Some(mnemonic.to_string()),
            secret_key_hex: hex::encode(wallet.secret_key_bytes()),
        },
    )?;

    println!("Wallet imported and saved to {}", wallet_path.display());
    println!("Address:  {}", wallet.get_address());
    Ok(())
}

fn cmd_address(wallet_path: &PathBuf) -> Result<(), String> {
    let wf = load_wallet_file(wallet_path)?;
    let wallet = wallet_from_file(&wf)?;
    println!("{}", wallet.get_address());
    Ok(())
}

fn cmd_balance(
    client: &reqwest::blocking::Client,
    node: &str,
    wallet_path: &PathBuf,
    address: Option<String>,
) -> Result<(), String> {
    let addr = resolve_address(wallet_path, address)?;
    let resp: BalanceResponse = get_json(client, &format!("{}/balance/{}", node, addr))?;

    println!("Address:   {}", addr);
    println!("Balance:   {} coins", resp.balance);
    if resp.spendable_balance < resp.balance {
        println!(
            "Spendable: {} coins  ({} pending in mempool)",
            resp.spendable_balance,
            resp.balance - resp.spendable_balance
        );
    } else {
        println!("Spendable: {} coins", resp.spendable_balance);
    }
    Ok(())
}

fn cmd_send(
    client: &reqwest::blocking::Client,
    node: &str,
    wallet_path: &PathBuf,
    receiver: String,
    amount: u64,
    fee: u64,
) -> Result<(), String> {
    if fee < 2 {
        return Err("Fee must be at least 2 coins".to_string());
    }

    let wf = load_wallet_file(wallet_path)?;
    let wallet = wallet_from_file(&wf)?;

    // Fetch balance to get next_nonce before signing.
    let bal: BalanceResponse =
        get_json(client, &format!("{}/balance/{}", node, wallet.get_address()))?;
    let nonce = bal.next_nonce;

    println!("From:   {}", wallet.get_address());
    println!("To:     {}", receiver);
    println!("Amount: {} coins", amount);
    println!("Fee:    {} coins  (total debit: {})", fee, amount + fee);

    // How many blocks before this tx becomes eligible for inclusion.
    let wait = (120u64 / fee).saturating_sub(1);
    if wait > 0 {
        println!(
            "Wait:   ~{} blocks (~{} min) before miners can include this tx",
            wait, wait
        );
    }
    println!();

    eprint!("Confirm? [y/N] ");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if !line.trim().eq_ignore_ascii_case("y") {
        println!("Cancelled.");
        return Ok(());
    }

    let tx = Transaction::new_signed(&wallet, receiver, amount, fee, nonce);
    let body = TxSubmission {
        sender: tx.sender,
        receiver: tx.receiver,
        amount: tx.amount,
        fee: tx.fee,
        nonce: tx.nonce,
        public_key_hex: hex::encode(tx.public_key),
        signature_hex: hex::encode(&tx.signature),
    };

    let resp = client
        .post(format!("{}/transactions", node))
        .json(&body)
        .send()
        .map_err(|e| format!("Node unreachable: {}", e))?;

    if resp.status().is_success() {
        println!("Transaction submitted.");
    } else {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        return Err(format!("Rejected ({}): {}", status, text));
    }
    Ok(())
}

fn cmd_history(
    client: &reqwest::blocking::Client,
    node: &str,
    wallet_path: &PathBuf,
    address: Option<String>,
    limit: usize,
) -> Result<(), String> {
    let addr = resolve_address(wallet_path, address)?;
    let url = format!("{}/address/{}/transactions?limit={}", node, addr, limit);
    let records: Vec<TxRecord> = get_json(client, &url)?;

    if records.is_empty() {
        println!("No transactions found for {}", addr);
        return Ok(());
    }

    println!(
        "{:<8}  {:<7}  {:>12}  {:>5}  COUNTERPART",
        "BLOCK", "TYPE", "AMOUNT", "FEE"
    );
    println!("{}", "─".repeat(72));

    for r in &records {
        let (label, sign, counterpart) = if r.sender.is_empty() {
            ("REWARD", "+", String::new())
        } else if r.sender == "lottery" {
            ("LOTTERY", "+", String::new())
        } else if r.sender == addr {
            ("OUT", "-", abbrev(&r.receiver))
        } else {
            ("IN", "+", abbrev(&r.sender))
        };

        println!(
            "{:<8}  {:<7}  {:>12}  {:>5}  {}",
            r.block_index,
            label,
            format!("{}{}", sign, r.amount),
            r.fee,
            counterpart
        );
    }
    Ok(())
}

fn cmd_status(client: &reqwest::blocking::Client, node: &str) -> Result<(), String> {
    let head: ChainHead = get_json(client, &format!("{}/chain/head", node))?;

    println!("Height:         {}", head.height);
    println!("Difficulty:     {:.2} bits", head.difficulty);
    println!("Avg block time: {:.1} s", head.avg_block_time_secs);
    println!("Mempool:        {} pending tx(s)", head.mempool_size);
    println!("Lottery pot:    {} coins", head.pot);
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn resolve_address(wallet_path: &PathBuf, explicit: Option<String>) -> Result<String, String> {
    match explicit {
        Some(a) => Ok(a),
        None => {
            let wf = load_wallet_file(wallet_path)?;
            Ok(wallet_from_file(&wf)?.get_address())
        }
    }
}

fn get_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<T, String> {
    client
        .get(url)
        .send()
        .map_err(|e| format!("Node unreachable: {}", e))?
        .error_for_status()
        .map_err(|e| format!("Node error: {}", e))?
        .json()
        .map_err(|e| format!("Unexpected response: {}", e))
}

/// Shorten a bech32m address to fit in history output.
fn abbrev(addr: &str) -> String {
    if addr.len() > 24 {
        format!("{}…{}", &addr[..10], &addr[addr.len() - 8..])
    } else {
        addr.to_string()
    }
}

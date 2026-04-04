use bip39::Mnemonic;
use lootcoin_core::transaction::Transaction;
use lootcoin_core::wallet::Wallet as LootcoinWallet;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::Serialize;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct Wallet {
    inner: LootcoinWallet,
    /// Present for wallets created via `new()` or `from_mnemonic()`.
    /// `None` for wallets imported via the legacy `from_secret_key_hex()` path.
    mnemonic: Option<String>,
}

/// JSON payload expected by POST /transactions.
/// Matches `SubmitTransactionRequest` in the node API — hex-encodes the byte
/// fields so they round-trip cleanly through JSON.
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

/// Derive an ed25519 secret key from a BIP-39 mnemonic using SLIP-0010.
///
/// Follows the SLIP-0010 spec for ed25519 HD key derivation:
///   1. Master key: HMAC-SHA512(Key="ed25519 seed", Data=bip39_seed)
///   2. Five hardened child derivations along BIP-44 path m/44'/4103'/0'/0'/0'
///      (all components hardened, as required by SLIP-0010 for ed25519)
///
/// Coin type 4103 (0x1007 — "loot" in leetspeak) is Lootcoin's registered
/// SLIP-0044 coin type. The first 32 bytes of each HMAC output are the child
/// private key; the last 32 bytes are the chain code passed to the next step.
fn key_from_mnemonic(m: &Mnemonic) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha512;
    type HmacSha512 = Hmac<Sha512>;

    let seed = m.to_seed("");

    // Step 1: SLIP-0010 master key
    let mut mac = HmacSha512::new_from_slice(b"ed25519 seed").expect("HMAC accepts any key size");
    mac.update(&seed);
    let result = mac.finalize().into_bytes();
    let mut key: [u8; 32] = result[..32].try_into().expect("32 bytes");
    let mut chain_code: [u8; 32] = result[32..].try_into().expect("32 bytes");

    // Step 2: BIP-44 path m/44'/4103'/0'/0'/0' (all hardened)
    const PATH: [u32; 5] = [
        0x8000_002C, // 44'
        0x8000_1007, // 4103' (Lootcoin)
        0x8000_0000, // 0'  account
        0x8000_0000, // 0'  change
        0x8000_0000, // 0'  address index
    ];

    for index in PATH {
        let mut mac =
            HmacSha512::new_from_slice(&chain_code).expect("HMAC accepts any key size");
        mac.update(&[0x00]);          // hardened child prefix
        mac.update(&key);             // parent private key
        mac.update(&index.to_be_bytes());
        let result = mac.finalize().into_bytes();
        key = result[..32].try_into().expect("32 bytes");
        chain_code = result[32..].try_into().expect("32 bytes");
    }

    key
}

impl Default for Wallet {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl Wallet {
    /// Create a new wallet.
    ///
    /// Generates 16 bytes of cryptographically secure entropy (via WebCrypto
    /// in the browser), derives a 12-word BIP-39 mnemonic from it, and uses
    /// the BIP-39 seed to construct the ed25519 keypair.  The mnemonic is the
    /// only thing the user needs to back up — `mnemonic_phrase()` returns it.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Wallet {
        let mut entropy = [0u8; 16]; // 128 bits → 12 words
        OsRng.fill_bytes(&mut entropy);
        let mnemonic =
            Mnemonic::from_entropy(&entropy).expect("16-byte entropy is always valid for BIP-39");
        let key_bytes = key_from_mnemonic(&mnemonic);
        Wallet {
            inner: LootcoinWallet::from_secret_key_bytes(key_bytes),
            mnemonic: Some(mnemonic.to_string()),
        }
    }

    /// Restore a wallet from a 12-word BIP-39 recovery phrase.
    #[wasm_bindgen]
    pub fn from_mnemonic(phrase: &str) -> Result<Wallet, JsValue> {
        let mnemonic = Mnemonic::parse(phrase.trim())
            .map_err(|e| JsValue::from_str(&format!("Invalid recovery phrase: {}", e)))?;
        let key_bytes = key_from_mnemonic(&mnemonic);
        Ok(Wallet {
            inner: LootcoinWallet::from_secret_key_bytes(key_bytes),
            mnemonic: Some(mnemonic.to_string()),
        })
    }

    /// Restore a wallet from a 32-byte secret key hex string (legacy import).
    ///
    /// Wallets imported this way have no associated mnemonic — `mnemonic_phrase()`
    /// will return `undefined` for them.
    #[wasm_bindgen]
    pub fn from_secret_key_hex(secret_hex: &str) -> Result<Wallet, JsValue> {
        let bytes = hex::decode(secret_hex).map_err(|_| JsValue::from_str("invalid secret hex"))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| JsValue::from_str("secret must be exactly 32 bytes"))?;
        Ok(Wallet {
            inner: LootcoinWallet::from_secret_key_bytes(arr),
            mnemonic: None,
        })
    }

    /// Return the 12-word BIP-39 recovery phrase, or `undefined` if this wallet
    /// was imported from a raw secret key.
    #[wasm_bindgen]
    pub fn mnemonic_phrase(&self) -> Option<String> {
        self.mnemonic.clone()
    }

    /// Export the secret key as a 32-byte hex string (for advanced / legacy use).
    #[wasm_bindgen]
    pub fn secret_key_hex(&self) -> String {
        hex::encode(self.inner.secret_key_bytes())
    }

    #[wasm_bindgen]
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.inner.get_public_key_bytes())
    }

    #[wasm_bindgen]
    pub fn address(&self) -> String {
        self.inner.get_address()
    }

    /// Build and sign a transaction, returning a JSON string ready for
    /// POST /transactions. `nonce` must equal `next_nonce` from the node's
    /// GET /balance/{address} response.
    #[wasm_bindgen]
    pub fn sign_transaction(&self, receiver: &str, amount: u64, fee: u64, nonce: u64) -> String {
        let tx = Transaction::new_signed(&self.inner, receiver.to_string(), amount, fee, nonce);
        let submission = TxSubmission {
            sender: tx.sender,
            receiver: tx.receiver,
            amount: tx.amount,
            fee: tx.fee,
            nonce: tx.nonce,
            public_key_hex: hex::encode(tx.public_key),
            signature_hex: hex::encode(tx.signature),
        };
        serde_json::to_string(&submission).unwrap()
    }
}

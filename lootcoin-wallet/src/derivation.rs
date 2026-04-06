use bip39::Mnemonic;
use hmac::{Hmac, Mac};
use sha2::Sha512;

type HmacSha512 = Hmac<Sha512>;

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
pub fn key_from_mnemonic(m: &Mnemonic) -> [u8; 32] {
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
        let mut mac = HmacSha512::new_from_slice(&chain_code).expect("HMAC accepts any key size");
        mac.update(&[0x00]); // hardened child prefix
        mac.update(&key); // parent private key
        mac.update(&index.to_be_bytes());
        let result = mac.finalize().into_bytes();
        key = result[..32].try_into().expect("32 bytes");
        chain_code = result[32..].try_into().expect("32 bytes");
    }

    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use lootcoin_core::wallet::Wallet;

    /// Regression test: SLIP-0010 derivation must produce a stable, known address
    /// for a fixed mnemonic.  If this breaks, CLI and WASM wallets diverge again.
    #[test]
    fn known_vector() {
        let phrase =
            "addict rookie smile vote knock yellow camera room suggest when endless winner";
        let mnemonic = Mnemonic::parse(phrase).unwrap();
        let wallet = Wallet::from_secret_key_bytes(key_from_mnemonic(&mnemonic));
        assert_eq!(
            wallet.get_address(),
            "loot1hd9xz3rfdflaegwlvpqhg6rpftvwl4mt678kg9kf6nnengm8celsw7dnkm",
        );
    }
}

/// Developer-attested checkpoint heights and their canonical block hashes.
///
/// These are the trust anchors for peer snapshot sync. A node bootstrapping
/// from a peer will only accept a snapshot if the block hash at that height
/// matches an entry here — same model as Bitcoin's hardcoded checkpoints.
///
/// The chain operator adds entries here as the chain matures and the blocks
/// at those heights are considered irreversible.
///
/// Hash format: lowercase hex, no "0x" prefix.
pub const TRUSTED_CHECKPOINTS: &[(u64, &str)] = &[
    // Add entries as the chain matures, e.g.:
    // (10_000, "0000a3f1..."),
];

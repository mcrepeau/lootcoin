//! GPU mining via a pre-compiled CubeHash-16-32-256 CUDA kernel.
//!
//! The kernel is compiled by `build.rs` from `kernels/cubehash_mine.cu` using
//! `nvcc` and embedded as PTX text at compile time.  `GpuMiner::new()` loads it
//! onto device 0 once; subsequent calls to `mine()` reuse the loaded function.

use cudarc::driver::{CudaDevice, CudaFunction, CudaSlice, LaunchAsync, LaunchConfig};
use cudarc::nvrtc::Ptx;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Threads per CUDA block.
const BLOCK_SIZE: u32 = 256;

/// Nonces tested per kernel launch (~4M — at 10 GH/s that's ~0.4 ms per batch).
const BATCH: u64 = 1 << 22;

// Helper: map DriverError (which doesn't impl std::error::Error) to anyhow::Error.
macro_rules! cuda {
    ($expr:expr, $msg:literal) => {
        $expr.map_err(|e| anyhow::anyhow!("{}: {:?}", $msg, e))?
    };
    ($expr:expr) => {
        $expr.map_err(|e| anyhow::anyhow!("CUDA error: {:?}", e))?
    };
}

pub struct GpuMiner {
    dev: Arc<CudaDevice>,
    func: CudaFunction,
}

/// Build the 104-byte bincode header template from a block's fields.
/// Exposed so the test module (and callers other than main) can use it.
pub fn make_header_template(block: &lootcoin_core::block::Block) -> [u8; 104] {
    let mut h = [0u8; 104];
    h[0..8].copy_from_slice(&block.index.to_le_bytes());
    h[8..16].copy_from_slice(&(block.previous_hash.len() as u64).to_le_bytes());
    h[16..48].copy_from_slice(&block.previous_hash);
    h[48..56].copy_from_slice(&block.timestamp.to_le_bytes());
    h[56..64].copy_from_slice(&block.nonce.to_le_bytes());
    h[64..72].copy_from_slice(&(block.tx_root.len() as u64).to_le_bytes());
    h[72..104].copy_from_slice(&block.tx_root);
    h
}

// CudaDevice is Arc-wrapped; CudaFunction holds a stable device pointer.
// Both are safe to send across threads when used from one thread at a time,
// which spawn_blocking guarantees.
unsafe impl Send for GpuMiner {}
unsafe impl Sync for GpuMiner {}

impl GpuMiner {
    /// Initialise CUDA device 0 and load the mining kernel.
    pub fn new() -> anyhow::Result<Self> {
        let dev = cuda!(CudaDevice::new(0), "Failed to initialise CUDA device 0");

        // PTX was compiled by build.rs from kernels/cubehash_mine.cu
        let ptx_src = include_str!(concat!(env!("OUT_DIR"), "/cubehash_mine.ptx"));
        let ptx = Ptx::from_src(ptx_src);
        cuda!(
            dev.load_ptx(ptx, "lootcoin_miner", &["mine_cubehash"]),
            "Failed to load mining PTX onto device"
        );

        let func = dev
            .get_func("lootcoin_miner", "mine_cubehash")
            .ok_or_else(|| anyhow::anyhow!("mine_cubehash not found after loading PTX"))?;

        Ok(Self { dev, func })
    }

    /// Mine in batches until a valid nonce is found or `cancel`/`shutdown` is set.
    ///
    /// `tmpl` is the 104-byte bincode block header; the kernel patches the nonce
    /// at byte offset 56 per-thread.
    ///
    /// Returns `Some((winning_nonce, total_hashes_tried))` on success, or `None`
    /// if cancelled before a solution was found.
    pub fn mine(
        &self,
        tmpl: &[u8; 104],
        nonce_start: u64,
        difficulty: f64,
        cancel: &AtomicBool,
        shutdown: &AtomicBool,
    ) -> anyhow::Result<Option<(u64, u64)>> {
        let tmpl_dev: CudaSlice<u8> = cuda!(
            self.dev.htod_sync_copy(tmpl.as_ref()),
            "htod_sync_copy tmpl"
        );

        let mut out_nonce: CudaSlice<u64> =
            cuda!(self.dev.alloc_zeros::<u64>(1), "alloc out_nonce");
        let mut out_found: CudaSlice<i32> =
            cuda!(self.dev.alloc_zeros::<i32>(1), "alloc out_found");

        let grid = (BATCH / BLOCK_SIZE as u64) as u32;
        let cfg = LaunchConfig {
            grid_dim: (grid, 1, 1),
            block_dim: (BLOCK_SIZE, 1, 1),
            shared_mem_bytes: 0,
        };

        let mut nonce_base = nonce_start;
        let mut total: u64 = 0;

        loop {
            if cancel.load(Ordering::Relaxed) || shutdown.load(Ordering::Relaxed) {
                return Ok(None);
            }

            unsafe {
                cuda!(
                    self.func.clone().launch(
                        cfg,
                        (
                            &tmpl_dev,
                            nonce_base,
                            difficulty,
                            &mut out_nonce,
                            &mut out_found
                        ),
                    ),
                    "kernel launch"
                );
            }
            cuda!(self.dev.synchronize(), "synchronize");
            total = total.saturating_add(BATCH);

            let found: Vec<i32> = cuda!(self.dev.dtoh_sync_copy(&out_found), "dtoh out_found");
            if found[0] != 0 {
                let nonce_vec: Vec<u64> =
                    cuda!(self.dev.dtoh_sync_copy(&out_nonce), "dtoh out_nonce");
                return Ok(Some((nonce_vec[0], total)));
            }

            nonce_base = nonce_base.wrapping_add(BATCH);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lootcoin_core::block::{meets_difficulty, Block};
    use std::sync::atomic::AtomicBool;

    fn test_block() -> Block {
        let txs = vec![];
        let tx_root = Block::compute_tx_root(&txs).expect("infallible");
        Block {
            index: 1,
            previous_hash: vec![0u8; 32],
            timestamp: 1_700_000_000,
            nonce: 0,
            tx_root,
            transactions: txs,
            hash: vec![],
        }
    }

    /// Mine a block at 8-bit difficulty on the GPU, then recompute the hash on
    /// the CPU using the returned nonce.  If the GPU CubeHash implementation is
    /// correct the CPU hash will meet the same difficulty target.
    ///
    /// Run with: cargo test --features gpu -- --nocapture
    #[test]
    fn gpu_nonce_validates_on_cpu() {
        // 8 bits: first byte of hash must be 0x00.
        // Expected ~256 nonce tries — completes in well under a second on any GPU.
        let difficulty = 8.0_f64;

        let mut block = test_block();
        let tmpl = make_header_template(&block);

        let cancel = AtomicBool::new(false);
        let shutdown = AtomicBool::new(false);

        let miner = GpuMiner::new().expect("CUDA device 0 must be available for this test");

        let (gpu_nonce, tries) = miner
            .mine(&tmpl, 0, difficulty, &cancel, &shutdown)
            .expect("GPU mine() returned an error")
            .expect("GPU mine() was cancelled unexpectedly");

        println!("GPU found nonce={gpu_nonce} after {tries} hashes");

        // Recompute on CPU — this is the ground-truth check.
        block.nonce = gpu_nonce;
        let cpu_hash = block.calculate_hash().expect("infallible");

        assert!(
            meets_difficulty(&cpu_hash, difficulty),
            "GPU nonce {gpu_nonce} does NOT produce a valid hash on CPU\nhash = {cpu_hash:?}"
        );
    }
}

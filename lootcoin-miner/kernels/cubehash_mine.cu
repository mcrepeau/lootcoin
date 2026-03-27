/*
 * cubehash_mine.cu — CubeHash-16-32-256 GPU mining kernel for Lootcoin
 *
 * Each thread tests one nonce value against the current difficulty target.
 * The 104-byte bincode block header template has the following layout:
 *
 *   bytes  0.. 7   index          (u64 LE)
 *   bytes  8..15   prev_hash len  (u64 LE = 32)
 *   bytes 16..47   prev_hash      (32 raw bytes)
 *   bytes 48..55   timestamp      (u64 LE)
 *   bytes 56..63   nonce          (u64 LE)  ← varied per thread
 *   bytes 64..71   tx_root len    (u64 LE = 32)
 *   bytes 72..103  tx_root        (32 raw bytes)
 *
 * The CubeHash implementation exactly mirrors the scalar Rust backend in the
 * cubehash-0.4.x crate (state layout, load_bytes word reversal, transmute
 * output reversal, finalization flag position) so hashes match bit-for-bit.
 *
 * Build (PTX for use with cudarc):
 *   nvcc -O3 -arch=sm_86 -ptx -o cubehash_mine.ptx cubehash_mine.cu
 *
 * Kernel: mine_cubehash
 *   tmpl       — pointer to 104-byte header template in device memory
 *   nonce_base — starting nonce for this batch (thread adds its linear ID)
 *   difficulty — fractional bit difficulty (matches Rust f64)
 *   out_nonce  — written with the winning nonce on success
 *   out_found  — atomically set to 1 on success; pre-initialise to 0 on host
 */

#include <stdint.h>

/* ── Constants ───────────────────────────────────────────────────────────── */

#define HEADER_LEN   104
#define NONCE_OFFSET  56   /* byte offset of nonce u64 LE in header */
#define HASH_BYTES    32   /* CubeHash-256 digest length */
#define CH_ROUNDS     16   /* rounds per block */

/* ── Helpers ─────────────────────────────────────────────────────────────── */

__device__ __forceinline__ uint32_t rotl32(uint32_t x, int n)
{
    return (x << n) | (x >> (32 - n));
}

/* Load 4 bytes from an arbitrary byte offset as a little-endian uint32. */
__device__ __forceinline__ uint32_t le32(const uint8_t *p)
{
    return (uint32_t)p[0]
         | ((uint32_t)p[1] <<  8)
         | ((uint32_t)p[2] << 16)
         | ((uint32_t)p[3] << 24);
}

/* ── Difficulty check ────────────────────────────────────────────────────── */
/*
 * Mirrors lootcoin_core::block::meets_difficulty (Rust f64 semantics).
 * Uses double precision throughout to match the Rust f64 arithmetic exactly.
 */
__device__ bool meets_difficulty_gpu(const uint8_t hash[HASH_BYTES], double bits)
{
    if (bits <= 0.0) return true;

    int    n         = (int)(bits / 8.0);
    double remainder = bits - (double)n * 8.0;

    for (int i = 0; i < n; i++)
        if (hash[i] != 0) return false;

    if (remainder == 0.0) return true;

    double threshold = pow(2.0, 8.0 - remainder);
    for (int i = n; i < HASH_BYTES; i++) {
        double b       = (double)(unsigned int)hash[i];
        double t_floor = floor(threshold);
        if (b < t_floor) return true;
        if (b > t_floor) return false;
        double frac = threshold - t_floor;
        if (frac == 0.0) return false;
        threshold = frac * 256.0;
    }
    return (threshold - floor(threshold)) > 0.0;
}

/* ── CubeHash state ──────────────────────────────────────────────────────── */
/*
 * The state is 8 vectors of 4×u32, stored flat in s[0..31]:
 *
 *   s[ 0.. 3] = x0   s[ 4.. 7] = x1   s[ 8..11] = x2   s[12..15] = x3
 *   s[16..19] = x4   s[20..23] = x5   s[24..27] = x6   s[28..31] = x7
 *
 * Indices within each vector follow the Rust U32x4([a,b,c,d]) convention,
 * i.e. element 0 is the first field, NOT the high word.
 *
 * Permutations:
 *   permute_badc([a,b,c,d]) = [b,a,d,c]   (swap adjacent pairs)
 *   permute_cdab([a,b,c,d]) = [c,d,a,b]   (swap 64-bit halves)
 */

/* ── One CubeHash round ──────────────────────────────────────────────────── */

__device__ __forceinline__ void cubehash_round(uint32_t s[32])
{
    /* ── Half-round 1 ── */

    /* Snapshot x0..x3 before any writes. */
    uint32_t a0=s[0],  a1=s[1],  a2=s[2],  a3=s[3];
    uint32_t b0=s[4],  b1=s[5],  b2=s[6],  b3=s[7];
    uint32_t c0=s[8],  c1=s[9],  c2=s[10], c3=s[11];
    uint32_t d0=s[12], d1=s[13], d2=s[14], d3=s[15];

    /* x4 = x0 + permute_badc(x4); same for x5..x7 */
    { uint32_t t0=s[16],t1=s[17],t2=s[18],t3=s[19];
      s[16]=a0+t1; s[17]=a1+t0; s[18]=a2+t3; s[19]=a3+t2; }
    { uint32_t t0=s[20],t1=s[21],t2=s[22],t3=s[23];
      s[20]=b0+t1; s[21]=b1+t0; s[22]=b2+t3; s[23]=b3+t2; }
    { uint32_t t0=s[24],t1=s[25],t2=s[26],t3=s[27];
      s[24]=c0+t1; s[25]=c1+t0; s[26]=c2+t3; s[27]=c3+t2; }
    { uint32_t t0=s[28],t1=s[29],t2=s[30],t3=s[31];
      s[28]=d0+t1; s[29]=d1+t0; s[30]=d2+t3; s[31]=d3+t2; }

    /*
     * x0 = rotl(x2, 7) ^ x4    (t0 = rotl(old_x2, 7))
     * x1 = rotl(x3, 7) ^ x5    (t1 = rotl(old_x3, 7))
     * x2 = rotl(x0, 7) ^ x6    (t2 = rotl(old_x0, 7))
     * x3 = rotl(x1, 7) ^ x7    (t3 = rotl(old_x1, 7))
     * (x4..x7 already updated above)
     */
    s[ 0]=rotl32(c0,7)^s[16]; s[ 1]=rotl32(c1,7)^s[17];
    s[ 2]=rotl32(c2,7)^s[18]; s[ 3]=rotl32(c3,7)^s[19];
    s[ 4]=rotl32(d0,7)^s[20]; s[ 5]=rotl32(d1,7)^s[21];
    s[ 6]=rotl32(d2,7)^s[22]; s[ 7]=rotl32(d3,7)^s[23];
    s[ 8]=rotl32(a0,7)^s[24]; s[ 9]=rotl32(a1,7)^s[25];
    s[10]=rotl32(a2,7)^s[26]; s[11]=rotl32(a3,7)^s[27];
    s[12]=rotl32(b0,7)^s[28]; s[13]=rotl32(b1,7)^s[29];
    s[14]=rotl32(b2,7)^s[30]; s[15]=rotl32(b3,7)^s[31];

    /* ── Half-round 2 ── */

    /* Snapshot x0..x3 after the first half-round. */
    uint32_t e0=s[0],  e1=s[1],  e2=s[2],  e3=s[3];
    uint32_t f0=s[4],  f1=s[5],  f2=s[6],  f3=s[7];
    uint32_t g0=s[8],  g1=s[9],  g2=s[10], g3=s[11];
    uint32_t h0=s[12], h1=s[13], h2=s[14], h3=s[15];

    /* x4 = x0 + permute_cdab(x4); same for x5..x7 */
    { uint32_t t0=s[16],t1=s[17],t2=s[18],t3=s[19];
      s[16]=e0+t2; s[17]=e1+t3; s[18]=e2+t0; s[19]=e3+t1; }
    { uint32_t t0=s[20],t1=s[21],t2=s[22],t3=s[23];
      s[20]=f0+t2; s[21]=f1+t3; s[22]=f2+t0; s[23]=f3+t1; }
    { uint32_t t0=s[24],t1=s[25],t2=s[26],t3=s[27];
      s[24]=g0+t2; s[25]=g1+t3; s[26]=g2+t0; s[27]=g3+t1; }
    { uint32_t t0=s[28],t1=s[29],t2=s[30],t3=s[31];
      s[28]=h0+t2; s[29]=h1+t3; s[30]=h2+t0; s[31]=h3+t1; }

    /*
     * x0 = rotl(x1, 11) ^ x4
     * x1 = rotl(x0, 11) ^ x5    (old x0)
     * x2 = rotl(x3, 11) ^ x6
     * x3 = rotl(x2, 11) ^ x7    (old x2)
     */
    s[ 0]=rotl32(f0,11)^s[16]; s[ 1]=rotl32(f1,11)^s[17];
    s[ 2]=rotl32(f2,11)^s[18]; s[ 3]=rotl32(f3,11)^s[19];
    s[ 4]=rotl32(e0,11)^s[20]; s[ 5]=rotl32(e1,11)^s[21];
    s[ 6]=rotl32(e2,11)^s[22]; s[ 7]=rotl32(e3,11)^s[23];
    s[ 8]=rotl32(h0,11)^s[24]; s[ 9]=rotl32(h1,11)^s[25];
    s[10]=rotl32(h2,11)^s[26]; s[11]=rotl32(h3,11)^s[27];
    s[12]=rotl32(g0,11)^s[28]; s[13]=rotl32(g1,11)^s[29];
    s[14]=rotl32(g2,11)^s[30]; s[15]=rotl32(g3,11)^s[31];
}

/* Run CH_ROUNDS rounds. */
__device__ __forceinline__ void cubehash_rounds(uint32_t s[32])
{
    #pragma unroll
    for (int i = 0; i < CH_ROUNDS; i++)
        cubehash_round(s);
}

/* ── Block absorption ────────────────────────────────────────────────────── */
/*
 * XOR a 32-byte block into x0/x1, then run CH_ROUNDS rounds.
 *
 * The Rust load_bytes() reverses the four u32 words within each 16-byte half:
 *   element[0] ← LE32(bytes[12..16])
 *   element[1] ← LE32(bytes[ 8..12])
 *   element[2] ← LE32(bytes[ 4.. 8])
 *   element[3] ← LE32(bytes[ 0.. 4])
 * so the first byte of the block XORs into the LOW byte of element[3], etc.
 */
__device__ __forceinline__ void absorb_block(uint32_t s[32], const uint8_t blk[32])
{
    /* x0 ^= load_bytes(blk[0..16]) */
    s[0] ^= le32(blk + 12);
    s[1] ^= le32(blk +  8);
    s[2] ^= le32(blk +  4);
    s[3] ^= le32(blk +  0);
    /* x1 ^= load_bytes(blk[16..32]) */
    s[4] ^= le32(blk + 28);
    s[5] ^= le32(blk + 24);
    s[6] ^= le32(blk + 20);
    s[7] ^= le32(blk + 16);

    cubehash_rounds(s);
}

/* ── State initialisation ────────────────────────────────────────────────── */
/*
 * CubeHash-16-32-256, revision 3:
 *   x0 = U32x4([0, ROUNDS=16, BLOCKSIZE=32, hashlen/8=32])
 *   x1..x7 = zero
 * Then run irounds/ROUNDS = 16/16 = 1 group of CH_ROUNDS rounds.
 */
__device__ __forceinline__ void cubehash_init(uint32_t s[32])
{
    /* x0 */
    s[0]=0; s[1]=16; s[2]=32; s[3]=32;
    /* x1..x7 */
    #pragma unroll
    for (int i = 4; i < 32; i++) s[i] = 0;

    cubehash_rounds(s);
}

/* ── Hash extraction ─────────────────────────────────────────────────────── */
/*
 * The Rust transmute() reverses the four words within each 16-byte half:
 *   output bytes  0.. 3 ← element[3].to_le_bytes()
 *   output bytes  4.. 7 ← element[2].to_le_bytes()
 *   output bytes  8..11 ← element[1].to_le_bytes()
 *   output bytes 12..15 ← element[0].to_le_bytes()
 * This applies to x0 (bytes 0-15) and x1 (bytes 16-31).
 */
__device__ __forceinline__ void extract_hash(const uint32_t s[32],
                                             uint8_t hash[HASH_BYTES])
{
#define EMIT_LE(dst, word)                      \
    hash[dst+0] = (uint8_t)((word)      );      \
    hash[dst+1] = (uint8_t)((word) >>  8);      \
    hash[dst+2] = (uint8_t)((word) >> 16);      \
    hash[dst+3] = (uint8_t)((word) >> 24)

    /* x0 reversed */
    EMIT_LE( 0, s[3]); EMIT_LE( 4, s[2]); EMIT_LE( 8, s[1]); EMIT_LE(12, s[0]);
    /* x1 reversed */
    EMIT_LE(16, s[7]); EMIT_LE(20, s[6]); EMIT_LE(24, s[5]); EMIT_LE(28, s[4]);

#undef EMIT_LE
}

/* ── Full hash for the fixed 104-byte Lootcoin header ───────────────────── */
/*
 * Absorbs 4 blocks (3 full + 1 padded) then finalises:
 *   block 1 : header[  0.. 31]
 *   block 2 : header[ 32.. 63]
 *   block 3 : header[ 64.. 95]
 *   block 4 : header[ 96..103] + 0x80 + 23 zero bytes
 *
 * Finalisation:
 *   x7[1] ^= 1   (set_finalize_flag: U32x4::new(0,1,0,0), so element[1] = 1)
 *   run frounds/ROUNDS = 32/16 = 2 groups of CH_ROUNDS rounds
 */
__device__ void cubehash256_header(const uint8_t header[HEADER_LEN],
                                   uint8_t hash[HASH_BYTES])
{
    uint32_t s[32];
    cubehash_init(s);

    absorb_block(s, header +  0);
    absorb_block(s, header + 32);
    absorb_block(s, header + 64);

    /* Build and absorb the padded final block. */
    uint8_t pad[32];
    #pragma unroll
    for (int i = 0; i < 8; i++)  pad[i] = header[96 + i];
    pad[8] = 0x80;
    #pragma unroll
    for (int i = 9; i < 32; i++) pad[i] = 0;
    absorb_block(s, pad);

    /* Finalisation flag: x7[1] ^= 1  →  s[29] ^= 1 */
    s[29] ^= 1;

    /* Two groups of CH_ROUNDS finalization rounds */
    cubehash_rounds(s);
    cubehash_rounds(s);

    extract_hash(s, hash);
}

/* ── Mining kernel ───────────────────────────────────────────────────────── */
/*
 * Launch with enough threads to cover the desired batch size.
 * Typical invocation: <<<grid, 256>>> where grid = batch_size / 256.
 *
 * Parameters
 *   tmpl       Device pointer to a 104-byte header with an arbitrary nonce
 *              at NONCE_OFFSET.  Each thread patches its own nonce before
 *              hashing — the template is never modified.
 *   nonce_base Base nonce for this batch.  Thread global_id adds its index.
 *   difficulty Fractional bit difficulty (same value as f64 on the Rust side).
 *   out_nonce  Written with the winning nonce when a solution is found.
 *   out_found  Pre-initialised to 0 by the host.  Atomically set to 1 on
 *              the first solution found; further solutions are suppressed.
 */
extern "C" __global__
void mine_cubehash(const uint8_t *tmpl,
                   uint64_t       nonce_base,
                   double         difficulty,
                   uint64_t      *out_nonce,
                   int           *out_found)
{
    /* Bail early if another thread already found a solution. */
    if (atomicAdd(out_found, 0) != 0) return;

    uint64_t tid   = (uint64_t)blockIdx.x * blockDim.x + threadIdx.x;
    uint64_t nonce = nonce_base + tid;

    /* Copy the template into local registers and patch the nonce. */
    uint8_t header[HEADER_LEN];
    #pragma unroll
    for (int i = 0; i < HEADER_LEN; i++) header[i] = tmpl[i];

    /* Write nonce as little-endian u64 at NONCE_OFFSET. */
    #pragma unroll
    for (int i = 0; i < 8; i++)
        header[NONCE_OFFSET + i] = (uint8_t)(nonce >> (i * 8));

    /* Hash and check. */
    uint8_t hash[HASH_BYTES];
    cubehash256_header(header, hash);

    if (meets_difficulty_gpu(hash, difficulty)) {
        if (atomicExch(out_found, 1) == 0)
            *out_nonce = nonce;
    }
}

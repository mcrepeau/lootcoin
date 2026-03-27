fn main() {
    if std::env::var("CARGO_FEATURE_GPU").is_ok() {
        compile_cuda_kernel();
    }
}

fn compile_cuda_kernel() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let kernel = "kernels/cubehash_mine.cu";
    let ptx_out = format!("{}/cubehash_mine.ptx", out_dir);

    println!("cargo:rerun-if-changed={}", kernel);

    // Allow overriding SM version via CUDA_ARCH env var (e.g. CUDA_ARCH=sm_86).
    // Defaults to sm_75 (Turing / RTX 2000 series) which is forward-compatible
    // with all later architectures via the driver's JIT compiler.
    let arch = std::env::var("CUDA_ARCH").unwrap_or_else(|_| "sm_75".to_string());

    let status = std::process::Command::new("nvcc")
        .args([
            "-O3",
            "-ptx",
            &format!("-arch={}", arch),
            "-o",
            &ptx_out,
            kernel,
        ])
        .status()
        .expect(
            "nvcc not found — install the CUDA Toolkit and ensure nvcc is on PATH, \
             or build without --features gpu",
        );

    assert!(
        status.success(),
        "nvcc failed — check CUDA Toolkit installation and kernel source"
    );
}

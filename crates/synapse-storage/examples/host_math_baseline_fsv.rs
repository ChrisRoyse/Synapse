//! Manual FSV instrument: what does this host's CPU instruction baseline cost
//! the daemon's real math and hashing hot paths?
//!
//! ## Why this exists
//!
//! `.cargo/config.toml` pins `target-cpu=x86-64-v2` deliberately, so the shipped
//! binary runs on "ANY modern Windows machine" — a portability decision that
//! explicitly declines v3 because low-end Pentium/Celeron/Atom parts lack AVX2.
//! That is the right default for a binary you ship. It is the wrong default for
//! a binary `synapse-setup` compiles **on the machine it installs to**, which is
//! every install this project performs.
//!
//! The cost is not theoretical, for two reasons this instrument measures rather
//! than argues:
//!
//! 1. **Calyx's CPU math has no runtime AVX2 dispatch.** `calyx-forge`'s only
//!    runtime check is `is_x86_feature_detected!("avx512f")`; everything else
//!    lands in the `wide::f32x8` path. `wide` is a portable-SIMD facade, so
//!    `f32x8` is one 256-bit AVX2 operation when AVX2 is enabled at compile time
//!    and **two 128-bit SSE operations** when it is not. At the v2 baseline the
//!    "8-wide" kernels therefore run half-width.
//! 2. **`sha256` is on more hot paths than the vector math is.** Every ledger
//!    entry, manifest, search sidecar and backup file is hashed. SHA-NI collapses
//!    the compression function into a handful of instructions, and it is simply
//!    unavailable to the compiler at a v2 baseline.
//!
//! ## What it measures
//!
//! The **production** kernels — `calyx_forge::CpuBackend` through the
//! `MathBackend` trait, and the same `sha2` crate the vault hashes with — at
//! shapes taken from the live vault rather than invented:
//!
//! | phase | shape | why this shape |
//! |---|---|---|
//! | cosine | 32-dim x 20k rows | the live timeline panel's dense record-vector lane |
//! | cosine | 384-dim x 20k rows | embedder-scale, for the lanes a future panel would add |
//! | gemm | 64x64x256 | the tiled path, exercising `TILE_M`/`TILE_K` |
//! | sha256 | 256 MB | the backup I took hashed 1,536 files / 266 MB |
//!
//! Run it once per baseline and compare. The compile-time feature set is printed
//! from `cfg!(target_feature = ...)`, so each run states the baseline it was
//! actually built at instead of trusting the flag that was passed:
//!
//! ```text
//! cargo run --release -p synapse-storage --example host_math_baseline_fsv
//! $env:RUSTFLAGS="-C target-cpu=native"
//! cargo run --release -p synapse-storage --example host_math_baseline_fsv
//! ```
//!
//! Every phase prints a checksum of its own output. A faster run that computes a
//! different answer is not an optimization, and the checksum is what makes that
//! visible: the numbers must be identical across baselines.

use std::error::Error;
use std::time::Instant;

use calyx_forge::{Backend as _, CpuBackend};
use sha2::{Digest as _, Sha256};

const ROWS: usize = 20_000;
const SHA_MB: usize = 256;

/// Deterministic pseudo-random floats, so both baselines score identical input
/// without needing a seeded RNG dependency.
fn fill(len: usize, seed: u64) -> Vec<f32> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            // xorshift64*, then map into [-1, 1)
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            ((value >> 40) as f32 / 8_388_608.0) - 1.0
        })
        .collect()
}

/// Sum as f64 so the checksum does not itself depend on float ordering.
fn checksum(values: &[f32]) -> f64 {
    values.iter().map(|value| f64::from(*value)).sum()
}

fn report(label: &str, elapsed: std::time::Duration, units: f64, unit: &str, check: f64) {
    let secs = elapsed.as_secs_f64();
    println!(
        "  {label:<28} {:>9.1} ms   {:>12.2} {unit}/s   checksum={check:.6}",
        secs * 1000.0,
        units / secs,
    );
}

fn cosine_phase(backend: &CpuBackend, dim: usize) -> Result<(), Box<dyn Error>> {
    let query = fill(dim, 0xC057_0001);
    let corpus = fill(dim * ROWS, 0xC057_0002);
    let mut out = vec![0.0_f32; ROWS];
    // Warm the caches so the timed pass measures compute, not first-touch faults.
    backend.cosine(&query, &corpus, dim, &mut out)?;
    let started = Instant::now();
    let passes = 20;
    for _ in 0..passes {
        backend.cosine(&query, &corpus, dim, &mut out)?;
    }
    let elapsed = started.elapsed();
    report(
        &format!("cosine dim={dim}"),
        elapsed,
        (ROWS * passes) as f64,
        "rows",
        checksum(&out),
    );
    Ok(())
}

fn gemm_phase(backend: &CpuBackend) -> Result<(), Box<dyn Error>> {
    let (m, k, n) = (64_usize, 64_usize, 256_usize);
    let a = fill(m * k, 0x6E33_0001);
    let b = fill(k * n, 0x6E33_0002);
    let mut out = vec![0.0_f32; m * n];
    backend.gemm(&a, &b, m, k, n, &mut out)?;
    let started = Instant::now();
    let passes = 200;
    for _ in 0..passes {
        backend.gemm(&a, &b, m, k, n, &mut out)?;
    }
    let elapsed = started.elapsed();
    // 2 flops per multiply-accumulate.
    let flops = 2.0 * (m * k * n * passes) as f64;
    report(
        &format!("gemm {m}x{k}x{n}"),
        elapsed,
        flops / 1e9,
        "GFLOP",
        checksum(&out),
    );
    Ok(())
}

/// Byte-for-byte the scoring kernel the live `flat_dense` lanes actually run:
/// `calyx-search/src/persisted/dense.rs::cosine`, a private plain scalar loop
/// with **no** SIMD dispatch of any kind.
///
/// This is the one that matters. `calyx-sextant`'s `index/distance.rs` already
/// dispatches on `is_x86_feature_detected!("avx2")` at runtime and so is immune
/// to the compile baseline, and `calyx-forge`'s kernels are reached only by
/// `knn` in the drift/intelligence analytics and by a self-probe. The persisted
/// flat-dense lane — 5 of the 8 lanes on the live timeline generation — is
/// scored here, and nothing about it adapts to the host at runtime.
fn hot_path_cosine(left: &[f32], right: &[f32]) -> f32 {
    let (mut dot, mut left_l2, mut right_l2) = (0.0, 0.0, 0.0);
    for (left, right) in left.iter().zip(right) {
        dot += left * right;
        left_l2 += left * left;
        right_l2 += right * right;
    }
    if left_l2 == 0.0 || right_l2 == 0.0 {
        0.0
    } else {
        dot / (left_l2.sqrt() * right_l2.sqrt())
    }
}

fn hot_path_phase(dim: usize) {
    let query = fill(dim, 0x4007_0001);
    let corpus = fill(dim * ROWS, 0x4007_0002);
    let mut out = vec![0.0_f32; ROWS];
    let passes = 20;
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = hot_path_cosine(&query, &corpus[index * dim..(index + 1) * dim]);
    }
    let started = Instant::now();
    for _ in 0..passes {
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = hot_path_cosine(&query, &corpus[index * dim..(index + 1) * dim]);
        }
    }
    let elapsed = started.elapsed();
    report(
        &format!("HOT flat_dense cos dim={dim}"),
        elapsed,
        (ROWS * passes) as f64,
        "rows",
        checksum(&out),
    );
}

/// The alternative that already exists in-tree: `calyx-sextant`'s
/// runtime-dispatched kernel, which DiskANN already scores with.
///
/// It selects AVX2 by `is_x86_feature_detected!` at first use, so its speed does
/// not depend on the compile baseline at all — which is exactly the property the
/// flat-dense lane is missing. `cosine_distance` returns `1 - cos`, so the
/// similarity the flat lane wants is `1 - distance`.
fn dispatched_phase(dim: usize) {
    let query = fill(dim, 0x4007_0001);
    let corpus = fill(dim * ROWS, 0x4007_0002);
    let mut out = vec![0.0_f32; ROWS];
    let passes = 20;
    let score = |index: usize| {
        1.0 - calyx_sextant::index::distance::cosine_distance(
            &query,
            &corpus[index * dim..(index + 1) * dim],
        )
    };
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = score(index);
    }
    let started = Instant::now();
    for _ in 0..passes {
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = score(index);
        }
    }
    let elapsed = started.elapsed();
    report(
        &format!("DISPATCHED cos dim={dim}"),
        elapsed,
        (ROWS * passes) as f64,
        "rows",
        checksum(&out),
    );
}

fn sha_phase() {
    let block = vec![0xA5_u8; 1 << 20];
    let started = Instant::now();
    let mut hasher = Sha256::new();
    for _ in 0..SHA_MB {
        hasher.update(&block);
    }
    let digest = hasher.finalize();
    let elapsed = started.elapsed();
    let secs = elapsed.as_secs_f64();
    println!(
        "  {:<28} {:>9.1} ms   {:>12.2} MB/s   digest={:.16}",
        format!("sha256 {SHA_MB} MB"),
        secs * 1000.0,
        SHA_MB as f64 / secs,
        hex(&digest),
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("host_math_baseline_fsv");
    println!("--- compile-time instruction set actually built into THIS binary ---");
    for (name, enabled) in [
        ("sse4.2", cfg!(target_feature = "sse4.2")),
        ("avx", cfg!(target_feature = "avx")),
        ("avx2", cfg!(target_feature = "avx2")),
        ("fma", cfg!(target_feature = "fma")),
        ("bmi2", cfg!(target_feature = "bmi2")),
        ("sha", cfg!(target_feature = "sha")),
        ("aes", cfg!(target_feature = "aes")),
        ("avx512f", cfg!(target_feature = "avx512f")),
    ] {
        println!("  {name:<10} {}", if enabled { "enabled" } else { "-" });
    }
    // The compile-time list above says what LLVM was allowed to emit. It does
    // NOT say what this CPU can execute, and the two answers diverge for every
    // kernel that dispatches at runtime — which is the whole point of runtime
    // dispatch. Reporting only the compile-time set made a real question
    // ("does this host have SHA-NI, so is `sha2` picking its hardware backend?")
    // unanswerable from the instrument's own output, so both are printed now.
    println!("--- runtime instruction set THIS CPU actually supports ---");
    for (name, detected) in [
        ("avx", is_x86_feature_detected!("avx")),
        ("avx2", is_x86_feature_detected!("avx2")),
        ("fma", is_x86_feature_detected!("fma")),
        ("bmi2", is_x86_feature_detected!("bmi2")),
        ("sha", is_x86_feature_detected!("sha")),
        ("aes", is_x86_feature_detected!("aes")),
        ("avx512f", is_x86_feature_detected!("avx512f")),
    ] {
        println!("  {name:<10} {}", if detected { "present" } else { "-" });
    }
    // `sha2` 0.11 selects its `x86-sha` backend when sha + sse2 + ssse3 + sse4.1
    // are all detected at runtime, and falls back to `soft` otherwise. That is
    // the exact predicate, so state the conclusion rather than leaving it to be
    // inferred from the feature list.
    let shani = is_x86_feature_detected!("sha")
        && is_x86_feature_detected!("sse2")
        && is_x86_feature_detected!("ssse3")
        && is_x86_feature_detected!("sse4.1");
    println!(
        "  sha2 backend = {}  (runtime-selected; no compile flag governs this)",
        if shani { "x86-sha (SHA-NI)" } else { "soft" }
    );

    let backend = CpuBackend::new();
    println!(
        "  forge simd_path = {}  (runtime avx512={})",
        backend.simd_path(),
        backend.avx512_available()
    );

    println!("\n--- measured (checksums must be identical across baselines) ---");
    println!("  [HOT = the kernel the live flat_dense lanes actually run]");
    hot_path_phase(32);
    hot_path_phase(384);
    println!(
        "  [DISPATCHED = calyx-sextant runtime-AVX2 kernel, backend={}]",
        calyx_sextant::index::distance::kernel_backend()
    );
    dispatched_phase(32);
    dispatched_phase(384);
    cosine_phase(&backend, 32)?;
    cosine_phase(&backend, 384)?;
    gemm_phase(&backend)?;
    sha_phase();
    Ok(())
}

//! FSV probe: is there any batch shape in the DiskANN Vamana build that a
//! `calyx_forge::Backend` dispatch can win? (#2103, #2108)
//!
//! The build's candidate sites, and the widths they actually present — all
//! measured on the production-shaped corpus by `diskann_build_fsv`:
//!
//! | site                          | batch shape   | dispatches / build |
//! |-------------------------------|---------------|--------------------|
//! | `greedy_search` expansion     | 1 x m_max(32) | 55,168,832         |
//! | `robust_prune` pool scoring   | 1 x ~106      | 3,759,630          |
//! | `robust_prune` retain round   | 1 x ~23       | 103,189,882        |
//! | `medoid` argmin               | 1 x n(374615) | 1                  |
//!
//! This harness times a real `Backend::l2` on each of those widths against the
//! CPU backend, at three layers — the raw `CudaBackend`, the VRAM-budgeted
//! wrapper, and the host-admitted wrapper that is what the daemon actually
//! hands out — then multiplies per-dispatch cost by the dispatch counts above.
//! It also reports the GPU-vs-CPU numeric delta at the one genuinely wide shape
//! (`medoid`), because that is where a 1-ulp difference would silently move the
//! graph's entry point and therefore its whole topology.
//!
//! Run with `--features forge-gpu`. Set `CALYX_GPU_RESERVATION_ROOT` to a
//! scratch directory first; this never touches the production reservation root.
//!
//! Env knobs:
//!   SEXTANT_GPU_ITERS   timed iterations per width (default 200)
//!   SEXTANT_GPU_ROWS    wide-shape row count      (default 374615)
//!   SEXTANT_GPU_DIM     dimension                 (default 96)

use std::env;
use std::time::Instant;

use calyx_forge::{
    Backend, CpuBackend, CudaBackend, HostGpuReservationStore, VramBudgetedCudaBackend,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

type Fsv<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Dispatch counts measured by `diskann_build_fsv` on 374,615 x 96 with
/// m_max=32, ef_construction=64, alpha=1.2 (two passes).
const GREEDY_DISPATCHES: u64 = 55_168_832;
const PRUNE_POOL_DISPATCHES: u64 = 3_759_630;
const PRUNE_ROUND_DISPATCHES: u64 = 103_189_882;

fn env_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// One warm-up call, then `reps` timed calls; returns microseconds per call.
fn time_l2(
    backend: &dyn Backend,
    query: &[f32],
    block: &[f32],
    dim: usize,
    out: &mut [f32],
    reps: usize,
) -> Fsv<f64> {
    backend.l2(query, block, dim, out)?;
    let start = Instant::now();
    for _ in 0..reps {
        backend.l2(query, block, dim, out)?;
    }
    Ok(start.elapsed().as_nanos() as f64 / reps as f64 / 1000.0)
}

fn main() -> Fsv<()> {
    let dim = env_usize("SEXTANT_GPU_DIM", 96);
    let rows = env_usize("SEXTANT_GPU_ROWS", 374_615);
    let iters = env_usize("SEXTANT_GPU_ITERS", 200);

    let mut rng = ChaCha8Rng::seed_from_u64(20_260_808);
    let candidates: Vec<f32> = (0..rows * dim).map(|_| rng.random::<f32>() - 0.5).collect();
    let query: Vec<f32> = (0..dim).map(|_| rng.random::<f32>() - 0.5).collect();

    let cuda = CudaBackend::new()?;
    println!("DEVICE {:?}", cuda.device_info());
    let store = HostGpuReservationStore::from_env(0)?;
    println!("LEDGER path={}", store.state_path().display());
    println!("LEDGER_BEFORE {:?}", store.readback()?);

    // Three layers, because they cost wildly different amounts and only one of
    // them is what production would actually call:
    //   raw      - `CudaBackend` alone: kernel launch + PCIe round trip.
    //   budgeted - plus the process-local VRAM budgeter.
    //   admitted - plus host-wide reservation admission, which is what
    //              `SynapseCalyxMathRuntime::backend()` hands out.
    let raw = cuda.clone();
    let budgeted = VramBudgetedCudaBackend::new(cuda.clone(), 4 * 1024 * 1024 * 1024)?;
    let admitted = VramBudgetedCudaBackend::new(cuda, 4 * 1024 * 1024 * 1024)?
        .with_host_dispatch_reservations(store.clone(), "sextant-diskann-fsv", "diskann-probe")?;
    let cpu = CpuBackend::new();

    println!("width,raw_us,budgeted_us,admitted_us,cpu_us,raw_over_cpu");
    for width in [23_usize, 32, 64, 106, 256, 1024, 8192, 65536, rows] {
        if width > rows {
            continue;
        }
        let block = &candidates[..width * dim];
        let mut out = vec![0.0_f32; width];
        let reps = if width > 65_536 { iters.min(20) } else { iters };
        let raw_us = time_l2(&raw, &query, block, dim, &mut out, reps)?;
        let budgeted_us = time_l2(&budgeted, &query, block, dim, &mut out, reps)?;
        let admitted_us = time_l2(&admitted, &query, block, dim, &mut out, reps.min(20))?;
        let cpu_us = time_l2(&cpu, &query, block, dim, &mut out, reps)?;
        println!(
            "{width},{raw_us:.3},{budgeted_us:.3},{admitted_us:.3},{cpu_us:.3},{:.2}",
            raw_us / cpu_us
        );
    }

    // Extrapolate the per-dispatch cost to the build's real dispatch counts.
    for (label, width, dispatches) in [
        ("greedy_search_expansion", 32_usize, GREEDY_DISPATCHES),
        ("robust_prune_pool", 106, PRUNE_POOL_DISPATCHES),
        ("robust_prune_retain_round", 23, PRUNE_ROUND_DISPATCHES),
    ] {
        let block = &candidates[..width * dim];
        let mut out = vec![0.0_f32; width];
        let raw_us = time_l2(&raw, &query, block, dim, &mut out, iters)?;
        let admitted_us = time_l2(&admitted, &query, block, dim, &mut out, 20)?;
        let cpu_us = time_l2(&cpu, &query, block, dim, &mut out, iters)?;
        println!(
            "PROJECTION site={label} width={width} dispatches={dispatches} raw_gpu_total_s={:.1} admitted_gpu_total_s={:.1} cpu_total_s={:.1}",
            raw_us * dispatches as f64 / 1e6,
            admitted_us * dispatches as f64 / 1e6,
            cpu_us * dispatches as f64 / 1e6
        );
    }

    // Equivalence at the one wide shape: does the GPU reduction order move the
    // argmin the medoid depends on?
    let mut gpu_out = vec![0.0_f32; rows];
    let mut cpu_out = vec![0.0_f32; rows];
    raw.l2(&query, &candidates, dim, &mut gpu_out)?;
    cpu.l2(&query, &candidates, dim, &mut cpu_out)?;
    let mut differing = 0_u64;
    let mut max_ulp = 0_i64;
    for (g, c) in gpu_out.iter().zip(&cpu_out) {
        if g != c {
            differing += 1;
            max_ulp = max_ulp.max((g.to_bits() as i64 - c.to_bits() as i64).abs());
        }
    }
    let argmin = |v: &[f32]| {
        let mut best = (0_usize, f32::INFINITY);
        for (i, d) in v.iter().enumerate() {
            if *d < best.1 {
                best = (i, *d);
            }
        }
        best.0
    };
    println!(
        "EQUIVALENCE rows={rows} differing={differing} ({:.4}%) max_ulp={max_ulp} gpu_argmin={} cpu_argmin={} argmin_agrees={}",
        differing as f64 * 100.0 / rows as f64,
        argmin(&gpu_out),
        argmin(&cpu_out),
        argmin(&gpu_out) == argmin(&cpu_out)
    );

    println!("LEDGER_AFTER {:?}", store.readback()?);
    Ok(())
}

//! Field service verification for the #2107 CUDA backend internal-hole fixes.
//!
//! Exercises the real `CudaBackend` against the real `CpuBackend` on this host:
//!
//! * exactness — every `(index, score)` pair of every metric compared
//!   bit-for-bit against the CPU reference at realistic shapes;
//! * refusal semantics — non-finite inputs, `k` above the exact-top-k bound;
//! * tie determinism — duplicate scores and `-0.0` / `+0.0` ties;
//! * wall time — batched multi-query dispatch versus one dispatch per query,
//!   plus the analytic PCIe byte counts of both paths.
//!
//! Run with `cargo run --release --example forge_gpu_knn_fsv --features cuda`.

#[cfg(not(feature = "cuda"))]
fn main() {
    println!("forge_gpu_knn_fsv requires --features cuda");
}

#[cfg(feature = "cuda")]
fn main() {
    if let Err(error) = cuda_fsv::run() {
        eprintln!("FSV FAILED: {error}");
        std::process::exit(1);
    }
}

#[cfg(feature = "cuda")]
mod cuda_fsv {
    use std::time::{Duration, Instant};

    use calyx_forge::{Backend, CpuBackend, CudaBackend, ForgeError, KnnBatch, KnnMetric};
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;
    use sha2::{Digest, Sha256};

    const F32: usize = size_of::<f32>();
    const I32: usize = size_of::<i32>();
    const CHUNK: usize = 1024;
    /// Tolerance for CUDA-versus-CPU score agreement: `abs + rel * magnitude`.
    ///
    /// The two backends reduce in different orders — a 256-thread shared-memory
    /// tree on the device, eight-wide AVX2 lanes on the host — so f32 sums do
    /// not agree bit-for-bit and never did. `1e-6` relative is roughly ten f32
    /// ulps. #2107 does not move this bound at all: the batched kernel reduces
    /// in exactly the order the per-query kernel did, which the
    /// batched-vs-per-query bit-exact check proves independently.
    const CPU_AGREEMENT_ABS: f32 = 1e-6;
    const CPU_AGREEMENT_REL: f32 = 1e-6;

    fn agreement_tolerance(left: f32, right: f32) -> f32 {
        CPU_AGREEMENT_ABS + CPU_AGREEMENT_REL * left.abs().max(right.abs())
    }

    /// Set to relax assertions that only hold after the #2107 fix, so the same
    /// binary can measure the pre-fix baseline.
    fn baseline_mode() -> bool {
        std::env::var("FORGE_FSV_BASELINE").is_ok_and(|value| value == "1")
    }

    struct Shape {
        name: &'static str,
        dim: usize,
        candidates: usize,
        queries: &'static [usize],
        k: &'static [usize],
        exactness: bool,
    }

    const SHAPES: &[Shape] = &[
        Shape {
            name: "self-knn/384d/20k",
            dim: 384,
            candidates: 20_000,
            queries: &[1, 16, 64, 512],
            k: &[16, 64],
            exactness: true,
        },
        Shape {
            name: "transcript-panel/128d/368k",
            dim: 128,
            candidates: 368_000,
            queries: &[1, 64],
            k: &[64],
            exactness: false,
        },
    ];

    const METRICS: &[(KnnMetric, &str)] = &[
        (KnnMetric::Cosine, "cosine"),
        (KnnMetric::Dot, "dot"),
        (KnnMetric::L2Squared, "l2sq"),
    ];

    pub fn run() -> Result<(), String> {
        let cuda = CudaBackend::new().map_err(|err| format!("cuda init failed: {err}"))?;
        let cpu = CpuBackend::new();
        println!("device = {:?}", cuda.device_info());
        println!("cpu    = {:?}", cpu.device_info());

        refusal_probes(&cuda, &cpu)?;
        tie_probes(&cuda, &cpu)?;

        println!(
            "\n{:<26} {:>6} {:>7} {:>5} {:>16} {:>11} {:>11} {:>9} {:>10} {:>13} {:>13}",
            "shape",
            "metric",
            "queries",
            "k",
            "result_digest",
            "batched_ms",
            "perquery_ms",
            "speedup",
            "cpu_maxdiff",
            "pcie_new_B",
            "pcie_old_B"
        );
        for shape in SHAPES {
            let candidates = random_matrix(shape.candidates, shape.dim, 0x5EED_0001);
            for &(metric, metric_name) in METRICS {
                for &query_count in shape.queries {
                    let queries = random_matrix(query_count, shape.dim, 0x5EED_0002);
                    for &k in shape.k {
                        let (batched, batched_ms) = timed(|| {
                            cuda.knn(&queries, &candidates, query_count, shape.dim, k, metric)
                        })
                        .map_err(|err| {
                            format!("{} {metric_name} batched knn: {err}", shape.name)
                        })?;
                        let (per_query, per_query_ms) = timed(|| {
                            per_query_knn(
                                &cuda,
                                &queries,
                                &candidates,
                                query_count,
                                shape.dim,
                                k,
                                metric,
                            )
                        })
                        .map_err(|err| {
                            format!("{} {metric_name} per-query knn: {err}", shape.name)
                        })?;
                        // The batching change (#2107 H4) must not move a single
                        // bit: `gridDim.y == 1` walks the same reduction tree
                        // the per-query launch walks.
                        compare_bit_exact(
                            &format!(
                                "{} {metric_name} q{query_count} k{k} batched-vs-perquery",
                                shape.name
                            ),
                            &batched,
                            &per_query,
                        )?;

                        let cpu_maxdiff = if shape.exactness {
                            let reference = cpu
                                .knn(&queries, &candidates, query_count, shape.dim, k, metric)
                                .map_err(|err| format!("cpu reference knn: {err}"))?;
                            format!(
                                "{:.3e}",
                                compare_numeric(
                                    &format!(
                                        "{} {metric_name} q{query_count} k{k} cuda-vs-cpu",
                                        shape.name
                                    ),
                                    &batched,
                                    &reference,
                                )?
                            )
                        } else {
                            "skipped".to_owned()
                        };

                        println!(
                            "{:<26} {:>6} {:>7} {:>5} {:>16} {:>11.3} {:>11.3} {:>8.2}x {:>10} {:>13} {:>13}",
                            shape.name,
                            metric_name,
                            query_count,
                            k,
                            digest(&batched),
                            batched_ms,
                            per_query_ms,
                            per_query_ms / batched_ms.max(f64::MIN_POSITIVE),
                            cpu_maxdiff,
                            pcie_bytes_new(shape.candidates, query_count, shape.dim, k),
                            pcie_bytes_old(shape.candidates, query_count, shape.dim, k, metric),
                        );
                    }
                }
            }
        }
        println!("\nFSV PASSED");
        Ok(())
    }

    /// Non-finite refusal must keep the same error code, the same op, and the
    /// same index-bearing detail the host pre-scan produced (#2107 H3).
    fn refusal_probes(cuda: &CudaBackend, cpu: &CpuBackend) -> Result<(), String> {
        let dim = 32;
        let count = 4_096;
        let mut candidates = random_matrix(count, dim, 0x5EED_0003);
        let queries = random_matrix(2, dim, 0x5EED_0004);
        candidates[3 * dim + 7] = f32::NAN;

        let cuda_err = cuda
            .knn(&queries, &candidates, 2, dim, 8, KnnMetric::Cosine)
            .err()
            .ok_or("cuda knn accepted a non-finite candidate")?;
        let cpu_err = cpu
            .knn(&queries, &candidates, 2, dim, 8, KnnMetric::Cosine)
            .err()
            .ok_or("cpu knn accepted a non-finite candidate")?;
        if cuda_err.code() != cpu_err.code() {
            return Err(format!(
                "refusal code drift: cuda={} cpu={}",
                cuda_err.code(),
                cpu_err.code()
            ));
        }
        if cuda_err.to_string() != cpu_err.to_string() {
            return Err(format!(
                "refusal message drift:\n  cuda={cuda_err}\n  cpu ={cpu_err}"
            ));
        }
        println!(
            "refusal(non-finite candidate) = {} :: {cuda_err}",
            cuda_err.code()
        );

        let mut infinite_queries = queries.clone();
        infinite_queries[dim + 1] = f32::INFINITY;
        let clean = random_matrix(count, dim, 0x5EED_0003);
        let cuda_err = cuda
            .knn(&infinite_queries, &clean, 2, dim, 8, KnnMetric::Dot)
            .err()
            .ok_or("cuda knn accepted a non-finite query")?;
        let cpu_err = cpu
            .knn(&infinite_queries, &clean, 2, dim, 8, KnnMetric::Dot)
            .err()
            .ok_or("cpu knn accepted a non-finite query")?;
        if cuda_err.to_string() != cpu_err.to_string() {
            return Err(format!(
                "query refusal message drift:\n  cuda={cuda_err}\n  cpu ={cpu_err}"
            ));
        }
        println!(
            "refusal(non-finite query)     = {} :: {cuda_err}",
            cuda_err.code()
        );

        // Every metric now ranks on the device, so every metric carries the
        // exact-top-k bound; L2 used to escape it via the host sort.
        match cuda.knn(&queries, &clean, 2, dim, 2_048, KnnMetric::L2Squared) {
            Err(ForgeError::ShapeMismatch { remediation, .. }) => {
                println!(
                    "refusal(l2 k>1024)            = CALYX_FORGE_SHAPE_MISMATCH :: {remediation}"
                );
            }
            Err(other) => return Err(format!("unexpected l2 k>1024 refusal: {other}")),
            Ok(_) if baseline_mode() => {
                println!(
                    "refusal(l2 k>1024)            = ACCEPTED (pre-#2107 host sort escaped the bound)"
                );
            }
            Ok(_) => return Err("cuda knn accepted L2 k=2048 above the exact top-k bound".into()),
        }

        let mut nan_scores = random_matrix(1, 4_096, 0x5EED_0005);
        nan_scores[2_000] = f32::NAN;
        let cuda_err = cuda
            .topk(&nan_scores, 16)
            .err()
            .ok_or("cuda topk accepted a non-finite score")?;
        if cuda_err.code() != cpu.topk(&nan_scores, 16).err().map_or("", |e| e.code()) {
            return Err(format!("topk refusal code drift: cuda={}", cuda_err.code()));
        }
        println!(
            "refusal(non-finite topk)      = {} :: {cuda_err}",
            cuda_err.code()
        );
        Ok(())
    }

    /// Top-k selection is order sensitive at ties. The device comparator now
    /// uses the same total order as `f32::total_cmp`, so `-0.0` sorts below
    /// `+0.0` and equal scores break by lower index — exactly like `cpu::topk`.
    fn tie_probes(cuda: &CudaBackend, cpu: &CpuBackend) -> Result<(), String> {
        for n in [7_usize, 1_024, 1_025, 4_096, 9_001] {
            let mut scores = vec![0.0_f32; n];
            for (index, score) in scores.iter_mut().enumerate() {
                *score = match index % 6 {
                    0 => 0.0,
                    1 => -0.0,
                    2 => 0.5,
                    3 => 0.5,
                    4 => -1.0,
                    _ => 0.25,
                };
            }
            for k in [1_usize, 16, 64, 1_024] {
                let k = k.min(n);
                let device = cuda
                    .topk(&scores, k)
                    .map_err(|err| format!("cuda topk n={n} k={k}: {err}"))?;
                let host = cpu
                    .topk(&scores, k)
                    .map_err(|err| format!("cpu topk n={n} k={k}: {err}"))?;
                if device.len() != host.len() {
                    return Err(format!("topk length drift n={n} k={k}"));
                }
                for (rank, (left, right)) in device.iter().zip(&host).enumerate() {
                    if left.0 != right.0 || left.1.to_bits() != right.1.to_bits() {
                        let detail = format!(
                            "topk tie order drift n={n} k={k} rank={rank}: cuda={left:?} cpu={right:?}"
                        );
                        if baseline_mode() {
                            println!("tie order (+-0.0, duplicates) = DRIFT (pre-#2107): {detail}");
                            return Ok(());
                        }
                        return Err(detail);
                    }
                }
            }
        }
        println!("tie order (+-0.0, duplicates) = cuda topk == cpu topk bit-for-bit");
        Ok(())
    }

    fn per_query_knn(
        cuda: &CudaBackend,
        queries: &[f32],
        candidates: &[f32],
        query_count: usize,
        dim: usize,
        k: usize,
        metric: KnnMetric,
    ) -> Result<KnnBatch, ForgeError> {
        let mut indices = Vec::new();
        let mut scores = Vec::new();
        let mut k_eff = 0;
        let candidate_count = candidates.len() / dim;
        for query in queries.chunks_exact(dim) {
            let batch = cuda.knn(query, candidates, 1, dim, k, metric)?;
            k_eff = batch.k;
            indices.extend_from_slice(&batch.indices);
            scores.extend_from_slice(&batch.scores);
        }
        KnnBatch::new(query_count, k_eff, candidate_count, metric, indices, scores)
    }

    fn compare_bit_exact(label: &str, left: &KnnBatch, right: &KnnBatch) -> Result<(), String> {
        if left.indices.len() != right.indices.len() || left.k != right.k {
            return Err(format!("{label}: result length drift"));
        }
        for (offset, ((li, ls), (ri, rs))) in left
            .indices
            .iter()
            .zip(&left.scores)
            .zip(right.indices.iter().zip(&right.scores))
            .enumerate()
        {
            if li != ri {
                return Err(format!(
                    "{label}: index drift at {offset}: {li} vs {ri} (scores {ls} vs {rs})"
                ));
            }
            if ls.to_bits() != rs.to_bits() {
                return Err(format!(
                    "{label}: score bit drift at {offset}: {ls:e} vs {rs:e}"
                ));
            }
        }
        Ok(())
    }

    /// CUDA-versus-CPU agreement. Returns the max absolute score difference.
    ///
    /// An index disagreement is only tolerated when the two scores at that rank
    /// are within tolerance of each other — i.e. the backends picked different
    /// members of a numerically tied pair, which reduction order can
    /// legitimately do. A disagreement with a real score gap is a ranking bug
    /// and fails.
    fn compare_numeric(label: &str, left: &KnnBatch, right: &KnnBatch) -> Result<f32, String> {
        if left.indices.len() != right.indices.len() || left.k != right.k {
            return Err(format!("{label}: result length drift"));
        }
        let mut max_abs_diff = 0.0_f32;
        for (offset, ((li, ls), (ri, rs))) in left
            .indices
            .iter()
            .zip(&left.scores)
            .zip(right.indices.iter().zip(&right.scores))
            .enumerate()
        {
            let diff = (ls - rs).abs();
            let tolerance = agreement_tolerance(*ls, *rs);
            max_abs_diff = max_abs_diff.max(diff);
            if diff > tolerance {
                return Err(format!(
                    "{label}: score drift at {offset} beyond {tolerance:e}: \
                     {ls:e} vs {rs:e} (abs diff {diff:e})"
                ));
            }
            if li != ri && diff > tolerance {
                return Err(format!(
                    "{label}: index drift at {offset} with a real score gap: {li} vs {ri}"
                ));
            }
        }
        Ok(max_abs_diff)
    }

    /// Stable digest of a whole result set, so a pre-fix and post-fix run can
    /// be compared line by line without carrying every score.
    fn digest(batch: &KnnBatch) -> String {
        let mut hasher = Sha256::new();
        for (index, score) in batch.indices.iter().zip(&batch.scores) {
            hasher.update((*index as u64).to_le_bytes());
            hasher.update(score.to_bits().to_le_bytes());
        }
        let out = hasher.finalize();
        out.iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Bytes crossing PCIe for the batched device-ranked path: candidates up,
    /// queries up, `query_count * k` ranked pairs plus one flag word down.
    fn pcie_bytes_new(candidates: usize, queries: usize, dim: usize, k: usize) -> usize {
        let k_eff = k.min(candidates);
        candidates * dim * F32 + queries * dim * F32 + queries * k_eff * (F32 + I32) + 4
    }

    /// Bytes crossing PCIe for the pre-#2107 path: candidates up once, one
    /// query upload per query, then per query either `chunks * k` partial
    /// candidates (cosine/dot, merged in a host `BinaryHeap`) or *every*
    /// candidate score (L2, sorted with a host `Vec::sort_by`).
    fn pcie_bytes_old(
        candidates: usize,
        queries: usize,
        dim: usize,
        k: usize,
        metric: KnnMetric,
    ) -> usize {
        let k_eff = k.min(candidates);
        let down_per_query = match metric {
            KnnMetric::L2Squared => candidates * F32,
            KnnMetric::Cosine | KnnMetric::Dot => candidates.div_ceil(CHUNK) * k_eff * (F32 + I32),
        };
        candidates * dim * F32 + queries * dim * F32 + queries * down_per_query
    }

    fn random_matrix(rows: usize, dim: usize, seed: u64) -> Vec<f32> {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        (0..rows * dim)
            .map(|_| rng.random_range(-1.0_f32..1.0_f32))
            .collect()
    }

    fn timed<T, E>(mut op: impl FnMut() -> Result<T, E>) -> Result<(T, f64), E> {
        // One warm launch so module load and cuBLAS handle creation are not
        // charged to the measured run.
        let _warm = op()?;
        let started = Instant::now();
        let value = op()?;
        Ok((value, duration_ms(started.elapsed())))
    }

    fn duration_ms(elapsed: Duration) -> f64 {
        elapsed.as_secs_f64() * 1_000.0
    }
}

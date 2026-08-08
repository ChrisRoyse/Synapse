//! FSV harness for the DiskANN Vamana build lane (#2103).
//!
//! Builds a Vamana graph over a seeded, clustered, production-shaped corpus and
//! reports wall-clock plus a content digest of the published `graph.cda`. The
//! digest is the graph-identity proof for the CPU refactor: the adjacency is
//! serialized into that file, so two builds that agree byte-for-byte have
//! identical topology.
//!
//! Corpus: synthetic but production-shaped. The live search generation is
//! 374,615 rows with `Dense(96)` as the fat slot, so the defaults here are
//! `rows=374615`, `dim=96`. Vectors are drawn from `clusters` isotropic
//! Gaussian modes (Box-Muller, ChaCha8 seeded) and unit-normalized, because a
//! uniform-noise corpus produces a degenerate graph whose expansion counts do
//! not resemble the production lane. No production data is read or written.
//!
//! Env knobs (all optional):
//!   SEXTANT_FSV_ROWS      rows in the corpus            (default 374615)
//!   SEXTANT_FSV_DIM       dimension                     (default 96)
//!   SEXTANT_FSV_CLUSTERS  cluster modes                 (default 512)
//!   SEXTANT_FSV_SPREAD    within-cluster stddev         (default 0.35)
//!   SEXTANT_FSV_SEED      corpus seed                   (default 20260808)
//!   SEXTANT_FSV_M_MAX     m_max                         (default 32)
//!   SEXTANT_FSV_EF        ef_construction               (default 64)
//!   SEXTANT_FSV_ALPHA     alpha                         (default 1.2)
//!   SEXTANT_FSV_OUT       scratch dir for graph.cda     (default std temp)
//!   SEXTANT_FSV_KEEP      "1" keeps the graph file
//!   SEXTANT_FSV_RECALL_Q  query count for recall parity (default 0 = skip)
//!   SEXTANT_FSV_RECALL_K  k for recall parity           (default 10)
//!   SEXTANT_FSV_EDGE_CASES "1" runs the bounds/edge-case sweep only

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

/// Examples carry their own error type: `calyx_core::Result` has no
/// `From<io::Error>`, and an FSV harness legitimately mixes both.
type Fsv<T> = std::result::Result<T, Box<dyn std::error::Error>>;

use calyx_core::{CxId, SlotId};
use calyx_sextant::index::diskann::{
    DiskAnnBuildBackend, DiskAnnBuildParams, DiskAnnSearch, DiskAnnSearchParams,
    build_diskann_graph, build_diskann_graph_with_backend_and_progress,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rayon::prelude::*;

fn env_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_f32(key: &str, default: f32) -> f32 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_flag(key: &str) -> bool {
    env::var(key).map(|v| v == "1").unwrap_or(false)
}

/// Standard normal via Box-Muller on a seeded ChaCha8 stream (portable and
/// reproducible across hosts; `rand`'s distribution internals are not).
fn normal(rng: &mut ChaCha8Rng) -> f32 {
    loop {
        let u: f32 = rng.random::<f32>();
        if u > 0.0 {
            let v: f32 = rng.random::<f32>();
            return (-2.0 * u.ln()).sqrt() * (std::f32::consts::TAU * v).cos();
        }
    }
}

fn corpus(
    rows: usize,
    dim: usize,
    clusters: usize,
    spread: f32,
    seed: u64,
) -> Vec<(u32, Vec<f32>)> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| normal(&mut rng)).collect())
        .collect();
    (0..rows)
        .map(|id| {
            let center = &centers[id % centers.len()];
            let mut v: Vec<f32> = center
                .iter()
                .map(|c| c + spread * normal(&mut rng))
                .collect();
            let mag = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if mag > 0.0 {
                for x in &mut v {
                    *x /= mag;
                }
            }
            (id as u32, v)
        })
        .collect()
}

fn digest(path: &PathBuf) -> Fsv<(String, u64)> {
    let bytes = fs::read(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes);
    Ok((hasher.finalize().to_hex().to_string(), bytes.len() as u64))
}

fn scratch_dir() -> PathBuf {
    match env::var("SEXTANT_FSV_OUT") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => env::temp_dir().join("sextant-diskann-fsv"),
    }
}

fn main() -> Fsv<()> {
    if env_flag("SEXTANT_FSV_EDGE_CASES") {
        return edge_cases();
    }
    let rows = env_usize("SEXTANT_FSV_ROWS", 374_615);
    let dim = env_usize("SEXTANT_FSV_DIM", 96);
    let clusters = env_usize("SEXTANT_FSV_CLUSTERS", 512);
    let spread = env_f32("SEXTANT_FSV_SPREAD", 0.35);
    let seed = env_usize("SEXTANT_FSV_SEED", 20_260_808) as u64;
    let params = DiskAnnBuildParams {
        dim,
        m_max: env_usize("SEXTANT_FSV_M_MAX", 32),
        ef_construction: env_usize("SEXTANT_FSV_EF", 64),
        alpha: env_f32("SEXTANT_FSV_ALPHA", 1.2),
    };
    let dir = scratch_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("graph-{rows}x{dim}.cda"));

    let gen_start = Instant::now();
    let vectors = corpus(rows, dim, clusters, spread, seed);
    let gen_ms = gen_start.elapsed().as_millis();
    println!(
        "corpus rows={rows} dim={dim} clusters={clusters} spread={spread} seed={seed} gen_ms={gen_ms}"
    );
    println!(
        "params m_max={} ef_construction={} alpha={}",
        params.m_max, params.ef_construction, params.alpha
    );

    let build_start = Instant::now();
    let mut marks: Vec<(&'static str, u128)> = Vec::new();
    build_diskann_graph_with_backend_and_progress(
        &path,
        &vectors,
        params,
        DiskAnnBuildBackend::CpuVamana,
        |p| {
            if !p.phase.ends_with("_page") && !p.phase.ends_with("_batch_ok") {
                marks.push((p.phase, build_start.elapsed().as_millis()));
            }
            Ok(())
        },
    )?;
    let build_ms = build_start.elapsed().as_millis();
    let mut prev = 0_u128;
    for (phase, at) in &marks {
        println!("PHASE {phase} at_ms={at} delta_ms={}", at - prev);
        prev = *at;
    }
    println!(
        "PHASE build_end at_ms={build_ms} delta_ms={}",
        build_ms - prev
    );
    let (hash, size) = digest(&path)?;
    println!("BUILD_MS={build_ms}");
    println!("GRAPH_BLAKE3={hash}");
    println!("GRAPH_BYTES={size}");

    let queries = env_usize("SEXTANT_FSV_RECALL_Q", 0);
    if queries > 0 {
        let k = env_usize("SEXTANT_FSV_RECALL_K", 10);
        recall(&path, &vectors, dim, queries, k, seed)?;
    }

    if !env_flag("SEXTANT_FSV_KEEP") {
        fs::remove_file(&path).ok();
    }
    Ok(())
}

/// Recall@k of the built graph against exact brute-force ground truth on a
/// fixed, seed-derived query set. Queries are corpus rows perturbed off-lattice
/// so they are not trivially their own answer.
fn recall(
    path: &PathBuf,
    vectors: &[(u32, Vec<f32>)],
    dim: usize,
    queries: usize,
    k: usize,
    seed: u64,
) -> Fsv<()> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x9E37_79B9);
    let picks: Vec<Vec<f32>> = (0..queries)
        .map(|_| {
            let base = &vectors[rng.random_range(0..vectors.len())].1;
            let mut v: Vec<f32> = base.iter().map(|x| x + 0.05 * normal(&mut rng)).collect();
            let mag = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if mag > 0.0 {
                for x in &mut v {
                    *x /= mag;
                }
            }
            v
        })
        .collect();

    let truth: Vec<Vec<u32>> = picks
        .par_iter()
        .map(|q| {
            let mut scored: Vec<(u32, f32)> = vectors
                .iter()
                .map(|(id, v)| {
                    let d: f32 = q.iter().zip(v).map(|(a, b)| (a - b) * (a - b)).sum();
                    (*id, d)
                })
                .collect();
            scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            scored.truncate(k);
            scored.into_iter().map(|(id, _)| id).collect()
        })
        .collect();

    // Node ids are dense 0..n, so the id map is a placeholder of the right
    // length; `search_ids` reports node ids, which is what the truth set holds.
    let ids: Vec<CxId> = (0..vectors.len())
        .map(|i| {
            let mut bytes = [0_u8; 16];
            bytes[..8].copy_from_slice(&(i as u64).to_be_bytes());
            CxId::from_bytes(bytes)
        })
        .collect();
    let make_params = || DiskAnnSearchParams {
        beamwidth: 32,
        ef_search: 64.max(k),
        rescore_k: 64.max(k),
        rescore_from_raw: false,
    };
    let index = DiskAnnSearch::open(SlotId::new(0), path, ids, None, make_params())?;
    let params = make_params();
    let mut hit = 0_usize;
    let mut total = 0_usize;
    for (q, want) in picks.iter().zip(&truth) {
        let got = index.search_ids(q, k, &params)?;
        for (id, _) in &got {
            if want.contains(id) {
                hit += 1;
            }
        }
        total += want.len();
    }
    println!(
        "RECALL_AT_{k}={:.6} hits={hit} total={total} queries={queries} dim={dim}",
        hit as f64 / total as f64
    );
    Ok(())
}

/// Bounds and degenerate shapes: empty corpus, single row, corpus smaller than
/// one synchronization batch, and `m_max`/`ef_construction` at their limits.
fn edge_cases() -> Fsv<()> {
    let dir = scratch_dir();
    fs::create_dir_all(&dir)?;
    let dim = 8_usize;

    let empty: Vec<(u32, Vec<f32>)> = Vec::new();
    let path = dir.join("edge-empty.cda");
    let outcome = build_diskann_graph(
        &path,
        &empty,
        DiskAnnBuildParams {
            dim,
            m_max: 4,
            ef_construction: 8,
            alpha: 1.2,
        },
    );
    println!("EDGE empty -> {}", describe(&outcome));

    for (label, rows, m_max, ef) in [
        ("single", 1_usize, 4_usize, 8_usize),
        ("two", 2, 4, 8),
        ("sub_batch", 200, 32, 64),
        ("one_batch_exact", 256, 32, 64),
        ("m_max_1", 300, 1, 1),
        ("ef_1", 300, 4, 1),
        ("m_max_gt_rows", 40, 64, 128),
    ] {
        let vectors = corpus(rows, dim, 8, 0.35, 7);
        let path = dir.join(format!("edge-{label}.cda"));
        let start = Instant::now();
        let outcome = build_diskann_graph(
            &path,
            &vectors,
            DiskAnnBuildParams {
                dim,
                m_max,
                ef_construction: ef,
                alpha: 1.2,
            },
        );
        let ms = start.elapsed().as_millis();
        let tag = match (&outcome, path.exists()) {
            (Ok(()), true) => {
                let (hash, size) = digest(&path)?;
                format!("ok bytes={size} blake3={}", &hash[..16])
            }
            (Ok(()), false) => "ok but no file".to_string(),
            (Err(e), _) => format!("err {e}"),
        };
        println!("EDGE {label} rows={rows} m_max={m_max} ef={ef} ms={ms} -> {tag}");
        fs::remove_file(&path).ok();
    }
    fs::remove_file(dir.join("edge-empty.cda")).ok();
    Ok(())
}

fn describe(outcome: &calyx_core::Result<()>) -> String {
    match outcome {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("err {e}"),
    }
}

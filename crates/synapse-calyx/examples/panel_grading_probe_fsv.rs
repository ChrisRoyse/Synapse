//! Manual FSV probe for #1963: does a candidate graded dense lens actually
//! grade `syn-timeline-v1`, measured against the real corpus?
//!
//! ## Why a probe before a panel bump
//!
//! #1963 asks for "a graded dense lens" on the active operator panel. Adding a
//! slot to a published panel costs a panel-version bump plus a full backfill,
//! so the lens has to be chosen by measurement, not by argument. Two candidates
//! are plausible from first principles and only one of them can be right:
//!
//! * a **temporal** graded lane (`syn_scalar_rank_arc`, the half-circle
//!   encoding of the same frozen rank slot 7 already carries). Cosine becomes
//!   `cos(pi * du)` — genuinely monotone in the rank difference — but the frozen
//!   range is 1970..2100 while the corpus spans months, so `du` between two
//!   adjacent events is ~1e-8 and the cosine rounds to exactly 1.0. Worse, the
//!   *nearest* neighbour in a dense event stream is always seconds away, so no
//!   time-only lane can grade a nearest-neighbour statistic at all.
//! * a **record-shape** graded lane (`syn_record_vector` over a scale-normalized
//!   numeric view of the record: kind, actor, time-of-day/week position, and the
//!   log-normalized lengths of app / title / url / raw bytes). Two records are
//!   close here when they genuinely describe similar activity.
//!
//! This harness measures both over the real corpus and prints the distinct-value
//! structure of each one's nearest-neighbour cosine, beside the five existing
//! dense lenses so the comparison is like-for-like.
//!
//! ## Source of truth
//!
//! The `Base` CF of a frozen copy of the live vault. Base rows carry the exact
//! `scalars` and verbatim `metadata` (#1894: they carry no slot vectors), which
//! is precisely the field set a candidate lens would measure, so the candidate
//! vectors here are computed through the **real** frozen Calyx encoders rather
//! than reimplemented.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example panel_grading_probe_fsv -- <vault-copy-dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{Input, Lens as _, Modality, SlotVector};
use calyx_loom::SimilarityDiscrimination;
use calyx_registry::{AlgorithmicEncoder, AlgorithmicLens, DenseCosineGrading};
use serde_json::json;
use synapse_calyx::{
    SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxTuningConfig, SynapseCalyxVault,
};

const PANEL: u32 = 1_900_001;
/// The frozen bound `syn.timeline.event_time_rank.v1` already carries.
const RECENCY_RANK_MAX_UNIX_MS: i64 = 4_102_444_800_000;
const RANK_BOUND_MICROS_PER_UNIT: i64 = 1_000_000;
const RECORD_VECTOR_DIM: u32 = 32;

const SECS_PER_DAY: f64 = 86_400.0;
const SECS_PER_WEEK: f64 = 604_800.0;

fn main() -> Result<(), Box<dyn Error>> {
    // ------------------------------------------------------------------
    // Part 1 — the structural declaration, checked against hand arithmetic
    // ------------------------------------------------------------------
    println!("== Part 1: dense_cosine_grading, expectations computed by hand ==");
    let cases: &[(&str, AlgorithmicEncoder, DenseCosineGrading)] = &[
        (
            "syn_scalar_rank (slot 7 today: dim 1, image [0,1] => one ray)",
            AlgorithmicEncoder::SynScalarRank {
                min_micros: 0,
                max_micros: RECENCY_RANK_MAX_UNIX_MS * RANK_BOUND_MICROS_PER_UNIT,
            },
            DenseCosineGrading::Constant,
        ),
        (
            "syn_scalar_rank_arc (same bounds, half-circle image)",
            AlgorithmicEncoder::SynScalarRankArc {
                min_micros: 0,
                max_micros: RECENCY_RANK_MAX_UNIX_MS * RANK_BOUND_MICROS_PER_UNIT,
            },
            DenseCosineGrading::Graded,
        ),
        (
            "syn_one_hot(32) (slots 1/6)",
            AlgorithmicEncoder::SynOneHot { buckets: 32 },
            DenseCosineGrading::Finite(32),
        ),
        (
            "syn_cyclic_time(24) (slot 4: 24 positions on a cycle)",
            AlgorithmicEncoder::SynCyclicTime { period: 24 },
            DenseCosineGrading::Finite(24),
        ),
        (
            "syn_record_vector(32) (the candidate)",
            AlgorithmicEncoder::SynRecordVector { dim: 32 },
            DenseCosineGrading::Graded,
        ),
        (
            "syn_sparse_text_tf(2048) (slot 103: not a dense lane)",
            AlgorithmicEncoder::SynSparseTextTf { dim: 2048 },
            DenseCosineGrading::NotDense,
        ),
        (
            "syn_scalar_zscore (signed dim 1 => two opposite rays)",
            AlgorithmicEncoder::SynScalarZScore {
                mean_micros: 0,
                std_micros: 1_000_000,
            },
            DenseCosineGrading::Finite(2),
        ),
    ];
    let mut failures = Vec::new();
    for (label, encoder, expected) in cases {
        let observed = encoder.dense_cosine_grading();
        let verdict = if observed == *expected {
            "PASS"
        } else {
            "FAIL"
        };
        println!("  [{verdict}] {label}\n           observed={observed:?} expected={expected:?}");
        if observed != *expected {
            failures.push(format!(
                "{label}: observed={observed:?} expected={expected:?}"
            ));
        }
    }

    // ------------------------------------------------------------------
    // Part 2 — the arc encoder's own arithmetic, hand-computed
    // ------------------------------------------------------------------
    println!("\n== Part 2: syn_scalar_rank_arc arithmetic (expected values by hand) ==");
    // Bounds 0..1_000_000 micros == the value range [0.0, 1.0].
    let arc = AlgorithmicLens::syn_scalar_rank_arc(
        "probe.arc.v1",
        Modality::Structured,
        0,
        1_000_000, // == 1.0 in value units
    );
    let at = |v: f64| -> Result<Vec<f32>, Box<dyn Error>> {
        let out = arc.measure(&Input::new(
            Modality::Structured,
            v.to_string().into_bytes(),
        ))?;
        match out {
            SlotVector::Dense { data, .. } => Ok(data),
            other => Err(format!("expected dense, got {other:?}").into()),
        }
    };
    let v0 = at(0.0)?;
    let v_half = at(0.5)?;
    let v1 = at(1.0)?;
    println!("  u=0.0 -> {v0:?}   (expected [cos 0, sin 0] = [1, 0])");
    println!("  u=0.5 -> {v_half:?}   (expected [cos pi/2, sin pi/2] = [0, 1])");
    println!("  u=1.0 -> {v1:?}   (expected [cos pi, sin pi] = [-1, 0])");
    let mut check = |label: &str, observed: f32, expected: f32| {
        let ok = (observed - expected).abs() <= 1e-6;
        println!(
            "  [{}] {label}: observed={observed} expected={expected}",
            if ok { "PASS" } else { "FAIL" }
        );
        if !ok {
            failures.push(format!("{label}: observed={observed} expected={expected}"));
        }
    };
    check("cos(0,0)", cosine(&v0, &v0), 1.0);
    check("cos(0,0.5) == cos(pi/2)", cosine(&v0, &v_half), 0.0);
    check("cos(0,1.0) == cos(pi)", cosine(&v0, &v1), -1.0);
    check("cos(0.5,1.0) == cos(pi/2)", cosine(&v_half, &v1), 0.0);
    check("norm(u=0.5)", norm(&v_half), 1.0);
    // Monotone: cosine must strictly decrease as the rank gap widens.
    let steps: Vec<Vec<f32>> = (0..=10)
        .map(|i| at(f64::from(i) / 10.0))
        .collect::<Result<_, _>>()?;
    let sims: Vec<f32> = steps.iter().map(|v| cosine(&steps[0], v)).collect();
    println!("  cos(u=0, u=k/10) for k=0..10: {sims:?}");
    let monotone = sims.windows(2).all(|w| w[1] < w[0]);
    println!(
        "  [{}] strictly decreasing in the rank gap: {monotone}",
        if monotone { "PASS" } else { "FAIL" }
    );
    if !monotone {
        failures.push("arc cosine is not strictly decreasing in the rank gap".to_owned());
    }
    // Fail-closed on an out-of-range value, exactly like syn_scalar_rank.
    let out_of_range = arc
        .measure(&Input::new(Modality::Structured, b"2.0".to_vec()))
        .is_err();
    println!(
        "  [{}] out-of-range input (2.0 with max 1.0) is refused: {out_of_range}",
        if out_of_range { "PASS" } else { "FAIL" }
    );
    if !out_of_range {
        failures.push("arc encoder accepted an out-of-range value".to_owned());
    }

    // ------------------------------------------------------------------
    // Part 3 — the real corpus
    // ------------------------------------------------------------------
    let Some(root) = std::env::args().nth(1).map(PathBuf::from) else {
        println!("\n== Part 3 skipped: no <vault-copy-dir> argument ==");
        return finish(&failures);
    };
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let config = SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    };
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        config,
        Some(vec![ColumnFamily::Kv, ColumnFamily::Base]),
    )?;
    println!("\n== Part 3: real corpus, frozen vault copy ==");
    println!("  vault = {}", vault_dir.display());

    let rows = vault.scan_cf_latest(ColumnFamily::Base)?;
    let mut records: Vec<TimelineShape> = Vec::new();
    for (_, value) in rows {
        let base = decode_constellation_base(&value)?;
        if base.panel_version != PANEL {
            continue;
        }
        records.push(TimelineShape::from_base(&base.metadata, &base.scalars));
    }
    println!("  panel {PANEL} base rows = {}", records.len());
    if records.len() < 2 {
        failures.push(format!(
            "panel {PANEL} has {} rows; the probe needs at least 2",
            records.len()
        ));
        return finish(&failures);
    }

    // Candidate A: the record-shape vector.
    let record_lens = AlgorithmicLens::syn_record_vector(
        "syn.timeline.record_vector.v1",
        Modality::Structured,
        RECORD_VECTOR_DIM,
    );
    let mut record_vectors = Vec::with_capacity(records.len());
    for shape in &records {
        let bytes = serde_json::to_vec(&shape.numeric_record())?;
        match record_lens.measure(&Input::new(Modality::Structured, bytes))? {
            SlotVector::Dense { data, .. } => record_vectors.push(data),
            other => return Err(format!("record vector returned {other:?}").into()),
        }
    }

    // Candidate B: the temporal arc over the frozen 1970..2100 range.
    let arc_lens = AlgorithmicLens::syn_scalar_rank_arc(
        "syn.timeline.event_time_arc.v1",
        Modality::Structured,
        0,
        RECENCY_RANK_MAX_UNIX_MS * RANK_BOUND_MICROS_PER_UNIT,
    );
    let mut arc_vectors = Vec::with_capacity(records.len());
    for shape in &records {
        match arc_lens.measure(&Input::new(
            Modality::Structured,
            shape.unix_ms.to_string().into_bytes(),
        ))? {
            SlotVector::Dense { data, .. } => arc_vectors.push(data),
            other => return Err(format!("arc lens returned {other:?}").into()),
        }
    }

    println!("\n  candidate nearest-neighbour cosine discrimination:");
    let mut verdicts = BTreeMap::new();
    for (label, vectors) in [
        (
            "record_vector(32) over scale-normalized fields",
            &record_vectors,
        ),
        (
            "event_time_arc over the frozen 1970..2100 range",
            &arc_vectors,
        ),
    ] {
        let sims = nearest_neighbour_sims(vectors);
        let d = SimilarityDiscrimination::measure(&sims)?;
        println!(
            "    {label}\n      n={} distinct={} modal={:.6} share={:.6} range=[{:.6},{:.6}] definitional={}",
            d.n,
            d.distinct_values,
            d.modal_value,
            d.modal_share,
            d.min,
            d.max,
            d.is_definitional()
        );
        verdicts.insert(label, d);
    }

    // The binding decision: the lens Synapse adds to the panel must NOT be
    // definitional over the real corpus. Anything else is a slot that costs a
    // panel bump and a backfill and still cannot rank.
    let record = verdicts["record_vector(32) over scale-normalized fields"];
    println!("\n  decision:");
    println!(
        "    record_vector is_definitional={} (must be false to be worth adding)",
        record.is_definitional()
    );
    if record.is_definitional() {
        failures.push(format!(
            "record_vector candidate is still definitional over the real corpus: \
             distinct={} modal_share={:.6}",
            record.distinct_values, record.modal_share
        ));
    }

    // Show what the corpus looks like, so the numbers above are interpretable.
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for shape in &records {
        *kinds.entry(shape.kind.as_str()).or_default() += 1;
    }
    println!("\n  corpus composition by kind: {kinds:?}");
    let distinct_titles: std::collections::BTreeSet<&String> =
        records.iter().map(|r| &r.title).collect();
    let distinct_apps: std::collections::BTreeSet<&String> =
        records.iter().map(|r| &r.app).collect();
    println!(
        "  distinct titles={} distinct apps={} distinct raw_len={}",
        distinct_titles.len(),
        distinct_apps.len(),
        records
            .iter()
            .map(|r| r.raw_len_bytes)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );

    // Guard against a silent, uninformative pass: the vault copy must actually
    // be the corpus the issue was filed against.
    let _ = SynapseCalyxVault::open;
    finish(&failures)
}

/// The exact record-shape fields a candidate lens would measure, read verbatim
/// off a `Base` row.
struct TimelineShape {
    kind: String,
    actor_is_agent: f64,
    unix_ms: u64,
    app: String,
    title: String,
    url_host: String,
    raw_len_bytes: u64,
}

impl TimelineShape {
    fn from_base(metadata: &BTreeMap<String, String>, scalars: &BTreeMap<String, f64>) -> Self {
        let get = |key: &str| metadata.get(key).cloned().unwrap_or_default();
        Self {
            kind: get("timeline_kind"),
            actor_is_agent: f64::from(u8::from(get("timeline_actor") == "agent")),
            unix_ms: scalars
                .get("ts_unix_ms")
                .copied()
                .map_or(0, |v| v.max(0.0) as u64),
            app: get("timeline_app"),
            title: get("timeline_title_excerpt"),
            url_host: get("timeline_url_host"),
            raw_len_bytes: scalars
                .get("raw_len_bytes")
                .copied()
                .map_or(0, |v| v.max(0.0) as u64),
        }
    }

    /// Every field on a comparable scale.
    ///
    /// `syn_record_vector` multiplies each field's value by a signed hash of its
    /// *path* and unit-normalizes the sum. A raw magnitude therefore decides the
    /// direction on its own: feeding `ts_unix_ms` (1.7e12) beside a byte count
    /// (~200) makes the vector a re-encoding of the timestamp. Every field here
    /// is mapped into roughly `[0, 1]` first, which is what makes the resulting
    /// direction a summary of the record rather than of its largest unit.
    fn numeric_record(&self) -> serde_json::Value {
        let secs = self.unix_ms / 1_000;
        let day_fraction = (secs % 86_400) as f64 / SECS_PER_DAY;
        let week_fraction = (secs % 604_800) as f64 / SECS_PER_WEEK;
        json!({
            "kind_ordinal": kind_ordinal(&self.kind),
            "actor_is_agent": self.actor_is_agent,
            "day_fraction": day_fraction,
            "week_fraction": week_fraction,
            "has_app": present(&self.app),
            "app_len_norm": len_norm(&self.app, 64.0),
            "has_title": present(&self.title),
            "title_len_norm": len_norm(&self.title, 128.0),
            "has_url_host": present(&self.url_host),
            "url_host_len_norm": len_norm(&self.url_host, 64.0),
            "raw_len_norm": (1.0 + self.raw_len_bytes as f64).ln() / (1.0 + 4096.0f64).ln(),
        })
    }
}

/// Stable ordinal position of a timeline kind, normalized to `[0, 1]`.
///
/// The order is the declaration order of `TimelineKind`, frozen here so the
/// probe's numbers are reproducible.
fn kind_ordinal(kind: &str) -> f64 {
    const KINDS: &[&str] = &[
        "focus_change",
        "title_change",
        "idle_start",
        "idle_end",
        "session_start",
        "session_end",
        "interaction_summary",
        "clipboard",
        "file_activity",
        "browser_nav",
        "demo_marker",
        "purge",
    ];
    let last = (KINDS.len() - 1) as f64;
    KINDS
        .iter()
        .position(|k| *k == kind)
        .map_or(0.0, |i| i as f64 / last)
}

fn present(value: &str) -> f64 {
    f64::from(u8::from(!value.is_empty()))
}

fn len_norm(value: &str, scale: f64) -> f64 {
    (1.0 + value.chars().count() as f64).ln() / (1.0 + scale).ln()
}

/// Each record's maximum cosine against any *other* record, which is the exact
/// statistic #1963 reports as saturating at 1.0.
fn nearest_neighbour_sims(vectors: &[Vec<f32>]) -> Vec<f32> {
    let mut sims = Vec::with_capacity(vectors.len());
    for (i, a) in vectors.iter().enumerate() {
        let mut best = f32::NEG_INFINITY;
        for (j, b) in vectors.iter().enumerate() {
            if i == j {
                continue;
            }
            let s = cosine(a, b);
            if s > best {
                best = s;
            }
        }
        if best.is_finite() {
            sims.push(best);
        }
    }
    sims
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = norm(a);
    let nb = norm(b);
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na * nb)).clamp(-1.0, 1.0)
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn finish(failures: &[String]) -> Result<(), Box<dyn Error>> {
    println!("\n================================================================");
    if failures.is_empty() {
        println!("ALL CHECKS PASSED");
        Ok(())
    } else {
        println!("{} CHECK(S) FAILED:", failures.len());
        for failure in failures {
            println!("  - {failure}");
        }
        Err("panel_grading_probe_fsv failed".into())
    }
}

//! Manual FSV for #1964: does each panel's `record_vector` slot actually grade
//! its corpus, or is it a re-encoding of the record's clock?
//!
//! ## The claim under test
//!
//! `syn_record_vector` places each numeric field at `hash(path) % dim` and
//! multiplies by the field's **raw value**, then unit-normalizes. The direction
//! is therefore owned by whichever field carries the largest units. Nine of the
//! ten built-in numeric-record builders feed a unix-millisecond timestamp
//! (~1.7e12) beside counts, flags and durations (0..1e5), so the unit vector is
//! the timestamp with every other field as rounding noise below f32 resolution.
//!
//! #1963 fixed exactly this on `syn-timeline-v1`, which is why slot 104 is the
//! **control** here: a probe that cannot separate the fixed slot from the broken
//! ones is measuring nothing.
//!
//! ## Where the truth is read from
//!
//! Not from re-measuring a record through the encoder — from the **bytes on
//! disk**. Each slot's per-slot column family (`cf/slot_NN`) holds the vector
//! that was actually committed for every constellation, and this probe decodes
//! those rows and computes each record's nearest-neighbour cosine against the
//! others. `SimilarityDiscrimination` then reports whether that statistic is a
//! measurement or a constant.
//!
//! ```text
//! cargo run -p synapse-calyx --example record_vector_grading_fsv -- <vault-copy-dir>
//! ```
//!
//! `<vault-copy-dir>` must contain `db-daemon/` and `machine-salt.bin`. Use a
//! copy: this opens read-only, but a live vault holds a writer lock.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_slot_vector;
use calyx_core::{SlotId, SlotVector};
use calyx_loom::SimilarityDiscrimination;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxTuningConfig};

/// Every built-in `record_vector` slot, with the panel that owns it and the
/// largest-unit field its numeric-record builder feeds.
///
/// `raw_scale_field` is `None` only where the builder was already rebuilt on
/// comparable scales — today that is `syn-timeline-v1` alone (#1963).
struct Probe {
    slot: u16,
    panel: u32,
    lens: &'static str,
    raw_scale_field: Option<&'static str>,
}

const PROBES: &[Probe] = &[
    Probe {
        slot: 104,
        panel: 1_963_001,
        lens: "syn.timeline.record_vector.v1",
        raw_scale_field: None, // control: rebuilt on comparable scales by #1963
    },
    Probe {
        slot: 22,
        panel: 1_904_002,
        lens: "syn.episode.record_vector.v1",
        raw_scale_field: Some("start_unix_ms / end_unix_ms"),
    },
    Probe {
        slot: 34,
        panel: 1_665_001,
        lens: "syn.agent_event.record_vector.v1",
        raw_scale_field: Some("ts_unix_ms"),
    },
    Probe {
        slot: 47,
        panel: 1_921_001,
        lens: "syn.agent_transcript.record_vector.v1",
        raw_scale_field: Some("ts_unix_ms"),
    },
    Probe {
        slot: 50,
        panel: 1_776_001,
        lens: "syn.action.params_record_vector.v1",
        raw_scale_field: Some("ts_unix_ms"),
    },
    Probe {
        slot: 59,
        panel: 1_776_002,
        lens: "syn.reflex.record_vector.v1",
        raw_scale_field: Some("ts_unix_ms"),
    },
    Probe {
        slot: 66,
        panel: 1_776_003,
        lens: "syn.process.record_vector.v1",
        raw_scale_field: Some("ts_unix_ms"),
    },
    Probe {
        slot: 74,
        panel: 1_776_004,
        lens: "syn.observation.record_vector.v1",
        raw_scale_field: Some("ts_unix_ms"),
    },
    Probe {
        slot: 81,
        panel: 1_776_005,
        lens: "syn.outcome.record_vector.v1",
        raw_scale_field: Some("raw_len_bytes"),
    },
    Probe {
        slot: 93,
        panel: 1_776_006,
        lens: "syn.mcp_usage.record_vector.v1",
        raw_scale_field: Some("finished_unix_ms"),
    },
];

/// Deterministic sample ceiling per slot.
///
/// Nearest-neighbour discrimination is O(n^2); 101k rows is 1e10 cosines. A
/// stride sample of 3000 is 9e6, runs in under a second, and is *conservative*
/// for the question asked: a sample drawn evenly across the whole keyspace that
/// still reports one distinct cosine cannot be a sampling artifact — collapsing
/// a diverse population into one value is not something subsampling does.
const SAMPLE_CEILING: usize = 3_000;

fn main() -> Result<(), Box<dyn Error>> {
    let Some(root) = std::env::args().nth(1).map(PathBuf::from) else {
        return Err("usage: record_vector_grading_fsv <vault-copy-dir>".into());
    };
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }

    let cfs: Vec<ColumnFamily> = PROBES
        .iter()
        .map(|p| ColumnFamily::slot(SlotId::new(p.slot)))
        .collect();
    let config = SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    };
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(config, Some(cfs))?;
    println!("== #1964 record_vector grading, read off the stored slot CFs ==");
    println!("   vault = {}\n", vault_dir.display());

    let mut failures: Vec<String> = Vec::new();
    let mut verdicts: BTreeMap<u16, (bool, usize, f32, f32)> = BTreeMap::new();

    for probe in PROBES {
        let cf = ColumnFamily::slot(SlotId::new(probe.slot));
        let rows = vault.scan_cf_latest(cf)?;
        let total = rows.len();
        if total < 2 {
            println!(
                "  slot {:>3} {:<40} rows={total} -- SKIPPED (needs >= 2)",
                probe.slot, probe.lens
            );
            continue;
        }
        let stride = total.div_ceil(SAMPLE_CEILING).max(1);
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        let mut non_dense = 0_usize;
        for (index, (_, value)) in rows.into_iter().enumerate() {
            if index % stride != 0 {
                continue;
            }
            match decode_slot_vector(&value)? {
                SlotVector::Dense { data, .. } => vectors.push(data),
                _ => non_dense += 1,
            }
        }
        if non_dense > 0 {
            failures.push(format!(
                "slot {} returned {non_dense} non-dense rows; a record_vector lane must be dense",
                probe.slot
            ));
        }
        if vectors.len() < 2 {
            println!(
                "  slot {:>3} {:<40} rows={total} sampled={} -- SKIPPED",
                probe.slot,
                probe.lens,
                vectors.len()
            );
            continue;
        }

        let sims = nearest_neighbour_sims(&vectors);
        let d = SimilarityDiscrimination::measure(&sims)?;
        let definitional = d.is_definitional();
        println!(
            "  slot {:>3} panel {} {:<40}\n      rows={total} sampled={} dim={} distinct={} modal={:.6} share={:.6} range=[{:.6},{:.6}]\n      is_definitional={}  raw-scale field: {}",
            probe.slot,
            probe.panel,
            probe.lens,
            vectors.len(),
            vectors.first().map_or(0, Vec::len),
            d.distinct_values,
            d.modal_value,
            d.modal_share,
            d.min,
            d.max,
            definitional,
            probe.raw_scale_field.unwrap_or("none (rebuilt by #1963)"),
        );
        verdicts.insert(probe.slot, (definitional, d.distinct_values, d.min, d.max));
    }

    // The control must separate from the population, or this probe measures
    // nothing and no verdict below it is admissible.
    println!("\n== control check ==");
    match verdicts.get(&104) {
        Some((definitional, distinct, ..)) => {
            println!(
                "  slot 104 (the #1963-fixed timeline record vector): is_definitional={definitional} distinct={distinct}"
            );
            if *definitional {
                failures.push(
                    "control slot 104 is definitional: the #1963 fix did not survive to disk, or \
                     this probe cannot distinguish a graded lane from a constant one"
                        .to_owned(),
                );
            }
        }
        None => failures.push("control slot 104 has no rows; the probe proves nothing".to_owned()),
    }

    println!("\n== verdict ==");
    let degenerate: Vec<u16> = verdicts
        .iter()
        .filter(|(slot, (definitional, ..))| **slot != 104 && *definitional)
        .map(|(slot, _)| *slot)
        .collect();
    println!("  definitional (cannot rank their corpus): {degenerate:?}");
    println!(
        "  graded: {:?}",
        verdicts
            .iter()
            .filter(|(_, (definitional, ..))| !*definitional)
            .map(|(slot, _)| *slot)
            .collect::<Vec<_>>()
    );

    println!("\n================================================================");
    if failures.is_empty() {
        println!("PROBE COMPLETED (this reports a measurement; it does not assert a fix)");
        Ok(())
    } else {
        println!("{} PROBE FAULT(S):", failures.len());
        for failure in &failures {
            println!("  - {failure}");
        }
        Err("record_vector_grading_fsv failed".into())
    }
}

/// Each record's maximum cosine against any *other* sampled record.
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
    if a.len() != b.len() {
        return f32::NAN;
    }
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

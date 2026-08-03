//! Real-corpus discrimination probe for #1965's transcript record-vector migration.
//!
//! Opens an exact frozen vault directory, decodes every authoritative
//! `CF_AGENT_TRANSCRIPTS` row, measures it through the production constellation
//! builder, and grades a deterministic stride sample of slot 110. This is a
//! source-corpus probe; the post-deployment FSV separately reads slot 110's
//! committed bytes.

use std::error::Error;
use std::path::PathBuf;

use calyx_core::{Constellation, SlotId, SlotVector};
use synapse_core::types::AgentTranscriptRecord;
use synapse_storage::Db;
use synapse_storage::constellations::{
    NativeConstellationContext, build_agent_transcript_constellation,
};

const SCHEMA_VERSION: u32 = 1;
const SLOT_NEW: u16 = 110;
const SAMPLE_CEILING: usize = 3_000;
const DISTINCT_TOLERANCE: f32 = 1.0e-6;

fn main() -> Result<(), Box<dyn Error>> {
    let vault_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: transcript_record_vector_fsv <exact-vault-dir>")?;
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let rows = db.scan_cf(synapse_storage::cf::CF_AGENT_TRANSCRIPTS)?;
    if rows.len() < 2 {
        return Err(format!(
            "CF_AGENT_TRANSCRIPTS has {} row(s); need at least 2",
            rows.len()
        )
        .into());
    }
    let stride = rows.len().div_ceil(SAMPLE_CEILING).max(1);
    println!("source_of_truth={}", vault_dir.display());
    println!("source_rows={} stride={stride}", rows.len());

    let mut vectors = Vec::new();
    let mut decode_failures = 0usize;
    let mut measurement_failures = 0usize;
    for (index, (key, raw)) in rows.iter().enumerate() {
        let Ok(record) = serde_json::from_slice::<AgentTranscriptRecord>(raw) else {
            decode_failures += 1;
            continue;
        };
        let cx = match build_agent_transcript_constellation(context()?, key, raw, &record) {
            Ok(cx) => cx,
            Err(error) => {
                measurement_failures += 1;
                eprintln!("measurement_failure key={} error={error}", hex(key));
                continue;
            }
        };
        if index % stride == 0 {
            match dense_slot(&cx, SLOT_NEW) {
                Some(vector) => vectors.push(vector),
                None => measurement_failures += 1,
            }
        }
    }
    println!(
        "decoded={} decode_failures={decode_failures} measurement_failures={measurement_failures} sampled_vectors={}",
        rows.len() - decode_failures,
        vectors.len()
    );
    if decode_failures != 0 || measurement_failures != 0 {
        return Err("not every authoritative transcript row measured successfully".into());
    }

    let sims = nearest_neighbour_sims(&vectors);
    let (distinct, modal_share, min, max) = discrimination(&sims)?;
    println!(
        "nearest_neighbor_cosine distinct={distinct} modal_share={modal_share:.6} range=[{min:.6},{max:.6}]"
    );
    if distinct <= 1 || modal_share >= 1.0 {
        return Err("candidate slot 110 remains definitional and may not be published".into());
    }
    println!("verdict=PASS candidate grades the real transcript corpus");
    Ok(())
}

fn context() -> Result<NativeConstellationContext, Box<dyn Error>> {
    Ok(NativeConstellationContext {
        vault_id: "00000000000000000000000003"
            .parse()
            .map_err(|error| format!("fixed synthetic vault id does not parse: {error:?}"))?,
        cx_id: calyx_core::CxId::from_bytes([4u8; 16]),
        created_at_ms: 1_760_000_000_000,
        next_ledger_seq: 1,
    })
}

fn dense_slot(cx: &Constellation, slot: u16) -> Option<Vec<f32>> {
    match cx.slots.get(&SlotId::new(slot)) {
        Some(SlotVector::Dense { data, .. }) => Some(data.clone()),
        _ => None,
    }
}

fn norm(vector: &[f32]) -> f32 {
    vector.iter().map(|value| value * value).sum::<f32>().sqrt()
}

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() {
        return f32::NAN;
    }
    let dot = left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>();
    let denominator = norm(left) * norm(right);
    if denominator == 0.0 {
        return f32::NAN;
    }
    (dot / denominator).clamp(-1.0, 1.0)
}

fn nearest_neighbour_sims(vectors: &[Vec<f32>]) -> Vec<f32> {
    vectors
        .iter()
        .enumerate()
        .filter_map(|(left_index, left)| {
            vectors
                .iter()
                .enumerate()
                .filter(|(right_index, _)| *right_index != left_index)
                .map(|(_, right)| cosine(left, right))
                .filter(|similarity| similarity.is_finite())
                .max_by(f32::total_cmp)
        })
        .collect()
}

fn discrimination(values: &[f32]) -> Result<(usize, f32, f32, f32), Box<dyn Error>> {
    if values.is_empty() {
        return Err("no finite nearest-neighbour similarities".into());
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let mut distinct = 0usize;
    let mut modal = 0usize;
    let mut run = 0usize;
    let mut start = sorted[0];
    for value in &sorted {
        if run > 0 && (*value - start).abs() <= DISTINCT_TOLERANCE {
            run += 1;
        } else {
            modal = modal.max(run);
            distinct += 1;
            start = *value;
            run = 1;
        }
    }
    modal = modal.max(run);
    #[allow(clippy::cast_precision_loss, reason = "sample is bounded to 3000 rows")]
    let modal_share = modal as f32 / sorted.len() as f32;
    Ok((distinct, modal_share, sorted[0], sorted[sorted.len() - 1]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

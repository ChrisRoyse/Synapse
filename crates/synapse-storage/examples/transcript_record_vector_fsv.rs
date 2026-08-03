//! Real-corpus discrimination probe for #1965's transcript record-vector migration.
//!
//! Opens an exact frozen vault directory, decodes every authoritative
//! `CF_AGENT_TRANSCRIPTS` row, measures it through the production constellation
//! builder, and grades a deterministic stride sample of slot 110. This is a
//! source-corpus probe; the post-deployment FSV separately reads slot 110's
//! committed bytes.

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::{decode_constellation_base, decode_slot_vector};
use calyx_core::{Constellation, SlotId, SlotVector};
use synapse_core::types::AgentTranscriptRecord;
use synapse_storage::Db;
use synapse_storage::constellations::{
    NativeConstellationContext, build_agent_transcript_constellation,
};

const SCHEMA_VERSION: u32 = 1;
const SLOT_NEW: u16 = 110;
const AGENT_PANEL: u32 = 1_965_001;
const TRANSCRIPT_PANEL: u32 = 1_965_002;
const AGENT_OLD_SLOT: u16 = 34;
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
    let mode = std::env::args().nth(2);
    if mode
        .as_deref()
        .is_some_and(|mode| mode != "--physical-only")
    {
        return Err(format!(
            "unknown mode `{}`; accepted mode is --physical-only",
            mode.as_deref().unwrap_or_default()
        )
        .into());
    }
    let physical_only = mode.is_some();
    if !physical_only {
        verify_candidate(&vault_dir)?;
    }

    println!("\n== committed physical state ==");
    let config = synapse_calyx::SynapseCalyxConfig {
        machine_salt_path: vault_dir
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::parent)
            .ok_or("backup vault path has no synapse data root ancestor")?
            .join("machine-salt.bin"),
        vault_dir: vault_dir.clone(),
        tuning: synapse_calyx::SynapseCalyxTuningConfig::default().validate()?,
    };
    let slot_cf = ColumnFamily::slot(SlotId::new(SLOT_NEW));
    let vault = synapse_calyx::SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        config,
        Some(vec![ColumnFamily::Base, slot_cf]),
    )?;
    let mut agent_rows = 0usize;
    let mut agent_rows_with_old_slot = 0usize;
    let mut transcript_rows = 0usize;
    let mut transcript_rows_with_new_slot = 0usize;
    for (_, raw) in vault.scan_cf_latest(ColumnFamily::Base)? {
        let base = decode_constellation_base(&raw)?;
        match base.panel_version {
            AGENT_PANEL => {
                agent_rows += 1;
                agent_rows_with_old_slot +=
                    usize::from(base.slots.contains_key(&SlotId::new(AGENT_OLD_SLOT)));
            }
            TRANSCRIPT_PANEL => {
                transcript_rows += 1;
                transcript_rows_with_new_slot +=
                    usize::from(base.slots.contains_key(&SlotId::new(SLOT_NEW)));
            }
            _ => {}
        }
    }
    let stored_rows = vault.scan_cf_latest(slot_cf)?;
    let total_stored_rows = stored_rows.len();
    let stored_stride = total_stored_rows.div_ceil(SAMPLE_CEILING).max(1);
    let stored_vectors: Vec<Vec<f32>> = stored_rows
        .iter()
        .enumerate()
        .filter(|(index, _)| index % stored_stride == 0)
        .map(|(_, (_, raw))| decode_slot_vector(raw))
        .map(|decoded| match decoded? {
            SlotVector::Dense { data, .. } => Ok(data),
            other => Err(format!("slot 110 stored non-dense value: {other:?}").into()),
        })
        .collect::<Result<_, Box<dyn Error>>>()?;
    let stored_sims = nearest_neighbour_sims(&stored_vectors);
    let (stored_distinct, stored_modal_share, stored_min, stored_max) =
        discrimination(&stored_sims)?;
    println!(
        "agent panel={AGENT_PANEL} base_rows={agent_rows} rows_declaring_retired_slot_34={agent_rows_with_old_slot}"
    );
    println!(
        "transcript panel={TRANSCRIPT_PANEL} base_rows={transcript_rows} rows_declaring_slot_110={transcript_rows_with_new_slot} slot_110_cf_rows={total_stored_rows}"
    );
    println!(
        "stored_slot_110_nearest_neighbor_cosine sampled={} distinct={stored_distinct} modal_share={stored_modal_share:.6} range=[{stored_min:.6},{stored_max:.6}]",
        stored_vectors.len()
    );
    if agent_rows == 0
        || agent_rows_with_old_slot != 0
        || transcript_rows == 0
        || transcript_rows_with_new_slot != transcript_rows
        || total_stored_rows != transcript_rows
        || stored_distinct <= 1
        || stored_modal_share >= 1.0
    {
        return Err("committed Base/slot CF state does not prove the migration".into());
    }
    println!("verdict=PASS committed Base and slot CF bytes prove the migration");
    Ok(())
}

fn verify_candidate(vault_dir: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let db = Db::open(vault_dir, SCHEMA_VERSION)?;
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

//! Measures every numeric field in the real MCP-usage corpus before #1965
//! freezes a replacement record-vector scale.

use std::error::Error;
use std::path::PathBuf;

use calyx_core::{Constellation, SlotId, SlotVector};
use serde_json::Value;
use synapse_storage::{
    StorageBackendKind, cf,
    constellations::{
        NativeConstellationContext, build_mcp_usage_constellation,
        mcp_usage_constellation_input_bytes,
    },
    scan_cf_read_only,
};

const PREFIX: &[u8] = b"mcp-usage/v1/";
const FIELDS: &[&str] = &[
    "schema_version",
    "seq",
    "session_sequence_position",
    "duration_ms",
    "response_size_bytes",
    "response_content_count",
    "argument_top_level_key_count",
    "argument_nested_path_count",
    "finished_at_unix_ms",
];

fn quantile(sorted: &[u64], numerator: usize, denominator: usize) -> Result<u64, &'static str> {
    if sorted.is_empty() {
        return Ok(0);
    }
    if denominator == 0 || numerator > denominator {
        return Err("quantile ratio must satisfy numerator <= denominator and denominator > 0");
    }
    let idx = (sorted.len() - 1)
        .checked_mul(numerator)
        .and_then(|product| product.checked_add(denominator / 2))
        .ok_or("quantile index arithmetic overflowed")?
        / denominator;
    Ok(sorted[idx])
}

#[expect(
    clippy::cast_precision_loss,
    reason = "this read-only distribution inspector intentionally represents observed integer magnitudes as approximate f64 values"
)]
const fn observed_u64_as_f64(value: u64) -> f64 {
    value as f64
}

fn count_norm(value: u64, scale: f64) -> f64 {
    (observed_u64_as_f64(value).ln_1p() / scale.ln_1p()).clamp(0.0, 1.0)
}

#[expect(
    clippy::too_many_lines,
    reason = "one read-only corpus inspection prints the source census, field distributions, and measured discrimination verdict together"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let vault_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: mcp_usage_scale_probe <backup-vault-dir>")?;
    let rows = scan_cf_read_only(
        &vault_dir,
        synapse_core::SCHEMA_VERSION,
        StorageBackendKind::Calyx,
        cf::CF_KV,
    )?;
    let source_rows: Vec<_> = rows
        .into_iter()
        .filter(|(key, _)| key.starts_with(PREFIX))
        .collect();
    println!("source_of_truth={}", vault_dir.display());
    println!("mcp_usage_source_rows={}", source_rows.len());
    if source_rows.is_empty() {
        return Err("no MCP usage source rows; scales cannot be grounded".into());
    }

    let mut records = Vec::with_capacity(source_rows.len());
    let mut decode_failures = 0usize;
    for (_, raw) in &source_rows {
        match serde_json::from_slice::<Value>(raw) {
            Ok(Value::Object(object)) => records.push((raw.len() as u64, Value::Object(object))),
            _ => decode_failures = decode_failures.saturating_add(1),
        }
    }
    println!(
        "decoded={} decode_failures={decode_failures}",
        records.len()
    );
    if decode_failures != 0 {
        return Err("authoritative MCP usage rows include undecodable data".into());
    }

    println!(
        "{:<32} {:>8} {:>8} {:>10} {:>10} {:>12} {:>14} {:>12}",
        "field", "present", "min", "median", "p90", "p99", "max", "scale"
    );
    for field in std::iter::once("raw_len_bytes").chain(FIELDS.iter().copied()) {
        let mut values: Vec<u64> = records
            .iter()
            .filter_map(|(raw_len, record)| {
                if field == "raw_len_bytes" {
                    Some(*raw_len)
                } else {
                    record.get(field).and_then(Value::as_u64)
                }
            })
            .collect();
        values.sort_unstable();
        let median = quantile(&values, 50, 100)?;
        let p90 = quantile(&values, 90, 100)?;
        let p99 = quantile(&values, 99, 100)?;
        let max = values.last().copied().unwrap_or(0);
        let scale = if p99 == 0 {
            1.0
        } else {
            10f64.powf(observed_u64_as_f64(p99).log10().ceil())
        };
        println!(
            "{field:<32} {:>8} {:>8} {median:>10} {p90:>10} {p99:>12} {max:>14} {scale:>12.0}",
            values.len(),
            values.first().copied().unwrap_or(0),
        );
        println!(
            "  normalized median={:.6} p90={:.6} p99={:.6} max={:.6}",
            count_norm(median, scale),
            count_norm(p90, scale),
            count_norm(p99, scale),
            count_norm(max, scale),
        );
    }

    let mut vectors = Vec::new();
    let mut measurement_failures = 0usize;
    for (index, (key, raw)) in source_rows.iter().enumerate() {
        let record: Value = serde_json::from_slice(raw)?;
        let input = mcp_usage_constellation_input_bytes(cf::CF_KV, key, raw);
        match build_mcp_usage_constellation(context()?, key, raw, &input, &record)
            .ok()
            .and_then(|cx| dense_slot(&cx, 115))
        {
            Some(vector) if vector.iter().all(|value| value.is_finite()) => {
                if index % 10 == 0 {
                    vectors.push(vector);
                }
            }
            _ => measurement_failures = measurement_failures.saturating_add(1),
        }
    }
    println!(
        "candidate_slot=115 measured={} failures={measurement_failures} discrimination_sample={}",
        source_rows.len().saturating_sub(measurement_failures),
        vectors.len()
    );
    if measurement_failures != 0 || vectors.len() < 2 {
        return Err("candidate record vector did not measure the authoritative corpus".into());
    }
    let nearest = nearest_neighbour_sims(&vectors);
    let mut buckets = nearest.clone();
    buckets.sort_by(f32::total_cmp);
    buckets.dedup_by(|left, right| (*left - *right).abs() <= 0.000_001);
    let min = nearest.iter().copied().fold(f32::INFINITY, f32::min);
    let max = nearest.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    println!(
        "candidate_nearest_neighbor_cosine distinct={} range=[{min:.6},{max:.6}]",
        buckets.len()
    );
    if buckets.len() <= 1 || min >= 1.0 - f32::EPSILON {
        return Err(
            "candidate slot remains geometrically constant and may not be published".into(),
        );
    }
    println!("candidate_verdict=PASS real MCP-usage rows produce discriminating geometry");
    Ok(())
}

fn context() -> Result<NativeConstellationContext, Box<dyn Error>> {
    Ok(NativeConstellationContext {
        vault_id: "00000000000000000000000003".parse()?,
        cx_id: calyx_core::CxId::from_bytes([4; 16]),
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

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    let dot = left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>();
    let left_norm = left.iter().map(|v| v * v).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|v| v * v).sum::<f32>().sqrt();
    (dot / (left_norm * right_norm)).clamp(-1.0, 1.0)
}

fn nearest_neighbour_sims(vectors: &[Vec<f32>]) -> Vec<f32> {
    vectors
        .iter()
        .enumerate()
        .map(|(left_index, left)| {
            vectors
                .iter()
                .enumerate()
                .filter(|(right_index, _)| *right_index != left_index)
                .map(|(_, right)| cosine(left, right))
                .fold(f32::NEG_INFINITY, f32::max)
        })
        .collect()
}

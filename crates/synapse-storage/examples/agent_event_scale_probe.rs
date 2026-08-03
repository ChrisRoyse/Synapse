//! Measures the real distribution of every numeric field
//! `agent_event_numeric_record` feeds, so #1965's rebuilt scales are **chosen
//! from the corpus** rather than guessed.
//!
//! ## Why this exists as its own step
//!
//! #1964's fix replaces a raw magnitude with `ln(1+n)/ln(1+scale)`, and that
//! `scale` is frozen into the lens forever. Picking it by taste would make the
//! lens's discrimination an accident: too high and every record squashes toward
//! 0, too low and everything clamps at 1.0 and the lane is tied again — the
//! exact defect being fixed, reintroduced from the other side.
//!
//! The scale wants to be "the order of magnitude at which this field stops
//! discriminating", so the honest way to set it is to look at where the corpus
//! actually lives. This prints per-field min/median/p90/p99/max over every real
//! `CF_AGENT_EVENTS` row, plus how many rows carry the field at all — a field
//! that is absent on 99% of rows is a different problem from one that is small.
//!
//! ```text
//! cargo run --release -p synapse-storage --example agent_event_scale_probe -- <vault-copy-dir>
//! ```

use std::error::Error;
use std::path::PathBuf;

use serde_json::Value;
use synapse_core::types::AgentEventRecord;

/// One numeric field, named exactly as `agent_event_numeric_record` emits it.
struct Field {
    name: &'static str,
    /// `None` when the record does not carry the field at all, which is
    /// counted separately from "carries it, value 0".
    read: fn(&AgentEventRecord) -> Option<u64>,
}

fn payload_u64(payload: &Value, path: &[&str]) -> Option<u64> {
    let mut node = payload;
    for key in path {
        node = node.get(*key)?;
    }
    node.as_u64()
}

const FIELDS: &[Field] = &[
    Field {
        name: "usage_input_tokens",
        read: |r| r.attributes.usage_input_tokens,
    },
    Field {
        name: "usage_output_tokens",
        read: |r| r.attributes.usage_output_tokens,
    },
    Field {
        name: "usage_cache_read_input_tokens",
        read: |r| r.attributes.usage_cache_read_input_tokens,
    },
    Field {
        name: "usage_cache_creation_input_tokens",
        read: |r| r.attributes.usage_cache_creation_input_tokens,
    },
    Field {
        name: "duration_ms",
        read: |r| payload_u64(&r.payload, &["duration_ms"]),
    },
];

fn quantile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// `ln(1+v)/ln(1+scale)` clamped — the exact transform the rebuilt lens applies.
fn count_norm(value: u64, scale: f64) -> f64 {
    ((1.0 + value as f64).ln() / (1.0 + scale).ln()).clamp(0.0, 1.0)
}

fn main() -> Result<(), Box<dyn Error>> {
    let parent = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: agent_event_scale_probe <vault-parent-dir>")?;
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }

    // Schema version 1, matching every other storage FSV in this directory.
    let db = synapse_storage::Db::open(&vault_dir, 1)?;
    let rows = db.scan_cf(synapse_storage::cf::CF_AGENT_EVENTS)?;
    println!("agent_event_scale_probe  (#1965)");
    println!("CF_AGENT_EVENTS rows on disk = {}", rows.len());

    let mut records = Vec::with_capacity(rows.len());
    let mut decode_failures = 0usize;
    for (_key, raw) in &rows {
        match serde_json::from_slice::<AgentEventRecord>(raw) {
            Ok(record) => records.push(record),
            Err(_) => decode_failures += 1,
        }
    }
    println!("decoded = {} unreadable = {decode_failures}", records.len());
    if records.is_empty() {
        return Err("no agent event rows decoded; nothing to ground a scale on".into());
    }

    println!(
        "\n{:<36} {:>7} {:>8} {:>9} {:>9} {:>10} {:>12}",
        "field", "present", "min", "median", "p90", "p99", "max"
    );
    // Carries the quantiles forward rather than re-deriving them per pass: a
    // by-name lookup back into FIELDS is a panic waiting for a typo, and the
    // values are already computed here.
    let mut chosen: Vec<(&str, u64, f64, u64, u64, u64)> = Vec::new();
    for field in FIELDS {
        let mut values: Vec<u64> = records.iter().filter_map(|r| (field.read)(r)).collect();
        values.sort_unstable();
        let present = values.len();
        let (min, med, p90, p99, max) = (
            values.first().copied().unwrap_or(0),
            quantile(&values, 0.50),
            quantile(&values, 0.90),
            quantile(&values, 0.99),
            values.last().copied().unwrap_or(0),
        );
        println!(
            "{:<36} {present:>7} {min:>8} {med:>9} {p90:>9} {p99:>10} {max:>12}",
            field.name
        );
        // The scale is the p99 rounded UP to a power of ten: above it the field
        // has stopped discriminating (only 1% of the corpus is out there), and a
        // round number keeps the frozen constant readable and defensible.
        let scale = if p99 == 0 {
            1.0
        } else {
            10f64.powf((p99 as f64).log10().ceil())
        };
        chosen.push((field.name, p99, scale, med, p90, max));
    }

    println!("\n-- proposed frozen scales (p99 rounded up to a power of ten) --");
    println!(
        "{:<36} {:>10} {:>14} {:>12} {:>12} {:>12}",
        "field", "p99", "scale", "norm(med)", "norm(p90)", "norm(max)"
    );
    for (name, p99, scale, med, p90, max) in &chosen {
        println!(
            "{name:<36} {p99:>10} {scale:>14.0} {:>12.4} {:>12.4} {:>12.4}",
            count_norm(*med, *scale),
            count_norm(*p90, *scale),
            count_norm(*max, *scale),
        );
    }

    // A scale is only useful if the transform SPREADS the corpus. If median and
    // p90 land on the same normalized value the field is not discriminating at
    // that scale, and the constant is wrong regardless of how principled its
    // derivation looked.
    println!("\n-- spread check: norm(p90) - norm(median), want clearly > 0 --");
    let mut flat = 0usize;
    for (name, _p99, scale, med, p90, _max) in &chosen {
        let spread = count_norm(*p90, *scale) - count_norm(*med, *scale);
        let verdict = if spread > 0.02 { "ok" } else { "FLAT" };
        if spread <= 0.02 {
            flat += 1;
        }
        println!("{name:<36} spread={spread:>8.4}  {verdict}");
    }
    println!("\nfields with no usable spread at the proposed scale: {flat}");

    // The clock field the defect is about, for the record.
    let mut ts: Vec<u64> = records.iter().map(|r| r.ts_ns / 1_000_000).collect();
    ts.sort_unstable();
    println!(
        "\nts_unix_ms (the field that owned the direction): min={} max={} span_ms={}",
        ts.first().copied().unwrap_or(0),
        ts.last().copied().unwrap_or(0),
        ts.last().copied().unwrap_or(0) - ts.first().copied().unwrap_or(0)
    );
    println!(
        "  magnitude ratio against the largest token p99: {:.3e}",
        ts.last().copied().unwrap_or(0) as f64
            / chosen
                .iter()
                .map(|(_, p99, ..)| *p99)
                .max()
                .unwrap_or(1)
                .max(1) as f64
    );

    Ok(())
}

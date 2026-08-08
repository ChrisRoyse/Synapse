//! Digests the cost rollup's per-spawn contribution markers for #2113 FSV.
//!
//! The markers under `CF_KV agent-cost/rollup/v2/mark/<spawn_id>` are the
//! canonical statement of what each spawn contributes to every rollup cell —
//! the cells themselves are nothing but the fold of these. So "the gated
//! incremental pass produced the same rollups as a from-scratch rebuild" is
//! exactly "these rows are byte-identical", and that is what this prints: a
//! sha256 over every marker row (key and value, in key order) plus the totals
//! the markers carry, so a mismatch names itself instead of just failing.
//!
//! Run against a vault no daemon holds open:
//!
//! ```text
//! cargo run -p synapse-mcp --example rollup_mark_digest_fsv -- <db-path>
//! ```

use sha2::{Digest as _, Sha256};
use synapse_storage::{Db, cf};

const MARK_PREFIX: &[u8] = b"agent-cost/rollup/v2/mark/";
const META_KEY: &[u8] = b"agent-cost/rollup/v2/__meta";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::args()
        .nth(1)
        .ok_or("usage: rollup_mark_digest_fsv <db-path>")?;
    let db = Db::open(std::path::Path::new(&db_path), synapse_core::SCHEMA_VERSION)?;

    let mut rows = db.scan_cf_prefix(cf::CF_KV, MARK_PREFIX)?;
    rows.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    let mut cells = 0_u64;
    let mut spawns_total = 0_u64;
    let mut spawns_complete = 0_u64;
    let mut spawns_incomplete = 0_u64;
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut source_micro_usd = 0_u64;
    for (key, value) in &rows {
        hasher.update((key.len() as u64).to_be_bytes());
        hasher.update(key);
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
        let mark: serde_json::Value = serde_json::from_slice(value)?;
        for cell in mark
            .get("cells")
            .and_then(serde_json::Value::as_array)
            .ok_or("marker has no cells array")?
        {
            cells += 1;
            let counters = cell.get("counters").ok_or("cell has no counters")?;
            let read = |name: &str| counters.get(name).and_then(serde_json::Value::as_u64);
            // Only meta cells (no model) carry the spawn counts; per-model cells
            // carry the usage. Summing each where it lives keeps the totals
            // reconcilable with `cost summarize` rather than double-counting.
            if cell.get("model").is_none() || cell.get("model").is_some_and(|m| m.is_null()) {
                spawns_total += read("spawns_total").unwrap_or(0);
                spawns_complete += read("spawns_complete").unwrap_or(0);
                spawns_incomplete += read("spawns_incomplete").unwrap_or(0);
            }
            let usage = counters.get("usage");
            input_tokens += usage
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            output_tokens += usage
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            source_micro_usd += read("source_reported_micro_usd").unwrap_or(0);
        }
    }
    let digest = hasher.finalize();

    let meta = db.get_cf(cf::CF_KV, META_KEY)?;
    let (schema_version, complete, spawns_marked, sealed_horizon_ns) = match &meta {
        Some(bytes) => {
            let meta: serde_json::Value = serde_json::from_slice(bytes)?;
            (
                meta.get("schema_version").cloned(),
                meta.get("complete").cloned(),
                meta.get("spawns_marked").cloned(),
                meta.get("sealed_horizon_ns").cloned(),
            )
        }
        None => (None, None, None, None),
    };

    println!(
        "marks={} mark_digest_sha256={} cells={cells} spawns_total={spawns_total} spawns_complete={spawns_complete} spawns_incomplete={spawns_incomplete} input_tokens={input_tokens} output_tokens={output_tokens} source_micro_usd={source_micro_usd}",
        rows.len(),
        hex(&digest)
    );
    println!(
        "meta: schema_version={schema_version:?} complete={complete:?} spawns_marked={spawns_marked:?} sealed_horizon_ns={sealed_horizon_ns:?}"
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

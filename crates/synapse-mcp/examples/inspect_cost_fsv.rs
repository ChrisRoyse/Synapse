//! Independently recomputes the deterministic manual-FSV corpus from raw rows.

use synapse_core::{AgentTranscriptRecord, TranscriptRole};
use synapse_storage::{Db, cf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::args()
        .nth(1)
        .ok_or("usage: inspect_cost_fsv <db-path>")?;
    let db = Db::open(std::path::Path::new(&db_path), synapse_core::SCHEMA_VERSION)?;
    let rows = db.scan_cf(cf::CF_AGENT_TRANSCRIPTS)?;
    let mut result_rows = 0_u64;
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut source_micro_usd = 0_u64;
    for (_key, value) in &rows {
        let row: AgentTranscriptRecord = serde_json::from_slice(value)?;
        row.validate()?;
        if row.role != Some(TranscriptRole::Result) {
            continue;
        }
        result_rows += 1;
        let usage = row.usage.ok_or("result row missing usage")?;
        input_tokens = input_tokens
            .checked_add(usage.input_tokens.unwrap_or(0))
            .ok_or("input token sum overflow")?;
        output_tokens = output_tokens
            .checked_add(usage.output_tokens.unwrap_or(0))
            .ok_or("output token sum overflow")?;
        source_micro_usd = source_micro_usd
            .checked_add(usage.total_cost_micro_usd.unwrap_or(0))
            .ok_or("source cost sum overflow")?;
    }
    println!(
        "physical_rows={} result_rows={result_rows} input_tokens={input_tokens} output_tokens={output_tokens} total_tokens={} source_micro_usd={source_micro_usd}",
        rows.len(),
        input_tokens + output_tokens
    );
    Ok(())
}

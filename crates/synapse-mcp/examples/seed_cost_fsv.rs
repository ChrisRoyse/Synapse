//! Seeds a deterministic, structurally valid cost corpus for manual FSV.

use sha2::{Digest as _, Sha256};
use synapse_core::{AgentTranscriptRecord, TranscriptRole, TranscriptSource, TranscriptUsage};
use synapse_storage::{Db, agent_transcripts::agent_transcript_key, cf};

const DEFAULT_SPAWNS: u64 = 1_000;
const DEFAULT_ROWS_PER_SPAWN: u64 = 200;
const BATCH_ROWS: usize = 2_000;
const BASE_TS_NS: u64 = 1_700_000_000_000_000_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::args()
        .nth(1)
        .ok_or("usage: seed_cost_fsv <db-path> [spawns] [rows-per-spawn]")?;
    let spawns = parse_arg(2, DEFAULT_SPAWNS)?;
    let rows_per_spawn = parse_arg(3, DEFAULT_ROWS_PER_SPAWN)?;
    if spawns == 0 || rows_per_spawn == 0 {
        return Err("spawns and rows-per-spawn must both be positive".into());
    }
    let db = Db::open(std::path::Path::new(&db_path), synapse_core::SCHEMA_VERSION)?;
    let mut batch = Vec::with_capacity(BATCH_ROWS);
    let mut row_count = 0_u64;
    for spawn_index in 0..spawns {
        let spawn_id = format!("agent-spawn-fsv-{spawn_index:08}");
        for line_no in 1..=rows_per_spawn {
            let ts_ns = BASE_TS_NS
                .checked_add(spawn_index)
                .and_then(|value| value.checked_add(line_no * spawns))
                .ok_or("timestamp overflow")?;
            let raw = format!("fsv:{spawn_index}:{line_no}");
            let mut row = AgentTranscriptRecord::new(
                ts_ns,
                spawn_id.clone(),
                line_no,
                TranscriptSource::ClaudeStreamJson,
                raw.len() as u64,
                hex_sha256(raw.as_bytes()),
            );
            if line_no == rows_per_spawn {
                row.role = Some(TranscriptRole::Result);
                row.event_kind = Some("result/success".to_owned());
                row.model = Some("fsv-model".to_owned());
                row.usage = Some(TranscriptUsage {
                    input_tokens: Some(100),
                    output_tokens: Some(50),
                    total_cost_micro_usd: Some(123),
                    ..TranscriptUsage::default()
                });
            }
            row.validate()?;
            batch.push((
                agent_transcript_key(&spawn_id, line_no),
                serde_json::to_vec(&row)?,
            ));
            row_count += 1;
            if batch.len() == BATCH_ROWS {
                db.put_batch_pressure_bypass(cf::CF_AGENT_TRANSCRIPTS, std::mem::take(&mut batch))?;
            }
        }
    }
    if !batch.is_empty() {
        db.put_batch_pressure_bypass(cf::CF_AGENT_TRANSCRIPTS, batch)?;
    }
    let physical_rows = db.scan_cf(cf::CF_AGENT_TRANSCRIPTS)?;
    if physical_rows.len() as u64 != row_count {
        return Err(format!(
            "SEED_COST_FSV_ROWCOUNT_MISMATCH: wrote {row_count}, physical read found {}",
            physical_rows.len()
        )
        .into());
    }
    println!(
        "rows={row_count} spawns={spawns} rows_per_spawn={rows_per_spawn} expected_input_tokens={} expected_output_tokens={} expected_total_tokens={} expected_source_micro_usd={} physical_rows={}",
        spawns * 100,
        spawns * 50,
        spawns * 150,
        spawns * 123,
        physical_rows.len()
    );
    Ok(())
}

fn parse_arg(index: usize, default: u64) -> Result<u64, Box<dyn std::error::Error>> {
    std::env::args()
        .nth(index)
        .map_or(Ok(default), |value| Ok(value.parse()?))
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

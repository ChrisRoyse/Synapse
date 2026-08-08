//! Seeds a realistic `CF_AGENT_EVENTS` corpus for the #2117 end-state anchoring
//! FSV, and reports the exact terminal-row census the daemon must reproduce.
//!
//! The pathology #2117 fixes is that the end-state anchor ran **two full
//! materializations of the whole family plus two full JSON decodes of it, per
//! terminal record** to find the handful of rows belonging to one spawn — the
//! spawn id is not in the key (`ts_ns || seq`), so every row had to be decoded
//! and 99.9% discarded. Reproducing that costs a family big enough for the
//! difference between "twice per record" and "once per batch" to be visible,
//! which is what this writes.
//!
//! Two spawns are named in the corpus and both are seeded with terminal events:
//!
//! * `--terminal-spawn` gets a single `exited/success`, the ordinary case.
//! * `--conflict-spawn` gets an EARLIER `killed` and a LATER `exited/success`,
//!   written in that key order, so the anchor path must emit
//!   `AGENT_EVENT_TERMINAL_OUTCOME_CONFLICT` and keep the earliest observation
//!   canonical (#2072). The conflicting pair is the case most likely to expose
//!   an ordering difference between the old per-spawn scans and the shared
//!   materialization, which is exactly why it is here.
//!
//! ```text
//! cargo run -p synapse-mcp --example seed_agent_events_fsv -- <db-path> [rows] [spawns]
//! ```

use synapse_core::{AgentEndState, AgentEventKind, AgentEventRecord};
use synapse_storage::{Db, agent_events::agent_event_key, cf};

const DEFAULT_ROWS: u64 = 46_000;
const DEFAULT_SPAWNS: u64 = 900;
const BATCH_ROWS: usize = 2_000;
const BASE_TS_NS: u64 = 1_700_000_000_000_000_000;

const TERMINAL_SPAWN: &str = "agent-spawn-fsvinject-0002";
const CONFLICT_SPAWN: &str = "agent-spawn-fsvconflict-0003";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::args()
        .nth(1)
        .ok_or("usage: seed_agent_events_fsv <db-path> [rows] [spawns]")?;
    let rows = parse_arg(2, DEFAULT_ROWS)?;
    let spawns = parse_arg(3, DEFAULT_SPAWNS)?;
    if rows == 0 || spawns == 0 {
        return Err("rows and spawns must both be positive".into());
    }
    let db = Db::open(std::path::Path::new(&db_path), synapse_core::SCHEMA_VERSION)?;

    let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(BATCH_ROWS);
    let mut seq = 0_u32;
    let mut written = 0_u64;
    // Noise: ordinary non-terminal lifecycle traffic spread across `spawns`
    // spawns, none of which the anchor path may ever anchor.
    for index in 0..rows {
        let spawn_index = index % spawns;
        let ts_ns = BASE_TS_NS + index * 1_000;
        let mut record = AgentEventRecord {
            record_version: 1,
            ts_ns,
            kind: AgentEventKind::ToolCallFinished,
            session_id: Some(format!("fsv-session-{spawn_index:05}")),
            spawn_id: Some(format!("agent-spawn-fsvnoise-{spawn_index:05}")),
            reason_code: None,
            end_state: None,
            state_from: None,
            state_to: None,
            attributes: synapse_core::GenAiAttributes::default(),
            payload: serde_json::json!({ "fsv_index": index }),
        };
        record.attributes.tool_name = Some("fsv_tool".to_owned());
        batch.push((agent_event_key(ts_ns, seq), serde_json::to_vec(&record)?));
        seq = seq.wrapping_add(1);
        written += 1;
        if batch.len() == BATCH_ROWS {
            db.put_batch(cf::CF_AGENT_EVENTS, std::mem::take(&mut batch))?;
        }
    }

    // The ordinary terminal case.
    let terminal_ts = BASE_TS_NS + rows * 1_000 + 1_000_000;
    batch.push(terminal_row(TERMINAL_SPAWN, terminal_ts, seq, true)?);
    seq = seq.wrapping_add(1);
    written += 1;

    // The conflicting pair: `killed` first in both key order and time order,
    // then a later disagreeing `exited/success`. The declared rule is
    // earliest-wins, so `killed` must stay canonical.
    let conflict_first_ts = terminal_ts + 1_000_000;
    let conflict_second_ts = conflict_first_ts + 5_000_000;
    batch.push(terminal_row(CONFLICT_SPAWN, conflict_first_ts, seq, false)?);
    seq = seq.wrapping_add(1);
    batch.push(terminal_row(CONFLICT_SPAWN, conflict_second_ts, seq, true)?);
    written += 2;
    db.put_batch(cf::CF_AGENT_EVENTS, std::mem::take(&mut batch))?;
    db.flush()?;

    // Physical census: the exact answer the daemon's anchor path must produce.
    let physical = db.scan_cf(cf::CF_AGENT_EVENTS)?;
    let mut terminal_rows_for_terminal_spawn = 0_u64;
    let mut terminal_rows_for_conflict_spawn = 0_u64;
    for (_key, value) in &physical {
        let record: AgentEventRecord = serde_json::from_slice(value)?;
        let terminal = matches!(record.kind, AgentEventKind::Killed | AgentEventKind::Exited);
        if !terminal {
            continue;
        }
        match record.spawn_id.as_deref() {
            Some(TERMINAL_SPAWN) => terminal_rows_for_terminal_spawn += 1,
            Some(CONFLICT_SPAWN) => terminal_rows_for_conflict_spawn += 1,
            _ => {}
        }
    }
    println!(
        "written={written} physical_rows={} terminal_spawn={TERMINAL_SPAWN} terminal_rows={terminal_rows_for_terminal_spawn} conflict_spawn={CONFLICT_SPAWN} conflict_terminal_rows={terminal_rows_for_conflict_spawn} expected_canonical_conflict_outcome=killed",
        physical.len()
    );
    Ok(())
}

fn terminal_row(
    spawn_id: &str,
    ts_ns: u64,
    seq: u32,
    success: bool,
) -> Result<(Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
    let record = AgentEventRecord {
        record_version: 1,
        ts_ns,
        kind: if success {
            AgentEventKind::Exited
        } else {
            AgentEventKind::Killed
        },
        session_id: Some(format!("fsv-session-{spawn_id}")),
        spawn_id: Some(spawn_id.to_owned()),
        reason_code: Some("fsv_seeded_terminal".to_owned()),
        end_state: success.then_some(AgentEndState::Success),
        state_from: None,
        state_to: None,
        attributes: synapse_core::GenAiAttributes::default(),
        payload: serde_json::json!({ "fsv": true }),
    };
    Ok((agent_event_key(ts_ns, seq), serde_json::to_vec(&record)?))
}

fn parse_arg(index: usize, default: u64) -> Result<u64, Box<dyn std::error::Error>> {
    match std::env::args().nth(index) {
        Some(value) => Ok(value.parse::<u64>()?),
        None => Ok(default),
    }
}

//! Writes / appends real Claude stream-json spawn transcripts with **exact
//! control over every row's `ts_ns`**, for the #2142 cost-rollup FSV.
//!
//! The #2142 gate turns on one fact about a committed chunk: whether any of its
//! rows carries a priced timestamp below the sealed horizon. Proving it
//! therefore needs two writers that differ in that one respect and in nothing
//! else — same ingest path, same parser, same commit site, same cadence — which
//! is what this produces. Nothing here writes to the vault: it writes the
//! on-disk spawn artifacts `act_spawn_agent` writes at launch, and the daemon's
//! own periodic ingester does the rest, so the rows reach
//! `CF_AGENT_TRANSCRIPTS` through `commit_transcript_chunk` exactly as
//! production rows do.
//!
//! Every line carries an explicit top-level `ts_ns`, which is the first field
//! `transcript_source_ts_ns` consults, so the priced timestamp of each row is
//! the number passed on the command line rather than a function of when the run
//! happened.
//!
//! ```text
//! cargo run -p synapse-mcp --example seed_transcript_spawns_fsv -- \
//!     <spawn-root> <spawn-id> <first-ts-ns> <lines> <open|complete>
//! ```
//!
//! * `open` — `system/init` (first write only) then `lines` `assistant` rows.
//!   A spawn with no `result` row does not resolve, so it contributes a cell
//!   only once its latest evidence falls below the sealed horizon.
//! * `complete` — the same, plus a terminal `result/success` row carrying
//!   usage, which resolves the spawn and gives it a full contribution.
//!
//! Appending to an existing spawn appends to its `stdout.jsonl` and leaves the
//! launch artifacts alone, which is exactly what a live session does.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// 1 ms between consecutive lines of one write, so line order and timestamp
/// order agree without any line escaping the hour the caller asked for.
const LINE_TS_STRIDE_NS: u64 = 1_000_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        return Err("usage: seed_transcript_spawns_fsv <spawn-root> <spawn-id> <first-ts-ns> <lines> <open|complete>".into());
    }
    let root = PathBuf::from(&args[1]);
    let spawn_id = args[2].clone();
    let first_ts_ns: u64 = args[3].parse()?;
    let lines: u64 = args[4].parse()?;
    let complete = match args[5].as_str() {
        "open" => false,
        "complete" => true,
        other => return Err(format!("mode must be `open` or `complete`, got {other:?}").into()),
    };
    if !spawn_id.starts_with("agent-spawn-") {
        return Err("spawn id must start with `agent-spawn-` or the ingester rejects it".into());
    }

    let dir = root.join(&spawn_id);
    let stdout_path = dir.join("stdout.jsonl");
    let fresh = !stdout_path.exists();
    std::fs::create_dir_all(&dir)?;
    if fresh {
        write_launch_artifacts(&dir, &spawn_id, first_ts_ns)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_path)?;

    let mut ts = first_ts_ns;
    let mut written = 0_u64;
    if fresh {
        writeln!(
            file,
            r#"{{"type":"system","subtype":"init","session_id":"{spawn_id}","model":"claude-opus-4-20250514","ts_ns":{ts}}}"#
        )?;
        ts += LINE_TS_STRIDE_NS;
        written += 1;
    }
    for index in 0..lines {
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"id":"msg_{spawn_id}_{index}","model":"claude-opus-4-20250514","content":[{{"type":"text","text":"fsv2142 turn {index}"}}],"usage":{{"input_tokens":1200,"output_tokens":340,"cache_read_input_tokens":800}}}},"ts_ns":{ts}}}"#
        )?;
        ts += LINE_TS_STRIDE_NS;
        written += 1;
    }
    if complete {
        writeln!(
            file,
            r#"{{"type":"result","subtype":"success","result":"fsv2142 complete","total_cost_usd":0.125,"usage":{{"input_tokens":4200,"output_tokens":1100,"cache_read_input_tokens":2600,"cache_creation_input_tokens":300}},"ts_ns":{ts}}}"#
        )?;
        written += 1;
    }
    file.flush()?;
    file.sync_all()?;

    let bytes = std::fs::metadata(&stdout_path)?.len();
    println!(
        "spawn={spawn_id} dir={} fresh={fresh} lines_written={written} first_ts_ns={first_ts_ns} last_ts_ns={ts} stdout_bytes={bytes}",
        dir.display()
    );
    Ok(())
}

/// The two artifacts the ingester requires before it will read a spawn dir: the
/// launch manifest (source of truth for the model / creation time) and one
/// Claude launch marker, which is how `detect_source` attributes the dir to the
/// stream-json parser rather than guessing a format.
fn write_launch_artifacts(dir: &Path, spawn_id: &str, first_ts_ns: u64) -> std::io::Result<()> {
    let created_unix_ms = first_ts_ns / 1_000_000;
    std::fs::write(
        dir.join("spawn-manifest.json"),
        format!(
            r#"{{"spawn_id":"{spawn_id}","model":"claude-opus-4-20250514","created_unix_ms":{created_unix_ms}}}"#
        ),
    )?;
    std::fs::write(dir.join("claude-mcp-config.json"), "{\"mcpServers\":{}}")?;
    Ok(())
}

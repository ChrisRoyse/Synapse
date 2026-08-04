//! Independent physical readback for action Oracle evidence FSV (#1678/#2005).

use std::{error::Error, path::PathBuf};

use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, RecurrenceSubjectKind, cf};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: action_oracle_readback_fsv <vault-dir> <action>")?;
    let action = args
        .next()
        .ok_or("usage: action_oracle_readback_fsv <vault-dir> <action>")?;
    if args.next().is_some() {
        return Err("usage: action_oracle_readback_fsv <vault-dir> <action>".into());
    }

    let db = Db::open(&root, SCHEMA_VERSION)?;
    let action_rows = db.scan_cf_prefix(cf::CF_ACTION_LOG, b"")?;
    println!("ACTION_LOG rows={}", action_rows.len());
    for (key, value) in action_rows {
        println!(
            "ACTION_ROW key_hex={} value={}",
            synapse_storage::constellations::hex_encode(&key),
            String::from_utf8_lossy(&value)
        );
    }

    let readback = db.read_recurrence_subject_series(RecurrenceSubjectKind::Action, &action)?;
    println!(
        "ORACLE_ACTION action={} cx_id={} latest_seq={} frequency={} active_occurrences={}",
        action,
        readback.cx_id,
        readback.latest_seq,
        readback.series.series.frequency,
        readback.series.series.occurrences.len()
    );
    for occurrence in readback.series.series.occurrences {
        println!(
            "ORACLE_OCCURRENCE id={} t_k={} context={}",
            occurrence.id.0,
            occurrence.t_k.0,
            String::from_utf8_lossy(&occurrence.context.bytes)
        );
    }
    Ok(())
}

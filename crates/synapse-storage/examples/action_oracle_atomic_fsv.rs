//! Atomic source/constellation/recurrence publication FSV (#2005).

use std::{error::Error, path::PathBuf};

use serde_json::json;
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, RecurrenceSubjectKind, cf, encode_json};

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: action_oracle_atomic_fsv <vault-dir>")?;
    let db = Db::open(&root, SCHEMA_VERSION)?;
    let event_time_ns = 1_785_865_000_000_000_000_u64;
    let source_key = event_time_ns.to_be_bytes().to_vec();
    let record = json!({
        "schema_version": 1,
        "audit_id": "atomic-fsv-1",
        "ts_ns": event_time_ns,
        "seq": 1,
        "tool": "atomic_fsv_action",
        "status": "ok",
        "error_code": null,
        "details": {"expected": "success"}
    });
    let bytes = encode_json(&record)?;
    let context = br#"{"action_id":"atomic_fsv_action","outcome_anchor":{"value":{"bool":true}},"source_action_audit_key_hex":"18c8"}"#;

    println!("HAPPY before_latest_seq={}", latest_seq(&db)?);
    let publication = db.put_action_oracle_publication(
        &source_key,
        &bytes,
        &record,
        event_time_ns,
        &source_key,
        context,
    )?;
    println!("HAPPY publication={publication:?}");
    let source = db.get_cf(cf::CF_ACTION_LOG, &source_key)?;
    let series =
        db.read_recurrence_subject_series(RecurrenceSubjectKind::Action, "atomic_fsv_action")?;
    let reproduced = db.reproduce_calyx_record(&publication.constellation_cx_id)?;
    let chain = db.verify_calyx_ledger_chain(None)?;
    println!(
        "HAPPY_READBACK latest_seq={} source_exact={} frequency={} active_occurrences={} reproduced={reproduced:?} chain={chain:?}",
        latest_seq(&db)?,
        source.as_deref() == Some(bytes.as_slice()),
        series.series.series.frequency,
        series.series.series.occurrences.len(),
    );

    let oversized_key = b"oversized";
    let oversized_record = terminal("atomic_fsv_oversized", event_time_ns + 1);
    let oversized_bytes = encode_json(&oversized_record)?;
    println!("OVERSIZED before_latest_seq={}", latest_seq(&db)?);
    let oversized = db.put_action_oracle_publication(
        oversized_key,
        &oversized_bytes,
        &oversized_record,
        event_time_ns + 1,
        oversized_key,
        &vec![b'x'; 257],
    );
    println!(
        "OVERSIZED error_code={} after_latest_seq={} source_present={}",
        oversized
            .as_ref()
            .err()
            .map_or("none", |error| error.code()),
        latest_seq(&db)?,
        db.get_cf(cf::CF_ACTION_LOG, oversized_key)?.is_some()
    );

    let empty_key = b"empty-tool";
    let empty_record = terminal("", event_time_ns + 2);
    let empty_bytes = encode_json(&empty_record)?;
    println!("EMPTY_TOOL before_latest_seq={}", latest_seq(&db)?);
    let empty = db.put_action_oracle_publication(
        empty_key,
        &empty_bytes,
        &empty_record,
        event_time_ns + 2,
        empty_key,
        context,
    );
    println!(
        "EMPTY_TOOL error_code={} after_latest_seq={} source_present={}",
        empty.as_ref().err().map_or("none", |error| error.code()),
        latest_seq(&db)?,
        db.get_cf(cf::CF_ACTION_LOG, empty_key)?.is_some()
    );

    println!("DUPLICATE before_latest_seq={}", latest_seq(&db)?);
    let duplicate = db.put_action_oracle_publication(
        &source_key,
        &bytes,
        &record,
        event_time_ns,
        &source_key,
        context,
    );
    let after_series =
        db.read_recurrence_subject_series(RecurrenceSubjectKind::Action, "atomic_fsv_action")?;
    println!(
        "DUPLICATE error_code={} after_latest_seq={} frequency={} active_occurrences={}",
        duplicate
            .as_ref()
            .err()
            .map_or("none", |error| error.code()),
        latest_seq(&db)?,
        after_series.series.series.frequency,
        after_series.series.series.occurrences.len()
    );
    Ok(())
}

fn terminal(tool: &str, ts_ns: u64) -> serde_json::Value {
    json!({
        "schema_version": 1,
        "audit_id": format!("atomic-fsv-{ts_ns}"),
        "ts_ns": ts_ns,
        "seq": 1,
        "tool": tool,
        "status": "error",
        "error_code": "SYNTHETIC",
        "details": {}
    })
}

fn latest_seq(db: &Db) -> Result<u64, Box<dyn Error>> {
    Ok(db
        .calyx_vault_inspect()?
        .ok_or("Calyx vault inspect unexpectedly absent")?
        .latest_seq)
}

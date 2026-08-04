//! Real-vault boundary audit for action Oracle recurrence evidence (#1678).

use std::{
    error::Error,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, RecurrenceSubjectKind};

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: action_oracle_edges_fsv <vault-dir>")?;
    let db = Db::open(&root, SCHEMA_VERSION)?;
    let now_ns = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;

    println!("EDGE_EMPTY before_latest_seq={}", latest_seq(&db)?);
    let empty = db.put_recurrence_subject_occurrence(
        RecurrenceSubjectKind::Action,
        "",
        now_ns,
        b"empty-action",
        br#"{"action_id":"","outcome_anchor":{"value":{"bool":true}}}"#,
    );
    println!(
        "EDGE_EMPTY error_code={} after_latest_seq={}",
        empty.as_ref().err().map_or("none", |error| error.code()),
        latest_seq(&db)?
    );

    let max_action = "x".repeat(200);
    println!("EDGE_MAX before_latest_seq={}", latest_seq(&db)?);
    db.put_recurrence_subject_occurrence(
        RecurrenceSubjectKind::Action,
        &max_action,
        now_ns.saturating_add(1_000_000_000),
        b"max-action-1",
        br#"{"action_id":"max","outcome_anchor":{"value":{"bool":true}}}"#,
    )?;
    let max_series =
        db.read_recurrence_subject_series(RecurrenceSubjectKind::Action, &max_action)?;
    println!(
        "EDGE_MAX after_latest_seq={} frequency={} active_occurrences={}",
        latest_seq(&db)?,
        max_series.series.series.frequency,
        max_series.series.series.occurrences.len()
    );

    let conflict_action = "edge_conflict";
    db.put_recurrence_subject_occurrence(
        RecurrenceSubjectKind::Action,
        conflict_action,
        now_ns.saturating_add(2_000_000_000),
        b"same-identity",
        br#"{"action_id":"edge_conflict","outcome_anchor":{"value":{"bool":true}}}"#,
    )?;
    let before =
        db.read_recurrence_subject_series(RecurrenceSubjectKind::Action, conflict_action)?;
    println!(
        "EDGE_CONFLICT before_latest_seq={} frequency={} active_occurrences={}",
        latest_seq(&db)?,
        before.series.series.frequency,
        before.series.series.occurrences.len()
    );
    let conflict = db.put_recurrence_subject_occurrence(
        RecurrenceSubjectKind::Action,
        conflict_action,
        now_ns.saturating_add(3_000_000_000),
        b"same-identity",
        br#"{"action_id":"edge_conflict","outcome_anchor":{"value":{"bool":false}}}"#,
    );
    let after =
        db.read_recurrence_subject_series(RecurrenceSubjectKind::Action, conflict_action)?;
    println!(
        "EDGE_CONFLICT error_code={} after_latest_seq={} frequency={} active_occurrences={}",
        conflict.as_ref().err().map_or("none", |error| error.code()),
        latest_seq(&db)?,
        after.series.series.frequency,
        after.series.series.occurrences.len()
    );
    Ok(())
}

fn latest_seq(db: &Db) -> Result<u64, Box<dyn Error>> {
    Ok(db
        .calyx_vault_inspect()?
        .ok_or("Calyx vault inspect unexpectedly absent")?
        .latest_seq)
}

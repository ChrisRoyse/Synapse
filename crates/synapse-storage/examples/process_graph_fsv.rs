//! Manual Full State Verification for the scheduled process-parent graph (#1685).

use std::{error::Error, path::PathBuf, sync::Arc};

use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, cf, constellations::SYN_GRAPHPOS_PROCESS_PANEL_VERSION};

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: process_graph_fsv <new-empty-vault-dir>")?;
    let db = Arc::new(Db::open(&dir, SCHEMA_VERSION)?);
    synapse_storage::derived_state::register_derived_state_source(&db);

    println!(
        "SOURCE_OF_TRUTH vault={} process_rows_before={} lifecycle_before={}",
        dir.display(),
        db.scan_cf_prefix(cf::CF_PROCESS_HISTORY, b"")?.len(),
        db.read_panel_lifecycle(SYN_GRAPHPOS_PROCESS_PANEL_VERSION)?
            .is_some()
    );
    println!("INVALID_TIMESTAMP before_rows=0 value=not-a-timestamp");
    let invalid_raw = br#"{"pid":9,"ts_ns":"not-a-timestamp"}"#;
    let invalid_record = serde_json::from_slice(invalid_raw)?;
    let invalid = db.put_process_constellation(b"invalid", invalid_raw, &invalid_record);
    match invalid {
        Ok(report) => {
            return Err(
                format!("invalid process timestamp unexpectedly committed: {report:?}").into(),
            );
        }
        Err(error) => println!("INVALID_TIMESTAMP error={error}"),
    }
    println!(
        "INVALID_TIMESTAMP after_rows={}",
        db.scan_cf_prefix(cf::CF_PROCESS_HISTORY, b"")?.len()
    );
    db.put_batch(
        cf::CF_PROCESS_HISTORY,
        [
            (b"p10".to_vec(), br#"{"pid":10,"parent_pid":1}"#.to_vec()),
            (b"p20".to_vec(), br#"{"pid":20,"parent_pid":10}"#.to_vec()),
            (b"p30".to_vec(), br#"{"pid":30,"ppid":10}"#.to_vec()),
            (
                b"p40".to_vec(),
                br#"{"pid":40,"parent_pid":10,"ts_ns":1785955000000000000}"#.to_vec(),
            ),
        ],
    )?;
    println!(
        "TRIGGER_INPUT process_rows_after_put={}",
        db.scan_cf_prefix(cf::CF_PROCESS_HISTORY, b"")?.len()
    );
    let temporal = db.backfill_temporal_metadata(cf::CF_PROCESS_HISTORY, None, None, 10)?;
    println!("TEMPORAL_READBACK report={temporal:?}");
    if temporal.temporal_ineligible_rows != 3 || temporal.examined_rows != 4 {
        return Err(format!(
            "expected three timestamp-absent rows to be explicitly ineligible: {temporal:?}"
        )
        .into());
    }

    let maintenance = synapse_storage::derived_state::run_derived_state_maintenance_once();
    println!("TRIGGER maintenance={maintenance:?}");

    let source_rows = db.scan_cf_prefix(cf::CF_PROCESS_HISTORY, b"")?;
    for (key, value) in &source_rows {
        println!(
            "PHYSICAL_SOURCE key={} value={}",
            String::from_utf8_lossy(key),
            String::from_utf8_lossy(value)
        );
    }
    let lifecycle = db
        .read_panel_lifecycle(SYN_GRAPHPOS_PROCESS_PANEL_VERSION)?
        .ok_or("scheduled process graph did not publish a lifecycle snapshot")?;
    println!(
        "PHYSICAL_LIFECYCLE panel={} added_lenses={} state={}",
        SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
        lifecycle.added_lenses.len(),
        serde_json::to_string(&lifecycle)?
    );
    if lifecycle.added_lenses.len() != 2 {
        return Err(format!(
            "expected two structural lenses, observed {}",
            lifecycle.added_lenses.len()
        )
        .into());
    }
    Ok(())
}

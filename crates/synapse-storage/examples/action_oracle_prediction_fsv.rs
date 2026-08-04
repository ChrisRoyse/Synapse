//! Grounded action Oracle prediction/reverse-query FSV (#1678, #2006).

use std::{error::Error, path::PathBuf};

use calyx_aster::cf::ColumnFamily;
use serde_json::{Value, json};
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, SYN_ACTION_PANEL_VERSION, cf, encode_json};

const ROWS_PER_CLASS: u64 = 200;
const DAY_NS: u64 = 86_400_000_000_000;

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: action_oracle_prediction_fsv <empty-vault-dir>")?;
    let db = Db::open(&root, SCHEMA_VERSION)?;

    println!("BEFORE seq={} action_rows={}", seq(&db)?, action_rows(&db)?);
    let readiness_absent_before = seq(&db)?;
    println!(
        "READINESS_ABSENT before_seq={readiness_absent_before} after_seq={} present={}",
        seq(&db)?,
        db.oracle_readiness()?.is_some()
    );

    let empty_before = seq(&db)?;
    let empty = db.oracle_predict_action("");
    println!(
        "EDGE_EMPTY before_seq={empty_before} after_seq={} code={}",
        seq(&db)?,
        empty.as_ref().err().map_or("none", |error| error.code())
    );

    let unknown_before = seq(&db)?;
    let unknown = db.oracle_predict_action("never_observed");
    println!(
        "EDGE_NO_CORPUS before_seq={unknown_before} after_seq={} code={}",
        seq(&db)?,
        unknown.as_ref().err().map_or("none", |error| error.code())
    );

    let mut completion_cx = None;
    for index in 0..ROWS_PER_CLASS * 2 {
        let outcome = index >= ROWS_PER_CLASS;
        let tool = if outcome { "fsv_succeeds" } else { "fsv_fails" };
        // Separate classes by twelve hours while retaining unique within-class
        // positions. The cyclic time lenses therefore carry known grounded
        // signal and remain non-degenerate for the continuous estimator.
        let class_index = index % ROWS_PER_CLASS;
        let ts_ns = 1_783_000_000_000_000_000
            + class_index * DAY_NS
            + if outcome {
                15 * 3_600_000_000_000
            } else {
                3 * 3_600_000_000_000
            }
            + class_index * 1_000_000_000;
        let cx_id = publish(&db, index, ts_ns, tool, outcome)?;
        if outcome && completion_cx.is_none() {
            completion_cx = Some(cx_id);
        }
    }
    println!(
        "INGEST_READBACK seq={} action_rows={} expected={}",
        seq(&db)?,
        action_rows(&db)?,
        ROWS_PER_CLASS * 2
    );

    let predict_before = seq(&db)?;
    let prediction = db.oracle_predict_action("fsv_succeeds")?;
    println!(
        "HAPPY_PREDICT before_seq={predict_before} after_seq={} result={prediction}",
        seq(&db)?
    );
    let reverse = db.oracle_reverse_action(false)?;
    println!("HAPPY_REVERSE after_seq={} result={reverse}", seq(&db)?);

    let completion_cx = completion_cx.ok_or("completion fixture cx missing")?;
    let complete_before = seq(&db)?;
    let completion = db.oracle_complete_action(&completion_cx, &[50])?;
    println!(
        "HAPPY_COMPLETE before_seq={complete_before} after_seq={} cx_id={completion_cx} result={completion}",
        seq(&db)?
    );

    let readiness_before = seq(&db)?;
    let readiness = db.oracle_measure_readiness()?;
    println!(
        "HAPPY_READINESS before_seq={readiness_before} after_seq={} result={readiness}",
        seq(&db)?
    );
    let readiness_read_before = seq(&db)?;
    let readiness_read = db.oracle_readiness()?.ok_or("readiness snapshot missing")?;
    println!(
        "READINESS_READBACK before_seq={readiness_read_before} after_seq={} result={readiness_read}",
        seq(&db)?
    );

    let empty_free_before = seq(&db)?;
    let empty_free = db.oracle_complete_action(&completion_cx, &[]);
    println!(
        "EDGE_COMPLETE_EMPTY_FREE before_seq={empty_free_before} after_seq={} code={}",
        seq(&db)?,
        empty_free
            .as_ref()
            .err()
            .map_or("none", |error| error.code())
    );
    let unknown_slot_before = seq(&db)?;
    let unknown_slot = db.oracle_complete_action(&completion_cx, &[999]);
    println!(
        "EDGE_COMPLETE_UNKNOWN_SLOT before_seq={unknown_slot_before} after_seq={} code={}",
        seq(&db)?,
        unknown_slot
            .as_ref()
            .err()
            .map_or("none", |error| error.code())
    );
    let invalid_cx_before = seq(&db)?;
    let invalid_cx = db.oracle_complete_action("not-a-cx", &[50]);
    println!(
        "EDGE_COMPLETE_INVALID_CX before_seq={invalid_cx_before} after_seq={} code={}",
        seq(&db)?,
        invalid_cx
            .as_ref()
            .err()
            .map_or("none", |error| error.code())
    );

    let absent_before = seq(&db)?;
    let absent = db.oracle_predict_action("never_observed");
    println!(
        "EDGE_UNKNOWN_ACTION before_seq={absent_before} after_seq={} code={}",
        seq(&db)?,
        absent.as_ref().err().map_or("none", |error| error.code())
    );

    let final_seq = seq(&db)?;
    drop(db);
    let readback = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(root),
        None,
    )?;
    let base = readback.scan_cf_at(final_seq, ColumnFamily::Base)?;
    let anchors = readback.scan_cf_at(final_seq, ColumnFamily::Anchors)?;
    let recurrence = readback.scan_cf_at(final_seq, ColumnFamily::Recurrence)?;
    let assay = readback.scan_cf_at(final_seq, ColumnFamily::Assay)?;
    let ledger = readback.scan_cf_at(final_seq, ColumnFamily::Ledger)?;
    let anneal_report = readback.scan_cf_at(final_seq, ColumnFamily::AnnealReport)?;
    let completion_ledger_rows = ledger
        .iter()
        .filter(|(_, value)| {
            value
                .windows(b"oracle_completion_v1".len())
                .any(|window| window == b"oracle_completion_v1")
        })
        .count();
    println!(
        "PHYSICAL_SOT snapshot={final_seq} Base={} Anchors={} Recurrence={} Assay={} Ledger={} AnnealReport={} completion_ledger_rows={completion_ledger_rows} panel={SYN_ACTION_PANEL_VERSION}",
        base.len(),
        anchors.len(),
        recurrence.len(),
        assay.len(),
        ledger.len(),
        anneal_report.len()
    );
    if anchors.len() != (ROWS_PER_CLASS * 2) as usize
        || assay.is_empty()
        || ledger.is_empty()
        || completion_ledger_rows != 1
        || anneal_report.len() != 1
    {
        return Err(
            "physical Oracle source-of-truth rows do not match the triggered corpus".into(),
        );
    }
    Ok(())
}

fn publish(
    db: &Db,
    index: u64,
    ts_ns: u64,
    tool: &str,
    outcome: bool,
) -> Result<String, Box<dyn Error>> {
    let key = ts_ns.to_be_bytes();
    let status = if outcome { "ok" } else { "error" };
    let record = json!({
        "schema_version": 1,
        "audit_id": format!("oracle-fsv-{index}"),
        "ts_ns": ts_ns,
        "seq": index,
        "tool": tool,
        "status": status,
        "error_code": if outcome { Value::Null } else { json!("SYNTHETIC_EXPECTED") },
        "details": {"fixture_index": index}
    });
    let raw = encode_json(&record)?;
    let context = serde_json::to_vec(&json!({
        "action_id": tool,
        "outcome_anchor": {"value": {"bool": outcome}},
        "ground_truth_anchor": {"value": {"bool": outcome}},
        "consequence": {
            "action_or_event": "terminal_outcome",
            "domain": "synapse.action",
            "outcome": {"value": {"bool": outcome}},
            "grounded": true,
            "provisional": false
        },
        "source_action_audit_key_hex": synapse_storage::constellations::hex_encode(&key),
    }))?;
    let report = db.put_action_oracle_publication(&key, &raw, &record, ts_ns, &key, &context)?;
    Ok(report.constellation_cx_id)
}

fn seq(db: &Db) -> Result<u64, Box<dyn Error>> {
    Ok(db.calyx_vault_inspect()?.ok_or("vault absent")?.latest_seq)
}

fn action_rows(db: &Db) -> Result<usize, Box<dyn Error>> {
    Ok(db.scan_cf(cf::CF_ACTION_LOG)?.len())
}

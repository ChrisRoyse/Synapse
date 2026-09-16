//! `CF_AGENT_EVENTS` integration tests (#897): real DB, physical rows,
//! typed `AgentEventRecord` round-trips, GC eviction order. TTL compaction
//! coverage lives in `compaction_tests.rs`, which iterates every retention
//! default including this CF.

use std::{
    error::Error,
    time::{SystemTime, UNIX_EPOCH},
};

use synapse_core::{AgentEndState, AgentEventKind, AgentEventRecord};
use synapse_storage::{Db, agent_events, cf, encode_json};

const TEST_SCHEMA_VERSION: u32 = 7;
const STEP_NS: u64 = 1_000_000_000;

/// Rows older than the 30-day TTL are removed whenever a compaction runs, so
/// synthetic rows carry recent timestamps to survive everything except the
/// eviction under test.
fn base_ts_ns() -> Result<u64, Box<dyn Error>> {
    let now_ns = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;
    Ok(now_ns - STEP_NS * 100)
}

fn event(ts_ns: u64, seq: u32, kind: AgentEventKind) -> AgentEventRecord {
    let mut record = AgentEventRecord::new(ts_ns, kind);
    record.session_id = Some(format!("itest-session-{seq:02}"));
    record
}

#[test]
fn agent_event_rows_survive_restart_and_iterate_chronologically() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");
    let base_ts_ns = base_ts_ns()?;
    {
        let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
        // Insert deliberately out of write order; key encoding must restore
        // chronological order on scan.
        for index in [3_u32, 0, 2, 1] {
            let ts_ns = base_ts_ns + STEP_NS * u64::from(index);
            let mut record = event(ts_ns, index, AgentEventKind::StateChanged);
            record.reason_code = Some("session_initialized".to_owned());
            record.state_to = Some("live".to_owned());
            db.put_batch(
                cf::CF_AGENT_EVENTS,
                vec![(
                    agent_events::agent_event_key(ts_ns, index),
                    encode_json(&record)?,
                )],
            )?;
        }
        db.flush()?;
    }

    let reopened = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let rows = reopened.scan_cf(cf::CF_AGENT_EVENTS)?;
    let decoded: Vec<(u64, AgentEventRecord)> = rows
        .iter()
        .map(|(key, value)| {
            let (ts_ns, _seq) = agent_events::decode_agent_event_key(key)?;
            let record: AgentEventRecord = synapse_storage::decode_json(value)?;
            Ok((ts_ns, record))
        })
        .collect::<Result<_, Box<dyn Error>>>()?;
    println!(
        "regression_state=cf_scan cf=CF_AGENT_EVENTS case=restart_order after={} observed_ts_offsets={:?}",
        decoded.len(),
        decoded
            .iter()
            .map(|(ts_ns, _record)| (ts_ns - base_ts_ns) / STEP_NS)
            .collect::<Vec<_>>()
    );
    assert_eq!(decoded.len(), 4);
    let ts_order: Vec<u64> = decoded.iter().map(|(ts_ns, _record)| *ts_ns).collect();
    let mut sorted = ts_order.clone();
    sorted.sort_unstable();
    assert_eq!(ts_order, sorted, "scan must iterate chronologically");
    for (ts_ns, record) in &decoded {
        assert_eq!(record.ts_ns, *ts_ns, "envelope ts must match key ts");
        assert_eq!(record.kind, AgentEventKind::StateChanged);
        record
            .validate()
            .map_err(|detail| format!("persisted row must stay valid: {detail}"))?;
    }
    Ok(())
}

#[test]
fn agent_event_gc_evicts_oldest_rows_first() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;
    let base_ts_ns = base_ts_ns()?;

    let rows = (0..20_u32)
        .map(|index| {
            let ts_ns = base_ts_ns + STEP_NS * u64::from(index);
            let mut record = event(ts_ns, index, AgentEventKind::Exited);
            record.end_state = Some(AgentEndState::Indeterminate);
            record.reason_code = Some("itest_gc".to_owned());
            Ok((
                agent_events::agent_event_key(ts_ns, index),
                encode_json(&record)?,
            ))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    db.put_batch(cf::CF_AGENT_EVENTS, rows)?;
    db.flush()?;

    let before = db.scan_cf(cf::CF_AGENT_EVENTS)?;
    let report = db.run_gc_once_with_row_caps(cf::CF_AGENT_EVENTS, 10, 30)?;
    let after = db.scan_cf(cf::CF_AGENT_EVENTS)?;
    let surviving_ts = after
        .iter()
        .map(|(key, _value)| agent_events::decode_agent_event_key(key).map(|(ts_ns, _seq)| ts_ns))
        .collect::<Result<Vec<_>, _>>()?;
    println!(
        "regression_state=cf_scan cf=CF_AGENT_EVENTS case=gc_oldest_first before={} after={} evicted={} surviving_ts_offsets={:?}",
        before.len(),
        after.len(),
        report.total_evicted_rows(),
        surviving_ts
            .iter()
            .map(|ts_ns| (ts_ns - base_ts_ns) / STEP_NS)
            .collect::<Vec<_>>()
    );

    assert_eq!(before.len(), 20);
    assert_eq!(report.total_evicted_rows(), 10);
    assert_eq!(after.len(), 10);
    let expected_newest: Vec<u64> = (10..20_u64)
        .map(|index| base_ts_ns + STEP_NS * index)
        .collect();
    assert_eq!(
        surviving_ts, expected_newest,
        "GC must evict the chronologically oldest agent-event rows"
    );
    Ok(())
}

#[test]
fn agent_event_gc_on_empty_cf_is_a_clean_no_op() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;
    let before = db.scan_cf(cf::CF_AGENT_EVENTS)?;
    let report = db.run_gc_once_with_row_caps(cf::CF_AGENT_EVENTS, 10, 30)?;
    let after = db.scan_cf(cf::CF_AGENT_EVENTS)?;
    println!(
        "regression_state=cf_scan cf=CF_AGENT_EVENTS case=gc_empty before={} after={} evicted={}",
        before.len(),
        after.len(),
        report.total_evicted_rows()
    );
    assert!(before.is_empty());
    assert!(after.is_empty());
    assert_eq!(report.total_evicted_rows(), 0);
    Ok(())
}

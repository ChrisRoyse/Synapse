use std::{
    error::Error,
    time::{SystemTime, UNIX_EPOCH},
};

use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::{Db, cf, encode_json, timeline};

const TEST_SCHEMA_VERSION: u32 = 7;
const STEP_NS: u64 = 1_000_000_000;

/// Timeline rows older than the 90-day TTL are removed by the compaction
/// filter whenever GC compacts, so synthetic rows must carry recent
/// timestamps to survive anything except the eviction under test.
fn base_ts_ns() -> Result<u64, Box<dyn Error>> {
    let now_ns = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;
    Ok(now_ns - STEP_NS * 100)
}

#[test]
fn timeline_gc_evicts_oldest_rows_first() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");
    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let base_ts_ns = base_ts_ns()?;

    let rows = (0..20_u32)
        .map(|index| {
            let ts_ns = base_ts_ns + STEP_NS * u64::from(index);
            let mut record =
                TimelineRecord::new(ts_ns, TimelineKind::FocusChange, TimelineActor::Human);
            record.app = Some(format!("app-{index:02}.exe"));
            Ok((timeline::timeline_key(ts_ns, index), encode_json(&record)?))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    db.put_batch(cf::CF_TIMELINE, rows)?;
    db.flush()?;

    let before = db.scan_cf(cf::CF_TIMELINE)?;
    let report = db.run_gc_once_with_row_caps(cf::CF_TIMELINE, 10, 30)?;
    let after = db.scan_cf(cf::CF_TIMELINE)?;
    let surviving_ts = after
        .iter()
        .map(|(key, _value)| timeline::decode_timeline_key(key).map(|(ts_ns, _seq)| ts_ns))
        .collect::<Result<Vec<_>, _>>()?;
    println!(
        "regression_state=cf_scan cf=CF_TIMELINE case=gc_oldest_first before={} after={} evicted={} surviving_ts_offsets={:?}",
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
        "GC must evict the chronologically oldest timeline rows"
    );
    Ok(())
}

#[test]
fn timeline_gc_on_empty_cf_is_a_clean_no_op() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;
    let before = db.scan_cf(cf::CF_TIMELINE)?;
    let report = db.run_gc_once_with_row_caps(cf::CF_TIMELINE, 10, 30)?;
    let after = db.scan_cf(cf::CF_TIMELINE)?;
    println!(
        "regression_state=cf_scan cf=CF_TIMELINE case=gc_empty before={} after={} evicted={}",
        before.len(),
        after.len(),
        report.total_evicted_rows()
    );
    assert!(before.is_empty());
    assert!(after.is_empty());
    assert_eq!(report.total_evicted_rows(), 0);
    Ok(())
}

#[test]
fn timeline_rows_survive_restart_and_iterate_chronologically() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");
    let base_ts_ns = base_ts_ns()?;
    {
        let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
        // Insert deliberately out of write order; key encoding must restore
        // chronological order on scan.
        for index in [3_u32, 0, 2, 1] {
            let ts_ns = base_ts_ns + STEP_NS * u64::from(index);
            let record = TimelineRecord::new(
                ts_ns,
                TimelineKind::TitleChange,
                TimelineActor::Agent {
                    session_id: format!("session-{index}"),
                },
            );
            db.put_batch(
                cf::CF_TIMELINE,
                vec![(timeline::timeline_key(ts_ns, index), encode_json(&record)?)],
            )?;
        }
        db.flush()?;
    }

    let reopened = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let rows = reopened.scan_cf(cf::CF_TIMELINE)?;
    let decoded: Vec<(u64, TimelineRecord)> = rows
        .iter()
        .map(|(key, value)| {
            let (ts_ns, _seq) = timeline::decode_timeline_key(key)?;
            let record: TimelineRecord = synapse_storage::decode_json(value)?;
            Ok((ts_ns, record))
        })
        .collect::<Result<_, Box<dyn Error>>>()?;
    println!(
        "regression_state=cf_scan cf=CF_TIMELINE case=restart_order after={} observed_ts_offsets={:?}",
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
    }
    Ok(())
}

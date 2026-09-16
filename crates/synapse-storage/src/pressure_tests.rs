use super::*;
use std::{error::Error, sync::Arc, time::Duration};

const TEST_SCHEMA_VERSION: u32 = 7;

#[test]
fn disk_pressure_transitions_emit_codes_once() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;
    let config = test_config();
    let cases = [
        (
            350,
            DiskPressureLevel::Level1,
            Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_1),
            false,
        ),
        (340, DiskPressureLevel::Level1, None, false),
        (
            250,
            DiskPressureLevel::Level2,
            Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_2),
            true,
        ),
        (
            150,
            DiskPressureLevel::Level3,
            Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_3),
            true,
        ),
        (
            50,
            DiskPressureLevel::Level4,
            Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_4),
            true,
        ),
        (
            150,
            DiskPressureLevel::Level3,
            Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_3),
            true,
        ),
        (500, DiskPressureLevel::Normal, None, false),
    ];

    for (free_bytes, expected_level, expected_code, expect_compaction) in cases {
        let before_level = db.pressure_level();
        let before_codes = db.pressure.transition_codes()?;
        let report =
            pressure::run_once_with_free_bytes(&db.inner, &db.pressure, &config, free_bytes)?;
        let after_level = db.pressure_level();
        let after_codes = db.pressure.transition_codes()?;
        println!(
            "regression_state=pressure_state free_bytes={} before_level={before_level:?} before_codes={before_codes:?} after_level={after_level:?} emitted_code={:?} compacted_cfs={} gc_advised={} observed=level:{after_level:?},codes:{after_codes:?}",
            report.free_bytes,
            report.emitted_code,
            report.compacted_cfs.len(),
            report.gc_advised
        );

        assert_eq!(report.current_level, expected_level);
        assert_eq!(after_level, expected_level);
        assert_eq!(report.emitted_code, expected_code);
        if expect_compaction {
            assert_eq!(report.compacted_cfs.len(), cf::ALL_COLUMN_FAMILIES.len());
        } else {
            assert!(report.compacted_cfs.is_empty());
        }
    }

    assert_eq!(
        db.pressure.transition_codes()?,
        vec![
            error_codes::STORAGE_DISK_PRESSURE_LEVEL_1,
            error_codes::STORAGE_DISK_PRESSURE_LEVEL_2,
            error_codes::STORAGE_DISK_PRESSURE_LEVEL_3,
            error_codes::STORAGE_DISK_PRESSURE_LEVEL_4,
            error_codes::STORAGE_DISK_PRESSURE_LEVEL_3,
        ]
    );
    Ok(())
}

#[test]
fn disk_pressure_write_gating() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;
    let config = test_config();

    db.put_batch(cf::CF_TIMELINE, row("timeline-normal"))?;
    db.flush()?;
    let normal_timeline = db.scan_cf(cf::CF_TIMELINE)?;
    println!(
        "regression_state=cf_scan level=Normal timeline={} observed=timeline:{:?}",
        normal_timeline.len(),
        printable_keys(&normal_timeline)
    );
    assert_eq!(normal_timeline.len(), 1);

    pressure::run_once_with_free_bytes(&db.inner, &db.pressure, &config, 150)?;
    let before_observations = db.scan_cf(cf::CF_OBSERVATIONS)?;
    assert_write_shed(
        db.put_batch(cf::CF_OBSERVATIONS, row("obs-l3")),
        cf::CF_OBSERVATIONS,
        DiskPressureLevel::Level3,
        1,
    );
    db.put_batch(cf::CF_EVENTS, row("event-l3"))?;
    assert_write_shed(
        db.put_batch(cf::CF_TIMELINE, row("timeline-l3")),
        cf::CF_TIMELINE,
        DiskPressureLevel::Level3,
        1,
    );
    db.flush()?;
    let after_l3_observations = db.scan_cf(cf::CF_OBSERVATIONS)?;
    let after_l3_events = db.scan_cf(cf::CF_EVENTS)?;
    let after_l3_timeline = db.scan_cf(cf::CF_TIMELINE)?;
    println!(
        "regression_state=cf_scan level=Level3 before_observations={} after_observations={} after_events={} after_timeline={} observed=observations:{:?},events:{:?},timeline:{:?}",
        before_observations.len(),
        after_l3_observations.len(),
        after_l3_events.len(),
        after_l3_timeline.len(),
        printable_keys(&after_l3_observations),
        printable_keys(&after_l3_events),
        printable_keys(&after_l3_timeline)
    );
    assert!(after_l3_observations.is_empty());
    assert_eq!(after_l3_events.len(), 1);
    assert_eq!(
        after_l3_timeline.len(),
        1,
        "CF_TIMELINE writes must shed at Level3 (only the Normal-level row survives)"
    );

    pressure::run_once_with_free_bytes(&db.inner, &db.pressure, &config, 50)?;
    assert_write_shed(
        db.put_batch(cf::CF_OBSERVATIONS, row("obs-l4")),
        cf::CF_OBSERVATIONS,
        DiskPressureLevel::Level4,
        1,
    );
    assert_write_shed(
        db.put_batch(cf::CF_EVENTS, row("event-l4")),
        cf::CF_EVENTS,
        DiskPressureLevel::Level4,
        1,
    );
    db.put_batch(cf::CF_REFLEX_AUDIT, row("audit-l4"))?;
    db.put_batch(cf::CF_SESSIONS, row("session-l4"))?;
    assert_write_shed(
        db.put_batch(cf::CF_TIMELINE, row("timeline-l4")),
        cf::CF_TIMELINE,
        DiskPressureLevel::Level4,
        1,
    );
    db.flush()?;
    let after_l4_observations = db.scan_cf(cf::CF_OBSERVATIONS)?;
    let after_l4_events = db.scan_cf(cf::CF_EVENTS)?;
    let after_l4_audit = db.scan_cf(cf::CF_REFLEX_AUDIT)?;
    let after_l4_sessions = db.scan_cf(cf::CF_SESSIONS)?;
    let after_l4_timeline = db.scan_cf(cf::CF_TIMELINE)?;
    assert_eq!(
        after_l4_timeline.len(),
        1,
        "CF_TIMELINE writes must shed at Level4 (only the Normal-level row survives)"
    );
    println!(
        "regression_state=cf_scan level=Level4 observations={} events={} audit={} sessions={} observed=observations:{:?},events:{:?},audit:{:?},sessions:{:?}",
        after_l4_observations.len(),
        after_l4_events.len(),
        after_l4_audit.len(),
        after_l4_sessions.len(),
        printable_keys(&after_l4_observations),
        printable_keys(&after_l4_events),
        printable_keys(&after_l4_audit),
        printable_keys(&after_l4_sessions)
    );

    assert!(after_l4_observations.is_empty());
    assert_eq!(after_l4_events.len(), 1);
    assert_eq!(after_l4_audit.len(), 1);
    assert_eq!(after_l4_sessions.len(), 1);
    Ok(())
}

#[tokio::test]
async fn disk_pressure_periodic_task_runs_tick() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;
    let task = pressure::spawn_with_free_bytes(
        Arc::clone(&db.inner),
        Arc::clone(&db.pressure),
        db.path.clone(),
        test_config(),
        vec![350],
    )?;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let after_level = db.pressure_level();
    let after_codes = db.pressure.transition_codes()?;
    let readback = db.pressure.probe_readback()?;
    println!(
        "regression_state=pressure_state case=periodic_task task_running={} observed={} last_free_bytes={:?} last_level={:?} last_started={:?} last_completed={:?} last_error={:?} after_level={after_level:?} after_codes={after_codes:?}",
        task.running(),
        readback.observed,
        readback.last_free_bytes,
        readback.last_level,
        readback.last_started_unix_ms,
        readback.last_completed_unix_ms,
        readback.last_error
    );
    assert!(task.running());
    assert!(readback.observed);
    assert_eq!(readback.last_free_bytes, Some(350));
    assert_eq!(readback.last_level, Some(DiskPressureLevel::Level1));
    assert!(readback.last_started_unix_ms.is_some());
    assert!(readback.last_completed_unix_ms.is_some());
    assert_eq!(readback.last_error, None);
    drop(task);
    assert_eq!(after_level, DiskPressureLevel::Level1);
    assert_eq!(
        after_codes,
        vec![error_codes::STORAGE_DISK_PRESSURE_LEVEL_1]
    );
    Ok(())
}

fn test_config() -> pressure::PressureConfig {
    pressure::PressureConfig::with_thresholds(Duration::from_millis(10), 400, 300, 200, 100)
}

fn row(label: &'static str) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![(
        label.as_bytes().to_vec(),
        format!(r#"{{"label":"{label}"}}"#).into_bytes(),
    )]
}

fn assert_write_shed(
    result: StorageResult<()>,
    expected_cf: &str,
    expected_level: DiskPressureLevel,
    expected_rows: usize,
) {
    match result {
        Err(StorageError::WriteShed {
            cf_name,
            pressure_level,
            rows,
        }) => {
            assert_eq!(cf_name, expected_cf);
            assert_eq!(pressure_level, format!("{expected_level:?}"));
            assert_eq!(rows, expected_rows);
        }
        other => panic!("expected WriteShed for {expected_cf}, got {other:?}"),
    }
}

fn printable_keys(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<String> {
    rows.iter()
        .map(|(key, _value)| String::from_utf8_lossy(key).into_owned())
        .collect()
}

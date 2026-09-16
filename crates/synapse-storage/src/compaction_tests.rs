use super::*;
use std::error::Error;

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use synapse_core::retention::DEFAULTS;

const TEST_SCHEMA_VERSION: u32 = 7;
const NOW_NS: u64 = 1_800_000_000_000_000_000;

#[test]
fn compaction_ttl_proptest_removes_old_keeps_fresh() -> Result<(), Box<dyn Error>> {
    let mut runner = TestRunner::new(ProptestConfig::with_cases(4));
    runner
        .run(&(1usize..6), |records_per_class| {
            run_ttl_property(records_per_class)
                .map_err(|error| TestCaseError::fail(error.to_string()))
        })
        .map_err(|error| format!("compaction proptest failed: {error}"))?;
    Ok(())
}

#[test]
fn compaction_ttl_edges_per_cf() -> Result<(), Box<dyn Error>> {
    for default in DEFAULTS {
        run_zero_record_edge(default.cf)?;
        run_boundary_edges(default.cf)?;
    }
    Ok(())
}

fn run_ttl_property(records_per_class: usize) -> Result<(), Box<dyn Error>> {
    let _clock = compaction::set_test_now_ns(NOW_NS);
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;

    for default in DEFAULTS {
        let cf = default.cf;
        let ttl = compaction::ttl_ns_for_cf(cf);
        write_records(&db, cf, "old", records_per_class, timestamp_before_ttl(ttl))?;
        write_records(&db, cf, "fresh", records_per_class, NOW_NS)?;
        flush_cf(&db, cf)?;

        let before = db.scan_cf(cf)?;
        db.compact_cf(cf)?;
        let after = db.scan_cf(cf)?;
        let old_after = count_label(&after, "old");
        let fresh_after = count_label(&after, "fresh");
        println!(
            "regression_state=cf_scan cf={cf} case=proptest before_count={} after_count={} observed=old_after:{old_after} fresh_after:{fresh_after} ttl_ns:{ttl:?}",
            before.len(),
            after.len()
        );

        if ttl.is_some() {
            assert_eq!(old_after, 0);
        } else {
            assert_eq!(old_after, records_per_class);
        }
        assert_eq!(fresh_after, records_per_class);
    }

    Ok(())
}

fn run_zero_record_edge(cf: &str) -> Result<(), Box<dyn Error>> {
    let _clock = compaction::set_test_now_ns(NOW_NS);
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;
    let before = db.scan_cf(cf)?;
    db.compact_cf(cf)?;
    let after = db.scan_cf(cf)?;
    println!(
        "regression_state=cf_scan cf={cf} case=zero before_count={} after_count={} observed={after:?}",
        before.len(),
        after.len()
    );
    assert!(before.is_empty());
    assert!(after.is_empty());
    Ok(())
}

fn run_boundary_edges(cf: &str) -> Result<(), Box<dyn Error>> {
    let _clock = compaction::set_test_now_ns(NOW_NS);
    let temp = tempfile::tempdir()?;
    let db = Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?;
    let ttl = compaction::ttl_ns_for_cf(cf);

    write_record(
        &db,
        cf,
        b"boundary",
        timestamp_at_ttl_boundary(ttl),
        "boundary",
    )?;
    write_record(&db, cf, b"over", timestamp_one_ns_over_ttl(ttl), "over")?;
    flush_cf(&db, cf)?;

    let before = db.scan_cf(cf)?;
    db.compact_cf(cf)?;
    let after = db.scan_cf(cf)?;
    let boundary_after = count_label(&after, "boundary");
    let over_after = count_label(&after, "over");
    println!(
        "regression_state=cf_scan cf={cf} case=boundary before_count={} after_count={} observed=boundary_after:{boundary_after} over_after:{over_after} ttl_ns:{ttl:?}",
        before.len(),
        after.len()
    );

    assert_eq!(boundary_after, 1);
    if ttl.is_some() {
        assert_eq!(over_after, 0);
    } else {
        assert_eq!(over_after, 1);
    }
    Ok(())
}

fn write_records(
    db: &Db,
    cf: &str,
    label: &str,
    count: usize,
    ts_ns: u64,
) -> Result<(), Box<dyn Error>> {
    for index in 0..count {
        let key = format!("{label}-{index:02}");
        write_record(db, cf, key.as_bytes(), ts_ns, label)?;
    }
    Ok(())
}

fn write_record(
    db: &Db,
    cf: &str,
    key: &[u8],
    ts_ns: u64,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let handle = db.inner.cf_handle(cf).ok_or("column family missing")?;
    db.inner.put_cf(&handle, key, value(ts_ns, label))?;
    Ok(())
}

fn flush_cf(db: &Db, cf: &str) -> Result<(), Box<dyn Error>> {
    let handle = db.inner.cf_handle(cf).ok_or("column family missing")?;
    db.inner.flush_cf(&handle)?;
    Ok(())
}

fn value(ts_ns: u64, label: &str) -> Vec<u8> {
    format!(r#"{{"ts_ns":{ts_ns},"label":"{label}"}}"#).into_bytes()
}

fn timestamp_before_ttl(ttl: Option<u64>) -> u64 {
    ttl.map_or(NOW_NS.saturating_sub(1_000_000_000), |ttl_ns| {
        NOW_NS.saturating_sub(ttl_ns).saturating_sub(1)
    })
}

fn timestamp_at_ttl_boundary(ttl: Option<u64>) -> u64 {
    ttl.map_or(NOW_NS.saturating_sub(1), |ttl_ns| {
        NOW_NS.saturating_sub(ttl_ns)
    })
}

fn timestamp_one_ns_over_ttl(ttl: Option<u64>) -> u64 {
    ttl.map_or(NOW_NS.saturating_sub(2), |ttl_ns| {
        NOW_NS.saturating_sub(ttl_ns).saturating_sub(1)
    })
}

fn count_label(rows: &[(Vec<u8>, Vec<u8>)], label: &str) -> usize {
    let needle = format!(r#""label":"{label}""#);
    rows.iter()
        .filter(|(_key, value)| String::from_utf8_lossy(value).contains(&needle))
        .count()
}

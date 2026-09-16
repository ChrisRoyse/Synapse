use std::{error::Error, time::Instant};

use synapse_storage::{Db, cf};

const TEST_SCHEMA_VERSION: u32 = 7;
const THROUGHPUT_ROWS: usize = 10_000;
const TARGET_MS: u128 = 200;

#[test]
fn batch_throughput_edges_and_restart() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");
    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;

    let empty: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let before_empty = db.scan_cf(cf::CF_KV)?;
    db.put_batch(cf::CF_KV, empty)?;
    db.flush()?;
    let after_empty = db.scan_cf(cf::CF_KV)?;
    println!(
        "regression_state=batch_cf_scan edge=empty before={} after={} observed={after_empty:?}",
        before_empty.len(),
        after_empty.len()
    );
    assert!(after_empty.is_empty());

    db.put_batch(cf::CF_KV, vec![(b"single".to_vec(), b"z".to_vec())])?;
    db.flush()?;
    let after_single = db.scan_cf(cf::CF_KV)?;
    println!(
        "regression_state=batch_cf_scan edge=single_byte before=0 after={} observed={:?}",
        after_single.len(),
        printable_rows(&after_single)
    );
    assert_eq!(after_single.len(), 1);

    let kvs = event_rows(THROUGHPUT_ROWS);
    let expected_bytes = total_value_bytes(&kvs);
    let started = Instant::now();
    db.put_batch(cf::CF_EVENTS, kvs)?;
    db.flush()?;
    let elapsed_ms = started.elapsed().as_millis();
    let after_events = db.scan_cf(cf::CF_EVENTS)?;
    println!(
        "regression_state=batch_cf_scan edge=throughput before=0 after=count:{} bytes:{} elapsed_ms:{elapsed_ms} observed=pass:{}",
        after_events.len(),
        total_value_bytes(&after_events),
        elapsed_ms <= TARGET_MS
    );
    assert_eq!(after_events.len(), THROUGHPUT_ROWS);
    assert_eq!(total_value_bytes(&after_events), expected_bytes);
    assert!(elapsed_ms <= TARGET_MS);
    drop(db);

    let reopened = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let reopened_events = reopened.scan_cf(cf::CF_EVENTS)?;
    let reopened_kv = reopened.scan_cf(cf::CF_KV)?;
    println!(
        "regression_state=batch_cf_scan edge=restart before=dropped after=events:{} kv:{} observed=durable:{}",
        reopened_events.len(),
        reopened_kv.len(),
        reopened_events.len() == THROUGHPUT_ROWS && reopened_kv.len() == 1
    );
    assert_eq!(reopened_events.len(), THROUGHPUT_ROWS);
    assert_eq!(reopened_kv.len(), 1);
    Ok(())
}

fn event_rows(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..count)
        .map(|index| {
            (
                format!("{index:016x}").into_bytes(),
                format!(r#"{{"ts_ns":{index},"event":"integration"}}"#).into_bytes(),
            )
        })
        .collect()
}

fn total_value_bytes(rows: &[(Vec<u8>, Vec<u8>)]) -> usize {
    rows.iter().map(|(_key, value)| value.len()).sum()
}

fn printable_rows(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(key, value)| {
            (
                String::from_utf8_lossy(key).into_owned(),
                String::from_utf8_lossy(value).into_owned(),
            )
        })
        .collect()
}

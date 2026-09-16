use std::{
    error::Error,
    time::{SystemTime, UNIX_EPOCH},
};

use synapse_core::retention::{DEFAULTS, RetentionTtl};
use synapse_storage::Db;

const TEST_SCHEMA_VERSION: u32 = 7;

#[test]
fn compaction_ttl_old_fresh_invalid_and_restart() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("db");
    let db = Db::open(&path, TEST_SCHEMA_VERSION)?;
    let now_ns = now_ns()?;

    for default in DEFAULTS {
        let cf_name = default.cf;
        let ttl_ns = ttl_to_ns(default.ttl);
        db.put_batch(
            cf_name,
            vec![
                row("old", 0),
                row("fresh", now_ns),
                (b"invalid".to_vec(), br#"{"label":"invalid"}"#.to_vec()),
            ],
        )?;
        db.flush()?;
        let before = db.scan_cf(cf_name)?;
        db.compact_cf(cf_name)?;
        let after = db.scan_cf(cf_name)?;
        let old_after = count_key(&after, "old");
        let fresh_after = count_key(&after, "fresh");
        let invalid_after = count_key(&after, "invalid");
        println!(
            "regression_state=ttl_cf_scan cf={cf_name} before={} after=old:{old_after},fresh:{fresh_after},invalid:{invalid_after} observed=ttl_ns:{ttl_ns:?}",
            before.len()
        );

        if ttl_ns.is_some() {
            assert_eq!(old_after, 0);
        } else {
            assert_eq!(old_after, 1);
        }
        assert_eq!(fresh_after, 1);
        assert_eq!(invalid_after, 1);
    }
    drop(db);

    let reopened = Db::open(&path, TEST_SCHEMA_VERSION)?;
    for default in DEFAULTS {
        let cf_name = default.cf;
        let rows = reopened.scan_cf(cf_name)?;
        println!(
            "regression_state=ttl_cf_scan cf={cf_name} edge=restart before=dropped after=count:{} observed=durable:true",
            rows.len()
        );
        assert!(rows.len() >= 2);
    }
    Ok(())
}

fn row(label: &str, ts_ns: u64) -> (Vec<u8>, Vec<u8>) {
    (
        label.as_bytes().to_vec(),
        format!(r#"{{"ts_ns":{ts_ns},"label":"{label}"}}"#).into_bytes(),
    )
}

fn count_key(rows: &[(Vec<u8>, Vec<u8>)], key: &str) -> usize {
    rows.iter()
        .filter(|(row_key, _value)| row_key == key.as_bytes())
        .count()
}

fn ttl_to_ns(ttl: RetentionTtl) -> Option<u64> {
    match ttl {
        RetentionTtl::None | RetentionTtl::LruOnly => None,
        RetentionTtl::Hours(hours) => hours
            .checked_mul(60)?
            .checked_mul(60)?
            .checked_mul(1_000_000_000),
        RetentionTtl::Days(days) => days
            .checked_mul(24)?
            .checked_mul(60)?
            .checked_mul(60)?
            .checked_mul(1_000_000_000),
    }
}

fn now_ns() -> Result<u64, Box<dyn Error>> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
    )?)
}

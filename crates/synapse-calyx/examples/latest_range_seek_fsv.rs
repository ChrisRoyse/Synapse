//! Manual FSV for #2036: prove `scan_cf_range_latest` seeks its range instead
//! of walking the column family.
//!
//! ## What was wrong
//!
//! `latest_rows_from_view` iterated every key in the CF's row `BTreeMap` and
//! applied `KeyRange::contains` as a filter, so a prefix read of a dozen rows
//! cost the whole family — while holding the vault-wide row-table read guard
//! every constellation writer needs (#1950). This is the same defect #1973
//! fixed in the overlay reader, left behind in the latest-view reader.
//!
//! On the live daemon that produced 4,417 `CALYX_ASTER_ROW_READ_GUARD_SLOW`
//! warnings, 3,772 of them (85%) from `scan_cf_range_latest`, 512 of 512 sampled
//! commits over the duration floor, and `/health` taking 13.5-23.7 s on every
//! single request because every writer queued behind these reads.
//!
//! ## The claim under test, stated before the run
//!
//! If the reader SEEKS, a narrow read's cost is a function of how many rows it
//! returns, not of how many rows the family holds. So against one large family:
//!
//! * a narrow prefix read must be dramatically faster than a whole-family read,
//! * and it must return exactly the keys in its range — seeking must not change
//!   which rows are visible, only how they are found.
//!
//! A walk-and-filter implementation fails the first assertion: every read costs
//! the family. That is the discriminating observation.
//!
//! Edge cases are exercised because the bounds are where a seek goes wrong:
//! an empty-but-ordered range (`end == start`) is legal and yields nothing, a
//! range entirely past the last key yields nothing, an unbounded-end range runs
//! to the end of the family, and an INVERTED range must fail closed rather than
//! read as "this family is empty" (`BTreeMap::range` panics on one, and the
//! filter shape this replaces silently returned nothing).
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example latest_range_seek_fsv -- <empty-scratch-dir>`

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::time::Instant;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETP";
const CF: ColumnFamily = ColumnFamily::Kv;
/// Large enough that a whole-family walk is unmistakably more expensive than a
/// narrow seek, small enough to seed in seconds.
const FAMILY_ROWS: u64 = 60_000;
/// The narrow read: one prefix holding exactly this many rows.
const NARROW_ROWS: u64 = 16;

fn key(prefix: u8, index: u64) -> Vec<u8> {
    let mut key = vec![prefix];
    key.extend_from_slice(&index.to_be_bytes());
    key
}

fn prefix_range(prefix: u8) -> KeyRange {
    KeyRange {
        start: vec![prefix],
        end: Some(vec![prefix + 1]),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: latest_range_seek_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    let vault = AsterVault::open(
        &dir,
        VaultId::from_str(VAULT_ID)?,
        [0_u8; 32],
        VaultOptions::default(),
    )?;

    // Family under prefix 0x01: FAMILY_ROWS rows. Narrow prefix 0x02: NARROW_ROWS.
    println!("seeding {FAMILY_ROWS} rows under prefix 0x01 and {NARROW_ROWS} under 0x02 ...");
    let mut rows = (0..FAMILY_ROWS)
        .map(|index| (CF, key(1, index), index.to_be_bytes().to_vec()))
        .collect::<Vec<_>>();
    rows.extend((0..NARROW_ROWS).map(|index| (CF, key(2, index), index.to_be_bytes().to_vec())));
    vault.write_cf_batch(rows)?;

    // ---- Correctness first: seeking must not change which rows are visible.
    let narrow = vault.scan_cf_range_latest(CF, &prefix_range(2))?;
    let expected: Vec<Vec<u8>> = (0..NARROW_ROWS).map(|index| key(2, index)).collect();
    let actual: Vec<Vec<u8>> = narrow.iter().map(|(key, _)| key.clone()).collect();
    println!("\nCASE 1 narrow prefix read returns exactly its range");
    println!("  expected_rows={NARROW_ROWS} actual_rows={}", actual.len());
    assert_eq!(actual, expected, "narrow range returned the wrong keys");
    println!("  PASS keys match exactly");

    let wide = vault.scan_cf_range_latest(CF, &prefix_range(1))?;
    println!("\nCASE 2 whole-family read still returns the whole family");
    println!("  expected_rows={FAMILY_ROWS} actual_rows={}", wide.len());
    assert_eq!(wide.len() as u64, FAMILY_ROWS, "wide range lost rows");
    println!("  PASS");

    // ---- The discriminating measurement.
    // Warm both paths so neither pays a first-touch cost.
    let _ = vault.scan_cf_range_latest(CF, &prefix_range(2))?;
    let _ = vault.scan_cf_range_latest(CF, &prefix_range(1))?;

    let mut narrow_us = u128::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let rows = vault.scan_cf_range_latest(CF, &prefix_range(2))?;
        let elapsed = started.elapsed().as_micros();
        assert_eq!(rows.len() as u64, NARROW_ROWS);
        narrow_us = narrow_us.min(elapsed);
    }
    let mut wide_us = u128::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let rows = vault.scan_cf_range_latest(CF, &prefix_range(1))?;
        let elapsed = started.elapsed().as_micros();
        assert_eq!(rows.len() as u64, FAMILY_ROWS);
        wide_us = wide_us.min(elapsed);
    }

    println!("\nCASE 3 cost tracks rows RETURNED, not rows in the family");
    println!("  narrow read ({NARROW_ROWS} rows):      {narrow_us} us");
    println!("  whole-family read ({FAMILY_ROWS} rows): {wide_us} us");
    let ratio = wide_us as f64 / narrow_us.max(1) as f64;
    println!("  ratio = {ratio:.0}x");
    // A walk-and-filter reader pays the family on BOTH reads, so the ratio
    // collapses toward 1. Seeking keeps them orders of magnitude apart.
    assert!(
        ratio > 20.0,
        "narrow read costs the same order as the whole family: the reader is walking, not seeking (narrow={narrow_us}us wide={wide_us}us)"
    );
    println!("  PASS narrow read is not paying for the family");

    // ---- Bounds edge cases.
    println!("\nCASE 4 empty-but-ordered range (end == start) is legal and yields nothing");
    let empty = vault.scan_cf_range_latest(
        CF,
        &KeyRange {
            start: key(1, 10),
            end: Some(key(1, 10)),
        },
    )?;
    println!("  rows={}", empty.len());
    assert!(empty.is_empty());
    println!("  PASS");

    println!("\nCASE 5 range entirely past the last key yields nothing");
    let past = vault.scan_cf_range_latest(
        CF,
        &KeyRange {
            start: vec![0xfe],
            end: Some(vec![0xff]),
        },
    )?;
    println!("  rows={}", past.len());
    assert!(past.is_empty());
    println!("  PASS");

    println!("\nCASE 6 unbounded end runs to the end of the family");
    let unbounded = vault.scan_cf_range_latest(
        CF,
        &KeyRange {
            start: vec![2],
            end: None,
        },
    )?;
    println!("  rows={} (expected {NARROW_ROWS})", unbounded.len());
    assert_eq!(unbounded.len() as u64, NARROW_ROWS);
    println!("  PASS");

    println!("\nCASE 7 INVERTED range fails closed, never reads as an empty family");
    let inverted = vault.scan_cf_range_latest(
        CF,
        &KeyRange {
            start: key(1, 500),
            end: Some(key(1, 100)),
        },
    );
    match inverted {
        Ok(rows) => {
            return Err(format!(
                "inverted range returned Ok with {} rows; a malformed request must not be indistinguishable from an empty family",
                rows.len()
            )
            .into());
        }
        Err(error) => {
            println!("  code={} ", error.code);
            assert_eq!(error.code, "CALYX_ASTER_OVERLAY_RANGE_INVERTED");
            println!("  PASS fails closed");
        }
    }

    println!("\nALL CASES PASSED");
    Ok(())
}

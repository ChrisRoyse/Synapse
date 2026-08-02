//! Manual FSV for #1952 ask 2: does `count_cf_latest` return **exactly** what
//! `scan_cf_latest(cf)?.len()` returns?
//!
//! ## Why equality is the whole test
//!
//! The 14 converted call sites are physical-readback counts — they exist to
//! prove that a write landed. A cheaper number that is silently different would
//! be worse than the allocation it saves: it would turn evidence into
//! decoration. So "faster" is not the acceptance criterion; "identical" is.
//!
//! The interesting cases are the ones where a naive count would drift, so they
//! are constructed deliberately rather than hoped for:
//!
//! | case | why it can drift |
//! |---|---|
//! | empty CF | off-by-one / `None` vs `Some(0)` |
//! | rows only in a memtable | table overlay path only |
//! | rows only in SSTs | router path only |
//! | rows in both | the merge, where a key can be double-counted |
//! | a key overwritten in the memtable after flushing | dedup across sources |
//! | a tombstone still in the memtable | `Tombstone` must REMOVE |
//! | a tombstone flushed to an SST | `is_tombstone_value` on the router side |
//! | tombstone for a key that never existed | must not underflow below zero |
//!
//! Every case asserts `count == scan.len()` against the same vault at the same
//! sequence. A single disagreement fails the run.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example count_cf_latest_fsv -- <empty-scratch-dir>`

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::mvcc::tombstone_value;
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const CF: ColumnFamily = ColumnFamily::Kv;

fn key_of(tag: u64) -> Vec<u8> {
    format!("fsv1952/{tag:06}").into_bytes()
}

/// Reads both ways and reports whether they agree.
fn compare(vault: &AsterVault, label: &str, expected: usize) -> Result<bool, Box<dyn Error>> {
    let scanned = vault.scan_cf_latest(CF)?.len();
    let counted = vault.count_cf_latest(CF)?;
    let agree = scanned == counted;
    let as_expected = scanned == expected;
    println!(
        "  {label:<44} scan={scanned:<6} count={counted:<6} expected={expected:<6} {}{}",
        if agree { "AGREE" } else { "DISAGREE" },
        if as_expected {
            ""
        } else {
            "  <-- and neither matches the expected row count"
        }
    );
    Ok(agree && as_expected)
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: count_cf_latest_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    let vault_id = VaultId::from_str(VAULT_ID)?;
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"count-cf-latest-fsv".to_vec(),
        VaultOptions::default(),
    )?;

    println!("count_cf_latest_fsv  (#1952 ask 2: the counts must be EQUAL)");
    println!("vault_dir = {}\ncf = {}\n", dir.display(), CF.name());

    let mut ok = true;

    // 1. empty CF
    ok &= compare(&vault, "1. empty CF", 0)?;

    // 2. rows only in the memtable (no flush yet)
    vault
        .write_cf_batch((0..40_u64).map(|tag| (CF, key_of(tag), format!("v{tag}").into_bytes())))?;
    ok &= compare(&vault, "2. 40 rows, memtable only", 40)?;

    // 3. the same rows, now only in SSTs
    vault.flush_all_cfs()?;
    ok &= compare(&vault, "3. same 40 rows, flushed to SSTs", 40)?;

    // 4. rows in BOTH sources: 20 new ones land in a fresh memtable
    vault.write_cf_batch(
        (40..60_u64).map(|tag| (CF, key_of(tag), format!("v{tag}").into_bytes())),
    )?;
    ok &= compare(&vault, "4. 40 in SSTs + 20 in memtable", 60)?;

    // 5. overwrite a flushed key from the memtable: present in both sources,
    //    must be counted once.
    vault.write_cf_batch([(CF, key_of(7), b"rewritten".to_vec())])?;
    ok &= compare(&vault, "5. key 7 overwritten (in both sources)", 60)?;

    // 6. tombstone a key while the tombstone is still in the memtable
    vault.write_cf_batch([(CF, key_of(11), tombstone_value())])?;
    ok &= compare(&vault, "6. key 11 tombstoned, memtable-resident", 59)?;

    // 7. flush so that tombstone now lives in an SST with no memtable copy
    vault.flush_all_cfs()?;
    ok &= compare(&vault, "7. same tombstone, now flushed to an SST", 59)?;

    // 8. tombstone a key that was never written: must not go below the true
    //    count, and must not panic on an underflow.
    vault.write_cf_batch([(CF, key_of(9_999), tombstone_value())])?;
    ok &= compare(&vault, "8. tombstone for a never-written key", 59)?;

    // 9. flush that one too, so the router carries a tombstone with no live row
    vault.flush_all_cfs()?;
    ok &= compare(&vault, "9. same, flushed", 59)?;

    // 10. delete everything: the empty case reached from a populated CF, which
    //     is a different path than case 1's never-written CF.
    vault.write_cf_batch(
        (0..60_u64)
            .filter(|tag| *tag != 11)
            .map(|tag| (CF, key_of(tag), tombstone_value())),
    )?;
    vault.flush_all_cfs()?;
    ok &= compare(&vault, "10. every row tombstoned and flushed", 0)?;

    println!("\n--- VERDICT ---");
    if ok {
        println!("  PASS: count_cf_latest equals scan_cf_latest().len() in every case");
        Ok(())
    } else {
        Err("count_cf_latest disagreed with scan_cf_latest().len()".into())
    }
}

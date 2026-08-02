//! Manual FSV for #1954: does a tombstone flushed into an SST read as *live*
//! on the key-based latest path?
//!
//! ## The claim under test
//!
//! `CfRouter::range_keys_until` determines tombstone status only for rows still
//! resident in a memtable:
//!
//! ```ignore
//! for key in level.range_keys_until(start, end)? {
//!     rows.insert(key, false);            // every SST key marked LIVE
//! }
//! for table in self.read_tables_oldest_first(cf) {
//!     for (key, value) in table.range_until(start, end) {
//!         rows.insert(key, is_tombstone_value(&value));   // memtables only
//!     }
//! }
//! ```
//!
//! `SstLevel::range_keys_until` returns keys without values, so once a
//! tombstone is flushed and no memtable copy remains, the deleted key should be
//! reported as visible. `scan_cf_range_page_latest` then resolves values with
//! `read_batch` — which *does* filter tombstones — and fails closed with
//! `CALYX_ASTER_CORRUPT_SHARD` "disappeared during pinned page read".
//!
//! #1954 was filed from reading that code. This runs it.
//!
//! ## Construction, and what each outcome means
//!
//! Known input, known expected output, stated before the run:
//!
//! 1. write N rows to a CF, flush -> rows live in an SST
//! 2. write a tombstone over one key, flush -> tombstone in an SST, no memtable copy
//! 3. `scan_cf_latest` (value-based path) MUST NOT contain the deleted key
//! 4. `scan_cf_range_page_latest` (key-based path) over the same range:
//!      * `CALYX_ASTER_CORRUPT_SHARD`  -> #1954 reproduced
//!      * returns the deleted key      -> worse: a deleted row served as live
//!      * clean, key absent            -> #1954 does NOT reproduce; the paths
//!        agree and the issue is wrong
//!
//! Step 3 is the control. If the value-based path also still shows the key, the
//! delete simply did not happen and step 4 proves nothing about either path.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example flushed_tombstone_fsv -- <empty-scratch-dir>`

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use calyx_aster::mvcc::tombstone_value;
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
/// A plain key/value CF, so nothing else in the vault writes to it and the row
/// set is exactly what this harness put there.
const CF: ColumnFamily = ColumnFamily::Kv;
const ROWS: u64 = 64;
/// The row that gets deleted. Deliberately not the first or last key, so an
/// off-by-one in range handling cannot be mistaken for the finding.
const VICTIM: u64 = 31;

fn key_of(tag: u64) -> Vec<u8> {
    format!("fsv1954/{tag:06}").into_bytes()
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: flushed_tombstone_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    let vault_id = VaultId::from_str(VAULT_ID)?;
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"flushed-tombstone-fsv".to_vec(),
        VaultOptions::default(),
    )?;

    println!("flushed_tombstone_fsv  (#1954)");
    println!("vault_dir = {}", dir.display());
    println!("cf = {}   rows = {ROWS}   victim = {VICTIM}", CF.name());

    // --- 1. write and flush so the rows live in an SST ---------------------
    let rows = (0..ROWS)
        .map(|tag| (CF, key_of(tag), format!("value-{tag}").into_bytes()))
        .collect::<Vec<_>>();
    vault.write_cf_batch(rows)?;
    let flushed = vault.flush_all_cfs()?.len();
    println!("\n=== 1. wrote {ROWS} rows, flushed {flushed} SST(s) ===");

    // --- 2. tombstone one key and flush again ------------------------------
    vault.write_cf_batch([(CF, key_of(VICTIM), tombstone_value())])?;
    let flushed2 = vault.flush_all_cfs()?.len();
    println!("=== 2. tombstoned key {VICTIM}, flushed {flushed2} more SST(s) ===");
    println!("   the tombstone is now in an SST with no memtable copy");

    let victim = key_of(VICTIM);

    // --- 3. control: the value-based path must not show the deleted key ----
    println!("\n=== 3. control: scan_cf_latest (value-based) ===");
    let live = vault.scan_cf_latest(CF)?;
    let victim_in_values = live.iter().any(|(key, _)| key == &victim);
    println!(
        "   rows returned = {}   (wrote {ROWS}, deleted 1)",
        live.len()
    );
    println!("   deleted key present = {victim_in_values}");
    if victim_in_values {
        return Err(
            "the value-based path still shows the deleted key: the delete did not take, so nothing below is a test of the key path"
                .into(),
        );
    }

    // --- 4. the key-based path over the same range -------------------------
    println!("\n=== 4. scan_cf_range_page_latest (key-based) over the same range ===");
    let range = KeyRange {
        start: b"fsv1954/".to_vec(),
        end: Some(b"fsv19540".to_vec()),
    };
    let outcome = vault.scan_cf_range_page_latest(CF, &range, None, ROWS as usize);
    let verdict = match &outcome {
        Ok(page) => {
            let served = page.rows.iter().any(|(key, _)| key == &victim);
            println!(
                "   Ok: {} row(s), examined {}, more={}",
                page.rows.len(),
                page.examined_rows,
                page.more
            );
            println!("   deleted key served as live = {served}");
            if served {
                "SERVED-DELETED-ROW"
            } else {
                "AGREES"
            }
        }
        Err(error) => {
            println!("   Err[{}]: {}", error.code, error.message);
            if error.code == "CALYX_ASTER_CORRUPT_SHARD" {
                "REPRODUCED"
            } else {
                "OTHER-ERROR"
            }
        }
    };

    println!("\n--- VERDICT ---");
    match verdict {
        "REPRODUCED" => {
            println!("  #1954 REPRODUCED: the key-based path reported a flushed tombstone as");
            println!("  visible, and the page read failed closed with CALYX_ASTER_CORRUPT_SHARD.");
            Ok(())
        }
        "SERVED-DELETED-ROW" => Err(
            "WORSE THAN FILED: the key-based path served a deleted row as live rather than erroring"
                .into(),
        ),
        "AGREES" => {
            println!("  #1954 does NOT reproduce: both paths agree that the key is gone.");
            println!("  The issue's code reading is wrong, or compaction dropped the tombstone");
            println!("  before it could be observed. Either way the issue must be corrected.");
            Ok(())
        }
        _ => Err("the page read failed for an unrelated reason; this run proves nothing".into()),
    }
}

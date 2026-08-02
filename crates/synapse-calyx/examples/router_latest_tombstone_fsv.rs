//! Manual FSV for #1954 (corrected scope): does the **gated, key-based** latest
//! path report a tombstone that has been flushed into an SST as visible?
//!
//! ## Why this harness exists and `flushed_tombstone_fsv` is not enough
//!
//! `flushed_tombstone_fsv` tested `scan_cf_range_page_latest`, which resolves
//! through `latest_range_page_from_view` and is **value-based** — it reads
//! values and filters tombstones, so it was never the code #1954 quoted. The
//! quoted code lives in `scan_cf_range_page_at`, behind:
//!
//! ```ignore
//! if self.router_latest_readback.load(Ordering::Acquire) {
//!     let keys = self.scan_cf_range_keys_at(...)?;   // router_latest_keys -> range_keys_until
//!     let values = self.read_batch(...)?;            // filters tombstones
//!     // key present in the key view but absent from the value view => CORRUPT_SHARD
//! }
//! ```
//!
//! `router_latest_readback` is `true` only when a vault is opened with
//! `restore_mvcc_rows: false`. The live daemon opens `full_mvcc_restore`, so on
//! the daemon that branch never runs and the earlier harness could not have
//! reached it at any input. This one opens the vault in the mode that arms it.
//!
//! ## The trap this harness is built to avoid
//!
//! The dangerous outcome is not a failure — it is a *vacuous pass*: opening in
//! the wrong mode, taking the row-table branch, seeing a clean result, and
//! reporting "#1954 does not reproduce on the gated path" having never executed
//! the gated path. Two independent defences:
//!
//! 1. **Read the mode back.** `AsterVault::router_latest_readback()` reports the
//!    branch selector itself. `false` aborts.
//! 2. **The row-table branch is self-identifying on this construction.** With
//!    `restore_mvcc_rows: false` the MVCC row table is *empty*, so the ungated
//!    branch returns **0 rows** where the gated branch returns 63. A silently
//!    ungated run therefore cannot look like a pass.
//!
//! ## Known input, expected output, stated before the run
//!
//! 64 rows written to `Kv` and flushed; key 31 tombstoned and flushed again, so
//! the tombstone is SST-resident with no memtable copy. Then, on a handle
//! reopened with `restore_mvcc_rows: false`:
//!
//! | observation                                | meaning                                     |
//! |--------------------------------------------|---------------------------------------------|
//! | `Err CALYX_ASTER_CORRUPT_SHARD` naming key 31 | #1954 REPRODUCED on the gated path        |
//! | 64 rows, key 31 present                    | WORSE: a deleted row served as live          |
//! | 63 rows, key 31 absent                     | does NOT reproduce; the gated path is sound  |
//! | 0 rows                                     | vacuous — the gate was off; harness aborts   |
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example router_latest_tombstone_fsv -- <empty-scratch-dir>`

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use calyx_aster::mvcc::{Freshness, tombstone_value};
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
/// A plain key/value CF, so nothing else in the vault writes to it and the row
/// set is exactly what this harness put there.
const CF: ColumnFamily = ColumnFamily::Kv;
const ROWS: u64 = 64;
/// Deliberately neither the first nor the last key, so an off-by-one in range
/// handling cannot be mistaken for the finding.
const VICTIM: u64 = 31;

const KEY_PREFIX: &[u8] = b"fsv1954r/";
/// One byte past `'/'`, so the range covers exactly the harness's own keys.
const KEY_END: &[u8] = b"fsv1954r0";

fn key_of(tag: u64) -> Vec<u8> {
    format!("fsv1954r/{tag:06}").into_bytes()
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: router_latest_tombstone_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    let vault_id = VaultId::from_str(VAULT_ID)?;
    let victim = key_of(VICTIM);
    let range = KeyRange {
        start: KEY_PREFIX.to_vec(),
        end: Some(KEY_END.to_vec()),
    };

    println!("router_latest_tombstone_fsv  (#1954, gated key-based path)");
    println!("vault_dir = {}", dir.display());
    println!("cf = {}   rows = {ROWS}   victim = {VICTIM}", CF.name());

    // --- phase A: build the on-disk state with an ordinary writable handle ---
    {
        let vault = AsterVault::open(
            &dir,
            vault_id,
            b"router-latest-tombstone-fsv".to_vec(),
            VaultOptions::default(),
        )?;
        println!(
            "\n=== A. writer handle opened   router_latest_readback = {} (expected false)",
            vault.router_latest_readback()
        );
        if vault.router_latest_readback() {
            return Err("the default open already armed the gated path; the two-mode \
                        comparison below would not be comparing two modes"
                .into());
        }

        let rows = (0..ROWS)
            .map(|tag| (CF, key_of(tag), format!("value-{tag}").into_bytes()))
            .collect::<Vec<_>>();
        vault.write_cf_batch(rows)?;
        let flushed = vault.flush_all_cfs()?.len();
        println!("   wrote {ROWS} rows, flushed {flushed} SST(s)");

        vault.write_cf_batch([(CF, victim.clone(), tombstone_value())])?;
        let flushed2 = vault.flush_all_cfs()?.len();
        println!("   tombstoned key {VICTIM}, flushed {flushed2} more SST(s)");
        println!("   the tombstone is now SST-resident with no memtable copy");

        // Publish a manifest. Without `CURRENT`, `recover_batches` takes its
        // WAL-only branch, which hardcodes `router_latest_readback: false` and
        // therefore *silently ignores* `restore_mvcc_rows: false` — the run
        // would take the row-table branch while believing it took the gated
        // one. Phase B's readback catches that, but the vault has to be
        // manifest-backed for the gated branch to be reachable at all.
        vault.checkpoint()?;
        println!("   checkpointed: CURRENT published, so the reopen can be manifest-backed");

        // Control on the writable handle: the value-based path must already
        // agree the key is gone. If it does not, the delete never took and
        // nothing below is a test of anything.
        let live = vault.scan_cf_latest(CF)?;
        let mine = live
            .iter()
            .filter(|(key, _)| key.starts_with(KEY_PREFIX))
            .count();
        let served = live.iter().any(|(key, _)| key == &victim);
        println!("   control scan_cf_latest: {mine} own rows, victim present = {served}");
        if served || mine != (ROWS as usize - 1) {
            return Err(format!(
                "control failed on the writer handle: expected {} rows and victim absent, \
                 got {mine} rows and victim present = {served}",
                ROWS - 1
            )
            .into());
        }
    }

    // --- phase B: reopen in the mode that arms the gated branch --------------
    let latest_only = VaultOptions {
        restore_mvcc_rows: false,
        ..VaultOptions::default()
    };
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"router-latest-tombstone-fsv".to_vec(),
        latest_only,
    )?;

    let armed = vault.router_latest_readback();
    println!("\n=== B. reopened with restore_mvcc_rows=false");
    println!("   router_latest_readback = {armed} (expected true)");
    if !armed {
        return Err("the gated branch is NOT armed on this handle: every result below \
                    would come from the row-table branch, so this run would prove \
                    nothing about the path #1954 is about"
            .into());
    }

    // Control on the *same* handle, so both paths see one identical vault.
    let live = vault.scan_cf_latest(CF)?;
    let control_rows = live
        .iter()
        .filter(|(key, _)| key.starts_with(KEY_PREFIX))
        .count();
    let control_served = live.iter().any(|(key, _)| key == &victim);
    println!("\n=== C. control on this handle: scan_cf_latest (value-based)");
    println!("   own rows = {control_rows}   victim present = {control_served}");
    if control_served || control_rows != (ROWS as usize - 1) {
        return Err(format!(
            "control failed on the latest-only handle: expected {} rows and victim \
             absent, got {control_rows} rows and victim present = {control_served}",
            ROWS - 1
        )
        .into());
    }

    // --- phase D: the gated key-based page read -----------------------------
    println!("\n=== D. scan_cf_range_page_snapshot (gated key-based path)");
    let snapshot = vault.pin_reader(Freshness::FreshDerived, 30_000);
    println!("   pinned snapshot seq = {}", snapshot.seq());
    let outcome =
        vault.scan_cf_range_page_snapshot(snapshot, CF, &range, None, ROWS as usize + 8);

    let verdict = match &outcome {
        Ok(rows) => {
            let served = rows.iter().any(|(key, _)| key == &victim);
            println!("   Ok: {} row(s)", rows.len());
            println!("   deleted key served as live = {served}");
            if rows.is_empty() {
                "VACUOUS"
            } else if served {
                "SERVED-DELETED-ROW"
            } else if rows.len() == ROWS as usize - 1 {
                "AGREES"
            } else {
                "WRONG-COUNT"
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

    // --- phase E: boundary audit of the key/value merge -----------------------
    //
    // Phase D covers one point on the merge. These are the other three states a
    // key can be in where the key view and the value view could disagree. Each
    // states its expected live count before it runs, and each is checked against
    // BOTH views on the same armed handle, so "the two agree" cannot pass by
    // both being wrong in the same way.
    //
    // These need a writable handle to mutate, so each case writes through a
    // fresh writer, checkpoints, and reopens armed.
    println!("\n=== E. boundary audit (each on a freshly armed handle)");
    drop(vault);

    let cases: [(&str, bool, bool, u64, usize); 3] = [
        // (name, flush_after_write, expect_victim_live, extra_key, expected_own_rows)
        ("E1 tombstone memtable-resident, never flushed", false, false, VICTIM, ROWS as usize - 1),
        ("E2 tombstoned key re-written (resurrection)", true, true, VICTIM, ROWS as usize),
        // E3 runs after E2, which resurrected the victim, so the live count is
        // back to ROWS. Tombstoning a key that was never written must leave it
        // exactly there — the underflow a count-by-subtraction would hit.
        ("E3 tombstone for a key that never existed", true, false, 9_999, ROWS as usize),
    ];

    for (name, flush_after, expect_live, target, expected_rows) in cases {
        {
            let writer = AsterVault::open(
                &dir,
                vault_id,
                b"router-latest-tombstone-fsv".to_vec(),
                VaultOptions::default(),
            )?;
            match name {
                // Re-write the victim so it must come back to life.
                n if n.starts_with("E2") => {
                    writer.write_cf_batch([(
                        CF,
                        key_of(target),
                        format!("resurrected-{target}").into_bytes(),
                    )])?;
                }
                // Tombstone a key that was never written.
                n if n.starts_with("E3") => {
                    writer.write_cf_batch([(CF, key_of(target), tombstone_value())])?;
                }
                // E1: undo E2/E3 is not needed — E1 runs first, and the victim
                // is already tombstoned and flushed by phase A. Write a second
                // tombstone that stays memtable-resident.
                _ => {
                    writer.write_cf_batch([(CF, key_of(target), tombstone_value())])?;
                }
            }
            if flush_after {
                writer.flush_all_cfs()?;
            }
            writer.checkpoint()?;
        }

        let armed_vault = AsterVault::open(
            &dir,
            vault_id,
            b"router-latest-tombstone-fsv".to_vec(),
            VaultOptions {
                restore_mvcc_rows: false,
                ..VaultOptions::default()
            },
        )?;
        if !armed_vault.router_latest_readback() {
            return Err(format!("{name}: handle not armed; case is vacuous").into());
        }

        let values = armed_vault.scan_cf_latest(CF)?;
        let value_rows = values
            .iter()
            .filter(|(key, _)| key.starts_with(KEY_PREFIX))
            .count();
        let value_live = values.iter().any(|(key, _)| key == &key_of(target));

        let snapshot = armed_vault.pin_reader(Freshness::FreshDerived, 30_000);
        let page = armed_vault.scan_cf_range_page_snapshot(
            snapshot,
            CF,
            &range,
            None,
            ROWS as usize + 16,
        )?;
        let key_rows = page.len();
        let key_live = page.iter().any(|(key, _)| key == &key_of(target));

        let agree = value_rows == key_rows && value_live == key_live;
        let correct = key_rows == expected_rows && key_live == expect_live;
        println!(
            "   {name}\n      expected rows={expected_rows} target_live={expect_live}\n\
             \x20     value view  rows={value_rows} target_live={value_live}\n\
             \x20     key   view  rows={key_rows} target_live={key_live}\n\
             \x20     agree={agree}  correct={correct}"
        );
        if !agree {
            return Err(format!("{name}: the two latest views DISAGREE").into());
        }
        if !correct {
            return Err(format!(
                "{name}: both views agree but on the WRONG answer \
                 (expected rows={expected_rows} target_live={expect_live})"
            )
            .into());
        }
    }

    println!("\n--- VERDICT ---");
    match verdict {
        "REPRODUCED" => {
            println!("  #1954 REPRODUCED on the gated path: the key view reported a");
            println!("  flushed tombstone as visible and the page read failed closed.");
            Ok(())
        }
        "SERVED-DELETED-ROW" => Err(
            "WORSE THAN FILED: the gated path served a deleted row as live rather than erroring"
                .into(),
        ),
        "AGREES" => {
            println!("  #1954 does NOT reproduce, on the gated path either.");
            println!("  Both latest views agree at {} rows with the tombstoned key", ROWS - 1);
            println!("  absent, on a handle proven to have router_latest_readback = true.");
            Ok(())
        }
        "VACUOUS" => Err("the gated page read returned 0 rows: this is the row-table \
                          branch signature and the run proves nothing"
            .into()),
        "WRONG-COUNT" => Err(
            "the gated page returned neither the expected live count nor the victim: \
             the disagreement is real but is not the one this harness characterises"
                .into(),
        ),
        _ => Err("the page read failed for an unrelated reason; this run proves nothing".into()),
    }
}

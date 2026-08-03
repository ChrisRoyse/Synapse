//! Full-state verification for #1973: the MVCC row overlay **seeks** to a
//! requested key range instead of walking the whole column family.
//!
//! ## The defect
//!
//! `overlay_table_rows` / `overlay_table_keys` iterated every key in the CF's
//! overlay `BTreeMap` and applied `KeyRange::contains` as a filter. A narrow
//! prefix read therefore cost the *whole family* — and it paid that cost while
//! holding the vault-wide row-table read guard every constellation writer needs
//! (#1950). On the live daemon the shared `overlay_table_rows` site measured a
//! 112 ms mean and a **452 ms max** against a 25 ms budget.
//!
//! ## Why this shape
//!
//! The live census could not say *which* caller was responsible, because
//! `scan_cf_at` (whole family, inherently O(family)) and `scan_cf_range_at`
//! (bounded, and the thing being fixed) shared one site name. This run
//! therefore does two things the production census cannot:
//!
//! 1. it reads the two now-separate sites, so the ranged path is attributable;
//!    and
//! 2. it uses the whole-family arm as the **control**. A whole-family overlay
//!    walk is exactly what the ranged path used to cost, so `scan_cf_at`'s hold
//!    on the same corpus in the same process *is* the pre-fix number for the
//!    ranged read. That makes the comparison a same-process A/B rather than a
//!    before/after across two builds and two machine states.
//!
//! ## Synthetic corpus, so the expected answer is known in advance
//!
//! `ROWS` rows are written into `Graph` with 8-byte big-endian keys `0..ROWS`,
//! so key ordering is numeric ordering and every query's answer is arithmetic:
//! a range over `[a, b)` must return **exactly** `b - a` rows, and they must be
//! exactly the keys `a..b`. A row set that is merely the right *size* is not
//! accepted — the keys themselves are compared, because a seek that lands in
//! the wrong place would still return a plausible count.
//!
//! The rows stay in the overlay (they are committed but not compacted away), so
//! they are the population the row guard is held over.
//!
//! ## Edge cases, with state printed before and after
//!
//! * an **inverted** range (`end < start`) must fail closed with
//!   `CALYX_ASTER_OVERLAY_RANGE_INVERTED`. The shape this replaces returned
//!   zero rows, so a malformed request was indistinguishable from an empty
//!   family — and `BTreeMap::range` would panic on it.
//! * an **empty but ordered** range (`end == start`) must return zero rows and
//!   must NOT error: it is a legal, answerable question.
//! * an **unbounded-end** range must return every key at or above `start`.
//! * a range **entirely past the last key** must return zero rows while still
//!   costing far less than a family walk — this is the case that proves the
//!   seek happens rather than a walk that filters everything out.
//!
//! Run against a disposable copy of a real vault (this example WRITES):
//!
//! ```text
//! cargo run --release -p synapse-calyx --example overlay_range_seek_fsv -- <vault-copy-dir>
//! ```

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use synapse_calyx::{
    SynapseCalyxCfWrite, SynapseCalyxConfig, SynapseCalyxTuningConfig, SynapseCalyxVault,
};

/// The CF the synthetic corpus is written into. `Graph` is the family the live
/// finding named: it holds 81,236 derived-association rows and is read by
/// prefix from several callers.
const CF: ColumnFamily = ColumnFamily::Graph;

/// Synthetic rows written. Large enough that a whole-family walk and a
/// ten-row seek cannot be confused for each other, small enough to write and
/// measure in seconds. The question this answers is "is the cost proportional
/// to the range or to the family", and 20k vs 10 settles that with margin —
/// a larger corpus would not make the conclusion any more certain.
const ROWS: u64 = 20_000;

/// Repeats per arm. Enough that one descheduled run cannot decide the answer.
const REPEATS: usize = 5;

/// Key prefix keeping the synthetic rows in their own contiguous span, clear of
/// whatever real `Graph` keys the copied vault carries.
const KEY_PREFIX: u8 = 0xF7;

fn key_at(index: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(9);
    key.push(KEY_PREFIX);
    key.extend_from_slice(&index.to_be_bytes());
    key
}

fn range(start: u64, end: u64) -> KeyRange {
    KeyRange {
        start: key_at(start),
        end: Some(key_at(end)),
    }
}

/// One site's `(holds, total_held_us)` at this instant.
fn site_counters(vault: &SynapseCalyxVault, site: &str) -> Result<(u64, u64), Box<dyn Error>> {
    vault
        .row_guard_census()
        .into_iter()
        .find(|entry| entry.site == site)
        .map(|entry| (entry.holds, entry.total_held_us))
        .ok_or_else(|| format!("the census has no site named '{site}'").into())
}

#[derive(Clone, Copy, Default)]
struct Arm {
    min_us: u64,
    total_us: u64,
    holds: u64,
    rows: usize,
}

impl Arm {
    fn mean_us(&self) -> f64 {
        self.total_us as f64 / self.holds.max(1) as f64
    }
}

/// Runs one call, bracketing it with the census so the hold is **read** rather
/// than timed from outside. An outside timer would include the work done after
/// the guard is released, which is not what stalls a committer.
fn bracket<F>(
    vault: &SynapseCalyxVault,
    site: &str,
    arm: &mut Arm,
    mut call: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(&SynapseCalyxVault) -> Result<usize, Box<dyn Error>>,
{
    let (holds_before, held_before) = site_counters(vault, site)?;
    arm.rows = call(vault)?;
    let (holds_after, held_after) = site_counters(vault, site)?;
    let delta_holds = holds_after - holds_before;
    if delta_holds != 1 {
        return Err(format!(
            "one call to '{site}' produced {delta_holds} guard holds; a site that is not exactly \
             one hold per call cannot be compared against another"
        )
        .into());
    }
    let held = held_after - held_before;
    arm.min_us = if arm.holds == 0 {
        held
    } else {
        arm.min_us.min(held)
    };
    arm.total_us += held;
    arm.holds += 1;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: overlay_range_seek_fsv <disposable-vault-copy-dir>")?;
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let config = SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    };
    let vault = SynapseCalyxVault::open(config)?;

    println!("overlay_range_seek_fsv  (#1973)");
    println!("vault copy  = {}", vault_dir.display());
    println!("latest_seq  = {}", vault.latest_seq());
    println!("cf          = {}", CF.name());

    let mut failures: Vec<String> = Vec::new();

    // -- state BEFORE --------------------------------------------------------
    let rows_before = vault.scan_cf_at(vault.latest_seq(), CF)?.len();
    let mine_before = vault
        .scan_cf_range_at(vault.latest_seq(), CF, &range(0, ROWS))?
        .len();
    println!("\n== state BEFORE ==");
    println!("  {} rows in {}", rows_before, CF.name());
    println!("  {mine_before} rows in the synthetic span (expected 0)");

    // -- write the synthetic corpus -----------------------------------------
    println!("\n== writing {ROWS} synthetic rows ==");
    for chunk_start in (0..ROWS).step_by(2_000) {
        let chunk_end = (chunk_start + 2_000).min(ROWS);
        let writes: Vec<SynapseCalyxCfWrite> = (chunk_start..chunk_end)
            .map(|index| {
                SynapseCalyxCfWrite::new(
                    CF,
                    key_at(index),
                    format!("{{\"n\":{index}}}").into_bytes(),
                )
            })
            .collect();
        vault.write_cf_batch(writes)?;
    }

    // -- state AFTER the write, read back from the vault ---------------------
    let seq = vault.latest_seq();
    let rows_after = vault.scan_cf_at(seq, CF)?.len();
    let mine_after = vault.scan_cf_range_at(seq, CF, &range(0, ROWS))?.len();
    println!("\n== state AFTER the write ==");
    println!(
        "  {} rows in {} (was {rows_before}, delta {})",
        rows_after,
        CF.name(),
        rows_after as i64 - rows_before as i64
    );
    println!("  {mine_after} rows in the synthetic span (expected {ROWS})");
    if mine_after as u64 != ROWS {
        failures.push(format!(
            "the synthetic span holds {mine_after} rows after writing {ROWS}"
        ));
    }

    // -- correctness: the seek must return EXACTLY the keys asked for --------
    println!("\n== correctness: a range returns exactly its keys ==");
    for (start, end) in [(0_u64, 10_u64), (9_995, 10_005), (ROWS - 10, ROWS)] {
        let got = vault.scan_cf_range_at(seq, CF, &range(start, end))?;
        let expected: Vec<Vec<u8>> = (start..end).map(key_at).collect();
        let actual: Vec<Vec<u8>> = got.iter().map(|(key, _)| key.clone()).collect();
        let ok = actual == expected;
        println!(
            "  [{start}, {end}) -> {} rows, expected {} : keys {}",
            actual.len(),
            end - start,
            if ok { "EXACT MATCH" } else { "MISMATCH" }
        );
        if !ok {
            failures.push(format!(
                "range [{start}, {end}) returned {} rows whose keys are not exactly {start}..{end}",
                actual.len()
            ));
        }
    }

    // -- the A/B: bounded seek vs the whole-family control -------------------
    println!("\n== row-guard hold: bounded seek vs whole-family control ==");
    let mut whole = Arm::default();
    let mut narrow = Arm::default();
    let mut past_end = Arm::default();
    let narrow_range = range(9_995, 10_005);
    let past_end_range = KeyRange {
        start: key_at(ROWS + 1_000),
        end: Some(key_at(ROWS + 1_010)),
    };
    for _ in 0..REPEATS {
        bracket(&vault, "scan_cf_at_overlay", &mut whole, |vault| {
            Ok(vault.scan_cf_at(seq, CF)?.len())
        })?;
        bracket(&vault, "scan_cf_range_at_overlay", &mut narrow, |vault| {
            Ok(vault.scan_cf_range_at(seq, CF, &narrow_range)?.len())
        })?;
        bracket(&vault, "scan_cf_range_at_overlay", &mut past_end, |vault| {
            Ok(vault.scan_cf_range_at(seq, CF, &past_end_range)?.len())
        })?;
    }
    println!(
        "  {:<34} rows={:<7} holds={} min_us={:<9} mean_us={:.1}",
        "scan_cf_at (whole family, CONTROL)",
        whole.rows,
        whole.holds,
        whole.min_us,
        whole.mean_us()
    );
    println!(
        "  {:<34} rows={:<7} holds={} min_us={:<9} mean_us={:.1}",
        "scan_cf_range_at (10-key seek)",
        narrow.rows,
        narrow.holds,
        narrow.min_us,
        narrow.mean_us()
    );
    println!(
        "  {:<34} rows={:<7} holds={} min_us={:<9} mean_us={:.1}",
        "scan_cf_range_at (past last key)",
        past_end.rows,
        past_end.holds,
        past_end.min_us,
        past_end.mean_us()
    );
    let ratio = whole.min_us as f64 / narrow.min_us.max(1) as f64;
    println!(
        "  minimum-hold ratio whole/narrow = {ratio:.1}x  ({} us vs {} us)",
        whole.min_us, narrow.min_us
    );
    // A filter-over-the-whole-family costs the same for both arms by
    // construction, so anything near 1x means the seek is not happening. The
    // bar is deliberately modest relative to the 2000x row ratio: the overlay
    // is only part of the read, and the claim being tested is "proportional to
    // the range, not the family", not a specific speedup.
    if ratio < 5.0 {
        failures.push(format!(
            "the whole-family control cost only {ratio:.1}x the bounded seek; a bounded read that \
             costs as much as a family walk is the defect #1973 reports, not the fix"
        ));
    }
    if past_end.rows != 0 {
        failures.push(format!(
            "a range entirely past the last key returned {} rows",
            past_end.rows
        ));
    }

    // -- edge cases, state printed before and after -------------------------
    println!("\n== edge cases ==");

    // 1. inverted range must fail closed.
    let inverted = KeyRange {
        start: key_at(500),
        end: Some(key_at(100)),
    };
    println!(
        "  [1] inverted range start={} end={} (start > end)",
        500, 100
    );
    match vault.scan_cf_range_at(seq, CF, &inverted) {
        Ok(rows) => {
            println!("      after: Ok({} rows)  <-- REFUSAL MISSING", rows.len());
            failures.push(format!(
                "an inverted range returned Ok({} rows) instead of failing closed",
                rows.len()
            ));
        }
        Err(error) => {
            let expected_code = error.code == "CALYX_ASTER_OVERLAY_RANGE_INVERTED";
            println!("      after: Err({}) {}", error.code, error.message);
            println!(
                "      code is CALYX_ASTER_OVERLAY_RANGE_INVERTED: {}",
                expected_code
            );
            if !expected_code {
                failures.push(format!(
                    "an inverted range failed with {} rather than CALYX_ASTER_OVERLAY_RANGE_INVERTED",
                    error.code
                ));
            }
        }
    }

    // 2. empty but ordered range must answer zero, not error.
    let empty = KeyRange {
        start: key_at(500),
        end: Some(key_at(500)),
    };
    println!("  [2] empty ordered range start == end == 500");
    match vault.scan_cf_range_at(seq, CF, &empty) {
        Ok(rows) => {
            println!("      after: Ok({} rows), expected 0", rows.len());
            if !rows.is_empty() {
                failures.push(format!("an empty range returned {} rows", rows.len()));
            }
        }
        Err(error) => {
            println!(
                "      after: Err({})  <-- a legal question was refused",
                error.code
            );
            failures.push(format!(
                "an empty but ordered range failed with {}; end == start is answerable",
                error.code
            ));
        }
    }

    // 3. unbounded end must return every key at or above start.
    let unbounded = KeyRange {
        start: key_at(ROWS - 25),
        end: None,
    };
    let got = vault.scan_cf_range_at(seq, CF, &unbounded)?;
    let synthetic: Vec<&Vec<u8>> = got
        .iter()
        .map(|(key, _)| key)
        .filter(|key| key.first() == Some(&KEY_PREFIX))
        .collect();
    println!(
        "  [3] unbounded end from {} -> {} rows total, {} of them synthetic, expected 25",
        ROWS - 25,
        got.len(),
        synthetic.len()
    );
    if synthetic.len() != 25 {
        failures.push(format!(
            "an unbounded-end range from {} returned {} synthetic rows, expected 25",
            ROWS - 25,
            synthetic.len()
        ));
    }

    // -- bits on disk: a fresh read-only reopen -----------------------------
    println!("\n== bits on disk: fresh reopen, independent of the writing process ==");
    vault.flush()?;
    drop(vault);
    let reopened = SynapseCalyxVault::open(SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    })?;
    let reopened_seq = reopened.latest_seq();
    let persisted = reopened
        .scan_cf_range_at(reopened_seq, CF, &range(0, ROWS))?
        .len();
    let spot = reopened.scan_cf_range_at(reopened_seq, CF, &range(9_995, 10_005))?;
    let spot_keys: Vec<Vec<u8>> = spot.iter().map(|(key, _)| key.clone()).collect();
    let spot_expected: Vec<Vec<u8>> = (9_995..10_005).map(key_at).collect();
    println!("  reopened latest_seq = {reopened_seq}");
    println!("  synthetic rows persisted = {persisted}, expected {ROWS}");
    println!(
        "  spot range [9995, 10005) after reopen = {} rows, keys exact: {}",
        spot_keys.len(),
        spot_keys == spot_expected
    );
    if persisted as u64 != ROWS {
        failures.push(format!(
            "after a fresh reopen the synthetic span holds {persisted} rows, expected {ROWS}"
        ));
    }
    if spot_keys != spot_expected {
        failures
            .push("after a fresh reopen the spot range's keys are not exactly 9995..10005".into());
    }

    println!("\n== verdict ==");
    if failures.is_empty() {
        println!("PASS: the overlay seeks to the range, returns exactly the requested keys, fails");
        println!("      closed on an inverted range, and the bounded read no longer costs a");
        println!("      whole-family walk.");
        Ok(())
    } else {
        for failure in &failures {
            println!("  FAIL: {failure}");
        }
        Err(format!("{} check(s) failed", failures.len()).into())
    }
}

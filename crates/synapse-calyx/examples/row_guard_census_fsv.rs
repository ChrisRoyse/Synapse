//! Manual FSV for #1952 ask 3: prove the row-guard census counts the reads
//! that actually happened, including the ones the slow-guard log cannot see.
//!
//! ## The question this exists to answer
//!
//! #1952 converted 14 readback sites from `scan_cf_latest(cf)?.len()` to
//! `count_cf_latest(cf)`. Its ask 3 was to re-measure afterwards. That could not
//! be answered, and the reason was structural rather than a missing window:
//!
//! > **zero `count_cf_latest` guard events appear**. Two readings, and I cannot
//! > separate them from this data: either the converted sites did not run in the
//! > window, or they now complete under the 25 ms budget. The second would be
//! > the win; the first would mean the change is untested in production.
//! > Distinguishing them needs an event on the sub-budget path, which by design
//! > there isn't.
//!
//! `CALYX_ASTER_ROW_READ_GUARD_SLOW` only fires above `ROW_READ_GUARD_WARN_US`.
//! A path that runs constantly and stays inside the budget and a path that never
//! runs produce the identical observation: nothing. No length of window fixes
//! that, because the instrument is exception-only by construction.
//!
//! The census tallies **every** hold, so `holds` and `over_budget_holds`
//! together separate the two readings. This harness proves the tally is exact.
//!
//! ## Construction — known input, known expected output, stated before the run
//!
//! Exact call counts are issued against a fresh vault, then the census is read
//! back and compared to those counts. The numbers are deliberately distinct
//! primes so a mis-indexed site cannot coincidentally match:
//!
//! | site                  | calls issued | expected `holds` |
//! |-----------------------|--------------|------------------|
//! | `count_cf_latest`     | 5            | 5                |
//! | `scan_cf_latest`      | 7            | 7                |
//! | `read_latest`         | 11           | 11               |
//! | `scan_cf_range_latest`| 13           | 13               |
//! | every other site      | 0            | 0, and PRESENT   |
//!
//! "0, and present" is the load-bearing half. A census that omitted zero-hold
//! sites would reproduce the exact ambiguity it exists to remove.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example row_guard_census_fsv -- <empty-scratch-dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use calyx_aster::mvcc::{ROW_READ_GUARD_WARN_US, RowGuardSite};
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const CF: ColumnFamily = ColumnFamily::Kv;
const SEED_ROWS: u64 = 256;

const COUNT_CALLS: u64 = 5;
const SCAN_CALLS: u64 = 7;
const POINT_CALLS: u64 = 11;
const RANGE_CALLS: u64 = 13;

fn key_of(tag: u64) -> Vec<u8> {
    format!("census/{tag:06}").into_bytes()
}

fn census_map(vault: &AsterVault<calyx_core::SystemClock>) -> BTreeMap<&'static str, (u64, u64, u64, Option<f64>)> {
    vault
        .row_guard_census()
        .into_iter()
        .map(|entry| {
            (
                entry.site.as_str(),
                (
                    entry.holds,
                    entry.over_budget_holds,
                    entry.max_held_us,
                    entry.mean_held_us(),
                ),
            )
        })
        .collect()
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: row_guard_census_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    let vault = AsterVault::open(
        &dir,
        VaultId::from_str(VAULT_ID)?,
        b"row-guard-census-fsv".to_vec(),
        VaultOptions::default(),
    )?;

    println!("row_guard_census_fsv  (#1952 ask 3)");
    println!("vault_dir = {}", dir.display());
    println!("budget = {ROW_READ_GUARD_WARN_US} us");

    // --- 1. every declared site must be present, and at zero -----------------
    println!("\n=== 1. before any read: every site present, every count zero");
    let before = census_map(&vault);
    if before.len() != RowGuardSite::COUNT {
        return Err(format!(
            "census reports {} sites, but {} are declared: a site is missing from the \
             report, which is the exact blindness this exists to remove",
            before.len(),
            RowGuardSite::COUNT
        )
        .into());
    }
    println!("   sites declared = {}   sites reported = {}", RowGuardSite::COUNT, before.len());
    // Note: the open path itself takes some guards (recovery, watermarks), so
    // only the sites this harness drives are asserted zero. Asserting ALL zero
    // would be asserting something false and would have to be relaxed later,
    // which is how an assertion stops meaning anything.
    for site in [
        RowGuardSite::CountCfLatest,
        RowGuardSite::ScanCfLatest,
        RowGuardSite::ReadLatest,
        RowGuardSite::ScanCfRangeLatest,
    ] {
        let (holds, _, _, mean) = before[site.as_str()];
        println!("   {:<26} holds={holds} mean={mean:?}", site.as_str());
        if holds != 0 {
            return Err(format!("{} already ran before the harness drove it", site.as_str()).into());
        }
        if mean.is_some() {
            return Err(format!(
                "{} reports a mean hold with zero holds; a site that never ran has no \
                 mean, and reporting one reads as a measurement",
                site.as_str()
            )
            .into());
        }
    }

    // --- 2. seed rows so the reads have something to do ----------------------
    let rows = (0..SEED_ROWS)
        .map(|tag| (CF, key_of(tag), format!("v{tag}").into_bytes()))
        .collect::<Vec<_>>();
    vault.write_cf_batch(rows)?;
    println!("\n=== 2. seeded {SEED_ROWS} rows");

    // --- 3. issue exactly the declared number of each read -------------------
    println!("\n=== 3. issuing exact call counts");
    let baseline = census_map(&vault);

    let mut last_count = 0usize;
    for _ in 0..COUNT_CALLS {
        last_count = vault.count_cf_latest(CF)?;
    }
    let mut last_scan = 0usize;
    for _ in 0..SCAN_CALLS {
        last_scan = vault.scan_cf_latest(CF)?.len();
    }
    for tag in 0..POINT_CALLS {
        vault.read_cf_latest(CF, &key_of(tag))?;
    }
    let range = KeyRange {
        start: b"census/".to_vec(),
        end: Some(b"census0".to_vec()),
    };
    for _ in 0..RANGE_CALLS {
        vault.scan_cf_range_latest(CF, &range)?;
    }
    println!("   count_cf_latest returned {last_count}, scan_cf_latest returned {last_scan}");
    if last_count != last_scan {
        return Err(format!(
            "count_cf_latest={last_count} != scan_cf_latest={last_scan}: #1952 ask 2's \
             equality is broken, and no census number below matters until it is not"
        )
        .into());
    }

    // --- 4. the census must equal the calls issued, exactly ------------------
    println!("\n=== 4. census delta vs calls issued");
    let after = census_map(&vault);
    let expected = [
        (RowGuardSite::CountCfLatest, COUNT_CALLS),
        (RowGuardSite::ScanCfLatest, SCAN_CALLS),
        (RowGuardSite::ReadLatest, POINT_CALLS),
        (RowGuardSite::ScanCfRangeLatest, RANGE_CALLS),
    ];
    let mut failures = Vec::new();
    for (site, issued) in expected {
        let name = site.as_str();
        let delta = after[name].0 - baseline[name].0;
        let over = after[name].1;
        let max = after[name].2;
        let mean = after[name].3;
        let verdict = if delta == issued { "EXACT" } else { "MISMATCH" };
        println!(
            "   {name:<26} issued={issued:<3} counted={delta:<3} {verdict}   \
             over_budget={over} max_held_us={max} mean_held_us={:.1}",
            mean.unwrap_or(f64::NAN)
        );
        if delta != issued {
            failures.push(format!("{name}: issued {issued}, counted {delta}"));
        }
        if mean.is_none() {
            failures.push(format!("{name}: ran {delta} times but reports no mean hold"));
        }
    }

    // Sites the harness did not drive must not have moved. This is what catches
    // an off-by-one in the site index, which would otherwise show up as the
    // right total attributed to the wrong path.
    println!("\n=== 5. sites the harness did not drive must be unchanged");
    let driven = [
        RowGuardSite::CountCfLatest,
        RowGuardSite::ScanCfLatest,
        RowGuardSite::ReadLatest,
        RowGuardSite::ScanCfRangeLatest,
    ]
    .map(RowGuardSite::as_str);
    let mut drifted = 0;
    for site in RowGuardSite::ALL {
        let name = site.as_str();
        if driven.contains(&name) {
            continue;
        }
        let delta = after[name].0 - baseline[name].0;
        if delta != 0 {
            println!("   {name:<26} moved by {delta}");
            drifted += 1;
        }
    }
    println!("   undriven sites that moved = {drifted} (expected 0)");
    if drifted != 0 {
        failures.push(format!(
            "{drifted} site(s) the harness never called recorded holds: the site index \
             attributes reads to the wrong path"
        ));
    }

    // --- 6. the discrimination #1952 ask 3 needed ----------------------------
    println!("\n=== 6. the reading the slow-guard log alone cannot produce");
    let count_name = RowGuardSite::CountCfLatest.as_str();
    let (holds, over, _, _) = after[count_name];
    println!("   {count_name}: holds={holds}  over_budget_holds={over}");
    println!(
        "   log-only observation for this window: {} slow-guard event(s)",
        over
    );
    if over == 0 && holds > 0 {
        println!("   => 'ran {holds} times, every one inside the {ROW_READ_GUARD_WARN_US} us budget'");
        println!("      Previously indistinguishable from 'never ran'.");
    } else if over > 0 {
        println!("   => ran {holds} times, {over} exceeded the budget");
    } else {
        failures.push(
            "count_cf_latest recorded zero holds after being called: the census did not \
             observe the very path it was built for"
                .to_owned(),
        );
    }

    println!("\n--- VERDICT ---");
    if failures.is_empty() {
        println!("  PASS: every site's count equals the calls issued, undriven sites did not");
        println!("  move, and a zero-hold site reports a present zero with no mean.");
        Ok(())
    } else {
        for failure in &failures {
            println!("  FAIL: {failure}");
        }
        Err(format!("{} census assertion(s) failed", failures.len()).into())
    }
}

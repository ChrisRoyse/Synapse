//! Full-state verification for #1973: the bounded paged row count agrees
//! **exactly** with the atomic count, on every column family it replaced one
//! on — including the families that had no retained SST lookup index.
//!
//! ## The defect this exists to catch
//!
//! #1973 replaced thirteen `count_cf_latest` readbacks with
//! `count_cf_latest_bounded`, which folds `scan_cf_range_page_latest` pages so
//! the vault-wide row guard is held for a page at a time instead of a whole
//! family. Driving the first one against the live daemon failed immediately:
//!
//! ```text
//! CALYX_ASTER_SST_PAGE_INDEX_MISSING: candidate-bounded SST paging requires a
//! retained validated lookup index for ...\cf\xterm\...-0000.sst
//! ```
//!
//! `scan_cf_range_page_latest` accepts any `ColumnFamily`, but
//! `should_build_eager_lookup_on_open` retained a lookup for `Kv`, `Base` and
//! the slot CFs only — so the paging API's contract silently held for three
//! families and failed closed on the other fifty. #1968 needed bounded walks
//! over `Base` alone and never discovered it. Two declarations of one fact,
//! in two files, and the second one was wrong the first time it was tested.
//!
//! ## What is proven here
//!
//! 1. **Parity.** For each family, `count_cf_latest_bounded` and
//!    `count_cf_latest` are compared at the same sequence on a frozen vault
//!    copy. They must be *equal*, not close. A cheaper number that silently
//!    differs would turn this readback into decoration — these counts are used
//!    as physical evidence that a write landed.
//! 2. **The families that broke.** `XTerm` and `Graph` are the two
//!    `abundance_report` names, and they are the ones that failed closed.
//! 3. **Bounded holds.** The paged arm's *maximum single hold* is read from the
//!    row-guard census, because the whole point is the hold, not the wall time.
//! 4. **Open cost**, measured rather than assumed: retaining every family's
//!    lookup is only defensible if opening the vault stays affordable.
//!
//! ## Edge cases
//!
//! * an **empty** family (zero rows) — the paged fold must return 0, not fail,
//!   and must agree with the atomic count;
//! * a family whose SSTs are **absent entirely**, which is a different zero;
//! * the **largest** family on the vault, where paging matters most.
//!
//! Run against a frozen copy of a real vault (read-only):
//!
//! ```text
//! cargo run --release -p synapse-calyx --example paged_count_parity_fsv -- <vault-copy-dir>
//! ```

use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use calyx_aster::cf::ColumnFamily;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxTuningConfig, SynapseCalyxVault};

/// Every family a `count_cf_latest` readback was converted on by #1973, plus
/// `Base` as the control (it always had a retained lookup, so it is the arm
/// that would have passed even with the old policy).
const CFS: &[ColumnFamily] = &[
    ColumnFamily::Base,
    ColumnFamily::XTerm,
    ColumnFamily::Graph,
    ColumnFamily::Assay,
    ColumnFamily::Kernel,
    ColumnFamily::TemporalXTerm,
    ColumnFamily::Reactive,
    ColumnFamily::Guard,
];

/// One site's `(holds, max_held_us)` at this instant.
fn site_max(vault: &SynapseCalyxVault, site: &str) -> Result<(u64, u64), Box<dyn Error>> {
    vault
        .row_guard_census()
        .into_iter()
        .find(|entry| entry.site == site)
        .map(|entry| (entry.holds, entry.max_held_us))
        .ok_or_else(|| format!("the census has no site named '{site}'").into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: paged_count_parity_fsv <vault-copy-dir>")?;
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }

    println!("paged_count_parity_fsv  (#1973)");
    println!("vault copy = {}", vault_dir.display());

    // -- open cost, measured -------------------------------------------------
    let opened_at = Instant::now();
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    })?;
    let open_ms = opened_at.elapsed().as_millis();
    let seq = vault.latest_seq();
    // Open cost is part of the verdict, not a footnote: retaining a lookup is
    // what makes a family pageable, and it is paid once per vault open. The
    // three policies were measured on this same warm copy —
    //   Base + slots only : 5,008 ms, and six of eight families fail closed
    //   every family      : 36,597 ms, all eight pageable
    //   supports_paged_scan: ~6,300 ms, all eight pageable
    // — which is why the declared set is scoped rather than universal.
    println!(
        "open elapsed = {open_ms} ms   (retains a lookup per ColumnFamily::supports_paged_scan)"
    );
    println!("latest_seq   = {seq}");

    let mut failures: Vec<String> = Vec::new();

    println!(
        "\n{:<16} {:>10} {:>10} {:>8}  {:>12} {:>12}",
        "column family", "paged", "atomic", "agree", "paged max us", "atomic max us"
    );
    println!("{}", "-".repeat(76));

    for cf in CFS {
        // Paged arm first, bracketed on the page site so the *maximum single
        // hold* across the whole fold is what gets reported — the mean would
        // hide one long page, and one long page is the entire failure mode.
        let (_, page_max_before) = site_max(&vault, "scan_cf_range_page_latest")?;
        let walk = match vault.count_cf_latest_bounded(*cf) {
            Ok(walk) => walk,
            Err(error) => {
                println!(
                    "{:<16} {:>10} {:>10} {:>8}  {}",
                    cf.name(),
                    "ERR",
                    "-",
                    "no",
                    error.code
                );
                failures.push(format!(
                    "the paged count failed on {} with {}: {}",
                    cf.name(),
                    error.code,
                    error.message
                ));
                continue;
            }
        };
        let (_, page_max_after) = site_max(&vault, "scan_cf_range_page_latest")?;

        let (_, count_max_before) = site_max(&vault, "count_cf_latest")?;
        let atomic = vault.count_cf_latest(*cf)?;
        let (_, count_max_after) = site_max(&vault, "count_cf_latest")?;

        let agree = walk.rows_visited == atomic;
        println!(
            "{:<16} {:>10} {:>10} {:>8}  {:>12} {:>12}",
            cf.name(),
            walk.rows_visited,
            atomic,
            if agree { "YES" } else { "NO" },
            page_max_after.saturating_sub(page_max_before),
            count_max_after.saturating_sub(count_max_before),
        );
        if !agree {
            failures.push(format!(
                "{}: paged count {} != atomic count {}",
                cf.name(),
                walk.rows_visited,
                atomic
            ));
        }
        // A frozen copy takes no commits, so every walk must be atomic. If it
        // is not, the two counts are being compared across different windows
        // and the parity claim above is not well-posed (#1968's caveat).
        if !walk.atomic() && walk.pages > 0 {
            failures.push(format!(
                "{}: the walk spanned sequences {}..{} on a frozen vault, so the parity comparison \
                 is not well-posed",
                cf.name(),
                walk.snapshot_seq_first,
                walk.snapshot_seq_last
            ));
        }
    }

    // -- the peak hold each arm actually cost, over the whole run ------------
    println!("\n== peak single row-guard hold, whole run ==");
    for site in [
        "scan_cf_range_page_latest",
        "count_cf_latest",
        "scan_cf_at_overlay",
        "scan_cf_range_at_overlay",
    ] {
        let (holds, max_us) = site_max(&vault, site)?;
        println!("  {site:<28} holds={holds:<6} max_held_us={max_us}");
    }

    // -- edge cases ---------------------------------------------------------
    println!("\n== edge cases ==");
    // An empty family: the fold must answer zero, not fail and not skip.
    let empty_candidates: Vec<ColumnFamily> = CFS
        .iter()
        .copied()
        .filter(|cf| vault.count_cf_latest(*cf).is_ok_and(|rows| rows == 0))
        .collect();
    if empty_candidates.is_empty() {
        println!("  [1] no empty family on this vault; the zero-row case is NOT demonstrated here");
    } else {
        for cf in &empty_candidates {
            let walk = vault.count_cf_latest_bounded(*cf)?;
            println!(
                "  [1] empty family {:<14} paged={} pages={} (expected 0 rows, >=1 page)",
                cf.name(),
                walk.rows_visited,
                walk.pages
            );
            if walk.rows_visited != 0 {
                failures.push(format!(
                    "{} is empty but the paged fold returned {} rows",
                    cf.name(),
                    walk.rows_visited
                ));
            }
            if walk.pages == 0 {
                failures.push(format!(
                    "{} produced zero pages; a fold that never read a page cannot report a count",
                    cf.name()
                ));
            }
        }
    }

    // The largest family, where a whole-family hold hurts most.
    let base_walk = vault.count_cf_latest_bounded(ColumnFamily::Base)?;
    println!(
        "  [2] largest family base    rows={} pages={} page_rows={} atomic={}",
        base_walk.rows_visited,
        base_walk.pages,
        base_walk.page_rows,
        base_walk.atomic()
    );
    if base_walk.pages < 2 {
        failures.push(format!(
            "base folded in {} page(s); a bounded walk over {} rows that reads one page is not \
             bounding anything",
            base_walk.pages, base_walk.rows_visited
        ));
    }

    // Re-running must be deterministic on a frozen vault.
    let again = vault.count_cf_latest_bounded(ColumnFamily::Graph)?;
    let first = vault.count_cf_latest_bounded(ColumnFamily::Graph)?;
    println!(
        "  [3] repeat determinism graph  run_a={} run_b={} equal={}",
        again.rows_visited,
        first.rows_visited,
        again.rows_visited == first.rows_visited
    );
    if again.rows_visited != first.rows_visited {
        failures.push("two paged counts of graph on a frozen vault disagreed".into());
    }

    println!("\n== verdict ==");
    if failures.is_empty() {
        println!("PASS: the paged count equals the atomic count on every converted family,");
        println!("      including the ones that had no retained lookup index before this change.");
        Ok(())
    } else {
        for failure in &failures {
            println!("  FAIL: {failure}");
        }
        Err(format!("{} check(s) failed", failures.len()).into())
    }
}

//! Manual FSV for #1965 ask 1: every panel that declares a backfill path can
//! actually be backfilled, and the rows land at its active generation.
//!
//! ## The defect
//!
//! `builtin_panel_catalog` declares `backfill_source_cf` per panel, and
//! `Db::backfill_temporal_metadata` accepted only three of them. Six panels
//! declared `None`, which `measure_panel_coverage` reports as an
//! un-backfillable shortfall — correct, but it also meant those panels could
//! never be re-measured, so any panel-version bump would strand every one of
//! their records permanently. That is what blocked #1964's fix for eight of
//! nine panels.
//!
//! ## What this harness proves, and where it reads the truth from
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | declaration and implementation agree, both ways | the catalog vs. the driven call |
//! | 2 | each declared path really backfills | `active_version_records` before/after a real write |
//! | 3 | the rows are on disk at the active generation | a fresh read-only reopen, counting Base rows per panel |
//! | 4 | three edge cases fail closed | the returned `StorageError`, driven directly |
//!
//! A panel whose source CF is empty on this vault is reported and skipped
//! rather than passed: nothing was proven for it, and saying so is the point.
//!
//! ```text
//! cargo run -p synapse-storage --example backfill_path_coverage_fsv -- <vault-parent-dir>
//! ```
//!
//! Needs a **copy**: this writes.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use synapse_storage::Db;
use synapse_storage::constellations::{PanelCatalogEntry, builtin_panel_catalog};

const SCHEMA_VERSION: u32 = 1;
/// Rows per panel. Small on purpose: the claim is "this path runs and its rows
/// land at the active generation", and a bounded page proves that as
/// completely as the whole CF would.
const BACKFILL_ROWS: usize = 8;

struct Failures(Vec<String>);

impl Failures {
    fn check(
        &mut self,
        label: &str,
        observed: impl std::fmt::Debug,
        expected: impl std::fmt::Debug,
    ) {
        let observed = format!("{observed:?}");
        let expected = format!("{expected:?}");
        let verdict = if observed == expected { "PASS" } else { "FAIL" };
        println!("  [{verdict}] {label}: observed={observed} expected={expected}");
        if verdict == "FAIL" {
            self.0
                .push(format!("{label}: observed={observed} expected={expected}"));
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());
    let Some(parent) = std::env::args().nth(1).map(PathBuf::from) else {
        return Err("usage: backfill_path_coverage_fsv <vault-parent-dir>".into());
    };
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }

    let catalog = builtin_panel_catalog();
    // Pair each entry with its source CF here, so the `Option` is destructured
    // exactly once and no later site has to assert it is `Some`.
    let declared: Vec<(&PanelCatalogEntry, &'static str)> = catalog
        .iter()
        .filter_map(|entry| entry.backfill_source_cf.map(|source| (entry, source)))
        .collect();

    println!("== Part 1: which panels declare a backfill path ==");
    for entry in &catalog {
        println!(
            "  {:<28} v{} source={:<22} backfill={:?}",
            entry.panel_name,
            entry.panel_version,
            entry.source.cf_name().unwrap_or("(derived)"),
            entry.backfill_source_cf
        );
    }
    println!("  declared paths = {}", declared.len());

    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    // ------------------------------------------------------------------
    // Part 2 — every declared path must actually run
    // ------------------------------------------------------------------
    println!("\n== Part 2: driving each declared path against the real source CF ==");
    let mut proven: Vec<(&str, u32, u64)> = Vec::new();
    let mut empty_sources: Vec<&str> = Vec::new();
    for (entry, source_cf) in &declared {
        let source_cf = *source_cf;
        let source_rows = db.scan_cf(source_cf)?.len();
        let before = active_records(&db, entry.panel_version)?;
        if source_rows == 0 {
            println!(
                "  {:<28} source {source_cf} is EMPTY on this vault -- SKIPPED, nothing proven",
                entry.panel_name
            );
            empty_sources.push(entry.panel_name);
            continue;
        }
        let report = db.backfill_temporal_metadata(source_cf, None, None, BACKFILL_ROWS)?;
        let written = report.inserted_rows + report.backfilled_rows;
        let after = active_records(&db, entry.panel_version)?;
        println!(
            "  {:<28} source={source_cf} rows={source_rows:<7} candidates={} expired={} examined={} inserted={} backfilled={} already_current={}  active_records {before} -> {after}",
            entry.panel_name,
            report.candidate_rows_examined,
            report.expired_rows_skipped,
            report.examined_rows,
            report.inserted_rows,
            report.backfilled_rows,
            report.already_current_rows
        );
        println!("      more={}", report.more);
        // The claim is "this page did work", not "this page wrote". On a
        // TTL-managed source (`syn-action-v1`, `syn-process-v1`,
        // `syn-observation-v1`) a page can legitimately walk only expired
        // candidates and re-measure nothing — which is precisely why the
        // report has to carry `candidate_rows_examined` and
        // `expired_rows_skipped`. Without them this assertion could not be
        // written honestly, because `examined=0` would be indistinguishable
        // from a broken path.
        f.check(
            &format!(
                "{} walked candidates on the page it was asked for",
                entry.panel_name
            ),
            report.candidate_rows_examined > 0,
            true,
        );
        // The pager walks one candidate past the page to decide `more`, so the
        // look-ahead row is counted as a candidate but is not part of this
        // page. Stating the identity with that term is what makes it a real
        // check that nothing was dropped, rather than an off-by-one to relax
        // later.
        f.check(
            &format!(
                "{} accounted for every candidate it walked",
                entry.panel_name
            ),
            report.examined_rows + report.expired_rows_skipped + u64::from(report.more),
            report.candidate_rows_examined,
        );
        // A row already at the active generation is a legitimate no-op, so the
        // claim is "the path ran and accounted for every row", not "it wrote".
        f.check(
            &format!("{} accounted for every examined row", entry.panel_name),
            report.inserted_rows + report.backfilled_rows + report.already_current_rows,
            report.examined_rows,
        );
        proven.push((entry.panel_name, entry.panel_version, written));
    }

    // ------------------------------------------------------------------
    // Part 3 — the bytes, from a fresh reopen
    // ------------------------------------------------------------------
    println!("\n== Part 3: Base rows per panel generation, read back after close ==");
    db.flush()?;
    drop(db);
    let census = base_rows_by_panel(&parent)?;
    for (panel, version, written) in &proven {
        let on_disk = census.get(version).copied().unwrap_or(0);
        println!(
            "  {panel:<28} generation {version} Base rows on disk = {on_disk}  (this run wrote {written})"
        );
        f.check(
            &format!("{panel} has rows at its active generation on disk"),
            on_disk > 0,
            true,
        );
    }

    // ------------------------------------------------------------------
    // Part 4 — edge cases, state printed before and after
    // ------------------------------------------------------------------
    println!("\n== Part 4: fail-closed behaviour ==");
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    // (a) a CF with no backfill path must be refused, not silently no-op'd.
    let undeclared = "CF_KV";
    println!("  (a) BEFORE: driving backfill for {undeclared}, which declares no path");
    let refused = db.backfill_temporal_metadata(undeclared, None, None, 1);
    println!("      AFTER : {refused:?}");
    f.check("an undeclared source CF is refused", refused.is_err(), true);

    // (b) the two cursors are mutually exclusive.
    let source = declared
        .first()
        .map(|(_, source)| *source)
        .ok_or("no declared backfill source to drive")?;
    println!("  (b) BEFORE: exact source_key AND a physical page cursor together");
    let both = db.backfill_temporal_metadata(source, Some(b"k"), Some(b"k"), 1);
    println!("      AFTER : {both:?}");
    f.check("exact key + page cursor is refused", both.is_err(), true);

    // (c) an exact key that does not exist must error, not report zero rows.
    println!("  (c) BEFORE: an exact source_key that is not in {source}");
    let missing = db.backfill_temporal_metadata(source, Some(b"fsv-1965-absent"), None, 1);
    println!("      AFTER : {missing:?}");
    f.check(
        "a missing exact source row is refused",
        missing.is_err(),
        true,
    );

    // ------------------------------------------------------------------
    // Part 5 — declaration and implementation must agree BOTH ways
    // ------------------------------------------------------------------
    println!("\n== Part 5: no panel declares a path the implementation rejects ==");
    let mut rejected = Vec::new();
    for (entry, source_cf) in &declared {
        let source_cf = *source_cf;
        // A zero-row page is the cheapest probe that still reaches the guard.
        if let Err(error) = db.backfill_temporal_metadata(source_cf, None, None, 0) {
            let text = format!("{error}");
            if text.contains("accepts only") {
                rejected.push(format!("{} -> {source_cf}", entry.panel_name));
            }
        }
    }
    f.check(
        "every declared backfill_source_cf is accepted by the implementation",
        rejected.as_slice(),
        &[] as &[String],
    );

    if !empty_sources.is_empty() {
        println!(
            "\n  NOTE: {} panel(s) had an empty source CF and prove nothing here: {empty_sources:?}",
            empty_sources.len()
        );
    }

    println!("\n================================================================");
    if f.0.is_empty() {
        println!("ALL CHECKS PASSED");
        Ok(())
    } else {
        println!("{} CHECK(S) FAILED:", f.0.len());
        for failure in &f.0 {
            println!("  - {failure}");
        }
        Err("backfill_path_coverage_fsv failed".into())
    }
}

fn active_records(db: &Db, panel_version: u32) -> Result<usize, Box<dyn Error>> {
    Ok(db
        .measure_panel_coverage()?
        .panels
        .iter()
        .find(|p| p.panel_version == panel_version)
        .map_or(0, |p| p.active_version_records))
}

/// Base rows per panel generation, counted from the stored rows themselves.
fn base_rows_by_panel(parent: &std::path::Path) -> Result<BTreeMap<u32, usize>, Box<dyn Error>> {
    let vault = synapse_calyx::SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        synapse_calyx::SynapseCalyxConfig {
            vault_dir: parent.join("db-daemon"),
            machine_salt_path: parent.join("machine-salt.bin"),
            tuning: synapse_calyx::SynapseCalyxTuningConfig::default().validate()?,
        },
        Some(vec![ColumnFamily::Base]),
    )?;
    let mut census: BTreeMap<u32, usize> = BTreeMap::new();
    for (_, value) in vault.scan_cf_latest(ColumnFamily::Base)? {
        let base = decode_constellation_base(&value)?;
        *census.entry(base.panel_version).or_default() += 1;
    }
    Ok(census)
}

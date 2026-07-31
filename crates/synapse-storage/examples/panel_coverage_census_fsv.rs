//! Full State Verification for the panel coverage + grounding census
//! (issues #1927 ask 1, #1920 ask 1).
//!
//! # What is being proven, and against what
//!
//! The census claims two numbers per panel generation — how many `Base`
//! constellations carry it, and how many of those are grounded — from ONE
//! decode-only scan with no per-slot hydration. The whole value of the readback
//! rests on those numbers being the same numbers the expensive authoritative
//! path already reports. So this run does not check the census against itself:
//!
//! 1. **Independent-path agreement.** For every panel small enough that the
//!    heavy path is not clamped, the census's `records` and `grounded_records`
//!    are compared against `SynapseCalyxVault::grounding_gap_report`, which
//!    hydrates every record from its per-slot CFs and counts anchors through a
//!    completely separate code path. Two implementations, one number, or this
//!    run fails.
//! 2. **Physical accounting.** `base_cf_rows == records_total + decode_failures`
//!    over the real `Base` CF. A census that silently dropped rows would pass
//!    every fraction check and fail this one.
//! 3. **Denominator agreement.** Each full-CF panel's `source_cf_rows` is
//!    compared against an independent `Db` row count of the same CF.
//!
//! # Boundary and edge cases exercised
//!
//! * A panel with **zero records** (registered, never written to) must report
//!   coverage 1.0 against an empty CF and must NOT be flagged deficient — a
//!   0/0 read as a deficiency would flag every unused panel forever.
//! * A **subset-fed** panel (`mcp-usage`, a key prefix inside `CF_KV`) must
//!   report `coverage_fraction = None`. Deriving `records / CF_KV rows` there
//!   would read as a permanent ~3% outage against a denominator that is the
//!   wrong population.
//! * An **observation-shaped** panel at 0.0 grounded coverage must NOT be
//!   flagged `grounding_below_floor`, because it is declared `outcome_bearing =
//!   false` (#1920 ask 3), while an outcome-bearing panel at the same 0.0 must
//!   be flagged.
//!
//! # Usage
//!
//! ```text
//! cargo run -p synapse-storage --example panel_coverage_census_fsv -- <vault-parent-dir>
//! ```
//!
//! `<vault-parent-dir>` is the directory holding `db-daemon/`, i.e. a real (or
//! copied) Synapse data directory. This example is READ-ONLY with respect to
//! panel data; point it at a copy so the live daemon keeps its writer lock.

use std::error::Error;
use std::path::PathBuf;

use synapse_storage::Db;
use synapse_storage::constellations::{PanelSource, builtin_panel_catalog};

const SCHEMA_VERSION: u32 = 1;

/// Panels with fewer records than the heavy path's clamp can be cross-checked
/// against it directly. Above the clamp the two paths measure different
/// populations by construction, and comparing them would prove nothing.
const HEAVY_PATH_RECORD_CLAMP: usize = 20_000;

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "FAIL" }
}

fn main() -> Result<(), Box<dyn Error>> {
    let parent = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: panel_coverage_census_fsv <dir-containing-db-daemon>")?;
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!(
            "no db-daemon directory under {}; point this at a Synapse data dir (or a copy of one)",
            parent.display()
        )
        .into());
    }

    println!("panel_coverage_census_fsv");
    println!("  vault_dir = {}", vault_dir.display());
    println!();

    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let mut failures: Vec<String> = Vec::new();

    // -----------------------------------------------------------------------
    // BEFORE: the independently-read Source of Truth
    // -----------------------------------------------------------------------
    println!("== SOURCE OF TRUTH (read independently, before the census) ==");
    let cf_counts = db.cf_row_counts()?;
    for entry in builtin_panel_catalog() {
        if let PanelSource::FullCf(cf) = entry.source {
            println!(
                "  {cf:<24} rows={}",
                cf_counts.get(cf).copied().unwrap_or(0)
            );
        }
    }
    println!();

    // -----------------------------------------------------------------------
    // EXECUTE
    // -----------------------------------------------------------------------
    let started = std::time::Instant::now();
    let report = db.measure_panel_coverage()?;
    let census_ms = started.elapsed().as_millis();
    println!("== CENSUS ({census_ms} ms, one decode-only Base scan) ==");
    println!(
        "  base_cf_rows={} records_total={} decode_failures={} accounting_holds={}",
        report.base_cf_rows,
        report.records_total,
        report.decode_failures,
        report.accounting_holds()
    );
    println!(
        "  superseded_records_total={} coverage_floor={} grounding_floor={}",
        report.superseded_records_total, report.coverage_floor, report.grounding_floor
    );
    if let Some(detail) = &report.first_decode_failure {
        println!("  first_decode_failure={detail}");
    }
    println!();

    // CHECK 2 — physical accounting.
    if !report.accounting_holds() {
        failures.push(format!(
            "accounting: base_cf_rows={} != records_total={} + decode_failures={}",
            report.base_cf_rows, report.records_total, report.decode_failures
        ));
    }
    println!(
        "  [check] every Base row counted exactly once: {}",
        verdict(report.accounting_holds())
    );
    println!();

    println!("== PER-PANEL ==");
    println!(
        "  {:<28} {:>9} {:>8} {:>9} {:>8} {:>8} {:>10} {:>6}",
        "panel", "records", "cf_rows", "coverage", "grounded", "gfrac", "superseded", "assay"
    );
    for panel in &report.panels {
        println!(
            "  {:<28} {:>9} {:>8} {:>9} {:>8} {:>8.4} {:>10} {:>6}",
            format!("{}@{}", panel.panel_name, panel.panel_version),
            panel.active_version_records,
            panel
                .source_cf_rows
                .map_or_else(|| "-".to_owned(), |rows| rows.to_string()),
            panel
                .coverage_fraction
                .map_or_else(|| "n/a".to_owned(), |fraction| format!("{fraction:.4}")),
            panel.grounded_records,
            panel.grounded_fraction,
            panel.superseded_records,
            if panel.assay_measurable() {
                "yes"
            } else {
                "no"
            },
        );
    }
    println!();

    // CHECK 3 — denominator agreement against the independent row count.
    println!("== CHECK: denominators match an independent CF row count ==");
    for entry in builtin_panel_catalog() {
        let PanelSource::FullCf(cf) = entry.source else {
            continue;
        };
        let Some(panel) = report
            .panels
            .iter()
            .find(|row| row.panel_version == entry.panel_version)
        else {
            failures.push(format!("panel {} absent from the report", entry.panel_name));
            continue;
        };
        let independent = cf_counts.get(cf).copied().unwrap_or(0);
        let reported = panel.source_cf_rows.unwrap_or(0);
        let held = independent == reported;
        if !held {
            failures.push(format!(
                "{}: source_cf_rows={reported} but an independent count of {cf} says {independent}",
                entry.panel_name
            ));
        }
        println!(
            "  {:<28} reported={reported:<8} independent={independent:<8} {}",
            entry.panel_name,
            verdict(held)
        );
    }
    println!();

    // CHECK 1 — the load-bearing one: agreement with the heavy authoritative path.
    println!("== CHECK: census agrees with grounding_gap (a fully independent path) ==");
    println!("  grounding_gap hydrates every record from its per-slot CFs and counts anchors");
    println!("  through separate code. Same two numbers, or this run is worthless.");
    for panel in &report.panels {
        if panel.active_version_records == 0 {
            println!(
                "  {:<28} SKIPPED — 0 records, nothing for either path to count",
                panel.panel_name
            );
            continue;
        }
        if panel.active_version_records > HEAVY_PATH_RECORD_CLAMP {
            println!(
                "  {:<28} SKIPPED — {} records exceeds the heavy path's {HEAVY_PATH_RECORD_CLAMP} clamp, so the two paths would measure different populations",
                panel.panel_name, panel.active_version_records
            );
            continue;
        }
        let heavy = db.grounding_gap_intelligence(panel.panel_version, HEAVY_PATH_RECORD_CLAMP)?;
        let records_held = heavy.records_scanned == panel.active_version_records;
        let grounded_held = heavy.grounded_records == panel.grounded_records;
        if !records_held {
            failures.push(format!(
                "{}: census records={} but grounding_gap records_scanned={}",
                panel.panel_name, panel.active_version_records, heavy.records_scanned
            ));
        }
        if !grounded_held {
            failures.push(format!(
                "{}: census grounded={} but grounding_gap grounded_records={}",
                panel.panel_name, panel.grounded_records, heavy.grounded_records
            ));
        }
        println!(
            "  {:<28} records {}=={} {}   grounded {}=={} {}",
            panel.panel_name,
            panel.active_version_records,
            heavy.records_scanned,
            verdict(records_held),
            panel.grounded_records,
            heavy.grounded_records,
            verdict(grounded_held),
        );
    }
    println!();

    // -----------------------------------------------------------------------
    // BOUNDARY / EDGE CASES
    // -----------------------------------------------------------------------
    println!("== EDGE CASE 1: an empty panel is fully covered, not deficient ==");
    let mut empty_seen = 0usize;
    for panel in &report.panels {
        if panel.active_version_records != 0 || !panel.source_is_full_cf {
            continue;
        }
        if panel.source_cf_rows != Some(0) {
            // A panel with 0 records against a NON-empty CF is a real shortfall
            // and must be flagged; that is the #1927 condition, not this case.
            continue;
        }
        empty_seen += 1;
        let held = panel.coverage_fraction == Some(1.0) && !panel.coverage_below_floor;
        if !held {
            failures.push(format!(
                "{}: empty panel over an empty CF reported coverage={:?} below_floor={}",
                panel.panel_name, panel.coverage_fraction, panel.coverage_below_floor
            ));
        }
        println!(
            "  {:<28} records=0 cf_rows=0 -> coverage={:?} below_floor={} {}",
            panel.panel_name,
            panel.coverage_fraction,
            panel.coverage_below_floor,
            verdict(held)
        );
    }
    if empty_seen == 0 {
        println!("  (no empty full-CF panel present on this vault; case not exercised)");
    }
    println!();

    println!("== EDGE CASE 2: a subset-fed panel gets NO coverage fraction ==");
    let mut subset_seen = 0usize;
    for entry in builtin_panel_catalog() {
        if !matches!(entry.source, PanelSource::SubsetOfCf(_)) {
            continue;
        }
        let Some(panel) = report
            .panels
            .iter()
            .find(|row| row.panel_version == entry.panel_version)
        else {
            continue;
        };
        subset_seen += 1;
        let held = panel.coverage_fraction.is_none()
            && panel.source_cf_rows.is_none()
            && !panel.coverage_below_floor;
        if !held {
            failures.push(format!(
                "{}: subset-fed panel exposed coverage={:?} cf_rows={:?} below_floor={} — that \
                 ratio's denominator is the wrong population",
                panel.panel_name,
                panel.coverage_fraction,
                panel.source_cf_rows,
                panel.coverage_below_floor
            ));
        }
        println!(
            "  {:<28} records={:<6} coverage={:?} cf_rows={:?} {}",
            panel.panel_name,
            panel.active_version_records,
            panel.coverage_fraction,
            panel.source_cf_rows,
            verdict(held)
        );
    }
    if subset_seen == 0 {
        failures.push("no subset-fed panel in the catalog; edge case 2 proved nothing".to_owned());
    }
    println!();

    println!("== EDGE CASE 4: a panel outliving its TTL'd source is NOT a shortfall ==");
    println!("  CF_ACTION_LOG carries a 24h audit TTL and CF_PROCESS_HISTORY a 6h one, so the GC");
    println!("  expires source rows while the Base constellations measured from them are sacred");
    println!("  and never auto-deleted. The panel then holds MORE records than its CF has rows.");
    println!("  That must report unclamped (>1.0), flag records_exceed_source, and must NOT be");
    println!(
        "  called a coverage deficiency — nothing is missing and a backfill would do nothing."
    );
    let mut exceed_seen = 0usize;
    for panel in &report.panels {
        if !panel.records_exceed_source {
            continue;
        }
        exceed_seen += 1;
        // Either the true unclamped ratio (> 1.0), or None when the source CF
        // is empty and the ratio is genuinely undefined. Never a clean-looking
        // 1.0, which would say "fully covered" about a panel whose source the
        // GC has emptied underneath it.
        let fraction_honest = match panel.coverage_fraction {
            Some(fraction) => fraction > 1.0,
            None => panel.source_cf_rows == Some(0),
        };
        let held = fraction_honest
            && !panel.coverage_below_floor
            && !report.coverage_deficient_panels.contains(&panel.panel_name)
            && panel.uncovered_rows() == Some(0);
        if !held {
            failures.push(format!(
                "{}: records={} > cf_rows={:?} but coverage={:?} below_floor={} uncovered={:?} \
                 — the ratio must be reported unclamped (or None when the CF is empty and the \
                 ratio is undefined) and must not read as a shortfall",
                panel.panel_name,
                panel.active_version_records,
                panel.source_cf_rows,
                panel.coverage_fraction,
                panel.coverage_below_floor,
                panel.uncovered_rows(),
            ));
        }
        println!(
            "  {:<28} records={:<6} cf_rows={:<6} coverage={:<9} below_floor={} {}",
            panel.panel_name,
            panel.active_version_records,
            panel.source_cf_rows.unwrap_or(0),
            panel.coverage_fraction.map_or_else(
                || "undefined".to_owned(),
                |fraction| format!("{fraction:.4}")
            ),
            panel.coverage_below_floor,
            verdict(held)
        );
    }
    // Every panel flagged must appear in the roll-up and vice versa; a flag the
    // roll-up disagrees with would be invisible to health, which reads the
    // roll-up.
    let flagged: Vec<String> = report
        .panels
        .iter()
        .filter(|panel| panel.records_exceed_source)
        .map(|panel| panel.panel_name.clone())
        .collect();
    let rollup_held = flagged == report.records_exceed_source_panels;
    if !rollup_held {
        failures.push(format!(
            "roll-up disagrees with the per-panel flags: rows say {flagged:?}, report says {:?}",
            report.records_exceed_source_panels
        ));
    }
    println!(
        "  roll-up matches the per-panel flags: {:?} {}",
        report.records_exceed_source_panels,
        verdict(rollup_held)
    );
    if exceed_seen == 0 {
        println!("  (no panel currently outlives its source CF; case not exercised this run)");
    }
    println!();

    println!("== EDGE CASE 3: 0.0 grounded is a gap ONLY on an outcome-bearing panel ==");
    println!("  (#1920 ask 3: the declaration decides, not the number)");
    let mut declared_observation = 0usize;
    let mut declared_outcome = 0usize;
    for panel in &report.panels {
        if panel.active_version_records == 0 {
            continue;
        }
        if panel.outcome_bearing {
            declared_outcome += 1;
            let expected = panel.grounded_fraction < report.grounding_floor;
            let held = panel.grounding_below_floor == expected;
            if !held {
                failures.push(format!(
                    "{}: outcome-bearing panel at gfrac={:.4} reported grounding_below_floor={}",
                    panel.panel_name, panel.grounded_fraction, panel.grounding_below_floor
                ));
            }
            println!(
                "  {:<28} outcome-bearing gfrac={:.4} -> flagged={} {}",
                panel.panel_name,
                panel.grounded_fraction,
                panel.grounding_below_floor,
                verdict(held)
            );
        } else {
            declared_observation += 1;
            let held = !panel.grounding_below_floor;
            if !held {
                failures.push(format!(
                    "{}: observation-shaped panel was flagged grounding_below_floor; a timeline \
                     row is an observation and has no outcome to carry",
                    panel.panel_name
                ));
            }
            println!(
                "  {:<28} observation    gfrac={:.4} -> flagged={} {}",
                panel.panel_name,
                panel.grounded_fraction,
                panel.grounding_below_floor,
                verdict(held)
            );
        }
    }
    if declared_observation == 0 || declared_outcome == 0 {
        failures.push(format!(
            "edge case 3 needs both kinds of panel with records present; saw {declared_observation} \
             observation and {declared_outcome} outcome-bearing"
        ));
    }
    println!();

    // -----------------------------------------------------------------------
    // VERDICT
    // -----------------------------------------------------------------------
    println!("== DEFICIENCIES THE CENSUS REPORTS ==");
    println!(
        "  coverage_deficient   = {:?}",
        report.coverage_deficient_panels
    );
    println!(
        "  unbackfillable       = {:?}",
        report.unbackfillable_deficient_panels
    );
    println!(
        "  grounding_deficient  = {:?}",
        report.grounding_deficient_panels
    );
    println!(
        "  records_exceed_source= {:?}  (TTL'd source, retained constellations - NOT a shortfall)",
        report.records_exceed_source_panels
    );
    println!(
        "  unknown_panel_versions = {:?}",
        report.unknown_panel_versions
    );
    match report.most_owed_backfill() {
        Some(target) => println!(
            "  most_owed_backfill   = {} ({} uncovered rows, from {:?})",
            target.panel_name,
            target.uncovered_rows().unwrap_or(0),
            target.backfill_source_cf
        ),
        None => println!("  most_owed_backfill   = <none owed, or none repairable>"),
    }
    println!();

    if failures.is_empty() {
        println!("panel_coverage_census_fsv: PASS — every check held against the physical vault.");
        Ok(())
    } else {
        println!(
            "panel_coverage_census_fsv: FAIL — {} check(s):",
            failures.len()
        );
        for failure in &failures {
            println!("  - {failure}");
        }
        Err(format!("{} FSV check(s) failed", failures.len()).into())
    }
}

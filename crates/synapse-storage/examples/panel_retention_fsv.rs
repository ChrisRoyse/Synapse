//! Full State Verification for the #1927 ask 3 retention classification.
//!
//! # What ask 3 actually needed
//!
//! `superseded_records_total = 41,237` is a number nobody can act on. Ask 3 asked
//! what happens to those records, and the honest answer depends on two facts the
//! census computed and then threw away on its way out of the vault: **is anything
//! at that generation grounded**, and **is anything still writing to it**.
//!
//! This run proves the classification built from those facts is real, against the
//! bytes rather than against itself.
//!
//! # Source of truth, and the independent path
//!
//! The source of truth is the Calyx `Base` column family on disk. The census
//! reads it once, decode-only, and reports per-generation counts.
//! `grounding_gap_intelligence` reads the same records through **entirely
//! separate code** — it hydrates every record from its per-slot CFs and counts
//! anchors there — so pointing it at a *superseded* panel version yields the same
//! two numbers by a different route, or the classification is not measuring what
//! it claims.
//!
//! That cross-check is the load-bearing one here, because
//! `superseded_grounded_records` is the number the whole retention decision turns
//! on: it is what separates "derived state a re-measure can rebuild" from "the
//! only surviving copy of an observed outcome".
//!
//! # Invariants checked
//!
//! 1. `superseded_grounded_records <= superseded_records`, per panel and in total.
//!    A grounded count exceeding the population it is drawn from would mean the
//!    generation join is wrong.
//! 2. `superseded_reclaim_candidates <= superseded_records - superseded_grounded_records`.
//!    A candidate set that includes a grounded record is the one failure mode
//!    that would cause a reclaim to destroy an anchor.
//! 3. A panel that is coverage-deficient, or has no `backfill_source_cf`, must
//!    contribute **zero** candidates. Reclaiming from an old generation while
//!    the active one has not covered the corpus deletes the only measurement of
//!    records the backfill has not reached yet.
//! 4. An **open** superseded generation (something still writing to it)
//!    contributes zero candidates and is named in
//!    `open_superseded_generations`.
//! 5. `orphaned_records` is a **per-record source-key probe**, not a
//!    subtraction (#1940), and splits into `orphaned_source_evicted` (the
//!    source CF is declared TTL-managed — expected) and
//!    `orphaned_source_missing` (it is not — a real integrity finding).
//! 6. Per-panel totals sum to the report-level totals.
//!
//! # Usage
//!
//! ```text
//! cargo run -p synapse-storage --example panel_retention_fsv -- <vault-parent-dir>
//! ```
//!
//! READ-ONLY with respect to panel data. Point it at a copy so the live daemon
//! keeps its writer lock.

use std::error::Error;
use std::path::PathBuf;

use std::collections::{BTreeMap, BTreeSet};

use synapse_calyx::{SynapseCalyxPanelCensus, SynapseCalyxPanelCensusEntry};
use synapse_storage::Db;
use synapse_storage::constellations::{
    PanelSource, SYN_ACTION_PANEL_VERSION, SYN_TIMELINE_PANEL_VERSION,
    SYN_TIMELINE_PANEL_VERSION_PRE_1900, builtin_panel_catalog,
};
use synapse_storage::panel_coverage::build_panel_coverage_report;

const SCHEMA_VERSION: u32 = 1;
const HEAVY_PATH_RECORD_CLAMP: usize = 20_000;

/// `CF_TIMELINE`, which `syn-timeline-v1` declares as both its source and its
/// backfill path — the one built-in panel that satisfies every reclaim
/// precondition, so it is the vehicle for the synthetic cases.
const TIMELINE_CF: &str = "CF_TIMELINE";

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "FAIL" }
}

/// A synthetic source row key, so a record's declared provenance is a value the
/// #1940 probe can test for membership.
fn source_key(index: usize) -> String {
    format!("{index:08x}")
}

/// The set of source keys a CF physically holds: `present` of them, starting at
/// index 0. Every record above that index is an orphan by construction, which
/// is what makes the expected probe output known in advance.
fn source_keys(cf: &str, present: usize) -> BTreeMap<String, BTreeSet<String>> {
    BTreeMap::from([(cf.to_owned(), (0..present).map(source_key).collect())])
}

/// Build a census holding exactly one active and one superseded generation of
/// `syn-timeline-v1`, with every timestamp and count chosen by the caller.
fn synthetic_census(
    active_records: usize,
    active_earliest_ms: u64,
    superseded_records: usize,
    superseded_grounded: usize,
    superseded_latest_ms: u64,
) -> SynapseCalyxPanelCensus {
    let entry = |version, records, grounded, earliest, latest| SynapseCalyxPanelCensusEntry {
        panel_version: version,
        records,
        grounded_records: grounded,
        anchor_kind_records: BTreeMap::new(),
        earliest_created_at_ms: Some(earliest),
        latest_created_at_ms: Some(latest),
        // Each record declares its own source key, so the #1940 orphan probe
        // has something to probe. `source_keys` below overrides these for the
        // cases that exercise the probe itself.
        source_key_hexes: BTreeMap::from([(
            TIMELINE_CF.to_owned(),
            (0..records).map(source_key).collect(),
        )]),
        unattributed_records: 0,
    };
    SynapseCalyxPanelCensus {
        entries: vec![
            entry(
                SYN_TIMELINE_PANEL_VERSION_PRE_1900,
                superseded_records,
                superseded_grounded,
                1,
                superseded_latest_ms,
            ),
            entry(
                SYN_TIMELINE_PANEL_VERSION,
                active_records,
                0,
                active_earliest_ms,
                active_earliest_ms + 1,
            ),
        ],
        base_cf_rows: active_records + superseded_records,
        decode_failures: 0,
        first_decode_failure: None,
        measured_at_unix_ms: Some(1_785_000_000_000),
        // Hand-assembled to exercise the retention selection downstream of the
        // fold; no `Base` walk ran, and this says so rather than inventing one.
        walk: synapse_calyx::SynapseCalyxCfWalk::not_walked("base"),
    }
}

/// Runs the cases the live corpus cannot be in. Returns failure descriptions.
fn run_synthetic_cases() -> Vec<String> {
    let mut failures = Vec::new();
    let timeline = |report: &synapse_storage::panel_coverage::PanelCoverageReport| {
        report
            .panels
            .iter()
            .find(|panel| panel.panel_version == SYN_TIMELINE_PANEL_VERSION)
            .cloned()
    };

    // --- S1: an OPEN superseded generation -------------------------------
    // A superseded record written at t=500 while the active generation's oldest
    // is t=400 means something is STILL writing at the old version. Expected,
    // by construction: closed=false, named in open_superseded_generations, and
    // ZERO candidates despite all 100 records being ungrounded.
    let rows = BTreeMap::from([(TIMELINE_CF.to_owned(), 1000u64)]);
    let report = build_panel_coverage_report(
        &synthetic_census(1000, 400, 100, 0, 500),
        &rows,
        &source_keys(TIMELINE_CF, 1000),
    );
    let Some(panel) = timeline(&report) else {
        failures.push("syn-timeline-v1 absent from a synthetic report".to_owned());
        return failures;
    };
    let generation = &panel.superseded_versions_present[0];
    let expected_label = format!("syn-timeline-v1@{SYN_TIMELINE_PANEL_VERSION_PRE_1900}");
    let held = !generation.closed
        && panel.superseded_reclaim_candidates == 0
        && report.open_superseded_generations == vec![expected_label.clone()];
    println!(
        "  S1 open generation (superseded latest 500 > active earliest 400)\n     \
         closed={} candidates={} (expected 0 of 100 ungrounded) named={:?}  {}",
        generation.closed,
        panel.superseded_reclaim_candidates,
        report.open_superseded_generations,
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "S1: closed={} candidates={} named={:?}",
            generation.closed,
            panel.superseded_reclaim_candidates,
            report.open_superseded_generations
        ));
    }

    // --- S2: the #1927 condition itself ----------------------------------
    // 389 active records against 22,999 source rows = 1.7% coverage. Every one
    // of the 100 superseded records is ungrounded and its generation is closed,
    // so conditions 1 and 2 hold — and candidates must STILL be 0, because
    // reclaiming while the active generation covers 1.7% of the corpus would
    // delete the only measurement of records the backfill has not reached.
    let rows = BTreeMap::from([(TIMELINE_CF.to_owned(), 22_999u64)]);
    let report = build_panel_coverage_report(
        &synthetic_census(389, 900, 100, 0, 500),
        &rows,
        &source_keys(TIMELINE_CF, 22_999),
    );
    let Some(panel) = timeline(&report) else {
        failures.push("syn-timeline-v1 absent from a synthetic report".to_owned());
        return failures;
    };
    let held = panel.coverage_below_floor
        && panel.superseded_versions_present[0].closed
        && panel.superseded_grounded_records == 0
        && panel.superseded_reclaim_candidates == 0;
    println!(
        "  S2 #1927 condition (389/22999 = {:.4} coverage)\n     \
         below_floor={} closed={} ungrounded=100 candidates={} (expected 0)  {}",
        panel.coverage_fraction.unwrap_or(f32::NAN),
        panel.coverage_below_floor,
        panel.superseded_versions_present[0].closed,
        panel.superseded_reclaim_candidates,
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "S2: below_floor={} candidates={}",
            panel.coverage_below_floor, panel.superseded_reclaim_candidates
        ));
    }

    // --- S3: partially grounded superseded generation ---------------------
    // 100 superseded records, 40 grounded, closed, active generation fully
    // covering. Expected by arithmetic fixed here: exactly 60 candidates, and
    // superseded_grounded_records exactly 40. The failure this catches is a
    // candidate count of 100 — which would mean a reclaim eats 40 anchors.
    let rows = BTreeMap::from([(TIMELINE_CF.to_owned(), 1000u64)]);
    let report = build_panel_coverage_report(
        &synthetic_census(1000, 900, 100, 40, 500),
        &rows,
        &source_keys(TIMELINE_CF, 1000),
    );
    let Some(panel) = timeline(&report) else {
        failures.push("syn-timeline-v1 absent from a synthetic report".to_owned());
        return failures;
    };
    let held = panel.superseded_grounded_records == 40
        && panel.superseded_reclaim_candidates == 60
        && report.superseded_grounded_records_total == 40
        && report.superseded_reclaim_candidates == 60;
    println!(
        "  S3 partially grounded (100 superseded, 40 grounded, closed, covered)\n     \
         grounded={} (expected 40) candidates={} (expected 60)  {}",
        panel.superseded_grounded_records,
        panel.superseded_reclaim_candidates,
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "S3: grounded={} candidates={}",
            panel.superseded_grounded_records, panel.superseded_reclaim_candidates
        ));
    }

    // --- S4: every superseded record grounded ----------------------------
    // The whole generation is sacred. 0 candidates, and the 50 are counted as
    // grounded rather than quietly dropped from both totals.
    let report = build_panel_coverage_report(
        &synthetic_census(1000, 900, 50, 50, 500),
        &rows,
        &source_keys(TIMELINE_CF, 1000),
    );
    let Some(panel) = timeline(&report) else {
        failures.push("syn-timeline-v1 absent from a synthetic report".to_owned());
        return failures;
    };
    let held = panel.superseded_grounded_records == 50
        && panel.superseded_reclaim_candidates == 0
        && panel.superseded_records == 50;
    println!(
        "  S4 fully grounded (50 superseded, all 50 grounded)\n     \
         grounded={} candidates={} (expected 0)  {}",
        panel.superseded_grounded_records,
        panel.superseded_reclaim_candidates,
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "S4: grounded={} candidates={}",
            panel.superseded_grounded_records, panel.superseded_reclaim_candidates
        ));
    }

    // --- S5: missing timestamps are UNKNOWN, and unknown is not closed ----
    // A generation whose created_at window the census could not read must not be
    // treated as safe. Expected: closed=false, 0 candidates, named as open.
    let mut census = synthetic_census(1000, 900, 100, 0, 500);
    census.entries[0].latest_created_at_ms = None;
    let report = build_panel_coverage_report(&census, &rows, &source_keys(TIMELINE_CF, 1000));
    let Some(panel) = timeline(&report) else {
        failures.push("syn-timeline-v1 absent from a synthetic report".to_owned());
        return failures;
    };
    let held =
        !panel.superseded_versions_present[0].closed && panel.superseded_reclaim_candidates == 0;
    println!(
        "  S5 unreadable timestamp (latest_created_at_ms = None)\n     \
         closed={} candidates={} (expected 0 — unknown is not closed)  {}",
        panel.superseded_versions_present[0].closed,
        panel.superseded_reclaim_candidates,
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "S5: closed={} candidates={}",
            panel.superseded_versions_present[0].closed, panel.superseded_reclaim_candidates
        ));
    }

    // --- S6: the #1940 defect, at the exact point it hid ------------------
    // 1000 active records, and the source CF holds 1000 rows — so the OLD
    // arithmetic `active_version_records - source_cf_rows` is exactly 0 and
    // reports a clean panel. But the source CF holds keys 0..998 while the
    // records declare keys 0..999, so record 999's own source row is gone.
    //
    // Expected, by construction: orphaned_records = 1, not 0. This is the case
    // no subtraction can see, because the two counts agree while the sets do
    // not. `syn-timeline-v1` declares source_ttl_managed = false, so the one
    // orphan must be classified MISSING (an integrity finding) and the panel
    // must be named in orphaned_source_missing_panels.
    let rows = BTreeMap::from([(TIMELINE_CF.to_owned(), 1000u64)]);
    let report = build_panel_coverage_report(
        &synthetic_census(1000, 900, 0, 0, 500),
        &rows,
        &source_keys(TIMELINE_CF, 999),
    );
    let Some(panel) = timeline(&report) else {
        failures.push("syn-timeline-v1 absent from a synthetic report".to_owned());
        return failures;
    };
    let subtraction_would_say =
        (panel.active_version_records as u64).saturating_sub(panel.source_cf_rows.unwrap_or(0));
    let held = panel.orphaned_records == 1
        && panel.orphaned_source_missing == 1
        && panel.orphaned_source_evicted == 0
        && panel.unattributed_records == 0
        && subtraction_would_say == 0
        && report.orphaned_source_missing_panels == vec!["syn-timeline-v1".to_owned()];
    println!(
        "  S6 probe vs subtraction (1000 records, 1000 source rows, 999 keys present)\n     \
         subtraction_would_say={subtraction_would_say} (expected 0 — the defect) \
         probe orphaned={} missing={} evicted={} unattributed={} named={:?}  {}",
        panel.orphaned_records,
        panel.orphaned_source_missing,
        panel.orphaned_source_evicted,
        panel.unattributed_records,
        report.orphaned_source_missing_panels,
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "S6: orphaned={} missing={} evicted={} subtraction={subtraction_would_say} named={:?}",
            panel.orphaned_records,
            panel.orphaned_source_missing,
            panel.orphaned_source_evicted,
            report.orphaned_source_missing_panels
        ));
    }

    // --- S7: a TTL-managed source is EVICTED, not MISSING -----------------
    // The same shape on `syn-action-v1`, which declares source_ttl_managed =
    // true over CF_ACTION_LOG. 40 active records, 37 source keys present, so 3
    // orphans — and all 3 must land in `orphaned_source_evicted`, with the
    // panel absent from orphaned_source_missing_panels. This is ask 2: the
    // expected class must stop being reported as a finding.
    let action_cf = "CF_ACTION_LOG";
    let mut census = synthetic_census(0, 900, 0, 0, 500);
    census.entries.push(SynapseCalyxPanelCensusEntry {
        panel_version: SYN_ACTION_PANEL_VERSION,
        records: 40,
        grounded_records: 0,
        anchor_kind_records: BTreeMap::new(),
        earliest_created_at_ms: Some(900),
        latest_created_at_ms: Some(901),
        source_key_hexes: BTreeMap::from([(
            action_cf.to_owned(),
            (0..40).map(source_key).collect(),
        )]),
        unattributed_records: 0,
    });
    census.base_cf_rows += 40;
    let rows = BTreeMap::from([(action_cf.to_owned(), 40u64)]);
    let report = build_panel_coverage_report(&census, &rows, &source_keys(action_cf, 37));
    let action = report
        .panels
        .iter()
        .find(|panel| panel.panel_version == SYN_ACTION_PANEL_VERSION)
        .cloned();
    let Some(action) = action else {
        failures.push("syn-action-v1 absent from a synthetic report".to_owned());
        return failures;
    };
    let held = action.orphaned_records == 3
        && action.orphaned_source_evicted == 3
        && action.orphaned_source_missing == 0
        && report.orphaned_source_missing_panels.is_empty()
        && report.orphaned_source_evicted_total == 3;
    println!(
        "  S7 TTL-managed source (40 records, 37 keys present, CF_ACTION_LOG)\n     \
         orphaned={} evicted={} (expected 3) missing={} (expected 0) missing_panels={:?} \
         (expected [])  {}",
        action.orphaned_records,
        action.orphaned_source_evicted,
        action.orphaned_source_missing,
        report.orphaned_source_missing_panels,
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "S7: orphaned={} evicted={} missing={} missing_panels={:?}",
            action.orphaned_records,
            action.orphaned_source_evicted,
            action.orphaned_source_missing,
            report.orphaned_source_missing_panels
        ));
    }

    // --- S8: no provenance is UNATTRIBUTED, never "covered" ---------------
    // 10 active records that declare no source at all. The probe cannot be
    // attempted, so they must be neither orphaned nor silently counted as
    // covered: unattributed = 10, orphaned = 0.
    let mut census = synthetic_census(10, 900, 0, 0, 500);
    let Some(active) = census
        .entries
        .iter_mut()
        .find(|entry| entry.panel_version == SYN_TIMELINE_PANEL_VERSION)
    else {
        failures.push("S8: synthetic census lacks the active timeline generation".to_owned());
        return failures;
    };
    active.source_key_hexes.clear();
    active.unattributed_records = 10;
    let rows = BTreeMap::from([(TIMELINE_CF.to_owned(), 10u64)]);
    let report = build_panel_coverage_report(&census, &rows, &source_keys(TIMELINE_CF, 10));
    let Some(panel) = timeline(&report) else {
        failures.push("syn-timeline-v1 absent from a synthetic report".to_owned());
        return failures;
    };
    let held = panel.unattributed_records == 10
        && panel.orphaned_records == 0
        && report.unattributed_records_total == 10;
    println!(
        "  S8 no provenance (10 records, no source_cf/source_key metadata)\n     \
         unattributed={} (expected 10) orphaned={} (expected 0 — cannot ask is not answered no)  {}",
        panel.unattributed_records,
        panel.orphaned_records,
        verdict(held)
    );
    if !held {
        failures.push(format!(
            "S8: unattributed={} orphaned={}",
            panel.unattributed_records, panel.orphaned_records
        ));
    }

    failures
}

#[allow(clippy::too_many_lines, reason = "one linear verification narrative")]
fn main() -> Result<(), Box<dyn Error>> {
    let parent = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: panel_retention_fsv <dir-containing-db-daemon>")?;
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("no db-daemon directory under {}", parent.display()).into());
    }

    println!("panel_retention_fsv  (#1927 ask 3)");
    println!("  vault_dir = {}", vault_dir.display());
    println!();

    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let mut failures: Vec<String> = Vec::new();

    // ------------------------------------------------------------------
    // BEFORE — the source of truth, read independently of the census
    // ------------------------------------------------------------------
    println!("== BEFORE: source CF row counts, read independently ==");
    let cf_counts = db.cf_row_counts()?;
    for entry in builtin_panel_catalog() {
        if let PanelSource::FullCf(cf) = entry.source {
            println!(
                "  {cf:<26} rows={}",
                cf_counts.get(cf).copied().unwrap_or(0)
            );
        }
    }
    println!();

    // ------------------------------------------------------------------
    // EXECUTE
    // ------------------------------------------------------------------
    let started = std::time::Instant::now();
    let report = db.measure_panel_coverage()?;
    println!("== AFTER: census ({} ms) ==", started.elapsed().as_millis());
    println!(
        "  base_cf_rows                     = {}",
        report.base_cf_rows
    );
    println!(
        "  records_total                    = {}",
        report.records_total
    );
    println!(
        "  accounting_holds                 = {}",
        report.accounting_holds()
    );
    println!("  ---- #1927 ask 3 ----");
    println!(
        "  superseded_records_total         = {}",
        report.superseded_records_total
    );
    println!(
        "  superseded_grounded_records_total= {}   (SACRED: anchors are not regenerable)",
        report.superseded_grounded_records_total
    );
    println!(
        "  superseded_reclaim_candidates    = {}   (UPPER BOUND, not a delete list)",
        report.superseded_reclaim_candidates
    );
    println!(
        "  orphaned_records_total           = {}   (SACRED: source row TTL'd away)",
        report.orphaned_records_total
    );
    println!(
        "  open_superseded_generations      = {:?}   (non-empty is a DEFECT)",
        report.open_superseded_generations
    );
    println!();

    if !report.accounting_holds() {
        failures.push("physical accounting does not hold".to_owned());
    }

    // ------------------------------------------------------------------
    // PER-GENERATION detail
    // ------------------------------------------------------------------
    println!("== PER SUPERSEDED GENERATION ==");
    println!(
        "  {:<28} {:>9} {:>9} {:>9} {:>7}  created_at window (ms)",
        "panel@superseded", "records", "grounded", "cands", "closed"
    );
    for panel in &report.panels {
        for generation in &panel.superseded_versions_present {
            println!(
                "  {:<28} {:>9} {:>9} {:>9} {:>7}  {:?}..{:?}",
                format!("{}@{}", panel.panel_name, generation.panel_version),
                generation.records,
                generation.grounded_records,
                if generation.closed {
                    generation
                        .records
                        .saturating_sub(generation.grounded_records)
                } else {
                    0
                },
                generation.closed,
                generation.earliest_created_at_ms,
                generation.latest_created_at_ms,
            );
        }
    }
    println!();

    // ------------------------------------------------------------------
    // CHECK 1 — the load-bearing cross-check, on SUPERSEDED versions
    // ------------------------------------------------------------------
    println!("== CHECK 1: superseded grounded counts agree with the hydrating path ==");
    println!("  grounding_gap_intelligence hydrates each record from its per-slot CFs and");
    println!("  counts anchors there. Same two numbers by different code, or this is worthless.");
    let mut compared = 0usize;
    for panel in &report.panels {
        for generation in &panel.superseded_versions_present {
            if generation.records == 0 {
                continue;
            }
            if generation.records > HEAVY_PATH_RECORD_CLAMP {
                println!(
                    "  {:<28} SKIPPED — {} records exceeds the heavy path's {HEAVY_PATH_RECORD_CLAMP} clamp",
                    format!("{}@{}", panel.panel_name, generation.panel_version),
                    generation.records
                );
                continue;
            }
            let heavy =
                db.grounding_gap_intelligence(generation.panel_version, HEAVY_PATH_RECORD_CLAMP)?;
            let records_held = heavy.records_scanned == generation.records;
            let grounded_held = heavy.grounded_records == generation.grounded_records;
            compared += 1;
            if !records_held {
                failures.push(format!(
                    "{}@{}: census records={} but grounding_gap says {}",
                    panel.panel_name,
                    generation.panel_version,
                    generation.records,
                    heavy.records_scanned
                ));
            }
            if !grounded_held {
                failures.push(format!(
                    "{}@{}: census grounded={} but grounding_gap says {}",
                    panel.panel_name,
                    generation.panel_version,
                    generation.grounded_records,
                    heavy.grounded_records
                ));
            }
            println!(
                "  {:<28} records {}=={} {}   grounded {}=={} {}",
                format!("{}@{}", panel.panel_name, generation.panel_version),
                generation.records,
                heavy.records_scanned,
                verdict(records_held),
                generation.grounded_records,
                heavy.grounded_records,
                verdict(grounded_held),
            );
        }
    }
    if compared == 0 {
        failures.push(
            "no superseded generation was small enough to cross-check; check 1 proved nothing"
                .to_owned(),
        );
    }
    println!("  generations cross-checked: {compared}");
    println!();

    // ------------------------------------------------------------------
    // CHECK 2 — the invariant that stops a reclaim eating an anchor
    // ------------------------------------------------------------------
    println!("== CHECK 2: candidates never include a grounded record ==");
    for panel in &report.panels {
        if panel.superseded_records == 0 {
            continue;
        }
        let ungrounded = panel
            .superseded_records
            .saturating_sub(panel.superseded_grounded_records);
        let grounded_le = panel.superseded_grounded_records <= panel.superseded_records;
        let candidates_le = panel.superseded_reclaim_candidates <= ungrounded;
        if !grounded_le {
            failures.push(format!(
                "{}: superseded_grounded={} exceeds superseded_records={}",
                panel.panel_name, panel.superseded_grounded_records, panel.superseded_records
            ));
        }
        if !candidates_le {
            failures.push(format!(
                "{}: candidates={} exceeds ungrounded={}, so a reclaim would destroy an anchor",
                panel.panel_name, panel.superseded_reclaim_candidates, ungrounded
            ));
        }
        println!(
            "  {:<28} grounded {}<={} {}   candidates {}<={} {}",
            panel.panel_name,
            panel.superseded_grounded_records,
            panel.superseded_records,
            verdict(grounded_le),
            panel.superseded_reclaim_candidates,
            ungrounded,
            verdict(candidates_le),
        );
    }
    println!();

    // ------------------------------------------------------------------
    // CHECK 3 — no candidates without a repair path and full coverage
    // ------------------------------------------------------------------
    println!("== CHECK 3: no candidates on an unbackfillable or under-covered panel ==");
    for panel in &report.panels {
        let disqualified = panel.backfill_source_cf.is_none() || panel.coverage_below_floor;
        if !disqualified {
            continue;
        }
        let held = panel.superseded_reclaim_candidates == 0;
        if !held {
            failures.push(format!(
                "{}: {} candidates despite backfill_source_cf={:?} coverage_below_floor={}",
                panel.panel_name,
                panel.superseded_reclaim_candidates,
                panel.backfill_source_cf,
                panel.coverage_below_floor
            ));
        }
        println!(
            "  {:<28} backfill={:<22} below_floor={:<5} candidates={} {}",
            panel.panel_name,
            panel
                .backfill_source_cf
                .clone()
                .unwrap_or_else(|| "<none>".to_owned()),
            panel.coverage_below_floor,
            panel.superseded_reclaim_candidates,
            verdict(held)
        );
    }
    println!();

    // ------------------------------------------------------------------
    // CHECK 4 — open generations are named and contribute nothing
    // ------------------------------------------------------------------
    println!("== CHECK 4: an OPEN superseded generation is named and yields no candidates ==");
    let mut open_seen = 0usize;
    for panel in &report.panels {
        for generation in &panel.superseded_versions_present {
            if generation.closed || generation.records == 0 {
                continue;
            }
            open_seen += 1;
            let label = format!("{}@{}", panel.panel_name, generation.panel_version);
            let named = report.open_superseded_generations.contains(&label);
            if !named {
                failures.push(format!(
                    "{label}: open but absent from open_superseded_generations"
                ));
            }
            println!("  {label:<28} open, named={named} {}", verdict(named));
        }
    }
    if open_seen == 0 {
        println!("  none open on this vault — every superseded generation is closed, which is");
        println!("  the healthy state, so the live corpus cannot exercise this path. It is");
        println!("  driven from synthetic inputs below instead of being asserted.");
    }
    println!();

    // ------------------------------------------------------------------
    // SYNTHETIC CASES — conditions the live vault is (correctly) not in
    // ------------------------------------------------------------------
    //
    // Each answer below is fixed BEFORE the code runs, by arithmetic on inputs
    // chosen here. `build_panel_coverage_report` is a pure function of a census
    // and a row-count map, so these drive the exact code the live path uses.
    println!("== SYNTHETIC CASES (answers fixed before the code ran) ==");
    failures.extend(run_synthetic_cases());
    println!();

    // ------------------------------------------------------------------
    // CHECK 5 — the orphan count is a probe, and it splits by declaration
    // ------------------------------------------------------------------
    //
    // This check used to assert `orphaned_records == active_records -
    // source_cf_rows`, which is the very arithmetic #1940 found to be wrong: it
    // subtracts two counts taken over different populations, so it can be
    // non-zero with no record orphaned and zero with records orphaned. It now
    // asserts the two properties that hold of a real probe:
    //
    //   * the classes partition the count exactly, and
    //   * every orphan lands in the class its panel's DECLARED
    //     `source_ttl_managed` dictates — evicted (expected) or missing
    //     (a finding).
    //
    // The subtraction is still printed, as the number the old code would have
    // reported, so the divergence is visible rather than asserted away.
    println!("== CHECK 5: orphaned_records is a per-record probe, split by declaration ==");
    for entry in builtin_panel_catalog() {
        let PanelSource::FullCf(cf) = entry.source else {
            continue;
        };
        let Some(panel) = report
            .panels
            .iter()
            .find(|row| row.panel_version == entry.panel_version)
        else {
            continue;
        };
        let independent = cf_counts.get(cf).copied().unwrap_or(0);
        let old_subtraction =
            (panel.active_version_records as u64).saturating_sub(independent) as usize;
        let partitions =
            panel.orphaned_source_evicted + panel.orphaned_source_missing == panel.orphaned_records;
        let classified = if entry.source_ttl_managed {
            panel.orphaned_source_missing == 0
        } else {
            panel.orphaned_source_evicted == 0
        };
        let named = (panel.orphaned_source_missing > 0)
            == report
                .orphaned_source_missing_panels
                .contains(&entry.panel_name.to_owned());
        let held = partitions && classified && named;
        if !held {
            failures.push(format!(
                "{}: orphaned={} evicted={} missing={} ttl_managed={} named_ok={named}",
                entry.panel_name,
                panel.orphaned_records,
                panel.orphaned_source_evicted,
                panel.orphaned_source_missing,
                entry.source_ttl_managed
            ));
        }
        if panel.orphaned_records > 0 || old_subtraction > 0 || !held {
            println!(
                "  {:<28} probe orphaned={} (evicted={} missing={}) ttl_managed={} \
                 | old subtraction {} records - {cf} rows={independent} would say {old_subtraction} {}",
                entry.panel_name,
                panel.orphaned_records,
                panel.orphaned_source_evicted,
                panel.orphaned_source_missing,
                entry.source_ttl_managed,
                panel.active_version_records,
                verdict(held)
            );
        }
    }
    for entry in builtin_panel_catalog() {
        let Some(panel) = report
            .panels
            .iter()
            .find(|row| row.panel_version == entry.panel_version)
        else {
            continue;
        };
        if panel.superseded_orphaned_records > 0 {
            println!(
                "  {:<28} SUPERSEDED probe orphaned={} (missing={}) ttl_managed={} \
                 — not reclaimable: nothing can re-measure a row that is gone",
                entry.panel_name,
                panel.superseded_orphaned_records,
                panel.superseded_orphaned_source_missing,
                entry.source_ttl_managed
            );
        }
    }
    println!(
        "  totals: active_orphaned={} superseded_orphaned={} evicted={} missing={} \
         unattributed={} missing_panels={:?}",
        report.orphaned_records_total,
        report.superseded_orphaned_records_total,
        report.orphaned_source_evicted_total,
        report.orphaned_source_missing_total,
        report.unattributed_records_total,
        report.orphaned_source_missing_panels
    );
    println!();

    // ------------------------------------------------------------------
    // CHECK 6 — per-panel totals sum to the report totals
    // ------------------------------------------------------------------
    println!("== CHECK 6: per-panel figures sum to the report totals ==");
    let sum_grounded: usize = report
        .panels
        .iter()
        .map(|panel| panel.superseded_grounded_records)
        .sum();
    let sum_candidates: usize = report
        .panels
        .iter()
        .map(|panel| panel.superseded_reclaim_candidates)
        .sum();
    let sum_orphans: usize = report
        .panels
        .iter()
        .map(|panel| panel.orphaned_records)
        .sum();
    let sum_superseded: usize = report
        .panels
        .iter()
        .map(|panel| panel.superseded_records)
        .sum();
    let unknown_records: usize = report
        .unknown_panel_versions
        .iter()
        .map(|(_, records)| *records)
        .sum();

    // superseded_records_total also absorbs unclaimed generations, so the panel
    // sum plus those must equal it exactly.
    let superseded_held = sum_superseded + unknown_records == report.superseded_records_total;
    let candidates_held = sum_candidates == report.superseded_reclaim_candidates;
    let orphans_held = sum_orphans == report.orphaned_records_total;
    // Grounded total also absorbs unclaimed generations, so the panel sum is a
    // lower bound rather than an equality.
    let grounded_held = sum_grounded <= report.superseded_grounded_records_total;
    for (name, held, got, want) in [
        (
            "superseded (+unclaimed)",
            superseded_held,
            sum_superseded + unknown_records,
            report.superseded_records_total,
        ),
        (
            "reclaim candidates",
            candidates_held,
            sum_candidates,
            report.superseded_reclaim_candidates,
        ),
        (
            "orphans",
            orphans_held,
            sum_orphans,
            report.orphaned_records_total,
        ),
        (
            "grounded (panel sum <= total)",
            grounded_held,
            sum_grounded,
            report.superseded_grounded_records_total,
        ),
    ] {
        if !held {
            failures.push(format!("total {name}: panel sum {got} vs report {want}"));
        }
        println!("  {name:<32} {got:>8} vs {want:<8} {}", verdict(held));
    }
    println!();

    // ------------------------------------------------------------------
    // VERDICT
    // ------------------------------------------------------------------
    if failures.is_empty() {
        println!("RESULT: every check held against the real Base CF.");
        Ok(())
    } else {
        println!("RESULT: {} FAILURE(S)", failures.len());
        for failure in &failures {
            println!("  - {failure}");
        }
        Err(format!("{} check(s) failed", failures.len()).into())
    }
}

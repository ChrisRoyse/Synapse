//! Full State Verification for the coverage-deficiency and backfill-selection
//! logic (issue #1927 asks 2 and 4).
//!
//! # Why this run is synthetic, and why that is the right choice here
//!
//! `panel_coverage_census_fsv` proves the census reads the physical vault
//! correctly. It cannot prove what happens when a panel is *deficient*, because
//! the live vault is currently at 1.0 coverage on every full-CF panel — the
//! #1927 condition (an active generation holding 389 of 22,999 rows) only exists
//! in the window between a panel version bump and a completed backfill.
//!
//! Waiting for that window is not verification, it is luck. So this run
//! constructs the exact censuses whose correct answers are known in advance —
//! 389/22,999 is #1927's own measurement — and checks the report against them.
//! The inputs are chosen here, so the expected output is known before the code
//! runs, and a wrong answer cannot be rationalised after the fact.
//!
//! It then drives one **real** backfill page against a real vault, so the
//! primitive the maintainer's loop calls is exercised against physical rows
//! rather than assumed to work.
//!
//! # Cases
//!
//! | # | Input | Expected |
//! |---|---|---|
//! | 1 | 389 records / 22,999 CF rows, backfillable | deficient, owed, selected, 22,610 uncovered |
//! | 2 | two deficient panels, one with no re-measure path | the repairable one is selected; the other is named unbackfillable |
//! | 3 | exactly at the 0.95 floor, and one ULP below | at-floor is clean; below-floor is deficient |
//! | 4 | a panel version no catalog entry claims | reported, and counted as stranded |
//! | 5 | Base rows that will not decode | accounting fails closed |

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::{SynapseCalyxPanelCensus, SynapseCalyxPanelCensusEntry};
use synapse_storage::Db;
use synapse_storage::cf;
use synapse_storage::constellations::{
    SYN_AGENT_TRANSCRIPT_PANEL_VERSION, SYN_EPISODE_PANEL_VERSION, SYN_TIMELINE_PANEL_VERSION,
};
use synapse_storage::panel_coverage::{SYN_PANEL_COVERAGE_FLOOR, build_panel_coverage_report};

const SCHEMA_VERSION: u32 = 1;

/// #1927's own measurement: the active transcript generation held this many of
/// its source CF's rows.
const ISSUE_1927_ACTIVE_RECORDS: usize = 389;
const ISSUE_1927_SOURCE_ROWS: u64 = 22_999;

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "FAIL" }
}

fn entry(panel_version: u32, records: usize, grounded: usize) -> SynapseCalyxPanelCensusEntry {
    SynapseCalyxPanelCensusEntry {
        panel_version,
        records,
        grounded_records: grounded,
        anchor_kind_records: BTreeMap::new(),
        earliest_created_at_ms: None,
        latest_created_at_ms: None,
        // These cases are about coverage and backfill selection, not the #1940
        // orphan probe: no record declares a source key, so nothing is probed
        // and every orphan count is a measured zero rather than an accident.
        source_key_hexes: BTreeMap::new(),
        unattributed_records: 0,
    }
}

fn census(
    entries: Vec<SynapseCalyxPanelCensusEntry>,
    decode_failures: usize,
) -> SynapseCalyxPanelCensus {
    let records_total: usize = entries.iter().map(|row| row.records).sum();
    SynapseCalyxPanelCensus {
        entries,
        base_cf_rows: records_total + decode_failures,
        decode_failures,
        first_decode_failure: (decode_failures > 0)
            .then(|| "key_hex=deadbeef error=synthetic truncated Base row".to_owned()),
        measured_at_unix_ms: Some(0),
        // Hand-assembled to exercise the selection logic downstream of the
        // fold; no `Base` walk ran, and this says so rather than inventing one.
        walk: synapse_calyx::SynapseCalyxCfWalk::not_walked("base"),
    }
}

fn check(label: &str, held: bool, detail: String, failures: &mut Vec<String>) {
    println!("  {:<58} {}", label, verdict(held));
    if !held {
        failures.push(format!("{label}: {detail}"));
    }
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    println!("panel_backfill_selection_fsv");
    println!("  floor = {SYN_PANEL_COVERAGE_FLOOR}");
    println!();
    let mut failures: Vec<String> = Vec::new();

    // -----------------------------------------------------------------------
    // CASE 1 — #1927's exact measured condition.
    // -----------------------------------------------------------------------
    println!("== CASE 1: the #1927 condition, 389 records / 22,999 source rows ==");
    println!("  input chosen HERE, so the answer is known before the code runs:");
    println!("    coverage = 389/22999 = 0.016914...  -> below the 0.95 floor");
    println!("    uncovered = 22999 - 389 = 22610");
    let mut rows = BTreeMap::new();
    rows.insert(cf::CF_AGENT_TRANSCRIPTS.to_owned(), ISSUE_1927_SOURCE_ROWS);
    let report = build_panel_coverage_report(
        &census(
            vec![entry(
                SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
                ISSUE_1927_ACTIVE_RECORDS,
                0,
            )],
            0,
        ),
        &rows,
        &BTreeMap::new(),
    );
    let transcript = report
        .panels
        .iter()
        .find(|panel| panel.panel_version == SYN_AGENT_TRANSCRIPT_PANEL_VERSION)
        .ok_or("transcript panel absent from the report")?;
    let expected_fraction = ISSUE_1927_ACTIVE_RECORDS as f32 / ISSUE_1927_SOURCE_ROWS as f32;
    println!(
        "  measured: coverage={:?} uncovered={:?} below_floor={} owed={}",
        transcript.coverage_fraction,
        transcript.uncovered_rows(),
        transcript.coverage_below_floor,
        transcript.backfill_owed()
    );
    check(
        "coverage fraction is 389/22999",
        transcript
            .coverage_fraction
            .is_some_and(|fraction| (fraction - expected_fraction).abs() < f32::EPSILON),
        format!(
            "got {:?}, expected {expected_fraction}",
            transcript.coverage_fraction
        ),
        &mut failures,
    );
    check(
        "uncovered_rows is exactly 22610",
        transcript.uncovered_rows() == Some(22_610),
        format!("got {:?}", transcript.uncovered_rows()),
        &mut failures,
    );
    check(
        "flagged coverage_below_floor",
        transcript.coverage_below_floor,
        "a panel at 1.7% must be deficient".to_owned(),
        &mut failures,
    );
    check(
        "backfill_owed (deficient AND repairable)",
        transcript.backfill_owed(),
        "the transcript panel declares CF_AGENT_TRANSCRIPTS as its re-measure path".to_owned(),
        &mut failures,
    );
    check(
        "selected as most_owed_backfill",
        report
            .most_owed_backfill()
            .is_some_and(|target| target.panel_version == SYN_AGENT_TRANSCRIPT_PANEL_VERSION),
        format!(
            "selected {:?}",
            report.most_owed_backfill().map(|t| &t.panel_name)
        ),
        &mut failures,
    );
    check(
        "named in coverage_deficient_panels",
        report
            .coverage_deficient_panels
            .contains(&transcript.panel_name),
        format!("{:?}", report.coverage_deficient_panels),
        &mut failures,
    );
    println!();

    // -----------------------------------------------------------------------
    // CASE 2 — an unrepairable deficiency must not be reported as clean.
    // -----------------------------------------------------------------------
    println!("== CASE 2: two deficient panels, one with NO re-measure path ==");
    println!("  agent-event declares backfill_source_cf = None, so it CANNOT be repaired.");
    println!("  It must still be named, and the repairable panel must be the one selected.");
    let mut rows = BTreeMap::new();
    rows.insert(cf::CF_AGENT_TRANSCRIPTS.to_owned(), 1_000);
    rows.insert(cf::CF_AGENT_EVENTS.to_owned(), 9_000);
    let report = build_panel_coverage_report(
        &census(
            vec![
                entry(SYN_AGENT_TRANSCRIPT_PANEL_VERSION, 100, 0),
                entry(
                    synapse_storage::constellations::SYN_AGENT_EVENT_PANEL_VERSION,
                    10,
                    0,
                ),
            ],
            0,
        ),
        &rows,
        &BTreeMap::new(),
    );
    println!(
        "  measured: deficient={:?} unbackfillable={:?} selected={:?}",
        report.coverage_deficient_panels,
        report.unbackfillable_deficient_panels,
        report.most_owed_backfill().map(|target| &target.panel_name),
    );
    check(
        "both panels named deficient",
        report.coverage_deficient_panels.len() == 2,
        format!("{:?}", report.coverage_deficient_panels),
        &mut failures,
    );
    check(
        "the unrepairable one is named unbackfillable",
        report.unbackfillable_deficient_panels == vec!["syn-agent-event-v1".to_owned()],
        format!("{:?}", report.unbackfillable_deficient_panels),
        &mut failures,
    );
    check(
        "the REPAIRABLE panel is selected, though it has fewer uncovered rows (900 vs 8990)",
        report
            .most_owed_backfill()
            .is_some_and(|target| target.panel_version == SYN_AGENT_TRANSCRIPT_PANEL_VERSION),
        format!(
            "selected {:?}",
            report.most_owed_backfill().map(|t| &t.panel_name)
        ),
        &mut failures,
    );
    println!();

    // -----------------------------------------------------------------------
    // CASE 3 — the floor boundary itself.
    // -----------------------------------------------------------------------
    println!("== CASE 3: the 0.95 floor boundary ==");
    println!("  950/1000 = 0.9500 is AT the floor -> clean (the comparison is strict `<`)");
    println!("  949/1000 = 0.9490 is BELOW        -> deficient");
    for (records, expect_deficient) in [(950usize, false), (949usize, true)] {
        let mut rows = BTreeMap::new();
        rows.insert(cf::CF_TIMELINE.to_owned(), 1_000_u64);
        let report = build_panel_coverage_report(
            &census(vec![entry(SYN_TIMELINE_PANEL_VERSION, records, 0)], 0),
            &rows,
            &BTreeMap::new(),
        );
        let panel = report
            .panels
            .iter()
            .find(|panel| panel.panel_version == SYN_TIMELINE_PANEL_VERSION)
            .ok_or("timeline panel absent")?;
        check(
            &format!(
                "{records}/1000 = {:.4} -> deficient={expect_deficient}",
                panel.coverage_fraction.unwrap_or(0.0)
            ),
            panel.coverage_below_floor == expect_deficient,
            format!("got below_floor={}", panel.coverage_below_floor),
            &mut failures,
        );
    }
    println!();

    // -----------------------------------------------------------------------
    // CASE 4 — a generation no catalog entry claims.
    // -----------------------------------------------------------------------
    println!("== CASE 4: an unclaimed panel generation must be reported, not dropped ==");
    println!("  input: 700 records at version 1234567, which is in no catalog entry.");
    println!("  expected: named in unknown_panel_versions AND counted as stranded.");
    let mut rows = BTreeMap::new();
    rows.insert(cf::CF_EPISODES.to_owned(), 100_u64);
    let report = build_panel_coverage_report(
        &census(
            vec![
                entry(SYN_EPISODE_PANEL_VERSION, 100, 100),
                entry(1_234_567, 700, 0),
            ],
            0,
        ),
        &rows,
        &BTreeMap::new(),
    );
    println!(
        "  measured: unknown={:?} superseded_total={}",
        report.unknown_panel_versions, report.superseded_records_total
    );
    check(
        "the unclaimed generation is named with its record count",
        report.unknown_panel_versions == vec![(1_234_567_u32, 700_usize)],
        format!("{:?}", report.unknown_panel_versions),
        &mut failures,
    );
    check(
        "its 700 records count as stranded",
        report.superseded_records_total == 700,
        format!("got {}", report.superseded_records_total),
        &mut failures,
    );
    check(
        "records_total still accounts for every row (100 + 700)",
        report.records_total == 800 && report.accounting_holds(),
        format!(
            "records_total={} base_cf_rows={} holds={}",
            report.records_total,
            report.base_cf_rows,
            report.accounting_holds()
        ),
        &mut failures,
    );
    println!();

    // -----------------------------------------------------------------------
    // CASE 5 — decode failures must break the accounting invariant loudly.
    // -----------------------------------------------------------------------
    println!("== CASE 5: undecodable Base rows fail the accounting invariant ==");
    println!("  input: 100 decoded records + 3 rows that will not decode.");
    println!("  expected: base_cf_rows=103, records_total=100, accounting_holds=TRUE");
    println!("            (the invariant is rows == records + failures, and 103 == 100 + 3)");
    let mut rows = BTreeMap::new();
    rows.insert(cf::CF_EPISODES.to_owned(), 100_u64);
    let report = build_panel_coverage_report(
        &census(vec![entry(SYN_EPISODE_PANEL_VERSION, 100, 100)], 3),
        &rows,
        &BTreeMap::new(),
    );
    check(
        "decode failures are counted, not dropped",
        report.decode_failures == 3 && report.first_decode_failure.is_some(),
        format!(
            "decode_failures={} detail={:?}",
            report.decode_failures, report.first_decode_failure
        ),
        &mut failures,
    );
    check(
        "the invariant holds when failures are accounted (103 == 100 + 3)",
        report.accounting_holds(),
        format!(
            "base_cf_rows={} records_total={} decode_failures={}",
            report.base_cf_rows, report.records_total, report.decode_failures
        ),
        &mut failures,
    );
    // And the negative: a census whose rows do NOT add up must be caught.
    let mut broken = census(vec![entry(SYN_EPISODE_PANEL_VERSION, 100, 100)], 0);
    broken.base_cf_rows = 137; // 37 rows vanished with nothing recording it
    let broken_report = build_panel_coverage_report(&broken, &rows, &BTreeMap::new());
    check(
        "a census that silently lost 37 rows is caught (137 != 100 + 0)",
        !broken_report.accounting_holds(),
        format!(
            "base_cf_rows={} records_total={} decode_failures={} holds={}",
            broken_report.base_cf_rows,
            broken_report.records_total,
            broken_report.decode_failures,
            broken_report.accounting_holds()
        ),
        &mut failures,
    );
    println!();

    // -----------------------------------------------------------------------
    // CASE 6 — the real backfill page primitive, against physical rows.
    // -----------------------------------------------------------------------
    if let Some(parent) = std::env::args().nth(1).map(PathBuf::from) {
        let vault_dir = parent.join("db-daemon");
        println!(
            "== CASE 6: one REAL backfill page against {} ==",
            vault_dir.display()
        );
        println!("  This is the exact primitive the maintainer's loop calls. The synthetic cases");
        println!("  above prove which panel it picks; this proves the page it then drives works");
        println!("  and reports honest counters against physical rows.");
        let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
        let page = db.backfill_temporal_metadata(cf::CF_TIMELINE, None, None, 250)?;
        println!(
            "  examined={} inserted={} backfilled={} already_current={} more={} resume={}",
            page.examined_rows,
            page.inserted_rows,
            page.backfilled_rows,
            page.already_current_rows,
            page.more,
            page.resume_after_physical.is_some(),
        );
        check(
            "the page examined rows",
            page.examined_rows > 0,
            format!("examined={}", page.examined_rows),
            &mut failures,
        );
        check(
            "every examined row is accounted as inserted, backfilled, or already-current",
            page.inserted_rows + page.backfilled_rows + page.already_current_rows
                == page.examined_rows,
            format!(
                "{} + {} + {} != {}",
                page.inserted_rows,
                page.backfilled_rows,
                page.already_current_rows,
                page.examined_rows
            ),
            &mut failures,
        );
        check(
            "more=true carries a resume cursor (without one the driver cannot advance)",
            !page.more || page.resume_after_physical.is_some(),
            "more=true with no resume_after_physical would loop the sweep forever".to_owned(),
            &mut failures,
        );
        println!();
    } else {
        println!("== CASE 6: SKIPPED — pass a vault parent dir to drive a real backfill page ==");
        println!();
    }

    if failures.is_empty() {
        println!("panel_backfill_selection_fsv: PASS — every expected answer was produced.");
        Ok(())
    } else {
        println!(
            "panel_backfill_selection_fsv: FAIL — {} check(s):",
            failures.len()
        );
        for failure in &failures {
            println!("  - {failure}");
        }
        Err(format!("{} FSV check(s) failed", failures.len()).into())
    }
}

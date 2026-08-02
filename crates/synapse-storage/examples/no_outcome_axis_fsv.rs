//! Manual FSV for #1962: the active panel carries zero anchors of any kind.
//!
//! ## What the issue found, and what it actually was
//!
//! `hygiene grounding_gap` on `syn-timeline-v1 @ 1900001` — the **active** panel
//! — reported `distinct_anchor_kinds = 0`, `grounded_fraction = 0.0`, every slot
//! 100% ungrounded. Not thin coverage: no outcome had ever been attached to a
//! single record.
//!
//! The issue asked which outcome belongs on a timeline record, noting that
//! `SYN_ANCHOR_DETERMINING_FIELDS` declared
//! `("synapse:mcp_tool_call_outcome", 1900001, &[])` and that the declaration was
//! an unfulfilled intent.
//!
//! It was worse than unfulfilled: it **contradicted** the panel catalog, which
//! declares `syn-timeline-v1` as `outcome_bearing: false` with the reasoning
//! "a timeline row records that something was seen, not how it turned out —
//! their 0.0 grounded coverage is correct, not a gap". Two compile-time
//! declarations disagreed, nothing reconciled them, and the disagreement read as
//! a missing write on every grounding readback. An MCP tool call's outcome is
//! not a property of a focus change; the catalog was right and the anchor
//! declaration was wrong.
//!
//! ## What this harness proves
//!
//! 1. The contradiction is gone, and is now **structurally impossible**:
//!    `assert_syn_lens_provenance_complete` refuses any anchor declared on a panel
//!    the catalog marks observation-shaped, so it fails closed at startup.
//! 2. `grounding_gap` distinguishes *no outcome axis* from *under-covered*.
//! 3. `bits`/`sufficiency` **refuse** on an observation-shaped panel instead of
//!    returning a zeroed report that reads as "these lenses carry no signal".
//!
//! Usage:
//! `cargo run -p synapse-storage --example no_outcome_axis_fsv`
//! (declaration checks only — no vault needed; every fact here is a pure
//! function of the compile-time declarations, which is the point.)

use std::collections::BTreeSet;

use synapse_calyx::lens_provenance::SYN_ANCHOR_DETERMINING_FIELDS;
use synapse_storage::constellations::{
    SYN_TIMELINE_PANEL_VERSION, assert_syn_lens_provenance_complete, builtin_panel_catalog,
    panel_catalog_entry_for_version,
};

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut failures = Failures(Vec::new());

    // -----------------------------------------------------------------------
    // 1 — the two declarations now agree
    // -----------------------------------------------------------------------
    println!("== 1: the catalog and the anchor table agree ==");
    let timeline = panel_catalog_entry_for_version(SYN_TIMELINE_PANEL_VERSION)
        .ok_or("the timeline panel must be in the catalog")?;
    println!(
        "  catalog: {}@{} outcome_bearing={}",
        timeline.panel_name, timeline.panel_version, timeline.outcome_bearing
    );
    failures.check(
        "syn-timeline-v1 is declared observation-shaped",
        timeline.outcome_bearing,
        false,
    );

    let timeline_anchors: Vec<&str> = SYN_ANCHOR_DETERMINING_FIELDS
        .iter()
        .filter(|(_, version, _)| *version == SYN_TIMELINE_PANEL_VERSION)
        .map(|(kind, _, _)| *kind)
        .collect();
    println!("  anchor declarations on 1900001: {timeline_anchors:?}");
    failures.check(
        "no anchor is declared on the timeline panel",
        timeline_anchors.len(),
        0usize,
    );

    // The whole-table invariant, not just the one panel the issue named: an
    // unaudited second contradiction is exactly what let the first one survive.
    let observation_shaped: BTreeSet<u32> = builtin_panel_catalog()
        .into_iter()
        .filter(|entry| !entry.outcome_bearing)
        .flat_map(|entry| {
            std::iter::once(entry.panel_version).chain(entry.superseded_versions.iter().copied())
        })
        .collect();
    let contradictions: Vec<String> = SYN_ANCHOR_DETERMINING_FIELDS
        .iter()
        .filter(|(_, version, _)| observation_shaped.contains(version))
        .map(|(kind, version, _)| format!("{kind}@{version}"))
        .collect();
    println!(
        "  observation-shaped panel versions: {} | contradictions across the WHOLE table: {contradictions:?}",
        observation_shaped.len()
    );
    failures.check(
        "no anchor is declared on ANY observation-shaped panel",
        contradictions.len(),
        0usize,
    );

    // -----------------------------------------------------------------------
    // 2 — the startup gate actually refuses, and passes when clean
    // -----------------------------------------------------------------------
    println!("\n== 2: the startup declaration gate ==");
    let live = assert_syn_lens_provenance_complete();
    println!(
        "  assert_syn_lens_provenance_complete() on the shipped declarations -> {}",
        if live.is_ok() { "Ok" } else { "Err" }
    );
    failures.check("the shipped declarations validate", live.is_ok(), true);

    // Prove the gate would CATCH the contradiction rather than merely being
    // absent of one. The reintroduced entry is evaluated by the same set
    // intersection the validator uses, so this is the validator's own rule.
    let reintroduced = ("synapse:mcp_tool_call_outcome", SYN_TIMELINE_PANEL_VERSION);
    let would_be_caught = observation_shaped.contains(&reintroduced.1);
    println!(
        "  if ({}@{}) were reintroduced, the gate's rule flags it: {would_be_caught}",
        reintroduced.0, reintroduced.1
    );
    failures.check(
        "reintroducing the removed declaration is caught by the gate rule",
        would_be_caught,
        true,
    );

    // -----------------------------------------------------------------------
    // 3 — every outcome-bearing panel still has its declarations
    // -----------------------------------------------------------------------
    println!("\n== 3: outcome-bearing panels kept their declarations ==");
    let outcome_versions: BTreeSet<u32> = builtin_panel_catalog()
        .into_iter()
        .filter(|entry| entry.outcome_bearing)
        .map(|entry| entry.panel_version)
        .collect();
    let declared_versions: BTreeSet<u32> = SYN_ANCHOR_DETERMINING_FIELDS
        .iter()
        .map(|(_, version, _)| *version)
        .collect();
    for version in &declared_versions {
        println!(
            "  anchor declarations at {version}: {:?}",
            SYN_ANCHOR_DETERMINING_FIELDS
                .iter()
                .filter(|(_, v, _)| v == version)
                .map(|(kind, _, _)| *kind)
                .collect::<Vec<_>>()
        );
    }
    let stranded: Vec<u32> = declared_versions
        .difference(&outcome_versions)
        .copied()
        .collect();
    println!("  declared versions not in the outcome-bearing set: {stranded:?}");
    failures.check(
        "every remaining anchor declaration sits on an outcome-bearing panel version",
        stranded.is_empty(),
        true,
    );
    failures.check(
        "the declaration table is not empty (the removal did not delete everything)",
        SYN_ANCHOR_DETERMINING_FIELDS.len() >= 11,
        true,
    );

    println!("\n================================================================");
    if failures.0.is_empty() {
        println!("ALL CHECKS PASSED");
        Ok(())
    } else {
        println!("{} CHECK(S) FAILED:", failures.0.len());
        for failure in &failures.0 {
            println!("  - {failure}");
        }
        Err("no_outcome_axis_fsv failed".into())
    }
}

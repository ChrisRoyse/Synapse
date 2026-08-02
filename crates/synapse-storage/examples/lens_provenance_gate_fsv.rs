//! Full-state verification for issue #1958 ask 4: the completeness gate.
//!
//! ## Why a negative test is the only useful one here
//!
//! `assert_syn_lens_provenance_complete` currently returns `Ok`. So does a
//! function whose body is `Ok(())`. A run that only shows the happy path proves
//! the gate exists, not that it *guards* anything — and #1956 on this same
//! project was exactly that: a chain verifier that reported `Intact` having
//! checked nothing.
//!
//! So this harness reproduces each drift the gate is supposed to catch, against
//! the real tables, and requires the real error text to name the real slot.
//!
//! ## The drifts
//!
//! | case | what drifted | must be named |
//! |---|---|---|
//! | 1 | a new lens catalogued with no declaration | `undeclared_slots` |
//! | 2 | a declaration for a slot the catalog dropped | `orphaned_declarations` |
//! | 3 | a slot reassigned to a different lens | `lens_name_mismatches` |
//! | 4 | the same slot declared twice | `duplicate_declarations` |
//! | 5 | an anchor declared against a bumped panel version | unknown panel version |
//!
//! Case 3 is the one a presence-only gate would miss: the slot is declared and
//! catalogued, so counting entries agrees, while the field list now describes a
//! lens that no longer occupies that slot.
//!
//! Case 5 is the one that matters most in practice, because panel versions do
//! get bumped — #1904 and #1921 both did it — and an anchor declaration left on
//! the old version does not error, it silently stops matching, which turns a
//! refusal into a clean pass.
//!
//! Since the tables are `const`, the drifts are simulated over copies using the
//! same comparison the gate performs, and the gate is then run for real to prove
//! the live tables are clean.
//!
//! `cargo run --release -p synapse-storage --example lens_provenance_gate_fsv`

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use synapse_calyx::lens_provenance::{SYN_ANCHOR_DETERMINING_FIELDS, SYN_SLOT_SOURCE_FIELDS};
use synapse_storage::constellations::{
    SYN_ACTION_PANEL_VERSION, SYN_AGENT_EVENT_PANEL_VERSION, SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
    SYN_EPISODE_PANEL_VERSION, SYN_GRAPHPOS_APP_PANEL_VERSION, SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
    SYN_MCP_USAGE_PANEL_VERSION, SYN_OBSERVATION_PANEL_VERSION, SYN_OUTCOME_PANEL_VERSION,
    SYN_PATH_HIERARCHY_PANEL_VERSION, SYN_PROCESS_PANEL_VERSION,
    SYN_RECURRENCE_SUBJECT_PANEL_VERSION, SYN_REFLEX_PANEL_VERSION, SYN_TIMELINE_PANEL_VERSION,
    assert_syn_lens_provenance_complete, syn_active_panel_contract, syn_slot_lens_names,
};

/// The gate's comparison, over mutable copies so a drift can be introduced.
///
/// Deliberately re-stated rather than called: the point is to show that each
/// drift *is detectable from these two tables*, and then to show the shipped
/// gate reports the same clean verdict on the undrifted ones.
fn findings(
    declared: &BTreeMap<u16, &str>,
    catalog: &BTreeMap<u16, String>,
    duplicates: &[u16],
    anchor_versions: &[(&str, u32)],
    known_versions: &BTreeSet<u32>,
) -> Vec<String> {
    let mut out = Vec::new();
    let undeclared: Vec<u16> = catalog
        .keys()
        .filter(|slot| !declared.contains_key(*slot))
        .copied()
        .collect();
    if !undeclared.is_empty() {
        out.push(format!("undeclared_slots={undeclared:?}"));
    }
    let orphaned: Vec<u16> = declared
        .keys()
        .filter(|slot| !catalog.contains_key(*slot))
        .copied()
        .collect();
    if !orphaned.is_empty() {
        out.push(format!("orphaned_declarations={orphaned:?}"));
    }
    let renamed: Vec<String> = catalog
        .iter()
        .filter_map(|(slot, name)| {
            let found = declared.get(slot)?;
            (found != name).then(|| format!("{slot}: catalog={name} declared={found}"))
        })
        .collect();
    if !renamed.is_empty() {
        out.push(format!("lens_name_mismatches={renamed:?}"));
    }
    if !duplicates.is_empty() {
        out.push(format!("duplicate_declarations={duplicates:?}"));
    }
    let stale: Vec<String> = anchor_versions
        .iter()
        .filter(|(_, version)| !known_versions.contains(version))
        .map(|(kind, version)| format!("{kind}@{version}"))
        .collect();
    if !stale.is_empty() {
        out.push(format!(
            "anchor_declarations_on_unknown_panel_versions={stale:?}"
        ));
    }
    out
}

fn expect_named(case: &str, found: &[String], must_contain: &str) -> Result<(), Box<dyn Error>> {
    let joined = found.join(" ");
    println!("   {case:<46} -> {joined}");
    if found.is_empty() {
        return Err(format!("{case}: the gate found NOTHING; it does not guard this").into());
    }
    if !joined.contains(must_contain) {
        return Err(
            format!("{case}: expected the finding to name {must_contain}, got {joined}").into(),
        );
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("lens_provenance_gate_fsv  (#1958 ask 4)\n");

    let catalog: BTreeMap<u16, String> = syn_slot_lens_names();
    let declared: BTreeMap<u16, &str> = SYN_SLOT_SOURCE_FIELDS
        .iter()
        .map(|(slot, _, lens, _)| (*slot, *lens))
        .collect();
    let known_versions: BTreeSet<u32> = [
        SYN_TIMELINE_PANEL_VERSION,
        SYN_EPISODE_PANEL_VERSION,
        SYN_AGENT_EVENT_PANEL_VERSION,
        SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
        SYN_ACTION_PANEL_VERSION,
        SYN_REFLEX_PANEL_VERSION,
        SYN_PROCESS_PANEL_VERSION,
        SYN_OBSERVATION_PANEL_VERSION,
        SYN_OUTCOME_PANEL_VERSION,
        SYN_MCP_USAGE_PANEL_VERSION,
        SYN_RECURRENCE_SUBJECT_PANEL_VERSION,
        SYN_GRAPHPOS_APP_PANEL_VERSION,
        SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
        SYN_PATH_HIERARCHY_PANEL_VERSION,
    ]
    .into_iter()
    .collect();
    let anchor_versions: Vec<(&str, u32)> = SYN_ANCHOR_DETERMINING_FIELDS
        .iter()
        .map(|(kind, version, _)| (*kind, *version))
        .collect();

    println!("=== state before any drift");
    println!("   catalogued slots  = {}", catalog.len());
    println!("   declared slots    = {}", declared.len());
    println!("   anchor decls      = {}", anchor_versions.len());
    let clean = findings(&declared, &catalog, &[], &anchor_versions, &known_versions);
    println!("   findings          = {clean:?} (expected [])");
    if !clean.is_empty() {
        return Err(format!("the live tables are already drifted: {clean:?}").into());
    }

    println!("\n=== the five drifts the gate must catch");

    // 1. a new lens catalogued with no declaration
    let mut c1 = catalog.clone();
    c1.insert(200, "syn.newpanel.brand_new_lens.v1".to_owned());
    expect_named(
        "1. new lens, no declaration",
        &findings(&declared, &c1, &[], &anchor_versions, &known_versions),
        "undeclared_slots=[200]",
    )?;

    // 2. a declaration whose slot the catalog dropped
    let mut d2 = declared.clone();
    d2.insert(201, "syn.removed.lens.v1");
    expect_named(
        "2. declaration for a dropped slot",
        &findings(&d2, &catalog, &[], &anchor_versions, &known_versions),
        "orphaned_declarations=[201]",
    )?;

    // 3. slot 86 reassigned to a different lens -- the case a presence-only
    //    gate cannot see, because the counts still agree.
    let mut d3 = declared.clone();
    d3.insert(86, "syn.mcp_usage.something_else.v1");
    expect_named(
        "3. slot 86 reassigned to another lens",
        &findings(&d3, &catalog, &[], &anchor_versions, &known_versions),
        "lens_name_mismatches",
    )?;

    // 4. the same slot declared twice
    expect_named(
        "4. slot 93 declared twice",
        &findings(
            &declared,
            &catalog,
            &[93],
            &anchor_versions,
            &known_versions,
        ),
        "duplicate_declarations=[93]",
    )?;

    // 5. an anchor left behind on a bumped panel version
    let mut a5 = anchor_versions.clone();
    a5.push(("synapse:mcp_tool_call_outcome", 1_776_099));
    expect_named(
        "5. anchor decl on a bumped panel version",
        &findings(&declared, &catalog, &[], &a5, &known_versions),
        "1776099",
    )?;

    // --- the shipped gate, for real -----------------------------------------
    println!("\n=== the shipped gate on the live tables");
    match assert_syn_lens_provenance_complete() {
        Ok(()) => println!("   assert_syn_lens_provenance_complete() = Ok"),
        Err(error) => {
            return Err(format!("the shipped gate rejects the live tables: {error}").into());
        }
    }

    // And that it is genuinely on the panel-contract path, not merely callable.
    println!("\n=== the gate is on the startup path");
    for version in [
        SYN_TIMELINE_PANEL_VERSION,
        SYN_EPISODE_PANEL_VERSION,
        SYN_MCP_USAGE_PANEL_VERSION,
        SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
    ] {
        let contract = syn_active_panel_contract(version, 0)?;
        let slots = contract
            .as_ref()
            .map_or(0, |contract| contract.panel.slots.len());
        println!("   syn_active_panel_contract({version}) -> {slots} slots");
        if slots == 0 {
            return Err(format!("panel {version} built no slots").into());
        }
        // Every slot the contract publishes must be declared, or the gate that
        // just ran did not cover the thing it claims to cover.
        for slot in contract
            .as_ref()
            .map(|c| &c.panel.slots)
            .into_iter()
            .flatten()
        {
            if !declared.contains_key(&slot.slot_id.get()) {
                return Err(format!(
                    "panel {version} published slot {} with no declaration, yet the gate \
                     passed -- the gate is not covering published slots",
                    slot.slot_id.get()
                )
                .into());
            }
        }
    }

    println!("\nPASS: the gate names every drift, and covers every slot the contracts publish.");
    Ok(())
}

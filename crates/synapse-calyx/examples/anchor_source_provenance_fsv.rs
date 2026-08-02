//! Full-state verification for issue #1958: anchor leakage is refused from
//! declared provenance, not inferred from statistics.
//!
//! ## The limit this closes
//!
//! `detect_anchor_leakage` (#1953 ask 2) fires when a lens's marginal bits equal
//! `H(anchor)` at matching cardinality. Only a lens that **is** the label can
//! meet that signature. Measured on `syn-mcp-usage-v1 @ 1776006`:
//!
//! ```text
//! pass 1  nothing withheld     panel_bits=0.969018  detector: [86]
//! pass 2  withheld {86}        panel_bits=0.946621  detector: CLEAN   <-- floored on slot 93
//! pass 3  withheld {86,87,93}  panel_bits=0.958031  detector: CLEAN
//! ```
//!
//! Pass 2 is the worst outcome available: a `CLEAN` verdict over a number whose
//! floor is itself a carrier. No threshold fixes that, because two lenses with
//! identical bits — one reading the label, one predicting it — are
//! information-theoretically indistinguishable. The difference is provenance.
//!
//! ## What this proves
//!
//! The source of truth is the declaration tables in
//! `synapse_calyx::lens_provenance`, and the intersection over them is exact and
//! deterministic — no corpus, no estimator, no sample-size floor. This harness
//! runs every (anchor, panel) pair the daemon actually writes and checks the
//! carriers against independently-stated expectations, so "the code agrees with
//! itself" cannot pass.
//!
//! It also audits the tables themselves: every declared slot must be catalogued
//! with the same lens name, and vice versa, which is the drift the #1953 facade
//! gap is an example of.
//!
//! `cargo run --release -p synapse-calyx --example anchor_source_provenance_fsv`

use std::collections::BTreeSet;
use std::error::Error;

use synapse_calyx::lens_provenance::{
    SYN_ANCHOR_DETERMINING_FIELDS, SYN_SLOT_SOURCE_FIELDS, syn_anchor_source_provenance,
    syn_panel_slots, syn_slot_source_fields,
};

/// One (anchor, panel) pair and the slots that must be named as carriers.
///
/// The expectations are stated here, derived by reading the construction sites
/// by hand — deliberately a *second*, independent derivation from the tables the
/// check consults. If they agree, two derivations agree; if the table alone were
/// consulted the run would prove only that a set intersects itself.
struct Expectation {
    anchor: &'static str,
    panel_version: u32,
    panel: &'static str,
    /// Slot ids that must be refused, and why, in the order the check returns.
    carriers: &'static [(u16, &'static str)],
}

const EXPECTED: &[Expectation] = &[
    Expectation {
        anchor: "synapse:mcp_tool_call_outcome",
        panel_version: 1_776_006,
        panel: "syn-mcp-usage-v1",
        carriers: &[
            (86, "status_onehot IS the anchor field"),
            (
                87,
                "error_onehot reads error_type, Some exactly when the call failed",
            ),
            (
                93,
                "record_vector's has_error component derives from error_type",
            ),
        ],
    },
    Expectation {
        anchor: "synapse:mcp_tool_call_outcome",
        panel_version: 1_900_001,
        panel: "syn-timeline-v1",
        carriers: &[],
    },
    Expectation {
        anchor: "synapse:mcp_steering_enabled",
        panel_version: 1_776_006,
        panel: "syn-mcp-usage-v1",
        carriers: &[],
    },
    Expectation {
        anchor: "synapse:mcp_default_promotion_state",
        panel_version: 1_776_006,
        panel: "syn-mcp-usage-v1",
        carriers: &[],
    },
    Expectation {
        anchor: "synapse:agent_tool_call_success",
        panel_version: 1_665_001,
        panel: "syn-agent-event-v1",
        carriers: &[
            (29, "error_onehot reads attributes.error_type"),
            (30, "end_state_onehot reads end_state"),
            (34, "record_vector reads error_type, end_state and payload"),
        ],
    },
    Expectation {
        anchor: "synapse:agent_end_state",
        panel_version: 1_665_001,
        panel: "syn-agent-event-v1",
        carriers: &[
            (30, "end_state_onehot IS the anchor field"),
            (34, "record_vector carries end_state"),
        ],
    },
    Expectation {
        anchor: "synapse:agent_end_state",
        panel_version: 1_921_001,
        panel: "syn-agent-transcript-v1",
        carriers: &[],
    },
    Expectation {
        anchor: "synapse:episode_segmentation_outcome",
        panel_version: 1_904_002,
        panel: "syn-episode-v1",
        carriers: &[
            (20, "ended_boundary_onehot reads ended_because"),
            (21, "interruption_ratio reads interrupted_ms"),
            (
                22,
                "record_vector reads interrupted_ms and interruption_count",
            ),
        ],
    },
    Expectation {
        anchor: "synapse:verification_outcome",
        panel_version: 1_776_005,
        panel: "syn-outcome-v1",
        carriers: &[
            (77, "status_onehot reads code_count"),
            (81, "record_vector reads code_count"),
        ],
    },
    Expectation {
        anchor: "synapse:approval_decision",
        panel_version: 1_776_005,
        panel: "syn-outcome-v1",
        carriers: &[
            (77, "status_onehot reads after_status"),
            (81, "record_vector reads after_status"),
        ],
    },
    Expectation {
        anchor: "synapse:escalation_event",
        panel_version: 1_776_005,
        panel: "syn-outcome-v1",
        carriers: &[
            (76, "event_onehot reads event"),
            (81, "record_vector reads event"),
        ],
    },
    Expectation {
        anchor: "synapse:routine_transition",
        panel_version: 1_776_005,
        panel: "syn-outcome-v1",
        // Slot 76 was missing from the first draft of this expectation, and the
        // check caught it: `outcome_event` reads `action` as one of its
        // candidate fields, and `action` is what `routine_transition_anchor_value`
        // is computed from. Two independent derivations disagreeing is the point
        // of stating them separately, and here the table was right.
        carriers: &[
            (
                76,
                "event_onehot reads action, which the anchor value is computed from",
            ),
            (77, "status_onehot reads lifecycle"),
            (81, "record_vector reads lifecycle and action"),
        ],
    },
];

fn audit_tables() -> Result<(), Box<dyn Error>> {
    println!("=== A. the declaration tables themselves");
    let mut seen = BTreeSet::new();
    for (slot, _, lens, _) in SYN_SLOT_SOURCE_FIELDS {
        if !seen.insert(*slot) {
            return Err(format!("slot {slot} ({lens}) declared twice").into());
        }
    }
    println!("   {} slot declarations, no duplicates", seen.len());

    // A lens that declares nothing must be one of the derived-snapshot panels.
    // Anywhere else, an empty declaration would be a silent hole: the
    // intersection can never fire, so the slot is unauditable by construction.
    let empty: Vec<u16> = SYN_SLOT_SOURCE_FIELDS
        .iter()
        .filter(|(_, _, _, fields)| fields.is_empty())
        .map(|(slot, _, _, _)| *slot)
        .collect();
    let allowed_empty: BTreeSet<u16> = [75, 94, 95, 96, 97, 98, 99, 100, 101, 102]
        .into_iter()
        .collect();
    println!(
        "   {} slots declare no record field: {empty:?}",
        empty.len()
    );
    for slot in &empty {
        if !allowed_empty.contains(slot) {
            return Err(format!(
                "slot {slot} declares no record field but is not a derived-snapshot or \
                 source-CF lens; an empty declaration there is unauditable by construction"
            )
            .into());
        }
    }
    println!("   all of them are derived-snapshot / source-CF lenses, as declared");
    println!(
        "   {} anchor declarations across {} distinct anchor kinds",
        SYN_ANCHOR_DETERMINING_FIELDS.len(),
        SYN_ANCHOR_DETERMINING_FIELDS
            .iter()
            .map(|(kind, _, _)| *kind)
            .collect::<BTreeSet<_>>()
            .len()
    );
    Ok(())
}

fn check_expectations() -> Result<(), Box<dyn Error>> {
    println!("\n=== B. every (anchor, panel) pair the daemon writes");
    let mut refused = 0usize;
    for expected in EXPECTED {
        // The panel's slot set comes from the declaration table, so a panel that
        // declares nothing is a hole rather than a vacuous pass.
        let slots = syn_panel_slots(expected.panel_version);
        if slots.is_empty() {
            return Err(format!(
                "lens_provenance declares no slot for panel_version={}; the pair would be \
                 checked against an empty panel and would pass vacuously",
                expected.panel_version
            )
            .into());
        }
        let provenance =
            syn_anchor_source_provenance(expected.anchor, expected.panel_version, &BTreeSet::new());
        if !provenance.anchor_declared {
            return Err(format!(
                "{} @ {} is UNDECLARED — the daemon writes this anchor, so an undeclared \
                 pair is a hole the check cannot see through",
                expected.anchor, expected.panel
            )
            .into());
        }
        let got: Vec<u16> = provenance.carriers.iter().map(|c| c.slot).collect();
        let want: Vec<u16> = expected.carriers.iter().map(|(slot, _)| *slot).collect();
        let verdict = if got == want { "MATCH" } else { "MISMATCH" };
        println!(
            "\n   {} @ {} ({})",
            expected.anchor, expected.panel, expected.panel_version
        );
        println!(
            "     anchor determined by [{}]",
            provenance.anchor_fields.join(", ")
        );
        println!("     expected carriers {want:?}   got {got:?}   {verdict}");
        for carrier in &provenance.carriers {
            println!(
                "       slot {:>3}  {:<44} shares [{}]",
                carrier.slot,
                carrier.lens,
                carrier.shared_fields.join(", ")
            );
        }
        for (slot, why) in expected.carriers {
            println!("       expect {slot:>3}  {why}");
        }
        if got != want {
            return Err(format!(
                "{} @ {}: expected carriers {want:?}, got {got:?}",
                expected.anchor, expected.panel
            )
            .into());
        }
        if !got.is_empty() {
            refused += 1;
        }
    }
    println!(
        "\n   {refused} of {} pairs would be refused; the rest are declared clean.",
        EXPECTED.len()
    );
    Ok(())
}

fn check_edges() -> Result<(), Box<dyn Error>> {
    println!(
        "
=== C. boundary cases"
    );
    let none = BTreeSet::new();

    // C1: an undeclared anchor kind must report `anchor_declared = false`, never
    // an empty carrier list that reads as a clean bill of health.
    let unknown = syn_anchor_source_provenance("synapse:not_a_real_anchor", 1_776_006, &none);
    println!(
        "   C1 undeclared anchor kind             anchor_declared={} carriers={}",
        unknown.anchor_declared,
        unknown.carriers.len()
    );
    if unknown.anchor_declared {
        return Err("an undeclared anchor kind reported itself as declared".into());
    }

    // C2: the same anchor on an undeclared panel version is also undeclared,
    // rather than inheriting another panel's field set. This is what a panel
    // version bump would look like if the declaration were left behind.
    let wrong_panel = syn_anchor_source_provenance("synapse:mcp_tool_call_outcome", 42, &none);
    println!(
        "   C2 declared anchor, unknown panel     anchor_declared={}",
        wrong_panel.anchor_declared
    );
    if wrong_panel.anchor_declared {
        return Err("an anchor inherited a field set across panel versions".into());
    }

    // C3: an empty vault cannot change the verdict. The check is a function of
    // the declarations alone, so the same panel returns the same carriers
    // whether or not any row exists -- this is the defect the first version had,
    // where the verdict came from the slots that happened to have samples.
    let full = syn_anchor_source_provenance("synapse:mcp_tool_call_outcome", 1_776_006, &none);
    let got: Vec<u16> = full.carriers.iter().map(|c| c.slot).collect();
    println!("   C3 corpus-independent verdict         carriers={got:?} (expected [86, 87, 93])");
    if got != vec![86, 87, 93] {
        return Err(format!("expected [86, 87, 93], got {got:?}").into());
    }

    // C4: withholding the carriers is the documented remediation, so the same
    // panel minus those slots must come back clean. This is the exact pass-3
    // configuration from the issue.
    let withheld: BTreeSet<u16> = [86, 87, 93].into_iter().collect();
    let excluded =
        syn_anchor_source_provenance("synapse:mcp_tool_call_outcome", 1_776_006, &withheld);
    println!(
        "   C4 excluded_slots={{86,87,93}}           carriers={}",
        excluded.carriers.len()
    );
    if !excluded.carriers.is_empty() || !excluded.anchor_declared {
        return Err("withholding the named carriers did not clear the refusal".into());
    }

    // C5: a partial exclusion must leave exactly the rest. Excluding the lens
    // that IS the label while leaving the one that merely contains it is #1958's
    // pass 2 -- the configuration that used to read CLEAN.
    let partial: BTreeSet<u16> = [86].into_iter().collect();
    let rest = syn_anchor_source_provenance("synapse:mcp_tool_call_outcome", 1_776_006, &partial);
    let got: Vec<u16> = rest.carriers.iter().map(|c| c.slot).collect();
    println!("   C5 excluded_slots={{86}} (#1958 pass 2) carriers={got:?} (expected [87, 93])");
    if got != vec![87, 93] {
        return Err(format!("expected [87, 93], got {got:?}").into());
    }

    // C6: excluding slots that are not carriers, or not on this panel at all,
    // must not change the answer or crash.
    let irrelevant: BTreeSet<u16> = [1, 82, 91, 999].into_iter().collect();
    let unchanged =
        syn_anchor_source_provenance("synapse:mcp_tool_call_outcome", 1_776_006, &irrelevant);
    let got: Vec<u16> = unchanged.carriers.iter().map(|c| c.slot).collect();
    println!("   C6 irrelevant exclusions              carriers={got:?} (expected [86, 87, 93])");
    if got != vec![86, 87, 93] {
        return Err(format!("expected [86, 87, 93], got {got:?}").into());
    }
    if syn_slot_source_fields(999).is_some() {
        return Err("slot 999 should not be declared".into());
    }

    // C7: every panel the tables describe must publish at least one slot, or
    // `syn_panel_slots` would silently hand an empty panel to the check.
    for version in [
        1_900_001_u32,
        1_904_002,
        1_665_001,
        1_921_001,
        1_776_001,
        1_776_002,
        1_776_003,
        1_776_004,
        1_776_005,
        1_776_006,
        1_776_007,
        1_685_001,
        1_685_002,
        1_685_003,
    ] {
        let count = syn_panel_slots(version).len();
        if count == 0 {
            return Err(format!("panel {version} declares no slot").into());
        }
    }
    println!("   C7 every panel declares >=1 slot      ok");

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("anchor_source_provenance_fsv  (#1958)\n");
    audit_tables()?;
    check_expectations()?;
    check_edges()?;
    println!("\nPASS: leakage is decided from declared provenance, identically on every run.");
    Ok(())
}

//! Manual FSV for #1953 asks 3 and 4, and the #1958 rewrite that makes it sound.
//!
//! Run against a **frozen copy** of the live vault.
//!
//! ## What this produces
//!
//! For every (panel, anchor) pair Synapse actually writes: the **observable-
//! feature answer** — how much the panel's genuinely predictive lenses say about
//! the outcome, once every lens that carries the label has been withheld.
//!
//! ## What changed in #1958, and why the old version could not be trusted
//!
//! The previous version derived the carrier set from `detect_anchor_leakage` —
//! the statistical check — and carried a **hand-written** constant listing the
//! carriers that check misses. Both halves were wrong for the same reason.
//!
//! The statistical signature is "marginal bits equal `H(anchor)` at matching
//! cardinality", which only a lens that **is** the label can meet. A lens that
//! merely *contains* the label — one component of a 128-dimension vector, or a
//! field that determines it — meets neither condition. On
//! `syn-mcp-usage-v1 @ 1776006` that check found slot 86 and missed 87 and 93,
//! and withholding only slot 86 produced a `CLEAN` verdict over a number that
//! was floored on slot 93, itself a carrier. Circular, reported as clean.
//!
//! And the hand-written mirror listing the misses said `&[]` for eleven of the
//! twelve pairs, because only the mcp-usage pair had been worked through by
//! hand. It was not that the other eleven were clean; it was that nobody had
//! looked.
//!
//! So the carrier set now comes from `synapse_calyx::lens_provenance`, which
//! declares what each lens reads and what determines each anchor, and intersects
//! them. That is exact, deterministic, and independent of the corpus — and it is
//! also now enforced by `assay_sufficiency` itself, which refuses a circular
//! configuration outright. This harness therefore starts from the withheld set
//! rather than discovering it, and the un-withheld pass exists only to
//! demonstrate the refusal.
//!
//! ## Why a frozen copy rather than the live daemon
//!
//! Every number here is a *delta* — with the carriers versus without them — and
//! the live vault is a moving corpus. A before/after taken minutes apart on a
//! live vault measures drift plus the change and cannot separate them. The copy
//! is taken once and every arm of every comparison reads the same bytes.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example anchor_leakage_audit_fsv -- <vault-copy-dir>`
//! where `<vault-copy-dir>` contains `db-daemon/` and `machine-salt.bin`.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::lens_provenance::{syn_anchor_source_provenance, syn_panel_slots};
use synapse_calyx::{
    SYNAPSE_ASSAY_ANCHOR_SOURCE_LEAKAGE, SynapseCalyxAssayParams, SynapseCalyxConfig,
    SynapseCalyxTuningConfig, SynapseCalyxVault,
};

/// Every (panel, anchor) pair Synapse actually writes, so ask 4 is an
/// enumeration rather than a spot check. A pair with no anchored records is
/// reported as such — "not measurable here" is an observation, and silently
/// skipping it would make an unaudited panel look audited.
///
/// The carrier set is no longer a column here. It is derived per pair from the
/// declaration tables, which is #1958 ask 1.
type PanelAnchorPair = (&'static str, u32, &'static str);

const PAIRS: &[PanelAnchorPair] = &[
    (
        "syn-mcp-usage-v1",
        1_776_006,
        "synapse:mcp_tool_call_outcome",
    ),
    (
        "syn-mcp-usage-v1",
        1_776_006,
        "synapse:mcp_steering_enabled",
    ),
    (
        "syn-mcp-usage-v1",
        1_776_006,
        "synapse:mcp_default_promotion_state",
    ),
    (
        "syn-agent-event-v1",
        1_665_001,
        "synapse:agent_tool_call_success",
    ),
    ("syn-agent-event-v1", 1_665_001, "synapse:agent_end_state"),
    (
        "syn-agent-transcript-v1",
        1_921_001,
        "synapse:agent_end_state",
    ),
    (
        "syn-episode-v1",
        1_904_002,
        "synapse:episode_segmentation_outcome",
    ),
    ("syn-outcome-v1", 1_776_005, "synapse:verification_outcome"),
    ("syn-outcome-v1", 1_776_005, "synapse:approval_decision"),
    ("syn-outcome-v1", 1_776_005, "synapse:escalation_event"),
    ("syn-outcome-v1", 1_776_005, "synapse:routine_transition"),
    (
        "syn-timeline-v1",
        1_900_001,
        "synapse:mcp_tool_call_outcome",
    ),
];

const MAX_RECORDS: usize = 4_000;

struct PassResult {
    anchored_records: usize,
    panel_bits: f32,
    anchor_entropy_bits: f32,
    sufficient: bool,
    panel_measured: bool,
    deficit_bits: f32,
    leaking_slots: Vec<u16>,
    joint_records: usize,
    panel_floor_applied: bool,
    unmeasured_slots: usize,
    anchor_source_declared: bool,
}

fn run_pass(
    vault: &SynapseCalyxVault,
    panel_version: u32,
    anchor_kind: &str,
    excluded: &BTreeSet<u16>,
) -> Result<PassResult, Box<dyn Error>> {
    let mut params = SynapseCalyxAssayParams::new(panel_version, anchor_kind.to_owned());
    params.max_records = MAX_RECORDS;
    params.excluded_slots = excluded.clone();
    let report = vault.assay_sufficiency(&params)?;
    Ok(PassResult {
        anchored_records: report.anchored_records,
        panel_bits: report.panel_bits,
        anchor_entropy_bits: report.anchor_entropy_bits,
        sufficient: report.sufficient,
        panel_measured: report.panel_measured,
        deficit_bits: report.deficit_bits,
        joint_records: report.joint_records,
        panel_floor_applied: report.panel_floor_applied,
        unmeasured_slots: report.unmeasured_slots,
        anchor_source_declared: report.anchor_source_declared,
        leaking_slots: report.anchor_leakage.iter().map(|leak| leak.slot).collect(),
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear audit per pair; splitting would separate a measurement from the checks that make it admissible"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: anchor_leakage_audit_fsv <vault-copy-dir>")?;
    let vault_dir = root.join("db-daemon");
    let salt = root.join("machine-salt.bin");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }

    let config = SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: salt,
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    };
    let vault = SynapseCalyxVault::open(config)?;

    println!("anchor_leakage_audit_fsv  (#1953 asks 3 and 4, rewritten for #1958)");
    println!("frozen vault copy = {}", vault_dir.display());
    println!("latest_seq = {}", vault.latest_seq());
    println!("max_records per pass = {MAX_RECORDS}");

    let mut audited = 0usize;
    let mut carrier_pairs = 0usize;
    let mut unmeasurable = 0usize;
    let mut answers = Vec::new();
    let mut unresolved = Vec::new();

    for (panel_name, panel_version, anchor_kind) in PAIRS {
        println!("\n================================================================");
        println!("{panel_name} @ {panel_version}   anchor = {anchor_kind}");

        // The panel's slot set comes from the declaration table, so a panel
        // version with no declared slots is a hole rather than a clean pass.
        let slots = syn_panel_slots(*panel_version);
        if slots.is_empty() {
            unresolved.push(format!(
                "{panel_name}/{anchor_kind}: lens_provenance declares no slot for this panel \
                 version, so the carrier derivation would be asked about an empty panel"
            ));
            continue;
        }
        let provenance =
            syn_anchor_source_provenance(anchor_kind, *panel_version, &BTreeSet::new());
        if !provenance.anchor_declared {
            unresolved.push(format!(
                "{panel_name}/{anchor_kind}: UNDECLARED in lens_provenance. Synapse writes this \
                 anchor, so an undeclared pair is a hole the structural check cannot see \
                 through -- declare its determining fields"
            ));
            continue;
        }
        let withheld: BTreeSet<u16> = provenance.carriers.iter().map(|c| c.slot).collect();
        println!(
            "   anchor determined by [{}]",
            provenance.anchor_fields.join(", ")
        );
        if provenance.carriers.is_empty() {
            println!(
                "   declared carriers: NONE -- no lens on this panel reads a determining field"
            );
        } else {
            carrier_pairs += 1;
            println!(
                "   declared carriers: {} lens(es)",
                provenance.carriers.len()
            );
            for carrier in &provenance.carriers {
                println!(
                    "      slot {:>3}  {:<44} reads [{}]",
                    carrier.slot,
                    carrier.lens,
                    carrier.shared_fields.join(", ")
                );
            }

            // The un-withheld pass must now be REFUSED, not merely reported. A
            // pass that still succeeds here means the structural check is not on
            // this path and every number below is unguarded.
            match run_pass(&vault, *panel_version, anchor_kind, &BTreeSet::new()) {
                Ok(result) => {
                    unresolved.push(format!(
                        "{panel_name}/{anchor_kind}: the un-withheld pass SUCCEEDED \
                         (panel_bits={:.6}) despite {} declared carrier(s); the structural \
                         refusal is not guarding this path",
                        result.panel_bits,
                        provenance.carriers.len()
                    ));
                    continue;
                }
                Err(error) => {
                    let refused = error.to_string();
                    if refused.contains(SYNAPSE_ASSAY_ANCHOR_SOURCE_LEAKAGE) {
                        println!("   un-withheld pass: REFUSED by the structural check");
                    } else {
                        println!("   un-withheld pass: other error: {refused}");
                    }
                }
            }
        }

        // The measurement the panel exists to produce.
        let measured = match run_pass(&vault, *panel_version, anchor_kind, &withheld) {
            Ok(result) => result,
            Err(error) => {
                println!("   withheld pass ERROR: {error}");
                unresolved.push(format!(
                    "{panel_name}/{anchor_kind}: the withheld pass errored, so the carriers \
                     do not cover every route to the label: {error}"
                ));
                continue;
            }
        };

        if measured.anchored_records == 0 {
            println!("   no anchored records for this anchor on this panel -> NOT AUDITABLE HERE");
            unmeasurable += 1;
            continue;
        }
        audited += 1;

        println!(
            "   withheld {:?}: anchored={} panel_bits={:.6} H(anchor)={:.6} measured={} \
             sufficient={} deficit={:.6}",
            withheld,
            measured.anchored_records,
            measured.panel_bits,
            measured.anchor_entropy_bits,
            measured.panel_measured,
            measured.sufficient,
            measured.deficit_bits
        );
        println!(
            "          joint_records={} floor_applied={} unmeasured_slots={} \
             anchor_source_declared={}",
            measured.joint_records,
            measured.panel_floor_applied,
            measured.unmeasured_slots,
            measured.anchor_source_declared
        );

        if !measured.anchor_source_declared {
            unresolved.push(format!(
                "{panel_name}/{anchor_kind}: the surviving report says the structural check \
                 did not run, which contradicts the declaration this harness just read"
            ));
            continue;
        }
        if !measured.leaking_slots.is_empty() {
            unresolved.push(format!(
                "{panel_name}/{anchor_kind}: the statistical detector still names {:?} after \
                 every structural carrier was withheld -- there is a route to the label the \
                 declaration does not describe",
                measured.leaking_slots
            ));
            continue;
        }
        if !measured.panel_measured {
            println!("   => panel not measured (below the sample floor); no answer to quote");
            continue;
        }
        // A floored value is NOT a joint measurement -- #1916 raises
        // `panel_bits` to the best single-lens estimate when the joint estimator
        // returns less than a lens it contains. Quoting one as "what the panel
        // carries" reports one lens's marginal bits as a panel measurement.
        // Refuse rather than print.
        if measured.panel_floor_applied {
            unresolved.push(format!(
                "{panel_name}/{anchor_kind}: panel_floor_applied=true, so {:.6} is the best \
                 SINGLE lens raised as a floor (#1916), not a joint panel measurement; there \
                 is no observable-feature answer to quote",
                measured.panel_bits
            ));
            continue;
        }
        let share = measured.panel_bits / measured.anchor_entropy_bits;
        println!(
            "   => OBSERVABLE-FEATURE ANSWER: {:.6} bits, {:.1}% of H(anchor)={:.6}, \
             sufficient={}, deficit={:.6}",
            measured.panel_bits,
            share * 100.0,
            measured.anchor_entropy_bits,
            measured.sufficient,
            measured.deficit_bits
        );
        answers.push(format!(
            "{panel_name}/{anchor_kind}: {:.6} bits ({:.1}% of H={:.6}), withheld {:?}",
            measured.panel_bits,
            share * 100.0,
            measured.anchor_entropy_bits,
            withheld
        ));
    }

    println!("\n================================================================");
    println!("SUMMARY");
    println!("  pairs enumerated             = {}", PAIRS.len());
    println!("  pairs with declared carriers = {carrier_pairs}");
    println!("  pairs measured               = {audited}");
    println!("  pairs with no anchors        = {unmeasurable}");
    println!("  observable-feature answers:");
    for answer in &answers {
        println!("    {answer}");
    }
    if unresolved.is_empty() {
        println!(
            "\nPASS: every declared carrier is refused, and every measurable pair has a \
             non-circular answer."
        );
        return Ok(());
    }
    println!("\n  UNRESOLVED ({}):", unresolved.len());
    for item in &unresolved {
        println!("    {item}");
    }
    Err(format!("{} pair(s) unresolved", unresolved.len()).into())
}

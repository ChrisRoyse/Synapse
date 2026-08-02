//! Manual FSV for #1953 asks 3 and 4, run against a **frozen copy** of the live
//! vault.
//!
//! ## What is still owed on #1953
//!
//! Ask 2 (make leakage detectable) landed: `detect_anchor_leakage` fires and
//! `sufficient` flipped to false on the live daemon. Three asks remained, and
//! all three are measurements rather than decisions-in-the-abstract:
//!
//! - **ask 1** — decide slot 86's status against `synapse:mcp_tool_call_outcome`.
//!   It cannot be both a lens and the label.
//! - **ask 3** — re-run the acceptance *with slot 86 excluded*, to finally get
//!   the number the panel exists to produce: how much do the **observable**
//!   features (tool, route, params, profile, timing) say about the outcome.
//! - **ask 4** — audit the other panels for the same pattern.
//!
//! ## Why a frozen copy rather than the live daemon
//!
//! Every number here is a *delta* — with the leaking lens versus without it —
//! and the live vault is a moving corpus. A before/after taken minutes apart on
//! a live vault measures drift plus the change and cannot separate them. The
//! copy is taken once and both arms of every comparison read the same bytes.
//!
//! ## Method, and the trap it avoids
//!
//! Ask 3 as written says "re-run with slot 86 excluded". Taking that literally
//! would be a mistake if slot 86 is not the only carrier of the label — the
//! result would still be circular and would *look* clean. So this harness does
//! not assume the carrier set; it **derives** it:
//!
//! 1. measure per-lens bits with nothing withheld;
//! 2. let the detector name every slot whose bits match `H(anchor)` — the
//!    leakage signature (bits within CI at matching cardinality);
//! 3. withhold exactly that derived set;
//! 4. re-measure, and require the detector to find nothing on the second pass.
//!
//! Step 4 is the acceptance. If a second carrier exists, pass 2 still reports
//! leakage and the harness says so instead of reporting a clean number.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example anchor_leakage_audit_fsv -- <vault-copy-dir>`
//! where `<vault-copy-dir>` contains `db-daemon/` and `machine-salt.bin`.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::{
    SynapseCalyxAssayParams, SynapseCalyxConfig, SynapseCalyxTuningConfig, SynapseCalyxVault,
};

/// Every (panel, anchor) pair Synapse actually writes, so ask 4 is an
/// enumeration rather than a spot check. A pair with no anchored records is
/// reported as such — "not measurable here" is an observation, and silently
/// skipping it would make an unaudited panel look audited.
///
/// The fourth element is the provenance carrier set, documented below.
type PanelAnchorPair = (&'static str, u32, &'static str, ProvenanceCarriers);

/// Slots whose **source field** is the anchor's source field, or a deterministic
/// function of it, established by reading the encoder rather than by measuring.
///
/// This exists because the statistical detector added for #1953 ask 2 cannot
/// find them, and that limitation is fundamental rather than a tuning problem.
/// The detector's signature is "marginal bits equal `H(anchor)` at matching
/// cardinality" — a lens that *is* the label. A lens that merely *contains* the
/// label as one component of a 128-dimension vector has neither equal bits (the
/// KSG estimator under-reads a dense joint) nor matching cardinality, so it
/// passes every statistical test while carrying the answer.
///
/// **Leakage is a provenance property, not a statistical one.** Two lenses with
/// identical bits about an outcome — one that reads the label and one that
/// genuinely predicts it — are information-theoretically indistinguishable. The
/// only thing that separates them is where their input came from.
///
/// For `syn-mcp-usage-v1` / `synapse:mcp_tool_call_outcome`, the anchor is
/// `record.status`, and:
///
/// - **86** `status_onehot` — `json_string(record, &["status"])`. It *is* the
///   anchor. (The detector does find this one.)
/// - **87** `error_onehot` — `json_string(record, &["error_type"])`.
///   `error_type` is `Some` exactly when the call failed, which is what `status`
///   records, so presence determines the label.
/// - **93** `record_vector` — measured over `mcp_usage_numeric_record`, which
///   contains `"has_error": bool_u64(json_string(record, &["error_type"]).is_some())`.
///   The label is literally a component of the vector.
type ProvenanceCarriers = &'static [u16];

const PAIRS: &[PanelAnchorPair] = &[
    (
        "syn-mcp-usage-v1",
        1_776_006,
        "synapse:mcp_tool_call_outcome",
        &[86, 87, 93],
    ),
    (
        "syn-mcp-usage-v1",
        1_776_006,
        "synapse:mcp_steering_enabled",
        &[],
    ),
    (
        "syn-mcp-usage-v1",
        1_776_006,
        "synapse:mcp_default_promotion_state",
        &[],
    ),
    (
        "syn-agent-transcript-v1",
        1_921_001,
        "synapse:agent_tool_call_success",
        &[],
    ),
    (
        "syn-agent-event-v1",
        1_665_001,
        "synapse:agent_end_state",
        &[],
    ),
    (
        "syn-agent-transcript-v1",
        1_921_001,
        "synapse:agent_end_state",
        &[],
    ),
    (
        "syn-episode-v1",
        1_904_002,
        "synapse:episode_segmentation_outcome",
        &[],
    ),
    (
        "syn-outcome-v1",
        1_776_005,
        "synapse:verification_outcome",
        &[],
    ),
    (
        "syn-outcome-v1",
        1_776_005,
        "synapse:approval_decision",
        &[],
    ),
    ("syn-outcome-v1", 1_776_005, "synapse:escalation_event", &[]),
    (
        "syn-reflex-v1",
        1_776_002,
        "synapse:routine_transition",
        &[],
    ),
    (
        "syn-timeline-v1",
        1_900_001,
        "synapse:mcp_tool_call_outcome",
        &[],
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
        leaking_slots: report.anchor_leakage.iter().map(|leak| leak.slot).collect(),
    })
}

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

    println!("anchor_leakage_audit_fsv  (#1953 asks 3 and 4)");
    println!("frozen vault copy = {}", vault_dir.display());
    println!("latest_seq = {}", vault.latest_seq());
    println!("max_records per pass = {MAX_RECORDS}");

    let mut audited = 0usize;
    let mut leaking_pairs = 0usize;
    let mut unmeasurable = 0usize;
    let mut unresolved = Vec::new();

    for (panel_name, panel_version, anchor_kind, provenance_carriers) in PAIRS {
        println!("\n================================================================");
        println!("{panel_name} @ {panel_version}   anchor = {anchor_kind}");

        let first = match run_pass(&vault, *panel_version, anchor_kind, &BTreeSet::new()) {
            Ok(result) => result,
            Err(error) => {
                println!("   pass 1 ERROR: {error}");
                unresolved.push(format!(
                    "{panel_name}/{anchor_kind}: pass 1 errored: {error}"
                ));
                continue;
            }
        };

        if first.anchored_records == 0 {
            println!("   no anchored records for this anchor on this panel -> NOT AUDITABLE HERE");
            unmeasurable += 1;
            continue;
        }
        audited += 1;

        println!(
            "   pass 1 (nothing withheld): anchored={} panel_bits={:.6} H(anchor)={:.6} \
             measured={} sufficient={} deficit={:.6}",
            first.anchored_records,
            first.panel_bits,
            first.anchor_entropy_bits,
            first.panel_measured,
            first.sufficient,
            first.deficit_bits
        );
        println!(
            "          joint_records={} floor_applied={} unmeasured_slots={}",
            first.joint_records, first.panel_floor_applied, first.unmeasured_slots
        );

        if first.leaking_slots.is_empty() {
            println!("   leakage detector: CLEAN (no lens matches H(anchor))");
            continue;
        }

        leaking_pairs += 1;
        println!(
            "   leakage detector: {} slot(s) carry the label: {:?}",
            first.leaking_slots.len(),
            first.leaking_slots
        );

        // Ask 3 — withhold exactly the derived carrier set, not an assumed one.
        let withheld: BTreeSet<u16> = first.leaking_slots.iter().copied().collect();
        let second = match run_pass(&vault, *panel_version, anchor_kind, &withheld) {
            Ok(result) => result,
            Err(error) => {
                println!("   pass 2 ERROR: {error}");
                unresolved.push(format!(
                    "{panel_name}/{anchor_kind}: pass 2 errored: {error}"
                ));
                continue;
            }
        };

        println!(
            "   pass 2 (withheld {:?}): anchored={} panel_bits={:.6} H(anchor)={:.6} \
             measured={} sufficient={} deficit={:.6}",
            withheld,
            second.anchored_records,
            second.panel_bits,
            second.anchor_entropy_bits,
            second.panel_measured,
            second.sufficient,
            second.deficit_bits
        );
        println!(
            "          joint_records={} floor_applied={} unmeasured_slots={}",
            second.joint_records, second.panel_floor_applied, second.unmeasured_slots
        );

        // The corpus must not have moved between the passes. On a frozen copy
        // it cannot, and checking it is what proves the copy is frozen rather
        // than assuming it.
        if second.anchored_records != first.anchored_records {
            unresolved.push(format!(
                "{panel_name}/{anchor_kind}: anchored records changed between passes \
                 ({} -> {}); the two arms did not read the same corpus",
                first.anchored_records, second.anchored_records
            ));
            continue;
        }
        if (second.anchor_entropy_bits - first.anchor_entropy_bits).abs() > f32::EPSILON {
            unresolved.push(format!(
                "{panel_name}/{anchor_kind}: H(anchor) changed between passes ({:.6} -> {:.6}); \
                 withholding a LENS must not move the OUTCOME entropy",
                first.anchor_entropy_bits, second.anchor_entropy_bits
            ));
            continue;
        }

        // The acceptance. A clean second pass means the withheld set was the
        // whole carrier set; a dirty one means there is another route to the
        // label and the "observable features" number is still not this.
        if second.leaking_slots.is_empty() {
            println!("   pass 2 leakage detector: CLEAN");

            // ...and a CLEAN verdict here is exactly what must not be trusted
            // on its own. The detector's signature is "bits == H(anchor) at
            // matching cardinality", which only a lens that IS the label can
            // meet. A lens that CONTAINS the label as one component of a dense
            // vector passes it while carrying the answer. So pass 3 withholds
            // the carriers established by reading the encoders.
            let declared: BTreeSet<u16> = provenance_carriers.iter().copied().collect();
            if declared.is_subset(&withheld) {
                if second.panel_floor_applied {
                    unresolved.push(format!(
                        "{panel_name}/{anchor_kind}: panel_floor_applied=true, so {:.6} is \
                         one lens's marginal bits raised as a floor (#1916), not a panel \
                         measurement; there is no observable-feature answer to quote",
                        second.panel_bits
                    ));
                    continue;
                }
                let recovered = second.panel_bits;
                let share = recovered / first.anchor_entropy_bits;
                println!(
                    "   => OBSERVABLE-FEATURE ANSWER: {recovered:.6} bits, {:.1}% of \
                     H(anchor)={:.6}, sufficient={}",
                    share * 100.0,
                    first.anchor_entropy_bits,
                    second.sufficient
                );
                continue;
            }

            println!(
                "   provenance carriers NOT caught by the detector: {:?}",
                declared.difference(&withheld).collect::<Vec<_>>()
            );
            let full: BTreeSet<u16> = withheld.union(&declared).copied().collect();
            let third = match run_pass(&vault, *panel_version, anchor_kind, &full) {
                Ok(result) => result,
                Err(error) => {
                    println!("   pass 3 ERROR: {error}");
                    unresolved.push(format!(
                        "{panel_name}/{anchor_kind}: pass 3 errored: {error}"
                    ));
                    continue;
                }
            };
            println!(
                "   pass 3 (withheld {:?}): anchored={} panel_bits={:.6} H(anchor)={:.6} \
                 measured={} sufficient={} deficit={:.6}",
                full,
                third.anchored_records,
                third.panel_bits,
                third.anchor_entropy_bits,
                third.panel_measured,
                third.sufficient,
                third.deficit_bits
            );
            println!(
                "          joint_records={} floor_applied={} unmeasured_slots={}",
                third.joint_records, third.panel_floor_applied, third.unmeasured_slots
            );
            if third.anchored_records != first.anchored_records {
                unresolved.push(format!(
                    "{panel_name}/{anchor_kind}: anchored records moved between pass 1 and \
                     pass 3 ({} -> {})",
                    first.anchored_records, third.anchored_records
                ));
                continue;
            }
            // A floored value is NOT a joint measurement — #1916 raises
            // `panel_bits` to the best single-lens estimate when the joint
            // estimator returns less than a lens it contains. Quoting one as
            // "what the panel carries" reports one lens's marginal bits as a
            // panel measurement. Refuse rather than print.
            if third.panel_floor_applied {
                unresolved.push(format!(
                    "{panel_name}/{anchor_kind}: pass 3 has panel_floor_applied=true, so \
                     {:.6} is the best SINGLE lens raised as a floor, not a joint panel \
                     measurement; there is no observable-feature answer to quote",
                    third.panel_bits
                ));
                continue;
            }
            let share = third.panel_bits / first.anchor_entropy_bits;
            println!("   => TRUE OBSERVABLE-FEATURE ANSWER (every label carrier withheld):");
            println!(
                "      {:.6} bits, {:.1}% of H(anchor)={:.6}, sufficient={}, deficit={:.6}",
                third.panel_bits,
                share * 100.0,
                first.anchor_entropy_bits,
                third.sufficient,
                third.deficit_bits
            );
            if second.panel_floor_applied {
                println!(
                    "      This is NOT comparable to pass 2's {:.6}. That value has \
                     panel_floor_applied=true:",
                    second.panel_bits
                );
                println!(
                    "      it is the best SINGLE lens raised as a floor (#1916), and on this \
                     panel that lens is"
                );
                println!(
                    "      a withheld label carrier. So executing ask 3 literally — withhold \
                     only slot 86 —"
                );
                println!(
                    "      yields a number that is entirely the LEAKING lens's marginal bits: \
                     circular again,"
                );
                println!("      in a form the leakage detector reports as CLEAN.");
            } else {
                println!(
                    "      vs {:.6} bits ({:.1}%) when only the detector's find was withheld.",
                    second.panel_bits,
                    (second.panel_bits / first.anchor_entropy_bits) * 100.0
                );
            }
        } else {
            println!(
                "   pass 2 leakage detector: STILL LEAKING via {:?}",
                second.leaking_slots
            );
            unresolved.push(format!(
                "{panel_name}/{anchor_kind}: withholding {:?} did not remove the leakage; \
                 slot(s) {:?} still carry the label, so the observable-feature number is \
                 STILL not measured",
                withheld, second.leaking_slots
            ));
        }
    }

    println!("\n================================================================");
    println!("--- AUDIT SUMMARY (ask 4) ---");
    println!("  (panel, anchor) pairs declared   = {}", PAIRS.len());
    println!("  audited (had anchored records)   = {audited}");
    println!("  not auditable on this corpus     = {unmeasurable}");
    println!("  pairs where a lens IS the label  = {leaking_pairs}");
    println!("  unresolved                       = {}", unresolved.len());

    if unresolved.is_empty() {
        println!(
            "\n--- VERDICT: every leaking pair resolved to a clean observable-feature number."
        );
        Ok(())
    } else {
        println!();
        for item in &unresolved {
            println!("  UNRESOLVED: {item}");
        }
        Err(format!("{} pair(s) unresolved", unresolved.len()).into())
    }
}

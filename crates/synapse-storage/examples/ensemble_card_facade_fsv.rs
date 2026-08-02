//! Full-state verification for #1944 ask 1: the ensemble capability card is
//! reachable through the `storage` intelligence surface, and guarded.
//!
//! ## Why this exists
//!
//! #1944's finding is that **an unreached code path accumulates defects at full
//! rate and reports none of them** — `ensemble_card` had no caller anywhere in
//! Synapse, and three defects (#1942, #1943, and a whole-pass abort on one
//! degenerate lens) had accumulated on it unobserved. #1942 gave it a caller in
//! `synapse-calyx`. Ask 1 asked whether it belongs on the MCP `storage` facade,
//! which is the surface #1668's admission gate needs.
//!
//! This harness runs the same `Db` entry point the facade dispatches to, so what
//! it proves is the wiring the facade depends on, against a real vault.
//!
//! ## What it checks
//!
//! 1. The card runs and persists an Assay row — read back physically, not
//!    inferred from a return value.
//! 2. Its keep/park/retire verdicts cover every lens it measured, so the
//!    admission gate has an answer for each rather than a partial one.
//! 3. `pairs_monotonicity_floored` is reported. #1942's defect was a floored
//!    negative gain silently reading as "no synergy"; a count that cannot be
//!    seen is the same defect wearing a different hat.
//! 4. **The #1958 structural leakage refusal applies here too.** The card's
//!    headline is a panel-level claim with no per-slot breakdown a caller can
//!    inspect, which is exactly the shape that laundered a circular result on
//!    `sufficiency`. An un-withheld run on a panel with declared carriers must
//!    be refused; the withheld run must produce the card.
//!
//! Run against a **frozen copy** of the vault:
//!
//! ```text
//! cargo run --release -p synapse-storage --example ensemble_card_facade_fsv -- <vault-copy-dir>
//! ```

use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::SynapseCalyxAssayParams;
use synapse_calyx::lens_provenance::syn_anchor_source_provenance;
use synapse_storage::Db;

const PANEL: u32 = 1_776_006;
const ANCHOR: &str = "synapse:mcp_tool_call_outcome";
const MAX_RECORDS: usize = 2_000;
const MIN_GATE_LENSES: usize = 6;
/// Same schema version the daemon opens the vault with.
const SCHEMA_VERSION: u32 = 1;

fn params(excluded: &BTreeSet<u16>) -> SynapseCalyxAssayParams {
    let mut assay = SynapseCalyxAssayParams::new(PANEL, ANCHOR.to_owned())
        .with_lens_names(synapse_storage::constellations::syn_slot_lens_names());
    assay.max_records = MAX_RECORDS;
    assay.excluded_slots = excluded.clone();
    assay
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: ensemble_card_facade_fsv <vault-copy-dir>")?;
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    println!("ensemble_card_facade_fsv  (#1944 ask 1)");
    println!("vault  = {}", vault_dir.display());
    println!("panel  = {PANEL}   anchor = {ANCHOR}");

    // --- A. the structural guard reaches this path too (#1958 / #1959) ------
    let declared = syn_anchor_source_provenance(ANCHOR, PANEL, &BTreeSet::new());
    let carriers: Vec<u16> = declared.carriers.iter().map(|c| c.slot).collect();
    println!("\n=== A. declared label carriers on this panel: {carriers:?}");
    if carriers.is_empty() {
        return Err(
            "this panel declares no carrier, so phase A would pass vacuously; \
                    point the harness at a pair that has one"
                .into(),
        );
    }
    match db.assay_ensemble_card_intelligence(&params(&BTreeSet::new()), MIN_GATE_LENSES) {
        Ok(_) => {
            return Err(
                "the card ran over a panel containing the anchor's own source fields; a \
                 panel-level verdict with no per-slot breakdown is exactly what must not \
                 be served circularly (#1958)"
                    .into(),
            );
        }
        Err(error) => {
            let text = error.to_string();
            println!("   un-withheld run REFUSED: {text}");
            if !text.contains("ANCHOR_SOURCE_LEAKAGE") {
                return Err(
                    format!("refused, but not by the structural leakage check: {text}").into(),
                );
            }
        }
    }

    // --- B. the card itself, with the carriers withheld ---------------------
    let withheld: BTreeSet<u16> = carriers.iter().copied().collect();
    println!("\n=== B. card with excluded_slots={withheld:?}");
    let report = db.assay_ensemble_card_intelligence(&params(&withheld), MIN_GATE_LENSES)?;
    println!(
        "   records_scanned={} anchored={} declared_slots={} measured_slots={:?}",
        report.records_scanned,
        report.anchored_records,
        report.declared_slots,
        report.measured_slots
    );
    println!(
        "   panel_bits={:.6} H(anchor)={:.6} n_eff={:.3} sufficient={} deficit={:.6}",
        report.card.panel_bits,
        report.card.anchor_entropy_bits,
        report.card.n_eff,
        report.card.sufficient,
        report.card.deficit_bits
    );
    println!(
        "   keep={} park={} retire={} pairs_monotonicity_floored={} anchor_source_declared={}",
        report.card.keep_count,
        report.card.park_count,
        report.card.retire_count,
        report.card.pairs_monotonicity_floored,
        report.anchor_source_declared
    );
    for lens in &report.card.lenses {
        println!(
            "     slot {:>3}  {:<40} solo={:.4} marginal={:.4} corr={:.3} -> {:?}",
            lens.slot.get(),
            lens.name,
            lens.solo_bits,
            lens.marginal_bits,
            lens.max_pairwise_corr,
            lens.decision
        );
    }
    for excluded in &report.excluded_lenses {
        println!(
            "     slot {:>3}  EXCLUDED  {} -- {}",
            excluded.slot, excluded.name, excluded.reason
        );
    }

    // --- C. the checks that make the card usable as an admission gate -------
    println!("\n=== C. admission-gate invariants");
    if report.anchored_records == 0 {
        return Err("the card measured zero anchored records; it proves nothing".into());
    }
    if !report.anchor_source_declared {
        return Err("the card does not say whether the structural check ran".into());
    }
    let verdicts = report.card.keep_count + report.card.park_count + report.card.retire_count;
    println!(
        "   verdicts={} vs lenses={} (must be equal)",
        verdicts,
        report.card.lenses.len()
    );
    if verdicts != report.card.lenses.len() {
        return Err(format!(
            "the gate returned {verdicts} verdicts for {} lenses; an admission gate with \
             no answer for a lens cannot admit or park it",
            report.card.lenses.len()
        )
        .into());
    }
    let carried: BTreeSet<u16> = report
        .card
        .lenses
        .iter()
        .map(|lens| lens.slot.get())
        .collect();
    let leaked: Vec<u16> = withheld.intersection(&carried).copied().collect();
    println!("   withheld slots present on the card: {leaked:?} (must be empty)");
    if !leaked.is_empty() {
        return Err(format!("withheld slot(s) {leaked:?} still entered the card").into());
    }

    // --- D. the Assay row is on disk, not merely returned -------------------
    println!("\n=== D. physical readback");
    println!("   assay_cf_rows after the pass = {}", report.assay_cf_rows);
    if report.assay_cf_rows == 0 {
        return Err(
            "the card reported success with zero Assay CF rows on disk; the \
                    persisted row is the evidence, not the return value"
                .into(),
        );
    }

    println!(
        "\nPASS: the capability card is reachable, guarded, and its verdicts cover every lens."
    );
    Ok(())
}

//! Full-state verification for #1959 asks 1 and 3: `bits` and `synergy` consult
//! the anchor source declaration and **annotate** what they find, and the
//! annotation survives the whole way to the `storage` facade DTOs.
//!
//! ## Why annotate here and refuse on `sufficiency`
//!
//! A `sufficiency` verdict *launders* a label carrier: `sufficient=true,
//! deficit_bits=0` is one number with no per-slot breakdown, so a circular
//! result is indistinguishable from a real one (#1953, #1958). The capability
//! card has the same shape and takes the same refusal (#1959 ask 2).
//!
//! `bits` does not launder it — it reports each lens separately, so a carrier
//! *can* be noticed. But "a human can notice an implausibly exact number" is
//! precisely the standard that failed on #1958, and `synergy` is worse than
//! `bits`: a pair that includes a carrier reports interaction bits over a
//! concatenated column that contains the label, with no per-slot row to inspect
//! at all. So both surfaces mark the carrier rather than refuse — refusing would
//! remove the tool you use to *inspect* a carrier — and both carry
//! `anchor_source_declared`, so an unchecked report is distinguishable from a
//! clean one.
//!
//! ## What it proves, against the real corpus
//!
//! | phase | claim |
//! |---|---|
//! | A | `bits` marks every declared carrier row, and only those rows |
//! | B | `total_bits` and `total_bits_carrier_free` differ when a carrier is measured |
//! | C | withholding the carriers empties the list and collapses the two totals |
//! | D | `synergy` marks every pair with a carrier half, and reports a carrier-free headline |
//! | E | edge cases: undeclared anchor, declared-but-empty anchor, all-withheld |
//! | F | the Assay CF row count after each pass, read back from disk |
//!
//! Run against a **frozen copy** of the vault (see the vault-copy recipe):
//!
//! ```text
//! cargo run -p synapse-storage --example anchor_source_annotation_fsv -- <vault-copy-dir>
//! ```

use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::SynapseCalyxAssayParams;
use synapse_calyx::lens_provenance::syn_anchor_source_provenance;
use synapse_storage::Db;

const PANEL: u32 = 1_776_006;
/// Declared with two determining fields, and three slots on this panel read
/// them — the only (anchor, panel) pair in the table with a carrier we can also
/// measure over a real corpus.
const ANCHOR: &str = "synapse:mcp_tool_call_outcome";
/// Declared on this panel with an **empty** determining set: the check runs and
/// finds nothing. Distinguishable from `UNDECLARED_ANCHOR` only by the flag.
const DECLARED_CLEAN_ANCHOR: &str = "synapse:mcp_steering_enabled";
/// Not in the table for this panel at all: the check does **not** run.
const UNDECLARED_ANCHOR: &str = "synapse:agent_end_state";
const MAX_RECORDS: usize = 2_000;
/// Same schema version the daemon opens the vault with.
const SCHEMA_VERSION: u32 = 1;

fn params(anchor: &str, excluded: &BTreeSet<u16>) -> SynapseCalyxAssayParams {
    let mut assay = SynapseCalyxAssayParams::new(PANEL, anchor.to_owned())
        .with_lens_names(synapse_storage::constellations::syn_slot_lens_names());
    assay.max_records = MAX_RECORDS;
    assay.excluded_slots = excluded.clone();
    assay
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: anchor_source_annotation_fsv <vault-copy-dir>")?;
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    println!("anchor_source_annotation_fsv  (#1959 asks 1 and 3)");
    println!("vault  = {}", vault_dir.display());
    println!("panel  = {PANEL}   anchor = {ANCHOR}");

    // The declaration is a pure function of (anchor, panel, excluded), so the
    // expected answer is known before a single record is read. That is what
    // makes this a verification and not an observation.
    let declared = syn_anchor_source_provenance(ANCHOR, PANEL, &BTreeSet::new());
    let expected_carriers: BTreeSet<u16> = declared.carriers.iter().map(|c| c.slot).collect();
    println!("\nexpected carriers from the static declaration: {expected_carriers:?}");
    if expected_carriers.is_empty() {
        return Err(
            "this (anchor, panel) declares no carrier, so every phase below would \
                    pass vacuously"
                .into(),
        );
    }

    // --- A. bits marks every declared carrier row, and only those -----------
    println!("\n=== A. bits, nothing withheld");
    let bits = db.assay_bits_intelligence(&params(ANCHOR, &BTreeSet::new()))?;
    println!(
        "   anchored={} distinct_outcomes={} measurable={} anchor_source_declared={}",
        bits.anchored_records, bits.distinct_outcomes, bits.measurable, bits.anchor_source_declared
    );
    if !bits.anchor_source_declared {
        return Err("bits did not say whether the structural check ran".into());
    }
    if bits.anchored_records == 0 {
        return Err("no anchored record in this corpus; the phases below prove nothing".into());
    }
    for carrier in &bits.anchor_source_carriers {
        println!(
            "   CARRIER slot {:>3}  {:<40} shares {:?}",
            carrier.slot, carrier.lens, carrier.shared_fields
        );
        if carrier.shared_fields.is_empty() {
            return Err(format!(
                "slot {} is reported as a carrier with no shared field; the field set is \
                 the whole reason it is one",
                carrier.slot
            )
            .into());
        }
    }
    let reported_carriers: BTreeSet<u16> =
        bits.anchor_source_carriers.iter().map(|c| c.slot).collect();
    if reported_carriers != expected_carriers {
        return Err(format!(
            "bits reported carriers {reported_carriers:?} but the declaration says \
             {expected_carriers:?}"
        )
        .into());
    }
    let mut measured_carrier_bits = 0.0_f32;
    for slot in &bits.slots {
        let expected = expected_carriers.contains(&slot.slot);
        println!(
            "     slot {:>3}  bits={:.6} state={:<22} carrier={} shared={:?}",
            slot.slot,
            slot.marginal_bits,
            slot.state.as_str(),
            slot.anchor_source_carrier,
            slot.anchor_source_shared_fields
        );
        if slot.anchor_source_carrier != expected {
            return Err(format!(
                "slot {} marked carrier={} but the declaration says {expected}",
                slot.slot, slot.anchor_source_carrier
            )
            .into());
        }
        if slot.anchor_source_carrier != !slot.anchor_source_shared_fields.is_empty() {
            return Err(format!(
                "slot {} has carrier={} but shared_fields={:?}; the mark and its evidence \
                 must agree",
                slot.slot, slot.anchor_source_carrier, slot.anchor_source_shared_fields
            )
            .into());
        }
        if expected && slot.state.is_measured() {
            measured_carrier_bits += slot.marginal_bits;
        }
    }

    // --- B. the two totals differ exactly by the measured carrier bits ------
    println!("\n=== B. total_bits vs total_bits_carrier_free");
    println!(
        "   total_bits={:.6}  carrier_free={:.6}  measured carrier bits={:.6}",
        bits.total_bits, bits.total_bits_carrier_free, measured_carrier_bits
    );
    let delta = bits.total_bits - bits.total_bits_carrier_free;
    if (delta - measured_carrier_bits).abs() > 1e-4 {
        return Err(format!(
            "the two totals differ by {delta:.6} but the measured carrier rows sum to \
             {measured_carrier_bits:.6}; the carrier-free total is not the sum it claims to be"
        )
        .into());
    }
    if measured_carrier_bits > 0.0 && delta <= 0.0 {
        return Err("a carrier was measured yet the carrier-free total is not lower".into());
    }
    println!("   assay_cf_rows_after={}", bits.assay_cf_rows_after);
    if bits.assay_cf_rows_after == 0 {
        return Err("bits reported success with zero Assay CF rows on disk".into());
    }

    // --- C. withholding the carriers empties the list and collapses totals --
    println!("\n=== C. bits with excluded_slots={expected_carriers:?}");
    let withheld = db.assay_bits_intelligence(&params(ANCHOR, &expected_carriers))?;
    println!(
        "   anchor_source_declared={} carriers={:?} total_bits={:.6} carrier_free={:.6}",
        withheld.anchor_source_declared,
        withheld
            .anchor_source_carriers
            .iter()
            .map(|c| c.slot)
            .collect::<Vec<_>>(),
        withheld.total_bits,
        withheld.total_bits_carrier_free
    );
    if !withheld.anchor_source_declared {
        return Err("withholding the carriers must not turn the check off".into());
    }
    if !withheld.anchor_source_carriers.is_empty() {
        return Err("a withheld slot is still reported as a carrier".into());
    }
    if withheld.slots.iter().any(|slot| slot.anchor_source_carrier) {
        return Err("a row is still marked a carrier after every carrier was withheld".into());
    }
    if (withheld.total_bits - withheld.total_bits_carrier_free).abs() > f32::EPSILON {
        return Err("with no carrier present the two totals must be identical".into());
    }
    if withheld
        .slots
        .iter()
        .any(|slot| expected_carriers.contains(&slot.slot))
    {
        return Err("a withheld slot still produced a bits row".into());
    }

    // --- D. synergy marks each carrier pair and reports a clean headline ----
    println!("\n=== D. synergy, nothing withheld");
    let synergy = db.assay_synergy_intelligence(&params(ANCHOR, &BTreeSet::new()))?;
    println!(
        "   anchored={} lenses_paired={} pairs_evaluated={} anchor_source_declared={}",
        synergy.anchored_records,
        synergy.lenses_paired,
        synergy.pairs_evaluated,
        synergy.anchor_source_declared
    );
    println!(
        "   max_gain_bits={:.6}  carrier_free={:.6}  pairs_with_carrier={}",
        synergy.max_gain_bits,
        synergy.max_gain_bits_carrier_free,
        synergy.pairs_with_anchor_source_carrier
    );
    if !synergy.anchor_source_declared {
        return Err("synergy did not say whether the structural check ran".into());
    }
    let synergy_carriers: BTreeSet<u16> = synergy
        .anchor_source_carriers
        .iter()
        .map(|c| c.slot)
        .collect();
    if synergy_carriers != expected_carriers {
        return Err(format!(
            "synergy reported carriers {synergy_carriers:?} but the declaration says \
             {expected_carriers:?}"
        )
        .into());
    }
    let mut counted_carrier_pairs = 0usize;
    for pair in &synergy.pairs {
        let expected: Vec<u16> = expected_carriers
            .iter()
            .copied()
            .filter(|slot| *slot == pair.slot_a || *slot == pair.slot_b)
            .collect();
        if !expected.is_empty() {
            counted_carrier_pairs += 1;
        }
        if pair.anchor_source_carrier_slots != expected {
            return Err(format!(
                "pair ({},{}) reports carrier halves {:?} but the declaration says {expected:?}",
                pair.slot_a, pair.slot_b, pair.anchor_source_carrier_slots
            )
            .into());
        }
        if !expected.is_empty() {
            println!(
                "     pair ({:>3},{:>3})  gain={:.6} state={:<22} CARRIER halves {:?}",
                pair.slot_a, pair.slot_b, pair.gain_bits, pair.state, expected
            );
        }
    }
    if counted_carrier_pairs != synergy.pairs_with_anchor_source_carrier {
        return Err(format!(
            "the report counts {} carrier pairs; the rows show {counted_carrier_pairs}",
            synergy.pairs_with_anchor_source_carrier
        )
        .into());
    }
    if counted_carrier_pairs == 0 {
        return Err(
            "no evaluated pair contains a carrier, so phase D proves nothing on this \
                    corpus"
                .into(),
        );
    }
    // The carrier-free headline must be the max over the pairs that have no
    // carrier half — recomputed here from the rows rather than trusted.
    let recomputed = synergy
        .pairs
        .iter()
        .filter(|pair| pair.state == "measured" && pair.anchor_source_carrier_slots.is_empty())
        .map(|pair| pair.gain_bits)
        .fold(0.0_f32, f32::max);
    println!("   recomputed carrier-free max over the rows = {recomputed:.6}");
    if (recomputed - synergy.max_gain_bits_carrier_free).abs() > f32::EPSILON {
        return Err(format!(
            "max_gain_bits_carrier_free={:.6} but the carrier-free rows max at {recomputed:.6}",
            synergy.max_gain_bits_carrier_free
        )
        .into());
    }
    if synergy.max_gain_bits_carrier_free > synergy.max_gain_bits {
        return Err("the carrier-free maximum exceeds the overall maximum".into());
    }
    println!("   assay_cf_rows_after={}", synergy.assay_cf_rows_after);

    // --- E. edge cases ------------------------------------------------------
    println!("\n=== E. edge cases");

    println!("   E1. undeclared anchor {UNDECLARED_ANCHOR} @ {PANEL}");
    let undeclared = db.assay_bits_intelligence(&params(UNDECLARED_ANCHOR, &BTreeSet::new()))?;
    println!(
        "       anchor_source_declared={} carriers={} anchored={} measurable={}",
        undeclared.anchor_source_declared,
        undeclared.anchor_source_carriers.len(),
        undeclared.anchored_records,
        undeclared.measurable
    );
    if undeclared.anchor_source_declared {
        return Err(format!(
            "{UNDECLARED_ANCHOR} is not in the declaration table for panel {PANEL}, so the \
             check cannot have run"
        )
        .into());
    }
    if !undeclared.anchor_source_carriers.is_empty() {
        return Err("an undeclared anchor cannot have declared carriers".into());
    }

    println!("   E2. declared-but-clean anchor {DECLARED_CLEAN_ANCHOR} @ {PANEL}");
    let clean = db.assay_bits_intelligence(&params(DECLARED_CLEAN_ANCHOR, &BTreeSet::new()))?;
    println!(
        "       anchor_source_declared={} carriers={} anchored={} measurable={}",
        clean.anchor_source_declared,
        clean.anchor_source_carriers.len(),
        clean.anchored_records,
        clean.measurable
    );
    if !clean.anchor_source_declared {
        return Err(format!(
            "{DECLARED_CLEAN_ANCHOR} IS declared on panel {PANEL} with an empty determining \
             set; reporting it as undeclared makes 'checked and clean' indistinguishable \
             from 'never checked', which is #1953's defect exactly"
        )
        .into());
    }
    if !clean.anchor_source_carriers.is_empty() {
        return Err("this anchor declares no determining field, so it can have no carrier".into());
    }
    println!(
        "       E1 vs E2 carry the same empty carrier list and differ only in \
         anchor_source_declared ({} vs {}) -- which is the whole point of the flag",
        undeclared.anchor_source_declared, clean.anchor_source_declared
    );

    println!("   E3. synergy with every carrier withheld");
    let synergy_withheld = db.assay_synergy_intelligence(&params(ANCHOR, &expected_carriers))?;
    println!(
        "       anchor_source_declared={} carriers={} pairs_with_carrier={} max={:.6} \
         carrier_free={:.6}",
        synergy_withheld.anchor_source_declared,
        synergy_withheld.anchor_source_carriers.len(),
        synergy_withheld.pairs_with_anchor_source_carrier,
        synergy_withheld.max_gain_bits,
        synergy_withheld.max_gain_bits_carrier_free
    );
    if !synergy_withheld.anchor_source_declared {
        return Err("withholding the carriers must not turn the synergy check off".into());
    }
    if synergy_withheld.pairs_with_anchor_source_carrier != 0 {
        return Err("a carrier pair survived withholding every carrier".into());
    }
    if (synergy_withheld.max_gain_bits - synergy_withheld.max_gain_bits_carrier_free).abs()
        > f32::EPSILON
    {
        return Err("with no carrier pair the two synergy headlines must be identical".into());
    }
    if synergy_withheld
        .pairs
        .iter()
        .any(|pair| !pair.anchor_source_carrier_slots.is_empty())
    {
        return Err("a pair row is still marked after every carrier was withheld".into());
    }

    // --- F. physical readback ----------------------------------------------
    println!("\n=== F. physical readback");
    println!(
        "   Assay CF rows: bits={} bits_withheld={} synergy={} synergy_withheld={}",
        bits.assay_cf_rows_after,
        withheld.assay_cf_rows_after,
        synergy.assay_cf_rows_after,
        synergy_withheld.assay_cf_rows_after
    );
    if synergy_withheld.assay_cf_rows_after == 0 {
        return Err(
            "the Assay CF is empty after four passes; the persisted rows are the \
                    evidence, not the return values"
                .into(),
        );
    }

    println!(
        "\nPASS: bits and synergy both consult the declaration, mark every carrier they \
         find and only those, and say whether the check ran at all."
    );
    Ok(())
}

//! Full State Verification for #1939 (sparse lenses never reach the
//! intelligence stack) and #1941 (synergy reports a negative `gain_bits`).
//!
//! # The two defects, and why one run proves both
//!
//! Both are failures of the same discipline — reporting a number the physical
//! bytes do not support — measured on the same panel, `syn-mcp-usage-v1`.
//!
//! **#1939.** The panel declares twelve slots. Four of them (84 `route_hash`,
//! 85 `param_shape_hash`, 88 `profile_hash`, 89 `tool_surface_hash`) store
//! `SlotVector::Sparse`, and the corpus loader kept only dense vectors — so
//! every intelligence surface reported `n_lenses = 8` and
//! `c_n2_upper_bound = 28`. Not "four lenses reported dark": four lenses that
//! no surface could observe were missing at all.
//!
//! **#1941.** `gain = pair_bits - max(left_bits, right_bits)` was computed from
//! three estimates that could each come from a *different* instrument, because
//! estimator selection resolves per column. Subtracting a Miller-Madow-corrected
//! contingency-table estimate from a KSG estimate reintroduces exactly the
//! non-cancelling bias KSG is constructed to remove (Kraskov, Stögbauer &
//! Grassberger, Phys. Rev. E 69 066138, 2004), and the difference landed below
//! zero — asserting that measuring two lenses together says *less* about the
//! outcome than one of them alone, which the data-processing inequality forbids.
//!
//! # Source of truth
//!
//! The physical Calyx column families on disk: the `Base` CF for the panel's
//! record set, the per-slot `cf/slot_<id>` CFs for the vectors themselves (a
//! `Base` row decodes every slot to `Absent` by design — #1894), and the `Assay`
//! CF for the persisted `PairGain` rows.
//!
//! The BEFORE state is read straight from those per-slot CFs, independently of
//! any report: the census below classifies each declared slot by decoding the
//! bytes actually stored for it. The AFTER state is what the intelligence
//! surfaces say. They must agree, or a surface is describing a panel that is not
//! on disk.
//!
//! # Invariants checked
//!
//! 1. Every slot the `Base` rows declare appears in the corpus loader's
//!    `slot_states`, with the vector kind the per-slot CF actually holds.
//! 2. `abundance.n_lenses` equals the declared slot count, and
//!    `c_n2_upper_bound = C(n_lenses, 2)`.
//! 3. Every sparse slot is either carried (with a `densified_support` equal to
//!    its measured corpus support) or refused with a named reason. Never absent.
//! 4. Densification is **lossless**: the support the loader densified over
//!    equals the distinct occupied index count read from the per-slot CF.
//! 5. `redundancy.n_lenses` and `pairs_possible` state the panel contract.
//! 6. No synergy pair reports a negative `gain_bits`.
//! 7. Every measured pair names one instrument for all three of its terms.
//! 8. Every unmeasured pair carries a state and a reason — never a bare zero.
//! 9. A floored pair is flagged `monotonicity_floor_applied` and keeps its
//!    unclamped `raw_gain_bits`.
//! 10. The `PairGain` rows are physically in the `Assay` CF afterwards.
//!
//! # Usage
//!
//! ```text
//! cargo run -p synapse-storage --example sparse_lens_synergy_fsv -- <vault-parent-dir> [panel_version] [anchor_kind]
//! ```
//!
//! Writes `Assay` rows, so point it at a COPY of the vault, never at the vault
//! a live daemon holds the writer lock on.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::{Path, PathBuf};

use calyx_aster::cf::{ColumnFamily, slot_key};
use calyx_aster::vault::encode::{decode_constellation_base, decode_slot_vector};
use calyx_core::SlotVector;
use synapse_calyx::{
    SYNAPSE_SYNERGY_MAX_RECORDS, SynapseCalyxAssayParams, SynapseCalyxConfig,
    SynapseCalyxReadOnlyVault,
};
use synapse_storage::Db;

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_PANEL_VERSION: u32 = 1_776_006;
const DEFAULT_ANCHOR: &str = "synapse:mcp_tool_call_outcome";

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "FAIL" }
}

/// What one declared slot physically holds, decoded from its own column family.
#[derive(Default)]
struct SlotFacts {
    declared_rows: u64,
    dense: u64,
    sparse: u64,
    multi: u64,
    absent: u64,
    /// Distinct occupied indices across the corpus — the exact width a lossless
    /// densification needs, and the number invariant 4 checks against.
    observed_support: BTreeSet<u32>,
}

impl SlotFacts {
    fn kind(&self) -> &'static str {
        if self.sparse > 0 && self.dense == 0 {
            "sparse"
        } else if self.dense > 0 && self.sparse == 0 {
            "dense"
        } else if self.multi > 0 {
            "multi"
        } else if self.dense > 0 && self.sparse > 0 {
            "MIXED"
        } else {
            "absent"
        }
    }
}

/// Reads the BEFORE state straight from the physical per-slot CFs, with no
/// report in the path.
fn slot_census(
    vault_dir: &Path,
    panel_version: u32,
    max_records: usize,
) -> Result<(BTreeMap<u16, SlotFacts>, usize), Box<dyn Error>> {
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(vault_dir.to_path_buf()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    let mut per_slot: BTreeMap<u16, SlotFacts> = BTreeMap::new();
    let mut panel_rows = 0usize;
    for (_key, value) in vault.scan_cf_at(snapshot, ColumnFamily::Base)? {
        let Ok(constellation) = decode_constellation_base(&value) else {
            continue;
        };
        if constellation.panel_version != panel_version {
            continue;
        }
        panel_rows += 1;
        // The loader is bounded by the same cap, so the census must be too or
        // the two are describing different record sets.
        if panel_rows > max_records {
            continue;
        }
        let key = slot_key(constellation.cx_id);
        for slot_id in constellation.slots.keys() {
            let facts = per_slot.entry(slot_id.get()).or_default();
            facts.declared_rows += 1;
            let Some(bytes) = vault.read_cf_at(snapshot, ColumnFamily::slot(*slot_id), &key)?
            else {
                continue;
            };
            let Ok(vector) = decode_slot_vector(&bytes) else {
                continue;
            };
            match &vector {
                SlotVector::Dense { .. } => facts.dense += 1,
                SlotVector::Sparse { entries, .. } => {
                    facts.sparse += 1;
                    for entry in entries {
                        facts.observed_support.insert(entry.idx);
                    }
                }
                SlotVector::Multi { .. } => facts.multi += 1,
                SlotVector::Absent { .. } => facts.absent += 1,
            }
        }
    }
    Ok((per_slot, panel_rows.min(max_records)))
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear verification narrative: before-state, execute, after-state, per-invariant verdicts"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let parent = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: sparse_lens_synergy_fsv <dir-containing-db-daemon> [panel_version] [anchor] [max_records]")?;
    let panel_version = args
        .next()
        .map_or(Ok(DEFAULT_PANEL_VERSION), |raw| raw.parse::<u32>())?;
    let anchor_kind = args.next().unwrap_or_else(|| DEFAULT_ANCHOR.to_owned());
    // The record cap is a parameter because the property under test does not
    // depend on corpus size. #1941's cross-estimator condition needs only that
    // one lens on the panel routes to KSG while another routes to the discrete
    // plug-in, which holds at any n above the assay sample floor; and #1939's
    // lens count is a property of the panel, not the record set. So the
    // smallest corpus that still clears every estimator's floor proves both,
    // and a debug build can finish it — the KSG path is O(n^2 * d).
    let max_records = args
        .next()
        .map_or(Ok(SYNAPSE_SYNERGY_MAX_RECORDS), |raw| raw.parse::<usize>())?
        .clamp(1, SYNAPSE_SYNERGY_MAX_RECORDS);
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("no db-daemon directory under {}", parent.display()).into());
    }

    println!("sparse_lens_synergy_fsv  (#1939 sparse lenses, #1941 synergy monotonicity)");
    println!("  vault_dir     = {}", vault_dir.display());
    println!("  panel_version = {panel_version}");
    println!("  anchor_kind   = {anchor_kind}");
    println!();

    let mut failures: Vec<String> = Vec::new();

    // ------------------------------------------------------------------
    // BEFORE — the physical per-slot CFs, decoded with no report in the path
    // ------------------------------------------------------------------
    println!("== BEFORE: per-slot CF census, read independently of every report ==");
    let (census, records_measured) = slot_census(&vault_dir, panel_version, max_records)?;
    println!("  records_measured = {records_measured} (cap {max_records})");
    for (slot, facts) in &census {
        println!(
            "  slot={slot:<3} kind={:<7} rows={:<5} dense={:<5} sparse={:<5} absent={:<5} observed_support={}",
            facts.kind(),
            facts.declared_rows,
            facts.dense,
            facts.sparse,
            facts.absent,
            facts.observed_support.len()
        );
    }
    let declared_slots = census.len();
    let sparse_slots: Vec<u16> = census
        .iter()
        .filter(|(_, facts)| facts.sparse > 0)
        .map(|(slot, _)| *slot)
        .collect();
    println!("  declared_slots={declared_slots} sparse_slots={sparse_slots:?}");
    println!();

    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    // ------------------------------------------------------------------
    // EXECUTE — abundance
    // ------------------------------------------------------------------
    println!("== AFTER: abundance ==");
    let abundance = db.abundance_report_intelligence(panel_version, max_records)?;
    println!(
        "  n_lenses={} measurable_lenses={} c_n2_upper_bound={} dda_signal_yield={}",
        abundance.n_lenses,
        abundance.measurable_lenses,
        abundance.c_n2_upper_bound,
        abundance.dda_signal_yield
    );
    for state in &abundance.slot_states {
        println!(
            "  slot={:<3} kind={:<7} measurable={:<5} densified_support={:?}{}",
            state.slot,
            state.kind,
            state.measurable,
            state.densified_support,
            state
                .unusable_reason
                .as_ref()
                .map_or_else(String::new, |reason| format!(" reason={reason}"))
        );
    }

    // --- I1: every declared slot appears, with the kind on disk -----------
    let mut i1 = abundance.slot_states.len() == declared_slots;
    for (slot, facts) in &census {
        let Some(state) = abundance
            .slot_states
            .iter()
            .find(|state| state.slot == *slot)
        else {
            i1 = false;
            failures.push(format!("I1: slot {slot} absent from abundance.slot_states"));
            continue;
        };
        if state.kind != facts.kind() {
            i1 = false;
            failures.push(format!(
                "I1: slot {slot} reported kind={} but the per-slot CF holds {}",
                state.kind,
                facts.kind()
            ));
        }
    }
    println!(
        "  I1 every declared slot present with its on-disk kind ({declared_slots} expected)  {}",
        verdict(i1)
    );

    // --- I2: n_lenses is the panel contract, C(N,2) follows ---------------
    let expected_pairs = declared_slots * declared_slots.saturating_sub(1) / 2;
    let i2 = abundance.n_lenses == declared_slots && abundance.c_n2_upper_bound == expected_pairs;
    println!(
        "  I2 n_lenses={} (expected {declared_slots}) c_n2_upper_bound={} (expected {expected_pairs})  {}",
        abundance.n_lenses,
        abundance.c_n2_upper_bound,
        verdict(i2)
    );
    if !i2 {
        failures.push(format!(
            "I2: n_lenses={} c_n2={} expected {declared_slots} and {expected_pairs}",
            abundance.n_lenses, abundance.c_n2_upper_bound
        ));
    }

    // --- I3/I4: sparse slots carried, and densified losslessly ------------
    let mut i3 = true;
    let mut i4 = true;
    for slot in &sparse_slots {
        let Some(state) = abundance
            .slot_states
            .iter()
            .find(|state| state.slot == *slot)
        else {
            i3 = false;
            failures.push(format!("I3: sparse slot {slot} absent entirely"));
            continue;
        };
        if !state.measurable && state.unusable_reason.is_none() {
            i3 = false;
            failures.push(format!(
                "I3: sparse slot {slot} is not measurable and carries no reason"
            ));
        }
        let measured_support = census.get(slot).map_or(0, |f| f.observed_support.len());
        match state.densified_support {
            Some(support) if support == measured_support => {}
            Some(support) => {
                i4 = false;
                failures.push(format!(
                    "I4: slot {slot} densified over {support} indices but the CF holds {measured_support} distinct occupied indices"
                ));
            }
            None if state.measurable => {
                i4 = false;
                failures.push(format!(
                    "I4: slot {slot} is measurable but reports no densified support"
                ));
            }
            None => {}
        }
    }
    println!(
        "  I3 every sparse slot carried or refused by name ({} sparse)  {}",
        sparse_slots.len(),
        verdict(i3)
    );
    println!(
        "  I4 densified support == distinct occupied indices on disk (lossless)  {}",
        verdict(i4)
    );
    println!();

    // ------------------------------------------------------------------
    // EXECUTE — redundancy
    // ------------------------------------------------------------------
    println!("== AFTER: redundancy ==");
    let mut params = SynapseCalyxAssayParams::new(panel_version, anchor_kind.clone());
    params.max_records = max_records;
    params.lens_names = synapse_storage::constellations::syn_slot_lens_names();
    let redundancy = db.assay_redundancy_intelligence(&params)?;
    println!(
        "  n_lenses={} pairs_possible={} pairs_evaluated={} pairs_skipped={} effective_rank={:.4}",
        redundancy.n_lenses,
        redundancy.pairs_possible,
        redundancy.pairs_evaluated,
        redundancy.pairs_skipped,
        redundancy.effective_rank
    );
    let i5 = redundancy.n_lenses == declared_slots && redundancy.pairs_possible == expected_pairs;
    println!(
        "  I5 redundancy states the panel contract (n_lenses={} pairs_possible={})  {}",
        redundancy.n_lenses,
        redundancy.pairs_possible,
        verdict(i5)
    );
    if !i5 {
        failures.push(format!(
            "I5: redundancy n_lenses={} pairs_possible={} expected {declared_slots} and {expected_pairs}",
            redundancy.n_lenses, redundancy.pairs_possible
        ));
    }
    println!();

    // ------------------------------------------------------------------
    // EXECUTE — synergy
    // ------------------------------------------------------------------
    println!("== AFTER: synergy ==");
    let synergy = db.assay_synergy_intelligence(&params)?;
    println!(
        "  n_lenses={} lenses_paired={} anchored_records={} pairs_evaluated={} pairs_unmeasured={} \
         cross_estimator_unpinnable={} monotonicity_floored={} max_gain_bits={:.6}",
        synergy.n_lenses,
        synergy.lenses_paired,
        synergy.anchored_records,
        synergy.pairs_evaluated,
        synergy.pairs_unmeasured,
        synergy.pairs_cross_estimator_unpinnable,
        synergy.pairs_monotonicity_floored,
        synergy.max_gain_bits
    );
    println!(
        "  slot_a  slot_b  pair_bits   left_bits   right_bits  gain_bits   raw_gain    \
         floored    state/estimators"
    );
    for pair in &synergy.pairs {
        let instruments = match (
            &pair.pair_estimator,
            &pair.left_estimator,
            &pair.right_estimator,
        ) {
            (Some(p), Some(l), Some(r)) => format!("{p}|{l}|{r}"),
            _ => pair.state.clone(),
        };
        println!(
            "  {:<7} {:<7} {:<11.6} {:<11.6} {:<11.6} {:<11.6} {:<11.6} {:<10} {instruments}",
            pair.slot_a,
            pair.slot_b,
            pair.pair_bits,
            pair.left_bits,
            pair.right_bits,
            pair.gain_bits,
            pair.raw_gain_bits,
            pair.monotonicity_floor_applied
        );
    }

    // --- I6: no negative gain, ever --------------------------------------
    let negative: Vec<String> = synergy
        .pairs
        .iter()
        .filter(|pair| pair.gain_bits < 0.0)
        .map(|pair| format!("({},{})={}", pair.slot_a, pair.slot_b, pair.gain_bits))
        .collect();
    let i6 = negative.is_empty();
    println!(
        "  I6 no reported gain_bits is negative (DPI is a law)  {}",
        verdict(i6)
    );
    if !i6 {
        failures.push(format!("I6: negative gains reported: {negative:?}"));
    }

    // --- I7: one instrument per measured pair ----------------------------
    let mut i7 = true;
    for pair in synergy.pairs.iter().filter(|pair| pair.state == "measured") {
        match (
            &pair.pair_estimator,
            &pair.left_estimator,
            &pair.right_estimator,
        ) {
            (Some(p), Some(l), Some(r)) if p == l && l == r => {}
            other => {
                i7 = false;
                failures.push(format!(
                    "I7: measured pair ({},{}) reports mixed or missing instruments: {other:?}",
                    pair.slot_a, pair.slot_b
                ));
            }
        }
    }
    println!(
        "  I7 every measured pair names ONE instrument for all three terms  {}",
        verdict(i7)
    );

    // --- I8: every unmeasured pair carries a state and a reason -----------
    let mut i8 = true;
    for pair in synergy.pairs.iter().filter(|pair| pair.state != "measured") {
        if pair.unmeasured_reason.is_none() {
            i8 = false;
            failures.push(format!(
                "I8: unmeasured pair ({},{}) state={} carries no reason",
                pair.slot_a, pair.slot_b, pair.state
            ));
        }
        if pair.pair_estimator.is_some() {
            i8 = false;
            failures.push(format!(
                "I8: unmeasured pair ({},{}) still names an instrument",
                pair.slot_a, pair.slot_b
            ));
        }
    }
    println!(
        "  I8 every unmeasured pair carries a named reason, never a bare zero  {}",
        verdict(i8)
    );

    // --- I9: a floored pair is flagged and keeps its raw value ------------
    let mut i9 = true;
    for pair in &synergy.pairs {
        let raw = pair.raw_gain_bits;
        let expected_floor = raw < 0.0 && pair.state == "measured";
        if expected_floor != pair.monotonicity_floor_applied {
            i9 = false;
            failures.push(format!(
                "I9: pair ({},{}) raw_gain={raw} but monotonicity_floor_applied={}",
                pair.slot_a, pair.slot_b, pair.monotonicity_floor_applied
            ));
        }
        if pair.monotonicity_floor_applied && (pair.gain_bits != 0.0 || !pair.provisional) {
            i9 = false;
            failures.push(format!(
                "I9: floored pair ({},{}) reports gain={} provisional={}",
                pair.slot_a, pair.slot_b, pair.gain_bits, pair.provisional
            ));
        }
    }
    println!(
        "  I9 a floored pair is flagged, zeroed, provisional, and keeps raw_gain_bits  {}",
        verdict(i9)
    );
    println!();

    // ------------------------------------------------------------------
    // INDEPENDENT READBACK — the Assay CF bytes, through a fresh handle
    // ------------------------------------------------------------------
    println!("== INDEPENDENT READBACK: the physical Assay CF ==");
    let readback = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(vault_dir.clone()),
        None,
    )?;
    let assay_rows = readback
        .scan_cf_at(readback.latest_seq(), ColumnFamily::Assay)?
        .len();
    let expected_rows = synergy.assay_cf_rows_after;
    let i10 = assay_rows == expected_rows;
    println!(
        "  Assay CF rows via a handle that never held the writer lock = {assay_rows} \
         (report said {expected_rows})  {}",
        verdict(i10)
    );
    if !i10 {
        failures.push(format!(
            "I10: independent Assay readback {assay_rows} != reported {expected_rows}"
        ));
    }
    println!();

    if failures.is_empty() {
        println!("RESULT: every invariant held against the physical Calyx CFs.");
        Ok(())
    } else {
        for failure in &failures {
            println!("FAILURE: {failure}");
        }
        Err(format!("{} invariant(s) failed", failures.len()).into())
    }
}

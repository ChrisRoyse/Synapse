//! Manual Full State Verification for the `syn-episode-v1` weave's zero-norm
//! refusal (#2076).
//!
//! Reads the live episode panel **read-only**, hydrates every declared slot
//! vector from its per-slot column family exactly as
//! `load_panel_dense_corpus_in_window` does, and then runs the production
//! `LoomStore::materialize_plan` over the reconstructed corpus with the same
//! plan the unattended maintainer builds (`plan_cross_terms` under a
//! `StaticPairGainGate { gain_bits: 0.0 }`, with equal-dimension demotion).
//!
//! It answers the two questions #2076 asks, on real rows rather than on a
//! model of them:
//!
//! 1. **Why do zero-norm vectors reach the weave at all?** The per-slot census
//!    below reports, for every slot the panel declares, how many rows encode to
//!    a vector of norm exactly zero and what width those vectors are. A
//!    one-dimensional `[0.0]` is a mean-0 z-score lens reporting its own zero
//!    point — an attainable, exactly-encoded measurement, not a gap.
//! 2. **Does one such row still abort the whole panel?** The pass reports the
//!    cross-terms it inserted, the pairs it skipped, and — the number that
//!    decides it — how many records would have aborted the pass under the
//!    pre-fix `?`, versus how many records actually wove.
//!
//! Nothing is written and the writer lock is never taken.
//!
//! Usage:
//! `cargo run -p synapse-storage --example episode_weave_zero_norm_fsv -- <vault_dir> [panel_version]`

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    path::PathBuf,
};

use calyx_aster::{
    cf::{ColumnFamily, KeyRange, slot_key},
    vault::encode::{decode_constellation_base, decode_slot_vector},
};
use calyx_core::{SlotId, SlotVector};
use calyx_loom::{
    LoomStore, MaterializationAction, StaticPairGainGate, agreement_scalar, plan_cross_terms,
};
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault};
use synapse_storage::constellations::SYN_EPISODE_PANEL_VERSION;

/// Matches the unattended weave's cache sizing closely enough that the
/// materialization decisions are the ones production makes.
const CACHE_CAPACITY: usize = 4_096;
/// Bounds the read so a live 14 GB vault stays a minutes-long FSV.
const MAX_RECORDS: usize = 4_000;
/// Rows per bounded `Base` page.
const BASE_PAGE_ROWS: usize = 4_096;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .ok_or("usage: episode_weave_zero_norm_fsv <vault_dir> [panel_version]")?,
    );
    let panel_version = match args.next() {
        Some(value) => value.parse::<u32>()?,
        None => SYN_EPISODE_PANEL_VERSION,
    };

    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(dir.clone()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    println!(
        "SOURCE_OF_TRUTH vault={} mode=read_only vault_id={} snapshot={snapshot} \
         panel_version={panel_version}",
        dir.display(),
        vault.vault_id()
    );

    #[derive(Default)]
    struct SlotCensus {
        rows: u64,
        zero_norm_rows: u64,
        dims: BTreeSet<u32>,
        zero_norm_dims: BTreeSet<usize>,
    }
    let mut per_slot: BTreeMap<u16, SlotCensus> = BTreeMap::new();
    let mut records: Vec<(calyx_core::CxId, BTreeMap<SlotId, Vec<f32>>)> = Vec::new();
    let mut panel_rows = 0_u64;

    // Paged, not materialized: the live `Base` CF is far past the read-only
    // vault's 1 GiB aggregate level-merge budget, so a whole-CF scan fails
    // closed with `CALYX_ASTER_SCAN_MEMORY_BUDGET`. This is the bounded
    // range-page API that refusal names.
    let range = KeyRange::all();
    let mut cursor: Option<Vec<u8>> = None;
    let mut base_rows_visited = 0_u64;
    'scan: loop {
        let page = vault.scan_cf_range_page_latest(
            ColumnFamily::Base,
            &range,
            cursor.as_deref(),
            BASE_PAGE_ROWS,
        )?;
        for (_key, value) in &page.rows {
            base_rows_visited += 1;
            let Ok(constellation) = decode_constellation_base(value) else {
                continue;
            };
            if constellation.panel_version != panel_version {
                continue;
            }
            panel_rows += 1;
            if records.len() >= MAX_RECORDS {
                break 'scan;
            }
            let key = slot_key(constellation.cx_id);
            let mut slots: BTreeMap<SlotId, Vec<f32>> = BTreeMap::new();
            for slot_id in constellation.slots.keys() {
                let Some(bytes) = vault.read_cf_at(snapshot, ColumnFamily::slot(*slot_id), &key)?
                else {
                    continue;
                };
                let Ok(vector) = decode_slot_vector(&bytes) else {
                    continue;
                };
                // Only the dense lane participates in the within-record
                // agreement cross-term, which is the lane #2076's refusal came
                // from.
                let SlotVector::Dense { dim, data } = vector else {
                    continue;
                };
                let census = per_slot.entry(slot_id.get()).or_default();
                census.rows += 1;
                census.dims.insert(dim);
                let norm = data.iter().map(|value| value * value).sum::<f32>().sqrt();
                if norm == 0.0 {
                    census.zero_norm_rows += 1;
                    census.zero_norm_dims.insert(data.len());
                }
                slots.insert(*slot_id, data);
            }
            if !slots.is_empty() {
                records.push((constellation.cx_id, slots));
            }
        }
        if !page.more {
            break;
        }
        cursor = page.resume_after;
    }
    println!("BASE_SCAN base_rows_visited={base_rows_visited} panel_rows={panel_rows}");

    println!(
        "PANEL_CENSUS panel_rows={panel_rows} records_loaded={} max_records={MAX_RECORDS}",
        records.len()
    );
    for (slot, census) in &per_slot {
        println!(
            "SLOT_CENSUS slot={slot} dense_rows={} zero_norm_rows={} dims={:?} zero_norm_widths={:?}",
            census.rows, census.zero_norm_rows, census.dims, census.zero_norm_dims
        );
    }

    // The production plan, rebuilt: eager agreement only, and any pair whose two
    // slots differ in dimension demoted to lazy so the pass never fails closed
    // on a panel that mixes lens output shapes.
    let gate = StaticPairGainGate { gain_bits: 0.0 };
    let mut store = LoomStore::new(CACHE_CAPACITY);
    let mut records_woven = 0_usize;
    let mut records_with_zero_norm_pair = 0_usize;
    let mut eager_pairs = 0_usize;
    let mut pre_fix_aborting_records = 0_usize;

    for (cx_id, slots) in &records {
        if slots.len() < 2 {
            continue;
        }
        let mut slot_ids: Vec<SlotId> = slots.keys().copied().collect();
        slot_ids.sort_unstable();
        let mut plan = plan_cross_terms(&slot_ids, &gate);
        for entry in &mut plan.entries {
            if entry.action == MaterializationAction::EagerStore
                && slots.get(&entry.a).map(Vec::len) != slots.get(&entry.b).map(Vec::len)
            {
                entry.action = MaterializationAction::LazyCache;
            }
        }

        // What the pre-#2076 code would have done: the first eager agreement
        // pair over a zero-norm operand returned `Err` and `materialize_plan`
        // propagated it with `?`, aborting the entire panel weave.
        let mut aborts_pre_fix = false;
        for entry in &plan.entries {
            if entry.action != MaterializationAction::EagerStore {
                continue;
            }
            eager_pairs += 1;
            let (Some(left), Some(right)) = (slots.get(&entry.a), slots.get(&entry.b)) else {
                continue;
            };
            if agreement_scalar(left, right).is_err() {
                aborts_pre_fix = true;
            }
        }
        if aborts_pre_fix {
            pre_fix_aborting_records += 1;
        }

        let before = store.zero_norm_agreement_skip_total();
        store.materialize_plan(panel_version, *cx_id, slots, &plan)?;
        if store.zero_norm_agreement_skip_total() > before {
            records_with_zero_norm_pair += 1;
        }
        records_woven += 1;
    }

    println!(
        "WEAVE_PASS records_woven={records_woven} eager_pairs_planned={eager_pairs} \
         cross_terms_materialized={} zero_norm_skips={} records_with_zero_norm_pair={records_with_zero_norm_pair}",
        store.xterm_count(),
        store.zero_norm_agreement_skip_total()
    );
    println!(
        "PRE_FIX_COMPARISON records_that_would_have_aborted_the_panel={pre_fix_aborting_records} \
         panel_outcome_pre_fix={} panel_outcome_now=woven",
        if pre_fix_aborting_records > 0 {
            "CALYX_LOOM_ZERO_NORM_VECTOR (zero rows)"
        } else {
            "woven"
        }
    );
    for skip in store.zero_norm_agreement_skips().iter().take(8) {
        println!(
            "ZERO_NORM_SKIP cx_id={} a_slot={} b_slot={} side={:?}",
            skip.cx_id,
            skip.a.slot_id.get(),
            skip.b.slot_id.get(),
            skip.zero_side
        );
    }

    if store.xterm_count() == 0 {
        return Err("weave produced no cross-terms: the panel still cannot weave".into());
    }
    println!(
        "VERDICT weave_completed=true cross_terms={}",
        store.xterm_count()
    );
    Ok(())
}

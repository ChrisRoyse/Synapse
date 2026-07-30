//! Measuring a free-text query into per-slot query vectors.
//!
//! # Why the gate is not the modality tag (issue #1896)
//!
//! This module used to admit a slot into the text-query set when its declared
//! [`calyx_core::Modality`] was `Text`. That tag is far coarser than the
//! question being asked: a one-hot encoder over a closed vocabulary, a
//! cyclic-time encoder over a timestamp, a numeric scalar and a sparse text
//! encoder can all carry the same tag, and on the Synapse panels every one of
//! the 78 declared lenses is tagged `Structured`. The result was that
//! `query_vectors` was *always* empty on every Synapse panel and `by_text`
//! recall was unreachable by construction, no matter what had been indexed.
//!
//! The gate now asks the lens itself, through [`calyx_core::Lens::text_queryable`],
//! which is declared per encoder. Two invariants follow:
//!
//! * **Symmetry.** The query is measured with the *slot's own* modality and the
//!   slot's own lens, so it is produced by exactly the operation that produced
//!   the stored vectors. That symmetry is what makes a lexical or dense lane
//!   comparable at all; measuring a query under a different modality than the
//!   documents is not a stricter check, it is a different measurement.
//! * **Refusal is still real.** A lens that cannot answer text is still not
//!   consulted. The point was never to widen the gate to everything — measuring
//!   the literal word "focus_change" through a timestamp encoder would be
//!   meaningless, and a ranking built from it worse than an honest failure.
//!
//! Every slip through the gate is recorded with its reason in
//! [`QueryMeasurement::skipped`], so an empty query set names *why* no slot
//! qualified rather than reporting only that nothing was produced.

use std::collections::BTreeSet;

use calyx_core::{CalyxError, SlotId, SlotVector};

use crate::engine_trace::SearchTracer;
use crate::error::CliResult;

/// Reason code for a panel slot that did not contribute a text query vector.
pub const QUERY_SKIP_SLOT_NOT_ACTIVE: &str = "slot_not_active";
/// The caller restricted the query to a physical slot set that excludes this slot.
pub const QUERY_SKIP_NOT_SELECTED: &str = "not_selected_by_caller";
/// The panel references a lens id this registry does not hold.
pub const QUERY_SKIP_LENS_NOT_REGISTERED: &str = "lens_not_registered";
/// The lens declares that it cannot answer a free-text query.
pub const QUERY_SKIP_LENS_NOT_TEXT_QUERYABLE: &str = "lens_not_text_queryable";
/// The lens measured the query but produced nothing an index can probe with.
pub const QUERY_SKIP_VECTOR_NOT_INDEXABLE: &str = "query_vector_not_indexable";

/// One panel slot that did not contribute a query vector, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuerySlotSkip {
    pub slot: SlotId,
    pub slot_key: String,
    pub reason: &'static str,
    pub detail: String,
}

/// The full outcome of measuring one text query across a panel's slots.
///
/// The counts exist to separate three failures that previously produced the
/// same empty result: no lens is declared text-queryable, the text-queryable
/// lens produced nothing indexable, and the caller's slot restriction excluded
/// it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryMeasurement {
    pub vectors: Vec<(SlotId, SlotVector)>,
    /// Slots declared on the panel, whatever their state.
    pub panel_slots: usize,
    /// Panel slots in `SlotState::Active`.
    pub active_slots: usize,
    /// Active slots the caller's restriction (if any) allowed.
    pub considered_slots: usize,
    /// Considered slots whose lens declares itself text-queryable.
    pub text_queryable_slots: usize,
    /// Text-queryable slots that produced an indexable query vector.
    pub indexable_slots: usize,
    pub skipped: Vec<QuerySlotSkip>,
}

impl QueryMeasurement {
    /// A single-line, operator-readable summary of why the query set is what it
    /// is. Always names the numbers, never only the outcome.
    #[must_use]
    pub fn diagnostic(&self) -> String {
        let skipped = if self.skipped.is_empty() {
            "none".to_owned()
        } else {
            self.skipped
                .iter()
                .map(|skip| {
                    format!(
                        "{}({})={}:{}",
                        skip.slot, skip.slot_key, skip.reason, skip.detail
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        format!(
            "panel_slots={} active_slots={} considered_slots={} text_queryable_slots={} indexable_slots={} skipped=[{}]",
            self.panel_slots,
            self.active_slots,
            self.considered_slots,
            self.text_queryable_slots,
            self.indexable_slots,
            skipped
        )
    }
}

/// Measure the query through every active text-queryable lens materialized in
/// the registry, keeping only indexable vectors.
pub fn measure_query_vectors(
    state: &calyx_registry::VaultPanelState,
    query: &str,
) -> CliResult<Vec<(SlotId, SlotVector)>> {
    measure_query_vectors_with_slots(state, query, None)
}

/// Measure query vectors for active text-queryable slots, optionally restricted
/// to a caller-selected physical slot set.
pub fn measure_query_vectors_with_slots(
    state: &calyx_registry::VaultPanelState,
    query: &str,
    allowed_slots: Option<&BTreeSet<SlotId>>,
) -> CliResult<Vec<(SlotId, SlotVector)>> {
    Ok(measure_query(state, query, allowed_slots)?.vectors)
}

/// Measure query vectors and return the full per-slot accounting alongside them.
pub fn measure_query(
    state: &calyx_registry::VaultPanelState,
    query: &str,
    allowed_slots: Option<&BTreeSet<SlotId>>,
) -> CliResult<QueryMeasurement> {
    measure_query_traced(state, query, allowed_slots, None)
}

pub(crate) fn measure_query_vectors_with_slots_traced(
    state: &calyx_registry::VaultPanelState,
    query: &str,
    allowed_slots: Option<&BTreeSet<SlotId>>,
    trace: Option<&mut SearchTracer<'_>>,
) -> CliResult<Vec<(SlotId, SlotVector)>> {
    Ok(measure_query_traced(state, query, allowed_slots, trace)?.vectors)
}

pub(crate) fn measure_query_traced(
    state: &calyx_registry::VaultPanelState,
    query: &str,
    allowed_slots: Option<&BTreeSet<SlotId>>,
    trace: Option<&mut SearchTracer<'_>>,
) -> CliResult<QueryMeasurement> {
    use calyx_core::{Input, SlotState};
    let mut noop_trace;
    let trace = match trace {
        Some(trace) => trace,
        None => {
            noop_trace = SearchTracer::new(None);
            &mut noop_trace
        }
    };
    trace.emit_detail(
        "query.measure.start",
        None,
        Some(state.panel.slots.len()),
        Some(format!("bytes={}", query.len())),
    );

    let mut out = QueryMeasurement {
        panel_slots: state.panel.slots.len(),
        ..QueryMeasurement::default()
    };
    for slot in &state.panel.slots {
        let slot_key = slot.slot_key.key().to_string();
        if slot.state != SlotState::Active {
            out.skipped.push(QuerySlotSkip {
                slot: slot.slot_id,
                slot_key,
                reason: QUERY_SKIP_SLOT_NOT_ACTIVE,
                detail: format!("state={:?}", slot.state),
            });
            continue;
        }
        out.active_slots += 1;
        if allowed_slots.is_some_and(|allowed| !allowed.contains(&slot.slot_id)) {
            out.skipped.push(QuerySlotSkip {
                slot: slot.slot_id,
                slot_key,
                reason: QUERY_SKIP_NOT_SELECTED,
                detail: "not in the caller's persisted-index slot set".to_owned(),
            });
            continue;
        }
        out.considered_slots += 1;
        let Some(text_queryable) = state.registry.text_queryable(slot.lens_id) else {
            out.skipped.push(QuerySlotSkip {
                slot: slot.slot_id,
                slot_key,
                reason: QUERY_SKIP_LENS_NOT_REGISTERED,
                detail: format!("lens {} is not held by this registry", slot.lens_id),
            });
            continue;
        };
        if !text_queryable {
            out.skipped.push(QuerySlotSkip {
                slot: slot.slot_id,
                slot_key,
                reason: QUERY_SKIP_LENS_NOT_TEXT_QUERYABLE,
                detail: format!(
                    "lens {} declares it cannot measure a free-text query (modality={:?})",
                    slot.lens_id, slot.modality
                ),
            });
            continue;
        }
        out.text_queryable_slots += 1;

        trace.emit_detail(
            "query.measure_slot.start",
            Some(slot.slot_id),
            None,
            Some(slot.lens_id.to_string()),
        );
        // The slot's own modality, not a hardcoded `Modality::Text`: the query
        // must be measured by the same operation that measured the stored
        // bytes, or the two vectors are not comparable (#1896).
        let input = Input::new(slot.modality, query.as_bytes().to_vec());
        let vector = match state.registry.measure(slot.lens_id, &input) {
            Ok(vector) => vector,
            Err(error) => {
                trace.emit_detail(
                    "query.measure_slot.error",
                    Some(slot.slot_id),
                    None,
                    Some(format!("{} {}", error.code, error.message)),
                );
                return Err(error.into());
            }
        };
        let is_indexable = vector.is_indexable();
        trace.emit_detail(
            "query.measure_slot.done",
            Some(slot.slot_id),
            Some(is_indexable as usize),
            Some(slot_vector_shape(&vector)),
        );
        if is_indexable {
            out.indexable_slots += 1;
            out.vectors.push((slot.slot_id, vector));
        } else {
            out.skipped.push(QuerySlotSkip {
                slot: slot.slot_id,
                slot_key,
                reason: QUERY_SKIP_VECTOR_NOT_INDEXABLE,
                detail: slot_vector_shape(&vector),
            });
        }
    }
    trace.emit("query.measure.done", None, Some(out.vectors.len()));
    Ok(out)
}

pub(crate) fn no_indexable_query_vectors() -> CalyxError {
    CalyxError::stale_derived(
        "search has no indexable query vectors from active text-queryable lenses; re-enable a concrete lens or remeasure the panel",
    )
}

pub(crate) fn slot_vector_shape(vector: &SlotVector) -> String {
    match vector {
        SlotVector::Dense { dim, data } => format!("dense dim={dim} len={}", data.len()),
        SlotVector::Sparse { dim, entries } => format!("sparse dim={dim} nnz={}", entries.len()),
        SlotVector::Multi { token_dim, tokens } => {
            format!("multi token_dim={token_dim} tokens={}", tokens.len())
        }
        SlotVector::Absent { reason } => format!("absent reason={reason:?}"),
    }
}

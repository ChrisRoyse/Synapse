use std::collections::BTreeSet;

use calyx_core::{CalyxError, SlotId, SlotVector};

use crate::engine_trace::SearchTracer;
use crate::error::CliResult;

/// Measure the query through every active text lens that is materialized in the
/// registry, keeping only indexable vectors.
pub fn measure_query_vectors(
    state: &calyx_registry::VaultPanelState,
    query: &str,
) -> CliResult<Vec<(SlotId, SlotVector)>> {
    measure_query_vectors_with_slots(state, query, None)
}

/// Measure query vectors for active text slots, optionally restricted to a
/// caller-selected physical slot set.
pub fn measure_query_vectors_with_slots(
    state: &calyx_registry::VaultPanelState,
    query: &str,
    allowed_slots: Option<&BTreeSet<SlotId>>,
) -> CliResult<Vec<(SlotId, SlotVector)>> {
    measure_query_vectors_with_slots_traced(state, query, allowed_slots, None)
}

pub(crate) fn measure_query_vectors_with_slots_traced(
    state: &calyx_registry::VaultPanelState,
    query: &str,
    allowed_slots: Option<&BTreeSet<SlotId>>,
    trace: Option<&mut SearchTracer<'_>>,
) -> CliResult<Vec<(SlotId, SlotVector)>> {
    use calyx_core::{Input, Modality, SlotState};
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
    let input = Input::new(Modality::Text, query.as_bytes().to_vec());
    let mut out = Vec::new();
    for slot in &state.panel.slots {
        if allowed_slots.is_some_and(|allowed| !allowed.contains(&slot.slot_id)) {
            continue;
        }
        if slot.state == SlotState::Active
            && slot.modality == Modality::Text
            && state.registry.contains(slot.lens_id)
        {
            trace.emit_detail(
                "query.measure_slot.start",
                Some(slot.slot_id),
                None,
                Some(slot.lens_id.to_string()),
            );
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
                out.push((slot.slot_id, vector));
            }
        }
    }
    trace.emit("query.measure.done", None, Some(out.len()));
    Ok(out)
}

pub(crate) fn no_indexable_query_vectors() -> CalyxError {
    CalyxError::stale_derived(
        "search has no indexable query vectors from active text lenses; re-enable a concrete lens or remeasure the panel",
    )
}

pub(crate) fn no_indexable_stored_vectors() -> CalyxError {
    CalyxError::stale_derived(
        "search has no indexable stored slot vectors matching active query lenses; reingest or backfill stale slot rows",
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

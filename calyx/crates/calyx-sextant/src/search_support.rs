//! Small pure helpers for `search.rs`.

use std::collections::BTreeMap;

use calyx_core::{Anchor, Constellation, Panel, Result, SlotId, SlotVector};

use crate::fusion::FusionStrategy;
use crate::index::tokenizer::{TEXT_SPARSE_DIM, text_sparse_entries};
use crate::query::{AnchorPredicate, MetadataPredicate, ScalarOp, ScalarPredicate};

pub(crate) fn scalar_matches(cx: &Constellation, filter: &ScalarPredicate) -> bool {
    cx.scalars
        .get(&filter.name)
        .is_some_and(|actual| compare_scalar(*actual, filter.op, filter.value))
}

pub(crate) fn anchor_filter_matches(cx: &Constellation, filter: &AnchorPredicate) -> bool {
    cx.anchors
        .iter()
        .any(|anchor| anchor_matches(anchor, filter))
}

pub(crate) fn metadata_matches(cx: &Constellation, filter: &MetadataPredicate) -> bool {
    match filter {
        MetadataPredicate::Vault(vault) => &cx.vault_id == vault,
        MetadataPredicate::Modality(modality) => &cx.modality == modality,
        MetadataPredicate::PanelVersion(panel_version) => cx.panel_version == *panel_version,
        MetadataPredicate::CreatedAt { min, max } => {
            min.is_none_or(|value| cx.created_at >= value)
                && max.is_none_or(|value| cx.created_at <= value)
        }
        MetadataPredicate::InputRedacted(redacted) => cx.input_ref.redacted == *redacted,
        MetadataPredicate::InputPointerContains(fragment) => cx
            .input_ref
            .pointer
            .as_ref()
            .is_some_and(|pointer| pointer.contains(fragment)),
    }
}

pub(crate) fn default_strategy(slots: &[SlotId]) -> FusionStrategy {
    if slots.len() == 1 {
        FusionStrategy::SingleLens { slot: slots[0] }
    } else {
        FusionStrategy::Rrf
    }
}

pub(crate) fn strategy_weights(
    strategy: &FusionStrategy,
    panel: &Panel,
) -> Result<BTreeMap<SlotId, f32>> {
    match strategy {
        FusionStrategy::WeightedRrf { profile } => {
            Ok(crate::fusion::profiles::lookup(*profile, panel)?.weights)
        }
        _ => Ok(BTreeMap::new()),
    }
}

pub(crate) fn text_to_sparse(text: &str) -> SlotVector {
    SlotVector::Sparse {
        dim: TEXT_SPARSE_DIM,
        entries: text_sparse_entries(text),
    }
}

fn compare_scalar(actual: f64, op: ScalarOp, expected: f64) -> bool {
    if !actual.is_finite() || !expected.is_finite() {
        return false;
    }
    match op {
        ScalarOp::Eq => actual == expected,
        ScalarOp::Gt => actual > expected,
        ScalarOp::Gte => actual >= expected,
        ScalarOp::Lt => actual < expected,
        ScalarOp::Lte => actual <= expected,
    }
}

fn anchor_matches(anchor: &Anchor, filter: &AnchorPredicate) -> bool {
    if anchor.kind != filter.kind {
        return false;
    }
    if let Some(value) = &filter.value
        && &anchor.value != value
    {
        return false;
    }
    if let Some(min_confidence) = filter.min_confidence
        && (!min_confidence.is_finite() || anchor.confidence < min_confidence)
    {
        return false;
    }
    if let Some(source) = &filter.source
        && &anchor.source != source
    {
        return false;
    }
    true
}

use std::collections::BTreeMap;

use calyx_core::{CalyxError, Panel, SlotId, SlotVector};
use calyx_sextant::{FusionStrategy, RrfProfile, fusion};

use crate::error::CliResult;

pub(crate) fn weights_for(
    strategy: &FusionStrategy,
    panel: &Panel,
    slots: &[SlotId],
) -> CliResult<BTreeMap<SlotId, f32>> {
    let Some(profile) = weighted_profile(strategy) else {
        return Ok(BTreeMap::new());
    };
    let profile_weights = fusion::profiles::lookup(profile, panel)?.weights;
    let missing = slots
        .iter()
        .find(|slot| !profile_weights.contains_key(slot));
    if let Some(slot) = missing {
        return Err(CalyxError {
            code: calyx_sextant::error::CALYX_SEXTANT_PROFILE_UNBOUND,
            message: format!(
                "RRF profile {profile:?} for panel {} has no declared weight for searched slot {slot}",
                panel.version
            ),
            remediation: "restrict the query to slots bound by the selected profile or declare a matching slot axis in the exact panel contract",
        }
        .into());
    }
    Ok(slots
        .iter()
        .map(|slot| (*slot, profile_weights[slot]))
        .collect())
}

pub(crate) fn stage1_slots(
    strategy: &FusionStrategy,
    query_vectors: &[(SlotId, SlotVector)],
    slots: &[SlotId],
) -> Vec<SlotId> {
    if !matches!(strategy, FusionStrategy::Pipeline) {
        return Vec::new();
    }
    let sparse = query_vectors
        .iter()
        .filter_map(|(slot, vector)| matches!(vector, SlotVector::Sparse { .. }).then_some(*slot))
        .filter(|slot| slots.contains(slot))
        .collect::<Vec<_>>();
    if sparse.is_empty() {
        slots.first().copied().into_iter().collect()
    } else {
        sparse
    }
}

fn weighted_profile(strategy: &FusionStrategy) -> Option<RrfProfile> {
    match strategy {
        FusionStrategy::WeightedRrf { profile } => Some(*profile),
        _ => None,
    }
}

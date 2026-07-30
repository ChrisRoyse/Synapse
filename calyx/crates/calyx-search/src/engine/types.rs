use std::collections::{BTreeMap, BTreeSet};

use calyx_core::{Constellation, CxId, SlotId};
use calyx_sextant::{DroppedGuardHit, FusionStrategy, Hit, RrfProfile};

use crate::persisted::PersistedSearchGeneration;

use crate::error::{CliResult, SearchError};

/// Fusion strategy choice (transport-agnostic; the CLI flag parser and the HTTP
/// request both map onto this, then it resolves to a concrete `FusionStrategy`
/// against the live slot set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FusionChoice {
    Rrf,
    WeightedRrf,
    WeightedRrfProfile(RrfProfile),
    SingleLens,
    SingleLensSlot(SlotId),
    KernelFirst,
    Pipeline,
}

/// What a resolved fusion choice means for the caller (#1913).
///
/// `SingleLensSlot` used to answer this question with a bare `Result`, which
/// forced three genuinely different situations into one error sentence. The
/// third one is not an error at all, so the type has to be able to say so.
#[derive(Clone, Debug, PartialEq)]
pub enum FusionResolution {
    /// Fuse with this strategy.
    Ready(FusionStrategy),
    /// The requested lens exists and this query measured into it, but it
    /// returned no candidate. That is a miss, not a fault — the same call the
    /// fused path already makes when no slot produces candidates.
    NoMatch,
}

impl FusionChoice {
    /// Resolves a fusion choice against what the generation holds and what this
    /// query could actually measure.
    ///
    /// `scored_slots` are the slots that produced candidates; `measured_slots`
    /// are the slots this query produced a vector for; `generation_slots` are
    /// the slots the persisted generation holds. All three are needed because
    /// `scored_slots` alone cannot distinguish "no such lens", "this query kind
    /// cannot reach that lens", and "that lens matched nothing" (#1913) — and
    /// blaming the index for the second and third sent operators to rebuild a
    /// healthy generation.
    pub fn to_strategy(
        self,
        scored_slots: &[SlotId],
        measured_slots: &BTreeSet<SlotId>,
        generation: &PersistedSearchGeneration,
    ) -> CliResult<FusionResolution> {
        match self {
            Self::Rrf => Ok(FusionResolution::Ready(FusionStrategy::Rrf)),
            Self::WeightedRrf => Ok(FusionResolution::Ready(FusionStrategy::WeightedRrf {
                profile: RrfProfile::General,
            })),
            Self::WeightedRrfProfile(profile) => {
                Ok(FusionResolution::Ready(FusionStrategy::WeightedRrf {
                    profile,
                }))
            }
            Self::SingleLens => scored_slots
                .first()
                .copied()
                .map(|slot| FusionResolution::Ready(FusionStrategy::SingleLens { slot }))
                .ok_or_else(|| SearchError::usage("single-lens search has no active lens slot")),
            Self::SingleLensSlot(slot) => {
                if scored_slots.contains(&slot) {
                    return Ok(FusionResolution::Ready(FusionStrategy::SingleLens { slot }));
                }
                let held = generation
                    .slots
                    .iter()
                    .find(|persisted| persisted.panel_slot.slot_id() == slot);
                match (held, measured_slots.contains(&slot)) {
                    // Measured into the lane, the lane holds rows, nothing
                    // matched. A miss.
                    (Some(_), true) => Ok(FusionResolution::NoMatch),
                    // The lane exists but this query never probed it: a text
                    // query produces no vector for a dense record-vector
                    // encoder, so there was nothing to search it with. Naming
                    // the lane's kind and what the query DID reach is the
                    // operator's next move; the index is not at fault.
                    (Some(persisted), false) => Err(SearchError::usage(format!(
                        "single-lens search requested slot {slot}, which this generation holds \
                         ({} over {:?}, {} rows), but this query measured no vector for it — a query \
                         only probes lenses its own modality can be encoded into. This query measured \
                         slots {:?}; request one of those, or use a query mode this lens accepts. \
                         The index is not stale.",
                        persisted.kind,
                        persisted.shape,
                        persisted.len,
                        measured_slots.iter().copied().collect::<Vec<_>>(),
                    ))),
                    // Genuinely absent from the generation. The only case the
                    // original message described correctly.
                    (None, _) => Err(SearchError::usage(format!(
                        "single-lens search requested slot {slot}, which this persisted generation \
                         does not hold. It holds slots {:?}.",
                        generation
                            .slots
                            .iter()
                            .map(|persisted| persisted.panel_slot.slot_id())
                            .collect::<Vec<_>>(),
                    ))),
                }
            }
            Self::KernelFirst => Ok(FusionResolution::Ready(FusionStrategy::WeightedRrf {
                profile: RrfProfile::Kernel,
            })),
            Self::Pipeline => Ok(FusionResolution::Ready(FusionStrategy::Pipeline)),
        }
    }
}

/// Guard choice for a search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardChoice {
    Off,
    InRegion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchFreshness {
    Fresh,
    StaleOk,
}

/// The result of a search: ranked hits (each carrying score + stored
/// provenance), the flat operator guard tau actually applied (if any —
/// profile-backed guarding applies per-slot calibrated taus and reports
/// `None` here; the hits carry their guard verdict evidence), and the
/// candidates the profile-backed guard dropped (#1094, MCP parity).
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    pub guard_tau: Option<f32>,
    pub docs: BTreeMap<CxId, Constellation>,
    pub dropped_guard_hits: Vec<DroppedGuardHit>,
    pub generation: Option<PersistedSearchGeneration>,
}

impl SearchOutcome {
    pub(super) fn empty() -> Self {
        Self {
            hits: Vec::new(),
            guard_tau: None,
            docs: BTreeMap::new(),
            dropped_guard_hits: Vec::new(),
            generation: None,
        }
    }

    pub(super) fn empty_with_generation(generation: PersistedSearchGeneration) -> Self {
        Self {
            generation: Some(generation),
            ..Self::empty()
        }
    }
}

/// Fusion parameters an operator can tune per vault.
///
/// Threaded rather than hardcoded so a configured `calyx_fusion_k` actually
/// reaches the scoring law it names. Before #1883 the knob was validated,
/// lowered to an artifact and echoed to health while every fused query scored
/// with a constant, so tuning it changed nothing and reported success.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FusionTuning {
    /// The Reciprocal Rank Fusion rank constant.
    pub rrf_k: f32,
}

impl Default for FusionTuning {
    fn default() -> Self {
        Self {
            rrf_k: DEFAULT_RRF_K_F32,
        }
    }
}

/// The workspace default rank constant in scoring form.
///
/// Resolved once here so no caller repeats the conversion, and so a default
/// that stopped being exactly representable would fail loudly at first use
/// rather than silently round.
const DEFAULT_RRF_K_F32: f32 = match calyx_core::rrf_k_as_f32(calyx_core::RRF_K_DEFAULT) {
    Some(value) => value,
    None => panic!("the workspace default RRF k must be exactly representable in f32"),
};

impl FusionTuning {
    /// Rejects a rank constant that would make RRF scoring undefined or
    /// unfaithfully reported.
    pub fn new(rrf_k: u32) -> calyx_core::Result<Self> {
        let rrf_k = calyx_core::rrf_k_as_f32(rrf_k).ok_or_else(|| {
            calyx_core::CalyxError::fusion_tuning_invalid(format!(
                "fusion rrf_k must be in 1..=2^24 so it is exactly representable in the f32 the scoring law runs in, got {rrf_k}"
            ))
        })?;
        Ok(Self { rrf_k })
    }
}

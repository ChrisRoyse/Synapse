use std::collections::BTreeMap;

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

impl FusionChoice {
    pub fn to_strategy(self, slots: &[SlotId]) -> CliResult<FusionStrategy> {
        match self {
            Self::Rrf => Ok(FusionStrategy::Rrf),
            Self::WeightedRrf => Ok(FusionStrategy::WeightedRrf {
                profile: RrfProfile::General,
            }),
            Self::WeightedRrfProfile(profile) => Ok(FusionStrategy::WeightedRrf { profile }),
            Self::SingleLens => slots
                .first()
                .copied()
                .map(|slot| FusionStrategy::SingleLens { slot })
                .ok_or_else(|| SearchError::usage("single-lens search has no active lens slot")),
            Self::SingleLensSlot(slot) => {
                if slots.contains(&slot) {
                    Ok(FusionStrategy::SingleLens { slot })
                } else {
                    Err(SearchError::usage(format!(
                        "single-lens search requested slot {slot}, but the slot has no active persisted search results"
                    )))
                }
            }
            Self::KernelFirst => Ok(FusionStrategy::WeightedRrf {
                profile: RrfProfile::Kernel,
            }),
            Self::Pipeline => Ok(FusionStrategy::Pipeline),
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

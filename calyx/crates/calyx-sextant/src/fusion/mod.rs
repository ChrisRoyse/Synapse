//! Fusion strategies for Sextant search.

use std::collections::BTreeMap;

use calyx_core::{CxId, LedgerRef, Result, SlotId};
use serde::{Deserialize, Serialize};

use crate::hit::Hit;
use crate::index::IndexSearchHit;

pub mod pipeline;
pub mod profiles;
pub mod rrf;
pub mod single;

pub use pipeline::pipeline_fuse;
pub use profiles::{RrfProfile, WeightedProfile, lookup, weighted_profiles};
pub use rrf::{rrf_fuse, weighted_rrf_fuse};
pub use single::single_lens_fuse;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionStrategy {
    SingleLens { slot: SlotId },
    Rrf,
    WeightedRrf { profile: RrfProfile },
    Pipeline,
}

impl FusionStrategy {
    pub fn name(&self) -> String {
        match self {
            Self::SingleLens { slot } => format!("single_lens:{slot}"),
            Self::Rrf => "rrf".to_string(),
            Self::WeightedRrf { profile } => format!("weighted_rrf:{profile:?}").to_lowercase(),
            Self::Pipeline => "pipeline".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FusionContext {
    pub panel_version: u32,
    pub k: usize,
    /// The RRF rank constant this query scores with. Defaults to
    /// `calyx_core::RRF_K_DEFAULT`; a tuned vault overrides it (issue #1883).
    pub rrf_k: f32,
    pub explain: bool,
    pub strategy: FusionStrategy,
    pub weights: BTreeMap<SlotId, f32>,
    pub stage1_slots: Vec<SlotId>,
}

pub fn fuse(
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    context: &FusionContext,
    provenance: &dyn Fn(CxId) -> Result<LedgerRef>,
) -> Result<Vec<Hit>> {
    match &context.strategy {
        FusionStrategy::SingleLens { slot } => {
            single_lens_fuse(*slot, results, context, provenance)
        }
        FusionStrategy::Rrf => rrf_fuse(results, context, provenance),
        FusionStrategy::WeightedRrf { .. } => weighted_rrf_fuse(results, context, provenance),
        FusionStrategy::Pipeline => pipeline_fuse(results, context, provenance),
    }
}

//! Provenanced search hit types.

use calyx_core::{CxId, LedgerRef, PanelSlotId};
use calyx_ward::GuardVerdict;
use serde::{Deserialize, Serialize};

use crate::util::hex32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerLensContribution {
    pub slot: PanelSlotId,
    pub rank: usize,
    pub raw_score: f32,
    pub weight: f32,
    pub contribution: f32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessTag {
    pub built_at_seq: u64,
    pub base_seq: u64,
    pub stale_by: u64,
    pub policy: String,
    /// Immutable generation sequence when a bounded MVCC delta produced the
    /// logical `built_at_seq` view. Absent for an exact immutable generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciled_from_seq: Option<u64>,
}

impl FreshnessTag {
    pub fn fresh(seq: u64) -> Self {
        Self {
            built_at_seq: seq,
            base_seq: seq,
            stale_by: 0,
            policy: "fresh_derived".to_string(),
            reconciled_from_seq: None,
        }
    }

    pub fn fresh_reconciled(generation_seq: u64, pinned_seq: u64) -> Self {
        Self {
            built_at_seq: pinned_seq,
            base_seq: pinned_seq,
            stale_by: 0,
            policy: "fresh_reconciled".to_string(),
            reconciled_from_seq: Some(generation_seq),
        }
    }

    pub fn stale_ok(built_at_seq: u64, base_seq: u64) -> Self {
        Self {
            built_at_seq,
            base_seq,
            stale_by: base_seq.saturating_sub(built_at_seq),
            policy: "stale_ok".to_string(),
            reconciled_from_seq: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExplainBreakdown {
    pub strategy: String,
    pub per_lens_count: usize,
    pub provenance_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence_boost: Option<crate::temporal::RecurrenceBoostEvidence>,
    #[serde(default)]
    pub guard_dropped: Vec<DroppedGuardHit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitGuardMode {
    InRegionOnly,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HitGuardEvidence {
    pub mode: HitGuardMode,
    pub verdict: GuardVerdict,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DroppedGuardHit {
    pub cx_id: CxId,
    pub mode: HitGuardMode,
    pub reason: String,
    #[serde(default)]
    pub verdict: Option<GuardVerdict>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceSource {
    Stored,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub cx_id: CxId,
    pub score: f32,
    pub rank: usize,
    #[serde(default)]
    pub event_time_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_scores: Option<crate::temporal::TemporalScores>,
    #[serde(
        default,
        skip_serializing_if = "crate::temporal::CausalConfidence::is_absent"
    )]
    pub causal_confidence: crate::temporal::CausalConfidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal_gate: Option<crate::temporal::CausalGateEvidence>,
    pub per_lens: Vec<PerLensContribution>,
    pub cross_terms_used: bool,
    pub guard: Option<HitGuardEvidence>,
    pub provenance: LedgerRef,
    pub provenance_source: ProvenanceSource,
    pub freshness: FreshnessTag,
    pub explain: Option<ExplainBreakdown>,
}

impl Hit {
    pub fn with_explain(mut self, strategy: impl Into<String>) -> Self {
        self.explain = Some(ExplainBreakdown {
            strategy: strategy.into(),
            per_lens_count: self.per_lens.len(),
            provenance_hex: hex32(&self.provenance.hash),
            recurrence_boost: None,
            guard_dropped: Vec::new(),
        });
        self
    }
}

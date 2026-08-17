//! PH50 super-intelligence tier measurement.

use std::collections::{BTreeMap, BTreeSet};

use calyx_assay::PanelSufficiency;
use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, CxId, LensId, Panel};
use calyx_lodestar::{
    AnnIndex, CorpusReader, KernelIndex, LodestarError, RecallEvalParams, RecallReport,
    measure_kernel_recall_with_clock,
};
use serde::{Deserialize, Serialize};

use crate::honesty_gate::{SufficiencyAssay, VaultSufficiencyAssay};
use crate::self_consistency::oracle_self_consistency;
use crate::types::{DomainId, OracleSelfConsistency};
use crate::{OracleError, SuperIntelReport, Tier, TierResult};

pub const ORACLE_CLEAN_THRESHOLD: f32 = 0.7;
pub const KERNEL_RECALL_RATIO: f32 = 0.95;

const ORACLE_FIX_MORE_LABELS: &str = "label more oracle instances to reduce flakiness";
const ORACLE_FIX_VALIDITY_ANCHOR: &str = "add validity-tracking anchor";
const KERNEL_FIX_HELD_OUT: &str = "label held-out instances";
const KERNEL_FIX_ANCHORS: &str = "ingest more anchor instances for domain";

pub trait OracleConsistencySource {
    fn oracle_self_consistency(
        &self,
        domain: DomainId,
        clock: &dyn Clock,
    ) -> Result<OracleSelfConsistency, OracleError>;
}

impl<C> OracleConsistencySource for AsterVault<C>
where
    C: Clock,
{
    fn oracle_self_consistency(
        &self,
        domain: DomainId,
        clock: &dyn Clock,
    ) -> Result<OracleSelfConsistency, OracleError> {
        oracle_self_consistency(self, domain, clock)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldOutSplit {
    pub split_id: String,
    pub training_ids: Vec<CxId>,
    pub held_out_ids: Vec<CxId>,
}

impl HeldOutSplit {
    pub fn new(
        split_id: impl Into<String>,
        training_ids: Vec<CxId>,
        held_out_ids: Vec<CxId>,
    ) -> Self {
        Self {
            split_id: split_id.into(),
            training_ids,
            held_out_ids,
        }
    }

    pub fn held_out_count(&self) -> usize {
        self.held_out_ids.len()
    }

    pub fn has_training_leakage(&self) -> bool {
        let training = self.training_ids.iter().copied().collect::<BTreeSet<_>>();
        self.held_out_ids
            .iter()
            .any(|cx_id| training.contains(cx_id))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortCircuit {
    Enabled,
    #[default]
    MeasureAll,
}

impl ShortCircuit {
    fn stops_after_failure(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

pub trait KernelRecallSource {
    fn kernel_recall_report(
        &self,
        held_out: &HeldOutSplit,
        clock: &dyn Clock,
    ) -> Result<RecallReport, LodestarError>;
}

pub struct KernelRecallGate<'a> {
    kernel_index: &'a KernelIndex,
    full_index: &'a dyn AnnIndex,
    corpus: &'a dyn CorpusReader,
    params: RecallEvalParams,
}

impl<'a> KernelRecallGate<'a> {
    pub fn new(
        kernel_index: &'a KernelIndex,
        full_index: &'a dyn AnnIndex,
        corpus: &'a dyn CorpusReader,
        mut params: RecallEvalParams,
    ) -> Self {
        params.min_recall_ratio = KERNEL_RECALL_RATIO;
        Self {
            kernel_index,
            full_index,
            corpus,
            params,
        }
    }
}

impl KernelRecallSource for KernelRecallGate<'_> {
    fn kernel_recall_report(
        &self,
        held_out: &HeldOutSplit,
        clock: &dyn Clock,
    ) -> Result<RecallReport, LodestarError> {
        validate_held_out(held_out)?;
        measure_kernel_recall_with_clock(
            self.kernel_index,
            self.full_index,
            self.corpus,
            &self.params,
            clock,
        )
    }
}

pub struct TierMeasurementRequest<'a, O, A, K> {
    pub oracle: &'a O,
    pub assay: &'a A,
    pub kernel: &'a K,
    pub panel: &'a Panel,
    pub domain: DomainId,
    pub held_out: &'a HeldOutSplit,
    pub clock: &'a dyn Clock,
    pub short_circuit: ShortCircuit,
}

pub fn measure_tier_oracle_clean<C>(
    vault: &AsterVault<C>,
    domain: DomainId,
    clock: &dyn Clock,
) -> TierResult
where
    C: Clock,
{
    measure_tier_oracle_clean_with_source(vault, domain, clock)
}

pub fn measure_tier_oracle_clean_with_source<S>(
    source: &S,
    domain: DomainId,
    clock: &dyn Clock,
) -> TierResult
where
    S: OracleConsistencySource,
{
    match source.oracle_self_consistency(domain, clock) {
        Ok(result) => measure_tier_oracle_clean_from_result(&result),
        Err(error) => failed_tier(
            Tier::OracleClean,
            ORACLE_CLEAN_THRESHOLD,
            oracle_error_fix(&error),
        ),
    }
}

pub(crate) fn measure_tier_oracle_clean_from_result(result: &OracleSelfConsistency) -> TierResult {
    measured_tier(
        Tier::OracleClean,
        result.ceiling,
        ORACLE_CLEAN_THRESHOLD,
        || oracle_clean_fix(result),
    )
}

pub fn measure_tier_panel_sufficient<C>(
    vault: &AsterVault<C>,
    panel: &Panel,
    domain: DomainId,
    clock: &dyn Clock,
) -> TierResult
where
    C: Clock,
{
    let assay = VaultSufficiencyAssay::new(vault);
    measure_tier_panel_sufficient_with_assay(&assay, panel, domain, clock)
}

pub fn measure_tier_panel_sufficient_with_assay<A>(
    assay: &A,
    panel: &Panel,
    domain: DomainId,
    clock: &dyn Clock,
) -> TierResult
where
    A: SufficiencyAssay,
{
    match assay.panel_sufficiency(panel, &domain, clock) {
        Ok(report) => panel_sufficiency_tier(panel, &report),
        Err(error) => failed_tier(Tier::PanelSufficient, 0.0, oracle_error_fix(&error)),
    }
}

pub fn measure_tier_kernel_exists<K>(
    source: &K,
    _domain: DomainId,
    held_out: &HeldOutSplit,
    clock: &dyn Clock,
) -> TierResult
where
    K: KernelRecallSource,
{
    if held_out.held_out_ids.is_empty() {
        return failed_tier(
            Tier::KernelExists,
            KERNEL_RECALL_RATIO,
            KERNEL_FIX_HELD_OUT.to_string(),
        );
    }
    if held_out.has_training_leakage() {
        return failed_tier(
            Tier::KernelExists,
            KERNEL_RECALL_RATIO,
            "CALYX_RECALL_INVALID_PARAMS: held-out split overlaps training ids".to_string(),
        );
    }
    match source.kernel_recall_report(held_out, clock) {
        Ok(report) => kernel_recall_tier(&report),
        Err(error) => failed_tier(
            Tier::KernelExists,
            KERNEL_RECALL_RATIO,
            kernel_error_fix(&error),
        ),
    }
}

pub fn measure_tiers_1_to_3<O, A, K>(
    request: TierMeasurementRequest<'_, O, A, K>,
) -> Vec<TierResult>
where
    O: OracleConsistencySource,
    A: SufficiencyAssay,
    K: KernelRecallSource,
{
    let mut tiers = Vec::with_capacity(3);
    let tier = measure_tier_oracle_clean_with_source(
        request.oracle,
        request.domain.clone(),
        request.clock,
    );
    let keep_going = tier.passed || !request.short_circuit.stops_after_failure();
    tiers.push(tier);
    if !keep_going {
        return tiers;
    }

    let tier = measure_tier_panel_sufficient_with_assay(
        request.assay,
        request.panel,
        request.domain.clone(),
        request.clock,
    );
    let keep_going = tier.passed || !request.short_circuit.stops_after_failure();
    tiers.push(tier);
    if !keep_going {
        return tiers;
    }

    tiers.push(measure_tier_kernel_exists(
        request.kernel,
        request.domain,
        request.held_out,
        request.clock,
    ));
    tiers
}

pub fn measure_super_intelligence_tiers_1_to_3<O, A, K>(
    request: TierMeasurementRequest<'_, O, A, K>,
) -> SuperIntelReport
where
    O: OracleConsistencySource,
    A: SufficiencyAssay,
    K: KernelRecallSource,
{
    let domain = request.domain.clone();
    let tiers = measure_tiers_1_to_3(request);
    SuperIntelReport::new(domain, tiers)
}

pub(crate) fn panel_sufficiency_tier(panel: &Panel, report: &PanelSufficiency) -> TierResult {
    if !valid_measurement(report.panel_bits, report.anchor_entropy_bits) {
        return failed_tier(
            Tier::PanelSufficient,
            0.0,
            "CALYX_ORACLE_INSUFFICIENT: sufficiency report has invalid bits".to_string(),
        );
    }
    measured_tier(
        Tier::PanelSufficient,
        report.panel_bits,
        report.anchor_entropy_bits,
        || panel_sufficiency_fix(panel, report),
    )
}

pub(crate) fn kernel_recall_tier(report: &RecallReport) -> TierResult {
    if !valid_measurement(report.ratio, KERNEL_RECALL_RATIO) || report.n_queries_tested == 0 {
        return failed_tier(
            Tier::KernelExists,
            KERNEL_RECALL_RATIO,
            KERNEL_FIX_HELD_OUT.to_string(),
        );
    }
    measured_tier(
        Tier::KernelExists,
        report.ratio,
        KERNEL_RECALL_RATIO,
        || KERNEL_FIX_ANCHORS.to_string(),
    )
}

pub(crate) fn measured_tier(
    tier: Tier,
    measured_value: f32,
    threshold: f32,
    cheapest_fix: impl FnOnce() -> String,
) -> TierResult {
    let passed = valid_measurement(measured_value, threshold) && measured_value >= threshold;
    TierResult::new(
        tier,
        passed,
        if measured_value.is_finite() {
            measured_value
        } else {
            0.0
        },
        if threshold.is_finite() {
            threshold
        } else {
            0.0
        },
        (!passed).then(cheapest_fix),
    )
}

pub(crate) fn failed_tier(tier: Tier, threshold: f32, cheapest_fix: String) -> TierResult {
    TierResult::new(tier, false, 0.0, threshold, Some(cheapest_fix))
}

pub(crate) fn valid_measurement(measured_value: f32, threshold: f32) -> bool {
    measured_value.is_finite() && threshold.is_finite() && measured_value >= 0.0 && threshold >= 0.0
}

fn oracle_clean_fix(result: &OracleSelfConsistency) -> String {
    if result.provisional || result.validity < ORACLE_CLEAN_THRESHOLD {
        ORACLE_FIX_VALIDITY_ANCHOR.to_string()
    } else {
        ORACLE_FIX_MORE_LABELS.to_string()
    }
}

fn panel_sufficiency_fix(panel: &Panel, report: &PanelSufficiency) -> String {
    match max_deficit_lens(panel, report) {
        Some((lens_id, deficit)) => {
            format!("add a pre-trigger causal lens for {lens_id} (deficit {deficit:.6} bits)")
        }
        None => "add a pre-trigger causal lens for the unassigned sensor".to_string(),
    }
}

fn max_deficit_lens(panel: &Panel, report: &PanelSufficiency) -> Option<(LensId, f32)> {
    let mut gaps = BTreeMap::<_, f32>::new();
    for deficit in &report.deficits {
        if let Some(slot) = deficit.slot {
            gaps.entry(slot).or_insert(deficit.deficit_bits);
        }
        for (slot, gap) in &deficit.per_slot_gaps {
            gaps.insert(*slot, *gap);
        }
    }
    panel
        .slots
        .iter()
        .filter_map(|slot| gaps.get(&slot.slot_id).map(|gap| (slot.lens_id, *gap)))
        .max_by(|left, right| left.1.total_cmp(&right.1))
}

pub(crate) fn oracle_error_fix(error: &OracleError) -> String {
    format!("{}: {}", error.code(), error.remediation())
}

fn kernel_error_fix(error: &LodestarError) -> String {
    match error {
        LodestarError::RecallEmptyCorpus => KERNEL_FIX_HELD_OUT.to_string(),
        other => format!("{}: {}", other.code(), other),
    }
}

pub(crate) fn validate_held_out(held_out: &HeldOutSplit) -> Result<(), LodestarError> {
    if held_out.held_out_ids.is_empty() {
        return Err(LodestarError::RecallEmptyCorpus);
    }
    if held_out.has_training_leakage() {
        return Err(LodestarError::RecallInvalidParams {
            detail: "held-out split overlaps training ids".to_string(),
        });
    }
    Ok(())
}

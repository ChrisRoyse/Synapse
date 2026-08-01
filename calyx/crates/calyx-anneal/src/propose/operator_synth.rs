mod codec;
mod gate;
mod storage;

use calyx_aster::cf::full_content_hash;
use calyx_core::{CalyxError, Clock, Result, SystemClock};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactKey, ArtifactPtr, ChangeId, ChangeOutcome, HeadKind, LogicalTime,
    MAX_ONLINE_HEAD_PARAMS, ScopeId, ShadowRevertReason,
};

use super::{DEFAULT_DEFICIT_THRESHOLD_BITS, DeficitMap};
pub use codec::{
    OperatorProposalReadback, decode_operator_proposal, decode_operator_proposal_rows,
    encode_operator_proposal, operator_proposal_key,
};
pub use gate::OperatorPromotionGate;
use gate::operator_ledger_details;
pub use storage::{AsterOperatorProposalStorage, OperatorProposalStorage};

pub const ANNEAL_OPERATOR_PROPOSAL_TAG: &str = "anneal_operator_proposal_v1";
pub const CALYX_ANNEAL_OPERATOR_INVALID_RECORD: &str = "CALYX_ANNEAL_OPERATOR_INVALID_RECORD";
pub const CALYX_ANNEAL_OPERATOR_NO_GAIN: &str = "CALYX_ANNEAL_OPERATOR_NO_GAIN";

const METRIC_EPSILON: f64 = 1e-12;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operator", rename_all = "snake_case")]
pub enum ProposedOperator {
    OnlineHead {
        kind: HeadKind,
        param_count: usize,
    },
    KernelScope {
        scope: ScopeId,
        scope_hash: [u8; 32],
        kernel_recall_before: f64,
        kernel_recall_after: f64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OperatorTerminalState {
    NoDeficit,
    RefitClosed,
    Promoted,
    RolledBack { reason: ShadowRevertReason },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperatorProposalRecord {
    pub proposal_id: String,
    pub operator: ProposedOperator,
    pub deficit_total_bits: f64,
    pub refit_delta_j: f64,
    pub shadow_delta_j: f64,
    pub terminal_state: OperatorTerminalState,
    pub change_id: Option<ChangeId>,
    pub ts: LogicalTime,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperatorProposalOutcome {
    pub record_key: Option<Vec<u8>>,
    pub record: Option<OperatorProposalRecord>,
    pub terminal_state: OperatorTerminalState,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OperatorProposalConfig {
    pub deficit_threshold_bits: f64,
    pub min_delta_j: f64,
}

impl Default for OperatorProposalConfig {
    fn default() -> Self {
        Self {
            deficit_threshold_bits: DEFAULT_DEFICIT_THRESHOLD_BITS,
            min_delta_j: 1e-6,
        }
    }
}

pub struct ProposeOperator<'a> {
    clock: &'a dyn Clock,
    config: OperatorProposalConfig,
}

pub struct ProposeOperatorRequest<'a> {
    pub deficit: &'a DeficitMap,
    pub refit_delta_j: f64,
    pub storage: &'a dyn OperatorProposalStorage,
    pub gate: &'a mut dyn OperatorPromotionGate,
    pub kernel_recall_before: Option<f64>,
    pub kernel_recall_after: Option<f64>,
}

impl<'a> ProposeOperator<'a> {
    pub fn new(clock: &'a dyn Clock) -> Self {
        Self {
            clock,
            config: OperatorProposalConfig::default(),
        }
    }

    pub fn with_config(clock: &'a dyn Clock, config: OperatorProposalConfig) -> Result<Self> {
        validate_nonnegative("deficit_threshold_bits", config.deficit_threshold_bits)?;
        validate_nonnegative("min_delta_j", config.min_delta_j)?;
        Ok(Self { clock, config })
    }

    pub fn propose_operator(
        &self,
        request: ProposeOperatorRequest<'_>,
    ) -> Result<OperatorProposalOutcome> {
        validate_deficit(request.deficit)?;
        let deficit_total = validate_nonnegative(
            "deficit.total_bits_deficit",
            request.deficit.total_bits_deficit,
        )?;
        let refit_delta_j = validate_nonnegative("refit_delta_j", request.refit_delta_j)?;
        if deficit_total <= self.config.deficit_threshold_bits {
            return Ok(terminal_without_record(OperatorTerminalState::NoDeficit));
        }
        if refit_delta_j + METRIC_EPSILON >= deficit_total {
            return Ok(terminal_without_record(OperatorTerminalState::RefitClosed));
        }

        let operator = synthesize_operator(
            request.deficit,
            request.kernel_recall_before,
            request.kernel_recall_after,
        )?;
        validate_operator(&operator)?;
        let shadow_delta_j = shadow_delta_j(&operator, deficit_total, refit_delta_j)?;
        if shadow_delta_j + METRIC_EPSILON < self.config.min_delta_j {
            return Err(no_gain(shadow_delta_j, self.config.min_delta_j));
        }

        let candidate_hash =
            operator_hash(&operator, deficit_total, refit_delta_j, shadow_delta_j)?;
        let proposal_id = hex_prefix(&candidate_hash);
        let key = artifact_key(&operator, candidate_hash);
        let prior_ptr = prior_ptr(candidate_hash);
        let candidate_ptr = candidate_ptr(&operator, candidate_hash);
        request.gate.ensure_operator_prior(key.clone(), prior_ptr)?;
        let details = operator_ledger_details(
            &proposal_id,
            &operator,
            deficit_total,
            refit_delta_j,
            shadow_delta_j,
        );
        let description = format!(
            "learned_operator_synthesis operator={} shadow_delta_j={shadow_delta_j:.6} refit_delta_j={refit_delta_j:.6} deficit_bits={deficit_total:.6}",
            operator_label(&operator)
        );
        let (terminal_state, change_id) =
            match request
                .gate
                .propose_operator_change(key, candidate_ptr, details, &description)?
            {
                ChangeOutcome::Promoted(change_id) => (OperatorTerminalState::Promoted, change_id),
                ChangeOutcome::Reverted { reason, change_id } => {
                    (OperatorTerminalState::RolledBack { reason }, change_id)
                }
            };
        let record = OperatorProposalRecord {
            proposal_id,
            operator,
            deficit_total_bits: deficit_total,
            refit_delta_j,
            shadow_delta_j,
            terminal_state: terminal_state.clone(),
            change_id: Some(change_id),
            ts: self.clock.now(),
        };
        let record_key = operator_proposal_key(record.ts, &record.proposal_id);
        request
            .storage
            .save_operator_proposal(record_key.clone(), encode_operator_proposal(&record)?)?;
        Ok(OperatorProposalOutcome {
            record_key: Some(record_key),
            record: Some(record),
            terminal_state,
        })
    }
}

pub fn propose_operator(request: ProposeOperatorRequest<'_>) -> Result<OperatorProposalOutcome> {
    let clock = SystemClock;
    ProposeOperator::new(&clock).propose_operator(request)
}

fn terminal_without_record(terminal_state: OperatorTerminalState) -> OperatorProposalOutcome {
    OperatorProposalOutcome {
        record_key: None,
        record: None,
        terminal_state,
    }
}

fn synthesize_operator(
    deficit: &DeficitMap,
    kernel_recall_before: Option<f64>,
    kernel_recall_after: Option<f64>,
) -> Result<ProposedOperator> {
    let top_anchor = deficit
        .top_gaps
        .first()
        .map(|gap| gap.anchor_class.as_str())
        .unwrap_or("unknown");
    if prefers_kernel_scope(top_anchor) {
        let before = require_kernel_recall("kernel_recall_before", kernel_recall_before)?;
        let after = require_kernel_recall("kernel_recall_after", kernel_recall_after)?;
        let scope_hash = scope_hash(deficit)?;
        return Ok(ProposedOperator::KernelScope {
            scope: ScopeId::from_hash(scope_hash),
            scope_hash,
            kernel_recall_before: before,
            kernel_recall_after: after,
        });
    }
    Ok(ProposedOperator::OnlineHead {
        kind: head_kind(top_anchor),
        param_count: 1 + deficit.underrepresented_modalities.len().max(1),
    })
}

fn shadow_delta_j(
    operator: &ProposedOperator,
    deficit_total: f64,
    refit_delta_j: f64,
) -> Result<f64> {
    match operator {
        ProposedOperator::OnlineHead { .. } => {
            validate_nonnegative("online_head_shadow_delta_j", deficit_total - refit_delta_j)
        }
        ProposedOperator::KernelScope {
            kernel_recall_before,
            kernel_recall_after,
            ..
        } => validate_nonnegative(
            "kernel_scope_shadow_delta_j",
            kernel_recall_after - kernel_recall_before,
        ),
    }
}

fn require_kernel_recall(name: &'static str, value: Option<f64>) -> Result<f64> {
    let value = value.ok_or_else(|| CalyxError {
        code: super::CALYX_ASSAY_UNAVAILABLE,
        message: format!(
            "{name} is required for a kernel-scope operator proposal; no measured recall was supplied"
        ),
        remediation: "measure incumbent and candidate recall independently on the deterministic held-out replay before proposing a kernel-scope operator",
    })?;
    validate_unit(name, value)
}

fn validate_deficit(deficit: &DeficitMap) -> Result<()> {
    validate_nonnegative("deficit.total_bits_deficit", deficit.total_bits_deficit)?;
    for gap in &deficit.top_gaps {
        validate_nonnegative("deficit.entropy_h", gap.entropy_h)?;
        validate_nonnegative("deficit.mutual_info_i", gap.mutual_info_i)?;
        validate_nonnegative("deficit.gap", gap.gap)?;
        if super::deficit_localize::violates_dpi(gap.mutual_info_i, gap.entropy_h) {
            return Err(invalid_metric(format!(
                "deficit anchor '{}' violates DPI: I={} > H={}",
                gap.anchor_class, gap.mutual_info_i, gap.entropy_h
            )));
        }
    }
    Ok(())
}

fn validate_record(record: &OperatorProposalRecord) -> Result<()> {
    if record.proposal_id.trim().is_empty() {
        return Err(invalid_record("proposal_id must not be empty"));
    }
    validate_operator(&record.operator)?;
    validate_nonnegative("deficit_total_bits", record.deficit_total_bits)?;
    validate_nonnegative("refit_delta_j", record.refit_delta_j)?;
    validate_nonnegative("shadow_delta_j", record.shadow_delta_j)?;
    Ok(())
}

fn validate_operator(operator: &ProposedOperator) -> Result<()> {
    match operator {
        ProposedOperator::OnlineHead { param_count, .. } => {
            if *param_count == 0 || *param_count > MAX_ONLINE_HEAD_PARAMS {
                return Err(invalid_record(format!(
                    "online head proposal param_count {param_count} outside 1..={MAX_ONLINE_HEAD_PARAMS}"
                )));
            }
            Ok(())
        }
        ProposedOperator::KernelScope {
            kernel_recall_before,
            kernel_recall_after,
            ..
        } => {
            validate_unit("kernel_recall_before", *kernel_recall_before)?;
            validate_unit("kernel_recall_after", *kernel_recall_after)?;
            Ok(())
        }
    }
}

fn validate_nonnegative(name: &'static str, value: f64) -> Result<f64> {
    if !value.is_finite() || value < -METRIC_EPSILON {
        return Err(invalid_metric(format!(
            "{name} must be finite and non-negative, got {value}"
        )));
    }
    Ok(if value.abs() <= METRIC_EPSILON {
        0.0
    } else {
        value
    })
}

fn validate_unit(name: &'static str, value: f64) -> Result<f64> {
    let value = validate_nonnegative(name, value)?;
    if value > 1.0 {
        return Err(invalid_metric(format!(
            "{name} must be <= 1.0, got {value}"
        )));
    }
    Ok(value)
}

fn no_gain(observed: f64, threshold: f64) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_OPERATOR_NO_GAIN,
        message: format!("operator proposal shadow_delta_j {observed:.6} is below {threshold:.6}"),
        remediation: "do not grow a learned operator until shadow-measured J gain is positive",
    }
}

fn invalid_metric(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: super::CALYX_ASSAY_INVALID_METRIC,
        message: message.into(),
        remediation: "re-measure deficit and shadow deltaJ before proposing a learned operator",
    }
}

fn invalid_record(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_OPERATOR_INVALID_RECORD,
        message: message.into(),
        remediation: "repair or quarantine anneal_operators proposal rows before admission",
    }
}

fn operator_hash(
    operator: &ProposedOperator,
    deficit_total: f64,
    refit_delta_j: f64,
    shadow_delta_j: f64,
) -> Result<[u8; 32]> {
    let bytes = serde_json::to_vec(&(operator, deficit_total, refit_delta_j, shadow_delta_j))
        .map_err(|error| invalid_record(format!("hash operator proposal: {error}")))?;
    Ok(full_content_hash([bytes.as_slice()]))
}

fn scope_hash(deficit: &DeficitMap) -> Result<[u8; 32]> {
    let bytes = serde_json::to_vec(deficit)
        .map_err(|error| invalid_record(format!("hash operator scope: {error}")))?;
    Ok(full_content_hash([
        b"learned-kernel-scope-v1".as_slice(),
        bytes.as_slice(),
    ]))
}

fn artifact_key(operator: &ProposedOperator, hash: [u8; 32]) -> ArtifactKey {
    match operator {
        ProposedOperator::OnlineHead { .. } => ArtifactKey::ConfigCache(hash),
        ProposedOperator::KernelScope { scope_hash, .. } => ArtifactKey::QuantLevel(*scope_hash),
    }
}

fn prior_ptr(hash: [u8; 32]) -> ArtifactPtr {
    ArtifactPtr::ConfigCacheKeyHash(full_content_hash([
        b"learned-operator-prior-v1".as_slice(),
        &hash,
    ]))
}

fn candidate_ptr(operator: &ProposedOperator, hash: [u8; 32]) -> ArtifactPtr {
    match operator {
        ProposedOperator::OnlineHead { .. } => ArtifactPtr::ConfigCacheKeyHash(hash),
        ProposedOperator::KernelScope { .. } => ArtifactPtr::QuantLevelRecordHash(hash),
    }
}

fn prefers_kernel_scope(anchor: &str) -> bool {
    let anchor = anchor.to_ascii_lowercase();
    [
        "kernel",
        "scope",
        "recall",
        "graph",
        "tenant",
        "window",
        "collection",
    ]
    .iter()
    .any(|needle| anchor.contains(needle))
}

fn head_kind(anchor: &str) -> HeadKind {
    let anchor = anchor.to_ascii_lowercase();
    if anchor.contains("calibr") {
        HeadKind::Calibrator
    } else if anchor.contains("fusion") || anchor.contains("weight") {
        HeadKind::FusionWeights
    } else {
        HeadKind::Predictor
    }
}

fn operator_label(operator: &ProposedOperator) -> &'static str {
    match operator {
        ProposedOperator::OnlineHead { .. } => "online_head",
        ProposedOperator::KernelScope { .. } => "kernel_scope",
    }
}

fn hex_prefix(bytes: &[u8; 32]) -> String {
    bytes[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

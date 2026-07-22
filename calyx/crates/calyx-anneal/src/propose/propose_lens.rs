use calyx_assay::PanelResourceBudget;
use calyx_core::{
    CalyxError, Clock, Constellation, LensId, Panel, Result, SlotState, SystemClock, Ts,
};
use calyx_ledger::LedgerCfStore;
use calyx_registry::SwapController;
use serde::{Deserialize, Serialize};

use crate::{
    AnnealSubstrate, ArtifactKey, ArtifactPtr, BudgetProbe, ChangeId, ChangeOutcome,
    RollbackStorage, ShadowRevertReason,
};

use super::{
    AnchorId, AssayAttribution, CALYX_ASSAY_INVALID_METRIC, CALYX_ASSAY_UNAVAILABLE, CandidateLens,
    DeficitLocalizer, DifferentiationGate, GateOutcome, LensProfiler, PairNMI, gate, has_deficit,
    synthesize,
};

pub const CALYX_REGISTRY_HOT_ADD_FAIL: &str = "CALYX_REGISTRY_HOT_ADD_FAIL";

const METRIC_EPSILON: f64 = 1e-12;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProposalOutcome {
    pub candidate: Option<CandidateLens>,
    pub gate_outcome: Option<GateOutcome>,
    pub sufficiency_before: f64,
    pub sufficiency_after: Option<f64>,
    pub admitted: bool,
    pub change_id: Option<ChangeId>,
    pub hot_add: Option<HotAddReceipt>,
    pub terminal_state: ProposalTerminalState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProposalTerminalState {
    NoDeficit,
    GateRejected,
    HotAddFailed { code: String },
    SubstrateReverted { reason: ShadowRevertReason },
    NoSufficiencyGain,
    Admitted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotAddReceipt {
    pub lens_id: LensId,
    pub panel_version: u32,
    pub slot_count: usize,
}

#[derive(Clone, Debug)]
pub struct HotAddPlan {
    pub artifact_key: ArtifactKey,
    pub prior_ptr: ArtifactPtr,
    pub candidate_ptr: ArtifactPtr,
    pub description: String,
}

pub trait ProposalSubstrate {
    fn ensure_prior(&mut self, key: ArtifactKey, prior_ptr: ArtifactPtr) -> Result<()>;
    fn propose_hot_add(&mut self, plan: &HotAddPlan) -> Result<ChangeOutcome>;
    fn rollback_hot_add(&mut self, change_id: ChangeId) -> Result<()>;
}

impl<'a, R, L, C, P> ProposalSubstrate for AnnealSubstrate<'a, R, L, C, P>
where
    R: RollbackStorage,
    L: LedgerCfStore,
    C: Clock,
    P: BudgetProbe,
{
    fn ensure_prior(&mut self, key: ArtifactKey, prior_ptr: ArtifactPtr) -> Result<()> {
        if self.rollback.live_ptr(&key)?.is_none() {
            self.rollback.install_live_ptr(key, prior_ptr)?;
        }
        Ok(())
    }

    fn propose_hot_add(&mut self, plan: &HotAddPlan) -> Result<ChangeOutcome> {
        self.propose_artifact_change_with_description(
            plan.artifact_key.clone(),
            plan.candidate_ptr.clone(),
            plan.description.clone(),
        )
    }

    fn rollback_hot_add(&mut self, change_id: ChangeId) -> Result<()> {
        AnnealSubstrate::rollback_explicit(self, change_id)
    }
}

pub trait LensHotAdder {
    fn plan_hot_add(
        &mut self,
        panel: &Panel,
        candidate: &CandidateLens,
        corpus: &[Constellation],
    ) -> Result<HotAddPlan>;

    fn apply_hot_add(
        &mut self,
        controller: &mut SwapController,
        candidate: &CandidateLens,
        corpus: &[Constellation],
        now: Ts,
        next_panel_generation: u32,
    ) -> Result<HotAddReceipt>;
}

pub struct ProposeLensRequest<'a> {
    pub anchor: &'a AnchorId,
    pub controller: &'a mut SwapController,
    pub substrate: &'a mut dyn ProposalSubstrate,
    pub assay: &'a dyn AssayAttribution,
    pub hot_add: &'a mut dyn LensHotAdder,
    pub profiler: &'a dyn LensProfiler,
    pub nmi: &'a dyn PairNMI,
    pub corpus: &'a [Constellation],
    /// Vault-global generation reserved before proposing this mutation.
    pub next_panel_generation: u32,
}

pub struct ProposeLens<'a> {
    clock: &'a dyn Clock,
    deficit_threshold_bits: f64,
    resource_budget: Option<PanelResourceBudget>,
}

impl<'a> ProposeLens<'a> {
    pub fn new(clock: &'a dyn Clock) -> Self {
        Self {
            clock,
            deficit_threshold_bits: super::DEFAULT_DEFICIT_THRESHOLD_BITS,
            resource_budget: None,
        }
    }

    pub fn with_resource_budget(mut self, budget: PanelResourceBudget) -> Self {
        self.resource_budget = Some(budget);
        self
    }

    pub fn propose_lens(&self, request: ProposeLensRequest<'_>) -> Result<ProposalOutcome> {
        let panel_lenses = panel_lens_ids(request.controller.panel());
        let deficit = DeficitLocalizer::new(self.clock).localize(
            request.assay,
            request.anchor,
            &panel_lenses,
        )?;
        let sufficiency_before = sufficiency_from_deficit(request.anchor, &deficit)?;
        if !has_deficit(&deficit, self.deficit_threshold_bits) {
            return Ok(ProposalOutcome::terminal(
                None,
                None,
                sufficiency_before,
                None,
                None,
                None,
                ProposalTerminalState::NoDeficit,
            ));
        }

        let candidate = synthesize(&deficit, request.corpus)?;
        let gate_outcome = match self.resource_budget {
            Some(budget) => DifferentiationGate::new(self.clock)
                .with_resource_budget(budget)
                .gate(
                    &candidate,
                    &panel_lenses,
                    request.profiler,
                    request.nmi,
                    request.corpus,
                )?,
            None => gate(
                &candidate,
                &panel_lenses,
                request.profiler,
                request.nmi,
                request.corpus,
            )?,
        };
        if !matches!(gate_outcome, GateOutcome::Admitted { .. }) {
            return Ok(ProposalOutcome::terminal(
                Some(candidate),
                Some(gate_outcome),
                sufficiency_before,
                None,
                None,
                None,
                ProposalTerminalState::GateRejected,
            ));
        }

        let plan = match request.hot_add.plan_hot_add(
            request.controller.panel(),
            &candidate,
            request.corpus,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                return Ok(ProposalOutcome::hot_add_failed(
                    candidate,
                    gate_outcome,
                    sufficiency_before,
                    None,
                    None,
                    None,
                    &error,
                ));
            }
        };
        request
            .substrate
            .ensure_prior(plan.artifact_key.clone(), plan.prior_ptr.clone())?;
        let prior_controller = request.controller.clone();
        match request.substrate.propose_hot_add(&plan)? {
            ChangeOutcome::Reverted { reason, change_id } => Ok(ProposalOutcome::terminal(
                Some(candidate),
                Some(gate_outcome),
                sufficiency_before,
                None,
                None,
                Some(change_id),
                ProposalTerminalState::SubstrateReverted { reason },
            )),
            ChangeOutcome::Promoted(change_id) => {
                let receipt = match request.hot_add.apply_hot_add(
                    request.controller,
                    &candidate,
                    request.corpus,
                    self.clock.now(),
                    request.next_panel_generation,
                ) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        *request.controller = prior_controller.clone();
                        request.substrate.rollback_hot_add(change_id)?;
                        return Ok(ProposalOutcome::hot_add_failed(
                            candidate,
                            gate_outcome,
                            sufficiency_before,
                            None,
                            None,
                            Some(change_id),
                            &error,
                        ));
                    }
                };
                let sufficiency_after = match read_sufficiency(request.assay, request.anchor) {
                    Ok(value) => value,
                    Err(error) => {
                        *request.controller = prior_controller.clone();
                        request.substrate.rollback_hot_add(change_id)?;
                        return Ok(ProposalOutcome::terminal(
                            Some(candidate),
                            Some(gate_outcome),
                            sufficiency_before,
                            None,
                            Some(receipt),
                            Some(change_id),
                            ProposalTerminalState::HotAddFailed {
                                code: error.code.to_string(),
                            },
                        ));
                    }
                };
                if sufficiency_after <= sufficiency_before + METRIC_EPSILON {
                    *request.controller = prior_controller;
                    request.substrate.rollback_hot_add(change_id)?;
                    return Ok(ProposalOutcome::terminal(
                        Some(candidate),
                        Some(gate_outcome),
                        sufficiency_before,
                        Some(sufficiency_after),
                        Some(receipt),
                        Some(change_id),
                        ProposalTerminalState::NoSufficiencyGain,
                    ));
                }
                Ok(ProposalOutcome {
                    candidate: Some(candidate),
                    gate_outcome: Some(gate_outcome),
                    sufficiency_before,
                    sufficiency_after: Some(sufficiency_after),
                    admitted: true,
                    change_id: Some(change_id),
                    hot_add: Some(receipt),
                    terminal_state: ProposalTerminalState::Admitted,
                })
            }
        }
    }
}

impl ProposalOutcome {
    fn terminal(
        candidate: Option<CandidateLens>,
        gate_outcome: Option<GateOutcome>,
        sufficiency_before: f64,
        sufficiency_after: Option<f64>,
        hot_add: Option<HotAddReceipt>,
        change_id: Option<ChangeId>,
        terminal_state: ProposalTerminalState,
    ) -> Self {
        Self {
            candidate,
            gate_outcome,
            sufficiency_before,
            sufficiency_after,
            admitted: false,
            change_id,
            hot_add,
            terminal_state,
        }
    }

    fn hot_add_failed(
        candidate: CandidateLens,
        gate_outcome: GateOutcome,
        sufficiency_before: f64,
        sufficiency_after: Option<f64>,
        hot_add: Option<HotAddReceipt>,
        change_id: Option<ChangeId>,
        error: &CalyxError,
    ) -> Self {
        Self::terminal(
            Some(candidate),
            Some(gate_outcome),
            sufficiency_before,
            sufficiency_after,
            hot_add,
            change_id,
            ProposalTerminalState::HotAddFailed {
                code: error.code.to_string(),
            },
        )
    }
}

pub fn propose_lens(request: ProposeLensRequest<'_>) -> Result<ProposalOutcome> {
    let clock = SystemClock;
    ProposeLens::new(&clock).propose_lens(request)
}

fn panel_lens_ids(panel: &Panel) -> Vec<LensId> {
    panel
        .slots
        .iter()
        .filter(|slot| slot.state != SlotState::Retired)
        .map(|slot| slot.lens_id)
        .collect()
}

fn sufficiency_from_deficit(anchor: &AnchorId, deficit: &super::DeficitMap) -> Result<f64> {
    let value = deficit
        .top_gaps
        .iter()
        .find(|gap| gap.anchor_class == anchor.as_str())
        .map(|gap| gap.mutual_info_i)
        .ok_or_else(|| invalid_metric(format!("missing localized gap for anchor {anchor}")))?;
    validate_sufficiency(value)
}

fn read_sufficiency(assay: &dyn AssayAttribution, anchor: &AnchorId) -> Result<f64> {
    let value = assay
        .panel_sufficiency(anchor)
        .map_err(|error| CalyxError {
            code: CALYX_ASSAY_UNAVAILABLE,
            message: format!(
                "Assay attribution unavailable while re-measuring panel_sufficiency for anchor {anchor}: {}: {}",
                error.code, error.message
            ),
            remediation: "restore Assay sufficiency data before admitting a proposed lens",
        })?;
    validate_sufficiency(value)
}

fn validate_sufficiency(value: f64) -> Result<f64> {
    if !value.is_finite() || value < -METRIC_EPSILON {
        return Err(invalid_metric(format!(
            "panel_sufficiency must be finite and non-negative, got {value}"
        )));
    }
    Ok(if value.abs() <= METRIC_EPSILON {
        0.0
    } else {
        value
    })
}

fn invalid_metric(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ASSAY_INVALID_METRIC,
        message: message.into(),
        remediation: "re-measure sufficiency before admitting a proposed lens",
    }
}

use calyx_core::{Clock, LedgerRef, PanelSlotId, Result};
use calyx_ledger::LedgerCfStore;

use super::types::{
    RecalibrationOutcome, TauDriftEvent, TauShadowAction, WardRecalibrate, WardTauStore, ptr_hash,
    tau_artifact_key, tau_change_id, tau_hash, tau_ptr, validate_tau, ward_failed,
};
use crate::{
    AnnealLedger, AnnealLedgerAction, AnnealLedgerEntry, AnnealSubstrate, ArtifactPtr, BudgetProbe,
    ChangeOutcome, ComponentKind, DegradeRegistry, HealthStorage, LogicalTime, MetricSnapshot,
    RollbackStorage,
};

const TAU_CPU_WEIGHT: f64 = 0.01;
const TAU_VRAM_BYTES: u64 = 0;

pub fn trigger_tau_recalibration<S, R, L, C, P, W, T>(
    ward: &W,
    tau_store: &mut T,
    registry: &mut DegradeRegistry<S>,
    panel_slot: PanelSlotId,
    drift_event: &TauDriftEvent,
    substrate: &mut AnnealSubstrate<'_, R, L, C, P>,
) -> Result<RecalibrationOutcome>
where
    S: HealthStorage,
    R: RollbackStorage,
    L: LedgerCfStore,
    C: Clock,
    P: BudgetProbe,
    W: WardRecalibrate,
    T: WardTauStore,
{
    if drift_event.panel_slot != panel_slot {
        return Err(super::types::invalid_tau(
            "drift_event panel_slot does not match target panel slot",
        ));
    }
    let prior_tau = tau_store
        .current_tau(panel_slot)?
        .unwrap_or(drift_event.current_tau);
    validate_tau(prior_tau)?;
    ensure_live_tau_pointer(substrate, panel_slot, prior_tau)?;
    let budget = substrate.budget.acquire(TAU_CPU_WEIGHT, TAU_VRAM_BYTES)?;
    let new_tau = match ward.recalibrate(panel_slot, drift_event.snapshot, budget) {
        Ok(new_tau) => new_tau,
        Err(error) => {
            let wrapped = ward_failed(error);
            write_tau_event(
                &mut substrate.ledger,
                TauLedgerEvent {
                    action: AnnealLedgerAction::TauRecalibrationReverted,
                    panel_slot,
                    prior_tau,
                    candidate_tau: prior_tau,
                    prior_ptr: Some(tau_ptr(panel_slot, prior_tau)),
                    candidate_ptr: None,
                    ts: drift_event.observed_at,
                    description: format!(
                        "tau recalibration failed {}: {}",
                        wrapped.code, wrapped.message
                    ),
                },
            )?;
            return Err(wrapped);
        }
    };
    if new_tau.panel_slot != panel_slot {
        return Err(super::types::invalid_tau(
            "WardRecalibrate returned tau for the wrong panel slot",
        ));
    }
    validate_tau(new_tau.tau)?;
    let candidate_ptr = tau_ptr(panel_slot, new_tau.tau);
    let candidate = TauShadowAction(new_tau.shadow_metrics.clone());
    let incumbent = TauShadowAction(drift_event.incumbent_metrics.clone());
    let outcome = substrate.propose_change_with_description(
        tau_artifact_key(panel_slot),
        candidate_ptr.clone(),
        &candidate,
        &incumbent,
        "tau recalibration",
    )?;
    match outcome {
        ChangeOutcome::Promoted(change_id) => {
            tau_store.set_live_tau(panel_slot, &new_tau, drift_event.observed_at)?;
            registry.confirm_healed(
                ComponentKind::GuardProfile { panel_slot },
                &mut substrate.ledger,
            )?;
            write_tau_event(
                &mut substrate.ledger,
                TauLedgerEvent {
                    action: AnnealLedgerAction::TauRecalibrated,
                    panel_slot,
                    prior_tau,
                    candidate_tau: new_tau.tau,
                    prior_ptr: Some(tau_ptr(panel_slot, prior_tau)),
                    candidate_ptr: Some(candidate_ptr),
                    ts: drift_event.observed_at,
                    description: format!(
                        "tau recalibrated slot={} far={:.6}",
                        panel_slot, new_tau.far
                    ),
                },
            )?;
            Ok(RecalibrationOutcome::Promoted {
                change_id,
                panel_slot,
                prior_tau,
                new_tau: new_tau.tau,
            })
        }
        ChangeOutcome::Reverted { reason, change_id } => {
            write_tau_event(
                &mut substrate.ledger,
                TauLedgerEvent {
                    action: AnnealLedgerAction::TauRecalibrationReverted,
                    panel_slot,
                    prior_tau,
                    candidate_tau: new_tau.tau,
                    prior_ptr: Some(tau_ptr(panel_slot, prior_tau)),
                    candidate_ptr: Some(candidate_ptr),
                    ts: drift_event.observed_at,
                    description: format!(
                        "tau recalibration reverted slot={} reason={reason:?}",
                        panel_slot
                    ),
                },
            )?;
            Ok(RecalibrationOutcome::Reverted {
                change_id,
                panel_slot,
                prior_tau,
                candidate_tau: new_tau.tau,
                reason,
            })
        }
    }
}

fn ensure_live_tau_pointer<R, L, C, P>(
    substrate: &mut AnnealSubstrate<'_, R, L, C, P>,
    panel_slot: PanelSlotId,
    tau: f32,
) -> Result<()>
where
    R: RollbackStorage,
    L: LedgerCfStore,
    C: Clock,
    P: BudgetProbe,
{
    let key = tau_artifact_key(panel_slot);
    if substrate.rollback.live_ptr(&key)?.is_none() {
        substrate
            .rollback
            .install_live_ptr(key, tau_ptr(panel_slot, tau))?;
    }
    Ok(())
}

struct TauLedgerEvent {
    action: AnnealLedgerAction,
    panel_slot: PanelSlotId,
    prior_tau: f32,
    candidate_tau: f32,
    prior_ptr: Option<ArtifactPtr>,
    candidate_ptr: Option<ArtifactPtr>,
    ts: LogicalTime,
    description: String,
}

fn write_tau_event<L, C>(
    ledger: &mut AnnealLedger<L, C>,
    event: TauLedgerEvent,
) -> Result<LedgerRef>
where
    L: LedgerCfStore,
    C: Clock,
{
    ledger.write(AnnealLedgerEntry {
        action: event.action,
        change_id: tau_change_id(event.panel_slot, event.ts, event.action),
        artifact_id: format!(
            "guard_tau/panel_{:010}/slot_{:04}",
            event.panel_slot.panel_version(),
            event.panel_slot.slot_id().get()
        ),
        prior_ptr_hash: event
            .prior_ptr
            .map(|ptr| ptr_hash(&ptr))
            .unwrap_or_else(|| tau_hash(event.panel_slot, event.prior_tau)),
        candidate_ptr_hash: event
            .candidate_ptr
            .map(|ptr| ptr_hash(&ptr))
            .unwrap_or_else(|| tau_hash(event.panel_slot, event.candidate_tau)),
        metrics: MetricSnapshot::empty(event.ts),
        ts: event.ts,
        description: event.description,
        fault: None,
        proposal: None,
        details: None,
        prev_hash: None,
    })
}

mod artifact;

use std::sync::Arc;

use calyx_aster::cf::full_content_hash;
use calyx_core::{CalyxError, Clock, Result};
use calyx_ledger::LedgerCfStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::shadow::AnnealAction as ShadowAnnealAction;
use crate::{
    AnnealLedger, AnnealLedgerAction, AnnealLedgerEntry, ArtifactKey, ArtifactPtr,
    ArtifactReplayMeasurer, BudgetEnforcer, BudgetProbe, ChangeId, HeldOutReplay, MetricSnapshot,
    ProcStatBudgetProbe, RollbackReadback, RollbackStorage, RollbackStore, ShadowExecutor,
    ShadowRevertReason, ShadowVerdict, TripwireRegistry, TripwireStatus,
};

pub const CALYX_LEDGER_WRITE_FAIL: &str = "CALYX_LEDGER_WRITE_FAIL";

const DEFAULT_SHADOW_CPU_WEIGHT: f64 = 0.01;
const DEFAULT_SHADOW_VRAM_BYTES: u64 = 0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChangeOutcome {
    Promoted(ChangeId),
    Reverted {
        reason: ShadowRevertReason,
        change_id: ChangeId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnnealStatus {
    pub tripwire_states: Vec<TripwireStatus>,
    pub budget: crate::BudgetStatus,
    pub recent_changes: Vec<AnnealLedgerEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnealLedgerActionPair {
    pub promote: AnnealLedgerAction,
    pub revert: AnnealLedgerAction,
}

impl AnnealLedgerActionPair {
    pub const fn new(promote: AnnealLedgerAction, revert: AnnealLedgerAction) -> Self {
        Self { promote, revert }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnnealProposalLedgerOptions {
    pub actions: AnnealLedgerActionPair,
    pub details: Option<Value>,
}

impl AnnealProposalLedgerOptions {
    pub const fn new(actions: AnnealLedgerActionPair) -> Self {
        Self {
            actions,
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

pub struct AnnealSubstrate<'a, R, L, C, P = ProcStatBudgetProbe>
where
    R: RollbackStorage,
    L: LedgerCfStore,
    C: Clock,
    P: BudgetProbe,
{
    pub tripwires: TripwireRegistry,
    pub replay: HeldOutReplay,
    pub rollback: RollbackStore<'a, R>,
    pub ledger: AnnealLedger<L, C>,
    pub budget: BudgetEnforcer<'a, P>,
    clock: &'a dyn Clock,
    replay_measurer: Option<Arc<dyn ArtifactReplayMeasurer>>,
    shadow_cpu_weight: f64,
    shadow_vram_bytes: u64,
    shadow_metrics: Vec<crate::TripwireMetric>,
}

impl<'a, R, L, C, P> AnnealSubstrate<'a, R, L, C, P>
where
    R: RollbackStorage,
    L: LedgerCfStore,
    C: Clock,
    P: BudgetProbe,
{
    pub fn new(
        tripwires: TripwireRegistry,
        replay: HeldOutReplay,
        rollback: RollbackStore<'a, R>,
        ledger: AnnealLedger<L, C>,
        budget: BudgetEnforcer<'a, P>,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            tripwires,
            replay,
            rollback,
            ledger,
            budget,
            clock,
            replay_measurer: None,
            shadow_cpu_weight: DEFAULT_SHADOW_CPU_WEIGHT,
            shadow_vram_bytes: DEFAULT_SHADOW_VRAM_BYTES,
            shadow_metrics: crate::ALL_SHADOW_METRICS.to_vec(),
        }
    }

    pub const fn with_budget_request(mut self, cpu_weight: f64, vram_bytes: u64) -> Self {
        self.shadow_cpu_weight = cpu_weight;
        self.shadow_vram_bytes = vram_bytes;
        self
    }

    pub fn with_shadow_metrics(mut self, metrics: Vec<crate::TripwireMetric>) -> Self {
        self.shadow_metrics = metrics;
        self
    }

    pub fn propose_change<Candidate, Incumbent>(
        &mut self,
        key: ArtifactKey,
        candidate_ptr: ArtifactPtr,
        candidate: &Candidate,
        incumbent: &Incumbent,
    ) -> Result<ChangeOutcome>
    where
        Candidate: ShadowAnnealAction,
        Incumbent: ShadowAnnealAction,
    {
        self.propose_change_with_description(
            key,
            candidate_ptr,
            candidate,
            incumbent,
            "anneal proposal",
        )
    }

    pub fn propose_change_with_description<Candidate, Incumbent>(
        &mut self,
        key: ArtifactKey,
        candidate_ptr: ArtifactPtr,
        candidate: &Candidate,
        incumbent: &Incumbent,
        description: impl Into<String>,
    ) -> Result<ChangeOutcome>
    where
        Candidate: ShadowAnnealAction,
        Incumbent: ShadowAnnealAction,
    {
        self.propose_change_with_actions(
            key,
            candidate_ptr,
            candidate,
            incumbent,
            AnnealLedgerActionPair::new(AnnealLedgerAction::Promote, AnnealLedgerAction::Revert),
            description,
        )
    }

    pub fn propose_change_with_actions<Candidate, Incumbent>(
        &mut self,
        key: ArtifactKey,
        candidate_ptr: ArtifactPtr,
        candidate: &Candidate,
        incumbent: &Incumbent,
        actions: AnnealLedgerActionPair,
        description: impl Into<String>,
    ) -> Result<ChangeOutcome>
    where
        Candidate: ShadowAnnealAction,
        Incumbent: ShadowAnnealAction,
    {
        self.propose_change_with_actions_and_details(
            key,
            candidate_ptr,
            candidate,
            incumbent,
            AnnealProposalLedgerOptions::new(actions),
            description,
        )
    }

    pub fn propose_change_with_actions_and_details<Candidate, Incumbent>(
        &mut self,
        key: ArtifactKey,
        candidate_ptr: ArtifactPtr,
        candidate: &Candidate,
        incumbent: &Incumbent,
        ledger_options: AnnealProposalLedgerOptions,
        description: impl Into<String>,
    ) -> Result<ChangeOutcome>
    where
        Candidate: ShadowAnnealAction,
        Incumbent: ShadowAnnealAction,
    {
        let description = description.into();
        let (change_id, readback) =
            self.prepare_change(key, candidate_ptr, description.as_str())?;
        let verdict = self.shadow_verdict(candidate, incumbent)?;
        self.finish_prepared_change(change_id, readback, verdict, ledger_options, description)
    }

    pub(crate) fn prepare_change(
        &mut self,
        key: ArtifactKey,
        candidate_ptr: ArtifactPtr,
        description: &str,
    ) -> Result<(ChangeId, RollbackReadback)> {
        let change_id =
            self.rollback
                .prepare_with_description(key, candidate_ptr, description.to_string())?;
        let readback = self.rollback.readback(change_id)?;
        Ok((change_id, readback))
    }

    pub(crate) fn finish_prepared_change(
        &mut self,
        change_id: ChangeId,
        readback: RollbackReadback,
        verdict: ShadowVerdict,
        ledger_options: AnnealProposalLedgerOptions,
        description: String,
    ) -> Result<ChangeOutcome> {
        match verdict {
            ShadowVerdict::Promote { metrics } => {
                let details = ledger_options.details.clone();
                let entry = ledger_entry(
                    &readback,
                    ledger_options.actions.promote,
                    metrics,
                    description,
                    details,
                );
                self.write_ledger(entry)?;
                self.rollback.promote(change_id)?;
                Ok(ChangeOutcome::Promoted(change_id))
            }
            ShadowVerdict::Revert { reason, metrics } => {
                self.rollback.reject_prepared(change_id)?;
                let reverted = self.rollback.readback(change_id)?;
                let entry = ledger_entry(
                    &reverted,
                    ledger_options.actions.revert,
                    metrics,
                    description,
                    ledger_options.details,
                );
                self.write_ledger(entry)?;
                Ok(ChangeOutcome::Reverted { reason, change_id })
            }
        }
    }

    pub fn rollback_explicit(&mut self, change_id: ChangeId) -> Result<()> {
        self.rollback_explicit_with_action(
            change_id,
            AnnealLedgerAction::Revert,
            "explicit rollback".to_string(),
        )
    }

    pub fn rollback_explicit_with_action(
        &mut self,
        change_id: ChangeId,
        action: AnnealLedgerAction,
        description: String,
    ) -> Result<()> {
        self.rollback.rollback(change_id)?;
        let readback = self.rollback.readback(change_id)?;
        let entry = ledger_entry(
            &readback,
            action,
            MetricSnapshot::empty(self.clock.now()),
            description,
            None,
        );
        self.write_ledger(entry)
    }

    pub fn write_sleep_pass_deferred(&mut self, description: String) -> Result<()> {
        let ts = self.clock.now();
        let entry = AnnealLedgerEntry {
            action: AnnealLedgerAction::SleepPassDeferred,
            change_id: ChangeId(ts.max(1)),
            artifact_id: "sleep_pass".to_string(),
            prior_ptr_hash: [0; 32],
            candidate_ptr_hash: [0; 32],
            metrics: MetricSnapshot::empty(ts),
            ts,
            description,
            fault: None,
            proposal: None,
            details: None,
            prev_hash: None,
        };
        self.write_ledger(entry)
    }

    pub fn write_outcome_event(
        &mut self,
        action: AnnealLedgerAction,
        change_id: ChangeId,
        artifact_id: String,
        candidate_hash: [u8; 32],
        description: String,
    ) -> Result<()> {
        self.write_outcome_event_with_details(
            action,
            change_id,
            artifact_id,
            candidate_hash,
            description,
            None,
        )
    }

    pub fn write_outcome_event_with_details(
        &mut self,
        action: AnnealLedgerAction,
        change_id: ChangeId,
        artifact_id: String,
        candidate_hash: [u8; 32],
        description: String,
        details: Option<Value>,
    ) -> Result<()> {
        let ts = self.clock.now();
        let entry = AnnealLedgerEntry {
            action,
            change_id,
            artifact_id,
            prior_ptr_hash: [0; 32],
            candidate_ptr_hash: candidate_hash,
            metrics: MetricSnapshot::empty(ts),
            ts,
            description,
            fault: None,
            proposal: None,
            details,
            prev_hash: None,
        };
        self.write_ledger(entry)
    }

    pub fn status(&self) -> Result<AnnealStatus> {
        Ok(AnnealStatus {
            tripwire_states: self.tripwires.status(),
            budget: self.budget.status()?,
            recent_changes: self.ledger.read_recent(16)?,
        })
    }

    pub(crate) fn shadow_verdict<Candidate, Incumbent>(
        &mut self,
        candidate: &Candidate,
        incumbent: &Incumbent,
    ) -> Result<ShadowVerdict>
    where
        Candidate: ShadowAnnealAction,
        Incumbent: ShadowAnnealAction,
    {
        let budget = match self
            .budget
            .acquire(self.shadow_cpu_weight, self.shadow_vram_bytes)
        {
            Ok(handle) => handle,
            Err(error) if error.code == crate::CALYX_ANNEAL_BUDGET_EXHAUSTED => {
                return Ok(ShadowVerdict::Revert {
                    reason: ShadowRevertReason::BudgetExhausted,
                    metrics: MetricSnapshot::empty(self.clock.now()),
                });
            }
            Err(error) => return Err(error),
        };
        let mut executor = ShadowExecutor::new(
            self.tripwires.clone(),
            self.replay.clone(),
            budget,
            self.clock,
        )
        .with_required_metrics(self.shadow_metrics.clone());
        let verdict = executor.run_shadow(candidate, incumbent);
        self.tripwires = executor.registry;
        Ok(verdict)
    }

    fn write_ledger(&mut self, entry: AnnealLedgerEntry) -> Result<()> {
        self.ledger
            .write(entry)
            .map(|_| ())
            .map_err(ledger_write_fail)
    }
}

fn ledger_entry(
    readback: &RollbackReadback,
    action: AnnealLedgerAction,
    metrics: MetricSnapshot,
    description: String,
    details: Option<Value>,
) -> AnnealLedgerEntry {
    AnnealLedgerEntry {
        action,
        change_id: readback.snapshot.change_id,
        artifact_id: artifact_id(&readback.snapshot.key),
        prior_ptr_hash: ptr_hash(&readback.snapshot.prior_ptr),
        candidate_ptr_hash: ptr_hash(&readback.snapshot.candidate_ptr),
        metrics,
        ts: readback.snapshot.ts,
        description,
        fault: None,
        proposal: None,
        details,
        prev_hash: None,
    }
}

fn artifact_id(key: &ArtifactKey) -> String {
    match key {
        ArtifactKey::ConfigCache(hash)
        | ArtifactKey::HnswGraph(hash)
        | ArtifactKey::QuantLevel(hash) => hex32(hash),
    }
}

fn ptr_hash(ptr: &ArtifactPtr) -> [u8; 32] {
    match ptr {
        ArtifactPtr::ConfigCacheKeyHash(hash) | ArtifactPtr::QuantLevelRecordHash(hash) => *hash,
        ArtifactPtr::HnswGraphPath(path) => full_content_hash([path.as_bytes()]),
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn ledger_write_fail(error: CalyxError) -> CalyxError {
    CalyxError {
        code: CALYX_LEDGER_WRITE_FAIL,
        message: format!(
            "Anneal ledger write failed: {}: {}",
            error.code, error.message
        ),
        remediation: "repair the ledger CF before mutating the live Anneal pointer",
    }
}

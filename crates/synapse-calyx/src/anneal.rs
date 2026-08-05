use calyx_anneal::{
    AnnealAction, AnnealLedger, AnnealLedgerEntry, AnnealSubstrate, ArtifactKey, ArtifactPtr,
    AsterAnnealLedgerStore, AsterRollbackStorage, BudgetConfig, BudgetEnforcer, BudgetStatus,
    ChangeId, ChangeOutcome, HeldOutReplay, RollbackStore, TripwireRegistry, TripwireStatus,
};
use calyx_aster::cf::ColumnFamily;
use calyx_ledger::{ActorId, LedgerAppender};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::{SynapseCalyxError, SynapseCalyxTuningConfig, SynapseCalyxVault};

const TUNING_ARTIFACT_TAG: &[u8] = b"synapse-anneal-tuning-v1";
const TUNING_ARTIFACT_KEY: &[u8] = b"synapse/live-tuning/v1";
const ARTIFACT_ROW_PREFIX: &[u8] = b"synapse/tuning-artifact/v1/";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynapseCalyxAnnealStatus {
    pub live_artifact_sha256: String,
    pub live_artifact_bytes: usize,
    pub effective_tuning: SynapseCalyxTuningConfig,
    pub rollback_rows: usize,
    pub tripwires: Vec<TripwireStatus>,
    pub budget: BudgetStatus,
    pub recent_changes: Vec<AnnealLedgerEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynapseCalyxAnnealChangeReport {
    pub outcome: ChangeOutcome,
    pub prior_artifact_sha256: String,
    pub candidate_artifact_sha256: String,
    pub live_artifact_sha256_after: String,
    pub live_artifact_bytes_after: usize,
    pub rollback_rows_after: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynapseCalyxAnnealRollbackReport {
    pub change_id: u64,
    pub candidate_artifact_sha256: String,
    pub restored_artifact_sha256: String,
    pub restored_artifact_bytes: usize,
    pub rollback_rows_after: usize,
}

impl SynapseCalyxVault {
    pub(crate) fn initialize_anneal_tuning(&self) -> Result<(), SynapseCalyxError> {
        let configured = self.config.tuning.validate()?;
        let bytes = serde_json::to_vec(&configured).map_err(|error| {
            anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ARTIFACT_ENCODE_FAILED",
                format!("encode configured tuning artifact: {error}"),
                "repair the validated tuning serializer before reopening the vault",
            )
        })?;
        let hash = tuning_artifact_hash(&bytes);
        self.persist_tuning_artifact(hash, &bytes)?;

        let clock = self.anneal_clock()?;
        let rollback = RollbackStore::open(
            &clock,
            self.config.tuning.rng_seed,
            AsterRollbackStorage::new(&self.vault),
        )
        .map_err(|error| SynapseCalyxError::from_calyx("open Anneal rollback store", &error))?;
        let key = tuning_artifact_key();
        if rollback
            .live_ptr(&key)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read Anneal live tuning pointer", &error)
            })?
            .is_none()
        {
            rollback
                .install_live_ptr(key, ArtifactPtr::ConfigCacheKeyHash(hash))
                .map_err(|error| {
                    SynapseCalyxError::from_calyx("install initial Anneal tuning pointer", &error)
                })?;
        }
        let effective = self.effective_tuning()?;
        tracing::info!(
            code = "SYNAPSE_CALYX_ANNEAL_TUNING_OPENED",
            fusion_k = effective.fusion_k,
            index_ef_search = effective.index_ef_search,
            "resolved durable Anneal tuning pointer"
        );
        Ok(())
    }

    pub(crate) fn effective_tuning(&self) -> Result<SynapseCalyxTuningConfig, SynapseCalyxError> {
        self.read_live_tuning().map(|(_, _, tuning)| tuning)
    }

    pub fn anneal_status(&self) -> Result<SynapseCalyxAnnealStatus, SynapseCalyxError> {
        let (hash, bytes, effective_tuning) = self.read_live_tuning()?;
        let clock = self.anneal_clock()?;
        let tripwires =
            TripwireRegistry::load_from_vault(&self.config.vault_dir).map_err(|error| {
                SynapseCalyxError::from_calyx("load Anneal tripwire registry", &error)
            })?;
        let budget_config =
            BudgetConfig::load_from_vault(&self.config.vault_dir).map_err(|error| {
                SynapseCalyxError::from_calyx("load Anneal resource budget", &error)
            })?;
        let budget = BudgetEnforcer::new(budget_config, &clock)
            .and_then(|enforcer| enforcer.status())
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read Anneal resource budget", &error)
            })?;
        let appender =
            LedgerAppender::open(AsterAnnealLedgerStore::new(&self.vault), clock.clone())
                .map_err(|error| SynapseCalyxError::from_calyx("open Anneal ledger", &error))?;
        let ledger = AnnealLedger::new(appender, ActorId::Service("synapse-anneal".to_owned()))
            .map_err(|error| SynapseCalyxError::from_calyx("open Anneal ledger actor", &error))?;
        let recent_changes = ledger
            .read_recent(16)
            .map_err(|error| SynapseCalyxError::from_calyx("read recent Anneal changes", &error))?;
        let rollback_rows = self
            .vault
            .count_cf_latest(ColumnFamily::AnnealRollback)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("count physical Anneal rollback rows", &error)
            })?;
        Ok(SynapseCalyxAnnealStatus {
            live_artifact_sha256: hex32(hash),
            live_artifact_bytes: bytes.len(),
            effective_tuning,
            rollback_rows,
            tripwires: tripwires.status(),
            budget,
            recent_changes,
        })
    }

    /// Runs a caller-owned, real held-out replay through Calyx's native shadow gate.
    ///
    /// The actions must measure the actual candidate and incumbent. This API
    /// deliberately accepts no metric values or predeclared verdict, so callers
    /// cannot promote by asserting that a candidate was healthy.
    pub fn anneal_propose_tuning<Candidate, Incumbent>(
        &self,
        candidate_tuning: SynapseCalyxTuningConfig,
        replay: HeldOutReplay,
        candidate_action: &Candidate,
        incumbent_action: &Incumbent,
        description: &str,
    ) -> Result<SynapseCalyxAnnealChangeReport, SynapseCalyxError>
    where
        Candidate: AnnealAction,
        Incumbent: AnnealAction,
    {
        let candidate_tuning = candidate_tuning.validate()?;
        let (_, prior_bytes, incumbent_tuning) = self.read_live_tuning()?;
        ensure_supported_candidate(&incumbent_tuning, &candidate_tuning)?;
        let candidate_bytes = serde_json::to_vec(&candidate_tuning).map_err(|error| {
            anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ARTIFACT_ENCODE_FAILED",
                format!("encode candidate tuning artifact: {error}"),
                "repair the validated tuning serializer before proposing the candidate",
            )
        })?;
        let prior_hash = tuning_artifact_hash(&prior_bytes);
        let candidate_hash = tuning_artifact_hash(&candidate_bytes);
        if candidate_hash == prior_hash {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_CANDIDATE_UNCHANGED",
                format!(
                    "candidate tuning artifact {} is already live",
                    hex32(candidate_hash)
                ),
                "change at least one load-bearing Anneal-owned tuning field before proposing",
            ));
        }
        self.persist_tuning_artifact(candidate_hash, &candidate_bytes)?;
        tracing::info!(
            code = "SYNAPSE_CALYX_ANNEAL_CANDIDATE_PERSISTED",
            prior_artifact_sha256 = %hex32(prior_hash),
            candidate_artifact_sha256 = %hex32(candidate_hash),
            "persisted and independently read back Anneal candidate artifact"
        );

        let clock = self.anneal_clock()?;
        let rollback = RollbackStore::open(
            &clock,
            self.config.tuning.rng_seed,
            AsterRollbackStorage::new(&self.vault),
        )
        .map_err(|error| SynapseCalyxError::from_calyx("open Anneal rollback store", &error))?;
        let ledger = self.anneal_ledger(clock.clone())?;
        let tripwires =
            TripwireRegistry::load_from_vault(&self.config.vault_dir).map_err(|error| {
                SynapseCalyxError::from_calyx("load Anneal tripwire registry", &error)
            })?;
        let budget_config =
            BudgetConfig::load_from_vault(&self.config.vault_dir).map_err(|error| {
                SynapseCalyxError::from_calyx("load Anneal resource budget", &error)
            })?;
        let budget = BudgetEnforcer::new(budget_config, &clock).map_err(|error| {
            SynapseCalyxError::from_calyx("open Anneal resource budget", &error)
        })?;
        let mut substrate =
            AnnealSubstrate::new(tripwires, replay, rollback, ledger, budget, &clock);
        tracing::info!(
            code = "SYNAPSE_CALYX_ANNEAL_SHADOW_BEGIN",
            candidate_artifact_sha256 = %hex32(candidate_hash),
            "beginning native Anneal prepare and shadow transaction"
        );
        let outcome = substrate
            .propose_change_with_description(
                tuning_artifact_key(),
                ArtifactPtr::ConfigCacheKeyHash(candidate_hash),
                candidate_action,
                incumbent_action,
                description,
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx("shadow-test Anneal tuning candidate", &error)
            })?;
        tracing::info!(
            code = "SYNAPSE_CALYX_ANNEAL_SHADOW_COMPLETE",
            candidate_artifact_sha256 = %hex32(candidate_hash),
            outcome = ?outcome,
            "native Anneal transaction completed"
        );
        let (live_hash, live_bytes, _) = self.read_live_tuning()?;
        Ok(SynapseCalyxAnnealChangeReport {
            outcome,
            prior_artifact_sha256: hex32(prior_hash),
            candidate_artifact_sha256: hex32(candidate_hash),
            live_artifact_sha256_after: hex32(live_hash),
            live_artifact_bytes_after: live_bytes.len(),
            rollback_rows_after: self.anneal_rollback_row_count("after proposal")?,
        })
    }

    pub fn anneal_rollback(
        &self,
        change_id: u64,
    ) -> Result<SynapseCalyxAnnealRollbackReport, SynapseCalyxError> {
        let clock = self.anneal_clock()?;
        let rollback = RollbackStore::open(
            &clock,
            self.config.tuning.rng_seed,
            AsterRollbackStorage::new(&self.vault),
        )
        .map_err(|error| SynapseCalyxError::from_calyx("open Anneal rollback store", &error))?;
        let snapshot = rollback
            .snapshot(ChangeId(change_id))
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read Anneal rollback snapshot", &error)
            })?
            .ok_or_else(|| {
                anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_CHANGE_UNKNOWN",
                    format!("Anneal change_id {change_id} does not exist"),
                    "read anneal status and use an existing uncommitted promoted change id",
                )
            })?;
        let candidate_hash = ptr_config_hash(&snapshot.candidate_ptr)?;
        let ledger = self.anneal_ledger(clock.clone())?;
        let tripwires =
            TripwireRegistry::load_from_vault(&self.config.vault_dir).map_err(|error| {
                SynapseCalyxError::from_calyx("load Anneal tripwire registry", &error)
            })?;
        let budget_config =
            BudgetConfig::load_from_vault(&self.config.vault_dir).map_err(|error| {
                SynapseCalyxError::from_calyx("load Anneal resource budget", &error)
            })?;
        let budget = BudgetEnforcer::new(budget_config, &clock).map_err(|error| {
            SynapseCalyxError::from_calyx("open Anneal resource budget", &error)
        })?;
        let mut substrate = AnnealSubstrate::new(
            tripwires,
            HeldOutReplay {
                queries: Vec::new(),
                seed: self.config.tuning.rng_seed,
            },
            rollback,
            ledger,
            budget,
            &clock,
        );
        substrate
            .rollback_explicit(ChangeId(change_id))
            .map_err(|error| {
                SynapseCalyxError::from_calyx("rollback Anneal tuning change", &error)
            })?;
        let (restored_hash, restored_bytes, _) = self.read_live_tuning()?;
        Ok(SynapseCalyxAnnealRollbackReport {
            change_id,
            candidate_artifact_sha256: hex32(candidate_hash),
            restored_artifact_sha256: hex32(restored_hash),
            restored_artifact_bytes: restored_bytes.len(),
            rollback_rows_after: self.anneal_rollback_row_count("after rollback")?,
        })
    }

    fn anneal_ledger(
        &self,
        clock: crate::SynapseCalyxClock,
    ) -> Result<
        AnnealLedger<
            AsterAnnealLedgerStore<'_, crate::SynapseCalyxClock>,
            crate::SynapseCalyxClock,
        >,
        SynapseCalyxError,
    > {
        let appender = LedgerAppender::open(AsterAnnealLedgerStore::new(&self.vault), clock)
            .map_err(|error| SynapseCalyxError::from_calyx("open Anneal ledger", &error))?;
        AnnealLedger::new(appender, ActorId::Service("synapse-anneal".to_owned()))
            .map_err(|error| SynapseCalyxError::from_calyx("open Anneal ledger actor", &error))
    }

    fn anneal_rollback_row_count(&self, phase: &str) -> Result<usize, SynapseCalyxError> {
        self.vault
            .count_cf_latest(ColumnFamily::AnnealRollback)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!("count Anneal rollback rows {phase}"),
                    &error,
                )
            })
    }

    fn read_live_tuning(
        &self,
    ) -> Result<([u8; 32], Vec<u8>, SynapseCalyxTuningConfig), SynapseCalyxError> {
        let clock = self.anneal_clock()?;
        let rollback = RollbackStore::open(
            &clock,
            self.config.tuning.rng_seed,
            AsterRollbackStorage::new(&self.vault),
        )
        .map_err(|error| SynapseCalyxError::from_calyx("open Anneal rollback store", &error))?;
        let ptr = rollback
            .live_ptr(&tuning_artifact_key())
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read Anneal live tuning pointer", &error)
            })?
            .ok_or_else(|| {
                anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_LIVE_POINTER_MISSING",
                    "Anneal has no live Synapse tuning pointer",
                    "reopen the writable vault to install the validated initial tuning artifact",
                )
            })?;
        let ArtifactPtr::ConfigCacheKeyHash(hash) = ptr else {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_POINTER_KIND_INVALID",
                format!("live Synapse tuning pointer has incompatible kind: {ptr:?}"),
                "repair the AnnealRollback live pointer to a ConfigCacheKeyHash backed by the native Kv artifact row",
            ));
        };
        let row_key = tuning_artifact_row_key(hash);
        let bytes = self
            .vault
            .read_cf_latest(ColumnFamily::Kv, &row_key)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read live Anneal tuning artifact", &error)
            })?
            .ok_or_else(|| {
                anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_ARTIFACT_MISSING",
                    format!("live tuning artifact {} is absent from Kv", hex32(hash)),
                    "restore the referenced content-addressed Kv row before reopening the vault",
                )
            })?;
        if tuning_artifact_hash(&bytes) != hash {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ARTIFACT_HASH_MISMATCH",
                format!(
                    "live tuning artifact {} does not hash to its pointer",
                    hex32(hash)
                ),
                "restore the exact artifact bytes referenced by the AnnealRollback pointer",
            ));
        }
        let tuning = serde_json::from_slice::<SynapseCalyxTuningConfig>(&bytes)
            .map_err(|error| {
                anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_ARTIFACT_DECODE_FAILED",
                    format!("decode live tuning artifact {}: {error}", hex32(hash)),
                    "restore a valid versioned Synapse tuning artifact and pointer",
                )
            })?
            .validate()?;
        Ok((hash, bytes, tuning))
    }

    fn persist_tuning_artifact(
        &self,
        hash: [u8; 32],
        bytes: &[u8],
    ) -> Result<(), SynapseCalyxError> {
        let key = tuning_artifact_row_key(hash);
        if let Some(existing) =
            self.vault
                .read_cf_latest(ColumnFamily::Kv, &key)
                .map_err(|error| {
                    SynapseCalyxError::from_calyx("read candidate Anneal tuning artifact", &error)
                })?
        {
            if existing != bytes {
                return Err(anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_ARTIFACT_COLLISION",
                    format!("Kv artifact key {} contains different bytes", hex32(hash)),
                    "stop and inspect the vault for content-address corruption",
                ));
            }
            return Ok(());
        }
        self.vault
            .write_cf(ColumnFamily::Kv, key.clone(), bytes.to_vec())
            .map_err(|error| {
                SynapseCalyxError::from_calyx("write candidate Anneal tuning artifact", &error)
            })?;
        let observed = self
            .vault
            .read_cf_latest(ColumnFamily::Kv, &key)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read back candidate Anneal tuning artifact", &error)
            })?;
        if observed.as_deref() != Some(bytes) {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ARTIFACT_READBACK_MISMATCH",
                format!(
                    "Kv tuning artifact {} did not read back byte-identically",
                    hex32(hash)
                ),
                "inspect the vault write path and repair durable storage before retrying",
            ));
        }
        Ok(())
    }

    fn anneal_clock(&self) -> Result<crate::SynapseCalyxClock, SynapseCalyxError> {
        crate::SynapseCalyxClock::from_tuning(&self.config.tuning)
    }
}

fn tuning_artifact_key() -> ArtifactKey {
    ArtifactKey::ConfigCache(tuning_artifact_hash(TUNING_ARTIFACT_KEY))
}

fn ensure_supported_candidate(
    incumbent: &SynapseCalyxTuningConfig,
    candidate: &SynapseCalyxTuningConfig,
) -> Result<(), SynapseCalyxError> {
    let mut expected = *incumbent;
    expected.fusion_k = candidate.fusion_k;
    expected.guard_far_identity = candidate.guard_far_identity;
    expected.guard_far_content = candidate.guard_far_content;
    expected.guard_far_stylistic = candidate.guard_far_stylistic;
    expected.index_m_max = candidate.index_m_max;
    expected.index_ef_construction = candidate.index_ef_construction;
    expected.index_beamwidth = candidate.index_beamwidth;
    expected.index_ef_search = candidate.index_ef_search;
    expected.index_alpha = candidate.index_alpha;
    if expected != *candidate {
        return Err(anneal_error(
            "SYNAPSE_CALYX_ANNEAL_TARGET_NOT_LOAD_BEARING",
            "candidate changes a field outside the fusion, guard-FAR, or persisted-index owners",
            "change only fields with a measured shadow consumer; inert and runtime-identity fields are never promotable",
        ));
    }
    Ok(())
}

fn ptr_config_hash(ptr: &ArtifactPtr) -> Result<[u8; 32], SynapseCalyxError> {
    match ptr {
        ArtifactPtr::ConfigCacheKeyHash(hash) => Ok(*hash),
        other => Err(anneal_error(
            "SYNAPSE_CALYX_ANNEAL_POINTER_KIND_INVALID",
            format!("Synapse tuning snapshot has incompatible pointer kind: {other:?}"),
            "repair the AnnealRollback snapshot to reference a ConfigCacheKeyHash tuning artifact",
        )),
    }
}

fn tuning_artifact_hash(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((TUNING_ARTIFACT_TAG.len() as u64).to_be_bytes());
    hasher.update(TUNING_ARTIFACT_TAG);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    hasher.finalize().into()
}

fn tuning_artifact_row_key(hash: [u8; 32]) -> Vec<u8> {
    let mut key = Vec::with_capacity(ARTIFACT_ROW_PREFIX.len() + hash.len());
    key.extend_from_slice(ARTIFACT_ROW_PREFIX);
    key.extend_from_slice(&hash);
    key
}

fn hex32(hash: [u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn anneal_error(
    code: &'static str,
    message: impl Into<String>,
    remediation: &'static str,
) -> SynapseCalyxError {
    SynapseCalyxError::new(code, message, remediation)
}

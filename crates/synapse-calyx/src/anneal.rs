use std::collections::BTreeMap;
use std::time::Instant;

use calyx_anneal::{
    ActionMetricSnapshot, AnnealAction, AnnealLedger, AnnealLedgerEntry, AnnealSubstrate,
    ArtifactKey, ArtifactPtr, AsterAnnealLedgerStore, AsterRollbackStorage, BudgetConfig,
    BudgetEnforcer, BudgetStatus, ChangeId, ChangeOutcome, HeldOutReplay, ReplayAnchor,
    ReplayQuery, RollbackStore, TripwireMetric, TripwireRegistry, TripwireStatus,
};
use calyx_aster::cf::ColumnFamily;
use calyx_aster::mvcc::Freshness;
use calyx_core::{CalyxError, SlotId, SlotVector};
use calyx_ledger::{ActorId, LedgerAppender};
use calyx_registry::VaultPanelState;
use calyx_search::{
    PersistedSearchGeneration, PersistedSearchIndexes, PersistedSearchManifestArtifact,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::{SynapseCalyxError, SynapseCalyxTuningConfig, SynapseCalyxVault};

const TUNING_ARTIFACT_TAG: &[u8] = b"synapse-anneal-tuning-v1";
const TUNING_ARTIFACT_KEY: &[u8] = b"synapse/live-tuning/v1";
const ARTIFACT_ROW_PREFIX: &[u8] = b"synapse/tuning-artifact/v1/";
const SEARCH_BINDING_ROW_PREFIX: &[u8] = b"synapse/anneal-search-binding/v1/";
const SEARCH_BINDING_SCHEMA: &str = "synapse_anneal_search_binding/v1";
const SEARCH_REPLAY_QUERIES_PER_SLOT: usize = 8;
const SEARCH_REPLAY_PASSES: usize = 9;
const SEARCH_REPLAY_K: usize = 3;

struct AnnealProposalOptions<'a> {
    metrics: Vec<TripwireMetric>,
    allow_persisted_index_change: bool,
    description: &'a str,
}

struct AnnealReaderLease<'a> {
    owner: &'a SynapseCalyxVault,
    lease_id: Option<u64>,
}

impl AnnealReaderLease<'_> {
    fn release(mut self) -> Result<(), SynapseCalyxError> {
        let lease_id = self.lease_id.take().ok_or_else(|| {
            anneal_error(
                "SYNAPSE_CALYX_ANNEAL_READER_LEASE_INVALID",
                "Anneal shared MVCC reader lease was already released",
                "repair the single-owner proposal flow before retrying",
            )
        })?;
        if !self.owner.vault.release_reader(lease_id) {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_READER_LEASE_LOST",
                format!("Anneal shared MVCC reader lease {lease_id} expired before readback"),
                "reduce the bounded shadow workload or increase its explicit lease after measuring production duration",
            ));
        }
        Ok(())
    }
}

impl Drop for AnnealReaderLease<'_> {
    fn drop(&mut self) {
        if let Some(lease_id) = self.lease_id.take()
            && !self.owner.vault.release_reader(lease_id)
        {
            tracing::error!(
                code = "SYNAPSE_CALYX_ANNEAL_READER_LEASE_LOST",
                lease_id,
                "Anneal shared MVCC reader lease expired or disappeared before release"
            );
        }
    }
}

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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynapseCalyxAnnealSearchReport {
    pub change: SynapseCalyxAnnealChangeReport,
    pub panel_version: u32,
    pub source_base_seq: u64,
    pub query_count: usize,
    pub incumbent_manifest_sha256: String,
    pub candidate_manifest_sha256: String,
    pub live_manifest_sha256_after: String,
    pub candidate_generation: PersistedSearchGeneration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnealSearchBinding {
    schema: String,
    tuning_artifact_sha256: String,
    candidate_key: [u8; 32],
    manifest: PersistedSearchManifestArtifact,
}

#[derive(Clone)]
struct MeasuredSearchAction {
    by_query: BTreeMap<u64, ActionMetricSnapshot>,
}

impl AnnealAction for MeasuredSearchAction {
    fn apply_shadow(&self, query: &ReplayQuery) -> calyx_core::Result<ActionMetricSnapshot> {
        self.by_query.get(&query.query_id).cloned().ok_or_else(|| {
            CalyxError::stale_derived(format!(
                "Anneal measured-search action has no physical measurement for replay query {}",
                query.query_id
            ))
        })
    }
}

impl SynapseCalyxVault {
    pub(crate) fn initialize_anneal_tuning(&self) -> Result<(), SynapseCalyxError> {
        let configured = self.config.tuning.clone().validate()?;
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
                .install_live_ptr(key.clone(), ArtifactPtr::ConfigCacheKeyHash(hash))
                .map_err(|error| {
                    SynapseCalyxError::from_calyx("install initial Anneal tuning pointer", &error)
                })?;
        }
        let (prior_hash, prior_bytes, effective) = self.read_live_tuning()?;
        let canonical_bytes = serde_json::to_vec(&effective).map_err(|error| {
            anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ARTIFACT_ENCODE_FAILED",
                format!("encode migrated live tuning artifact: {error}"),
                "repair the validated tuning serializer before reopening the vault",
            )
        })?;
        if canonical_bytes != prior_bytes {
            let canonical_hash = tuning_artifact_hash(&canonical_bytes);
            self.persist_tuning_artifact(canonical_hash, &canonical_bytes)?;
            rollback
                .install_live_ptr(key, ArtifactPtr::ConfigCacheKeyHash(canonical_hash))
                .map_err(|error| {
                    SynapseCalyxError::from_calyx("install migrated Anneal tuning pointer", &error)
                })?;
            let (observed_hash, observed_bytes, observed_tuning) = self.read_live_tuning()?;
            if observed_hash != canonical_hash
                || observed_bytes != canonical_bytes
                || observed_tuning != effective
            {
                return Err(anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_MIGRATION_READBACK_MISMATCH",
                    format!(
                        "migrated live tuning pointer readback {} does not match canonical {}",
                        hex32(observed_hash),
                        hex32(canonical_hash)
                    ),
                    "inspect the native AnnealRollback live pointer and Kv artifact rows before reopening the vault",
                ));
            }
            tracing::info!(
                code = "SYNAPSE_CALYX_ANNEAL_TUNING_SCHEMA_MIGRATED",
                prior_artifact_sha256 = %hex32(prior_hash),
                live_artifact_sha256 = %hex32(canonical_hash),
                "migrated durable Anneal tuning artifact after retiring inert fields"
            );
        }
        tracing::info!(
            code = "SYNAPSE_CALYX_ANNEAL_TUNING_OPENED",
            fusion_k = effective.fusion_k,
            index_ef_search = effective.index_ef_search,
            "resolved durable Anneal tuning pointer"
        );
        self.reconcile_live_search_bindings()?;
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
        self.anneal_propose_tuning_with_metrics(
            candidate_tuning,
            replay,
            candidate_action,
            incumbent_action,
            AnnealProposalOptions {
                metrics: calyx_anneal::ALL_SHADOW_METRICS.to_vec(),
                allow_persisted_index_change: false,
                description,
            },
        )
    }

    /// Builds and measures an immutable persisted-search candidate from the
    /// same pinned Base cut as the incumbent, then promotes only the measured
    /// tuning+manifest pair.
    pub fn anneal_propose_search_tuning(
        &self,
        candidate_tuning: SynapseCalyxTuningConfig,
        panel: &VaultPanelState,
        description: &str,
    ) -> Result<SynapseCalyxAnnealSearchReport, SynapseCalyxError> {
        let candidate_tuning = candidate_tuning.validate()?;
        let (prior_hash, _, incumbent_tuning) = self.read_live_tuning()?;
        ensure_index_only_candidate(&incumbent_tuning, &candidate_tuning)?;
        let snapshot = self.vault.pin_reader(Freshness::FreshDerived, 300_000);
        let reader_lease = AnnealReaderLease {
            owner: self,
            lease_id: Some(snapshot.lease().id()),
        };

        calyx_search::rebuild_for_vault_with_panel_state_and_dense_config_at_snapshot(
            &self.config.vault_dir,
            &self.vault,
            panel,
            incumbent_tuning.dense_index_config(),
            snapshot,
        )
        .map_err(|error| search_shadow_error("rebuild incumbent generation", error))?;
        let incumbent_artifact =
            calyx_search::read_live_manifest_artifact(&self.config.vault_dir, panel.panel.version)
                .map_err(|error| search_shadow_error("read incumbent manifest artifact", error))?;

        let candidate_bytes = serde_json::to_vec(&candidate_tuning).map_err(|error| {
            anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ARTIFACT_ENCODE_FAILED",
                format!("encode search candidate tuning artifact: {error}"),
                "repair the validated tuning serializer before proposing search tuning",
            )
        })?;
        let candidate_hash = tuning_artifact_hash(&candidate_bytes);
        let built =
            calyx_search::rebuild_candidate_for_vault_with_panel_state_and_dense_config_at_snapshot(
                &self.config.vault_dir,
                &self.vault,
                panel,
                candidate_tuning.dense_index_config(),
                candidate_hash,
                snapshot,
            )
            .map_err(|error| search_shadow_error("build candidate search generation", error))?;
        if built.generation.base_seq != incumbent_artifact.base_seq {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_SEARCH_CUT_CHANGED",
                format!(
                    "candidate Base seq {} differs from incumbent Base seq {}",
                    built.generation.base_seq, incumbent_artifact.base_seq
                ),
                "retry after writes quiesce; incumbent and candidate must be built from one exact Base cut",
            ));
        }
        let candidate_artifact = calyx_search::read_candidate_manifest_artifact(
            &self.config.vault_dir,
            panel.panel.version,
            candidate_hash,
            built.generation.base_seq,
        )
        .map_err(|error| search_shadow_error("read candidate manifest artifact", error))?;

        let incumbent_indexes =
            PersistedSearchIndexes::open(&self.config.vault_dir, panel.panel.version)
                .map_err(|error| search_shadow_error("open incumbent search generation", error))?;
        let candidate_indexes = PersistedSearchIndexes::open_candidate(
            &self.config.vault_dir,
            panel.panel.version,
            candidate_hash,
            built.generation.base_seq,
        )
        .map_err(|error| search_shadow_error("open candidate search generation", error))?;
        let (replay, query_slots) =
            self.build_search_replay(&candidate_indexes, &built.generation, snapshot)?;
        let candidate_action = measure_search_action(&candidate_indexes, &replay, &query_slots)?;
        let incumbent_action = measure_search_action(&incumbent_indexes, &replay, &query_slots)?;
        reader_lease.release()?;

        self.persist_search_binding(prior_hash, prior_hash, incumbent_artifact.clone())?;
        self.persist_search_binding(candidate_hash, candidate_hash, candidate_artifact.clone())?;
        let change = self.anneal_propose_tuning_with_metrics(
            candidate_tuning,
            replay.clone(),
            &candidate_action,
            &incumbent_action,
            AnnealProposalOptions {
                metrics: vec![TripwireMetric::RecallAtK, TripwireMetric::SearchP99],
                allow_persisted_index_change: true,
                description,
            },
        )?;

        if let ChangeOutcome::Promoted(change_id) = &change.outcome
            && let Err(error) = calyx_search::publish_live_manifest_artifact(
                &self.config.vault_dir,
                panel.panel.version,
                &candidate_artifact,
            )
        {
            let rollback_error = self.anneal_rollback(change_id.0).err();
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_SEARCH_PUBLISH_FAILED",
                format!(
                    "candidate tuning pointer promoted but its validated manifest could not publish: {error}; rollback_error={rollback_error:?}"
                ),
                "inspect the exact candidate and incumbent manifest bindings; the pointer was rolled back when possible and search fails closed on config mismatch",
            ));
        }
        let live =
            calyx_search::read_live_manifest_artifact(&self.config.vault_dir, panel.panel.version)
                .map_err(|error| {
                    search_shadow_error("read live manifest after search proposal", error)
                })?;
        let expected_live = if matches!(change.outcome, ChangeOutcome::Promoted(_)) {
            &candidate_artifact
        } else {
            &incumbent_artifact
        };
        if live != *expected_live {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_SEARCH_READBACK_MISMATCH",
                format!(
                    "live search manifest {} differs from expected {} after {:?}",
                    live.manifest_sha256, expected_live.manifest_sha256, change.outcome
                ),
                "inspect the live Anneal pointer and exact manifest binding before allowing search",
            ));
        }
        Ok(SynapseCalyxAnnealSearchReport {
            change,
            panel_version: panel.panel.version,
            source_base_seq: built.generation.base_seq,
            query_count: replay.queries.len(),
            incumbent_manifest_sha256: incumbent_artifact.manifest_sha256,
            candidate_manifest_sha256: candidate_artifact.manifest_sha256,
            live_manifest_sha256_after: live.manifest_sha256,
            candidate_generation: built.generation,
        })
    }

    fn anneal_propose_tuning_with_metrics<Candidate, Incumbent>(
        &self,
        candidate_tuning: SynapseCalyxTuningConfig,
        replay: HeldOutReplay,
        candidate_action: &Candidate,
        incumbent_action: &Incumbent,
        options: AnnealProposalOptions<'_>,
    ) -> Result<SynapseCalyxAnnealChangeReport, SynapseCalyxError>
    where
        Candidate: AnnealAction,
        Incumbent: AnnealAction,
    {
        let candidate_tuning = candidate_tuning.validate()?;
        let (_, prior_bytes, incumbent_tuning) = self.read_live_tuning()?;
        ensure_supported_candidate(&incumbent_tuning, &candidate_tuning)?;
        if !options.allow_persisted_index_change
            && incumbent_tuning.dense_index_config() != candidate_tuning.dense_index_config()
        {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_SEARCH_SHADOW_REQUIRED",
                "the generic Anneal transaction cannot change persisted-search index parameters",
                "use anneal_propose_search_tuning so Synapse builds, replays, measures, binds, and atomically publishes the candidate generation",
            ));
        }
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
            AnnealSubstrate::new(tripwires, replay, rollback, ledger, budget, &clock)
                .with_shadow_metrics(options.metrics);
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
                options.description,
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

    fn build_search_replay(
        &self,
        indexes: &PersistedSearchIndexes,
        generation: &PersistedSearchGeneration,
        snapshot: calyx_aster::mvcc::Snapshot,
    ) -> Result<(HeldOutReplay, BTreeMap<u64, SlotId>), SynapseCalyxError> {
        (|| {
            let dense_slots = generation
                .slots
                .iter()
                .filter(|slot| matches!(slot.shape, calyx_core::SlotShape::Dense(_)))
                .map(|slot| slot.panel_slot.slot_id())
                .collect::<Vec<_>>();
            if dense_slots.is_empty() {
                return Err(anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_SEARCH_NO_DENSE_LANES",
                    format!(
                        "panel {} candidate generation has no dense lane to evaluate",
                        generation.panel_version
                    ),
                    "propose persisted-index tuning only for a panel with a real dense generation",
                ));
            }
            let mut queries = Vec::new();
            let mut query_slots = BTreeMap::new();
            let mut query_id = 1_u64;
            for slot in dense_slots {
                let ids = indexes.dense_ids(slot).map_err(|error| {
                    search_shadow_error(&format!("read candidate ids for slot {slot}"), error)
                })?;
                for cx_id in ids.into_iter().take(SEARCH_REPLAY_QUERIES_PER_SLOT) {
                    let constellation = self
                        .vault
                        .get_selected_slots_at_snapshot(cx_id, snapshot, [slot])
                        .map_err(|error| {
                            SynapseCalyxError::from_calyx(
                                "read pinned Anneal replay vector",
                                &error,
                            )
                        })?;
                    let vector = constellation.slots.get(&slot).ok_or_else(|| {
                        anneal_error(
                            "SYNAPSE_CALYX_ANNEAL_SEARCH_REPLAY_MISSING",
                            format!("pinned replay row {cx_id} lost dense slot {slot}"),
                            "repair the Base/slot atomicity violation before tuning search",
                        )
                    })?;
                    let SlotVector::Dense { data, .. } = vector else {
                        return Err(anneal_error(
                            "SYNAPSE_CALYX_ANNEAL_SEARCH_REPLAY_SHAPE",
                            format!("candidate dense slot {slot} row {cx_id} is not dense"),
                            "rebuild the exact panel after repairing its slot-shape contract",
                        ));
                    };
                    let expected = indexes
                        .exact_dense_search(slot, vector, SEARCH_REPLAY_K)
                        .map_err(|error| {
                            search_shadow_error(
                                &format!("derive exact replay rank for slot {slot}"),
                                error,
                            )
                        })?;
                    if expected.is_empty() {
                        return Err(anneal_error(
                            "SYNAPSE_CALYX_ANNEAL_SEARCH_REPLAY_EMPTY",
                            format!("exact replay for slot {slot} row {cx_id} returned no hit"),
                            "repair the candidate id map/raw sidecar before shadow evaluation",
                        ));
                    }
                    query_slots.insert(query_id, slot);
                    queries.push(ReplayQuery {
                        query_id,
                        query_vector: data.clone(),
                        expected_top_k: expected
                            .into_iter()
                            .map(|hit| ReplayAnchor {
                                cx_id: hit.cx_id,
                                similarity: hit.score,
                            })
                            .collect(),
                    });
                    query_id = query_id.checked_add(1).ok_or_else(|| {
                        anneal_error(
                            "SYNAPSE_CALYX_ANNEAL_SEARCH_REPLAY_OVERFLOW",
                            "search replay query id overflowed u64",
                            "reduce the bounded replay corpus and retry",
                        )
                    })?;
                }
            }
            if queries.is_empty() {
                return Err(anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_SEARCH_REPLAY_EMPTY",
                    "candidate dense generations contain no indexed identities",
                    "ingest and rebuild a real dense panel before tuning search",
                ));
            }
            Ok((
                HeldOutReplay {
                    queries,
                    seed: self.config.tuning.rng_seed,
                },
                query_slots,
            ))
        })()
    }

    fn persist_search_binding(
        &self,
        tuning_hash: [u8; 32],
        candidate_key: [u8; 32],
        manifest: PersistedSearchManifestArtifact,
    ) -> Result<(), SynapseCalyxError> {
        let binding = AnnealSearchBinding {
            schema: SEARCH_BINDING_SCHEMA.to_owned(),
            tuning_artifact_sha256: hex32(tuning_hash),
            candidate_key,
            manifest,
        };
        let key = search_binding_row_key(tuning_hash, binding.manifest.panel_version);
        let bytes = serde_json::to_vec(&binding).map_err(|error| {
            anneal_error(
                "SYNAPSE_CALYX_ANNEAL_SEARCH_BINDING_ENCODE_FAILED",
                format!("encode search generation binding: {error}"),
                "repair the strict binding serializer before retrying",
            )
        })?;
        if let Some(existing) = self
            .vault
            .read_cf_latest(ColumnFamily::Kv, &key)
            .map_err(|error| SynapseCalyxError::from_calyx("read Anneal search binding", &error))?
        {
            if existing != bytes {
                return Err(anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_SEARCH_BINDING_CONFLICT",
                    format!(
                        "tuning artifact {} panel {} already binds different manifest bytes",
                        binding.tuning_artifact_sha256, binding.manifest.panel_version
                    ),
                    "stop and inspect the content-addressed binding; one tuning hash cannot identify two search generations",
                ));
            }
            return Ok(());
        }
        self.vault
            .write_cf(ColumnFamily::Kv, key.clone(), bytes.clone())
            .map_err(|error| {
                SynapseCalyxError::from_calyx("write Anneal search binding", &error)
            })?;
        let observed = self
            .vault
            .read_cf_latest(ColumnFamily::Kv, &key)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read back Anneal search binding", &error)
            })?;
        if observed.as_deref() != Some(bytes.as_slice()) {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_SEARCH_BINDING_READBACK_MISMATCH",
                format!(
                    "tuning artifact {} panel {} binding did not read back byte-identically",
                    binding.tuning_artifact_sha256, binding.manifest.panel_version
                ),
                "inspect the native Kv commit before allowing optimizer promotion",
            ));
        }
        Ok(())
    }

    fn search_bindings_for_hash(
        &self,
        tuning_hash: [u8; 32],
    ) -> Result<Vec<AnnealSearchBinding>, SynapseCalyxError> {
        let prefix = search_binding_hash_prefix(tuning_hash);
        let range = prefix_range(&prefix);
        self.vault
            .scan_cf_range_latest(ColumnFamily::Kv, &range)
            .map_err(|error| SynapseCalyxError::from_calyx("scan Anneal search bindings", &error))?
            .into_iter()
            .map(|(_key, bytes)| decode_search_binding(&bytes, tuning_hash))
            .collect()
    }

    fn reconcile_live_search_bindings(&self) -> Result<(), SynapseCalyxError> {
        let (live_hash, _, tuning) = self.read_live_tuning()?;
        for binding in self.search_bindings_for_hash(live_hash)? {
            if binding.manifest.manifest_bytes.is_empty() {
                return Err(anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_SEARCH_BINDING_INVALID",
                    format!(
                        "live tuning {} panel {} binds an empty manifest",
                        hex32(live_hash),
                        binding.manifest.panel_version
                    ),
                    "restore the exact manifest binding before reopening the vault",
                ));
            }
            let current = calyx_search::read_live_manifest_artifact(
                &self.config.vault_dir,
                binding.manifest.panel_version,
            )
            .ok();
            if current.as_ref() != Some(&binding.manifest) {
                calyx_search::publish_live_manifest_artifact(
                    &self.config.vault_dir,
                    binding.manifest.panel_version,
                    &binding.manifest,
                )
                .map_err(|error| {
                    search_shadow_error("reconcile live Anneal search manifest", error)
                })?;
                tracing::warn!(
                    code = "SYNAPSE_CALYX_ANNEAL_SEARCH_MANIFEST_RECONCILED",
                    tuning_artifact_sha256 = %hex32(live_hash),
                    panel_version = binding.manifest.panel_version,
                    manifest_sha256 = %binding.manifest.manifest_sha256,
                    "reconciled a durable tuning pointer with its exact validated search manifest after an interrupted promotion"
                );
            }
            let generation = PersistedSearchIndexes::open(
                &self.config.vault_dir,
                binding.manifest.panel_version,
            )
            .and_then(|indexes| indexes.generation())
            .map_err(|error| search_shadow_error("reopen reconciled search manifest", error))?;
            if generation.dense_index_config != tuning.dense_index_config() {
                return Err(anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_SEARCH_CONFIG_MISMATCH",
                    format!(
                        "live tuning {} dense config {:?} differs from panel {} manifest {:?}",
                        hex32(live_hash),
                        tuning.dense_index_config(),
                        binding.manifest.panel_version,
                        generation.dense_index_config
                    ),
                    "restore the manifest bound to the live tuning artifact; never query an index under different build/search parameters",
                ));
            }
        }
        Ok(())
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
        let prior_hash = ptr_config_hash(&snapshot.prior_ptr)?;
        let (live_hash, _, _) = self.read_live_tuning()?;
        if live_hash != candidate_hash {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ROLLBACK_STALE",
                format!(
                    "change {change_id} candidate {} is not live; live tuning is {}",
                    hex32(candidate_hash),
                    hex32(live_hash)
                ),
                "read anneal status and roll back only the currently live uncommitted promoted change",
            ));
        }
        let prior_bindings = self.search_bindings_for_hash(prior_hash)?;
        let candidate_bindings = self.search_bindings_for_hash(candidate_hash)?;
        for binding in &prior_bindings {
            calyx_search::publish_live_manifest_artifact(
                &self.config.vault_dir,
                binding.manifest.panel_version,
                &binding.manifest,
            )
            .map_err(|error| search_shadow_error("restore prior search manifest", error))?;
        }
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
        if let Err(error) = substrate.rollback_explicit(ChangeId(change_id)) {
            let mut restore_failures = Vec::new();
            for binding in &candidate_bindings {
                if let Err(restore) = calyx_search::publish_live_manifest_artifact(
                    &self.config.vault_dir,
                    binding.manifest.panel_version,
                    &binding.manifest,
                ) {
                    restore_failures.push(format!(
                        "panel {}: {restore}",
                        binding.manifest.panel_version
                    ));
                }
            }
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ROLLBACK_FAILED",
                format!(
                    "native tuning rollback failed: {error}; candidate_manifest_restore_failures={restore_failures:?}"
                ),
                "inspect the live Anneal pointer and bound search manifests; search fails closed while their configs disagree",
            ));
        }
        let (restored_hash, restored_bytes, _) = self.read_live_tuning()?;
        if restored_hash != prior_hash {
            return Err(anneal_error(
                "SYNAPSE_CALYX_ANNEAL_ROLLBACK_READBACK_MISMATCH",
                format!(
                    "rollback restored tuning {}, expected {}",
                    hex32(restored_hash),
                    hex32(prior_hash)
                ),
                "inspect the native AnnealRollback live pointer before allowing tuned operations",
            ));
        }
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
        let tuning = decode_tuning_artifact(&bytes, hash)?;
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

fn decode_tuning_artifact(
    bytes: &[u8],
    hash: [u8; 32],
) -> Result<SynapseCalyxTuningConfig, SynapseCalyxError> {
    match serde_json::from_slice::<SynapseCalyxTuningConfig>(bytes) {
        Ok(tuning) => tuning.validate(),
        Err(direct_error) => {
            let mut value = serde_json::from_slice::<Value>(bytes).map_err(|error| {
                anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_ARTIFACT_DECODE_FAILED",
                    format!("decode live tuning artifact {}: {error}", hex32(hash)),
                    "restore a valid versioned Synapse tuning artifact and pointer",
                )
            })?;
            let object = value.as_object_mut().ok_or_else(|| {
                anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_ARTIFACT_DECODE_FAILED",
                    format!(
                        "decode live tuning artifact {}: expected a JSON object; original error: {direct_error}",
                        hex32(hash)
                    ),
                    "restore a valid versioned Synapse tuning artifact and pointer",
                )
            })?;
            let mut removed = Vec::new();
            for key in [
                "bit_floor_bits",
                "correlation_ceiling",
                "guard_cold_start_tau",
                "kernel_fraction",
                "kernel_recall_gate",
                "temporal_boost_min",
                "temporal_boost_max",
            ] {
                if object.remove(key).is_some() {
                    removed.push(key);
                }
            }
            if removed.is_empty() {
                return Err(anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_ARTIFACT_DECODE_FAILED",
                    format!(
                        "decode live tuning artifact {}: {direct_error}",
                        hex32(hash)
                    ),
                    "restore a valid versioned Synapse tuning artifact and pointer",
                ));
            }
            serde_json::from_value::<SynapseCalyxTuningConfig>(value)
                .map_err(|error| {
                    anneal_error(
                        "SYNAPSE_CALYX_ANNEAL_ARTIFACT_DECODE_FAILED",
                        format!(
                            "decode legacy live tuning artifact {} after removing retired fields {removed:?}: {error}",
                            hex32(hash)
                        ),
                        "restore a valid versioned Synapse tuning artifact and pointer; only the documented retired fields are migrated",
                    )
                })?
                .validate()
        }
    }
}

fn tuning_artifact_key() -> ArtifactKey {
    ArtifactKey::ConfigCache(tuning_artifact_hash(TUNING_ARTIFACT_KEY))
}

fn ensure_supported_candidate(
    incumbent: &SynapseCalyxTuningConfig,
    candidate: &SynapseCalyxTuningConfig,
) -> Result<(), SynapseCalyxError> {
    let mut expected = incumbent.clone();
    expected.fusion_k = candidate.fusion_k;
    expected.fusion_slot_weights = candidate.fusion_slot_weights.clone();
    expected.guard_far_identity = candidate.guard_far_identity;
    expected.guard_far_content = candidate.guard_far_content;
    expected.guard_far_stylistic = candidate.guard_far_stylistic;
    expected.index_m_max = candidate.index_m_max;
    expected.index_ef_construction = candidate.index_ef_construction;
    expected.index_beamwidth = candidate.index_beamwidth;
    expected.index_ef_search = candidate.index_ef_search;
    expected.index_alpha = candidate.index_alpha;
    expected.index_quant_bits_by_slot = candidate.index_quant_bits_by_slot.clone();
    if expected != *candidate {
        return Err(anneal_error(
            "SYNAPSE_CALYX_ANNEAL_TARGET_NOT_LOAD_BEARING",
            "candidate changes a field outside the fusion, guard-FAR, or persisted-index owners",
            "change only fields with a measured shadow consumer; inert and runtime-identity fields are never promotable",
        ));
    }
    Ok(())
}

fn ensure_index_only_candidate(
    incumbent: &SynapseCalyxTuningConfig,
    candidate: &SynapseCalyxTuningConfig,
) -> Result<(), SynapseCalyxError> {
    let mut expected = incumbent.clone();
    expected.index_m_max = candidate.index_m_max;
    expected.index_ef_construction = candidate.index_ef_construction;
    expected.index_beamwidth = candidate.index_beamwidth;
    expected.index_ef_search = candidate.index_ef_search;
    expected.index_alpha = candidate.index_alpha;
    expected.index_quant_bits_by_slot = candidate.index_quant_bits_by_slot.clone();
    if expected != *candidate {
        return Err(anneal_error(
            "SYNAPSE_CALYX_ANNEAL_SEARCH_TARGET_MIXED",
            "search-generation proposal changes a non-index tuning owner",
            "propose fusion or guard tuning in its own measured transaction; one shadow change owns one artifact family",
        ));
    }
    if incumbent.dense_index_config() == candidate.dense_index_config() {
        return Err(anneal_error(
            "SYNAPSE_CALYX_ANNEAL_CANDIDATE_UNCHANGED",
            "search-generation proposal does not change persisted dense index configuration",
            "change at least one load-bearing index build/search parameter",
        ));
    }
    Ok(())
}

fn measure_search_action(
    indexes: &PersistedSearchIndexes,
    replay: &HeldOutReplay,
    query_slots: &BTreeMap<u64, SlotId>,
) -> Result<MeasuredSearchAction, SynapseCalyxError> {
    let mut by_query = BTreeMap::new();
    for query in &replay.queries {
        let slot = query_slots.get(&query.query_id).copied().ok_or_else(|| {
            anneal_error(
                "SYNAPSE_CALYX_ANNEAL_SEARCH_REPLAY_SLOT_MISSING",
                format!("replay query {} has no bound dense slot", query.query_id),
                "rebuild the product-owned replay; callers cannot supply query/slot bindings",
            )
        })?;
        let vector = SlotVector::Dense {
            dim: u32::try_from(query.query_vector.len()).map_err(|error| {
                anneal_error(
                    "SYNAPSE_CALYX_ANNEAL_SEARCH_REPLAY_DIM_OVERFLOW",
                    format!("query {} dimension overflow: {error}", query.query_id),
                    "reduce the lens dimension to the declared u32 slot shape",
                )
            })?,
            data: query.query_vector.clone(),
        };
        let expected = query
            .expected_top_k
            .iter()
            .map(|anchor| anchor.cx_id)
            .collect::<Vec<_>>();
        let mut elapsed = Vec::with_capacity(SEARCH_REPLAY_PASSES);
        let mut observed = Vec::new();
        for _ in 0..SEARCH_REPLAY_PASSES {
            let started = Instant::now();
            let hits = indexes
                .search(slot, &vector, expected.len())
                .map_err(|error| {
                    search_shadow_error(
                        &format!("measure replay query {} slot {slot}", query.query_id),
                        error,
                    )
                })?;
            elapsed.push(started.elapsed().as_secs_f64() * 1_000.0);
            observed = hits.into_iter().map(|hit| hit.cx_id).collect();
        }
        elapsed.sort_by(f64::total_cmp);
        let matched = observed.iter().filter(|id| expected.contains(id)).count();
        let recall = matched as f64 / expected.len() as f64;
        let p99 = *elapsed.last().ok_or_else(|| {
            anneal_error(
                "SYNAPSE_CALYX_ANNEAL_SEARCH_MEASUREMENT_EMPTY",
                format!("query {} produced no timing samples", query.query_id),
                "repair the fixed replay pass count before optimizer evaluation",
            )
        })?;
        by_query.insert(
            query.query_id,
            ActionMetricSnapshot::from_values([
                (TripwireMetric::RecallAtK, recall),
                (TripwireMetric::SearchP99, p99),
            ]),
        );
    }
    Ok(MeasuredSearchAction { by_query })
}

fn search_binding_hash_prefix(tuning_hash: [u8; 32]) -> Vec<u8> {
    let mut key = Vec::with_capacity(SEARCH_BINDING_ROW_PREFIX.len() + 32);
    key.extend_from_slice(SEARCH_BINDING_ROW_PREFIX);
    key.extend_from_slice(&tuning_hash);
    key
}

fn search_binding_row_key(tuning_hash: [u8; 32], panel_version: u32) -> Vec<u8> {
    let mut key = search_binding_hash_prefix(tuning_hash);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key
}

fn prefix_range(prefix: &[u8]) -> calyx_aster::cf::KeyRange {
    let mut end = prefix.to_vec();
    for index in (0..end.len()).rev() {
        if end[index] != u8::MAX {
            end[index] += 1;
            end.truncate(index + 1);
            return calyx_aster::cf::KeyRange {
                start: prefix.to_vec(),
                end: Some(end),
            };
        }
    }
    calyx_aster::cf::KeyRange {
        start: prefix.to_vec(),
        end: None,
    }
}

fn decode_search_binding(
    bytes: &[u8],
    expected_hash: [u8; 32],
) -> Result<AnnealSearchBinding, SynapseCalyxError> {
    let binding = serde_json::from_slice::<AnnealSearchBinding>(bytes).map_err(|error| {
        anneal_error(
            "SYNAPSE_CALYX_ANNEAL_SEARCH_BINDING_DECODE_FAILED",
            format!("decode strict search generation binding: {error}"),
            "restore the exact binding bytes or rebuild the optimizer candidate",
        )
    })?;
    if binding.schema != SEARCH_BINDING_SCHEMA
        || binding.tuning_artifact_sha256 != hex32(expected_hash)
        || binding.candidate_key != expected_hash
    {
        return Err(anneal_error(
            "SYNAPSE_CALYX_ANNEAL_SEARCH_BINDING_INVALID",
            format!(
                "search binding identity disagrees with tuning artifact {}: schema={} tuning={} candidate={}",
                hex32(expected_hash),
                binding.schema,
                binding.tuning_artifact_sha256,
                hex32(binding.candidate_key)
            ),
            "restore a binding whose schema, tuning hash, and candidate key all match its Kv key",
        ));
    }
    Ok(binding)
}

fn search_shadow_error(action: &str, error: calyx_search::SearchError) -> SynapseCalyxError {
    SynapseCalyxError::new(
        error.code(),
        format!("{action}: {}", error.message()),
        error.remediation().unwrap_or(
            "inspect the exact live/candidate search artifacts and rebuild from authoritative Base rows",
        ),
    )
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

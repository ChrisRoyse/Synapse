mod checkpointing;
mod manifest_ops;
mod stale_sst_temp;

pub(in crate::vault) use checkpointing::CHECKPOINT_DRAIN_MAX_BATCHES;
pub(in crate::vault) mod recovery_readback;
pub(in crate::vault) mod router_coverage;

use recovery_readback::read_manifested_batches;

use super::encode::{WriteRow, decode_write_batch, encode_write_batch};
use crate::cf::ColumnFamily;
use crate::compaction::TieringPolicy;
use crate::dedup::DedupPolicy;
use crate::manifest::{recover_vault, recover_vault_metadata};
use crate::pressure::DiskPressureGuard;
use crate::resource::ResourceCounters;
use crate::security::value_crypto::{SharedVaultContext, open_rows, seal_rows};
use crate::timetravel::RetentionHorizon;
use crate::wal::{GroupCommitBatcher, ReplayRecord, WalOptions, WalRecycleReport, replay_dir};
use calyx_core::{CalyxError, Panel, Result, SystemClock, TemporalPolicy};
use calyx_ledger::CheckpointConfig;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Optional durable-recovery progress hook: `(bytes_replayed, bytes_total)`.
///
/// The hook fires only as WAL replay genuinely advances, allowing embedding
/// runtimes to distinguish slow recovery from a frozen open.
#[derive(Clone)]
pub struct RecoveryProgressHook(Arc<dyn Fn(u64, u64) + Send + Sync>);

impl RecoveryProgressHook {
    pub fn new(hook: impl Fn(u64, u64) + Send + Sync + 'static) -> Self {
        Self(Arc::new(hook))
    }

    fn report(&self, bytes_replayed: u64, bytes_total: u64) {
        (self.0)(bytes_replayed, bytes_total);
    }
}

impl std::fmt::Debug for RecoveryProgressHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryProgressHook")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct VaultOptions {
    pub wal_options: WalOptions,
    pub memtable_byte_cap: usize,
    pub tiering_policy: Option<TieringPolicy>,
    pub ledger_checkpoint: Option<CheckpointConfig>,
    pub temporal_policy: Option<TemporalPolicy>,
    pub dedup_policy: Option<DedupPolicy>,
    pub retention_horizon: RetentionHorizon,
    pub panel: Option<Panel>,
    /// Optional data-residency pin (PRD `30 §4`). When set, the vault's storage
    /// location is pinned and off-dataset writes/copies fail closed.
    pub residency: Option<crate::residency::Residency>,
    pub disk_pressure_guard: Option<DiskPressureGuard>,
    /// Optional authenticated value encryption context for durable bytes.
    pub value_crypto: Option<SharedVaultContext>,
    /// Restores checkpointed durable-batch/compacted SST rows plus the WAL
    /// tail into the in-memory MVCC table on open. Router memtable-flush SSTs
    /// carry flush ordinals, not commit seqs, so they are never restored; the
    /// open fails closed with `CALYX_ASTER_ROUTER_ONLY_ROWS` if any router
    /// row lacks a commit-domain durable home (issue #1132). Disable for
    /// latest-read workloads that use the CF router as the source of truth
    /// and do not request historical reads.
    pub restore_mvcc_rows: bool,
    /// Builds validated SST point-lookup metadata during router open. Keep this
    /// enabled for full-restore handles that must prove every historical row at
    /// open; disable for latest-readback startup so large routers validate
    /// lazily when a read touches the relevant SST.
    pub eager_router_lookup_on_open: bool,
    /// Restores the full in-memory ledger hook on open. Disable only for
    /// explicitly read-only handles that verify/search latest state without
    /// appending ledger entries.
    pub restore_ledger_hook: bool,
    /// Opens the vault as a read-only handle. Any write through this handle
    /// fails before WAL append or MVCC mutation.
    pub read_only: bool,
    /// Restricts router recovery to a concrete CF set for read-only handles.
    /// This keeps analytical/search reads from enumerating unrelated large CFs.
    pub selected_cfs: Option<Vec<ColumnFamily>>,
    /// Optional callback fired as WAL payload bytes are decoded during open.
    pub recovery_progress: Option<RecoveryProgressHook>,
}

impl Default for VaultOptions {
    fn default() -> Self {
        Self {
            wal_options: WalOptions::default(),
            memtable_byte_cap: 0,
            tiering_policy: None,
            ledger_checkpoint: Some(CheckpointConfig::default()),
            temporal_policy: Some(TemporalPolicy::default()),
            dedup_policy: Some(DedupPolicy::default()),
            retention_horizon: RetentionHorizon::default(),
            panel: None,
            residency: None,
            disk_pressure_guard: None,
            value_crypto: None,
            restore_mvcc_rows: true,
            eager_router_lookup_on_open: true,
            restore_ledger_hook: true,
            read_only: false,
            selected_cfs: None,
            recovery_progress: None,
        }
    }
}

#[derive(Debug)]
pub(super) struct DurableVault {
    root: PathBuf,
    batcher: GroupCommitBatcher,
    tiering_policy: Option<TieringPolicy>,
    ledger_checkpoint: Option<CheckpointConfig>,
    temporal_policy: Option<TemporalPolicy>,
    dedup_policy: Option<DedupPolicy>,
    retention_horizon: Mutex<RetentionHorizon>,
    panel: Option<Panel>,
    disk_pressure_guard: Option<DiskPressureGuard>,
    value_crypto: Option<SharedVaultContext>,
    pending_checkpoint: Mutex<Vec<(u64, Vec<WriteRow>)>>,
    /// Max checkpointed seq whose batch wrote a persistent-search-input CF
    /// (issues #1100 and #1808); persisted into every manifest write as
    /// `derived_content_seq`, clamped to that manifest's `durable_seq`.
    checkpointed_derived_content_seq: AtomicU64,
    /// Panel-scoped counterpart persisted by watermark model 3 (#1841).
    /// Values may include live WAL-tail sequences; manifest serialization
    /// clamps them to the manifest's durable floor, which is conservative.
    checkpointed_panel_content_seqs: Mutex<BTreeMap<u32, u64>>,
}

pub(super) struct RecoveredBatch {
    pub seq: u64,
    pub rows: Vec<WriteRow>,
}

pub(super) struct RecoveredBatches {
    pub batches: Vec<RecoveredBatch>,
    pub last_recovered_seq: u64,
    pub wal_replay_floor_seq: u64,
    /// Durably recorded derived-content watermark floor for seqs at or below
    /// `wal_replay_floor_seq`; WAL replay re-derives the rest per batch.
    pub derived_content_floor_seq: u64,
    /// Exact panel-scoped floors vouched for by a model-3 manifest.
    pub panel_content_floor_seqs: BTreeMap<u32, u64>,
    /// Active panel version decoded from the manifest's hash-verified panel
    /// asset. Required when migrating a latest-readback handle with no restored
    /// Base rows from the global watermark model.
    pub active_panel_version: Option<u32>,
    /// The pointed manifest predates the exact persistent-search-input model;
    /// open must re-derive and prove its floor from validated relevant levels.
    pub migrate_derived_content_model: bool,
    pub torn_tail: Option<crate::wal::TornTail>,
    pub temporal_policy: Option<TemporalPolicy>,
    pub dedup_policy: Option<DedupPolicy>,
    pub retention_horizon: RetentionHorizon,
    pub router_latest_readback: bool,
    /// A post-manifest WAL tail intentionally omitted from `batches`; open must
    /// replay it through the bounded authenticated stream and stage it durably.
    pub wal_tail_stream_floor: Option<u64>,
}

impl DurableVault {
    pub(super) fn validate_options(options: &VaultOptions) -> Result<()> {
        if let Some(policy) = &options.temporal_policy {
            policy.validate()?;
        }
        if let Some(policy) = &options.dedup_policy {
            validate_dedup_policy(policy, options.panel.as_ref())?;
        }
        options.retention_horizon.validate()?;
        if !options.restore_ledger_hook && !options.read_only {
            return Err(CalyxError {
                code: "CALYX_VAULT_OPTIONS_INVALID",
                message:
                    "restore_ledger_hook=false requires read_only=true to prevent unverified writes"
                        .to_string(),
                remediation: "open read workloads with read_only=true, or keep restore_ledger_hook=true for write-capable handles",
            });
        }
        if options.read_only && options.residency.is_some() {
            return Err(CalyxError {
                code: "CALYX_VAULT_OPTIONS_INVALID",
                message: "read_only=true cannot persist a new residency pin".to_string(),
                remediation: "persist residency with a write-capable open before opening read-only handles",
            });
        }
        if options.selected_cfs.is_some() && !options.read_only {
            return Err(CalyxError {
                code: "CALYX_VAULT_OPTIONS_INVALID",
                message: "selected_cfs requires read_only=true to prevent partial write handles"
                    .to_string(),
                remediation: "open full write-capable vault handles without selected_cfs, or mark the handle read_only=true",
            });
        }
        if options.selected_cfs.as_ref().is_some_and(Vec::is_empty) {
            return Err(CalyxError {
                code: "CALYX_VAULT_OPTIONS_INVALID",
                message: "selected_cfs cannot be empty".to_string(),
                remediation: "omit selected_cfs or include every CF required by the read workload",
            });
        }
        Ok(())
    }

    pub(super) fn open_after(
        root: impl AsRef<Path>,
        options: &VaultOptions,
        wal_replay_floor_seq: u64,
        derived_content_floor_seq: u64,
        panel_content_floor_seqs: BTreeMap<u32, u64>,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        Self::validate_options(options)?;
        fs::create_dir_all(root.join("cf"))
            .map_err(|error| storage_error("create durable CF root", error))?;
        if let Some(policy) = &options.tiering_policy {
            for tier_root in policy.tier_roots() {
                fs::create_dir_all(tier_root.join("cf"))
                    .map_err(|error| storage_error("create tiered durable CF root", error))?;
            }
        }
        let wal = crate::wal::Wal::open_after(
            root.join("wal"),
            options.wal_options,
            wal_replay_floor_seq,
        )?;
        let batcher = GroupCommitBatcher::new(
            wal,
            options.wal_options.group_commit_window,
            Arc::new(SystemClock),
        )?;
        let durable = Self {
            root,
            batcher,
            tiering_policy: options.tiering_policy.clone(),
            ledger_checkpoint: options.ledger_checkpoint.clone(),
            temporal_policy: options.temporal_policy,
            dedup_policy: options.dedup_policy.clone(),
            retention_horizon: Mutex::new(options.retention_horizon.clone()),
            panel: options.panel.clone(),
            disk_pressure_guard: options.disk_pressure_guard.clone(),
            value_crypto: options.value_crypto.clone(),
            pending_checkpoint: Mutex::new(Vec::new()),
            checkpointed_derived_content_seq: AtomicU64::new(0),
            checkpointed_panel_content_seqs: Mutex::new(panel_content_floor_seqs),
        };
        durable
            .checkpointed_derived_content_seq
            .store(derived_content_floor_seq, Ordering::Release);
        if durable.panel.is_some() && !durable.root.join("CURRENT").exists() {
            durable.write_manifest_with_seq(1, 0)?;
        }
        Ok(durable)
    }

    pub(super) fn recover_batches(
        root: impl AsRef<Path>,
        options: &VaultOptions,
    ) -> Result<RecoveredBatches> {
        Self::validate_options(options)?;
        let root = root.as_ref();
        tracing::info!(
            code = "CALYX_ASTER_RECOVERY_START",
            vault_dir = %root.display(),
            restore_mvcc_rows = options.restore_mvcc_rows,
            eager_router_lookup_on_open = options.eager_router_lookup_on_open,
            restore_ledger_hook = options.restore_ledger_hook,
            read_only = options.read_only,
            selected_cfs = ?options.selected_cfs,
            "starting Calyx Aster recovery"
        );
        if !options.read_only {
            stale_sst_temp::reclaim_stale_sst_temps(root, options.tiering_policy.as_ref())?;
        }
        if root.join("CURRENT").exists() {
            if !options.restore_mvcc_rows && !options.read_only && options.value_crypto.is_none() {
                let recovery = recover_vault_metadata(root)?;
                if let Some(policy) = &recovery.manifest.dedup_policy {
                    validate_dedup_policy(policy, options.panel.as_ref())?;
                }
                let migrate_derived_content_model =
                    !recovery.manifest.uses_persistent_search_content_model();
                let active_panel_version =
                    read_manifest_panel_version(root, &recovery.manifest.panel_ref)?;
                tracing::info!(
                    code = "CALYX_ASTER_RECOVERY_STREAM_PLAN",
                    vault_dir = %root.display(),
                    durable_seq = recovery.manifest.durable_seq,
                    last_recovered_seq = recovery.last_recovered_seq,
                    active_panel_version,
                    torn_tail = ?recovery.torn_tail,
                    "planned bounded WAL-tail streaming recovery"
                );
                return Ok(RecoveredBatches {
                    batches: Vec::new(),
                    last_recovered_seq: recovery.last_recovered_seq,
                    wal_replay_floor_seq: recovery.manifest.durable_seq,
                    derived_content_floor_seq: if migrate_derived_content_model {
                        0
                    } else {
                        recovery.manifest.effective_derived_content_seq()
                    },
                    panel_content_floor_seqs: if migrate_derived_content_model {
                        BTreeMap::new()
                    } else {
                        recovery.manifest.panel_content_seqs
                    },
                    active_panel_version,
                    migrate_derived_content_model,
                    torn_tail: recovery.torn_tail,
                    temporal_policy: recovery.manifest.temporal_policy,
                    dedup_policy: recovery.manifest.dedup_policy,
                    retention_horizon: recovery.manifest.retention_horizon,
                    router_latest_readback: true,
                    wal_tail_stream_floor: Some(recovery.manifest.durable_seq),
                });
            }
            let recovery = recover_vault(root)?;
            tracing::info!(
                code = "CALYX_ASTER_RECOVERY_MANIFEST_LOADED",
                vault_dir = %root.display(),
                manifest_seq = recovery.manifest.manifest_seq,
                durable_seq = recovery.manifest.durable_seq,
                derived_content_seq = recovery.manifest.effective_derived_content_seq(),
                derived_content_model = ?recovery.manifest.derived_content_model,
                panel_content_watermark_count = recovery.manifest.panel_content_seqs.len(),
                wal_record_count = recovery.wal_records.len(),
                restore_mvcc_rows = options.restore_mvcc_rows,
                eager_router_lookup_on_open = options.eager_router_lookup_on_open,
                "loaded Calyx Aster manifest recovery state"
            );
            if let Some(policy) = &recovery.manifest.dedup_policy {
                validate_dedup_policy(policy, options.panel.as_ref())?;
            }
            let router_latest_readback = !options.restore_mvcc_rows;
            let mut batches = if options.restore_mvcc_rows {
                read_manifested_batches(
                    root,
                    options.tiering_policy.as_ref(),
                    recovery.manifest.durable_seq,
                )?
            } else {
                Vec::new()
            };
            for batch in &mut batches {
                batch.rows = open_rows(
                    options.value_crypto.as_ref(),
                    std::mem::take(&mut batch.rows),
                )?;
            }
            replay_records_into(
                &mut batches,
                recovery.wal_records,
                options.value_crypto.as_ref(),
                options.recovery_progress.as_ref(),
            )?;
            let recovered_row_count = batches.iter().map(|batch| batch.rows.len()).sum::<usize>();
            let migrate_derived_content_model =
                !recovery.manifest.uses_persistent_search_content_model();
            let active_panel_version =
                read_manifest_panel_version(root, &recovery.manifest.panel_ref)?;
            tracing::info!(
                code = "CALYX_ASTER_RECOVERY_DONE",
                vault_dir = %root.display(),
                batch_count = batches.len(),
                row_count = recovered_row_count,
                last_recovered_seq = recovery.last_recovered_seq,
                wal_replay_floor_seq = recovery.manifest.durable_seq,
                router_latest_readback,
                migrate_derived_content_model,
                panel_content_watermark_count = recovery.manifest.panel_content_seqs.len(),
                active_panel_version,
                torn_tail = ?recovery.torn_tail,
                "completed Calyx Aster recovery planning"
            );
            return Ok(RecoveredBatches {
                batches,
                last_recovered_seq: recovery.last_recovered_seq,
                wal_replay_floor_seq: recovery.manifest.durable_seq,
                derived_content_floor_seq: if migrate_derived_content_model {
                    0
                } else {
                    recovery.manifest.effective_derived_content_seq()
                },
                panel_content_floor_seqs: if migrate_derived_content_model {
                    BTreeMap::new()
                } else {
                    recovery.manifest.panel_content_seqs
                },
                active_panel_version,
                migrate_derived_content_model,
                torn_tail: recovery.torn_tail,
                temporal_policy: recovery.manifest.temporal_policy,
                dedup_policy: recovery.manifest.dedup_policy,
                retention_horizon: recovery.manifest.retention_horizon,
                router_latest_readback,
                wal_tail_stream_floor: None,
            });
        }

        let replay = replay_dir(root.join("wal"))?;
        let last_recovered_seq = replay.records.last().map_or(0, |record| record.seq);
        let mut batches = Vec::new();
        replay_records_into(
            &mut batches,
            replay.records,
            options.value_crypto.as_ref(),
            options.recovery_progress.as_ref(),
        )?;
        let recovered_row_count = batches
            .iter()
            .map(|batch: &RecoveredBatch| batch.rows.len())
            .sum::<usize>();
        tracing::info!(
            code = "CALYX_ASTER_RECOVERY_DONE",
            vault_dir = %root.display(),
            batch_count = batches.len(),
            row_count = recovered_row_count,
            last_recovered_seq,
            wal_replay_floor_seq = 0_u64,
            router_latest_readback = false,
            migrate_derived_content_model = false,
            torn_tail = ?replay.torn_tail,
            "completed Calyx Aster WAL-only recovery planning"
        );
        Ok(RecoveredBatches {
            batches,
            last_recovered_seq,
            wal_replay_floor_seq: 0,
            derived_content_floor_seq: 0,
            panel_content_floor_seqs: BTreeMap::new(),
            active_panel_version: options.panel.as_ref().map(|panel| panel.version),
            migrate_derived_content_model: false,
            torn_tail: replay.torn_tail,
            temporal_policy: options.temporal_policy,
            dedup_policy: options.dedup_policy.clone(),
            retention_horizon: options.retention_horizon.clone(),
            router_latest_readback: false,
            wal_tail_stream_floor: None,
        })
    }

    pub(super) fn append_batch(&self, rows: &[WriteRow]) -> Result<u64> {
        let sealed_rows = seal_rows(self.value_crypto.as_ref(), rows)?;
        let payload = encode_write_batch(&sealed_rows)?;
        let ack = self.batcher.submit(payload)?;
        Ok(ack.seq)
    }

    pub(super) fn ensure_disk_write_allowed(&self, counters: &ResourceCounters) -> Result<()> {
        let Some(guard) = &self.disk_pressure_guard else {
            return Ok(());
        };
        match guard.check() {
            Ok(_) => Ok(()),
            Err(error) if error.code == "CALYX_DISK_PRESSURE" => {
                counters.record_disk_pressure();
                guard.request_spill();
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn durable_tip_seq(&self) -> Result<u64> {
        self.batcher.tip_seq()
    }

    fn advance_checkpointed_derived_content(&self, seq: u64, rows: &[WriteRow]) {
        if rows
            .iter()
            .any(|row| row.cf.feeds_persistent_search_index())
        {
            self.checkpointed_derived_content_seq
                .fetch_max(seq, Ordering::AcqRel);
        }
    }

    /// Watermark value a manifest written at `durable_seq` may vouch for.
    pub(super) fn derived_content_seq_for_manifest(&self, durable_seq: u64) -> u64 {
        self.checkpointed_derived_content_seq
            .load(Ordering::Acquire)
            .min(durable_seq)
    }

    /// Adopts a foreign writer's checkpointed watermark (picked up from the
    /// on-disk manifest under the commit lock) so later manifest writes from
    /// this handle vouch for content it did not checkpoint itself (#1100).
    pub(in crate::vault) fn advance_derived_content_watermark_to_at_least(&self, seq: u64) {
        self.checkpointed_derived_content_seq
            .fetch_max(seq, Ordering::AcqRel);
    }

    /// Adopts the live MVCC panel map before manifest publication. Entrywise
    /// max keeps the map monotone across local and foreign writers; the writer
    /// clamps each entry to the manifest durable floor.
    pub(in crate::vault) fn advance_panel_content_watermarks_to_at_least(
        &self,
        floors: &BTreeMap<u32, u64>,
    ) -> Result<()> {
        let mut watermarks = self
            .checkpointed_panel_content_seqs
            .lock()
            .map_err(|_| CalyxError::disk_pressure("panel-content manifest lock poisoned"))?;
        for (panel_version, seq) in floors {
            let watermark = watermarks.entry(*panel_version).or_default();
            *watermark = (*watermark).max(*seq);
        }
        Ok(())
    }

    pub(super) fn panel_content_seqs_for_manifest(
        &self,
        durable_seq: u64,
    ) -> Result<BTreeMap<u32, u64>> {
        let watermarks = self
            .checkpointed_panel_content_seqs
            .lock()
            .map_err(|_| CalyxError::disk_pressure("panel-content manifest lock poisoned"))?;
        Ok(watermarks
            .iter()
            .filter_map(|(panel_version, seq)| {
                let clamped = (*seq).min(durable_seq);
                (clamped != 0).then_some((*panel_version, clamped))
            })
            .collect())
    }

    pub(super) fn sync_wal(&self) -> Result<()> {
        self.batcher.flush_sync()
    }

    pub(super) fn flush(&self) -> Result<()> {
        self.sync_wal()?;
        self.flush_pending_checkpoints()
    }

    pub(super) fn recycle_durable_wal_segments(
        &self,
        max_segments: usize,
        fsync_budget: usize,
    ) -> Result<WalRecycleReport> {
        let newest_durable_seq = self.manifest_durable_seq()?;
        self.batcher
            .recycle_durable_segments(newest_durable_seq, max_segments, fsync_budget)
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    pub(super) fn value_crypto_enabled(&self) -> bool {
        self.value_crypto.is_some()
    }

    pub(super) fn recurrence_lock_path(&self) -> PathBuf {
        self.root.join("locks").join("recurrence.write.lock")
    }

    pub(super) fn commit_lock_path(&self) -> PathBuf {
        self.root.join("locks").join("durable.commit.lock")
    }

    pub(super) fn checkpoint_lock_path(&self) -> PathBuf {
        self.root.join("locks").join("durable.checkpoint.lock")
    }

    pub(super) fn native_compaction_lock_path(&self) -> PathBuf {
        self.root.join("locks").join("native.compaction.lock")
    }

    pub(super) fn recover_current_batches(&self) -> Result<RecoveredBatches> {
        let options = VaultOptions {
            tiering_policy: self.tiering_policy.clone(),
            ledger_checkpoint: self.ledger_checkpoint.clone(),
            temporal_policy: self.temporal_policy,
            dedup_policy: self.dedup_policy.clone(),
            retention_horizon: self.retention_horizon(),
            panel: self.panel.clone(),
            disk_pressure_guard: self.disk_pressure_guard.clone(),
            value_crypto: self.value_crypto.clone(),
            restore_mvcc_rows: true,
            ..VaultOptions::default()
        };
        Self::recover_batches(&self.root, &options)
    }

    pub(super) fn ledger_checkpoint(&self) -> Option<CheckpointConfig> {
        self.ledger_checkpoint.clone()
    }

    pub(super) fn tiering_policy(&self) -> Option<&TieringPolicy> {
        self.tiering_policy.as_ref()
    }

    pub(super) fn compaction_output_path(&self, cf: ColumnFamily, seq: u64) -> PathBuf {
        self.cf_dir(cf).join(format!("compacted-{seq:020}.sst"))
    }

    fn cf_dir(&self, cf: ColumnFamily) -> PathBuf {
        self.tiering_policy.as_ref().map_or_else(
            || self.root.join("cf").join(cf.name()),
            |policy| policy.place_current_cf(cf).absolute_dir(),
        )
    }
}

const RECOVERY_PROGRESS_STRIDE_BYTES: u64 = 4 * 1024 * 1024;

fn replay_records_into(
    batches: &mut Vec<RecoveredBatch>,
    records: Vec<ReplayRecord>,
    value_crypto: Option<&SharedVaultContext>,
    progress: Option<&RecoveryProgressHook>,
) -> Result<()> {
    let total_bytes = records.iter().fold(0_u64, |total, record| {
        total.saturating_add(u64::try_from(record.payload.len()).unwrap_or(u64::MAX))
    });
    if let Some(hook) = progress {
        hook.report(0, total_bytes);
    }
    let mut replayed_bytes = 0_u64;
    let mut last_reported = 0_u64;
    for record in records {
        let payload_bytes = u64::try_from(record.payload.len()).unwrap_or(u64::MAX);
        batches.push(RecoveredBatch {
            seq: record.seq,
            rows: open_rows(value_crypto, decode_write_batch(&record.payload)?)?,
        });
        replayed_bytes = replayed_bytes.saturating_add(payload_bytes);
        if let Some(hook) = progress
            && replayed_bytes.saturating_sub(last_reported) >= RECOVERY_PROGRESS_STRIDE_BYTES
        {
            hook.report(replayed_bytes, total_bytes);
            last_reported = replayed_bytes;
        }
    }
    if let Some(hook) = progress
        && replayed_bytes != last_reported
    {
        hook.report(replayed_bytes, total_bytes);
    }
    Ok(())
}

fn validate_dedup_policy(policy: &DedupPolicy, panel: Option<&Panel>) -> Result<()> {
    if let Some(panel) = panel {
        policy.validate(panel)
    } else {
        policy.validate_manifest()
    }
}

fn read_manifest_panel_version(
    root: &Path,
    panel_ref: &crate::manifest::ImmutableRef,
) -> Result<Option<u32>> {
    let path = root.join(&panel_ref.logical_path);
    let bytes = fs::read(&path).map_err(|error| {
        storage_error(
            &format!("read hash-verified manifest panel asset {}", path.display()),
            error,
        )
    })?;
    match serde_json::from_slice::<Panel>(&bytes) {
        Ok(panel) if panel.version != 0 => Ok(Some(panel.version)),
        Ok(_) => Err(CalyxError::aster_corrupt_shard(format!(
            "manifest panel asset {} has version zero",
            path.display()
        ))),
        Err(panel_error) => {
            let generated = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .is_some_and(|value| {
                    value.get("kind").and_then(serde_json::Value::as_str)
                        == Some("calyx_manifest_generated_asset_v1")
                        && value.get("asset_kind").and_then(serde_json::Value::as_str)
                            == Some("panel")
                        && value.get("status").and_then(serde_json::Value::as_str)
                            == Some("no-active-panel")
                });
            if generated {
                Ok(None)
            } else {
                Err(CalyxError::aster_corrupt_shard(format!(
                    "manifest panel asset {} is neither a valid Panel nor the canonical no-active-panel marker: {panel_error}",
                    path.display()
                )))
            }
        }
    }
}

fn storage_error(context: &str, error: io::Error) -> CalyxError {
    CalyxError::disk_pressure(format!("{context}: {error}"))
}

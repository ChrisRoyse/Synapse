use super::ColumnFamily;
use crate::compaction::TieringPolicy;
use crate::memtable::{FrozenMemtable, Memtable, MemtableUsage};
use crate::resource::ResourceCounters;
use crate::security::value_crypto::{
    SharedVaultContext, open_value as open_encrypted_value, seal_value,
};
use crate::sst::level::SstLevel;
use crate::sst::{SstEntry, SstSummary};
use crate::storage_names::flush_sst_file_name;
use calyx_core::{CalyxError, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod queries;

const DEFAULT_MEMTABLE_BYTES: usize = 8 * 1024 * 1024;

/// Commit watermark for router writes that have no commit domain (raw
/// `CfRouter` users such as drills and standalone stores). Flushes at this
/// watermark sort at the very start of the commit domain, which is exact for
/// standalone directories (there are no commit-domain files) and never
/// shadows commit-domain data elsewhere.
pub const NO_COMMIT_DOMAIN: u64 = 0;

#[derive(Debug)]
pub struct CfRouter {
    vault_dir: PathBuf,
    tiering_policy: Option<TieringPolicy>,
    pub(super) memtables: HashMap<ColumnFamily, Memtable>,
    pub(super) levels: HashMap<ColumnFamily, SstLevel>,
    pub(super) next_file: HashMap<ColumnFamily, u64>,
    memtable_byte_cap: usize,
    resource_counters: Arc<ResourceCounters>,
    value_crypto: Option<SharedVaultContext>,
}

impl CfRouter {
    pub(super) fn vault_dir(&self) -> &Path {
        &self.vault_dir
    }

    pub(crate) fn prove_persistent_search_content_watermark(
        &self,
        durable_seq: u64,
    ) -> Result<u64> {
        let mut watermark = 0_u64;
        for (cf, level) in &self.levels {
            if cf.feeds_persistent_search_index() {
                watermark = watermark.max(level.prove_commit_domain_watermark(durable_seq)?);
            }
        }
        Ok(watermark)
    }

    pub fn open(vault_dir: impl AsRef<Path>, memtable_byte_cap: usize) -> Result<Self> {
        Self::open_with_tiering(vault_dir, memtable_byte_cap, None)
    }

    pub(crate) fn open_selected_cfs(
        vault_dir: impl AsRef<Path>,
        memtable_byte_cap: usize,
        cfs: impl IntoIterator<Item = ColumnFamily>,
    ) -> Result<Self> {
        Self::open_selected_cfs_with_tiering(vault_dir, memtable_byte_cap, cfs, None)
    }

    pub(crate) fn open_selected_cfs_with_tiering(
        vault_dir: impl AsRef<Path>,
        memtable_byte_cap: usize,
        cfs: impl IntoIterator<Item = ColumnFamily>,
        tiering_policy: Option<TieringPolicy>,
    ) -> Result<Self> {
        Self::open_selected_cfs_with_tiering_and_crypto(
            vault_dir,
            memtable_byte_cap,
            cfs,
            tiering_policy,
            None,
        )
    }

    pub(crate) fn open_selected_cfs_with_tiering_and_crypto(
        vault_dir: impl AsRef<Path>,
        memtable_byte_cap: usize,
        cfs: impl IntoIterator<Item = ColumnFamily>,
        tiering_policy: Option<TieringPolicy>,
        value_crypto: Option<SharedVaultContext>,
    ) -> Result<Self> {
        Self::open_selected_cfs_with_tiering_crypto_and_lookup_policy(
            vault_dir,
            memtable_byte_cap,
            cfs,
            tiering_policy,
            value_crypto,
            true,
        )
    }

    pub(crate) fn open_selected_cfs_with_tiering_crypto_and_lookup_policy(
        vault_dir: impl AsRef<Path>,
        memtable_byte_cap: usize,
        cfs: impl IntoIterator<Item = ColumnFamily>,
        tiering_policy: Option<TieringPolicy>,
        value_crypto: Option<SharedVaultContext>,
        eager_lookup_on_open: bool,
    ) -> Result<Self> {
        let selected = cfs.into_iter().collect::<BTreeSet<_>>();
        if selected.is_empty() {
            return Err(CalyxError::aster_corrupt_shard(
                "selected CF router open requires at least one column family",
            ));
        }
        let mut router =
            Self::new_empty(vault_dir, memtable_byte_cap, tiering_policy, value_crypto)?;
        for cf in &selected {
            router.ensure_cf(*cf)?;
        }
        router.load_existing_cfs_with_lookup_policy(
            &selected.into_iter().collect::<Vec<_>>(),
            eager_lookup_on_open,
        )?;
        Ok(router)
    }

    pub fn open_with_tiering(
        vault_dir: impl AsRef<Path>,
        memtable_byte_cap: usize,
        tiering_policy: Option<TieringPolicy>,
    ) -> Result<Self> {
        Self::open_with_tiering_and_crypto(vault_dir, memtable_byte_cap, tiering_policy, None)
    }

    pub(crate) fn open_with_tiering_and_crypto(
        vault_dir: impl AsRef<Path>,
        memtable_byte_cap: usize,
        tiering_policy: Option<TieringPolicy>,
        value_crypto: Option<SharedVaultContext>,
    ) -> Result<Self> {
        Self::open_with_tiering_crypto_and_lookup_policy(
            vault_dir,
            memtable_byte_cap,
            tiering_policy,
            value_crypto,
            true,
        )
    }

    pub(crate) fn open_with_tiering_crypto_and_lookup_policy(
        vault_dir: impl AsRef<Path>,
        memtable_byte_cap: usize,
        tiering_policy: Option<TieringPolicy>,
        value_crypto: Option<SharedVaultContext>,
        eager_lookup_on_open: bool,
    ) -> Result<Self> {
        let mut router =
            Self::new_empty(vault_dir, memtable_byte_cap, tiering_policy, value_crypto)?;
        for cf in ColumnFamily::STATIC {
            router.ensure_cf(cf)?;
        }
        router.load_existing_with_lookup_policy(eager_lookup_on_open)?;
        Ok(router)
    }

    fn new_empty(
        vault_dir: impl AsRef<Path>,
        memtable_byte_cap: usize,
        tiering_policy: Option<TieringPolicy>,
        value_crypto: Option<SharedVaultContext>,
    ) -> Result<Self> {
        let vault_dir = vault_dir.as_ref().to_path_buf();
        let memtable_byte_cap = if memtable_byte_cap == 0 {
            DEFAULT_MEMTABLE_BYTES
        } else {
            memtable_byte_cap
        };
        fs::create_dir_all(vault_dir.join("cf"))
            .map_err(|error| CalyxError::disk_pressure(format!("create CF root: {error}")))?;
        if let Some(policy) = &tiering_policy {
            for tier_root in policy.tier_roots() {
                fs::create_dir_all(tier_root.join("cf")).map_err(|error| {
                    CalyxError::disk_pressure(format!("create tiered CF root: {error}"))
                })?;
            }
        }
        Ok(Self {
            vault_dir,
            tiering_policy,
            memtables: HashMap::new(),
            levels: HashMap::new(),
            next_file: HashMap::new(),
            memtable_byte_cap,
            resource_counters: Arc::new(ResourceCounters::default()),
            value_crypto,
        })
    }

    /// Raw write with no commit domain; see [`Self::put_at`].
    pub fn put(&mut self, cf: ColumnFamily, key: &[u8], value: &[u8]) -> Result<()> {
        self.put_at(cf, key, value, NO_COMMIT_DOMAIN)
    }

    /// Writes one row; any memtable flush this write triggers is stamped with
    /// `commit_watermark` (the highest commit seq whose rows can be in the
    /// flushed memtable), so the flush SST orders exactly against durable
    /// batches in the commit domain (issue #1138).
    pub fn put_at(
        &mut self,
        cf: ColumnFamily,
        key: &[u8],
        value: &[u8],
        commit_watermark: u64,
    ) -> Result<()> {
        self.ensure_cf(cf)?;
        let mut counted_backpressure = false;
        let ack = match self.memtable_mut(cf).write(key, value, 0) {
            Ok(ack) => ack,
            Err(error) => {
                if error.code != "CALYX_BACKPRESSURE" {
                    return Err(error);
                }
                self.flush_cf_at(cf, commit_watermark)?;
                match self.memtable_mut(cf).write(key, value, 0) {
                    Ok(ack) => {
                        self.resource_counters.record_memtable_absorbed();
                        counted_backpressure = true;
                        ack
                    }
                    Err(retry_error) => {
                        if retry_error.code == "CALYX_BACKPRESSURE" {
                            self.resource_counters.record_memtable_rejected();
                        }
                        return Err(retry_error);
                    }
                }
            }
        };
        if ack.flush_triggered {
            if !counted_backpressure {
                self.resource_counters.record_memtable_absorbed();
            }
            self.flush_cf_at(cf, commit_watermark)?;
        }
        Ok(())
    }

    /// Fails closed before WAL append when a row can never fit in one memtable.
    pub fn ensure_batch_admitted<I, K, V>(&self, rows: I) -> Result<()>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        for (cf, key, value) in rows {
            let row_bytes = Memtable::entry_size(key.as_ref(), value.as_ref());
            if row_bytes > self.memtable_byte_cap {
                self.resource_counters.record_memtable_rejected();
                return Err(CalyxError::backpressure(format!(
                    "memtable byte cap {} cannot fit {} row of {} bytes",
                    self.memtable_byte_cap,
                    cf.name(),
                    row_bytes
                )));
            }
        }
        Ok(())
    }

    /// Shares the backpressure counters this router increments.
    pub fn resource_counters(&self) -> Arc<ResourceCounters> {
        Arc::clone(&self.resource_counters)
    }

    pub fn memtable_usage_by_cf(&self) -> Vec<(ColumnFamily, MemtableUsage)> {
        let mut usage = self
            .memtables
            .iter()
            .map(|(cf, table)| (*cf, table.usage()))
            .collect::<Vec<_>>();
        usage.sort_by_key(|left| left.0.name());
        usage
    }

    /// Raw flush with no commit domain; see [`Self::flush_cf_at`].
    pub fn flush_cf(&mut self, cf: ColumnFamily) -> Result<SstSummary> {
        self.flush_cf_at(cf, NO_COMMIT_DOMAIN)
    }

    /// Flushes one CF's memtable to a commit-anchored flush SST
    /// (`flush-{watermark:020}-{ordinal:04}.sst`). `commit_watermark` must be
    /// the highest commit seq whose rows can be in the memtable; understating
    /// it is safe (the file sorts earlier and committed rows keep their
    /// durable-batch home), overstating it can shadow newer durable batches.
    pub fn flush_cf_at(&mut self, cf: ColumnFamily, commit_watermark: u64) -> Result<SstSummary> {
        self.ensure_cf(cf)?;
        let fresh = Memtable::new(self.memtable_byte_cap);
        let frozen = std::mem::replace(self.memtable_mut(cf), fresh).freeze();
        let ordinal = self.next_sequence(cf);
        let ordinal = usize::try_from(ordinal).map_err(|_| {
            CalyxError::aster_corrupt_shard(format!(
                "flush ordinal {ordinal} for {} exceeds the platform's usize range",
                cf.name()
            ))
        })?;
        let path = self
            .cf_dir(cf)
            .join(flush_sst_file_name(commit_watermark, ordinal));
        let publish = (|| -> Result<SstSummary> {
            let summary = match &self.value_crypto {
                Some(context) => {
                    let entries = frozen
                        .iter()
                        .map(|(key, value)| {
                            Ok((key.to_vec(), seal_value(context, cf, key, value)?))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    crate::sst::write_sst(
                        &path,
                        entries
                            .iter()
                            .map(|(key, value)| (key.as_slice(), value.as_slice())),
                    )?
                }
                None => frozen.flush_to_sst(&path)?,
            };
            self.levels
                .entry(cf)
                .or_default()
                .push_with_lookup(summary.path.clone())?;
            Ok(summary)
        })();

        match publish {
            Ok(summary) => Ok(summary),
            Err(publish_error) => {
                // Rotation happens before encryption and durable SST
                // publication. Restore the exact frozen rows into the fresh
                // memtable before returning any failure; otherwise an error
                // would silently remove previously accepted rows from the
                // router's serving view. A target SST that did reach disk is
                // intentionally retained as an idempotent physical projection.
                if let Err(restore_error) = self.restore_frozen_memtable(cf, &frozen) {
                    tracing::error!(
                        code = "CALYX_ASTER_ROUTER_FLUSH_RESTORE_FAILED",
                        cf = cf.name(),
                        commit_watermark,
                        path = %path.display(),
                        publish_error_code = publish_error.code,
                        publish_error = %publish_error,
                        restore_error_code = restore_error.code,
                        restore_error = %restore_error,
                        "router flush failed and the rotated memtable could not be restored"
                    );
                    return Err(CalyxError::aster_corrupt_shard(format!(
                        "router flush for {} at watermark {commit_watermark} failed and the rotated memtable could not be restored: publish=error[{}]: {}; restore=error[{}]: {}",
                        cf.name(),
                        publish_error.code,
                        publish_error.message,
                        restore_error.code,
                        restore_error.message
                    )));
                }
                tracing::error!(
                    code = publish_error.code,
                    cf = cf.name(),
                    commit_watermark,
                    path = %path.display(),
                    error = %publish_error,
                    "router flush failed; restored the complete rotated memtable before returning the error"
                );
                Err(publish_error)
            }
        }
    }

    fn restore_frozen_memtable(&mut self, cf: ColumnFamily, frozen: &FrozenMemtable) -> Result<()> {
        let memtable = self.memtable_mut(cf);
        for (key, value) in frozen.iter() {
            memtable.write(key, value, 0)?;
        }
        Ok(())
    }

    pub(super) fn ensure_cf(&mut self, cf: ColumnFamily) -> Result<()> {
        fs::create_dir_all(self.cf_dir(cf))
            .map_err(|error| CalyxError::disk_pressure(format!("create CF dir: {error}")))?;
        self.memtables
            .entry(cf)
            .or_insert_with(|| Memtable::new(self.memtable_byte_cap));
        self.levels.entry(cf).or_default();
        self.next_file.entry(cf).or_insert(1);
        Ok(())
    }

    fn memtable_mut(&mut self, cf: ColumnFamily) -> &mut Memtable {
        self.memtables
            .entry(cf)
            .or_insert_with(|| Memtable::new(self.memtable_byte_cap))
    }

    pub(super) fn next_sequence(&mut self, cf: ColumnFamily) -> u64 {
        let next = self.next_file.entry(cf).or_insert(1);
        let seq = *next;
        *next += 1;
        seq
    }

    pub(super) fn cf_dir(&self, cf: ColumnFamily) -> PathBuf {
        self.tiering_policy.as_ref().map_or_else(
            || self.vault_dir.join("cf").join(cf.name()),
            |policy| policy.place_current_cf(cf).absolute_dir(),
        )
    }

    fn open_value(&self, cf: ColumnFamily, key: &[u8], value: Vec<u8>) -> Result<Vec<u8>> {
        match &self.value_crypto {
            Some(context) => open_encrypted_value(context, cf, key, &value),
            None => Ok(value),
        }
    }

    pub(super) fn open_entries<I>(&self, cf: ColumnFamily, entries: I) -> Result<Vec<SstEntry>>
    where
        I: IntoIterator<Item = SstEntry>,
    {
        entries
            .into_iter()
            .map(|entry| {
                Ok(SstEntry {
                    value: self.open_value(cf, &entry.key, entry.value)?,
                    key: entry.key,
                })
            })
            .collect()
    }

    pub(super) fn cf_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.vault_dir.join("cf")];
        if let Some(policy) = &self.tiering_policy {
            for tier_root in policy.tier_roots() {
                let cf_root = tier_root.join("cf");
                if !roots.contains(&cf_root) {
                    roots.push(cf_root);
                }
            }
        }
        roots
    }
}

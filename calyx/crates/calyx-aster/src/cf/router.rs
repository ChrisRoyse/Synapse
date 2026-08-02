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
use calyx_core::{CalyxError, Result, SlotId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

mod queries;

const DEFAULT_MEMTABLE_BYTES: usize = 8 * 1024 * 1024;

/// Whole microseconds since `started`, saturating rather than wrapping.
fn elapsed_us(started: &Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// What one [`CfRouter::put_at`] did besides insert into the memtable.
///
/// A router write looks like an in-memory operation and is charged to the
/// caller as one, but it can synchronously seal, encrypt and write an entire
/// SST, and it runs `create_dir_all` on every call. Both costs are paid under
/// the vault's global write locks, so they are reported per commit rather than
/// left inside an opaque total (#1948).
#[derive(Debug, Default, Clone, Copy)]
pub struct RouterPutCost {
    /// Time in the `ensure_cf` / write-fanout admission prologue, which
    /// includes a `create_dir_all` syscall on every single write.
    pub ensure_cf_us: u64,
    /// Time in synchronous memtable-to-SST flushes triggered by this write.
    pub flush_us: u64,
    /// How many flushes this one write triggered.
    pub flushes: u32,
}

impl RouterPutCost {
    fn record_flush(&mut self, flush_us: u64) {
        self.flush_us = self.flush_us.saturating_add(flush_us);
        self.flushes = self.flushes.saturating_add(1);
    }

    /// Folds another write's outcome into this one, for per-batch totals.
    pub fn absorb(&mut self, other: Self) {
        self.ensure_cf_us = self.ensure_cf_us.saturating_add(other.ensure_cf_us);
        self.flush_us = self.flush_us.saturating_add(other.flush_us);
        self.flushes = self.flushes.saturating_add(other.flushes);
    }
}

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
    /// CFs whose directory this router has already created, so the per-write
    /// `ensure_cf` repeats neither the syscall nor the path construction.
    ///
    /// Keyed by CF rather than by path because `cf_dir` is a pure function of
    /// `vault_dir`, `tiering_policy` and the CF, all of which are fixed when
    /// the router is constructed — so a hit here proves the same directory was
    /// created, and the hot path needs no `PathBuf`. See [`Self::ensure_cf`].
    ensured_cfs: HashSet<ColumnFamily>,
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
            ensured_cfs: HashSet::new(),
        })
    }

    /// Raw write with no commit domain; see [`Self::put_at`].
    pub fn put(&mut self, cf: ColumnFamily, key: &[u8], value: &[u8]) -> Result<()> {
        self.put_at(cf, key, value, NO_COMMIT_DOMAIN).map(|_| ())
    }

    /// Writes one row; any memtable flush this write triggers is stamped with
    /// `commit_watermark` (the highest commit seq whose rows can be in the
    /// flushed memtable), so the flush SST orders exactly against durable
    /// batches in the commit domain (issue #1138).
    ///
    /// Returns what the write did *beyond* landing bytes in the memtable. A
    /// `put_at` can synchronously seal and write an entire SST, and its caller
    /// holds the vault's row-table and router write locks for the whole of it,
    /// so a commit that reported one opaque "router apply" number could not say
    /// whether it had spent its time on a memtable insert or on a multi-megabyte
    /// flush (#1948).
    pub fn put_at(
        &mut self,
        cf: ColumnFamily,
        key: &[u8],
        value: &[u8],
        commit_watermark: u64,
    ) -> Result<RouterPutCost> {
        let mut outcome = RouterPutCost::default();
        let ensure_started = Instant::now();
        self.ensure_cf(cf)?;
        self.ensure_cf_write_fanout_admitted(cf)?;
        outcome.ensure_cf_us = elapsed_us(&ensure_started);
        let mut counted_backpressure = false;
        let ack = match self.memtable_mut(cf).write(key, value, 0) {
            Ok(ack) => ack,
            Err(error) => {
                if error.code != "CALYX_BACKPRESSURE" {
                    return Err(error);
                }
                outcome.record_flush(self.timed_flush_cf_at(cf, commit_watermark)?);
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
            outcome.record_flush(self.timed_flush_cf_at(cf, commit_watermark)?);
        }
        Ok(outcome)
    }

    /// [`Self::flush_cf_at`] with the wall-clock cost of the flush attached.
    fn timed_flush_cf_at(&mut self, cf: ColumnFamily, commit_watermark: u64) -> Result<u64> {
        let started = Instant::now();
        self.flush_cf_at(cf, commit_watermark)?;
        Ok(elapsed_us(&started))
    }

    /// Fails closed before WAL append when a row can never fit in one memtable.
    pub fn ensure_batch_admitted<I, K, V>(&self, rows: I) -> Result<()>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        for (cf, key, value) in rows {
            self.ensure_cf_write_fanout_admitted(cf)?;
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

    fn ensure_cf_write_fanout_admitted(&self, cf: ColumnFamily) -> Result<()> {
        let current_files = self.level_file_count(cf);
        if current_files < crate::vault::LIVE_COMPACTION_TRIGGER_FILES {
            return Ok(());
        }
        self.resource_counters.record_memtable_rejected();
        Err(CalyxError {
            code: "CALYX_ASTER_SST_FANOUT_WRITE_STALL",
            message: format!(
                "stopped {} writes because its live router has {} immutable SST sources; proactive_compaction_trigger={} hard_page_source_limit={}",
                cf.name(),
                current_files,
                crate::vault::LIVE_COMPACTION_TRIGGER_FILES,
                crate::sst::MAX_INTERSECTING_SST_PAGE_SOURCES,
            ),
            remediation: "allow the owned native fan-out maintenance pass to compact this CF and inspect CALYX_ASTER_NATIVE_CF_COMPACTION_* telemetry before retrying",
        })
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

    /// Ensures the CF has a directory on disk and an entry in every in-memory
    /// map, creating the directory only the first time this router sees it.
    ///
    /// This runs on **every** `put_at`, so the `create_dir_all` it used to make
    /// unconditionally was one syscall per row written. Measured on a 27-row
    /// commit on the deployment host that was 3.0-4.3 ms, 92% of all non-flush
    /// router apply time and paid on every commit rather than only on the rare
    /// ones that flush (#1948). A directory this process already created does
    /// not need re-creating; the path is keyed rather than the CF so that a
    /// tiering policy moving a CF to another tier still creates the new
    /// directory.
    ///
    /// Deliberately not a fallback: if the directory is removed underneath a
    /// running vault, the next SST write fails loudly with the real filesystem
    /// error instead of being silently papered over by a re-create.
    pub(super) fn ensure_cf(&mut self, cf: ColumnFamily) -> Result<()> {
        // Recorded only after the directory exists: a failed create must leave
        // the CF unensured so the next write retries it rather than assuming a
        // directory that was never made.
        if !self.ensured_cfs.contains(&cf) {
            fs::create_dir_all(self.cf_dir(cf))
                .map_err(|error| CalyxError::disk_pressure(format!("create CF dir: {error}")))?;
            self.ensured_cfs.insert(cf);
        }
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

    /// Lists the distinct quantized-slot `SlotId`s that physically exist as
    /// `cf/slot_*` directories under any CF root. Raw sidecars (`slot_*.raw`)
    /// share the slot's identity and are folded into the same id.
    pub(crate) fn present_slot_cf_ids(&self) -> Result<BTreeSet<SlotId>> {
        let mut ids = BTreeSet::new();
        for root in self.cf_roots() {
            if !root.exists() {
                continue;
            }
            let entries = fs::read_dir(&root).map_err(|error| {
                CalyxError::disk_pressure(format!("read CF root for slot enumeration: {error}"))
            })?;
            for entry in entries {
                let path = entry
                    .map_err(|error| CalyxError::disk_pressure(format!("read CF entry: {error}")))?
                    .path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                    continue;
                };
                if let Some(ColumnFamily::Slot { slot, .. }) = ColumnFamily::from_name(name) {
                    ids.insert(slot);
                }
            }
        }
        Ok(ids)
    }

    /// Retires one column family: drops its in-memory serving state (releasing
    /// SST file handles before any unlink so the physical removal succeeds on
    /// Windows) and removes its directory under every CF root. Fail-closed
    /// readback proves no root still contains the directory. Idempotent: an
    /// already-absent CF returns `was_present == false` with zero removals.
    pub(crate) fn retire_cf(&mut self, cf: ColumnFamily) -> Result<RetiredCfPhysical> {
        let mut physical = RetiredCfPhysical::default();
        for root in self.cf_roots() {
            let dir = root.join(cf.name());
            match fs::read_dir(&dir) {
                Ok(entries) => {
                    physical.was_present = true;
                    for entry in entries {
                        let path = entry
                            .map_err(|error| {
                                CalyxError::disk_pressure(format!(
                                    "read CF dir for retire: {error}"
                                ))
                            })?
                            .path();
                        if path.extension().and_then(|value| value.to_str()) == Some("sst") {
                            physical.removed_sst_files =
                                physical.removed_sst_files.saturating_add(1);
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(CalyxError::disk_pressure(format!(
                        "stat CF dir {} for retire: {error}",
                        dir.display()
                    )));
                }
            }
        }
        // Drop in-memory serving state first so no SST handle keeps the files
        // mapped when the directory is unlinked.
        self.memtables.remove(&cf);
        self.levels.remove(&cf);
        self.next_file.remove(&cf);
        for root in self.cf_roots() {
            let dir = root.join(cf.name());
            match fs::remove_dir_all(&dir) {
                Ok(()) => physical.removed_dirs.push(dir),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CalyxError::disk_pressure(format!(
                        "remove CF dir {} during retire: {error}",
                        dir.display()
                    )));
                }
            }
        }
        for root in self.cf_roots() {
            if root.join(cf.name()).exists() {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "CF {} directory still present under {} after retire",
                    cf.name(),
                    root.display()
                )));
            }
        }
        Ok(physical)
    }
}

/// Physical outcome of retiring one column family directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetiredCfPhysical {
    /// True when at least one CF root physically held the directory.
    pub was_present: bool,
    /// Count of `*.sst` files removed with the directory.
    pub removed_sst_files: usize,
    /// Directories that were physically removed.
    pub removed_dirs: Vec<PathBuf>,
}

use super::ColumnFamily;
use crate::compaction::TieringPolicy;
use crate::memtable::{Memtable, MemtableUsage};
use crate::resource::ResourceCounters;
use crate::security::value_crypto::{
    SharedVaultContext, open_value_owned as open_encrypted_value_owned, seal_value,
};
use crate::sst::level::{PreparedLevelFile, SstLevel};
use crate::sst::{SstEntry, SstSummary};
use crate::storage_names::flush_sst_file_name;
use calyx_core::{CalyxError, Result, SlotId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

mod queries;

const DEFAULT_MEMTABLE_BYTES: usize = 8 * 1024 * 1024;

/// A poisoned shard names the column family that reached it, because "the
/// router lock is poisoned" used to be true of one lock and is now true of one
/// of [`ColumnFamily::SHARDS`].
fn poisoned_shard(cf: ColumnFamily) -> CalyxError {
    CalyxError::aster_corrupt_shard(format!(
        "CF router shard {} (column family {}) is poisoned; a thread panicked while holding it",
        cf.shard_index(),
        cf.name()
    ))
}

fn poisoned_all_shards() -> CalyxError {
    CalyxError::aster_corrupt_shard(
        "a CF router shard is poisoned; a thread panicked while holding it".to_owned(),
    )
}

/// Whole microseconds since `started`, saturating rather than wrapping.
fn elapsed_us(started: &Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// What one [`CfRouter::put_at`] did besides insert into the memtable.
///
/// A router write looks like an in-memory operation and is charged to the
/// caller as one, but it can seal a full memtable, and it runs `create_dir_all`
/// on every call. Both costs are paid under the vault's global write locks, so
/// they are reported per commit rather than left inside an opaque total
/// (#1948).
#[derive(Debug, Default, Clone, Copy)]
pub struct RouterPutCost {
    /// Waiting for this column family's router shard write guard.
    ///
    /// Reported because it is the only remaining router contention a commit can
    /// experience, and it must stay visible for the same reason #1948 made the
    /// old vault-wide router lock wait visible: a commit that waits needs to be
    /// distinguishable from a commit that works (#1950).
    pub lock_wait_us: u64,
    /// Time in the `ensure_cf` / write-fanout admission prologue, which
    /// includes a `create_dir_all` syscall on the first write to each CF.
    pub ensure_cf_us: u64,
    /// Time spent sealing full memtables. This is the pointer swap only; the
    /// SST write it defers is timed by the caller, outside the locks (#1949).
    pub seal_us: u64,
    /// How many memtables this one write sealed.
    pub seals: u32,
}

impl RouterPutCost {
    fn record_seal(&mut self, seal_us: u64) {
        self.seal_us = self.seal_us.saturating_add(seal_us);
        self.seals = self.seals.saturating_add(1);
    }

    /// Folds another write's outcome into this one, for per-batch totals.
    pub fn absorb(&mut self, other: Self) {
        self.lock_wait_us = self.lock_wait_us.saturating_add(other.lock_wait_us);
        self.ensure_cf_us = self.ensure_cf_us.saturating_add(other.ensure_cf_us);
        self.seal_us = self.seal_us.saturating_add(other.seal_us);
        self.seals = self.seals.saturating_add(other.seals);
    }
}

/// Upper bound on memtables sealed but not yet installed, per column family.
///
/// Under the commit path a seal is written and installed before the next one
/// can be taken, so this queue is normally 0 or 1 deep. The bound exists so an
/// unnoticed failure to drain becomes a loud, bounded error instead of
/// unbounded memory growth — RocksDB's `max_write_buffer_number` stall serves
/// the same purpose.
const MAX_SEALED_MEMTABLES_PER_CF: usize = 8;

/// How far below the configured cap a column family's own seal point may sit,
/// as a percentage. See [`RouterConfig::cf_memtable_cap`].
const MEMTABLE_CAP_STAGGER_PCT: usize = 25;

/// FNV-1a over the CF name.
///
/// Deliberately not `DefaultHasher`: `RandomState` is seeded per process, so a
/// CF's seal point would move every restart and the stagger would not be
/// reproducible from one run to the next. This is fixed for a given CF name
/// forever.
const fn cf_name_hash(name: &str) -> u64 {
    let bytes = name.as_bytes();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

/// A memtable that has been sealed and is awaiting its SST.
///
/// It stays in the router's read path for its whole life: the rows are still
/// the newest state for their keys until the SST is installed, and dropping
/// them from reads for the duration of the write would be a correctness bug,
/// not an optimisation.
#[derive(Debug)]
pub(super) struct PendingFlush {
    id: u64,
    table: Arc<Memtable>,
    path: PathBuf,
    /// Set once the SST is on disk and its lookup index has been read. Installs
    /// drain in seal order, so a written entry still waits behind an unwritten
    /// older sibling.
    prepared: Option<PreparedLevelFile>,
}

/// A sealed memtable that has reached disk, with its level entry prepared.
#[derive(Debug)]
pub struct WrittenFlush {
    summary: SstSummary,
    prepared: PreparedLevelFile,
}

impl WrittenFlush {
    /// The written SST.
    #[must_use]
    pub const fn summary(&self) -> &SstSummary {
        &self.summary
    }
}

/// A sealed memtable handed back to the caller so the SST can be written with
/// the vault's row-table and router locks released (#1949).
///
/// Carries everything the write needs — rows, target path, and a clone of the
/// value-crypto handle — precisely so that it does **not** borrow the router.
#[derive(Debug, Clone)]
pub struct SealedFlush {
    cf: ColumnFamily,
    id: u64,
    path: PathBuf,
    table: Arc<Memtable>,
    value_crypto: Option<SharedVaultContext>,
    retain_lookup: bool,
}

impl SealedFlush {
    /// The column family whose memtable this is.
    #[must_use]
    pub const fn cf(&self) -> ColumnFamily {
        self.cf
    }

    /// Rows carried by the sealed memtable.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.table.len()
    }

    /// Writes the sealed memtable to its SST **with no router lock held**.
    ///
    /// This is the whole point of the split: sealing is a pointer swap taken
    /// under the vault's global write locks, and this — the AEAD seal of every
    /// value plus the file write, measured at 652 ms for one commit's worth of
    /// memtables (#1948) — runs with those locks released.
    /// Also reads back the new SST's lookup index, because that read is I/O
    /// too and belongs on this side of the lock boundary rather than inside
    /// the install.
    pub fn write_sst(&self) -> Result<WrittenFlush> {
        let summary = match &self.value_crypto {
            Some(context) => {
                let entries = self
                    .table
                    .iter()
                    .map(|(key, value)| {
                        Ok((key.clone(), seal_value(context, self.cf, &key, &value)?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                crate::sst::write_sst(
                    &self.path,
                    entries
                        .iter()
                        .map(|(key, value)| (key.as_slice(), value.as_slice())),
                )?
            }
            None => self.table.flush_to_sst(&self.path)?,
        };
        let prepared = SstLevel::prepare(&summary, self.retain_lookup)?;
        Ok(WrittenFlush { summary, prepared })
    }
}

/// Commit watermark for router writes that have no commit domain (raw
/// `CfRouter` users such as drills and standalone stores). Flushes at this
/// watermark sort at the very start of the commit domain, which is exact for
/// standalone directories (there are no commit-domain files) and never
/// shadows commit-domain data elsewhere.
pub const NO_COMMIT_DOMAIN: u64 = 0;

/// Router-wide state fixed when the router is constructed.
///
/// Held outside every lock on purpose: `cf_dir`, `cf_memtable_cap` and
/// `open_value` are called from inside shard guards and from paths holding no
/// guard at all, and none of them can observe a torn value if there is nothing
/// to tear.
#[derive(Debug)]
pub(super) struct RouterConfig {
    vault_dir: PathBuf,
    tiering_policy: Option<TieringPolicy>,
    memtable_byte_cap: usize,
    value_crypto: Option<SharedVaultContext>,
    eager_lookup_on_open: bool,
    selected_lookup_is_universal: bool,
}

/// The per-column-family serving state routed to **one shard**.
///
/// Before #1950 these five maps were the whole router under one vault-wide
/// lock. They keep the same shape, so a call site holding one shard's guard
/// reads `shard.levels.get(&cf)` exactly as it always did.
#[derive(Debug, Default)]
pub(super) struct RouterShard {
    pub(super) memtables: HashMap<ColumnFamily, Memtable>,
    pub(super) levels: HashMap<ColumnFamily, SstLevel>,
    pub(super) next_file: HashMap<ColumnFamily, u64>,
    /// Memtables sealed but not yet installed as SSTs, oldest first (#1949).
    ///
    /// These are still served by every read path in `queries.rs`: a sealed
    /// memtable holds the newest state for its keys until its SST lands, so
    /// omitting it from reads would serve stale rows.
    pub(super) sealed: HashMap<ColumnFamily, VecDeque<PendingFlush>>,
    /// CFs whose directory this router has already created, so the per-write
    /// `ensure_cf` repeats neither the syscall nor the path construction.
    ///
    /// Keyed by CF rather than by path because `cf_dir` is a pure function of
    /// `vault_dir`, `tiering_policy` and the CF, all fixed when the router is
    /// constructed — so a hit here proves the same directory was created, and
    /// the hot path needs no `PathBuf`. See [`RouterShard::ensure_cf`].
    pub(super) ensured_cfs: HashSet<ColumnFamily>,
}

/// The CF router, split into [`ColumnFamily::SHARDS`] independently-locked
/// shards routed by [`ColumnFamily::shard_index`] (#1950).
///
/// Every method takes `&self`, and that is the whole change. The vault holds
/// the router behind one `RwLock` whose **write** side used to be taken by
/// every commit, so a commit to `Kv` queued behind a `scan_cf_latest(Base)`
/// holding the read side across its entire SST merge — measured at a 253 ms
/// mean and a 2.40 s maximum on the deployment host. Commits and scans now both
/// take that outer lock for read, and contend only on the shard of the column
/// family they actually name.
///
/// The shard vector is fixed at construction and never resized, so a shard
/// index is a pure function of the column family and there is no outer lock to
/// take before reaching one.
#[derive(Debug)]
pub struct CfRouter {
    pub(super) config: Arc<RouterConfig>,
    resource_counters: Arc<ResourceCounters>,
    shards: Box<[RwLock<RouterShard>]>,
    /// Monotonic seal identifier, so an install matches the exact memtable it
    /// wrote rather than "the one at the front".
    ///
    /// Router-wide rather than per-shard because a [`SealedFlush`] is matched
    /// by id; unique ids across shards mean an install can never find a same-id
    /// seal belonging to another column family.
    next_seal_id: AtomicU64,
}

impl RouterConfig {
    /// This column family's own memtable seal point, staggered below the
    /// configured cap so that sibling CFs do not all seal on the same write.
    ///
    /// Measured motivation (#1949): one constellation writes 12 equal-sized
    /// slot vectors to 12 slot CFs, so those CFs fill at identical rates and
    /// cross a single shared cap on the *same* commit. The FSV harness caught
    /// exactly that — 12 seals charged to one 27-row commit, 731 ms of SST
    /// writing paid by whichever caller happened to be holding the pen. The
    /// per-CF cost was never the problem; their synchronisation was.
    ///
    /// Spreading the seal points decorrelates them, so that burst becomes ~12
    /// separate commits paying one flush each. This is the same reasoning
    /// behind RocksDB's refusal to re-switch a CF that already has an
    /// immutable memtable pending (facebook/rocksdb#6364): the fix for a
    /// synchronised flush storm is to break the synchronisation, not to make
    /// each flush cheaper.
    ///
    /// Deterministic per CF name, so a given CF seals at the same fill level on
    /// every run and across restarts. Only ever *reduces* the cap, so the
    /// configured value stays a true upper bound and the
    /// `ensure_batch_admitted` single-row check stays the binding one.
    pub(super) fn cf_memtable_cap(&self, cf: ColumnFamily) -> usize {
        let stagger = (cf_name_hash(&cf.name()) as usize) % (MEMTABLE_CAP_STAGGER_PCT + 1);
        let reduction = self.memtable_byte_cap / 100 * stagger;
        self.memtable_byte_cap.saturating_sub(reduction).max(1)
    }

    pub(super) fn retains_lookup(&self, cf: ColumnFamily) -> bool {
        cf == ColumnFamily::Kv
            || (self.eager_lookup_on_open
                && (self.selected_lookup_is_universal || cf.retains_eager_lookup()))
    }

    pub(super) fn cf_dir(&self, cf: ColumnFamily) -> PathBuf {
        self.tiering_policy.as_ref().map_or_else(
            || self.vault_dir.join("cf").join(cf.name()),
            |policy| policy.place_current_cf(cf).absolute_dir(),
        )
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

    pub(super) fn open_value(
        &self,
        cf: ColumnFamily,
        key: &[u8],
        value: Vec<u8>,
    ) -> Result<Vec<u8>> {
        match &self.value_crypto {
            Some(context) => open_encrypted_value_owned(context, cf, key, value),
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
}

impl RouterShard {
    /// Ensures the CF has a directory on disk and an entry in every in-memory
    /// map, creating the directory only the first time this router sees it.
    ///
    /// This runs on **every** `put_at`, so the `create_dir_all` it used to make
    /// unconditionally was one syscall per row written. Measured on a 27-row
    /// commit on the deployment host that was 3.0-4.3 ms, 92% of all non-flush
    /// router apply time and paid on every commit rather than only on the rare
    /// ones that flush (#1948).
    ///
    /// Deliberately not a fallback: if the directory is removed underneath a
    /// running vault, the next SST write fails loudly with the real filesystem
    /// error instead of being silently papered over by a re-create.
    pub(super) fn ensure_cf(&mut self, config: &RouterConfig, cf: ColumnFamily) -> Result<()> {
        // Recorded only after the directory exists: a failed create must leave
        // the CF unensured so the next write retries it rather than assuming a
        // directory that was never made.
        if !self.ensured_cfs.contains(&cf) {
            fs::create_dir_all(config.cf_dir(cf))
                .map_err(|error| CalyxError::disk_pressure(format!("create CF dir: {error}")))?;
            self.ensured_cfs.insert(cf);
        }
        let cap = config.cf_memtable_cap(cf);
        self.memtables
            .entry(cf)
            .or_insert_with(|| Memtable::new(cap));
        self.levels.entry(cf).or_default();
        self.next_file.entry(cf).or_insert(1);
        Ok(())
    }

    fn memtable_mut(&mut self, config: &RouterConfig, cf: ColumnFamily) -> &mut Memtable {
        let cap = config.cf_memtable_cap(cf);
        self.memtables
            .entry(cf)
            .or_insert_with(|| Memtable::new(cap))
    }

    pub(super) fn next_sequence(&mut self, cf: ColumnFamily) -> u64 {
        let next = self.next_file.entry(cf).or_insert(1);
        let seq = *next;
        *next += 1;
        seq
    }

    /// Every in-memory read source for `cf`, **newest first**: the active
    /// memtable, then each sealed-but-not-yet-installed memtable from newest to
    /// oldest.
    ///
    /// Every read path must merge across all of these. A sealed memtable holds
    /// the newest state for its keys until its SST is installed, so consulting
    /// only the active memtable would serve stale rows for the whole duration
    /// of a flush — precisely the window #1949 widened by moving the SST write
    /// out from under the locks.
    pub(super) fn read_tables(&self, cf: ColumnFamily) -> impl Iterator<Item = &Memtable> {
        self.memtables.get(&cf).into_iter().chain(
            self.sealed
                .get(&cf)
                .into_iter()
                .flat_map(|queue| queue.iter().rev().map(|pending| pending.table.as_ref())),
        )
    }

    /// The same sources **oldest first**, for callers that fold into a map and
    /// rely on a later insert overwriting an earlier one.
    pub(super) fn read_tables_oldest_first(
        &self,
        cf: ColumnFamily,
    ) -> impl Iterator<Item = &Memtable> {
        self.sealed
            .get(&cf)
            .into_iter()
            .flat_map(|queue| queue.iter().map(|pending| pending.table.as_ref()))
            .chain(self.memtables.get(&cf))
    }

    /// Publishes every written sealed memtable at the front of `cf`'s queue.
    fn drain_installable(&mut self, cf: ColumnFamily) {
        loop {
            let Some(queue) = self.sealed.get_mut(&cf) else {
                return;
            };
            if !queue
                .front()
                .is_some_and(|pending| pending.prepared.is_some())
            {
                if queue.is_empty() {
                    self.sealed.remove(&cf);
                }
                return;
            }
            let Some(mut pending) = queue.pop_front() else {
                return;
            };
            let Some(prepared) = pending.prepared.take() else {
                return;
            };
            self.levels.entry(cf).or_default().push_prepared(prepared);
        }
    }
}

/// A write guard over a chosen set of router shards, held in shard-index
/// order.
///
/// **The ordering is the deadlock argument.** `ColumnFamily::shard_index` is a
/// pure function, so every writer needing several shards acquires them in the
/// same total order; two such writers cannot each hold what the other wants.
pub(super) struct RouterWriteSet<'a> {
    /// `(shard_index, guard)`, ascending by shard index.
    guards: Vec<(usize, RwLockWriteGuard<'a, RouterShard>)>,
}

impl RouterWriteSet<'_> {
    /// The held shard owning `cf`.
    ///
    /// # Errors
    ///
    /// Fails closed when this set does not hold that shard, because writing
    /// through a lock that was never taken is the data race the set exists to
    /// prevent.
    pub(super) fn shard_mut(&mut self, cf: ColumnFamily) -> Result<&mut RouterShard> {
        let index = cf.shard_index();
        let slot = self
            .guards
            .binary_search_by_key(&index, |(held, _)| *held)
            .map_err(|_| {
                CalyxError::aster_corrupt_shard(format!(
                    "router load tried to install {} but did not lock its shard {index}; the load's lock set is wrong (#1950)",
                    cf.name()
                ))
            })?;
        Ok(&mut self.guards[slot].1)
    }
}

impl CfRouter {
    /// The shard owning `cf`.
    fn shard(&self, cf: ColumnFamily) -> &RwLock<RouterShard> {
        &self.shards[cf.shard_index()]
    }

    /// Read guard on the shard owning `cf`.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when the shard's lock is poisoned.
    pub(super) fn read_shard(&self, cf: ColumnFamily) -> Result<RwLockReadGuard<'_, RouterShard>> {
        self.shard(cf).read().map_err(|_| poisoned_shard(cf))
    }

    /// Write guard on the shard owning `cf`.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when the shard's lock is poisoned.
    pub(super) fn write_shard(
        &self,
        cf: ColumnFamily,
    ) -> Result<RwLockWriteGuard<'_, RouterShard>> {
        self.shard(cf).write().map_err(|_| poisoned_shard(cf))
    }

    /// Read guards on **every** shard, in index order, for the few questions
    /// that genuinely span column families (usage census, seal census,
    /// whole-router watermark proofs).
    ///
    /// Index order is the deadlock argument: every multi-shard acquisition in
    /// this type ascends, so two of them cannot each hold what the other wants.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when any shard's lock is poisoned.
    pub(super) fn read_all_shards(&self) -> Result<Vec<RwLockReadGuard<'_, RouterShard>>> {
        self.shards
            .iter()
            .map(|shard| shard.read().map_err(|_| poisoned_all_shards()))
            .collect()
    }

    /// Write guards for exactly the shards owning `cfs`, ascending by shard
    /// index, deduplicated.
    ///
    /// The ascent is the deadlock argument, and taking only what is named is
    /// the point: a reclaim that refreshes three column families must exclude
    /// readers of those three, and has no reason to stop the other 49 (#1950).
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when any shard's lock is poisoned.
    pub(super) fn write_shards_for(
        &self,
        cfs: impl IntoIterator<Item = ColumnFamily>,
    ) -> Result<RouterWriteSet<'_>> {
        let mut indexes: Vec<usize> = cfs.into_iter().map(ColumnFamily::shard_index).collect();
        indexes.sort_unstable();
        indexes.dedup();
        let mut guards = Vec::with_capacity(indexes.len());
        for index in indexes {
            guards.push((
                index,
                self.shards[index]
                    .write()
                    .map_err(|_| poisoned_all_shards())?,
            ));
        }
        Ok(RouterWriteSet { guards })
    }

    /// Write guards on every shard, in index order, for whole-router
    /// maintenance whose column-family set is discovered rather than given.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when any shard's lock is poisoned.
    pub(super) fn write_all_shards(&self) -> Result<RouterWriteSet<'_>> {
        let mut guards = Vec::with_capacity(self.shards.len());
        for (index, lock) in self.shards.iter().enumerate() {
            guards.push((index, lock.write().map_err(|_| poisoned_all_shards())?));
        }
        Ok(RouterWriteSet { guards })
    }

    pub(super) fn vault_dir(&self) -> &Path {
        &self.config.vault_dir
    }

    pub(crate) fn prove_persistent_search_content_watermark(
        &self,
        durable_seq: u64,
    ) -> Result<u64> {
        let mut watermark = 0_u64;
        for shard in self.read_all_shards()? {
            for (cf, level) in &shard.levels {
                if cf.feeds_persistent_search_index() {
                    watermark = watermark.max(level.prove_commit_domain_watermark(durable_seq)?);
                }
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
        let router = Self::new_empty(
            vault_dir,
            memtable_byte_cap,
            tiering_policy,
            value_crypto,
            eager_lookup_on_open,
            true,
        )?;
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
        let router = Self::new_empty(
            vault_dir,
            memtable_byte_cap,
            tiering_policy,
            value_crypto,
            eager_lookup_on_open,
            false,
        )?;
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
        eager_lookup_on_open: bool,
        selected_lookup_is_universal: bool,
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
            config: Arc::new(RouterConfig {
                vault_dir,
                tiering_policy,
                memtable_byte_cap,
                value_crypto,
                eager_lookup_on_open,
                selected_lookup_is_universal,
            }),
            resource_counters: Arc::new(ResourceCounters::default()),
            shards: (0..ColumnFamily::SHARDS)
                .map(|_| RwLock::new(RouterShard::default()))
                .collect(),
            next_seal_id: AtomicU64::new(0),
        })
    }

    /// Raw write with no commit domain; see [`Self::put_at`].
    ///
    /// Writes and installs any SST the write seals inline, because a standalone
    /// caller has no lock to release and therefore nothing to gain from
    /// deferring it.
    pub fn put(&self, cf: ColumnFamily, key: &[u8], value: &[u8]) -> Result<()> {
        let mut sealed = Vec::new();
        let result = self.put_at(cf, key, value, NO_COMMIT_DOMAIN, &mut sealed);
        self.write_and_install_sealed(&sealed)?;
        result.map(|_| ())
    }

    /// Writes and installs sealed memtables, in seal order.
    ///
    /// Callers on the commit path invoke this **after** releasing the vault's
    /// row-table and router write locks (#1949); callers with no lock to
    /// release use it inline.
    pub fn write_and_install_sealed(&self, sealed: &[SealedFlush]) -> Result<()> {
        for handle in sealed {
            match handle.write_sst() {
                Ok(written) => self.install_sealed(handle, written)?,
                Err(write_error) => {
                    self.abandon_sealed(handle, &write_error)?;
                    return Err(write_error);
                }
            }
        }
        Ok(())
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
    /// Takes the shard owning `cf` for write **once** and holds it for the
    /// whole call. `std::sync::RwLock` is not reentrant, so every helper below
    /// this point receives the guard rather than re-acquiring it.
    pub fn put_at(
        &self,
        cf: ColumnFamily,
        key: &[u8],
        value: &[u8],
        commit_watermark: u64,
        sealed: &mut Vec<SealedFlush>,
    ) -> Result<RouterPutCost> {
        let mut outcome = RouterPutCost::default();
        let lock_started = Instant::now();
        let mut shard = self.write_shard(cf)?;
        outcome.lock_wait_us = elapsed_us(&lock_started);
        let ensure_started = Instant::now();
        shard.ensure_cf(&self.config, cf)?;
        self.ensure_cf_write_fanout_admitted_in(&shard, cf)?;
        outcome.ensure_cf_us = elapsed_us(&ensure_started);
        let mut counted_backpressure = false;
        let ack = match shard.memtable_mut(&self.config, cf).write(key, value, 0) {
            Ok(ack) => ack,
            Err(error) => {
                if error.code != "CALYX_BACKPRESSURE" {
                    return Err(error);
                }
                // Sealing alone relieves the backpressure: the active memtable
                // is empty again once its contents move to the pending queue,
                // so the retry does not need the SST to have been written.
                outcome.record_seal(self.timed_seal_cf_at(
                    &mut shard,
                    cf,
                    commit_watermark,
                    sealed,
                )?);
                match shard.memtable_mut(&self.config, cf).write(key, value, 0) {
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
            outcome.record_seal(self.timed_seal_cf_at(&mut shard, cf, commit_watermark, sealed)?);
        }
        Ok(outcome)
    }

    /// [`Self::seal_cf_at`] with its wall-clock cost, collecting the handle.
    fn timed_seal_cf_at(
        &self,
        shard: &mut RouterShard,
        cf: ColumnFamily,
        commit_watermark: u64,
        sealed: &mut Vec<SealedFlush>,
    ) -> Result<u64> {
        let started = Instant::now();
        let handle = self.seal_cf_in(shard, cf, commit_watermark)?;
        sealed.push(handle);
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
            {
                let shard = self.read_shard(cf)?;
                self.ensure_cf_write_fanout_admitted_in(&shard, cf)?;
            }
            let row_bytes = Memtable::entry_size(key.as_ref(), value.as_ref());
            // Checked against this CF's own staggered cap, not the configured
            // one. The stagger only ever lowers a CF's cap, so screening
            // against the global value would admit a row that the CF's
            // memtable then refuses — turning a pre-WAL fail-closed check into
            // a mid-commit failure, which is the opposite of what this
            // function exists to do.
            let cap = self.config.cf_memtable_cap(cf);
            if row_bytes > cap {
                self.resource_counters.record_memtable_rejected();
                return Err(CalyxError::backpressure(format!(
                    "memtable byte cap {cap} for {} cannot fit row of {row_bytes} bytes",
                    cf.name()
                )));
            }
        }
        Ok(())
    }

    /// Fan-out admission against a shard guard the caller already holds.
    ///
    /// Takes the guard rather than the CF because both callers are already
    /// inside the shard, and `std::sync::RwLock` is not reentrant: re-entering
    /// through `level_file_count` would deadlock a writer against itself.
    fn ensure_cf_write_fanout_admitted_in(
        &self,
        shard: &RouterShard,
        cf: ColumnFamily,
    ) -> Result<()> {
        let current_files = shard.levels.get(&cf).map_or(0, SstLevel::file_count);
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

    /// # Panics
    ///
    /// Panics when a router shard's lock is poisoned.
    pub fn memtable_usage_by_cf(&self) -> Vec<(ColumnFamily, MemtableUsage)> {
        let guards = self
            .read_all_shards()
            .expect("CF router shard poisoned during memtable usage census");
        let mut usage = guards
            .iter()
            .flat_map(|shard| shard.memtables.iter())
            .map(|(cf, table)| (*cf, table.usage()))
            .collect::<Vec<_>>();
        usage.sort_by_key(|left| left.0.name());
        usage
    }

    /// Raw flush with no commit domain; see [`Self::flush_cf_at`].
    pub fn flush_cf(&self, cf: ColumnFamily) -> Result<SstSummary> {
        self.flush_cf_at(cf, NO_COMMIT_DOMAIN)
    }

    /// Flushes one CF's memtable to a commit-anchored flush SST
    /// (`flush-{watermark:020}-{ordinal:04}.sst`). `commit_watermark` must be
    /// the highest commit seq whose rows can be in the memtable; understating
    /// it is safe (the file sorts earlier and committed rows keep their
    /// durable-batch home), overstating it can shadow newer durable batches.
    pub fn flush_cf_at(&self, cf: ColumnFamily, commit_watermark: u64) -> Result<SstSummary> {
        // Seals unconditionally, including an empty memtable: the pre-#1949
        // path wrote an empty SST in that case and `flush_cf` has callers
        // outside this crate that may hit it, so the observable behaviour of
        // this entry point is unchanged.
        //
        // The seal takes the shard guard and **drops it** before `write_sst`:
        // the SST write is the expensive part, and holding the shard across it
        // would put the flush back on the critical path of every commit to the
        // same column family (#1949, #1950).
        let sealed = self.seal_cf_at(cf, commit_watermark)?;
        match sealed.write_sst() {
            Ok(written) => {
                let summary = written.summary().clone();
                self.install_sealed(&sealed, written)?;
                Ok(summary)
            }
            Err(write_error) => {
                self.abandon_sealed(&sealed, &write_error)?;
                Err(write_error)
            }
        }
    }

    /// Seals `cf`'s active memtable and installs a fresh empty one.
    ///
    /// This is the "switch" half of switch-then-flush (#1949). It is a pointer
    /// swap plus a queue push — no encryption, no file I/O — so it is safe to
    /// perform while the vault's row-table and router write locks are held. The
    /// returned handle owns everything the SST write needs, and the caller is
    /// expected to release those locks before calling
    /// [`SealedFlush::write_sst`] and then [`Self::install_sealed`].
    ///
    /// The sealed memtable stays in this router's read path until it is
    /// installed, so no reader can miss its rows during the write.
    ///
    /// Seals unconditionally, empty memtable included, so that
    /// [`Self::flush_cf_at`] keeps its pre-#1949 behaviour for callers that
    /// flush a CF without knowing whether it has rows.
    pub fn seal_cf_at(&self, cf: ColumnFamily, commit_watermark: u64) -> Result<SealedFlush> {
        let mut shard = self.write_shard(cf)?;
        self.seal_cf_in(&mut shard, cf, commit_watermark)
    }

    /// [`Self::seal_cf_at`] against a shard guard the caller already holds.
    fn seal_cf_in(
        &self,
        shard: &mut RouterShard,
        cf: ColumnFamily,
        commit_watermark: u64,
    ) -> Result<SealedFlush> {
        shard.ensure_cf(&self.config, cf)?;
        let depth = shard.sealed.get(&cf).map_or(0, VecDeque::len);
        if depth >= MAX_SEALED_MEMTABLES_PER_CF {
            self.resource_counters.record_memtable_rejected();
            return Err(CalyxError {
                code: "CALYX_ASTER_ROUTER_SEALED_MEMTABLE_BACKLOG",
                message: format!(
                    "{} already holds {depth} sealed memtables awaiting their SST (cap {MAX_SEALED_MEMTABLES_PER_CF}); refusing to seal another",
                    cf.name()
                ),
                remediation: "a sealed memtable is not being written or installed; inspect CALYX_ASTER_ROUTER_FLUSH_* telemetry and the vault volume for write failures",
            });
        }
        let ordinal = shard.next_sequence(cf);
        let ordinal = usize::try_from(ordinal).map_err(|_| {
            CalyxError::aster_corrupt_shard(format!(
                "flush ordinal {ordinal} for {} exceeds the platform's usize range",
                cf.name()
            ))
        })?;
        let path = self
            .config
            .cf_dir(cf)
            .join(flush_sst_file_name(commit_watermark, ordinal));
        let fresh = Memtable::new(self.config.cf_memtable_cap(cf));
        let table = Arc::new(std::mem::replace(
            shard.memtable_mut(&self.config, cf),
            fresh,
        ));
        // Router-wide and monotonic across shards, so an install can never
        // match a same-id seal belonging to another column family.
        let id = self.next_seal_id.fetch_add(1, Ordering::Relaxed) + 1;
        shard.sealed.entry(cf).or_default().push_back(PendingFlush {
            id,
            table: Arc::clone(&table),
            path: path.clone(),
            prepared: None,
        });
        Ok(SealedFlush {
            cf,
            id,
            path,
            table,
            value_crypto: self.config.value_crypto.clone(),
            retain_lookup: self.config.retains_lookup(cf),
        })
    }

    /// Publishes a written SST into its level and retires the sealed memtable.
    ///
    /// Installs drain the per-CF queue **in seal order**: a newer SST whose
    /// write finished first still waits behind an older sibling, because
    /// `push_with_lookup` encodes recency by position and installing out of
    /// order would invert read precedence and serve stale rows.
    pub fn install_sealed(&self, sealed: &SealedFlush, written: WrittenFlush) -> Result<()> {
        let mut shard = self.write_shard(sealed.cf)?;
        let queue = shard.sealed.get_mut(&sealed.cf).ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "install of {} seal {} found no sealed-memtable queue",
                sealed.cf.name(),
                sealed.id
            ))
        })?;
        let pending = queue
            .iter_mut()
            .find(|pending| pending.id == sealed.id)
            .ok_or_else(|| {
                CalyxError::aster_corrupt_shard(format!(
                    "install of {} seal {} found no matching sealed memtable",
                    sealed.cf.name(),
                    sealed.id
                ))
            })?;
        // The file the writer actually produced must be the one this queue
        // entry planned, so an install can never publish a name the write did
        // not create. Checked against the queue entry rather than the caller's
        // handle so the authority is the router's own record of the seal.
        if written.summary.path != pending.path {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "router flush for {} wrote {} but seal {} planned {}",
                sealed.cf.name(),
                written.summary.path.display(),
                sealed.id,
                pending.path.display()
            )));
        }
        pending.prepared = Some(written.prepared);
        shard.drain_installable(sealed.cf);
        Ok(())
    }

    /// Returns a failed seal's rows to the active memtable.
    ///
    /// Without this the rows would sit in the pending queue forever, blocking
    /// every later install for the CF and eventually tripping the backlog cap.
    /// A target SST that did reach disk is intentionally retained as an
    /// idempotent physical projection, exactly as the pre-#1949 path did.
    pub fn abandon_sealed(&self, sealed: &SealedFlush, write_error: &CalyxError) -> Result<()> {
        let mut shard = self.write_shard(sealed.cf)?;
        if let Some(queue) = shard.sealed.get_mut(&sealed.cf) {
            queue.retain(|pending| pending.id != sealed.id);
            if queue.is_empty() {
                shard.sealed.remove(&sealed.cf);
            }
        }
        let rows = sealed.table.iter().collect::<Vec<_>>();
        let memtable = shard.memtable_mut(&self.config, sealed.cf);
        let mut restored = Ok(());
        for (key, value) in rows {
            if let Err(error) = memtable.write(&key, &value, 0) {
                restored = Err(error);
                break;
            }
        }
        if let Err(restore_error) = restored {
            tracing::error!(
                code = "CALYX_ASTER_ROUTER_FLUSH_RESTORE_FAILED",
                cf = sealed.cf.name(),
                path = %sealed.path.display(),
                write_error_code = write_error.code,
                write_error = %write_error,
                restore_error_code = restore_error.code,
                restore_error = %restore_error,
                "router flush failed and the sealed memtable could not be restored"
            );
            return Err(CalyxError::aster_corrupt_shard(format!(
                "router flush for {} failed and the sealed memtable could not be restored: write=error[{}]: {}; restore=error[{}]: {}",
                sealed.cf.name(),
                write_error.code,
                write_error.message,
                restore_error.code,
                restore_error.message
            )));
        }
        tracing::error!(
            code = write_error.code,
            cf = sealed.cf.name(),
            path = %sealed.path.display(),
            error = %write_error,
            "router flush failed; restored the complete sealed memtable before returning the error"
        );
        Ok(())
    }

    /// Sealed memtables awaiting their SST, for health readback and tests.
    ///
    /// # Panics
    ///
    /// Panics when a router shard's lock is poisoned.
    #[must_use]
    pub fn sealed_memtable_count(&self) -> usize {
        self.read_all_shards()
            .expect("CF router shard poisoned during sealed-memtable census")
            .iter()
            .flat_map(|shard| shard.sealed.values())
            .map(VecDeque::len)
            .sum()
    }

    /// Ensures the CF has a directory on disk and an entry in every in-memory
    /// map. See [`RouterShard::ensure_cf`].
    ///
    /// # Errors
    ///
    /// Returns an error when the CF directory cannot be created, or a
    /// corrupt-shard error when the shard's lock is poisoned.
    pub(super) fn ensure_cf(&self, cf: ColumnFamily) -> Result<()> {
        self.write_shard(cf)?.ensure_cf(&self.config, cf)
    }

    pub(super) fn cf_dir(&self, cf: ColumnFamily) -> PathBuf {
        self.config.cf_dir(cf)
    }

    pub(super) fn open_value(
        &self,
        cf: ColumnFamily,
        key: &[u8],
        value: Vec<u8>,
    ) -> Result<Vec<u8>> {
        self.config.open_value(cf, key, value)
    }

    pub(super) fn open_entries<I>(&self, cf: ColumnFamily, entries: I) -> Result<Vec<SstEntry>>
    where
        I: IntoIterator<Item = SstEntry>,
    {
        self.config.open_entries(cf, entries)
    }

    pub(super) fn cf_roots(&self) -> Vec<PathBuf> {
        self.config.cf_roots()
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
    pub(crate) fn retire_cf(&self, cf: ColumnFamily) -> Result<RetiredCfPhysical> {
        // Held across the whole retire, physical removal included: a concurrent
        // write to this CF between the state drop and the unlink would recreate
        // the directory and leave the fail-closed readback below reporting a CF
        // that was never actually retired. Only this one family's shard is
        // held, so every other CF keeps serving throughout (#1950).
        let mut shard = self.write_shard(cf)?;
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
        shard.memtables.remove(&cf);
        shard.levels.remove(&cf);
        shard.next_file.remove(&cf);
        shard.ensured_cfs.remove(&cf);
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

//! The reflex tick's frozen-intelligence feed (#1686).
//!
//! The scheduler tick must never issue a live Calyx call, so the guard
//! thresholds it treats as authoritative are read from a **lowered** artifact:
//! computed off-runtime by the storage maintenance pass, frozen into a
//! content-fingerprinted file, atomically published, and handed to the tick
//! through an `arc_swap::ArcSwap` pointer.
//!
//! Two threads participate and they are deliberately different threads:
//!
//! * The **tick thread** calls [`LoweredGuardThresholdFeed::load_for_tick`] once
//!   at tick start. That is a single `ArcSwap::load_full` — lock-free and
//!   wait-free per the `arc-swap` documentation, roughly 50 ns uncontended, with
//!   no filesystem access and no Calyx call.
//! * The **refresher thread** (spawned here, never the tick thread) calls
//!   `LoweredArtifactHandle::refresh` on an interval. Refresh does the file
//!   read, the magic/kind/schema checks and the SHA-256 fingerprint
//!   re-verification, then swaps the pointer. A missing, corrupt, mismatched or
//!   stale artifact swaps in the documented fail-closed safe default; it never
//!   falls back to a live Calyx call.
//!
//! A scheduler started without an audit database has no vault at all, and that
//! case is reported as such ([`REFLEX_LOWERED_ARTIFACT_NO_VAULT`]) rather than
//! being papered over with a fabricated artifact path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use synapse_calyx::{
    LoweredArtifactHandle, LoweredArtifactKind, LoweredArtifactState, LoweredGuardThresholds,
};

/// Off-tick refresh cadence for the lowered artifact.
///
/// Fast enough that a maintenance publish becomes visible to the tick within one
/// cadence, cheap enough to be irrelevant: one ~1 KiB read, one JSON parse and
/// one SHA-256 over the frozen payload, on a dedicated thread.
pub const LOWERED_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Structured code for a refresher thread that could not be spawned.
pub const REFLEX_LOWERED_REFRESHER_SPAWN_FAILED: &str = "REFLEX_LOWERED_REFRESHER_SPAWN_FAILED";

/// Structured code reported when the scheduler runs without a vault-backed
/// database, so no lowered artifact can exist to be consumed.
pub const REFLEX_LOWERED_ARTIFACT_NO_VAULT: &str = "REFLEX_LOWERED_ARTIFACT_NO_VAULT";

/// Counters describing what the tick actually consumed.
#[derive(Debug, Default)]
struct FeedCounters {
    fresh_refreshes: AtomicU64,
    safe_default_refreshes: AtomicU64,
    ticks_on_fresh: AtomicU64,
    ticks_on_safe_default: AtomicU64,
}

/// Point-in-time, externally readable view of the feed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoweredFeedSnapshot {
    pub artifact_path: Option<PathBuf>,
    pub state: &'static str,
    pub safe_default_code: Option<String>,
    pub safe_default_detail: Option<String>,
    pub safe_default_remediation: Option<String>,
    pub content_sha256: Option<String>,
    pub generation: Option<u64>,
    pub source_ledger_seq: Option<u64>,
    pub vault_id: Option<String>,
    pub produced_at_unix_ms: Option<u64>,
    pub staleness_bound_ms: Option<u64>,
    pub hot_reads_total: u64,
    pub refreshes_total: u64,
    pub fresh_refreshes_total: u64,
    pub safe_default_refreshes_total: u64,
    pub ticks_on_fresh_total: u64,
    pub ticks_on_safe_default_total: u64,
    pub refresher_running: bool,
    pub refresher_interval_ms: u64,
}

/// The tick's frozen guard-threshold source of record.
#[derive(Debug)]
pub struct LoweredGuardThresholdFeed {
    /// `None` when the scheduler has no vault-backed database.
    handle: Option<Arc<LoweredArtifactHandle>>,
    counters: FeedCounters,
    refresher_running: AtomicBool,
    refresh_interval: Duration,
}

impl LoweredGuardThresholdFeed {
    /// Creates a feed rooted at `vault_dir` and performs one initial off-tick
    /// refresh. `None` builds the vault-less feed.
    ///
    /// Called from the runtime-construction path, never from a tick.
    #[must_use]
    pub fn new(vault_dir: Option<&Path>) -> Arc<Self> {
        let handle = vault_dir.map(|dir| {
            Arc::new(LoweredArtifactHandle::unloaded(
                dir,
                LoweredArtifactKind::GuardThresholds,
            ))
        });
        let feed = Arc::new(Self {
            handle,
            counters: FeedCounters::default(),
            refresher_running: AtomicBool::new(false),
            refresh_interval: LOWERED_REFRESH_INTERVAL,
        });
        feed.refresh_off_tick();
        feed
    }

    /// Hot-path entry point: one lock-free atomic load, then the frozen
    /// thresholds for this tick.
    ///
    /// No filesystem access, no Calyx call, no allocation beyond the `Arc`
    /// clone. When the artifact is absent, corrupt, or stale the returned value
    /// is the documented fail-closed default rather than anything live.
    pub fn load_for_tick(&self, now_unix_ms: u64) -> LoweredGuardThresholds {
        let Some(handle) = self.handle.as_ref() else {
            self.counters
                .ticks_on_safe_default
                .fetch_add(1, Ordering::Relaxed);
            return LoweredGuardThresholds::fail_closed_default();
        };
        let state = handle.load();
        if state.is_fresh() {
            self.counters.ticks_on_fresh.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters
                .ticks_on_safe_default
                .fetch_add(1, Ordering::Relaxed);
        }
        state.guard_thresholds(now_unix_ms)
    }

    /// Re-reads and re-verifies the published artifact. **Off-tick only** — the
    /// boundary guard records a violation if this is ever reached from a tagged
    /// hot thread.
    pub fn refresh_off_tick(&self) {
        crate::hot_path::guard_cold("lowered_guard_thresholds_refresh");
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let outcome = handle.refresh(crate::hot_path::unix_time_ms_now());
        if outcome.became_fresh {
            self.counters
                .fresh_refreshes
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters
                .safe_default_refreshes
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot for `health`.
    #[must_use]
    pub fn snapshot(&self) -> LoweredFeedSnapshot {
        let mut snapshot = LoweredFeedSnapshot {
            state: "safe_default",
            ticks_on_fresh_total: self.counters.ticks_on_fresh.load(Ordering::Relaxed),
            ticks_on_safe_default_total: self
                .counters
                .ticks_on_safe_default
                .load(Ordering::Relaxed),
            fresh_refreshes_total: self.counters.fresh_refreshes.load(Ordering::Relaxed),
            safe_default_refreshes_total: self
                .counters
                .safe_default_refreshes
                .load(Ordering::Relaxed),
            refresher_running: self.refresher_running.load(Ordering::Relaxed),
            refresher_interval_ms: u64::try_from(self.refresh_interval.as_millis())
                .unwrap_or(u64::MAX),
            ..LoweredFeedSnapshot::default()
        };
        let Some(handle) = self.handle.as_ref() else {
            snapshot.safe_default_code = Some(REFLEX_LOWERED_ARTIFACT_NO_VAULT.to_owned());
            snapshot.safe_default_detail = Some(
                "the reflex scheduler was started without a vault-backed database, so no lowered \
                 guard-threshold artifact can exist"
                    .to_owned(),
            );
            snapshot.safe_default_remediation = Some(
                "start the reflex runtime against an open Calyx vault; the tick is on the \
                 documented fail-closed guard thresholds until then"
                    .to_owned(),
            );
            return snapshot;
        };
        snapshot.artifact_path = Some(handle.path().to_path_buf());
        snapshot.hot_reads_total = handle.read_count();
        snapshot.refreshes_total = handle.refresh_count();
        match handle.load().as_ref() {
            LoweredArtifactState::Fresh(artifact) => {
                snapshot.state = "fresh";
                snapshot.content_sha256 = Some(artifact.fingerprint.content_sha256.clone());
                snapshot.generation = Some(artifact.fingerprint.generation);
                snapshot.source_ledger_seq = Some(artifact.fingerprint.source_ledger_seq);
                snapshot.vault_id = Some(artifact.fingerprint.vault_id.clone());
                snapshot.produced_at_unix_ms = Some(artifact.fingerprint.produced_at_unix_ms);
                snapshot.staleness_bound_ms = Some(artifact.staleness_bound_ms);
            }
            LoweredArtifactState::SafeDefault(default) => {
                snapshot.safe_default_code = Some(default.code.to_owned());
                snapshot.safe_default_detail = Some(default.message.clone());
                snapshot.safe_default_remediation = Some(default.remediation.to_owned());
            }
        }
        snapshot
    }
}

/// Owns the dedicated off-tick refresher thread.
#[derive(Debug)]
pub struct LoweredRefresher {
    feed: Arc<LoweredGuardThresholdFeed>,
    stop: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl LoweredRefresher {
    /// Spawns the refresher thread for `feed`.
    ///
    /// Deliberately a plain OS thread, not a tokio task and never the tick
    /// thread: the refresh performs blocking file I/O, and running it anywhere
    /// the tick could queue behind it would reintroduce exactly the coupling the
    /// boundary exists to remove. A vault-less feed has nothing to refresh and
    /// spawns no thread.
    ///
    /// # Errors
    ///
    /// Fails closed with [`REFLEX_LOWERED_REFRESHER_SPAWN_FAILED`] when the OS
    /// refuses the thread: a feed that could never refresh would pin the tick to
    /// the fail-closed defaults forever while reporting a healthy scheduler.
    pub fn start(feed: Arc<LoweredGuardThresholdFeed>) -> Result<Self, crate::ReflexError> {
        let stop = Arc::new(AtomicBool::new(false));
        if feed.handle.is_none() {
            return Ok(Self {
                feed,
                stop,
                join: Mutex::new(None),
            });
        }
        let thread_feed = Arc::clone(&feed);
        let thread_stop = Arc::clone(&stop);
        let interval = feed.refresh_interval;
        let join = thread::Builder::new()
            .name("synapse-reflex-lowered-refresh".to_owned())
            .spawn(move || {
                thread_feed.refresher_running.store(true, Ordering::Relaxed);
                // Poll in short slices so shutdown is prompt without making the
                // refresh itself any more frequent.
                let slice = Duration::from_millis(100);
                let mut waited = Duration::ZERO;
                while !thread_stop.load(Ordering::Acquire) {
                    if waited >= interval {
                        thread_feed.refresh_off_tick();
                        waited = Duration::ZERO;
                    }
                    thread::sleep(slice);
                    waited = waited.saturating_add(slice);
                }
                thread_feed
                    .refresher_running
                    .store(false, Ordering::Relaxed);
            })
            .map_err(|error| crate::ReflexError::ParamsInvalid {
                detail: format!(
                    "{REFLEX_LOWERED_REFRESHER_SPAWN_FAILED}: lowered-artifact refresher thread \
                     spawn failed: {error}; the scheduler refuses to start because the tick would \
                     otherwise read a never-refreshed artifact"
                ),
            })?;
        Ok(Self {
            feed,
            stop,
            join: Mutex::new(Some(join)),
        })
    }

    /// The feed this refresher keeps current.
    #[must_use]
    pub const fn feed(&self) -> &Arc<LoweredGuardThresholdFeed> {
        &self.feed
    }

    /// Signals the thread to stop and joins it.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let handle = match self.join.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(handle) = handle {
            let _joined = handle.join();
        }
    }
}

impl Drop for LoweredRefresher {
    fn drop(&mut self) {
        self.stop();
    }
}

//! Snapshot-pin watchdog for MVCC reader leases (PRD 24 §4).

use calyx_core::{Clock, Seq, SystemClock, Ts};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod reclaimer;

pub use reclaimer::{
    CALYX_GC_ERROR, DEFAULT_GC_MAX_OPS_PER_RUN, DEFAULT_GC_MIN_INTERVAL_MS, GcMetrics, GcRateLimit,
    GcResult, GcScheduler, GcSchedulerTick, GcTask, SnapshotGcCounters, SnapshotGcReclaimer,
    SnapshotVersionGc, gc_error,
};

/// Reader id used by the watchdog and MVCC lease registry.
pub type ReaderId = u64;

/// Default reader lease age, matching the PH58 FoundationDB-style discipline.
pub const DEFAULT_READER_LEASE_MS: u64 = 5_000;

/// Default maximum allowed `newest_seq - oldest_pinned_seq` gap.
pub const DEFAULT_MAX_PINNED_SEQ_GAP: u64 = 1_000_000;

/// A bounded read lease that pins one MVCC sequence until expiry.
///
/// Calyx clocks use Unix-millisecond [`Ts`] values, so the persisted shape keeps
/// milliseconds while the public registration API accepts [`Duration`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadLease {
    pub seq: Seq,
    pub created_at: Ts,
    pub lease_duration_ms: u64,
    pub reader_id: ReaderId,
}

impl ReadLease {
    pub fn new(reader_id: ReaderId, seq: Seq, created_at: Ts, lease_duration: Duration) -> Self {
        Self::from_millis(reader_id, seq, created_at, duration_millis(lease_duration))
    }

    pub const fn from_millis(
        reader_id: ReaderId,
        seq: Seq,
        created_at: Ts,
        lease_duration_ms: u64,
    ) -> Self {
        Self {
            seq,
            created_at,
            lease_duration_ms,
            reader_id,
        }
    }

    pub fn is_expired(&self, clock: &dyn Clock) -> bool {
        self.is_expired_at(clock.now())
    }

    pub fn is_expired_at(&self, now: Ts) -> bool {
        now >= self.expires_at()
    }

    pub fn expires_at(&self) -> Ts {
        self.created_at.saturating_add(self.lease_duration_ms)
    }
}

/// Alert returned when an old reader pins too wide a sequence gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapAlert {
    pub gap: u64,
    pub oldest_reader_id: ReaderId,
    pub oldest_pinned_seq: Seq,
    pub newest_seq: Seq,
    pub max_gap_seqs: u64,
}

/// Watchdog metrics surfaced through resource-status Prometheus text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPinMetrics {
    pub reader_lease_expired_total: u64,
    pub oldest_pinned_seq_gap: u64,
}

/// One background-GC tick result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotGcTick {
    pub aborted_readers: Vec<ReaderId>,
    pub gap_alert: Option<GapAlert>,
    pub metrics: SnapshotPinMetrics,
}

/// Checkpoint-backed analytics snapshot that does not pin the live frontier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedStalenessSnapshot {
    checkpoint_seq: Seq,
}

impl BoundedStalenessSnapshot {
    pub const fn at_checkpoint(seq: Seq) -> Self {
        Self {
            checkpoint_seq: seq,
        }
    }

    pub const fn seq(self) -> Seq {
        self.checkpoint_seq
    }
}

/// Watchdog over active snapshot pins.
pub struct SnapshotPinWatchdog {
    leases: Mutex<HashMap<ReaderId, ReadLease>>,
    max_gap_seqs: u64,
    clock: Arc<dyn Clock>,
    reader_lease_expired_total: AtomicU64,
    oldest_pinned_seq_gap: AtomicU64,
}

impl fmt::Debug for SnapshotPinWatchdog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotPinWatchdog")
            .field("leases", &self.lock().len())
            .field("max_gap_seqs", &self.max_gap_seqs)
            .field(
                "reader_lease_expired_total",
                &self.reader_lease_expired_total(),
            )
            .field(
                "oldest_pinned_seq_gap",
                &self.oldest_pinned_seq_gap.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl Default for SnapshotPinWatchdog {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

impl SnapshotPinWatchdog {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_max_gap(clock, DEFAULT_MAX_PINNED_SEQ_GAP)
    }

    pub fn with_max_gap(clock: Arc<dyn Clock>, max_gap_seqs: u64) -> Self {
        Self {
            leases: Mutex::new(HashMap::new()),
            max_gap_seqs,
            clock,
            reader_lease_expired_total: AtomicU64::new(0),
            oldest_pinned_seq_gap: AtomicU64::new(0),
        }
    }

    pub fn register(&self, reader_id: ReaderId, seq: Seq, duration: Duration) {
        let lease = ReadLease::new(reader_id, seq, self.clock.now(), duration);
        self.register_lease(lease);
    }

    pub fn register_lease(&self, lease: ReadLease) {
        self.lock().insert(lease.reader_id, lease);
    }

    pub fn release(&self, reader_id: ReaderId) -> bool {
        self.lock().remove(&reader_id).is_some()
    }

    pub fn abort_reader(&self, reader_id: ReaderId) -> Option<ReadLease> {
        self.lock().remove(&reader_id)
    }

    pub fn abort_if_expired_at(&self, reader_id: ReaderId, now: Ts) -> bool {
        let mut leases = self.lock();
        let expired = leases
            .get(&reader_id)
            .is_some_and(|lease| lease.is_expired_at(now));
        if expired {
            leases.remove(&reader_id);
            self.reader_lease_expired_total
                .fetch_add(1, Ordering::Relaxed);
        }
        expired
    }

    pub fn check_and_abort_expired(&self) -> Vec<ReaderId> {
        self.check_and_abort_expired_at(self.clock.now())
    }

    pub fn check_and_abort_expired_at(&self, now: Ts) -> Vec<ReaderId> {
        let mut leases = self.lock();
        let mut expired = leases
            .iter()
            .filter_map(|(id, lease)| lease.is_expired_at(now).then_some(*id))
            .collect::<Vec<_>>();
        expired.sort_unstable();
        for id in &expired {
            leases.remove(id);
        }
        if !expired.is_empty() {
            self.reader_lease_expired_total
                .fetch_add(expired.len() as u64, Ordering::Relaxed);
        }
        expired
    }

    pub fn oldest_pinned_seq(&self) -> Option<Seq> {
        self.oldest_pinned_seq_at(self.clock.now())
    }

    pub fn oldest_pinned_seq_at(&self, now: Ts) -> Option<Seq> {
        self.check_and_abort_expired_at(now);
        self.lock().values().map(|lease| lease.seq).min()
    }

    pub fn lease_count(&self) -> usize {
        self.lock().len()
    }

    pub fn check_gap(&self, newest_seq: Seq) -> Option<GapAlert> {
        self.check_gap_at(newest_seq, self.clock.now())
    }

    pub fn check_gap_at(&self, newest_seq: Seq, now: Ts) -> Option<GapAlert> {
        self.check_gap_at_with_max(newest_seq, now, self.max_gap_seqs)
    }

    pub fn check_gap_at_with_max(
        &self,
        newest_seq: Seq,
        now: Ts,
        max_gap_seqs: u64,
    ) -> Option<GapAlert> {
        self.check_and_abort_expired_at(now);
        let oldest = self
            .lock()
            .values()
            .min_by_key(|lease| (lease.seq, lease.reader_id))
            .copied();
        let Some(oldest) = oldest else {
            self.oldest_pinned_seq_gap.store(0, Ordering::Relaxed);
            return None;
        };
        let gap = newest_seq.saturating_sub(oldest.seq);
        self.oldest_pinned_seq_gap.store(gap, Ordering::Relaxed);
        (gap > max_gap_seqs).then_some(GapAlert {
            gap,
            oldest_reader_id: oldest.reader_id,
            oldest_pinned_seq: oldest.seq,
            newest_seq,
            max_gap_seqs,
        })
    }

    pub fn metrics_at(&self, newest_seq: Seq, now: Ts) -> SnapshotPinMetrics {
        let _ = self.check_gap_at(newest_seq, now);
        SnapshotPinMetrics {
            reader_lease_expired_total: self.reader_lease_expired_total(),
            oldest_pinned_seq_gap: self.oldest_pinned_seq_gap.load(Ordering::Relaxed),
        }
    }

    pub fn reader_lease_expired_total(&self) -> u64 {
        self.reader_lease_expired_total.load(Ordering::Relaxed)
    }

    pub const fn max_gap_seqs(&self) -> u64 {
        self.max_gap_seqs
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ReaderId, ReadLease>> {
        self.leases.lock().expect("snapshot watchdog poisoned")
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

//! Active reader-lease registry for oldest-pinned-seq gap accounting.

use crate::gc::{
    GapAlert, ReadLease, ReadLeaseRenewal, ReaderId, SnapshotPinMetrics, SnapshotPinWatchdog,
};
use crate::mvcc::ReaderLease;
use calyx_core::{Seq, Ts};

/// Registry of live reader leases pinned through `VersionedCfStore`.
///
/// Bounded by construction (A26): every lease carries an expiry, and expired
/// entries are aborted on every watchdog tick/view, so the registry can never
/// grow past the set of leases that are still within their `max_age_ms` window.
/// Scoped vault reads release their lease when the operation ends; explicit
/// long-reader pins remain until caller release or expiry.
#[derive(Debug, Default)]
pub struct LeaseRegistry {
    watchdog: SnapshotPinWatchdog,
}

/// Live-lease view used by the resource status collector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseView {
    /// Number of unexpired leases.
    pub active_leases: usize,
    /// Smallest pinned sequence among unexpired leases.
    pub oldest_pinned_seq: Option<Seq>,
    /// Monotonic count of expired leases aborted by the watchdog.
    pub reader_lease_expired_total: u64,
}

/// Store-facing result of renewing one reader lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReaderLeaseRenewal {
    Renewed(ReaderLease),
    Missing,
    Expired,
    PinnedSeqMismatch { registered: Seq, requested: Seq },
}

impl LeaseRegistry {
    /// Registers a freshly issued lease.
    pub fn register(&self, lease: ReaderLease) {
        self.watchdog.register_lease(ReadLease::from_millis(
            lease.id(),
            lease.pinned_seq(),
            lease.issued_at(),
            lease.max_age_ms(),
        ));
    }

    /// Releases one lease; returns whether it was still registered.
    pub fn release(&self, lease_id: u64) -> bool {
        self.watchdog.release(lease_id)
    }

    /// Atomically renews one live lease at the same identity and pinned seq.
    pub(crate) fn renew(&self, lease: ReaderLease, now: Ts) -> ReaderLeaseRenewal {
        match self
            .watchdog
            .renew_lease_at(lease.id(), lease.pinned_seq(), now)
        {
            ReadLeaseRenewal::Renewed(renewed) => ReaderLeaseRenewal::Renewed(ReaderLease::new(
                renewed.reader_id,
                renewed.seq,
                renewed.created_at,
                renewed.lease_duration_ms,
            )),
            ReadLeaseRenewal::Missing => ReaderLeaseRenewal::Missing,
            ReadLeaseRenewal::Expired => ReaderLeaseRenewal::Expired,
            ReadLeaseRenewal::PinnedSeqMismatch {
                registered,
                requested,
            } => ReaderLeaseRenewal::PinnedSeqMismatch {
                registered,
                requested,
            },
        }
    }

    /// Aborts one lease if the caller observed it expired at `now`.
    pub fn abort_if_expired(&self, lease: ReaderLease, now: Ts) -> bool {
        let probe = ReadLease::from_millis(
            lease.id(),
            lease.pinned_seq(),
            lease.issued_at(),
            lease.max_age_ms(),
        );
        probe.is_expired_at(now) && self.watchdog.abort_if_expired_at(lease.id(), now)
    }

    /// Background tick hook: abort all expired leases at `now`.
    pub fn check_and_abort_expired(&self, now: Ts) -> Vec<ReaderId> {
        self.watchdog.check_and_abort_expired_at(now)
    }

    /// Checks the live oldest-pinned gap at `newest_seq`.
    pub fn check_gap(&self, newest_seq: Seq, now: Ts, max_gap_seqs: u64) -> Option<GapAlert> {
        self.watchdog
            .check_gap_at_with_max(newest_seq, now, max_gap_seqs)
    }

    /// Returns the watchdog metrics after refreshing expiry state.
    pub fn metrics(&self, newest_seq: Seq, now: Ts) -> SnapshotPinMetrics {
        self.watchdog.metrics_at(newest_seq, now)
    }

    /// Returns the live view at `now`, pruning expired leases first.
    pub fn live_view(&self, now: Ts) -> LeaseView {
        self.watchdog.check_and_abort_expired_at(now);
        LeaseView {
            active_leases: self.watchdog.lease_count(),
            oldest_pinned_seq: self.watchdog.oldest_pinned_seq_at(now),
            reader_lease_expired_total: self.watchdog.reader_lease_expired_total(),
        }
    }
}

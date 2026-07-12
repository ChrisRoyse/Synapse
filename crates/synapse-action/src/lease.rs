//! Process-global, renewable, TTL-bounded **input lease** for the real
//! foreground/cursor/keyboard/clipboard (epic #719).
//!
//! Synapse runs one daemon serving many MCP sessions (one per agent terminal).
//! Background action tiers (CDP, UIA control patterns, `PostMessage`) touch no
//! shared OS input state and run in parallel without ever taking this lease.
//! Only a *leased foreground* action — a real `SendInput`/`SetPhysicalCursorPos`
//! that steals the single physical cursor/foreground — must own the lease.
//!
//! Semantics are **refuse, not block**: a contended `try_acquire` returns
//! [`LeaseOutcome::Busy`] with the current holder and a retry hint rather than
//! waiting, so an agent never deadlocks on another agent's foreground action.
//! The lease auto-expires lazily on TTL lapse, but a lapsed owner leaves a
//! pending cleanup record that blocks new acquisition until that session's
//! held-input ledger is drained. Session disconnect releases inputs and lease
//! through the same cleanup path, so a crashed agent cannot leave foreground
//! input stuck behind an unowned lease.
//!
//! The lock-free-at-rest static mirrors the [`crate::hotkey`] module's
//! process-global state pattern. The guard is a plain `std::sync::Mutex` held
//! only for the O(1) critical section — never across an `.await` or an action
//! emit — so [`status`] never blocks a health probe.

use std::{
    cell::Cell,
    collections::BTreeMap,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use synapse_core::error_codes;

/// Default lease lifetime when a caller does not specify one.
pub const DEFAULT_LEASE_TTL_MS: u64 = 5_000;
/// Minimum acceptable lease lifetime (clamped by [`ttl_from_ms`]).
pub const MIN_LEASE_TTL_MS: u64 = 100;
/// Maximum acceptable lease lifetime (clamped by [`ttl_from_ms`]).
pub const MAX_LEASE_TTL_MS: u64 = 30_000;
/// Synthetic holder used when the operator panic hotkey preempts agents.
pub const OPERATOR_LEASE_OWNER_SESSION_ID: &str = "__operator__";
/// How long the operator owns the real-input resource after panic preemption.
pub const OPERATOR_PREEMPT_LEASE_TTL_MS: u64 = MAX_LEASE_TTL_MS;

/// Internal lease record. Stored behind the process-global mutex.
#[derive(Clone, Debug)]
struct InputLease {
    owner_session_id: String,
    acquired_at: Instant,
    renewed_at: Instant,
    ttl: Duration,
}

impl InputLease {
    fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.renewed_at) >= self.ttl
    }

    fn expires_in(&self, now: Instant) -> Duration {
        self.ttl.saturating_sub(now.duration_since(self.renewed_at))
    }

    fn status(&self, now: Instant) -> LeaseStatus {
        LeaseStatus {
            held: true,
            owner_session_id: Some(self.owner_session_id.clone()),
            acquired_at_ms_ago: Some(duration_ms(now.duration_since(self.acquired_at))),
            renewed_at_ms_ago: Some(duration_ms(now.duration_since(self.renewed_at))),
            ttl_ms: Some(duration_ms(self.ttl)),
            expires_in_ms: Some(duration_ms(self.expires_in(now))),
        }
    }
}

/// Serializable snapshot of the lease, used by MCP tools and `/health`.
///
/// All time fields are milliseconds. When `held` is `false` every optional
/// field is `None`, so an unheld lease serializes to an unambiguous "nobody
/// owns the foreground" shape.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseStatus {
    pub held: bool,
    pub owner_session_id: Option<String>,
    pub acquired_at_ms_ago: Option<u64>,
    pub renewed_at_ms_ago: Option<u64>,
    pub ttl_ms: Option<u64>,
    pub expires_in_ms: Option<u64>,
}

impl LeaseStatus {
    /// The canonical "nobody holds the lease" snapshot.
    #[must_use]
    pub const fn unheld() -> Self {
        Self {
            held: false,
            owner_session_id: None,
            acquired_at_ms_ago: None,
            renewed_at_ms_ago: None,
            ttl_ms: None,
            expires_in_ms: None,
        }
    }
}

/// Result of [`try_acquire`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// The lease was free (or expired) and is now owned by the caller.
    Acquired(LeaseStatus),
    /// The caller already owned the lease; its TTL window was refreshed.
    Renewed(LeaseStatus),
    /// Another live session owns the lease. The caller is refused (not blocked).
    Busy {
        holder: LeaseStatus,
        retry_after_ms: u64,
    },
    /// A lapsed holder must have its per-session held-input ledger drained
    /// before any session can acquire the real-input lease.
    CleanupPending {
        expired: LeaseStatus,
        retry_after_ms: u64,
    },
}

/// Before/after snapshots from an atomic holder-to-peer handoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseHandoff {
    pub prior: LeaseStatus,
    pub current: LeaseStatus,
}

/// Error returned by [`renew`]/[`release`] when the caller is not the holder.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    #[error("input lease not held by session {session_id:?}; current holder {holder:?}")]
    NotHeld {
        session_id: String,
        holder: Option<String>,
    },
}

impl LeaseError {
    /// Stable error code for MCP surfacing.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotHeld { .. } => error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
        }
    }
}

/// Process-global daemon state for the input lease: the current holder slot and
/// the pending expired-cleanup ledger, bundled so a test can swap both together
/// behind a single thread-local override.
struct LeaseCell {
    slot: Mutex<Option<InputLease>>,
    expired_cleanup: Mutex<BTreeMap<String, LeaseStatus>>,
}

impl LeaseCell {
    const fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            expired_cleanup: Mutex::new(BTreeMap::new()),
        }
    }
}

/// The real process-global lease state. In production every caller resolves to
/// this single cell, so the lease behaves as one shared singleton exactly as
/// before this indirection was introduced.
static GLOBAL_CELL: LeaseCell = LeaseCell::new();

thread_local! {
    /// Per-thread override of [`GLOBAL_CELL`], installed only by
    /// [`isolate_for_test`]. Production never sets it, so [`current_cell`]
    /// always returns `&GLOBAL_CELL` and behavior is byte-for-byte unchanged.
    static CELL_OVERRIDE: Cell<Option<&'static LeaseCell>> = const { Cell::new(None) };
}

/// Resolves the lease cell for the current thread: the test override if one is
/// installed, otherwise the process-global cell.
fn current_cell() -> &'static LeaseCell {
    CELL_OVERRIDE.with(Cell::get).unwrap_or(&GLOBAL_CELL)
}

/// Installs a fresh, thread-local input-lease cell so a test's lease reads and
/// writes are hermetic.
///
/// A parallel test acquiring the process-global lease can then no longer inject
/// a phantom holder into this thread's [`status`] reads — the confirmed root
/// cause of the flaky `session_list` projection (issue #1574).
///
/// Idempotent per thread: the first call installs the isolated cell and later
/// calls reuse it, so multiple services built on one test thread still share a
/// single lease exactly as production does. The cell is intentionally leaked to
/// obtain a `'static` reference; the count is bounded by the number of tests
/// that opt in, and libtest gives each test a fresh thread (so no override
/// bleeds between tests).
#[cfg(feature = "test-support")]
pub fn isolate_for_test() {
    CELL_OVERRIDE.with(|override_cell| {
        if override_cell.get().is_none() {
            override_cell.set(Some(Box::leak(Box::new(LeaseCell::new()))));
        }
    });
}

/// Locks the lease slot, recovering from a poisoned mutex rather than panicking:
/// a foreground lease that panicked mid-action must still be reclaimable.
fn lock() -> MutexGuard<'static, Option<InputLease>> {
    current_cell()
        .slot
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn lock_expired_cleanup() -> MutexGuard<'static, BTreeMap<String, LeaseStatus>> {
    current_cell()
        .expired_cleanup
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Clamps a caller-supplied TTL in milliseconds into the accepted range.
#[must_use]
pub fn ttl_from_ms(ms: u64) -> Duration {
    Duration::from_millis(ms.clamp(MIN_LEASE_TTL_MS, MAX_LEASE_TTL_MS))
}

/// Drops the stored lease if its TTL has lapsed and records the expired owner
/// until its held-input ledger has been drained. Caller holds the lease lock.
fn expire_if_lapsed(guard: &mut Option<InputLease>, now: Instant) -> Option<LeaseStatus> {
    let expired = guard
        .as_ref()
        .filter(|lease| lease.is_expired(now))
        .map(|lease| lease.status(now));
    if let Some(status) = expired.clone() {
        *guard = None;
        remember_expired_cleanup(&status);
    }
    expired
}

fn remember_expired_cleanup(status: &LeaseStatus) {
    let Some(owner_session_id) = status.owner_session_id.clone() else {
        return;
    };
    if owner_session_id == OPERATOR_LEASE_OWNER_SESSION_ID {
        return;
    }
    {
        let mut pending = lock_expired_cleanup();
        pending
            .entry(owner_session_id.clone())
            .or_insert_with(|| status.clone());
    }
    tracing::warn!(
        code = error_codes::ACTION_FOREGROUND_LEASE_BUSY,
        owner_session_id,
        ttl_ms = status.ttl_ms,
        acquired_at_ms_ago = status.acquired_at_ms_ago,
        renewed_at_ms_ago = status.renewed_at_ms_ago,
        "readback=input_lease edge=expired pending_session_input_cleanup"
    );
}

fn first_pending_expired_cleanup() -> Option<LeaseStatus> {
    lock_expired_cleanup().values().next().cloned()
}

/// Reads expired lease owners whose held-input ledgers must be drained before
/// the foreground lease can be granted again.
#[must_use]
pub fn expired_cleanup_snapshot() -> Vec<LeaseStatus> {
    lock_expired_cleanup().values().cloned().collect()
}

/// Marks a previously expired owner's held-input cleanup as complete.
///
/// Returns `true` when a pending expired owner record was removed.
#[must_use]
pub fn complete_expired_cleanup(session_id: &str) -> bool {
    lock_expired_cleanup().remove(session_id).is_some()
}

/// Attempts to acquire (or renew) the lease for `session_id`.
///
/// Refuse-not-block: if another live session holds it, returns
/// [`LeaseOutcome::Busy`] immediately with the holder and a retry hint.
#[must_use]
pub fn try_acquire(session_id: &str, ttl: Duration) -> LeaseOutcome {
    let now = Instant::now();
    let mut guard = lock();
    if let Some(expired) = expire_if_lapsed(&mut guard, now) {
        return LeaseOutcome::CleanupPending {
            expired,
            retry_after_ms: 100,
        };
    }
    if let Some(expired) = first_pending_expired_cleanup() {
        return LeaseOutcome::CleanupPending {
            expired,
            retry_after_ms: 100,
        };
    }
    let outcome = match guard.as_mut() {
        Some(lease) if lease.owner_session_id == session_id => {
            lease.renewed_at = now;
            lease.ttl = ttl;
            LeaseOutcome::Renewed(lease.status(now))
        }
        Some(lease) => {
            let holder = lease.status(now);
            let retry_after_ms = duration_ms(lease.expires_in(now));
            LeaseOutcome::Busy {
                holder,
                retry_after_ms,
            }
        }
        None => {
            let lease = InputLease {
                owner_session_id: session_id.to_owned(),
                acquired_at: now,
                renewed_at: now,
                ttl,
            };
            let status = lease.status(now);
            *guard = Some(lease);
            LeaseOutcome::Acquired(status)
        }
    };
    drop(guard);
    outcome
}

/// Refreshes the TTL window for the holder. Errors if `session_id` is not the holder.
///
/// # Errors
///
/// Returns [`LeaseError::NotHeld`] when the lease is unheld, expired, or owned
/// by a different session.
pub fn renew(session_id: &str, ttl: Option<Duration>) -> Result<LeaseStatus, LeaseError> {
    let now = Instant::now();
    let mut guard = lock();
    let _expired = expire_if_lapsed(&mut guard, now);
    match guard.as_mut() {
        Some(lease) if lease.owner_session_id == session_id => {
            lease.renewed_at = now;
            if let Some(ttl) = ttl {
                lease.ttl = ttl;
            }
            Ok(lease.status(now))
        }
        other => Err(LeaseError::NotHeld {
            session_id: session_id.to_owned(),
            holder: other.map(|lease| lease.owner_session_id.clone()),
        }),
    }
}

/// Releases the lease on behalf of its holder. Errors if `session_id` is not the holder.
///
/// # Errors
///
/// Returns [`LeaseError::NotHeld`] when the caller does not currently hold the lease.
pub fn release(session_id: &str) -> Result<LeaseStatus, LeaseError> {
    let now = Instant::now();
    let mut guard = lock();
    let _expired = expire_if_lapsed(&mut guard, now);
    let result = match guard.as_ref() {
        Some(lease) if lease.owner_session_id == session_id => {
            *guard = None;
            Ok(LeaseStatus::unheld())
        }
        other => Err(LeaseError::NotHeld {
            session_id: session_id.to_owned(),
            holder: other.map(|lease| lease.owner_session_id.clone()),
        }),
    };
    drop(guard);
    result
}

/// Transfers the currently held lease from `from_session_id` directly to
/// `to_session_id` without releasing it into the free pool.
///
/// # Errors
///
/// Returns [`LeaseError::NotHeld`] when `from_session_id` does not currently
/// hold the lease. Lapsed holders are expired before the check, so a handoff
/// cannot revive an expired lease without the normal held-input cleanup path.
pub fn handoff(
    from_session_id: &str,
    to_session_id: &str,
    ttl: Duration,
) -> Result<LeaseHandoff, LeaseError> {
    let now = Instant::now();
    let mut guard = lock();
    let _expired = expire_if_lapsed(&mut guard, now);
    match guard.as_mut() {
        Some(lease) if lease.owner_session_id == from_session_id => {
            let prior = lease.status(now);
            to_session_id.clone_into(&mut lease.owner_session_id);
            lease.acquired_at = now;
            lease.renewed_at = now;
            lease.ttl = ttl;
            let current = lease.status(now);
            Ok(LeaseHandoff { prior, current })
        }
        other => Err(LeaseError::NotHeld {
            session_id: from_session_id.to_owned(),
            holder: other.map(|lease| lease.owner_session_id.clone()),
        }),
    }
}

/// Releases the lease iff `session_id` currently holds it. Infallible; for
/// session-disconnect/expiry cleanup where a non-owner call must be a no-op.
///
/// Returns `true` when a lease owned by `session_id` was released.
#[must_use]
pub fn release_if_owner(session_id: &str) -> bool {
    let mut guard = lock();
    let released = guard
        .as_ref()
        .is_some_and(|lease| lease.owner_session_id == session_id);
    if released {
        *guard = None;
    }
    drop(guard);
    released
}

/// Operator override: transfers the lease to the operator (e.g. panic hotkey).
///
/// Returns the prior holder's snapshot, if any, so the preemption can be logged.
pub fn force_preempt(reason: &str) -> Option<LeaseStatus> {
    let now = Instant::now();
    let mut guard = lock();
    let prior = guard.as_ref().map(|lease| lease.status(now));
    if let Some(prior) = &prior {
        tracing::warn!(
            code = error_codes::ACTION_FOREGROUND_LEASE_BUSY,
            reason,
            prior_owner = ?prior.owner_session_id,
            "input lease force-preempted by operator"
        );
    }
    *guard = Some(InputLease {
        owner_session_id: OPERATOR_LEASE_OWNER_SESSION_ID.to_owned(),
        acquired_at: now,
        renewed_at: now,
        ttl: ttl_from_ms(OPERATOR_PREEMPT_LEASE_TTL_MS),
    });
    prior
}

/// Unconditionally clears the process-global lease.
///
/// This is intentionally separate from [`force_preempt`]: operator preemption
/// must leave a visible, bounded operator holder so agents fail closed instead
/// of immediately reacquiring the foreground resource.
pub fn force_clear(reason: &str) -> Option<LeaseStatus> {
    let now = Instant::now();
    let mut guard = lock();
    let prior = guard.as_ref().map(|lease| lease.status(now));
    *guard = None;
    drop(guard);
    lock_expired_cleanup().clear();
    if let Some(prior) = &prior {
        tracing::info!(
            reason,
            prior_owner = ?prior.owner_session_id,
            "input lease force-cleared"
        );
    }
    prior
}

/// Clears the lease only when the named session is still the live owner.
///
/// This operator-control primitive is race-safe against clearing a lease that
/// moved to a different session after the UI read its before state.
pub fn force_clear_if_owner(session_id: &str, reason: &str) -> Option<LeaseStatus> {
    let now = Instant::now();
    let mut guard = lock();
    let _expired = expire_if_lapsed(&mut guard, now);
    let prior = guard
        .as_ref()
        .filter(|lease| lease.owner_session_id == session_id)
        .map(|lease| lease.status(now));
    if prior.is_some() {
        *guard = None;
    }
    drop(guard);
    if let Some(prior) = &prior {
        tracing::warn!(
            reason,
            prior_owner = ?prior.owner_session_id,
            "input lease force-cleared by owner-guarded operator override"
        );
        lock_expired_cleanup().clear();
    }
    prior
}

/// Reads the current lease snapshot, lazily expiring a lapsed lease first.
///
/// Never blocks beyond the O(1) critical section; safe for `/health`.
#[must_use]
pub fn status() -> LeaseStatus {
    let now = Instant::now();
    let mut guard = lock();
    let _expired = expire_if_lapsed(&mut guard, now);
    guard
        .as_ref()
        .map_or_else(LeaseStatus::unheld, |lease| lease.status(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Mutex, MutexGuard, PoisonError};

    // The lease is process-global, so these tests must not run concurrently.
    // Serialize them on a module-local mutex; the guard is held for the whole
    // test and resets the lease on entry so no test observes another's holder.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let _prior = force_clear("test_reset");
        guard
    }

    fn reset() {
        let _prior = force_clear("test_reset");
    }

    #[test]
    fn acquire_then_status_reports_holder() {
        let _serial = serial();
        let session = "fsv-acquire";
        let outcome = try_acquire(session, ttl_from_ms(5_000));
        assert!(matches!(outcome, LeaseOutcome::Acquired(_)));
        let status = status();
        assert!(status.held);
        assert_eq!(status.owner_session_id.as_deref(), Some(session));
        reset();
    }

    #[test]
    fn same_session_renews_not_busy() {
        let _serial = serial();
        let session = "fsv-renew";
        let _first = try_acquire(session, ttl_from_ms(5_000));
        let second = try_acquire(session, ttl_from_ms(5_000));
        assert!(matches!(second, LeaseOutcome::Renewed(_)));
        let renewed = renew(session, None);
        assert!(renewed.is_ok());
        reset();
    }

    #[test]
    fn contended_acquire_returns_busy_with_holder() {
        let _serial = serial();
        let owner = "fsv-busy-owner";
        let contender = "fsv-busy-contender";
        let _held = try_acquire(owner, ttl_from_ms(5_000));
        match try_acquire(contender, ttl_from_ms(5_000)) {
            LeaseOutcome::Busy { holder, .. } => {
                assert_eq!(holder.owner_session_id.as_deref(), Some(owner));
            }
            other => panic!("expected Busy, got {other:?}"),
        }
        reset();
    }

    #[test]
    fn owner_release_frees_lease_for_others() {
        let _serial = serial();
        let owner = "fsv-rel-owner";
        let next = "fsv-rel-next";
        let _held = try_acquire(owner, ttl_from_ms(5_000));
        assert!(release(owner).is_ok());
        assert!(!status().held);
        assert!(matches!(
            try_acquire(next, ttl_from_ms(5_000)),
            LeaseOutcome::Acquired(_)
        ));
        reset();
    }

    #[test]
    fn handoff_transfers_without_unheld_gap() {
        let _serial = serial();
        let owner = "fsv-handoff-owner";
        let recipient = "fsv-handoff-recipient";
        let _held = try_acquire(owner, ttl_from_ms(5_000));

        let handoff = handoff(owner, recipient, ttl_from_ms(7_000)).unwrap();
        assert_eq!(handoff.prior.owner_session_id.as_deref(), Some(owner));
        assert_eq!(handoff.current.owner_session_id.as_deref(), Some(recipient));
        assert_eq!(handoff.current.ttl_ms, Some(7_000));

        let after = status();
        assert!(after.held);
        assert_eq!(after.owner_session_id.as_deref(), Some(recipient));
        match try_acquire(owner, ttl_from_ms(5_000)) {
            LeaseOutcome::Busy { holder, .. } => {
                assert_eq!(holder.owner_session_id.as_deref(), Some(recipient));
            }
            other => panic!("expected prior owner to be busy after handoff, got {other:?}"),
        }
        println!(
            "readback=input_lease edge=handoff owner_before={:?} owner_after={:?}",
            handoff.prior.owner_session_id, after.owner_session_id
        );
        reset();
    }

    #[test]
    fn handoff_requires_current_owner() {
        let _serial = serial();
        let owner = "fsv-handoff-owner";
        let intruder = "fsv-handoff-intruder";
        let recipient = "fsv-handoff-recipient";
        let _held = try_acquire(owner, ttl_from_ms(5_000));

        assert!(matches!(
            handoff(intruder, recipient, ttl_from_ms(5_000)),
            Err(LeaseError::NotHeld { .. })
        ));
        let after = status();
        assert_eq!(after.owner_session_id.as_deref(), Some(owner));
        reset();
    }

    #[test]
    fn non_owner_release_and_renew_error() {
        let _serial = serial();
        let owner = "fsv-nonowner-owner";
        let intruder = "fsv-nonowner-intruder";
        let _held = try_acquire(owner, ttl_from_ms(5_000));
        assert!(matches!(release(intruder), Err(LeaseError::NotHeld { .. })));
        assert!(matches!(
            renew(intruder, None),
            Err(LeaseError::NotHeld { .. })
        ));
        // owner still holds it
        assert_eq!(status().owner_session_id.as_deref(), Some(owner));
        reset();
    }

    #[test]
    fn ttl_lapse_auto_releases() {
        let _serial = serial();
        let owner = "fsv-ttl-owner";
        let next = "fsv-ttl-next";
        let _held = try_acquire(owner, ttl_from_ms(MIN_LEASE_TTL_MS));
        std::thread::sleep(Duration::from_millis(MIN_LEASE_TTL_MS + 50));
        // Lazy expiry clears the holder but refuses a new owner until the
        // expired session's held-input ledger is drained.
        assert!(!status().held);
        let pending = expired_cleanup_snapshot();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].owner_session_id.as_deref(), Some(owner));
        match try_acquire(next, ttl_from_ms(5_000)) {
            LeaseOutcome::CleanupPending { expired, .. } => {
                assert_eq!(expired.owner_session_id.as_deref(), Some(owner));
            }
            other => panic!("expected cleanup pending, got {other:?}"),
        }
        assert!(complete_expired_cleanup(owner));
        assert!(matches!(
            try_acquire(next, ttl_from_ms(5_000)),
            LeaseOutcome::Acquired(_)
        ));
        reset();
    }

    #[test]
    fn release_if_owner_is_owner_scoped() {
        let _serial = serial();
        let owner = "fsv-rio-owner";
        let other = "fsv-rio-other";
        let _held = try_acquire(owner, ttl_from_ms(5_000));
        assert!(!release_if_owner(other));
        assert!(status().held);
        assert!(release_if_owner(owner));
        assert!(!status().held);
        reset();
    }

    #[test]
    fn force_clear_if_owner_is_owner_guarded() {
        let _serial = serial();
        let owner = "force-clear-owner";
        let other = "force-clear-other";
        let _held = try_acquire(owner, ttl_from_ms(5_000));

        let denied = force_clear_if_owner(other, "test_other_owner");
        assert!(denied.is_none());
        assert_eq!(status().owner_session_id.as_deref(), Some(owner));

        let cleared = force_clear_if_owner(owner, "test_current_owner");
        assert_eq!(
            cleared
                .as_ref()
                .and_then(|status| status.owner_session_id.as_deref()),
            Some(owner)
        );
        assert!(!status().held);
        reset();
    }

    #[test]
    fn operator_preempt_transfers_lease_to_operator_holder() {
        let _serial = serial();
        let owner = "fsv-operator-owner";
        let _held = try_acquire(owner, ttl_from_ms(5_000));

        let prior = force_preempt("operator_preempt_test");
        assert_eq!(
            prior
                .as_ref()
                .and_then(|status| status.owner_session_id.as_deref()),
            Some(owner)
        );
        let after = status();
        assert!(after.held);
        assert_eq!(
            after.owner_session_id.as_deref(),
            Some(OPERATOR_LEASE_OWNER_SESSION_ID)
        );
        assert_eq!(after.ttl_ms, Some(OPERATOR_PREEMPT_LEASE_TTL_MS));
        println!(
            "readback=input_lease edge=operator_preempt owner_before={:?} owner_after={:?} ttl_after={:?}",
            prior.and_then(|status| status.owner_session_id),
            after.owner_session_id,
            after.ttl_ms
        );
        reset();
    }

    #[test]
    fn operator_preempt_refuses_prior_owner_until_operator_ttl_lapses() {
        let _serial = serial();
        let owner = "fsv-operator-prior";
        let _held = try_acquire(owner, ttl_from_ms(5_000));
        let _prior = force_preempt("operator_preempt_test");

        match try_acquire(owner, ttl_from_ms(5_000)) {
            LeaseOutcome::Busy { holder, .. } => {
                assert_eq!(
                    holder.owner_session_id.as_deref(),
                    Some(OPERATOR_LEASE_OWNER_SESSION_ID)
                );
            }
            other => {
                panic!("expected prior owner to be refused after operator preempt, got {other:?}")
            }
        }
        match release(owner) {
            Err(LeaseError::NotHeld { holder, .. }) => {
                assert_eq!(holder.as_deref(), Some(OPERATOR_LEASE_OWNER_SESSION_ID));
            }
            other => {
                panic!("expected prior owner release to fail after operator preempt, got {other:?}")
            }
        }
        reset();
    }

    #[test]
    fn ttl_is_clamped() {
        assert_eq!(ttl_from_ms(0), Duration::from_millis(MIN_LEASE_TTL_MS));
        assert_eq!(
            ttl_from_ms(10_000_000),
            Duration::from_millis(MAX_LEASE_TTL_MS)
        );
    }
}

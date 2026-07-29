//! Release-observable enforcement of the Calyx hot-path boundary (#1686).
//!
//! # Why this module exists
//!
//! `synapse_calyx::lowering::hot_context` already provides the boundary: a
//! thread-local hot tag ([`enter_hot_context`]) and an assertion
//! ([`assert_cold_calyx`]) that every Calyx intelligence entry point calls. Two
//! things made that machinery unable to prove anything:
//!
//! 1. `enter_hot_context()` had **no callers anywhere in the repository**, so
//!    the tag was never set and the twelve assertion sites could not fire even
//!    in principle. This module is the caller.
//! 2. The assertion's hard check is a `debug_assert!`. `debug_assert!`
//!    statements "are only enabled in non optimized builds by default"
//!    (`std::debug_assert!`), so the shipping release daemon executes only the
//!    `tracing::error!` edge and nothing durable survives to be read back. An
//!    acceptance claim resting on a release run would therefore be worthless.
//!
//! The fix here is deliberately *not* to panic in production. It is an
//! **always-on counter plus a structured log**, maintained identically in debug
//! and release, that `health` surfaces as
//! `subsystems.calyx_hot_path.violations_total`. A genuinely cold tick reads
//! zero; a violated boundary reads non-zero and names the offending operation,
//! and neither outcome requires attaching a debugger to observe.
//!
//! # Guard lifetime
//!
//! [`enter_hot_tick_thread`] returns an RAII scope. Dropping it un-tags the
//! thread, so a scope that ends early silently disables every downstream
//! assertion — the same class of bug the `let_underscore_drop` /
//! `let_underscore_lock` lints exist to catch. [`run_hot_tick_thread`] removes
//! the hazard structurally rather than by convention: the guard is bound in a
//! frame whose *only* statement is the call to the thread body, so no `return`,
//! `?`, or `break` inside that body can drop it early.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use synapse_calyx::hot_context::{self, HotContextScope};

/// Structured code emitted, and counted, when a live Calyx operation is
/// attempted from a thread tagged as a latency-critical hot context.
pub const HOT_PATH_BOUNDARY_VIOLATION_CODE: &str = "SYNAPSE_CALYX_HOT_PATH_BOUNDARY_VIOLATION";

/// Always-on violation counter. Not `cfg(debug_assertions)`-gated: this is the
/// number a release acceptance run reads back.
static HOT_PATH_BOUNDARY_VIOLATIONS: AtomicU64 = AtomicU64::new(0);
/// Ticks executed while the scheduler thread carried the hot tag.
static HOT_TICKS: AtomicU64 = AtomicU64::new(0);
/// Live count of threads currently carrying the reflex hot tag.
static HOT_THREADS: AtomicU64 = AtomicU64::new(0);

static LAST_VIOLATION: Mutex<Option<HotPathViolation>> = Mutex::new(None);

/// Identity of the most recent boundary violation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HotPathViolation {
    /// Operation that attempted live Calyx work from a hot context.
    pub operation: String,
    /// Wall-clock time the violation was recorded.
    pub at_unix_ms: u64,
}

/// RAII scope tagging the current thread as a latency-critical hot context.
///
/// Holding this composes two tags: the `synapse-calyx` thread-local (so the
/// twelve in-crate `assert_cold_calyx` sites can fire) and the reflex-side
/// accounting used by [`guard_cold`].
#[derive(Debug)]
pub struct HotTickThreadScope {
    _calyx: HotContextScope,
}

impl Drop for HotTickThreadScope {
    fn drop(&mut self) {
        HOT_THREADS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Tags the current thread hot for the returned scope's lifetime.
///
/// Prefer [`run_hot_tick_thread`], which makes an early drop structurally
/// impossible. Use this directly only where the whole thread body is a single
/// expression.
#[must_use]
pub fn enter_hot_tick_thread() -> HotTickThreadScope {
    HOT_THREADS.fetch_add(1, Ordering::Relaxed);
    HotTickThreadScope {
        _calyx: hot_context::enter_hot_context(),
    }
}

/// Runs `body` with the current thread tagged as a hot context for its entire
/// execution.
///
/// The guard is bound in this frame and this frame does nothing else, so an
/// early `return` — or a `?` — anywhere inside `body` cannot un-tag the thread
/// while the thread is still ticking. That is the whole point of the wrapper:
/// the invariant is enforced by the shape of the code, not by remembering to
/// keep the binding alive.
pub fn run_hot_tick_thread<T>(body: impl FnOnce() -> T) -> T {
    let _hot_tick_thread = enter_hot_tick_thread();
    body()
}

/// Whether the calling thread is currently tagged as a hot context.
#[must_use]
pub fn in_hot_context() -> bool {
    hot_context::in_hot_context()
}

/// Whether any thread currently carries the reflex hot tag.
#[must_use]
pub fn tick_thread_tagged() -> bool {
    HOT_THREADS.load(Ordering::Relaxed) > 0
}

/// Records that one tick executed under the hot tag.
pub(crate) fn record_hot_tick() {
    if in_hot_context() {
        HOT_TICKS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Ticks observed under the hot tag.
#[must_use]
pub fn hot_ticks_total() -> u64 {
    HOT_TICKS.load(Ordering::Relaxed)
}

/// Always-on count of hot-path boundary violations. Zero on a genuinely cold
/// tick, in debug and release alike.
#[must_use]
pub fn violations_total() -> u64 {
    HOT_PATH_BOUNDARY_VIOLATIONS.load(Ordering::Relaxed)
}

/// The most recent violation, when any has been recorded.
#[must_use]
pub fn last_violation() -> Option<HotPathViolation> {
    match LAST_VIOLATION.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Enforces the hot-path boundary for one operation that would perform live
/// Calyx or vault work.
///
/// A no-op on a cold thread. On a hot thread it increments the always-on
/// counter, records the operation, and emits the structured error edge — then
/// **returns**. Production must not be taken down by an observability check;
/// the loud counter plus structured log that `health` exposes is the enforcement
/// surface. The hard stop is retained for debug builds only, where a violation
/// is a test failure rather than an outage.
pub fn guard_cold(operation: &'static str) {
    if !in_hot_context() {
        return;
    }
    HOT_PATH_BOUNDARY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    let at_unix_ms = unix_time_ms_now();
    let violation = HotPathViolation {
        operation: operation.to_owned(),
        at_unix_ms,
    };
    match LAST_VIOLATION.lock() {
        Ok(mut guard) => *guard = Some(violation),
        Err(poisoned) => *poisoned.into_inner() = Some(violation),
    }
    metrics::counter!(
        "reflex_hot_path_boundary_violations_total",
        "operation" => operation
    )
    .increment(1);
    tracing::error!(
        code = HOT_PATH_BOUNDARY_VIOLATION_CODE,
        operation,
        at_unix_ms,
        violations_total = HOT_PATH_BOUNDARY_VIOLATIONS.load(Ordering::Relaxed),
        "a live Calyx/vault operation was attempted from the reflex hot path; hot paths must \
         consume only lowered frozen artifacts. Move the work off-tick (storage maintenance or \
         the audit offload writer) and read the lowered artifact instead."
    );
    debug_assert!(
        false,
        "{HOT_PATH_BOUNDARY_VIOLATION_CODE}: live Calyx/vault operation `{operation}` attempted \
         from the reflex hot path; consume a lowered artifact instead"
    );
}

pub(crate) fn unix_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

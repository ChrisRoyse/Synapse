use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use serde::Serialize;
use synapse_action::{
    ActionError, OperatorHotkeyGuard, OperatorHotkeyShutdownReport, OperatorHotkeyStatus,
    RELEASE_ALL_HANDLE, set_operator_hotkey_status,
};
use synapse_core::error_codes;
use tokio::runtime::Handle;

use crate::m3::SharedM3State;
use crate::server::SynapseService;

pub const DISABLE_OPERATOR_HOTKEY_ENV: &str = "SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY";
/// When set truthy, a failure to register the operator panic hotkey aborts
/// startup instead of degrading. Defaults to true because the MCP daemon exposes
/// input-emitting tools; set this false only with an explicit operator decision
/// to run degraded, or set `SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY=1` to skip
/// registration intentionally.
pub const REQUIRE_OPERATOR_HOTKEY_ENV: &str = "SYNAPSE_MCP_REQUIRE_OPERATOR_HOTKEY";

/// Operator-facing remediation for an unavailable panic hotkey.
const OPERATOR_HOTKEY_REMEDIATION: &str = "the daemon-owned operator hotkey could not be armed; stop duplicate synapse-mcp instances or conflicting hook owners, set SYNAPSE_OPERATOR_HOTKEY / SYNAPSE_MCP_OPERATOR_HOTKEY to another Ctrl+Alt+Shift+<A-Z|0-9> chord, set SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY=1 to run intentionally without it, or set SYNAPSE_MCP_REQUIRE_OPERATOR_HOTKEY=0 only for an explicit degraded run";
const OPERATOR_RELEASE_ALL_TIMEOUT: Duration = Duration::from_millis(50);
const OPERATOR_HOTKEY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const OPERATOR_PANIC_K2_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATOR_PANIC_K2_ABORT_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct OperatorPanicK2TaskCompletion {
    generation: u64,
    failure: Option<String>,
}

#[derive(Debug)]
struct OperatorPanicK2TaskOwner {
    task_id: u64,
    generation: u64,
    task: tokio::task::JoinHandle<OperatorPanicK2TaskCompletion>,
}

#[derive(Debug, Default)]
struct OperatorPanicK2TaskTracker {
    next_task_id: u64,
    admission_closed: bool,
    pending_spawn_task_ids: BTreeSet<u64>,
    task_owners: Vec<OperatorPanicK2TaskOwner>,
    reservations_after_admission_close: usize,
    tracking_failures: Vec<String>,
    lock_poison_observed: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorPanicK2TaskOwnerReadback {
    pub(crate) admission_closed: bool,
    pub(crate) pending_spawn_task_ids: Vec<u64>,
    pub(crate) task_owners: Vec<OperatorPanicK2TaskOwnerState>,
    pub(crate) reservations_after_admission_close: usize,
    pub(crate) tracking_failures: Vec<String>,
    pub(crate) lock_poison_observed: bool,
}

impl OperatorPanicK2TaskOwnerReadback {
    #[must_use]
    pub(crate) fn owners_quiescent(&self) -> bool {
        self.admission_closed
            && self.pending_spawn_task_ids.is_empty()
            && self.task_owners.is_empty()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorPanicK2TaskOwnerState {
    pub(crate) task_id: u64,
    pub(crate) generation: u64,
    pub(crate) terminal: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorPanicK2TaskDrainReport {
    pub(crate) reason: &'static str,
    pub(crate) hotkey_spawn_source_quiescent: bool,
    pub(crate) owners_before: OperatorPanicK2TaskOwnerReadback,
    pub(crate) panic_safety_before: synapse_action::OperatorPanicSafetyReadback,
    pub(crate) tasks_observed: usize,
    pub(crate) graceful_joined: usize,
    pub(crate) abort_requests_sent: usize,
    pub(crate) joined_after_abort: usize,
    pub(crate) task_successes: usize,
    pub(crate) retained_task_owner_ids: Vec<u64>,
    pub(crate) owners_after: OperatorPanicK2TaskOwnerReadback,
    pub(crate) panic_safety_after: synapse_action::OperatorPanicSafetyReadback,
    pub(crate) elapsed_ms: u128,
    pub(crate) failures: Vec<String>,
}

impl OperatorPanicK2TaskDrainReport {
    #[must_use]
    pub(crate) fn owners_quiescent(&self) -> bool {
        self.hotkey_spawn_source_quiescent
            && self.owners_after.owners_quiescent()
            && !self.panic_safety_after.pending
    }

    pub(crate) fn verdict(&self) -> anyhow::Result<()> {
        let accounted_tasks =
            self.graceful_joined + self.joined_after_abort + self.retained_task_owner_ids.len();
        let accounted_aborts = self.joined_after_abort + self.retained_task_owner_ids.len();
        let retained_after = self
            .owners_after
            .task_owners
            .iter()
            .map(|owner| owner.task_id)
            .collect::<BTreeSet<_>>();
        let retained_reported = self
            .retained_task_owner_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let terminal_retained_after = self
            .owners_after
            .task_owners
            .iter()
            .filter(|owner| owner.terminal)
            .count();
        let owners_before_count = self.owners_before.task_owners.len();
        if self.owners_quiescent()
            && !self.panic_safety_after.pending
            && self.failures.is_empty()
            && self.owners_after.tracking_failures.is_empty()
            && !self.owners_after.lock_poison_observed
            && self.owners_after.reservations_after_admission_close == 0
            && self.tasks_observed == accounted_tasks
            && self.abort_requests_sent == accounted_aborts
            && self.task_successes <= self.graceful_joined + self.joined_after_abort
            && self.tasks_observed >= owners_before_count
            && retained_after == retained_reported
        {
            Ok(())
        } else {
            anyhow::bail!(
                "operator panic K2 task drain failed for {} after {} ms (terminal retained owners={}): readback={self:?}",
                self.reason,
                self.elapsed_ms,
                terminal_retained_after
            )
        }
    }
}

static OPERATOR_PANIC_K2_TASK_TRACKER: OnceLock<Mutex<OperatorPanicK2TaskTracker>> =
    OnceLock::new();
static OPERATOR_PANIC_K2_TASK_TRACKER_NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisableReport {
    pub(crate) result: &'static str,
    pub(crate) disabled_ids: Vec<String>,
    pub(crate) error_code: Option<&'static str>,
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseAllReport {
    pub(crate) result: &'static str,
    pub(crate) error_code: Option<&'static str>,
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorHotkeyImmediateReport {
    pub hotkey: &'static str,
    pub operator_panic_generation: u64,
    pub lease_before: synapse_action::LeaseStatus,
    pub preempted_lease: Option<synapse_action::LeaseStatus>,
    pub lease_after_preempt: synapse_action::LeaseStatus,
    pub disable_report: DisableReport,
    pub release_all_report: ReleaseAllReport,
    pub durable_browser_mutation_owners_after_disable:
        synapse_a11y::CdpDurableBrowserMutationOwnersReadback,
    pub release_all_elapsed_ms: u128,
    pub elapsed_ms: u128,
    pub within_budget: bool,
    pub k1_safety_terminal: bool,
}

pub fn install_operator_hotkey(
    service: SynapseService,
) -> synapse_action::ActionResult<Option<OperatorHotkeyGuard>> {
    if operator_hotkey_disabled_by_env()? {
        tracing::warn!(
            code = "SAFETY_OPERATOR_HOTKEY_DISABLED",
            env = DISABLE_OPERATOR_HOTKEY_ENV,
            "operator hotkey disabled by explicit environment override"
        );
        set_operator_hotkey_status(OperatorHotkeyStatus::DisabledByEnv);
        return Ok(None);
    }
    let m3_state = service.m3_state_handle();
    let runtime = Handle::current();
    match synapse_action::install_operator_hotkey(move |operator_panic_token| {
        handle_operator_hotkey(&service, &m3_state, &runtime, operator_panic_token);
    }) {
        Ok(guard) => {
            set_operator_hotkey_status(OperatorHotkeyStatus::Registered);
            Ok(Some(guard))
        }
        Err(error) => {
            set_operator_hotkey_status(OperatorHotkeyStatus::Unavailable);
            let install_unwind_report = operator_hotkey_install_unwind_report();
            let install_unwind_retained_live_owner =
                operator_hotkey_install_unwind_retained_live_owner();
            tracing::error!(
                code = "MCP_OPERATOR_HOTKEY_INSTALL_FAILED",
                component = "operator_hotkey",
                install_unwind_retained_live_owner,
                install_unwind_report = ?install_unwind_report,
                error = %error,
                "operator hotkey installation failed after bounded owner cleanup"
            );
            if install_unwind_retained_live_owner {
                let detail = format!(
                    "{}; installation cleanup retained a live exact owner, so degraded startup is unsafe; {OPERATOR_HOTKEY_REMEDIATION}",
                    error.detail()
                );
                return Err(error.with_detail(detail));
            }
            if operator_hotkey_required_by_env()? {
                // Default strict mode: caller propagates and startup fails closed.
                let detail = format!("{}; {OPERATOR_HOTKEY_REMEDIATION}", error.detail());
                return Err(error.with_detail(detail));
            }
            // Explicit degraded mode: do NOT abort the whole MCP server because
            // the operator chose to run without a bound global kill-switch.
            // Log loudly with exact cause/remediation and record status for
            // /health so the risk is visible.
            tracing::error!(
                code = error_codes::ACTION_BACKEND_UNAVAILABLE,
                component = "operator_hotkey",
                hotkey = synapse_action::hotkey::DEFAULT_OPERATOR_HOTKEY,
                status = OperatorHotkeyStatus::Unavailable.label(),
                error = %error,
                remediation = OPERATOR_HOTKEY_REMEDIATION,
                require_env = REQUIRE_OPERATOR_HOTKEY_ENV,
                disable_env = DISABLE_OPERATOR_HOTKEY_ENV,
                "operator panic hotkey unavailable; continuing only because degraded hotkey mode was explicitly allowed"
            );
            Ok(None)
        }
    }
}

/// Checked shutdown for the exact operator-hotkey owner held by a transport.
/// A live owner remains in `guard` so the transport can retain it together with
/// daemon lifetime locks until OS teardown.
pub(crate) fn shutdown_operator_hotkey(
    guard: &mut Option<OperatorHotkeyGuard>,
    reason: &'static str,
) -> Option<OperatorHotkeyShutdownReport> {
    let report = guard
        .as_mut()
        .map(|guard| guard.shutdown_checked(OPERATOR_HOTKEY_SHUTDOWN_TIMEOUT, reason))?;
    tracing::info!(
        code = "MCP_OPERATOR_HOTKEY_SHUTDOWN_READBACK",
        reason,
        owners_quiescent = report.owners_quiescent(),
        report = ?report,
        "readback=operator_hotkey_thread_owners edge=daemon_shutdown after_checked_stop"
    );
    if report.owners_quiescent() {
        drop(guard.take());
    }
    Some(report)
}

/// Retains an unquiescent exact guard until process teardown. Call only after a
/// checked report proved a live owner and the transport chose to retain its
/// lifetime locks for the same reason.
pub(crate) fn retain_operator_hotkey_guard_to_process_exit(
    guard: &mut Option<OperatorHotkeyGuard>,
    reason: &'static str,
) {
    let Some(guard) = guard.take() else {
        return;
    };
    tracing::error!(
        code = "MCP_OPERATOR_HOTKEY_GUARD_RETAINED",
        reason,
        "transferring live operator-hotkey ownership to the action-layer process-lifetime registry"
    );
    drop(guard);
}

#[must_use]
pub(crate) fn operator_hotkey_install_unwind_report() -> Option<OperatorHotkeyShutdownReport> {
    synapse_action::hotkey::operator_hotkey_install_unwind_report()
}

#[must_use]
pub(crate) fn operator_hotkey_install_unwind_retained_live_owner() -> bool {
    synapse_action::hotkey::operator_hotkey_install_unwind_retained_live_owner()
}

fn operator_panic_k2_task_tracker() -> &'static Mutex<OperatorPanicK2TaskTracker> {
    OPERATOR_PANIC_K2_TASK_TRACKER.get_or_init(|| Mutex::new(OperatorPanicK2TaskTracker::default()))
}

fn operator_panic_k2_task_tracker_notify() -> &'static tokio::sync::Notify {
    OPERATOR_PANIC_K2_TASK_TRACKER_NOTIFY.get_or_init(tokio::sync::Notify::new)
}

fn lock_operator_panic_k2_task_tracker() -> MutexGuard<'static, OperatorPanicK2TaskTracker> {
    match operator_panic_k2_task_tracker().lock() {
        Ok(state) => state,
        Err(poisoned) => {
            let mut state = poisoned.into_inner();
            state.lock_poison_observed = true;
            tracing::error!(
                code = "MCP_OPERATOR_PANIC_K2_TASK_TRACKER_POISONED",
                "operator-panic K2 task tracker lock was poisoned; recovering exact owners but refusing a graceful verdict"
            );
            state
        }
    }
}

fn operator_panic_k2_task_owner_readback_from(
    state: &OperatorPanicK2TaskTracker,
) -> OperatorPanicK2TaskOwnerReadback {
    OperatorPanicK2TaskOwnerReadback {
        admission_closed: state.admission_closed,
        pending_spawn_task_ids: state.pending_spawn_task_ids.iter().copied().collect(),
        task_owners: state
            .task_owners
            .iter()
            .map(|owner| OperatorPanicK2TaskOwnerState {
                task_id: owner.task_id,
                generation: owner.generation,
                terminal: owner.task.is_finished(),
            })
            .collect(),
        reservations_after_admission_close: state.reservations_after_admission_close,
        tracking_failures: state.tracking_failures.clone(),
        lock_poison_observed: state.lock_poison_observed,
    }
}

/// Non-mutating process-global ownership readback for physical-hotkey K2 work.
/// A terminal task remains an owned, unjoined task until the drain API consumes
/// its exact `JoinHandle`; callers must not infer quiescence from `terminal`.
#[must_use]
pub(crate) fn operator_panic_k2_task_owner_readback() -> OperatorPanicK2TaskOwnerReadback {
    let state = lock_operator_panic_k2_task_tracker();
    operator_panic_k2_task_owner_readback_from(&state)
}

fn reserve_operator_panic_k2_task(generation: u64) -> u64 {
    let task_id = {
        let mut state = lock_operator_panic_k2_task_tracker();
        state.next_task_id += 1;
        let task_id = state.next_task_id;
        if state.admission_closed {
            state.reservations_after_admission_close += 1;
            tracing::error!(
                code = "MCP_OPERATOR_PANIC_K2_RESERVED_AFTER_CLOSE",
                task_id,
                operator_panic_generation = generation,
                "physical-hotkey callback reserved K2 work after shutdown admission closed"
            );
        }
        state.pending_spawn_task_ids.insert(task_id);
        task_id
    };
    operator_panic_k2_task_tracker_notify().notify_one();
    task_id
}

fn publish_operator_panic_k2_task(
    task_id: u64,
    generation: u64,
    task: tokio::task::JoinHandle<OperatorPanicK2TaskCompletion>,
) {
    {
        let mut state = lock_operator_panic_k2_task_tracker();
        if !state.pending_spawn_task_ids.remove(&task_id) {
            state.tracking_failures.push(format!(
                "task {task_id}: returned JoinHandle had no matching pending-spawn reservation"
            ));
        }
        // The exact owner enters the tracker before this function returns. It
        // is never dropped on the physical-hotkey callback thread.
        state.task_owners.push(OperatorPanicK2TaskOwner {
            task_id,
            generation,
            task,
        });
    }
    operator_panic_k2_task_tracker_notify().notify_one();
}

fn record_operator_panic_k2_spawn_failure(task_id: u64, generation: u64, detail: String) {
    {
        let mut state = lock_operator_panic_k2_task_tracker();
        state.pending_spawn_task_ids.remove(&task_id);
        state
            .tracking_failures
            .push(format!("task {task_id} generation {generation}: {detail}"));
    }
    operator_panic_k2_task_tracker_notify().notify_one();
}

enum OperatorPanicK2TaskWait {
    Joined {
        task_id: u64,
        result: Result<OperatorPanicK2TaskCompletion, tokio::task::JoinError>,
    },
    TimedOut(OperatorPanicK2TaskOwner),
}

async fn wait_for_operator_panic_k2_task_until(
    mut owner: OperatorPanicK2TaskOwner,
    deadline: tokio::time::Instant,
) -> OperatorPanicK2TaskWait {
    match tokio::time::timeout_at(deadline, &mut owner.task).await {
        Ok(result) => OperatorPanicK2TaskWait::Joined {
            task_id: owner.task_id,
            result,
        },
        Err(_elapsed) => OperatorPanicK2TaskWait::TimedOut(owner),
    }
}

fn record_operator_panic_k2_join(
    task_id: u64,
    result: Result<OperatorPanicK2TaskCompletion, tokio::task::JoinError>,
    task_successes: &mut usize,
    failures: &mut Vec<String>,
) {
    match result {
        Ok(OperatorPanicK2TaskCompletion {
            generation,
            failure: None,
        }) => {
            tracing::info!(
                task_id,
                operator_panic_generation = generation,
                "operator panic K2 task owner joined successfully"
            );
            *task_successes += 1;
        }
        Ok(OperatorPanicK2TaskCompletion {
            generation,
            failure: Some(failure),
        }) => failures.push(format!("task {task_id} generation {generation}: {failure}")),
        Err(error) => failures.push(format!("task {task_id}: join failed: {error}")),
    }
}

fn take_operator_panic_k2_task_owners() -> Vec<OperatorPanicK2TaskOwner> {
    let mut state = lock_operator_panic_k2_task_tracker();
    std::mem::take(&mut state.task_owners)
}

fn operator_panic_k2_pending_spawn_count() -> usize {
    lock_operator_panic_k2_task_tracker()
        .pending_spawn_task_ids
        .len()
}

/// Closes K2 spawn admission and boundedly consumes every exact task owner that
/// was registered by the physical-hotkey callback. Cleanup callers should run
/// this even when checked hotkey shutdown failed, but must pass that physical
/// readback as `hotkey_spawn_source_quiescent = false`; the report will then
/// refuse to call ownership quiescent because another callback can still reserve
/// work after the final tracker readback.
pub(crate) async fn drain_operator_panic_k2_tasks(
    reason: &'static str,
    hotkey_spawn_source_quiescent: bool,
) -> OperatorPanicK2TaskDrainReport {
    drain_operator_panic_k2_tasks_with_timeouts(
        reason,
        hotkey_spawn_source_quiescent,
        OPERATOR_PANIC_K2_STOP_TIMEOUT,
        OPERATOR_PANIC_K2_ABORT_JOIN_TIMEOUT,
    )
    .await
}

async fn drain_operator_panic_k2_tasks_with_timeouts(
    reason: &'static str,
    hotkey_spawn_source_quiescent: bool,
    stop_timeout: Duration,
    abort_join_timeout: Duration,
) -> OperatorPanicK2TaskDrainReport {
    let started = Instant::now();
    let panic_safety_before = synapse_action::operator_panic_safety_readback();
    let (owners_before, mut graceful_owners) = {
        let mut state = lock_operator_panic_k2_task_tracker();
        let owners_before = operator_panic_k2_task_owner_readback_from(&state);
        state.admission_closed = true;
        let owners = std::mem::take(&mut state.task_owners);
        (owners_before, owners)
    };

    let graceful_deadline = tokio::time::Instant::now() + stop_timeout;
    let mut tasks_observed = 0;
    let mut graceful_joined = 0;
    let mut task_successes = 0;
    let mut failures = Vec::new();
    let mut abort_owners = Vec::new();

    loop {
        if graceful_owners.is_empty() {
            let notified = operator_panic_k2_task_tracker_notify().notified();
            graceful_owners = take_operator_panic_k2_task_owners();
            if graceful_owners.is_empty() {
                if operator_panic_k2_pending_spawn_count() == 0
                    || tokio::time::Instant::now() >= graceful_deadline
                {
                    break;
                }
                if tokio::time::timeout_at(graceful_deadline, notified)
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
        }

        tasks_observed += graceful_owners.len();
        let outcomes = join_all(
            std::mem::take(&mut graceful_owners)
                .into_iter()
                .map(|owner| wait_for_operator_panic_k2_task_until(owner, graceful_deadline)),
        )
        .await;
        for outcome in outcomes {
            match outcome {
                OperatorPanicK2TaskWait::Joined { task_id, result } => {
                    graceful_joined += 1;
                    record_operator_panic_k2_join(
                        task_id,
                        result,
                        &mut task_successes,
                        &mut failures,
                    );
                }
                OperatorPanicK2TaskWait::TimedOut(owner) => abort_owners.push(owner),
            }
        }
        if tokio::time::Instant::now() >= graceful_deadline {
            break;
        }
    }

    let newly_published_owners = take_operator_panic_k2_task_owners();
    tasks_observed += newly_published_owners.len();
    abort_owners.extend(newly_published_owners);
    let abort_deadline = tokio::time::Instant::now() + abort_join_timeout;
    let mut abort_requests_sent = 0;
    let mut joined_after_abort = 0;
    let mut retained_owners = Vec::new();

    loop {
        if abort_owners.is_empty() {
            let notified = operator_panic_k2_task_tracker_notify().notified();
            abort_owners = take_operator_panic_k2_task_owners();
            if abort_owners.is_empty() {
                if operator_panic_k2_pending_spawn_count() == 0
                    || tokio::time::Instant::now() >= abort_deadline
                {
                    break;
                }
                if tokio::time::timeout_at(abort_deadline, notified)
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
            tasks_observed += abort_owners.len();
        }

        for owner in &abort_owners {
            owner.task.abort();
            abort_requests_sent += 1;
            failures.push(format!(
                "task {}: did not finish within {} ms; abort requested",
                owner.task_id,
                stop_timeout.as_millis()
            ));
        }
        let outcomes = join_all(
            std::mem::take(&mut abort_owners)
                .into_iter()
                .map(|owner| wait_for_operator_panic_k2_task_until(owner, abort_deadline)),
        )
        .await;
        for outcome in outcomes {
            match outcome {
                OperatorPanicK2TaskWait::Joined { task_id, result } => {
                    joined_after_abort += 1;
                    record_operator_panic_k2_join(
                        task_id,
                        result,
                        &mut task_successes,
                        &mut failures,
                    );
                }
                OperatorPanicK2TaskWait::TimedOut(owner) => retained_owners.push(owner),
            }
        }
        if tokio::time::Instant::now() >= abort_deadline {
            break;
        }
    }

    // A reservation can publish at the abort deadline. Request cancellation
    // immediately and retain its exact handle for a later drain rather than
    // detaching it merely because this bounded attempt exhausted its budget.
    let mut deadline_owners = take_operator_panic_k2_task_owners();
    tasks_observed += deadline_owners.len();
    for owner in &deadline_owners {
        owner.task.abort();
        abort_requests_sent += 1;
        failures.push(format!(
            "task {}: published at the K2 drain deadline; abort requested and exact owner retained",
            owner.task_id
        ));
    }
    retained_owners.append(&mut deadline_owners);

    let retained_task_owner_ids = retained_owners
        .iter()
        .map(|owner| owner.task_id)
        .collect::<Vec<_>>();
    let owners_after = {
        let mut state = lock_operator_panic_k2_task_tracker();
        state.task_owners.append(&mut retained_owners);
        operator_panic_k2_task_owner_readback_from(&state)
    };
    let report = OperatorPanicK2TaskDrainReport {
        reason,
        hotkey_spawn_source_quiescent,
        owners_before,
        panic_safety_before,
        tasks_observed,
        graceful_joined,
        abort_requests_sent,
        joined_after_abort,
        task_successes,
        retained_task_owner_ids,
        owners_after,
        panic_safety_after: synapse_action::operator_panic_safety_readback(),
        elapsed_ms: started.elapsed().as_millis(),
        failures,
    };
    tracing::info!(
        code = "MCP_OPERATOR_PANIC_K2_TASK_DRAIN_READBACK",
        reason,
        owners_quiescent = report.owners_quiescent(),
        verdict_ok = report.verdict().is_ok(),
        report = ?report,
        "readback=operator_panic_k2_task_owners edge=daemon_shutdown after_bounded_drain"
    );
    report
}

fn operator_hotkey_required_by_env() -> synapse_action::ActionResult<bool> {
    parse_bool_env(REQUIRE_OPERATOR_HOTKEY_ENV, true)
}

fn operator_hotkey_disabled_by_env() -> synapse_action::ActionResult<bool> {
    parse_bool_env(DISABLE_OPERATOR_HOTKEY_ENV, false)
}

fn parse_bool_env(name: &str, default: bool) -> synapse_action::ActionResult<bool> {
    let raw = std::env::var_os(name);
    parse_bool_value(
        name,
        raw.as_ref().map(|value| value.to_string_lossy()),
        default,
    )
}

fn parse_bool_value(
    name: &str,
    value: Option<std::borrow::Cow<'_, str>>,
    default: bool,
) -> synapse_action::ActionResult<bool> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ActionError::BackendUnavailable {
            detail: format!("{name} must be one of 1/true/yes/on or 0/false/no/off"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalizer_accepts_intermediate_newer_tag_when_publication_outpaces_k1() {
        assert!(operator_panic_tag_belongs_to_newer_published_wave(
            10, 11, 12
        ));
        assert!(operator_panic_tag_belongs_to_newer_published_wave(
            10, 12, 12
        ));
        assert!(!operator_panic_tag_belongs_to_newer_published_wave(
            10, 10, 12
        ));
        assert!(!operator_panic_tag_belongs_to_newer_published_wave(
            10, 9, 12
        ));
        assert!(!operator_panic_tag_belongs_to_newer_published_wave(
            10, 13, 12
        ));
    }

    #[test]
    fn finalization_postcondition_requires_coherent_lease_and_live_newer_owner() {
        let held_newer = synapse_action::LeaseSafetySnapshot {
            status: synapse_action::LeaseStatus {
                held: true,
                owner_session_id: Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID.to_owned()),
                acquired_at_ms_ago: Some(0),
                renewed_at_ms_ago: Some(0),
                ttl_ms: Some(synapse_action::OPERATOR_PREEMPT_LEASE_TTL_MS),
                expires_in_ms: Some(synapse_action::OPERATOR_PREEMPT_LEASE_TTL_MS),
            },
            operator_panic_generation: Some(11),
        };
        let mut safety = synapse_action::OperatorPanicSafetyReadback {
            epoch: 12,
            publications_in_flight: 0,
            outstanding_generations: 0,
            outstanding_finalizations: 2,
            accounting_incident: false,
            pending: true,
        };
        assert!(operator_panic_finalization_postcondition(
            10,
            &held_newer,
            &safety
        ));

        safety.outstanding_finalizations = 1;
        assert!(!operator_panic_finalization_postcondition(
            10,
            &held_newer,
            &safety
        ));
        let unheld = synapse_action::LeaseSafetySnapshot {
            status: synapse_action::LeaseStatus::unheld(),
            operator_panic_generation: None,
        };
        assert!(operator_panic_finalization_postcondition(
            10, &unheld, &safety
        ));
    }

    #[test]
    fn browser_owner_reset_overlap_accepts_fresh_newer_closed_generation_not_stale_enable() {
        let fresh_after_newer_k1 = synapse_a11y::CdpDurableBrowserMutationOwnersReadback {
            enabled: false,
            disable_sequence: 12,
            fetch_interception_active_count: 0,
            network_override_active_count: 0,
            dialog_auto_policy_active_count: 0,
            clock_active_count: 0,
            init_script_active_count: 0,
            unresolved_raw_cdp_evaluate_timeout_count: 0,
            unresolved_raw_cdp_input_owner_count: 0,
            persisted_cdp_mutation_owner_count: 0,
            persisted_cdp_input_owner_count: 0,
            persisted_cdp_evaluate_owner_count: 0,
            persisted_cdp_init_script_effect_owner_count: 0,
            registry_readback_failures: Vec::new(),
            registry_readback_healthy: true,
        };
        assert!(browser_owner_gate_closed_for_newer_wave(
            11,
            &fresh_after_newer_k1
        ));

        let mut same_generation = fresh_after_newer_k1.clone();
        same_generation.disable_sequence = 11;
        assert!(!browser_owner_gate_closed_for_newer_wave(
            11,
            &same_generation
        ));
        let mut stale_enabled = fresh_after_newer_k1.clone();
        stale_enabled.enabled = true;
        assert!(!browser_owner_gate_closed_for_newer_wave(
            11,
            &stale_enabled
        ));
        let mut unhealthy = fresh_after_newer_k1;
        unhealthy.registry_readback_healthy = false;
        unhealthy
            .registry_readback_failures
            .push("synthetic poisoned registry".to_owned());
        assert!(!browser_owner_gate_closed_for_newer_wave(11, &unhealthy));
    }

    #[test]
    fn operator_hotkey_required_defaults_to_fail_closed() {
        let required = parse_bool_value(REQUIRE_OPERATOR_HOTKEY_ENV, None, true)
            .expect("missing require env should parse");

        assert!(required);
    }

    #[test]
    fn operator_hotkey_required_can_be_explicitly_relaxed() {
        let required = parse_bool_value(
            REQUIRE_OPERATOR_HOTKEY_ENV,
            Some(std::borrow::Cow::Borrowed("0")),
            true,
        )
        .expect("false require env should parse");

        assert!(!required);
    }

    #[test]
    fn operator_hotkey_disabled_defaults_to_false() {
        let disabled = parse_bool_value(DISABLE_OPERATOR_HOTKEY_ENV, None, false)
            .expect("missing disable env should parse");

        assert!(!disabled);
    }

    fn synthetic_k2_owner_readback(
        admission_closed: bool,
        pending_spawn_task_ids: Vec<u64>,
        task_owners: Vec<OperatorPanicK2TaskOwnerState>,
    ) -> OperatorPanicK2TaskOwnerReadback {
        OperatorPanicK2TaskOwnerReadback {
            admission_closed,
            pending_spawn_task_ids,
            task_owners,
            reservations_after_admission_close: 0,
            tracking_failures: Vec::new(),
            lock_poison_observed: false,
        }
    }

    fn synthetic_clean_k2_drain_report() -> OperatorPanicK2TaskDrainReport {
        OperatorPanicK2TaskDrainReport {
            reason: "synthetic",
            hotkey_spawn_source_quiescent: true,
            owners_before: synthetic_k2_owner_readback(
                false,
                Vec::new(),
                vec![OperatorPanicK2TaskOwnerState {
                    task_id: 1,
                    generation: 1,
                    terminal: false,
                }],
            ),
            panic_safety_before: synapse_action::OperatorPanicSafetyReadback {
                epoch: 1,
                publications_in_flight: 0,
                outstanding_generations: 1,
                outstanding_finalizations: 0,
                accounting_incident: false,
                pending: true,
            },
            tasks_observed: 1,
            graceful_joined: 1,
            abort_requests_sent: 0,
            joined_after_abort: 0,
            task_successes: 1,
            retained_task_owner_ids: Vec::new(),
            owners_after: synthetic_k2_owner_readback(true, Vec::new(), Vec::new()),
            panic_safety_after: synapse_action::OperatorPanicSafetyReadback {
                epoch: 1,
                publications_in_flight: 0,
                outstanding_generations: 0,
                outstanding_finalizations: 0,
                accounting_incident: false,
                pending: false,
            },
            elapsed_ms: 1,
            failures: Vec::new(),
        }
    }

    #[test]
    fn k2_drain_verdict_requires_closed_empty_owner_readback() {
        let clean = synthetic_clean_k2_drain_report();
        assert!(clean.owners_quiescent());
        clean.verdict().expect("closed empty tracker should pass");

        let mut pending = clean.clone();
        pending.owners_after.pending_spawn_task_ids.push(2);
        assert!(!pending.owners_quiescent());
        assert!(pending.verdict().is_err());

        let mut retained = clean;
        retained
            .owners_after
            .task_owners
            .push(OperatorPanicK2TaskOwnerState {
                task_id: 3,
                generation: 3,
                terminal: true,
            });
        retained.retained_task_owner_ids.push(3);
        assert!(!retained.owners_quiescent());
        assert!(retained.verdict().is_err());
    }

    #[test]
    fn k2_drain_verdict_rejects_unaccounted_or_late_work() {
        let mut unaccounted = synthetic_clean_k2_drain_report();
        unaccounted.tasks_observed = 2;
        assert!(unaccounted.verdict().is_err());

        let mut late = synthetic_clean_k2_drain_report();
        late.owners_after.reservations_after_admission_close = 1;
        assert!(late.owners_quiescent());
        assert!(late.verdict().is_err());

        let mut live_hotkey_source = synthetic_clean_k2_drain_report();
        live_hotkey_source.hotkey_spawn_source_quiescent = false;
        assert!(!live_hotkey_source.owners_quiescent());
        assert!(live_hotkey_source.verdict().is_err());

        let mut orphaned_generation = synthetic_clean_k2_drain_report();
        orphaned_generation
            .panic_safety_after
            .outstanding_generations = 1;
        orphaned_generation.panic_safety_after.pending = true;
        assert!(
            !orphaned_generation.owners_quiescent(),
            "an empty task tracker must not hide a published panic with no K2 owner"
        );
        assert!(orphaned_generation.verdict().is_err());
    }
}

fn handle_operator_hotkey(
    service: &SynapseService,
    m3_state: &SharedM3State,
    runtime: &Handle,
    mut operator_panic_token: synapse_action::OperatorPanicSafetyToken,
) {
    let operator_panic_generation = operator_panic_token.generation();
    let started = Instant::now();
    // The publication that created this token already closes all mutation
    // admission. Release physical inputs before taking any potentially
    // contended lease/service lock.
    let durable_browser_mutation_owners_after_disable =
        synapse_a11y::durable_browser_mutation_owners_disable_now();
    if !durable_browser_mutation_owners_after_disable.registry_readback_healthy
        || !durable_browser_mutation_owners_after_disable
            .registry_readback_failures
            .is_empty()
    {
        synapse_action::record_operator_panic_safety_incident();
    }
    // Closing the durable browser-owner admission gate is non-blocking. Do it
    // before ReleaseAll so autonomous CDP handlers cannot mutate during the
    // bounded emitter acknowledgement; ReleaseAll remains K1's first blocking
    // operation.
    let release_all_report = fire_release_all();
    let release_all_elapsed = started.elapsed();
    let lease_before = synapse_action::lease::status();
    let preempted_lease = synapse_action::force_preempt_input_lease_for_operator_panic(
        "operator_hotkey",
        operator_panic_generation,
    );
    let lease_snapshot_after_preempt = synapse_action::input_lease_safety_snapshot();
    let lease_after_preempt = lease_snapshot_after_preempt.status;
    let lease_generation_after_preempt = lease_snapshot_after_preempt.operator_panic_generation;
    // Reflex state uses non-blocking lock attempts so a contended service mutex
    // cannot extend the remainder of K1 indefinitely.
    let disable_report = disable_reflexes(m3_state);
    let elapsed = started.elapsed();
    let within_budget = release_all_elapsed <= OPERATOR_RELEASE_ALL_TIMEOUT;
    let current_panic_epoch = synapse_action::operator_panic_safety_readback().epoch;
    let lease_preemption_proven = lease_after_preempt.owner_session_id.as_deref()
        == Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID)
        && lease_generation_after_preempt.is_some_and(|current| {
            current >= operator_panic_generation && current <= current_panic_epoch
        });
    let reflexes_terminal = matches!(disable_report.result, "ok" | "not_initialized");
    let k1_evidence_terminal = lease_preemption_proven
        && release_all_report.result == "ok"
        && !durable_browser_mutation_owners_after_disable.enabled
        && durable_browser_mutation_owners_after_disable.registry_readback_healthy
        && durable_browser_mutation_owners_after_disable
            .registry_readback_failures
            .is_empty()
        && reflexes_terminal
        && within_budget;
    let k1_safety_terminal = k1_evidence_terminal
        && synapse_action::acknowledge_operator_panic_preemption(&mut operator_panic_token);
    if !k1_safety_terminal {
        tracing::error!(
            code = error_codes::ACTION_POSTCONDITION_FAILED,
            detail_code = "OPERATOR_PANIC_K1_PREEMPTION_UNACKNOWLEDGED",
            operator_panic_generation,
            lease_after_preempt = ?lease_after_preempt,
            lease_generation_after_preempt,
            release_all_result = release_all_report.result,
            reflex_result = disable_report.result,
            durable_browser_mutation_owners_after_disable = ?durable_browser_mutation_owners_after_disable,
            within_budget,
            "operator hotkey K1 could not prove every immediate safety postcondition; foreground admission remains fail-closed"
        );
    }
    let immediate = OperatorHotkeyImmediateReport {
        hotkey: synapse_action::hotkey::DEFAULT_OPERATOR_HOTKEY,
        operator_panic_generation,
        lease_before,
        preempted_lease: preempted_lease.clone(),
        lease_after_preempt,
        disable_report: disable_report.clone(),
        release_all_report: release_all_report.clone(),
        durable_browser_mutation_owners_after_disable:
            durable_browser_mutation_owners_after_disable.clone(),
        release_all_elapsed_ms: release_all_elapsed.as_millis(),
        elapsed_ms: elapsed.as_millis(),
        within_budget,
        k1_safety_terminal,
    };
    tracing::warn!(
        code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
        hotkey = immediate.hotkey,
        operator_panic_generation,
        input_lease_preempted = preempted_lease.is_some(),
        input_lease_prior_owner = ?preempted_lease
            .as_ref()
            .and_then(|status| status.owner_session_id.clone()),
        input_lease_operator_owner = synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID,
        input_lease_operator_ttl_ms = synapse_action::OPERATOR_PREEMPT_LEASE_TTL_MS,
        disabled_reflexes = disable_report.disabled_ids.len(),
        disabled_reflex_ids = ?disable_report.disabled_ids,
        reflex_result = disable_report.result,
        reflex_error_code = ?disable_report.error_code,
        reflex_detail = ?disable_report.detail,
        release_all_result = release_all_report.result,
        release_all_error_code = ?release_all_report.error_code,
        release_all_detail = ?release_all_report.detail,
        durable_browser_mutation_owners_after_disable = ?durable_browser_mutation_owners_after_disable,
        release_all_elapsed_ms = immediate.release_all_elapsed_ms,
        elapsed_ms = immediate.elapsed_ms,
        within_budget = immediate.within_budget,
        k1_safety_terminal,
        "operator hotkey fired release_all, disabled reflexes, and queued K2 fleet kill"
    );
    let service = service.clone();
    let task_id = reserve_operator_panic_k2_task(operator_panic_generation);
    let spawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.spawn(async move {
            let failure = match service.operator_panic_kill_all(immediate).await {
                Ok(response) if response.all_stopped => {
                    let chrome_extension_disable_sequence = response
                        .chrome_extension_mutation_owners
                        .disable
                        .as_ref()
                        .map(|readback| readback.disable_sequence);
                    match synapse_action::complete_operator_panic_safety_generation(
                        operator_panic_token,
                    ) {
                        Ok(synapse_action::OperatorPanicSafetyCompletion::Pending) => None,
                        Ok(synapse_action::OperatorPanicSafetyCompletion::Finalize(
                            finalization,
                        )) => reconcile_operator_panic_lease_finalization(
                            finalization,
                            chrome_extension_disable_sequence,
                        )
                        .await
                        .err(),
                        Err(detail) => Some(format!(
                            "K2 generation {operator_panic_generation} accounting failed: {detail}"
                        )),
                    }
                }
                Ok(response) => {
                    synapse_action::record_operator_panic_safety_incident();
                    Some(format!(
                        "K2 generation {operator_panic_generation} did not prove all_stopped: {response:?}"
                    ))
                }
                Err(error) => {
                    synapse_action::record_operator_panic_safety_incident();
                    let error_code = error
                        .data
                        .as_ref()
                        .and_then(|data| data.get("code"))
                        .and_then(serde_json::Value::as_str);
                    let detail = format!(
                        "K2 fleet kill failed: error_code={error_code:?}; detail={}",
                        error.message
                    );
                    tracing::error!(
                        code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                        task_id,
                        operator_panic_generation,
                        error_code = ?error_code,
                        detail = %error.message,
                        "operator hotkey K2 fleet kill task failed"
                    );
                    Some(detail)
                }
            };
            OperatorPanicK2TaskCompletion {
                generation: operator_panic_generation,
                failure,
            }
        })
    }));
    match spawn {
        Ok(task) => publish_operator_panic_k2_task(task_id, operator_panic_generation, task),
        Err(_panic) => {
            synapse_action::record_operator_panic_safety_incident();
            record_operator_panic_k2_spawn_failure(
                task_id,
                operator_panic_generation,
                "Tokio runtime spawn panicked before returning a JoinHandle".to_owned(),
            );
            tracing::error!(
                code = "MCP_OPERATOR_PANIC_K2_SPAWN_PANICKED",
                task_id,
                operator_panic_generation,
                "operator hotkey K2 fleet kill could not be spawned; no JoinHandle was returned"
            );
        }
    }
}

async fn reconcile_operator_panic_lease_finalization(
    finalization: synapse_action::OperatorPanicSafetyFinalization,
    chrome_extension_disable_sequence: Option<u64>,
) -> Result<(), String> {
    let generation = finalization.generation();
    let cleared = synapse_action::force_clear_operator_panic_input_lease_generation(
        generation,
        "operator_hotkey_k2_finalization",
    );
    // One coherent physical readback after the exact clear is the verdict. A
    // status/tag pair composed across two lease-lock acquisitions can be torn
    // by a concurrent finalizer and falsely retain safety accounting forever.
    let (lease_snapshot_after, safety_after, lease_postcondition_ok) =
        operator_panic_finalization_postcondition_readback(generation);
    let lease_after = &lease_snapshot_after.status;
    let tagged_generation_after = lease_snapshot_after.operator_panic_generation;
    let finished = synapse_action::finish_operator_panic_safety_finalization(
        finalization,
        lease_postcondition_ok,
    );
    tracing::info!(
        code = "MCP_OPERATOR_PANIC_K2_LEASE_FINALIZATION_READBACK",
        operator_panic_generation = generation,
        exact_generation_cleared = cleared.is_some(),
        lease_after = ?lease_after,
        tagged_generation_after,
        safety_after = ?safety_after,
        lease_postcondition_ok,
        accounting_finished = finished,
        "readback=operator_panic_lease edge=K2_finalization after_exact_generation_clear"
    );
    if !(lease_postcondition_ok && finished) {
        return Err(format!(
            "K2 generation {generation} lease finalization failed: cleared={} lease_after={lease_after:?} tagged_generation_after={tagged_generation_after:?} safety_after={safety_after:?} accounting_finished={finished}",
            cleared.is_some()
        ));
    }

    let reset_safety_before = synapse_action::operator_panic_safety_readback();
    if reset_safety_before.pending {
        // A newer hotkey wave owns the closed browser-mutator gate. Its K2
        // drain will reset the gate only after that newer generation becomes
        // the unique finalizer.
        return Ok(());
    }
    let reset_epoch = reset_safety_before.epoch;
    let Some(expected_chrome_extension_disable_sequence) = chrome_extension_disable_sequence else {
        synapse_action::record_operator_panic_safety_incident();
        return Err(format!(
            "K2 generation {generation} extension-owner reset lacked the exact disable generation proven by K2"
        ));
    };
    let chrome_extension_before_reset =
        crate::chrome_debugger_bridge::operator_panic_readback()
            .await
            .map_err(|error| {
                synapse_action::record_operator_panic_safety_incident();
                format!(
                    "K2 generation {generation} extension-owner reset precondition readback failed: {}: {}",
                    error.code(),
                    error.detail()
                )
            })?;
    if !chrome_extension_owner_closed_for_exact_wave(
        expected_chrome_extension_disable_sequence,
        &chrome_extension_before_reset,
    ) {
        synapse_action::record_operator_panic_safety_incident();
        return Err(format!(
            "K2 generation {generation} extension-owner reset precondition failed: expected_disable_sequence={expected_chrome_extension_disable_sequence} readback={chrome_extension_before_reset:?}"
        ));
    }
    let browser_owner_before_reset = synapse_a11y::durable_browser_mutation_owners_readback();
    if browser_owner_before_reset.enabled
        || !browser_owner_before_reset.registry_readback_healthy
        || !browser_owner_before_reset
            .registry_readback_failures
            .is_empty()
        || browser_owner_before_reset.fetch_interception_active_count != 0
        || browser_owner_before_reset.network_override_active_count != 0
        || browser_owner_before_reset.dialog_auto_policy_active_count != 0
        || browser_owner_before_reset.clock_active_count != 0
        || browser_owner_before_reset.init_script_active_count != 0
        || browser_owner_before_reset.unresolved_raw_cdp_evaluate_timeout_count != 0
        || browser_owner_before_reset.unresolved_raw_cdp_input_owner_count != 0
        || browser_owner_before_reset.persisted_cdp_mutation_owner_count != 0
        || browser_owner_before_reset.persisted_cdp_input_owner_count != 0
        || browser_owner_before_reset.persisted_cdp_evaluate_owner_count != 0
        || browser_owner_before_reset.persisted_cdp_init_script_effect_owner_count != 0
    {
        synapse_action::record_operator_panic_safety_incident();
        return Err(format!(
            "K2 generation {generation} browser-owner reset precondition failed: {browser_owner_before_reset:?}"
        ));
    }
    let expected_disable_sequence = browser_owner_before_reset.disable_sequence;
    let chrome_extension_reset = crate::chrome_debugger_bridge::operator_panic_enable_if_unchanged(
        expected_chrome_extension_disable_sequence,
    )
    .await;
    let chrome_extension_after_reset =
        crate::chrome_debugger_bridge::operator_panic_readback().await;
    let reset_safety_after_extension = synapse_action::operator_panic_safety_readback();
    if reset_safety_after_extension.pending || reset_safety_after_extension.epoch != reset_epoch {
        let browser_owner_disabled_for_newer_wave =
            synapse_a11y::durable_browser_mutation_owners_readback();
        if chrome_extension_after_reset.as_ref().is_ok_and(|readback| {
            chrome_extension_owner_closed_for_newer_wave(
                expected_chrome_extension_disable_sequence,
                readback,
            )
        }) && browser_owner_gate_closed_for_newer_wave(
            expected_disable_sequence,
            &browser_owner_disabled_for_newer_wave,
        ) {
            tracing::warn!(
                code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                operator_panic_generation = generation,
                reset_epoch,
                reset_safety_after_extension = ?reset_safety_after_extension,
                chrome_extension_reset = ?chrome_extension_reset,
                chrome_extension_after_reset = ?chrome_extension_after_reset,
                browser_owner_disabled_for_newer_wave = ?browser_owner_disabled_for_newer_wave,
                "a newer operator-panic wave superseded extension-owner reset; both generation-checked browser admission gates remained closed"
            );
            return Ok(());
        }
        synapse_action::record_operator_panic_safety_incident();
        return Err(format!(
            "K2 generation {generation} extension-owner reset crossed a newer panic wave without two closed newer gates: safety={reset_safety_after_extension:?} extension_reset={chrome_extension_reset:?} extension_readback={chrome_extension_after_reset:?} browser_readback={browser_owner_disabled_for_newer_wave:?}"
        ));
    }
    let chrome_extension_reset = chrome_extension_reset.map_err(|error| {
        synapse_action::record_operator_panic_safety_incident();
        format!(
            "K2 generation {generation} extension-owner conditional enable failed: {}: {}",
            error.code(),
            error.detail()
        )
    })?;
    let chrome_extension_after_reset = chrome_extension_after_reset.map_err(|error| {
        synapse_action::record_operator_panic_safety_incident();
        format!(
            "K2 generation {generation} extension-owner independent enabled readback failed: {}: {}",
            error.code(),
            error.detail()
        )
    })?;
    if !chrome_extension_enable_terminal(
        expected_chrome_extension_disable_sequence,
        &chrome_extension_reset,
        &chrome_extension_after_reset,
    ) {
        synapse_action::record_operator_panic_safety_incident();
        return Err(format!(
            "K2 generation {generation} extension-owner conditional enable failed its independent readback: reset={chrome_extension_reset:?} after={chrome_extension_after_reset:?}"
        ));
    }
    let browser_owner_reset = synapse_a11y::durable_browser_mutation_owners_enable_if_unchanged(
        expected_disable_sequence,
    )
    .await;
    let reset_safety_after = synapse_action::operator_panic_safety_readback();
    if reset_safety_after.pending || reset_safety_after.epoch != reset_epoch {
        // The enable call's returned snapshot can be stale if a newer K1
        // disables immediately after it. Read the physical registry again and
        // accept only strictly newer, healthy, empty, closed generations for
        // both daemon- and extension-owned browser mutation surfaces.
        let disabled_for_newer_wave = synapse_a11y::durable_browser_mutation_owners_readback();
        let chrome_extension_disabled_for_newer_wave =
            crate::chrome_debugger_bridge::operator_panic_readback().await;
        if browser_owner_gate_closed_for_newer_wave(
            expected_disable_sequence,
            &disabled_for_newer_wave,
        ) && chrome_extension_disabled_for_newer_wave
            .as_ref()
            .is_ok_and(|readback| {
                chrome_extension_owner_closed_for_newer_wave(
                    expected_chrome_extension_disable_sequence,
                    readback,
                )
            })
        {
            tracing::warn!(
                code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                operator_panic_generation = generation,
                reset_epoch,
                reset_safety_after = ?reset_safety_after,
                browser_owner_reset = ?browser_owner_reset,
                disabled_for_newer_wave = ?disabled_for_newer_wave,
                chrome_extension_disabled_for_newer_wave = ?chrome_extension_disabled_for_newer_wave,
                "a newer operator-panic wave superseded browser-owner reset; both generation-checked admission gates remained closed"
            );
            return Ok(());
        }
        synapse_action::record_operator_panic_safety_incident();
        return Err(format!(
            "K2 generation {generation} browser-owner reset crossed a newer panic wave: safety={reset_safety_after:?} reset_readback={browser_owner_reset:?} fresh_readback={disabled_for_newer_wave:?} extension_fresh_readback={chrome_extension_disabled_for_newer_wave:?}"
        ));
    }
    tracing::info!(
        code = "MCP_OPERATOR_PANIC_BROWSER_OWNER_RESET_READBACK",
        operator_panic_generation = generation,
        browser_owner_reset = ?browser_owner_reset,
        chrome_extension_reset = ?chrome_extension_reset,
        chrome_extension_after_reset = ?chrome_extension_after_reset,
        reset_safety_after = ?reset_safety_after,
        "readback=durable_browser_mutation_owners+chrome_extension_mutation_owners edge=K2_finalization after_explicit_reset"
    );
    if browser_owner_reset.enabled
        && browser_owner_reset.disable_sequence == expected_disable_sequence
        && browser_owner_reset.registry_readback_healthy
        && browser_owner_reset.registry_readback_failures.is_empty()
        && browser_owner_reset.fetch_interception_active_count == 0
        && browser_owner_reset.network_override_active_count == 0
        && browser_owner_reset.dialog_auto_policy_active_count == 0
        && browser_owner_reset.clock_active_count == 0
        && browser_owner_reset.init_script_active_count == 0
        && browser_owner_reset.unresolved_raw_cdp_evaluate_timeout_count == 0
        && browser_owner_reset.unresolved_raw_cdp_input_owner_count == 0
        && browser_owner_reset.persisted_cdp_mutation_owner_count == 0
        && browser_owner_reset.persisted_cdp_input_owner_count == 0
        && browser_owner_reset.persisted_cdp_evaluate_owner_count == 0
        && browser_owner_reset.persisted_cdp_init_script_effect_owner_count == 0
    {
        Ok(())
    } else {
        synapse_action::record_operator_panic_safety_incident();
        Err(format!(
            "K2 generation {generation} browser-owner reset failed its independent readback: {browser_owner_reset:?}"
        ))
    }
}

fn chrome_extension_owner_closed_for_exact_wave(
    expected_disable_sequence: u64,
    readback: &crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback,
) -> bool {
    !readback.enabled
        && readback.disable_sequence == expected_disable_sequence
        && readback.mutation_handlers_started_count == readback.mutation_handlers_completed_count
        && chrome_extension_owner_continuity_healthy(readback)
        && readback.active_after.is_empty()
        && readback.fully_drained
}

fn chrome_extension_owner_closed_for_newer_wave(
    expected_disable_sequence: u64,
    readback: &crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback,
) -> bool {
    !readback.enabled
        && readback.disable_sequence > expected_disable_sequence
        && readback.mutation_handlers_started_count == readback.mutation_handlers_completed_count
        && chrome_extension_owner_continuity_healthy(readback)
        && readback.active_after.is_empty()
        && readback.fully_drained
}

fn chrome_extension_owner_continuity_healthy(
    readback: &crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback,
) -> bool {
    readback.command_in_flight_count == 1
        && readback.command_activity_sequence
            == readback.command_last_completed_sequence.saturating_add(1)
        && !readback.worker_boot_id.is_empty()
        && readback
            .browser_session_id
            .as_deref()
            .is_some_and(|session_id| !session_id.is_empty())
        && !readback.ledger_browser_session_id.is_empty()
        && readback.browser_session_id.as_deref()
            == Some(readback.ledger_browser_session_id.as_str())
        && readback.browser_session_continuity_matched
        && readback.stale_browser_session_owner_count == 0
        && readback.storage_state_loaded
        && readback.storage_state_load_error.is_none()
        && readback.persisted_state_revision > 0
        && readback.persisted_in_flight_mutation.is_none()
        && readback.unresolved_debugger_command_timeouts.is_empty()
        && readback.unresolved_worker_restart_mutation_count == 0
        && readback.owner_continuity_healthy
}

fn chrome_extension_enable_terminal(
    expected_disable_sequence: u64,
    reset: &crate::chrome_debugger_bridge::ChromeDebuggerExtensionEnableReadback,
    after: &crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback,
) -> bool {
    reset.expected_disable_sequence == expected_disable_sequence
        && reset.generation_matched
        && reset.owners_were_empty
        && reset.enable_applied
        && reset.owner.enabled
        && reset.owner.disable_sequence == expected_disable_sequence
        && chrome_extension_owner_continuity_healthy(&reset.owner)
        && reset.owner.active_after.is_empty()
        && reset.owner.mutation_handlers_started_count
            == reset.owner.mutation_handlers_completed_count
        && after.enabled
        && after.disable_sequence == expected_disable_sequence
        && chrome_extension_owner_continuity_healthy(after)
        && after.active_after.is_empty()
        && after.mutation_handlers_started_count == after.mutation_handlers_completed_count
        && reset.owner.mutation_handlers_started_count == after.mutation_handlers_started_count
        && reset.owner.mutation_handlers_completed_count == after.mutation_handlers_completed_count
        && reset.owner.worker_boot_id == after.worker_boot_id
        && reset.owner.persisted_state_revision == after.persisted_state_revision
        && after.command_activity_sequence
            == reset.owner.command_activity_sequence.saturating_add(1)
        && after.command_last_completed_sequence == reset.owner.command_activity_sequence
}

fn browser_owner_gate_closed_for_newer_wave(
    expected_disable_sequence: u64,
    readback: &synapse_a11y::CdpDurableBrowserMutationOwnersReadback,
) -> bool {
    !readback.enabled
        && readback.disable_sequence > expected_disable_sequence
        && readback.registry_readback_healthy
        && readback.registry_readback_failures.is_empty()
        && readback.fetch_interception_active_count == 0
        && readback.network_override_active_count == 0
        && readback.dialog_auto_policy_active_count == 0
        && readback.clock_active_count == 0
        && readback.init_script_active_count == 0
        && readback.unresolved_raw_cdp_evaluate_timeout_count == 0
        && readback.unresolved_raw_cdp_input_owner_count == 0
        && readback.persisted_cdp_mutation_owner_count == 0
        && readback.persisted_cdp_input_owner_count == 0
        && readback.persisted_cdp_evaluate_owner_count == 0
        && readback.persisted_cdp_init_script_effect_owner_count == 0
}

fn operator_panic_finalization_postcondition_readback(
    generation: u64,
) -> (
    synapse_action::LeaseSafetySnapshot,
    synapse_action::OperatorPanicSafetyReadback,
    bool,
) {
    // Another finalizer can clear its newer lease and decrement its owner after
    // our lease snapshot but before our safety-counter snapshot. Retry that
    // mixed observation so a successful concurrent finalizer cannot create a
    // permanent false incident. Two passes are sufficient by program order:
    // exact lease clear precedes the other finalizer's atomic decrement.
    for attempt in 0..2 {
        let lease = synapse_action::input_lease_safety_snapshot();
        let safety = synapse_action::operator_panic_safety_readback();
        let ok = operator_panic_finalization_postcondition(generation, &lease, &safety);
        if ok || attempt == 1 {
            return (lease, safety, ok);
        }
        std::hint::spin_loop();
    }
    unreachable!("bounded operator-panic finalization readback loop always returns")
}

fn operator_panic_finalization_postcondition(
    generation: u64,
    lease: &synapse_action::LeaseSafetySnapshot,
    safety: &synapse_action::OperatorPanicSafetyReadback,
) -> bool {
    if !lease.status.held {
        return true;
    }
    lease.status.owner_session_id.as_deref()
        == Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID)
        && lease.operator_panic_generation.is_some_and(|current| {
            operator_panic_tag_belongs_to_newer_published_wave(generation, current, safety.epoch)
        })
        && (safety.outstanding_generations != 0 || safety.outstanding_finalizations > 1)
}

const fn operator_panic_tag_belongs_to_newer_published_wave(
    finalizing_generation: u64,
    tagged_generation: u64,
    latest_published_generation: u64,
) -> bool {
    // Publication can outrun K1: while generation g is finalizing, g+1 may
    // already own the tagged lease and g+2 may be published but not yet reach
    // its worker. Requiring the tag to equal the latest epoch would falsely
    // reject that protected g+1 lease. The outstanding-owner check at the call
    // site proves admission is still closed; this interval proves the tag is
    // newer than this finalizer and was actually published.
    tagged_generation > finalizing_generation && tagged_generation <= latest_published_generation
}

pub(crate) fn disable_reflexes(m3_state: &SharedM3State) -> DisableReport {
    let runtime = match m3_state.try_lock() {
        Ok(state) => state.reflex_runtime.clone(),
        Err(std::sync::TryLockError::Poisoned(_error)) => {
            return DisableReport {
                result: "error",
                disabled_ids: Vec::new(),
                error_code: Some(error_codes::TOOL_INTERNAL_ERROR),
                detail: Some("M3 service state lock poisoned".to_owned()),
            };
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            return DisableReport {
                result: "contended",
                disabled_ids: Vec::new(),
                error_code: Some(error_codes::TOOL_INTERNAL_ERROR),
                detail: Some(
                    "M3 service state lock was contended at the operator-panic K1 boundary"
                        .to_owned(),
                ),
            };
        }
    };
    let Some(runtime) = runtime else {
        return DisableReport {
            result: "not_initialized",
            disabled_ids: Vec::new(),
            error_code: None,
            detail: None,
        };
    };
    let mut runtime = match runtime.try_lock() {
        Ok(runtime) => runtime,
        Err(std::sync::TryLockError::Poisoned(_error)) => {
            return DisableReport {
                result: "error",
                disabled_ids: Vec::new(),
                error_code: Some(error_codes::TOOL_INTERNAL_ERROR),
                detail: Some("reflex runtime lock poisoned".to_owned()),
            };
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            return DisableReport {
                result: "contended",
                disabled_ids: Vec::new(),
                error_code: Some(error_codes::TOOL_INTERNAL_ERROR),
                detail: Some(
                    "reflex runtime lock was contended at the operator-panic K1 boundary"
                        .to_owned(),
                ),
            };
        }
    };
    match runtime.disable_all_by_operator() {
        Ok(disabled) => DisableReport {
            result: "ok",
            disabled_ids: disabled.into_iter().map(|status| status.id).collect(),
            error_code: None,
            detail: None,
        },
        Err(error) => DisableReport {
            result: "error",
            disabled_ids: Vec::new(),
            error_code: Some(error.code()),
            detail: Some(error.to_string()),
        },
    }
}

/// Separate post-disable readback used by K2 after every request-wide physical
/// mutation reservation has drained. A missing runtime is terminal because no
/// reflex scheduler exists; lock failures stay fail-closed.
pub(crate) fn operator_panic_reflex_active_count_readback(
    m3_state: &SharedM3State,
) -> Result<Option<usize>, String> {
    let runtime = match m3_state.try_lock() {
        Ok(state) => state.reflex_runtime.clone(),
        Err(std::sync::TryLockError::Poisoned(_error)) => {
            return Err("M3 service state lock poisoned during K2 reflex readback".to_owned());
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err("M3 service state lock contended during K2 reflex readback".to_owned());
        }
    };
    let Some(runtime) = runtime else {
        return Ok(None);
    };
    runtime
        .try_lock()
        .map(|runtime| Some(runtime.active_count()))
        .map_err(|error| match error {
            std::sync::TryLockError::Poisoned(_error) => {
                "reflex runtime lock poisoned during K2 readback".to_owned()
            }
            std::sync::TryLockError::WouldBlock => {
                "reflex runtime lock contended during K2 readback".to_owned()
            }
        })
}

pub(crate) fn fire_release_all_with_handle(
    handle: &synapse_action::ActionHandle,
) -> ReleaseAllReport {
    fire_release_all_with_handle_timeout(handle, OPERATOR_RELEASE_ALL_TIMEOUT)
}

pub(crate) fn fire_release_all_with_handle_timeout(
    handle: &synapse_action::ActionHandle,
    timeout: Duration,
) -> ReleaseAllReport {
    match handle.fire_release_all_blocking_with_timeout(timeout) {
        Ok(()) => ReleaseAllReport {
            result: "ok",
            error_code: None,
            detail: None,
        },
        Err(error) => ReleaseAllReport {
            result: "error",
            error_code: Some(error.code()),
            detail: Some(error.to_string()),
        },
    }
}

fn fire_release_all() -> ReleaseAllReport {
    let Some(handle) = RELEASE_ALL_HANDLE.get() else {
        return ReleaseAllReport {
            result: "missing_handle",
            error_code: Some(error_codes::ACTION_BACKEND_UNAVAILABLE),
            detail: Some("RELEASE_ALL_HANDLE is not initialized".to_owned()),
        };
    };
    fire_release_all_with_handle(handle)
}

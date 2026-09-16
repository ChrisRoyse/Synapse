use std::{
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use synapse_core::{ElementId, element_id, win32_hwnd::hwnd_to_wire};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::{
    Foundation::{GetLastError, HWND},
    System::Com::{
        APTTYPE, APTTYPE_MAINSTA, APTTYPE_MTA, APTTYPE_NA, APTTYPE_STA, APTTYPEQUALIFIER,
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoGetApartmentType, CoInitializeEx,
        CoUninitialize,
    },
    System::Threading::GetCurrentThreadId,
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        WindowsAndMessaging::{
            DispatchMessageW, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SELECTION, EVENT_OBJECT_VALUECHANGE,
            EVENT_SYSTEM_ALERT, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MENUEND,
            EVENT_SYSTEM_MENUSTART, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
            WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT,
        },
    },
};

use crate::{
    A11yError, A11yResult, AccessibleEvent, AccessibleEventKind, ComApartmentKind,
    WinEventHookReadback, WinEventSubscriptionShutdownRecord, WinEventSubscriptionShutdownReport,
};

use super::snapshot::invalidate_snapshot_cache;

const WIN_EVENT_START_TIMEOUT: Duration = Duration::from_secs(3);
const WIN_EVENT_DROP_TIMEOUT: Duration = Duration::from_millis(500);
const WIN_EVENT_WAKE_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(100);
const WIN_EVENT_MESSAGE_POLL_INTERVAL: Duration = Duration::from_millis(25);
const WIN_EVENT_STATE_LOCK_TIMEOUT: Duration = Duration::from_millis(100);
const WIN_EVENT_STATE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(1);
const WIN_EVENT_CALLBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

struct WinEventDeliveryState {
    sender: Option<UnboundedSender<AccessibleEvent>>,
    subscription_owner_id: Option<u64>,
    last_released_owner_id: Option<u64>,
}

static WIN_EVENT_DELIVERY: Mutex<WinEventDeliveryState> = Mutex::new(WinEventDeliveryState {
    sender: None,
    subscription_owner_id: None,
    last_released_owner_id: None,
});
static RETAINED_WIN_EVENT_OWNERS: AtomicPtr<RetainedWinEventOwnerNode> =
    AtomicPtr::new(std::ptr::null_mut());
static RETAINED_WIN_EVENT_OWNER_COUNT: AtomicUsize = AtomicUsize::new(0);
static WIN_EVENT_SHUTDOWN_REPORTS: AtomicPtr<WinEventShutdownReportNode> =
    AtomicPtr::new(std::ptr::null_mut());
static NEXT_WIN_EVENT_OWNER_ID: AtomicU64 = AtomicU64::new(1);
/// The WinEvent callback is an OS-owned latency-sensitive boundary. It marks
/// cache invalidation with one atomic store; the exact hook owner performs the
/// potentially blocking cache-mutex operation after the callback returns.
static SNAPSHOT_CACHE_INVALIDATION_PENDING: AtomicBool = AtomicBool::new(false);
static WIN_EVENT_CALLBACKS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static WIN_EVENT_CALLBACK_DELIVERY_CONTENTION: AtomicUsize = AtomicUsize::new(0);
static WIN_EVENT_CALLBACK_DELIVERY_POISON: AtomicUsize = AtomicUsize::new(0);

struct WinEventCallbackInFlight;

impl WinEventCallbackInFlight {
    fn enter() -> Self {
        WIN_EVENT_CALLBACKS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for WinEventCallbackInFlight {
    fn drop(&mut self) {
        WIN_EVENT_CALLBACKS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

const WIN_EVENT_IDS: [u32; 10] = [
    EVENT_SYSTEM_FOREGROUND,
    EVENT_OBJECT_FOCUS,
    EVENT_OBJECT_VALUECHANGE,
    EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_CREATE,
    EVENT_OBJECT_DESTROY,
    EVENT_OBJECT_SELECTION,
    EVENT_SYSTEM_MENUSTART,
    EVENT_SYSTEM_MENUEND,
    EVENT_SYSTEM_ALERT,
];
pub struct WinEventSubscription {
    owner_id: u64,
    stop: Arc<AtomicBool>,
    wake: mpsc::Sender<WinEventWakeRequest>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
    exit_report: Option<mpsc::Receiver<WinEventThreadExitReport>>,
    readback: WinEventHookReadback,
    shutdown_report: Option<WinEventSubscriptionShutdownReport>,
}

struct WinEventWakeRequest {
    acknowledgement: mpsc::Sender<u32>,
}

#[derive(Debug)]
struct WinEventWakeReadback {
    request_sent: bool,
    acknowledged_thread_id: Option<u32>,
    failure: Option<String>,
}

impl WinEventWakeReadback {
    fn exact_owner_acknowledged(&self, expected_thread_id: Option<u32>) -> bool {
        self.request_sent
            && self.acknowledged_thread_id.is_some()
            && expected_thread_id
                .is_none_or(|expected| self.acknowledged_thread_id == Some(expected))
            && self.failure.is_none()
    }
}

struct WinEventShutdownReportNode {
    next: AtomicPtr<Self>,
    record: WinEventSubscriptionShutdownRecord,
}

fn publish_win_event_shutdown_report(record: WinEventSubscriptionShutdownRecord) {
    let node = Box::new(WinEventShutdownReportNode {
        next: AtomicPtr::new(std::ptr::null_mut()),
        record,
    });
    let node = Box::into_raw(node);
    let mut head = WIN_EVENT_SHUTDOWN_REPORTS.load(Ordering::Acquire);
    loop {
        // SAFETY: `node` is exclusively owned until the successful CAS. Report
        // nodes are immutable, append-only, and never reclaimed in-process.
        unsafe { (*node).next.store(head, Ordering::Relaxed) };
        match WIN_EVENT_SHUTDOWN_REPORTS.compare_exchange_weak(
            head,
            node,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => head = actual,
        }
    }
}

fn win_event_shutdown_report_history() -> Vec<WinEventSubscriptionShutdownRecord> {
    let mut reports = Vec::new();
    let mut node = WIN_EVENT_SHUTDOWN_REPORTS.load(Ordering::Acquire);
    while !node.is_null() {
        // SAFETY: published report nodes are immutable and never reclaimed
        // before process teardown, so cloning through this pointer is safe.
        let current = unsafe { &*node };
        reports.push(current.record.clone());
        node = current.next.load(Ordering::Acquire);
    }
    reports
}

fn mark_snapshot_cache_invalidation_pending() {
    SNAPSHOT_CACHE_INVALIDATION_PENDING.store(true, Ordering::Release);
}

fn drain_snapshot_cache_invalidation() -> bool {
    if !SNAPSHOT_CACHE_INVALIDATION_PENDING.swap(false, Ordering::AcqRel) {
        return false;
    }
    invalidate_snapshot_cache();
    true
}

fn drain_callback_diagnostics() {
    let contention = WIN_EVENT_CALLBACK_DELIVERY_CONTENTION.swap(0, Ordering::AcqRel);
    if contention != 0 {
        tracing::debug!(
            code = "A11Y_WIN_EVENT_DELIVERY_LOCK_CONTENDED",
            dropped_event_count = contention,
            operation = "owner_loop_readback",
            "WinEvent callbacks dropped events instead of blocking on delivery-state shutdown"
        );
    }
    let poison = WIN_EVENT_CALLBACK_DELIVERY_POISON.swap(0, Ordering::AcqRel);
    if poison != 0 {
        tracing::error!(
            code = "A11Y_WIN_EVENT_DELIVERY_LOCK_POISONED",
            observed_count = poison,
            operation = "owner_loop_readback",
            "WinEvent callbacks observed a poisoned delivery-state lock"
        );
    }
}

fn wake_win_event_owner_before_stop(
    wake: &mpsc::Sender<WinEventWakeRequest>,
    expected_thread_id: Option<u32>,
    timeout: Duration,
) -> WinEventWakeReadback {
    let (acknowledgement, acknowledged) = mpsc::channel();
    if wake.send(WinEventWakeRequest { acknowledgement }).is_err() {
        return WinEventWakeReadback {
            request_sent: false,
            acknowledged_thread_id: None,
            failure: Some(
                "WinEvent exact-owner wake channel disconnected before stop was requested"
                    .to_owned(),
            ),
        };
    }

    match acknowledged.recv_timeout(timeout) {
        Ok(actual_thread_id) => {
            let failure = expected_thread_id
                .filter(|expected| *expected != actual_thread_id)
                .map(|expected| {
                    format!(
                        "WinEvent wake acknowledgement came from thread {actual_thread_id}, not published owner {expected}"
                    )
                });
            WinEventWakeReadback {
                request_sent: true,
                acknowledged_thread_id: Some(actual_thread_id),
                failure,
            }
        }
        Err(error) => WinEventWakeReadback {
            request_sent: true,
            acknowledged_thread_id: None,
            failure: Some(format!(
                "WinEvent exact-owner wake was not acknowledged within {} ms before stop: {error}",
                timeout.as_millis()
            )),
        },
    }
}

impl WinEventSubscription {
    pub const fn readback(&self) -> &WinEventHookReadback {
        &self.readback
    }

    pub fn shutdown_report_history() -> Vec<WinEventSubscriptionShutdownRecord> {
        win_event_shutdown_report_history()
    }

    pub fn shutdown_checked(
        mut self,
        timeout: Duration,
        reason: &'static str,
    ) -> WinEventSubscriptionShutdownReport {
        self.shutdown_inner(timeout, reason)
    }

    fn shutdown_inner(
        &mut self,
        timeout: Duration,
        reason: &'static str,
    ) -> WinEventSubscriptionShutdownReport {
        if let Some(previous) = self.shutdown_report.as_ref() {
            let mut report = previous.clone();
            report.reason = reason;
            return report;
        }

        let shutdown_started = Instant::now();
        let mut failures = Vec::new();
        // This process-private channel has one receiver owned by the exact
        // hook thread. Require its thread-ID acknowledgement before setting
        // stop; never address a recyclable numeric TID with a Win32 post.
        let wake = wake_win_event_owner_before_stop(
            &self.wake,
            Some(self.thread_id),
            timeout.min(WIN_EVENT_WAKE_HANDSHAKE_TIMEOUT),
        );
        if let Some(failure) = wake.failure.clone() {
            failures.push(failure);
        }
        let stop_wake_sent = wake.exact_owner_acknowledged(Some(self.thread_id));
        self.stop.store(true, Ordering::SeqCst);
        let (sender_disconnected, disconnect_failure) = disconnect_win_event_sender(self.owner_id);
        if let Some(failure) = disconnect_failure {
            failures.push(failure);
        }

        let thread_owner_present = self.join.is_some();
        let mut exit_report_receiver = self.exit_report.take();
        let mut owner_retained = false;
        let join_readback = match self.join.take() {
            None => ThreadJoinReadback::owner_absent(),
            Some(thread) => {
                match join_thread_until(thread, timeout.saturating_sub(shutdown_started.elapsed()))
                {
                    ThreadJoinOutcome::Joined(readback) => readback,
                    ThreadJoinOutcome::TimedOut { readback, thread } => {
                        retain_win_event_owner(RetainedWinEventOwner {
                            owner_id: self.owner_id,
                            thread_id: self.thread_id,
                            expected_hook_count: Some(self.readback.hook_count),
                            join: Some(thread),
                            exit_report: exit_report_receiver.take(),
                            terminal_report: None,
                            failures: Vec::new(),
                        });
                        owner_retained = true;
                        readback
                    }
                }
            }
        };
        if let Some(failure) = join_readback.failure.clone() {
            failures.push(failure);
        }

        let exit_report = if join_readback.thread_terminal && join_readback.thread_joined {
            exit_report_receiver
                .take()
                .and_then(|receiver| receiver.try_recv().ok())
        } else {
            None
        };
        let thread_exit_report_received = exit_report.is_some();
        let (unregister_attempted, unregister_succeeded, unregister_failed_event_ids) =
            exit_report.as_ref().map_or_else(
                || (0, 0, Vec::new()),
                |report| {
                    failures.extend(report.failures.iter().cloned());
                    (
                        report.unregister_attempted,
                        report.unregister_succeeded,
                        report.unregister_failed_event_ids.clone(),
                    )
                },
            );
        if thread_owner_present && join_readback.thread_terminal && !thread_exit_report_received {
            failures.push(
                "WinEvent hook thread terminated without publishing unregistration readback"
                    .to_owned(),
            );
        }
        let unregister_complete = thread_exit_report_received
            && unregister_attempted == self.readback.hook_count
            && unregister_succeeded == self.readback.hook_count
            && unregister_failed_event_ids.is_empty();
        let mut subscription_slot_released = false;
        if join_readback.thread_terminal && join_readback.thread_joined && unregister_complete {
            let (released, release_failure) = release_win_event_subscription_slot(self.owner_id);
            subscription_slot_released = released;
            if let Some(failure) = release_failure {
                failures.push(failure);
            }
        }
        if !subscription_slot_released {
            failures.push(
                "WinEvent process subscription slot retained because terminal unregistration was not proven"
                    .to_owned(),
            );
            if !owner_retained {
                let retained_failures = join_readback.failure.iter().cloned().collect();
                retain_win_event_owner(RetainedWinEventOwner {
                    owner_id: self.owner_id,
                    thread_id: self.thread_id,
                    expected_hook_count: Some(self.readback.hook_count),
                    join: None,
                    exit_report: exit_report_receiver,
                    terminal_report: exit_report,
                    failures: retained_failures,
                });
            }
        }

        let report = WinEventSubscriptionShutdownReport {
            reason,
            thread_id: self.thread_id,
            hook_count: self.readback.hook_count,
            stop_requested: true,
            stop_wake_sent,
            sender_disconnected,
            subscription_slot_released,
            thread_owner_present,
            thread_terminal: join_readback.thread_terminal,
            thread_joined: join_readback.thread_joined,
            thread_exit_report_received,
            unregister_attempted,
            unregister_succeeded,
            unregister_failed_event_ids,
            exact_owner_retained: join_readback.exact_owner_retained,
            failures,
        };
        publish_win_event_shutdown_report(WinEventSubscriptionShutdownRecord {
            owner_id: self.owner_id,
            report: report.clone(),
        });
        self.shutdown_report = Some(report.clone());
        report
    }
}

impl Drop for WinEventSubscription {
    fn drop(&mut self) {
        let report = self.shutdown_inner(WIN_EVENT_DROP_TIMEOUT, "drop_backstop");
        if report.verdict().is_err() {
            tracing::error!(
                code = "A11Y_WIN_EVENT_DROP_INCOMPLETE",
                report = ?report,
                "bounded WinEvent subscription drop could not prove every OS owner quiescent"
            );
        }
    }
}

#[derive(Clone, Debug)]
struct WinEventThreadExitReport {
    unregister_attempted: usize,
    unregister_succeeded: usize,
    unregister_failed_event_ids: Vec<u32>,
    failures: Vec<String>,
}

struct RetainedWinEventOwner {
    owner_id: u64,
    thread_id: u32,
    expected_hook_count: Option<usize>,
    join: Option<JoinHandle<()>>,
    exit_report: Option<mpsc::Receiver<WinEventThreadExitReport>>,
    terminal_report: Option<WinEventThreadExitReport>,
    failures: Vec<String>,
}

struct RetainedWinEventOwnerNode {
    next: AtomicPtr<Self>,
    owner: Mutex<RetainedWinEventOwner>,
    resolved: AtomicBool,
}

fn retain_win_event_owner(owner: RetainedWinEventOwner) {
    let node = Box::new(RetainedWinEventOwnerNode {
        next: AtomicPtr::new(std::ptr::null_mut()),
        owner: Mutex::new(owner),
        resolved: AtomicBool::new(false),
    });
    let node = Box::into_raw(node);
    // Publish the fail-closed count before the pointer. A concurrent reader may
    // briefly see a conservative nonzero count, but can never resolve a node
    // and decrement from zero.
    RETAINED_WIN_EVENT_OWNER_COUNT.fetch_add(1, Ordering::AcqRel);
    let mut head = RETAINED_WIN_EVENT_OWNERS.load(Ordering::Acquire);
    loop {
        // SAFETY: `node` is exclusively owned until the successful CAS. Nodes
        // are append-only and never reclaimed before process teardown.
        unsafe { (*node).next.store(head, Ordering::Relaxed) };
        match RETAINED_WIN_EVENT_OWNERS.compare_exchange_weak(
            head,
            node,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => head = actual,
        }
    }
}

fn retained_owner_unregister_complete(owner: &RetainedWinEventOwner) -> bool {
    let Some(report) = owner.terminal_report.as_ref() else {
        return false;
    };
    let expected = owner
        .expected_hook_count
        .unwrap_or(report.unregister_attempted);
    report.unregister_attempted == expected
        && report.unregister_succeeded == expected
        && report.unregister_failed_event_ids.is_empty()
        && report.failures.is_empty()
        && owner.failures.is_empty()
}

fn try_reconcile_retained_owner(owner: &mut RetainedWinEventOwner) -> bool {
    if owner.join.as_ref().is_some_and(|join| !join.is_finished()) {
        return false;
    }
    if let Some(join) = owner.join.take()
        && join.join().is_err()
    {
        owner.failures.push(format!(
            "retained WinEvent thread {} panicked",
            owner.thread_id
        ));
    }
    if owner.terminal_report.is_none()
        && let Some(receiver) = owner.exit_report.take()
    {
        match receiver.try_recv() {
            Ok(report) => owner.terminal_report = Some(report),
            Err(error) => owner.failures.push(format!(
                "retained WinEvent thread {} had no terminal unregister report: {error}",
                owner.thread_id
            )),
        }
    }
    if !retained_owner_unregister_complete(owner) {
        return false;
    }
    let (released, failure) = release_win_event_subscription_slot(owner.owner_id);
    if let Some(failure) = failure {
        tracing::error!(
            code = "A11Y_WIN_EVENT_RETAINED_SLOT_RELEASE_FAILED",
            thread_id = owner.thread_id,
            failure = %failure,
            "retained WinEvent owner could not yet reconcile the global subscription slot"
        );
        return false;
    }
    released
}

fn reconcile_retained_win_event_owners() {
    let mut node = RETAINED_WIN_EVENT_OWNERS.load(Ordering::Acquire);
    while !node.is_null() {
        // SAFETY: published nodes are never reclaimed or mutated except through
        // their mutex/atomics, so the process-lifetime pointer remains valid.
        let current = unsafe { &*node };
        if !current.resolved.load(Ordering::Acquire) {
            match current.owner.try_lock() {
                Ok(mut owner) => {
                    if try_reconcile_retained_owner(&mut owner)
                        && !current.resolved.swap(true, Ordering::AcqRel)
                    {
                        RETAINED_WIN_EVENT_OWNER_COUNT.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                Err(TryLockError::Poisoned(poisoned)) => {
                    let mut owner = poisoned.into_inner();
                    owner
                        .failures
                        .push("retained WinEvent owner registry lock was poisoned".to_owned());
                }
                Err(TryLockError::WouldBlock) => {}
            }
        }
        node = current.next.load(Ordering::Acquire);
    }
}

struct InstalledWinEventHook {
    event_id: u32,
    handle: Option<HWINEVENTHOOK>,
}

struct WinEventHookInstallFailure {
    detail: String,
    unregister_attempted: usize,
    unregister_succeeded: usize,
    unregister_failed_event_ids: Vec<u32>,
}

impl InstalledWinEventHook {
    fn unregister(&mut self) -> Result<(), String> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        if unsafe { UnhookWinEvent(handle) }.as_bool() {
            Ok(())
        } else {
            let os_error = unsafe { GetLastError() };
            // Keep the exact hook handle for the Drop backstop. The checked
            // report still records this first failed physical operation and
            // therefore cannot claim quiescence even if the retry succeeds.
            self.handle = Some(handle);
            Err(format!(
                "UnhookWinEvent failed for event_id={}: {os_error:?}",
                self.event_id
            ))
        }
    }
}

impl Drop for InstalledWinEventHook {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        if !unsafe { UnhookWinEvent(handle) }.as_bool() {
            let os_error = unsafe { GetLastError() };
            tracing::error!(
                code = "A11Y_WIN_EVENT_HOOK_DROP_UNREGISTER_FAILED",
                event_id = self.event_id,
                os_error = ?os_error,
                "WinEvent hook Drop backstop could not unregister the exact hook handle"
            );
        }
    }
}

#[derive(Debug)]
struct ThreadJoinReadback {
    thread_terminal: bool,
    thread_joined: bool,
    exact_owner_retained: bool,
    failure: Option<String>,
}

impl ThreadJoinReadback {
    const fn owner_absent() -> Self {
        Self {
            thread_terminal: true,
            thread_joined: true,
            exact_owner_retained: false,
            failure: None,
        }
    }
}

struct StartupUnwindReadback {
    join: ThreadJoinReadback,
    thread_exit_report_received: bool,
    unregister_attempted: usize,
    unregister_succeeded: usize,
    unregister_failed_event_ids: Vec<u32>,
    subscription_slot_released: bool,
    thread_failures: Vec<String>,
}

impl std::fmt::Display for StartupUnwindReadback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "join={:?} thread_exit_report_received={} unregister_attempted={} unregister_succeeded={} unregister_failed_event_ids={:?} subscription_slot_released={} thread_failures={:?}",
            self.join,
            self.thread_exit_report_received,
            self.unregister_attempted,
            self.unregister_succeeded,
            self.unregister_failed_event_ids,
            self.subscription_slot_released,
            self.thread_failures
        )
    }
}

fn lock_win_event_delivery_until(
    operation: &'static str,
) -> Result<(MutexGuard<'static, WinEventDeliveryState>, bool), String> {
    let started = Instant::now();
    loop {
        match WIN_EVENT_DELIVERY.try_lock() {
            Ok(state) => return Ok((state, false)),
            Err(TryLockError::Poisoned(poisoned)) => {
                WIN_EVENT_DELIVERY.clear_poison();
                tracing::error!(
                    code = "A11Y_WIN_EVENT_DELIVERY_LOCK_POISONED",
                    operation,
                    "recovering poisoned WinEvent delivery-state lock"
                );
                return Ok((poisoned.into_inner(), true));
            }
            Err(TryLockError::WouldBlock) => {
                if started.elapsed() >= WIN_EVENT_STATE_LOCK_TIMEOUT {
                    return Err(format!(
                        "WinEvent delivery-state lock remained contended for {} ms during {operation}",
                        WIN_EVENT_STATE_LOCK_TIMEOUT.as_millis()
                    ));
                }
                thread::sleep(WIN_EVENT_STATE_LOCK_POLL_INTERVAL);
            }
        }
    }
}

fn reserve_win_event_subscription_slot(
    owner_id: u64,
    sender: UnboundedSender<AccessibleEvent>,
) -> A11yResult<()> {
    reconcile_retained_win_event_owners();
    let retained_owner_count = RETAINED_WIN_EVENT_OWNER_COUNT.load(Ordering::Acquire);
    if retained_owner_count != 0 {
        return Err(A11yError::internal(format!(
            "{retained_owner_count} WinEvent ownership/evidence record(s) remain unreconciled"
        )));
    }
    let (mut state, poison_recovered) =
        lock_win_event_delivery_until("reserve").map_err(A11yError::internal)?;
    if poison_recovered {
        return Err(A11yError::internal(
            "WinEvent delivery-state lock was poisoned during subscription reservation",
        ));
    }
    if state.subscription_owner_id.is_some() || state.sender.is_some() {
        return Err(A11yError::internal(format!(
            "a WinEvent subscription owner is already active or retained in this process: owner_id={:?} sender_present={}",
            state.subscription_owner_id,
            state.sender.is_some()
        )));
    }
    state.subscription_owner_id = Some(owner_id);
    state.sender = Some(sender);
    Ok(())
}

fn disconnect_win_event_sender(owner_id: u64) -> (bool, Option<String>) {
    match lock_win_event_delivery_until("disconnect") {
        Ok((mut state, poison_recovered)) => {
            let mut failures = Vec::new();
            if poison_recovered {
                failures.push(
                    "WinEvent delivery-state lock was poisoned while disconnecting the sender"
                        .to_owned(),
                );
            }
            let disconnected = match disconnect_win_event_sender_for_owner(&mut state, owner_id) {
                Ok(()) => true,
                Err(error) => {
                    failures.push(error);
                    false
                }
            };
            (
                disconnected,
                (!failures.is_empty()).then(|| failures.join("; ")),
            )
        }
        Err(error) => (false, Some(error)),
    }
}

fn disconnect_win_event_sender_for_owner(
    state: &mut WinEventDeliveryState,
    owner_id: u64,
) -> Result<(), String> {
    if state.subscription_owner_id == Some(owner_id) {
        state.sender = None;
        return Ok(());
    }
    if state.subscription_owner_id.is_none()
        && state.last_released_owner_id == Some(owner_id)
        && state.sender.is_none()
    {
        return Ok(());
    }
    Err(format!(
        "WinEvent sender disconnect owner mismatch: expected={owner_id} actual={:?}",
        state.subscription_owner_id
    ))
}

fn release_win_event_subscription_slot(owner_id: u64) -> (bool, Option<String>) {
    match lock_win_event_delivery_until("release") {
        Ok((mut state, poison_recovered)) => {
            let mut failures = Vec::new();
            if poison_recovered {
                failures.push(
                    "WinEvent delivery-state lock was poisoned while releasing the subscription slot"
                        .to_owned(),
                );
            }
            let released = match release_win_event_subscription_slot_for_owner(&mut state, owner_id)
            {
                Ok(()) => true,
                Err(error) => {
                    failures.push(error);
                    false
                }
            };
            (
                released,
                (!failures.is_empty()).then(|| failures.join("; ")),
            )
        }
        Err(error) => (false, Some(error)),
    }
}

fn release_win_event_subscription_slot_for_owner(
    state: &mut WinEventDeliveryState,
    owner_id: u64,
) -> Result<(), String> {
    if state.subscription_owner_id == Some(owner_id) {
        state.sender = None;
        state.subscription_owner_id = None;
        state.last_released_owner_id = Some(owner_id);
        return Ok(());
    }
    if state.subscription_owner_id.is_none()
        && state.last_released_owner_id == Some(owner_id)
        && state.sender.is_none()
    {
        return Ok(());
    }
    Err(format!(
        "WinEvent subscription-slot release owner mismatch: expected={owner_id} actual={:?} last_released={:?}",
        state.subscription_owner_id, state.last_released_owner_id
    ))
}

enum ThreadJoinOutcome {
    Joined(ThreadJoinReadback),
    TimedOut {
        readback: ThreadJoinReadback,
        thread: JoinHandle<()>,
    },
}

fn join_thread_until(thread: JoinHandle<()>, timeout: Duration) -> ThreadJoinOutcome {
    let started = Instant::now();
    while !thread.is_finished() && started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        thread::sleep(Duration::from_millis(10).min(remaining));
    }
    if !thread.is_finished() {
        return ThreadJoinOutcome::TimedOut {
            readback: ThreadJoinReadback {
                thread_terminal: false,
                thread_joined: false,
                exact_owner_retained: true,
                failure: Some(format!(
                    "WinEvent hook thread did not terminate within {} ms; exact JoinHandle retained for reconciliation",
                    timeout.as_millis()
                )),
            },
            thread,
        };
    }
    ThreadJoinOutcome::Joined(match thread.join() {
        Ok(()) => ThreadJoinReadback {
            thread_terminal: true,
            thread_joined: true,
            exact_owner_retained: false,
            failure: None,
        },
        Err(_panic) => ThreadJoinReadback {
            thread_terminal: true,
            thread_joined: true,
            exact_owner_retained: false,
            failure: Some("WinEvent hook thread panicked".to_owned()),
        },
    })
}

pub fn retained_live_owner_count() -> usize {
    reconcile_retained_win_event_owners();
    let retained = RETAINED_WIN_EVENT_OWNER_COUNT.load(Ordering::Acquire);
    let active_or_unobservable_slot = match WIN_EVENT_DELIVERY.try_lock() {
        Ok(state) => usize::from(win_event_delivery_slot_is_active(&state)),
        // A poisoned or contended global slot is not proof of absence. Count it
        // as unsafe so the existing daemon lifetime gate fails closed.
        Err(TryLockError::Poisoned(_) | TryLockError::WouldBlock) => 1,
    };
    retained.max(active_or_unobservable_slot)
}

const fn win_event_delivery_slot_is_active(state: &WinEventDeliveryState) -> bool {
    state.subscription_owner_id.is_some() || state.sender.is_some()
}

fn unwind_failed_startup(
    owner_id: u64,
    thread: JoinHandle<()>,
    exit_report: mpsc::Receiver<WinEventThreadExitReport>,
    thread_id: Option<u32>,
) -> StartupUnwindReadback {
    let mut exit_report = Some(exit_report);
    let mut owner_retained = false;
    let join = match join_thread_until(thread, WIN_EVENT_START_TIMEOUT) {
        ThreadJoinOutcome::Joined(join) => join,
        ThreadJoinOutcome::TimedOut { readback, thread } => {
            retain_win_event_owner(RetainedWinEventOwner {
                owner_id,
                thread_id: thread_id.unwrap_or(0),
                expected_hook_count: None,
                join: Some(thread),
                exit_report: exit_report.take(),
                terminal_report: None,
                failures: Vec::new(),
            });
            owner_retained = true;
            readback
        }
    };
    let report = (join.thread_terminal && join.thread_joined)
        .then(|| {
            exit_report
                .take()
                .and_then(|receiver| receiver.try_recv().ok())
        })
        .flatten();
    let thread_exit_report_received = report.is_some();
    let mut thread_failures = join.failure.iter().cloned().collect::<Vec<_>>();
    let (unregister_attempted, unregister_succeeded, unregister_failed_event_ids) =
        report.as_ref().map_or_else(
            || (0, 0, Vec::new()),
            |report| {
                thread_failures.extend(report.failures.iter().cloned());
                (
                    report.unregister_attempted,
                    report.unregister_succeeded,
                    report.unregister_failed_event_ids.clone(),
                )
            },
        );
    let unregister_complete = thread_exit_report_received
        && unregister_attempted == unregister_succeeded
        && unregister_failed_event_ids.is_empty()
        && thread_failures.is_empty();
    let retained_failures = thread_failures.clone();
    let subscription_slot_released =
        if join.thread_terminal && join.thread_joined && unregister_complete {
            let (released, failure) = release_win_event_subscription_slot(owner_id);
            if let Some(failure) = failure {
                thread_failures.push(failure);
            }
            released
        } else {
            false
        };
    if !owner_retained && !subscription_slot_released {
        retain_win_event_owner(RetainedWinEventOwner {
            owner_id,
            thread_id: thread_id.unwrap_or(0),
            expected_hook_count: None,
            join: None,
            exit_report: None,
            terminal_report: report,
            failures: retained_failures,
        });
    }
    StartupUnwindReadback {
        join,
        thread_exit_report_received,
        unregister_attempted,
        unregister_succeeded,
        unregister_failed_event_ids,
        subscription_slot_released,
        thread_failures,
    }
}

pub fn subscribe_win_events(
    sender: UnboundedSender<AccessibleEvent>,
) -> A11yResult<WinEventSubscription> {
    let owner_id = NEXT_WIN_EVENT_OWNER_ID.fetch_add(1, Ordering::Relaxed);
    reserve_win_event_subscription_slot(owner_id, sender)?;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let (wake_tx, wake_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (thread_id_tx, thread_id_rx) = mpsc::channel();
    let (exit_report_tx, exit_report_rx) = mpsc::channel();
    let join = thread::Builder::new()
        .name("synapse-a11y-winevent".to_owned())
        .spawn(move || {
            win_event_thread(thread_stop, wake_rx, thread_id_tx, ready_tx, exit_report_tx);
        })
        .map_err(|err| {
            let release = release_win_event_subscription_slot(owner_id);
            A11yError::internal(format!("{err}; subscription_slot_release={release:?}"))
        })?;
    let thread_id = match thread_id_rx.recv_timeout(WIN_EVENT_START_TIMEOUT) {
        Ok(thread_id) => thread_id,
        Err(error) => {
            let wake =
                wake_win_event_owner_before_stop(&wake_tx, None, WIN_EVENT_WAKE_HANDSHAKE_TIMEOUT);
            stop.store(true, Ordering::SeqCst);
            let disconnect = disconnect_win_event_sender(owner_id);
            let unwind =
                unwind_failed_startup(owner_id, join, exit_report_rx, wake.acknowledged_thread_id);
            return Err(A11yError::internal(format!(
                "WinEvent hook thread did not publish its identity: {error}; wake={wake:?}; sender_disconnect={disconnect:?}; startup_unwind={unwind}"
            )));
        }
    };
    let readback = match ready_rx.recv_timeout(WIN_EVENT_START_TIMEOUT) {
        Ok(Ok(readback)) => readback,
        Ok(Err(error)) => {
            let wake = wake_win_event_owner_before_stop(
                &wake_tx,
                Some(thread_id),
                WIN_EVENT_WAKE_HANDSHAKE_TIMEOUT,
            );
            stop.store(true, Ordering::SeqCst);
            let disconnect = disconnect_win_event_sender(owner_id);
            let unwind = unwind_failed_startup(owner_id, join, exit_report_rx, Some(thread_id));
            return Err(A11yError::internal(format!(
                "{error}; wake={wake:?}; sender_disconnect={disconnect:?}; startup_unwind={unwind}"
            )));
        }
        Err(error) => {
            let wake = wake_win_event_owner_before_stop(
                &wake_tx,
                Some(thread_id),
                WIN_EVENT_WAKE_HANDSHAKE_TIMEOUT,
            );
            stop.store(true, Ordering::SeqCst);
            let disconnect = disconnect_win_event_sender(owner_id);
            let unwind = unwind_failed_startup(owner_id, join, exit_report_rx, Some(thread_id));
            return Err(A11yError::internal(format!(
                "WinEvent hook did not initialize: {error}; wake={wake:?}; sender_disconnect={disconnect:?}; startup_unwind={unwind}"
            )));
        }
    };
    if readback.thread_id != thread_id {
        let wake = wake_win_event_owner_before_stop(
            &wake_tx,
            Some(thread_id),
            WIN_EVENT_WAKE_HANDSHAKE_TIMEOUT,
        );
        stop.store(true, Ordering::SeqCst);
        let disconnect = disconnect_win_event_sender(owner_id);
        let unwind = unwind_failed_startup(owner_id, join, exit_report_rx, Some(thread_id));
        return Err(A11yError::internal(format!(
            "WinEvent readiness identity mismatch: identity_channel={thread_id} hook_readback={}; wake={wake:?}; sender_disconnect={disconnect:?}; startup_unwind={unwind}",
            readback.thread_id
        )));
    }

    Ok(WinEventSubscription {
        owner_id,
        stop,
        wake: wake_tx,
        thread_id: readback.thread_id,
        join: Some(join),
        exit_report: Some(exit_report_rx),
        readback,
        shutdown_report: None,
    })
}
#[allow(clippy::needless_pass_by_value)]
fn win_event_thread(
    stop: Arc<AtomicBool>,
    wake: mpsc::Receiver<WinEventWakeRequest>,
    thread_id_ready: mpsc::Sender<u32>,
    ready: mpsc::Sender<A11yResult<WinEventHookReadback>>,
    exit_report: mpsc::Sender<WinEventThreadExitReport>,
) {
    let thread_id = unsafe { GetCurrentThreadId() };
    let _ = thread_id_ready.send(thread_id);
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    if init.is_err() {
        let _ = ready.send(Err(A11yError::internal(format!("{init:?}"))));
        let _ = exit_report.send(WinEventThreadExitReport {
            unregister_attempted: 0,
            unregister_succeeded: 0,
            unregister_failed_event_ids: Vec::new(),
            failures: Vec::new(),
        });
        return;
    }

    let hooks = match install_hooks() {
        Ok(hooks) => hooks,
        Err(failure) => {
            let detail = failure.detail;
            let _ = ready.send(Err(A11yError::internal(detail.clone())));
            unsafe {
                CoUninitialize();
            }
            let _ = exit_report.send(WinEventThreadExitReport {
                unregister_attempted: failure.unregister_attempted,
                unregister_succeeded: failure.unregister_succeeded,
                unregister_failed_event_ids: failure.unregister_failed_event_ids,
                failures: vec![detail],
            });
            return;
        }
    };

    let readback = WinEventHookReadback {
        thread_id,
        apartment: read_current_apartment(),
        hook_count: hooks.len(),
        event_ids: hooks.iter().map(|hook| hook.event_id).collect(),
    };
    let _ = ready.send(Ok(readback));

    let mut failures = Vec::new();
    let mut msg = MSG::default();
    'run: while !stop.load(Ordering::SeqCst) {
        while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                break 'run;
            }
            unsafe {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        // The callback only set an atomic. Perform the potentially blocking
        // snapshot-cache mutation after DispatchMessageW has returned.
        let _ = drain_snapshot_cache_invalidation();
        drain_callback_diagnostics();
        if !stop.load(Ordering::SeqCst) {
            match wake.recv_timeout(WIN_EVENT_MESSAGE_POLL_INTERVAL) {
                Ok(request) => {
                    let _ = request.acknowledgement.send(thread_id);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // A disconnected control channel is abnormal unless the
                    // exact guard already requested stop. Preserve that fact
                    // in terminal readback; never turn it into clean shutdown.
                    if !stop.load(Ordering::SeqCst) {
                        failures.push(
                            "WinEvent exact-owner wake channel disconnected before stop was requested"
                                .to_owned(),
                        );
                    }
                    let _ = drain_snapshot_cache_invalidation();
                    break;
                }
            }
        }
    }

    // Drain before unregistering and again after every exact hook handle has
    // been processed. The terminal report is not published while callback
    // work remains only as an unobserved pending atomic.
    let _ = drain_snapshot_cache_invalidation();
    let unregister_attempted = hooks.len();
    let mut unregister_succeeded = 0;
    let mut unregister_failed_event_ids = Vec::new();
    for mut hook in hooks {
        let event_id = hook.event_id;
        match hook.unregister() {
            Ok(()) => unregister_succeeded += 1,
            Err(error) => {
                unregister_failed_event_ids.push(event_id);
                failures.push(error);
            }
        }
    }
    let callback_deadline = Instant::now() + WIN_EVENT_CALLBACK_DRAIN_TIMEOUT;
    while WIN_EVENT_CALLBACKS_IN_FLIGHT.load(Ordering::Acquire) != 0
        && Instant::now() < callback_deadline
    {
        thread::sleep(WIN_EVENT_STATE_LOCK_POLL_INTERVAL);
    }
    let callbacks_in_flight = WIN_EVENT_CALLBACKS_IN_FLIGHT.load(Ordering::Acquire);
    if callbacks_in_flight != 0 {
        failures.push(format!(
            "{callbacks_in_flight} WinEvent callback(s) remained in flight after exact hook unregistration"
        ));
    }
    while drain_snapshot_cache_invalidation() {}
    if SNAPSHOT_CACHE_INVALIDATION_PENDING.load(Ordering::Acquire) {
        failures.push(
            "WinEvent snapshot-cache invalidation remained pending at terminal owner readback"
                .to_owned(),
        );
    }
    drain_callback_diagnostics();
    unsafe {
        CoUninitialize();
    }
    let _ = exit_report.send(WinEventThreadExitReport {
        unregister_attempted,
        unregister_succeeded,
        unregister_failed_event_ids,
        failures,
    });
}

fn install_hooks() -> Result<Vec<InstalledWinEventHook>, WinEventHookInstallFailure> {
    let mut hooks = Vec::with_capacity(WIN_EVENT_IDS.len());
    for event_id in WIN_EVENT_IDS {
        let hook = unsafe {
            SetWinEventHook(
                event_id,
                event_id,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_invalid() {
            // SetWinEventHook documents a null handle on failure. Capture the
            // thread-local error before any unwind call can replace it.
            let install_error = unsafe { GetLastError() };
            let installed_event_ids = hooks
                .iter()
                .map(|installed: &InstalledWinEventHook| installed.event_id)
                .collect::<Vec<_>>();
            let mut unregister_succeeded = 0_usize;
            let mut unregister_failed_event_ids = Vec::new();
            let mut unregister_failures = Vec::new();
            for installed in hooks.iter_mut().rev() {
                match installed.unregister() {
                    Ok(()) => unregister_succeeded += 1,
                    Err(error) => {
                        unregister_failed_event_ids.push(installed.event_id);
                        unregister_failures.push(error);
                    }
                }
            }
            return Err(WinEventHookInstallFailure {
                detail: format!(
                    "SetWinEventHook failed for event_id={event_id}: {install_error:?}; installed_event_ids={installed_event_ids:?}; exact_unwind_failures={unregister_failures:?}"
                ),
                unregister_attempted: hooks.len(),
                unregister_succeeded,
                unregister_failed_event_ids,
            });
        }
        hooks.push(InstalledWinEventHook {
            event_id,
            handle: Some(hook),
        });
    }
    Ok(hooks)
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    idobject: i32,
    idchild: i32,
    _event_thread: u32,
    event_time_ms: u32,
) {
    let _in_flight = WinEventCallbackInFlight::enter();
    // A WinEvent callback must never wait behind shutdown. Clone the cheap
    // channel owner under a try-lock, then release the process-global state
    // before cache invalidation or event delivery.
    let sender = match WIN_EVENT_DELIVERY.try_lock() {
        Ok(guard) => guard.sender.clone(),
        Err(TryLockError::Poisoned(poisoned)) => {
            WIN_EVENT_CALLBACK_DELIVERY_POISON.fetch_add(1, Ordering::Relaxed);
            poisoned.into_inner().sender.clone()
        }
        Err(TryLockError::WouldBlock) => {
            WIN_EVENT_CALLBACK_DELIVERY_CONTENTION.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let Some(sender) = sender else {
        return;
    };
    let Some(kind) = event_kind(event) else {
        return;
    };
    mark_snapshot_cache_invalidation_pending();

    let window_id = hwnd_to_wire(hwnd.0 as isize);
    let event = AccessibleEvent {
        seq: u64::from(event_time_ms),
        at_ms: u64::from(event_time_ms),
        window_id,
        element_id: event_element_id(hwnd, event, idobject, idchild),
        kind,
        name: None,
        value: None,
    };
    let _ = sender.send(event);
}

const fn event_kind(event: u32) -> Option<AccessibleEventKind> {
    match event {
        EVENT_SYSTEM_FOREGROUND => Some(AccessibleEventKind::ForegroundChanged),
        EVENT_OBJECT_FOCUS => Some(AccessibleEventKind::FocusChanged),
        EVENT_OBJECT_VALUECHANGE => Some(AccessibleEventKind::ValueChanged),
        EVENT_OBJECT_NAMECHANGE => Some(AccessibleEventKind::NameChanged),
        EVENT_OBJECT_CREATE => Some(AccessibleEventKind::ElementAppeared),
        EVENT_OBJECT_DESTROY => Some(AccessibleEventKind::ElementDisappeared),
        EVENT_OBJECT_SELECTION => Some(AccessibleEventKind::SelectionChanged),
        EVENT_SYSTEM_MENUSTART => Some(AccessibleEventKind::MenuStart),
        EVENT_SYSTEM_MENUEND => Some(AccessibleEventKind::MenuEnd),
        EVENT_SYSTEM_ALERT => Some(AccessibleEventKind::Alert),
        _ => None,
    }
}

fn event_element_id(hwnd: HWND, event: u32, idobject: i32, idchild: i32) -> Option<ElementId> {
    if hwnd.0.is_null() {
        return None;
    }
    let window_id = hwnd_to_wire(hwnd.0 as isize);
    let runtime_id = format!(
        "{event:08x}{:08x}{:08x}",
        idobject.cast_unsigned(),
        idchild.cast_unsigned()
    );
    Some(element_id(window_id, &runtime_id))
}

fn read_current_apartment() -> ComApartmentKind {
    let mut apartment = APTTYPE::default();
    let mut qualifier = APTTYPEQUALIFIER::default();
    if unsafe { CoGetApartmentType(&raw mut apartment, &raw mut qualifier) }.is_err() {
        return ComApartmentKind::Unknown;
    }
    match apartment {
        value if value == APTTYPE_STA => ComApartmentKind::Sta,
        value if value == APTTYPE_MTA => ComApartmentKind::Mta,
        value if value == APTTYPE_NA => ComApartmentKind::Neutral,
        value if value == APTTYPE_MAINSTA => ComApartmentKind::MainSta,
        _ => ComApartmentKind::Unknown,
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    fn retained_owner_with_report(
        expected_hook_count: usize,
        unregister_attempted: usize,
        unregister_succeeded: usize,
    ) -> RetainedWinEventOwner {
        RetainedWinEventOwner {
            owner_id: 11,
            thread_id: 17,
            expected_hook_count: Some(expected_hook_count),
            join: None,
            exit_report: None,
            terminal_report: Some(WinEventThreadExitReport {
                unregister_attempted,
                unregister_succeeded,
                unregister_failed_event_ids: Vec::new(),
                failures: Vec::new(),
            }),
            failures: Vec::new(),
        }
    }

    fn clean_shutdown_report(thread_id: u32) -> WinEventSubscriptionShutdownReport {
        WinEventSubscriptionShutdownReport {
            reason: "synthetic_history_test",
            thread_id,
            hook_count: 1,
            stop_requested: true,
            stop_wake_sent: true,
            sender_disconnected: true,
            subscription_slot_released: true,
            thread_owner_present: true,
            thread_terminal: true,
            thread_joined: true,
            thread_exit_report_received: true,
            unregister_attempted: 1,
            unregister_succeeded: 1,
            unregister_failed_event_ids: Vec::new(),
            exact_owner_retained: false,
            failures: Vec::new(),
        }
    }

    #[test]
    fn retained_owner_requires_exact_expected_unregister_count() {
        let short = retained_owner_with_report(2, 1, 1);
        assert!(!retained_owner_unregister_complete(&short));

        let complete = retained_owner_with_report(2, 2, 2);
        assert!(retained_owner_unregister_complete(&complete));
    }

    #[test]
    fn retained_owner_failure_evidence_never_reconciles_as_clean() {
        let mut owner = retained_owner_with_report(1, 1, 1);
        owner
            .failures
            .push("synthetic retained-owner failure".to_owned());

        assert!(!retained_owner_unregister_complete(&owner));
    }

    #[test]
    fn wake_handshake_requires_acknowledgement_from_expected_owner() {
        let (wake_tx, wake_rx) = mpsc::channel::<WinEventWakeRequest>();
        let owner = thread::spawn(move || {
            let request = match wake_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(request) => request,
                Err(error) => panic!("synthetic WinEvent owner received no wake: {error}"),
            };
            if let Err(error) = request.acknowledgement.send(41) {
                panic!("synthetic WinEvent owner could not acknowledge wake: {error}");
            }
        });

        let readback = wake_win_event_owner_before_stop(&wake_tx, Some(41), Duration::from_secs(1));
        assert!(owner.join().is_ok(), "synthetic WinEvent owner panicked");

        assert!(readback.exact_owner_acknowledged(Some(41)), "{readback:?}");
        assert!(!readback.exact_owner_acknowledged(Some(42)), "{readback:?}");
    }

    #[test]
    fn shutdown_report_publication_is_append_only_per_owner() {
        let first_owner = NEXT_WIN_EVENT_OWNER_ID.fetch_add(1, Ordering::Relaxed);
        let second_owner = NEXT_WIN_EVENT_OWNER_ID.fetch_add(1, Ordering::Relaxed);
        publish_win_event_shutdown_report(WinEventSubscriptionShutdownRecord {
            owner_id: first_owner,
            report: clean_shutdown_report(71),
        });
        publish_win_event_shutdown_report(WinEventSubscriptionShutdownRecord {
            owner_id: second_owner,
            report: clean_shutdown_report(72),
        });

        let history = win_event_shutdown_report_history();
        assert!(history.iter().any(|record| record.owner_id == first_owner));
        assert!(history.iter().any(|record| record.owner_id == second_owner));
    }

    #[test]
    fn subscription_slot_release_requires_the_exact_owner_id() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut state = WinEventDeliveryState {
            sender: Some(sender),
            subscription_owner_id: Some(91),
            last_released_owner_id: None,
        };

        assert!(release_win_event_subscription_slot_for_owner(&mut state, 92).is_err());
        assert_eq!(state.subscription_owner_id, Some(91));
        assert!(state.sender.is_some());

        if let Err(error) = release_win_event_subscription_slot_for_owner(&mut state, 91) {
            panic!("exact subscription-slot owner could not release: {error}");
        }
        assert_eq!(state.subscription_owner_id, None);
        assert_eq!(state.last_released_owner_id, Some(91));
        assert!(state.sender.is_none());
    }

    #[test]
    fn retained_owner_gate_counts_owner_only_and_sender_only_slots() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let owner_only = WinEventDeliveryState {
            sender: None,
            subscription_owner_id: Some(101),
            last_released_owner_id: None,
        };
        let sender_only = WinEventDeliveryState {
            sender: Some(sender),
            subscription_owner_id: None,
            last_released_owner_id: None,
        };
        let released = WinEventDeliveryState {
            sender: None,
            subscription_owner_id: None,
            last_released_owner_id: Some(100),
        };

        assert!(win_event_delivery_slot_is_active(&owner_only));
        assert!(win_event_delivery_slot_is_active(&sender_only));
        assert!(!win_event_delivery_slot_is_active(&released));
    }
}

use std::{collections::HashMap, time::Duration};

use serde::{Deserialize, Serialize};
use synapse_core::ElementId;
use tokio::sync::mpsc::UnboundedSender;

use crate::{A11yError, A11yResult, platform};

pub type AccessibleEventSender = UnboundedSender<AccessibleEvent>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessibleEvent {
    pub seq: u64,
    pub at_ms: u64,
    pub window_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<ElementId>,
    pub kind: AccessibleEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessibleEventKind {
    ForegroundChanged,
    FocusChanged,
    ValueChanged,
    NameChanged,
    ElementAppeared,
    ElementDisappeared,
    SelectionChanged,
    MenuStart,
    MenuEnd,
    Alert,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WinEventHookReadback {
    pub thread_id: u32,
    pub apartment: ComApartmentKind,
    pub hook_count: usize,
    pub event_ids: Vec<u32>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComApartmentKind {
    Sta,
    Mta,
    Neutral,
    MainSta,
    Unknown,
    Unsupported,
}

impl ComApartmentKind {
    #[must_use]
    pub const fn is_sta_family(self) -> bool {
        matches!(self, Self::Sta | Self::MainSta)
    }

    #[must_use]
    pub const fn is_mta(self) -> bool {
        matches!(self, Self::Mta)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiaWorkerReadback {
    pub thread_id: u32,
    pub apartment: ComApartmentKind,
    pub owned_window_count: usize,
}

impl UiaWorkerReadback {
    #[must_use]
    pub const fn is_mta_windowless(&self) -> bool {
        self.apartment.is_mta() && self.owned_window_count == 0
    }
}

pub struct WinEventSubscription {
    inner: platform::WinEventSubscription,
}

impl WinEventSubscription {
    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    pub fn readback(&self) -> &WinEventHookReadback {
        self.inner.readback()
    }

    /// Stops the exact WinEvent hook owner and returns physical cleanup
    /// readback. The wait is bounded; an owner that misses the deadline is
    /// retained by the platform until process teardown rather than detached.
    #[must_use]
    pub fn shutdown_checked(
        self,
        timeout: Duration,
        reason: &'static str,
    ) -> WinEventSubscriptionShutdownReport {
        #[cfg(windows)]
        {
            self.inner.shutdown_checked(timeout, reason)
        }
        #[cfg(not(windows))]
        {
            let _ = self;
            let _ = timeout;
            WinEventSubscriptionShutdownReport {
                reason,
                thread_id: 0,
                hook_count: 0,
                stop_requested: true,
                stop_wake_sent: true,
                sender_disconnected: true,
                subscription_slot_released: true,
                thread_owner_present: false,
                thread_terminal: true,
                thread_joined: true,
                thread_exit_report_received: false,
                unregister_attempted: 0,
                unregister_succeeded: 0,
                unregister_failed_event_ids: Vec::new(),
                exact_owner_retained: false,
                failures: Vec::new(),
            }
        }
    }
}

/// Physical shutdown readback for the process-wide WinEvent subscription.
///
/// `owners_quiescent` is deliberately stricter than "the stop request was
/// sent": every owned thread must be terminal and joined, every installed
/// hook must have a successful `UnhookWinEvent` readback, and the global
/// delivery slot must be released.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WinEventSubscriptionShutdownReport {
    pub reason: &'static str,
    pub thread_id: u32,
    pub hook_count: usize,
    pub stop_requested: bool,
    pub stop_wake_sent: bool,
    /// True only after the callback delivery-state lock was acquired within
    /// its shutdown bound and the process-global sender read back empty.
    pub sender_disconnected: bool,
    pub subscription_slot_released: bool,
    pub thread_owner_present: bool,
    pub thread_terminal: bool,
    pub thread_joined: bool,
    pub thread_exit_report_received: bool,
    pub unregister_attempted: usize,
    pub unregister_succeeded: usize,
    pub unregister_failed_event_ids: Vec<u32>,
    pub exact_owner_retained: bool,
    pub failures: Vec<String>,
}

/// Process-lifetime publication of one immutable WinEvent owner shutdown
/// report. `owner_id` is allocated independently of the recyclable Windows
/// thread ID, so separate owners remain distinguishable in global readback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WinEventSubscriptionShutdownRecord {
    pub owner_id: u64,
    pub report: WinEventSubscriptionShutdownReport,
}

impl WinEventSubscriptionShutdownReport {
    #[must_use]
    pub const fn unregister_complete(&self) -> bool {
        self.hook_count == 0
            || (self.thread_exit_report_received
                && self.unregister_attempted == self.hook_count
                && self.unregister_succeeded == self.hook_count
                && self.unregister_failed_event_ids.is_empty())
    }

    #[must_use]
    pub const fn owners_quiescent(&self) -> bool {
        self.sender_disconnected
            && self.subscription_slot_released
            && (!self.thread_owner_present || (self.thread_terminal && self.thread_joined))
            && self.unregister_complete()
            && !self.exact_owner_retained
    }

    /// Converts the structured physical readback into the daemon shutdown
    /// verdict while preserving every cleanup failure in the error detail.
    ///
    /// # Errors
    ///
    /// Returns an error if stop delivery, owner termination, hook
    /// unregistration, or global-slot release was not proven.
    pub fn verdict(&self) -> A11yResult<()> {
        if self.failures.is_empty()
            && self.stop_requested
            && self.stop_wake_sent
            && self.owners_quiescent()
        {
            Ok(())
        } else {
            Err(A11yError::internal(format!(
                "WinEvent subscription shutdown failed at {}: {}; readback={self:?}",
                self.reason,
                self.failures.join("; ")
            )))
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
struct EventKey {
    window_id: i64,
    kind: AccessibleEventKind,
    element_id_hash: u64,
}

impl EventKey {
    fn from_event(event: &AccessibleEvent) -> Self {
        use std::{
            collections::hash_map::DefaultHasher,
            hash::{Hash, Hasher},
        };

        let mut hasher = DefaultHasher::new();
        event.element_id.hash(&mut hasher);
        Self {
            window_id: event.window_id,
            kind: event.kind,
            element_id_hash: hasher.finish(),
        }
    }
}

#[must_use]
pub fn coalesce_events<I>(events: I, window: Duration) -> Vec<AccessibleEvent>
where
    I: IntoIterator<Item = AccessibleEvent>,
{
    let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
    let mut output = Vec::new();
    let mut pending: Option<AccessibleEvent> = None;

    for event in events {
        let Some(current) = pending.take() else {
            pending = Some(event);
            continue;
        };

        let same_key = EventKey::from_event(&current) == EventKey::from_event(&event);
        let within_window = event.at_ms.saturating_sub(current.at_ms) < window_ms;

        if !(same_key && within_window) {
            output.push(current);
        }
        pending = Some(event);
    }

    if let Some(event) = pending {
        output.push(event);
    }

    output
}

#[must_use]
pub fn debounce_value_changes<I>(events: I, window: Duration) -> Vec<AccessibleEvent>
where
    I: IntoIterator<Item = AccessibleEvent>,
{
    let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
    let mut output = Vec::new();
    let mut last_emitted = HashMap::<EventKey, u64>::new();
    let mut pending = HashMap::<EventKey, AccessibleEvent>::new();

    for event in events {
        if event.kind == AccessibleEventKind::FocusChanged {
            flush_pending(&mut pending, &mut output);
            output.push(event);
            continue;
        }

        if event.kind != AccessibleEventKind::ValueChanged {
            output.push(event);
            continue;
        }

        let key = EventKey::from_event(&event);
        match last_emitted.get(&key).copied() {
            Some(last_at) if event.at_ms.saturating_sub(last_at) < window_ms => {
                pending.insert(key, event);
            }
            _ => {
                pending.remove(&key);
                last_emitted.insert(key, event.at_ms);
                output.push(event);
            }
        }
    }

    flush_pending(&mut pending, &mut output);
    output.sort_by_key(|event| (event.at_ms, event.seq));
    output
}

fn flush_pending(
    pending: &mut HashMap<EventKey, AccessibleEvent>,
    output: &mut Vec<AccessibleEvent>,
) {
    output.extend(pending.drain().map(|(_key, event)| event));
}

/// Starts the dedicated `WinEvent` hook thread and marshals events into `sender`.
///
/// # Errors
///
/// Returns a structured UIA error when the hook thread cannot initialize, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn subscribe_win_events(sender: AccessibleEventSender) -> A11yResult<WinEventSubscription> {
    let inner = platform::subscribe_win_events(sender)?;
    Ok(WinEventSubscription { inner })
}

/// Returns a fail-closed count of unreconciled WinEvent ownership/evidence.
/// This includes retained thread records and an active, poisoned, or temporarily
/// unobservable process-global subscription slot, so startup-unwind failures
/// cannot disappear merely because no `JoinHandle` was created.
#[must_use]
pub fn retained_win_event_owner_count() -> usize {
    #[cfg(windows)]
    {
        platform::retained_live_owner_count()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Returns the process-lifetime, newest-first history of immutable WinEvent
/// owner shutdown reports. Reports are append-only so cancellation or a later
/// clean owner cannot erase an earlier nonquiescent result.
#[must_use]
pub fn win_event_shutdown_report_history() -> Vec<WinEventSubscriptionShutdownRecord> {
    #[cfg(windows)]
    {
        platform::WinEventSubscription::shutdown_report_history()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Returns a live readback from the dedicated UI Automation client worker.
///
/// # Errors
///
/// Returns a structured accessibility error when the worker cannot initialize,
/// or `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn uia_worker_readback() -> A11yResult<UiaWorkerReadback> {
    platform::uia_worker_readback()
}

#[cfg(test)]
mod shutdown_report_tests {
    use super::WinEventSubscriptionShutdownReport;

    fn clean_report() -> WinEventSubscriptionShutdownReport {
        WinEventSubscriptionShutdownReport {
            reason: "test",
            thread_id: 17,
            hook_count: 2,
            stop_requested: true,
            stop_wake_sent: true,
            sender_disconnected: true,
            subscription_slot_released: true,
            thread_owner_present: true,
            thread_terminal: true,
            thread_joined: true,
            thread_exit_report_received: true,
            unregister_attempted: 2,
            unregister_succeeded: 2,
            unregister_failed_event_ids: Vec::new(),
            exact_owner_retained: false,
            failures: Vec::new(),
        }
    }

    #[test]
    fn live_win_event_owner_never_reports_quiescent() {
        let mut report = clean_report();
        report.thread_terminal = false;
        report.thread_joined = false;
        report.exact_owner_retained = true;

        assert!(!report.owners_quiescent());
        assert!(report.verdict().is_err());
    }

    #[test]
    fn incomplete_unregistration_never_reports_quiescent() {
        let mut report = clean_report();
        report.unregister_succeeded = 1;
        report.unregister_failed_event_ids.push(0x8005);

        assert!(!report.unregister_complete());
        assert!(!report.owners_quiescent());
        assert!(report.verdict().is_err());
    }

    #[test]
    fn complete_physical_readback_is_quiescent() {
        let report = clean_report();

        assert!(report.unregister_complete());
        assert!(report.owners_quiescent());
        assert!(report.verdict().is_ok());
    }

    #[test]
    fn missing_thread_owner_does_not_bypass_nonzero_hook_evidence() {
        let mut report = clean_report();
        report.thread_owner_present = false;
        report.thread_exit_report_received = false;
        report.unregister_attempted = 0;
        report.unregister_succeeded = 0;

        assert!(!report.unregister_complete());
        assert!(!report.owners_quiescent());
        assert!(report.verdict().is_err());
    }

    #[test]
    fn unobserved_bounded_sender_disconnect_fails_the_physical_verdict() {
        let mut report = clean_report();
        report.sender_disconnected = false;
        report.failures.push(
            "WinEvent delivery-state lock remained contended for the bounded disconnect".to_owned(),
        );

        assert!(!report.owners_quiescent());
        assert!(report.verdict().is_err());
    }
}

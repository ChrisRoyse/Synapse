use std::{
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use chrono::Utc;
use serde_json::json;
use synapse_a11y::{
    AccessibleEvent, AccessibleEventKind, WinEventSubscription, WinEventSubscriptionShutdownReport,
};
use synapse_core::{Event, EventFilter, EventSource, ForegroundContext};
use synapse_reflex::EventBus;
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};

use super::activity_recorder::ActivityRecorder;

pub struct A11yEventBridge {
    subscription: Option<WinEventSubscription>,
    task: Option<JoinHandle<()>>,
}

pub(crate) struct PreparedA11yEventBridge {
    subscription: WinEventSubscription,
    receiver: UnboundedReceiver<AccessibleEvent>,
}

#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "shutdown readback carries independent OS and Tokio owner postconditions"
)]
pub(crate) struct A11yEventBridgeShutdownReport {
    pub(crate) subscription_owner_present: bool,
    pub(crate) subscription: Option<WinEventSubscriptionShutdownReport>,
    pub(crate) task_owner_present: bool,
    pub(crate) task_terminal: bool,
    pub(crate) task_joined: bool,
    pub(crate) abort_requested: bool,
    pub(crate) exact_task_owner_retained: bool,
    pub(crate) retained_task_owner_count: usize,
    pub(crate) retained_subscription_owner_count: usize,
    /// Wall-clock anchor (RFC3339 UTC) for the start of the bridge shutdown.
    /// Each `*_elapsed_ms` phase field is measured relative to its own phase
    /// boundary; the anchor plus the cumulative elapsed reconstructs every
    /// phase's start/end for `synapse.log` correlation.
    pub(crate) shutdown_started_at: String,
    /// Time spent detaching the OS WinEvent owner (hook-thread join + hook
    /// unregister + sender disconnect). Large values indicate subscription
    /// thread delay rather than a Tokio scheduling problem.
    pub(crate) subscription_shutdown_elapsed_ms: u64,
    /// Time the cooperative-stop wait actually consumed. Equal to the graceful
    /// deadline when it times out; much smaller when the task joins in time.
    pub(crate) graceful_join_elapsed_ms: u64,
    pub(crate) graceful_join_timed_out: bool,
    /// Time the post-abort join consumed, present only when the graceful
    /// deadline was missed and an abort was issued.
    pub(crate) abort_join_elapsed_ms: Option<u64>,
    /// Scheduler responsiveness sampled concurrently with the graceful-stop
    /// wait. High `scheduler_probe_max_latency_ms` (or `scheduler_probe_samples
    /// == 0`) during a missed deadline is dispositive evidence of runtime-worker
    /// starvation; low probe latency with a missed deadline points instead at a
    /// genuinely stuck bridge task.
    pub(crate) scheduler_probe_samples: u32,
    pub(crate) scheduler_probe_max_latency_ms: u64,
    pub(crate) scheduler_probe_last_latency_ms: u64,
    pub(crate) failures: Vec<String>,
}

impl A11yEventBridgeShutdownReport {
    pub(crate) const fn owners_quiescent(&self) -> bool {
        self.subscription_owner_present
            && match self.subscription.as_ref() {
                Some(report) => report.owners_quiescent(),
                None => false,
            }
            && self.task_owner_present
            && self.task_terminal
            && self.task_joined
            && !self.exact_task_owner_retained
            && self.retained_task_owner_count == 0
            && self.retained_subscription_owner_count == 0
    }

    /// Emits the structured, greppable phase-timing/scheduling record. Reading
    /// every phase field here keeps the evidence a first-class log payload (not
    /// merely `Debug` filler) and gives the diagnostic a single stable log code.
    fn emit_phase_timing(&self) {
        let subscription_sender_disconnected = self
            .subscription
            .as_ref()
            .map(|report| report.sender_disconnected);
        tracing::info!(
            code = "M3_A11Y_BRIDGE_SHUTDOWN_PHASE_TIMING",
            shutdown_started_at = %self.shutdown_started_at,
            subscription_shutdown_elapsed_ms = self.subscription_shutdown_elapsed_ms,
            subscription_sender_disconnected = ?subscription_sender_disconnected,
            graceful_join_elapsed_ms = self.graceful_join_elapsed_ms,
            graceful_join_timed_out = self.graceful_join_timed_out,
            abort_join_elapsed_ms = ?self.abort_join_elapsed_ms,
            scheduler_probe_samples = self.scheduler_probe_samples,
            scheduler_probe_max_latency_ms = self.scheduler_probe_max_latency_ms,
            scheduler_probe_last_latency_ms = self.scheduler_probe_last_latency_ms,
            task_terminal = self.task_terminal,
            task_joined = self.task_joined,
            abort_requested = self.abort_requested,
            exact_task_owner_retained = self.exact_task_owner_retained,
            "M3 a11y bridge shutdown phase timing"
        );
    }

    pub(crate) fn verdict(&self) -> anyhow::Result<()> {
        let subscription_verdict = self
            .subscription
            .as_ref()
            .map(WinEventSubscriptionShutdownReport::verdict);
        let subscription_ok = subscription_verdict
            .as_ref()
            .is_some_and(|result| result.is_ok());
        if self.failures.is_empty()
            && subscription_ok
            && self.owners_quiescent()
            && !self.abort_requested
        {
            Ok(())
        } else {
            let mut failures = self.failures.clone();
            match subscription_verdict {
                Some(Err(error)) => failures.push(format!("WinEvent subscription: {error}")),
                None => failures.push("WinEvent subscription readback was missing".to_owned()),
                Some(Ok(())) => {}
            }
            anyhow::bail!(
                "a11y event bridge shutdown failed: {}; readback={self:?}",
                failures.join("; ")
            )
        }
    }
}

const A11Y_SUBSCRIPTION_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const A11Y_BRIDGE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const A11Y_BRIDGE_ABORT_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Cadence at which the shutdown path round-trips a trivial task through the
/// Tokio scheduler while it waits for the bridge task's cooperative deadline.
/// This is a sampling interval, not a deadline: it never extends any shutdown
/// bound. Each probe's round-trip latency tracks the exact delay that would
/// keep the (already channel-closed) bridge task from being polled to
/// completion, so the failure record can distinguish runtime-worker
/// starvation (#1798) from a genuinely stuck bridge task.
const A11Y_SCHEDULER_PROBE_INTERVAL: Duration = Duration::from_millis(250);

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Structured record of Tokio scheduler responsiveness sampled concurrently
/// with the bridge task's cooperative-stop wait. `samples == 0` over a full
/// graceful window is itself dispositive evidence of total worker starvation
/// (the sampler task could not be scheduled at all).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct A11ySchedulerResponsivenessProbe {
    pub(crate) samples: u32,
    pub(crate) max_latency_ms: u64,
    pub(crate) last_latency_ms: u64,
}

impl A11ySchedulerResponsivenessProbe {
    fn record(&mut self, latency: Duration) {
        let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
        self.samples = self.samples.saturating_add(1);
        self.last_latency_ms = latency_ms;
        self.max_latency_ms = self.max_latency_ms.max(latency_ms);
    }
}

/// Continuously round-trips an empty task through the scheduler until `budget`
/// elapses, recording per-probe latency into `out`. When the runtime is
/// healthy each round-trip is sub-millisecond; under worker starvation the
/// spawned probe waits in the run queue behind non-yielding maintenance work,
/// so the recorded latency equals the scheduling delay that starves the bridge
/// task. Cancelled early (dropped) the moment the bridge task joins.
async fn sample_scheduler_responsiveness(
    out: &mut A11ySchedulerResponsivenessProbe,
    budget: Duration,
) {
    let deadline = Instant::now() + budget;
    loop {
        let probe_start = Instant::now();
        // Round-trip latency of a trivial task is the physical scheduling delay.
        let _ = tokio::spawn(async {}).await;
        out.record(probe_start.elapsed());
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(remaining.min(A11Y_SCHEDULER_PROBE_INTERVAL)).await;
    }
}

static RETAINED_A11Y_BRIDGE_TASKS: OnceLock<Mutex<Vec<JoinHandle<()>>>> = OnceLock::new();

struct BridgeTaskShutdownOwner {
    task: Option<JoinHandle<()>>,
}

impl BridgeTaskShutdownOwner {
    const fn new(task: JoinHandle<()>) -> Self {
        Self { task: Some(task) }
    }

    fn task_mut(&mut self) -> &mut JoinHandle<()> {
        let Some(task) = self.task.as_mut() else {
            panic!("bridge shutdown owner must contain its task");
        };
        task
    }

    fn take_terminal(&mut self) {
        drop(self.task.take());
    }

    fn abort_and_retain(&mut self, reason: &'static str) {
        let Some(task) = self.task.take() else {
            return;
        };
        task.abort();
        retain_bridge_task(task);
        tracing::error!(
            code = "M3_A11Y_BRIDGE_TASK_RETAINED",
            reason,
            "exact a11y bridge JoinHandle retained until process teardown"
        );
    }
}

impl Drop for BridgeTaskShutdownOwner {
    fn drop(&mut self) {
        self.abort_and_retain("shutdown_future_cancelled");
    }
}

fn retain_bridge_task(task: JoinHandle<()>) {
    let tasks = RETAINED_A11Y_BRIDGE_TASKS.get_or_init(|| Mutex::new(Vec::new()));
    match tasks.lock() {
        Ok(mut tasks) => tasks.push(task),
        Err(poisoned) => poisoned.into_inner().push(task),
    }
}

pub(crate) fn retained_live_owner_count() -> usize {
    let tasks = RETAINED_A11Y_BRIDGE_TASKS.get_or_init(|| Mutex::new(Vec::new()));
    let tasks = match tasks.lock() {
        Ok(tasks) => tasks,
        Err(poisoned) => poisoned.into_inner(),
    };
    tasks.iter().filter(|task| !task.is_finished()).count()
}

const A11Y_EVENT_KINDS: [&str; 10] = [
    "foreground-changed",
    "focus-changed",
    "value-changed",
    "name-changed",
    "element-appeared",
    "element-disappeared",
    "selection-changed",
    "menustart",
    "menuend",
    "alert",
];

pub fn kinds_require_a11y_bridge(kinds: &[String]) -> bool {
    kinds.iter().any(|kind| is_a11y_event_kind(kind))
}

pub fn event_filter_requires_a11y_bridge(filter: &EventFilter) -> bool {
    match filter {
        EventFilter::Source { source } => matches!(
            source,
            EventSource::A11yUia | EventSource::A11yWinEvent | EventSource::A11yCdp
        ),
        EventFilter::Kind { kind } => is_a11y_event_kind(kind),
        EventFilter::And { args } | EventFilter::Or { args } => {
            args.iter().any(event_filter_requires_a11y_bridge)
        }
        EventFilter::All
        | EventFilter::None
        | EventFilter::Not { .. }
        | EventFilter::Data { .. } => false,
    }
}

pub fn is_a11y_event_kind(kind: &str) -> bool {
    let normalized = kind.trim().replace('_', "-").to_ascii_lowercase();
    A11Y_EVENT_KINDS.contains(&normalized.as_str())
}

impl A11yEventBridge {
    pub(crate) fn prepare() -> synapse_a11y::A11yResult<PreparedA11yEventBridge> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let subscription = synapse_a11y::subscribe_win_events(sender)?;
        Ok(PreparedA11yEventBridge {
            subscription,
            receiver,
        })
    }

    pub(crate) fn start_prepared(
        prepared: PreparedA11yEventBridge,
        event_bus: EventBus,
        activity_recorder: Option<Arc<ActivityRecorder>>,
    ) -> Self {
        let PreparedA11yEventBridge {
            subscription,
            receiver,
        } = prepared;
        let recorder_attached = activity_recorder.is_some();
        let task = tokio::spawn(run_bridge(event_bus, receiver, activity_recorder));
        tracing::info!(
            code = "M3_A11Y_EVENT_BRIDGE_STARTED",
            thread_id = subscription.readback().thread_id,
            hook_count = subscription.readback().hook_count,
            event_ids = ?subscription.readback().event_ids,
            recorder_attached,
            "M3 a11y WinEvent bridge started"
        );
        Self {
            subscription: Some(subscription),
            task: Some(task),
        }
    }

    pub(crate) fn start(
        event_bus: EventBus,
        activity_recorder: Option<Arc<ActivityRecorder>>,
    ) -> synapse_a11y::A11yResult<Self> {
        let prepared = Self::prepare()?;
        Ok(Self::start_prepared(prepared, event_bus, activity_recorder))
    }

    pub(crate) async fn shutdown(mut self) -> A11yEventBridgeShutdownReport {
        // Stop the OS source first. This operation is synchronous but bounded,
        // so the async shutdown future cannot be cancelled between detaching
        // the WinEvent owner and recording its physical unregister readback.
        let mut failures = Vec::new();
        let shutdown_started_at = Utc::now().to_rfc3339();
        let subscription_owner_present = self.subscription.is_some();
        let subscription_phase_start = Instant::now();
        let subscription = self.subscription.take().map(|subscription| {
            subscription
                .shutdown_checked(A11Y_SUBSCRIPTION_STOP_TIMEOUT, "a11y_event_bridge_shutdown")
        });
        let subscription_shutdown_elapsed_ms = elapsed_ms(subscription_phase_start);
        if !subscription_owner_present {
            failures.push("a11y bridge was missing its WinEvent subscription owner".to_owned());
        }

        let task_owner_present = self.task.is_some();
        let Some(task) = self.task.take() else {
            failures.push("a11y bridge was missing its async task owner".to_owned());
            return A11yEventBridgeShutdownReport {
                subscription_owner_present,
                subscription,
                task_owner_present,
                task_terminal: false,
                task_joined: false,
                abort_requested: false,
                exact_task_owner_retained: false,
                retained_task_owner_count: retained_live_owner_count(),
                retained_subscription_owner_count: synapse_a11y::retained_win_event_owner_count(),
                shutdown_started_at,
                subscription_shutdown_elapsed_ms,
                graceful_join_elapsed_ms: 0,
                graceful_join_timed_out: false,
                abort_join_elapsed_ms: None,
                scheduler_probe_samples: 0,
                scheduler_probe_max_latency_ms: 0,
                scheduler_probe_last_latency_ms: 0,
                failures,
            };
        };
        let mut task_owner = BridgeTaskShutdownOwner::new(task);
        // Sample scheduler responsiveness concurrently with the cooperative-stop
        // wait. The subscription is already terminal above (sender disconnected),
        // so a healthy runtime polls the bridge task's `recv() -> None` path in
        // microseconds. If this wait ever consumes the whole deadline, the probe
        // samples taken during exactly this window reveal whether the runtime
        // was starved (probe latency high / no samples) or the task was stuck.
        let graceful_phase_start = Instant::now();
        let mut scheduler_probe = A11ySchedulerResponsivenessProbe::default();
        let graceful_join = {
            // The sampler runs for strictly longer than the cooperative deadline
            // so the `timeout` branch is always the authority that ends this
            // wait: the probe only observes, it never shortens the deadline.
            let sampler = sample_scheduler_responsiveness(
                &mut scheduler_probe,
                A11Y_BRIDGE_STOP_TIMEOUT.saturating_add(A11Y_BRIDGE_ABORT_JOIN_TIMEOUT),
            );
            tokio::pin!(sampler);
            tokio::select! {
                biased;
                joined = tokio::time::timeout(A11Y_BRIDGE_STOP_TIMEOUT, task_owner.task_mut()) => {
                    joined.map_err(|_elapsed| ())
                }
                () = &mut sampler => Err(()),
            }
        };
        let graceful_join_elapsed_ms = elapsed_ms(graceful_phase_start);
        let mut graceful_join_timed_out = false;
        let mut abort_join_elapsed_ms = None;
        let (task_terminal, task_joined, abort_requested, exact_task_owner_retained) =
            match graceful_join {
                Ok(Ok(())) => {
                    task_owner.take_terminal();
                    (true, true, false, false)
                }
                Ok(Err(error)) => {
                    failures.push(format!("a11y bridge task join failed: {error}"));
                    task_owner.take_terminal();
                    (true, true, false, false)
                }
                Err(()) => {
                    graceful_join_timed_out = true;
                    task_owner.task_mut().abort();
                    let abort_phase_start = Instant::now();
                    let abort_join =
                        tokio::time::timeout(A11Y_BRIDGE_ABORT_JOIN_TIMEOUT, task_owner.task_mut())
                            .await;
                    abort_join_elapsed_ms = Some(elapsed_ms(abort_phase_start));
                    match abort_join {
                        Ok(result) => {
                            failures.push(format!(
                                "a11y bridge did not stop cooperatively; abort_join={result:?}"
                            ));
                            task_owner.take_terminal();
                            (true, true, true, false)
                        }
                        Err(_elapsed) => {
                            task_owner.abort_and_retain("abort_join_timeout");
                            failures.push(
                            "a11y bridge did not reach a terminal join after abort; exact JoinHandle retained until process teardown".to_owned(),
                        );
                            (false, false, true, true)
                        }
                    }
                }
            };
        let retained_task_owner_count = retained_live_owner_count();
        let retained_subscription_owner_count = synapse_a11y::retained_win_event_owner_count();
        if retained_task_owner_count != 0 {
            failures.push(format!(
                "{retained_task_owner_count} retained a11y bridge task owner(s) remain physically live"
            ));
        }
        if retained_subscription_owner_count != 0 {
            failures.push(format!(
                "{retained_subscription_owner_count} retained WinEvent subscription owner(s) remain physically live"
            ));
        }
        let report = A11yEventBridgeShutdownReport {
            subscription_owner_present,
            subscription,
            task_owner_present,
            task_terminal,
            task_joined,
            abort_requested,
            exact_task_owner_retained,
            retained_task_owner_count,
            retained_subscription_owner_count,
            shutdown_started_at,
            subscription_shutdown_elapsed_ms,
            graceful_join_elapsed_ms,
            graceful_join_timed_out,
            abort_join_elapsed_ms,
            scheduler_probe_samples: scheduler_probe.samples,
            scheduler_probe_max_latency_ms: scheduler_probe.max_latency_ms,
            scheduler_probe_last_latency_ms: scheduler_probe.last_latency_ms,
            failures,
        };
        // Emit the phase-specific timing/scheduling evidence unconditionally so
        // both the happy path (FSV read) and the failing path leave a greppable,
        // structured record that distinguishes subscription-thread delay,
        // channel closure, scheduling starvation, and join ordering without
        // reasoning from wall-clock diffs across separate log lines.
        report.emit_phase_timing();
        report
    }
}

impl fmt::Debug for A11yEventBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("A11yEventBridge")
            .field(
                "running",
                &self.task.as_ref().is_some_and(|task| !task.is_finished()),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for A11yEventBridge {
    fn drop(&mut self) {
        if let Some(subscription) = self.subscription.take() {
            let report = subscription.shutdown_checked(
                Duration::from_millis(500),
                "a11y_event_bridge_drop_backstop",
            );
            if report.verdict().is_err() {
                tracing::error!(
                    code = "M3_A11Y_BRIDGE_SUBSCRIPTION_DROP_INCOMPLETE",
                    report = ?report,
                    "bounded a11y bridge drop could not prove WinEvent owner cleanup"
                );
            }
        }
        if let Some(task) = self.task.take() {
            let mut owner = BridgeTaskShutdownOwner::new(task);
            owner.abort_and_retain("a11y_event_bridge_drop_backstop");
        }
    }
}

async fn run_bridge(
    event_bus: EventBus,
    mut receiver: UnboundedReceiver<AccessibleEvent>,
    activity_recorder: Option<Arc<ActivityRecorder>>,
) {
    let mut next_seq = 1_u64;
    while let Some(accessible_event) = receiver.recv().await {
        if let Some(recorder) = &activity_recorder {
            recorder.record_accessible_event(&accessible_event);
        }
        let event = event_from_accessible(&accessible_event, next_seq);
        next_seq = next_seq.saturating_add(1);
        let report = event_bus.publish(event.clone());
        tracing::debug!(
            code = "M3_A11Y_EVENT_PUBLISHED",
            seq = event.seq,
            kind = %event.kind,
            window_title = %event
                .data
                .get("window_title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
            matched = report.matched,
            queued = report.queued,
            dropped = report.dropped,
            "M3 a11y event published"
        );
    }
    tracing::info!(
        code = "M3_A11Y_EVENT_BRIDGE_STOPPED",
        "M3 a11y WinEvent bridge stopped"
    );
}

fn event_from_accessible(accessible_event: &AccessibleEvent, seq: u64) -> Event {
    let foreground = event_foreground_context(accessible_event);
    let element_id = accessible_event
        .element_id
        .as_ref()
        .map(ToString::to_string);
    let window_title = foreground
        .as_ref()
        .map(|context| context.window_title.clone())
        .unwrap_or_default();
    let process_name = foreground
        .as_ref()
        .map(|context| context.process_name.clone())
        .unwrap_or_default();
    let pid = foreground.as_ref().map_or(0, |context| context.pid);
    let foreground_window_id = foreground.as_ref().map(|context| context.hwnd);

    Event {
        seq,
        at: Utc::now(),
        source: EventSource::A11yWinEvent,
        kind: event_kind_name(accessible_event.kind).to_owned(),
        data: json!({
            "window_id": accessible_event.window_id,
            "foreground_window_id": foreground_window_id,
            "element_id": element_id,
            "event_kind": accessible_event.kind,
            "window_title": window_title,
            "process_name": process_name,
            "pid": pid,
            "name": accessible_event.name.clone(),
            "value": accessible_event.value.clone(),
            "win_event_seq": accessible_event.seq,
            "win_event_at_ms": accessible_event.at_ms,
        }),
        correlations: Vec::new(),
    }
}

fn event_foreground_context(accessible_event: &AccessibleEvent) -> Option<ForegroundContext> {
    let event_context = synapse_a11y::foreground_context(accessible_event.window_id).ok();
    if event_context
        .as_ref()
        .is_some_and(|context| !context.window_title.trim().is_empty())
    {
        return event_context;
    }

    synapse_a11y::current_foreground_context()
        .ok()
        .or(event_context)
}

const fn event_kind_name(kind: AccessibleEventKind) -> &'static str {
    match kind {
        AccessibleEventKind::ForegroundChanged => "foreground-changed",
        AccessibleEventKind::FocusChanged => "focus-changed",
        AccessibleEventKind::ValueChanged => "value-changed",
        AccessibleEventKind::NameChanged => "name-changed",
        AccessibleEventKind::ElementAppeared => "element-appeared",
        AccessibleEventKind::ElementDisappeared => "element-disappeared",
        AccessibleEventKind::SelectionChanged => "selection-changed",
        AccessibleEventKind::MenuStart => "menustart",
        AccessibleEventKind::MenuEnd => "menuend",
        AccessibleEventKind::Alert => "alert",
    }
}

//! Always-on operator-activity recorder (#837, epic #829).
//!
//! Consumes the daemon's single WinEvent hook stream — teed from
//! [`super::a11y_events::A11yEventBridge`], because the process-wide
//! `WIN_EVENT_SENDER` permits exactly one hook subscription — and persists
//! `CF_TIMELINE` rows: foreground app switches, foreground window title
//! changes, idle/active transitions (`GetLastInputInfo` polled at a coarse
//! interval), and recorder session boundaries.
//!
//! Design constraints carried from ADR 2026-06-11-timeline-data-model and
//! field-tested foreground-tracking practice:
//!
//! - WinEvents are *triggers*, not truth. `EVENT_SYSTEM_FOREGROUND` is
//!   delivered asynchronously and frequently names an invisible Alt-Tab
//!   staging window (`ForegroundStaging`), a window not yet shown, or one
//!   already destroyed. When the event hwnd is unusable the recorder
//!   re-reads `GetForegroundWindow` — the source of truth — so a real app
//!   switch hiding behind a transient event still lands in the timeline.
//! - Every idle poll tick also reconciles recorded foreground state against
//!   the real foreground (rows tagged `source: "poll"`), so a missed
//!   WinEvent can never desync the timeline for longer than one interval.
//! - `EVENT_OBJECT_NAMECHANGE` fires for child objects too; a title row is
//!   written only when the *foreground* window's title actually changed.
//! - Idle detection mirrors ActivityWatch's defaults (180 s timeout, coarse
//!   poll); the `idle_start` row is backdated to the last-input instant so
//!   the timeline reflects when input actually stopped.
//!
//! Attribution: rows produced while an agent session holds the real-input
//! lease are tagged `agent { session_id }` (the lease is the canonical "an
//! agent owns the foreground" signal, epic #719); everything else is `human`.

use std::{
    collections::{HashMap, VecDeque},
    panic::AssertUnwindSafe,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

type RetainedRecorderTaskOwner = (&'static str, JoinHandle<()>);

static RETAINED_RECORDER_TASK_OWNERS: OnceLock<Mutex<Vec<RetainedRecorderTaskOwner>>> =
    OnceLock::new();
static UNRESOLVED_RECORDER_DROP_PRODUCERS: AtomicU64 = AtomicU64::new(0);

use anyhow::{Context, Result, bail};
use chrono::Utc;
use futures_util::{FutureExt, future::join_all};
use serde_json::json;
use sha2::{Digest, Sha256};
use synapse_a11y::{AccessibleEvent, AccessibleEventKind};
use synapse_core::{
    Event, EventSource, SCHEMA_VERSION, StoredEvent,
    types::{
        AccessibleNode, FsEventKind, Observation, TIMELINE_RECORD_VERSION, TimelineActor,
        TimelineKind, TimelineRecord,
    },
};
use synapse_reflex::EventBus;
use synapse_storage::{Db, cf, timeline::timeline_key};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    demo_recording::DemoRecordControl,
    interaction_cadence::{
        InteractionEvent, InteractionEventKind, InteractionHook, InteractionKeySignal,
    },
    timeline_control::{RecorderControl, SuppressReason},
};
use crate::m1::{
    ClipboardTimelineSample, FsTimelineEvent, timeline_clipboard_enabled,
    timeline_file_activity_enabled,
};
use crate::server::url_redaction::{
    redact_url_fields_for_public_readback, redact_url_for_public_readback,
    redact_url_opt_for_public_readback,
};

/// Idle threshold override, in milliseconds. Default mirrors ActivityWatch.
pub const IDLE_TIMEOUT_ENV: &str = "SYNAPSE_TIMELINE_IDLE_TIMEOUT_MS";
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 180_000;
const MIN_IDLE_POLL_INTERVAL_MS: u64 = 250;
const MAX_IDLE_POLL_INTERVAL_MS: u64 = 5_000;
const RECORDER_TASK_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const RECORDER_TASK_ABORT_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
const RECORDER_PRODUCER_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
const RECORDER_PRODUCER_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(10);
const RECORDER_INTERACTION_HOOK_STOP_TIMEOUT: Duration = Duration::from_secs(5);
// The supervisor must outlive every legal inner stage: producer admission
// drain (15s), hook stop (5s), producer stop+abort join (7s), and worker
// stop+abort join (7s). Leave bounded headroom for checked storage readback.
const RECORDER_SHUTDOWN_SUPERVISOR_TIMEOUT: Duration = Duration::from_secs(45);
const RECORDER_SHUTDOWN_SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ASSIST_EVENT_KIND: &str = "assist.opportunity";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecorderConfig {
    pub idle_timeout_ms: u64,
    pub idle_poll_interval_ms: u64,
    interaction_hook_enabled: bool,
    assist: AssistDetectorConfig,
}

impl RecorderConfig {
    /// Reads `SYNAPSE_TIMELINE_IDLE_TIMEOUT_MS` and derives the poll cadence.
    ///
    /// # Errors
    ///
    /// Returns an error when the variable is set but is not a positive
    /// integer; the daemon must refuse to start rather than record with a
    /// silently-wrong idle policy.
    pub fn from_env() -> Result<Self> {
        Self::from_raw(std::env::var(IDLE_TIMEOUT_ENV).ok().as_deref())
    }

    fn from_raw(raw: Option<&str>) -> Result<Self> {
        let idle_timeout_ms = match raw {
            None => DEFAULT_IDLE_TIMEOUT_MS,
            Some(value) => value.trim().parse::<u64>().with_context(|| {
                format!(
                    "{IDLE_TIMEOUT_ENV} must be a positive integer of milliseconds, got {value:?}"
                )
            })?,
        };
        if idle_timeout_ms == 0 {
            bail!("{IDLE_TIMEOUT_ENV} must be at least 1 millisecond, got 0");
        }
        let idle_poll_interval_ms = (idle_timeout_ms / 4)
            .clamp(MIN_IDLE_POLL_INTERVAL_MS, MAX_IDLE_POLL_INTERVAL_MS)
            .min(idle_timeout_ms);
        Ok(Self {
            idle_timeout_ms,
            idle_poll_interval_ms,
            interaction_hook_enabled: true,
            assist: AssistDetectorConfig::from_env()?,
        })
    }
}

enum RecorderMessage {
    Accessible(AccessibleEvent),
    Interaction(InteractionEvent),
    IdleProbe {
        idle_ms: u64,
    },
    FlushInteractions {
        done: oneshot::Sender<()>,
    },
    Shutdown {
        done: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Debug, Default)]
struct RecorderProducerGate {
    state: Mutex<RecorderProducerGateState>,
    quiescent: Condvar,
}

#[derive(Debug, Default)]
struct RecorderProducerGateState {
    closed: bool,
    in_flight: usize,
}

struct RecorderProducerPermit<'a> {
    gate: &'a RecorderProducerGate,
}

impl RecorderProducerGate {
    fn enter(&self) -> Option<RecorderProducerPermit<'_>> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.closed {
            return None;
        }
        state.in_flight = state.in_flight.saturating_add(1);
        Some(RecorderProducerPermit { gate: self })
    }

    fn close(&self) -> (bool, usize) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.closed = true;
        (state.closed, state.in_flight)
    }

    fn readback(&self) -> (bool, usize) {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        (state.closed, state.in_flight)
    }

    async fn wait_for_quiescence_async(&self, timeout: Duration) -> (bool, usize) {
        let readback = self.readback();
        if readback.1 == 0 {
            return readback;
        }
        let wait_for_quiescence = async {
            loop {
                let readback = self.readback();
                if readback.1 == 0 {
                    return readback;
                }
                tokio::time::sleep(RECORDER_PRODUCER_DRAIN_POLL_INTERVAL).await;
            }
        };
        match tokio::time::timeout(timeout, wait_for_quiescence).await {
            Ok(readback) => readback,
            Err(_elapsed) => self.readback(),
        }
    }
}

impl Drop for RecorderProducerPermit<'_> {
    fn drop(&mut self) {
        let mut state = match self.gate.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.in_flight = state.in_flight.saturating_sub(1);
        if state.in_flight == 0 {
            self.gate.quiescent.notify_all();
        }
    }
}

struct RecorderTaskShutdownOwner {
    name: &'static str,
    task: Option<JoinHandle<()>>,
}

impl RecorderTaskShutdownOwner {
    const fn new(name: &'static str, task: JoinHandle<()>) -> Self {
        Self {
            name,
            task: Some(task),
        }
    }

    fn task_mut(&mut self) -> &mut JoinHandle<()> {
        let Some(task) = self.task.as_mut() else {
            panic!("recorder shutdown owner must contain its task");
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
        retain_recorder_task_owner(self.name, task);
        tracing::error!(
            code = "TIMELINE_RECORDER_TASK_RETAINED",
            task = self.name,
            reason,
            "exact activity-recorder JoinHandle retained until physical termination"
        );
    }
}

impl Drop for RecorderTaskShutdownOwner {
    fn drop(&mut self) {
        self.abort_and_retain("shutdown_future_cancelled_or_unwound");
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ActivityRecorderTaskDrainReport {
    tasks_before: usize,
    graceful_joined: usize,
    abort_requests_sent: usize,
    joined_after_abort: usize,
    still_live_task_names: Vec<&'static str>,
    failures: Vec<String>,
}

impl ActivityRecorderTaskDrainReport {
    pub(crate) fn owners_quiescent(&self) -> bool {
        self.still_live_task_names.is_empty()
    }

    fn verdict(&self) -> anyhow::Result<()> {
        let accounted_tasks =
            self.graceful_joined + self.joined_after_abort + self.still_live_task_names.len();
        let accounted_aborts = self.joined_after_abort + self.still_live_task_names.len();
        if self.failures.is_empty()
            && self.owners_quiescent()
            && self.tasks_before == accounted_tasks
            && self.abort_requests_sent == accounted_aborts
        {
            Ok(())
        } else {
            anyhow::bail!("activity recorder task drain failed; readback={self:?}")
        }
    }

    fn merge(mut self, mut other: Self) -> Self {
        self.tasks_before += other.tasks_before;
        self.graceful_joined += other.graceful_joined;
        self.abort_requests_sent += other.abort_requests_sent;
        self.joined_after_abort += other.joined_after_abort;
        self.still_live_task_names
            .append(&mut other.still_live_task_names);
        self.failures.append(&mut other.failures);
        self
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ActivityRecorderShutdownReport {
    shutdown_message_delivered: bool,
    shutdown_reply_received: bool,
    worker_boundary_committed: bool,
    fallback_attempted: bool,
    fallback_committed: bool,
    producer_gate_closed: bool,
    producer_gate_in_flight: usize,
    pipeline_task_owners_remaining: usize,
    task_drain: ActivityRecorderTaskDrainReport,
    owner_accounting_complete: bool,
    retained_task_owners: usize,
    interaction_hook_owners_quiescent: bool,
    rows_written: u64,
    write_failures: u64,
    failures: Vec<String>,
}

impl ActivityRecorderShutdownReport {
    pub(crate) fn owners_quiescent(&self) -> bool {
        self.owner_accounting_complete
            && self.producer_gate_closed
            && self.producer_gate_in_flight == 0
            && self.pipeline_task_owners_remaining == 0
            && self.retained_task_owners == 0
            && self.interaction_hook_owners_quiescent
    }

    pub(crate) fn verdict(&self) -> anyhow::Result<()> {
        let mut failures = self.failures.clone();
        if !self.shutdown_message_delivered {
            failures.push("shutdown message was not delivered to the recorder worker".to_owned());
        }
        if !self.shutdown_reply_received {
            failures.push("recorder worker did not reply to the shutdown request".to_owned());
        }
        if !self.worker_boundary_committed {
            failures
                .push("recorder worker did not commit its shutdown storage boundary".to_owned());
        }
        if self.fallback_attempted && !self.fallback_committed {
            failures.push("direct shutdown-boundary fallback did not commit".to_owned());
        }
        if !self.producer_gate_closed {
            failures.push("recorder producer admission gate remained open".to_owned());
        }
        if self.producer_gate_in_flight != 0 {
            failures.push(format!(
                "{} synchronous recorder producer(s) remained in flight",
                self.producer_gate_in_flight
            ));
        }
        if self.pipeline_task_owners_remaining != 0 {
            failures.push(format!(
                "{} recorder pipeline task owner(s) remained resident",
                self.pipeline_task_owners_remaining
            ));
        }
        if let Err(error) = self.task_drain.verdict() {
            failures.push(error.to_string());
        }
        if !self.interaction_hook_owners_quiescent {
            failures
                .push("interaction-hook owners remained live after recorder shutdown".to_owned());
        }
        if !self.owner_accounting_complete {
            failures.push(
                "recorder shutdown could not account for every expected worker/idle/hook/bridge owner"
                    .to_owned(),
            );
        }
        if self.retained_task_owners != 0 {
            failures.push(format!(
                "{} recorder task owner(s) remain retained and physically live",
                self.retained_task_owners
            ));
        }
        if self.write_failures != 0 {
            failures.push(format!(
                "timeline writer reported {} failed writes after {} successful writes at recorder shutdown",
                self.write_failures, self.rows_written
            ));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "activity recorder shutdown failed: {}; readback={self:?}",
                failures.join("; ")
            )
        }
    }
}

fn retain_recorder_task_owner(name: &'static str, task: JoinHandle<()>) {
    let owners = RETAINED_RECORDER_TASK_OWNERS.get_or_init(|| Mutex::new(Vec::new()));
    match owners.lock() {
        Ok(mut owners) => owners.push((name, task)),
        Err(poisoned) => poisoned.into_inner().push((name, task)),
    }
}

#[must_use]
pub(crate) fn retained_task_owner_count() -> usize {
    let owners = RETAINED_RECORDER_TASK_OWNERS.get_or_init(|| Mutex::new(Vec::new()));
    let mut owners = match owners.lock() {
        Ok(owners) => owners,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut still_live = Vec::with_capacity(owners.len());
    for (name, mut task) in std::mem::take(&mut *owners) {
        if !task.is_finished() {
            still_live.push((name, task));
            continue;
        }
        match (&mut task).now_or_never() {
            Some(Ok(())) => tracing::info!(
                code = "TIMELINE_RECORDER_RETAINED_TASK_REAPED",
                task = name,
                "terminal retained recorder task owner joined and reaped"
            ),
            Some(Err(error)) => tracing::error!(
                code = "TIMELINE_RECORDER_RETAINED_TASK_JOIN_FAILED",
                task = name,
                detail = %error,
                "terminal retained recorder task owner failed while being reaped"
            ),
            None => {
                // `is_finished` is only a hint until the JoinHandle itself
                // yields. Preserve exact ownership if that observation races.
                still_live.push((name, task));
            }
        }
    }
    let count = still_live.len();
    *owners = still_live;
    count
}

#[derive(Clone, Debug)]
pub(crate) struct ActivityRecorderRetainedOwnerReadback {
    pub(crate) retained_task_owner_count: usize,
    pub(crate) unresolved_drop_producer_count: u64,
}

impl ActivityRecorderRetainedOwnerReadback {
    pub(crate) const fn safe_to_unlock(&self) -> bool {
        self.retained_task_owner_count == 0 && self.unresolved_drop_producer_count == 0
    }
}

#[must_use]
pub(crate) fn retained_owner_readback() -> ActivityRecorderRetainedOwnerReadback {
    ActivityRecorderRetainedOwnerReadback {
        retained_task_owner_count: retained_task_owner_count(),
        unresolved_drop_producer_count: UNRESOLVED_RECORDER_DROP_PRODUCERS.load(Ordering::Acquire),
    }
}

fn close_producer_gate_for_drop(gate: &RecorderProducerGate) -> (bool, usize) {
    gate.close()
}

fn record_unresolved_drop_producers(in_flight: usize) {
    if in_flight == 0 {
        return;
    }
    let increment = u64::try_from(in_flight).unwrap_or(u64::MAX);
    let _prior = UNRESOLVED_RECORDER_DROP_PRODUCERS.fetch_update(
        Ordering::AcqRel,
        Ordering::Acquire,
        |current| Some(current.saturating_add(increment)),
    );
}

async fn drain_activity_recorder_tasks(
    tasks: Vec<RecorderTaskShutdownOwner>,
) -> ActivityRecorderTaskDrainReport {
    let tasks_before = tasks.len();
    let outcomes = join_all(tasks.into_iter().map(|mut owner| async move {
        let name = owner.name;
        match tokio::time::timeout(RECORDER_TASK_STOP_TIMEOUT, owner.task_mut()).await {
            Ok(Ok(())) => {
                owner.take_terminal();
                (name, true, false, true, None)
            }
            Ok(Err(error)) => {
                owner.take_terminal();
                (
                    name,
                    true,
                    false,
                    true,
                    Some(format!("{name}: join failed: {error}")),
                )
            }
            Err(_elapsed) => {
                owner.task_mut().abort();
                match tokio::time::timeout(
                    RECORDER_TASK_ABORT_JOIN_TIMEOUT,
                    owner.task_mut(),
                )
                .await
                {
                    Ok(result) => {
                        owner.take_terminal();
                        (
                            name,
                            false,
                            true,
                            true,
                            Some(format!(
                                "{name}: did not stop within {} ms after cooperative shutdown; abort_join={result:?}",
                                RECORDER_TASK_STOP_TIMEOUT.as_millis()
                            )),
                        )
                    }
                    Err(_elapsed) => {
                        owner.abort_and_retain("abort_join_timeout");
                        (
                            name,
                            false,
                            true,
                            false,
                            Some(format!(
                                "{name}: did not stop within {} ms after cooperative shutdown and did not join within {} ms after abort; exact JoinHandle retained until physical termination",
                                RECORDER_TASK_STOP_TIMEOUT.as_millis(),
                                RECORDER_TASK_ABORT_JOIN_TIMEOUT.as_millis()
                            )),
                        )
                    }
                }
            }
        }
    }))
    .await;

    let mut graceful_joined = 0;
    let mut abort_requests_sent = 0;
    let mut joined_after_abort = 0;
    let mut still_live_task_names = Vec::new();
    let mut failures = Vec::new();
    for (name, joined_during_grace, abort_requested, terminal_readback, failure) in outcomes {
        graceful_joined += usize::from(joined_during_grace);
        abort_requests_sent += usize::from(abort_requested);
        joined_after_abort += usize::from(abort_requested && terminal_readback);
        if !terminal_readback {
            still_live_task_names.push(name);
        }
        if let Some(failure) = failure {
            failures.push(failure);
        }
    }
    ActivityRecorderTaskDrainReport {
        tasks_before,
        graceful_joined,
        abort_requests_sent,
        joined_after_abort,
        still_live_task_names,
        failures,
    }
}

/// Shared write path: every producer (worker, spawn, drop backstop) goes
/// through one row encoder so key allocation and failure accounting are
/// uniform — and one gate, so pause/exclusion (#843) can never be bypassed
/// by a feed that forgot to check.
#[derive(Clone)]
struct TimelineWriter {
    db: Arc<Db>,
    control: Arc<RecorderControl>,
    seq: Arc<AtomicU32>,
    rows_written: Arc<AtomicU64>,
    write_failures: Arc<AtomicU64>,
    rows_suppressed_paused: Arc<AtomicU64>,
    rows_suppressed_excluded: Arc<AtomicU64>,
    demo_recording: Arc<DemoRecordControl>,
}

impl TimelineWriter {
    fn try_write(
        &self,
        ts_ns: u64,
        kind: TimelineKind,
        actor: TimelineActor,
        app: Option<String>,
        payload: serde_json::Value,
    ) -> Result<()> {
        let record = TimelineRecord {
            record_version: TIMELINE_RECORD_VERSION,
            ts_ns,
            kind,
            actor,
            app,
            payload,
        };
        let value = serde_json::to_vec(&record)
            .with_context(|| format!("encode CF_TIMELINE {kind:?} record"))?;
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let key = timeline_key(ts_ns, seq);
        self.db
            .put_batch(cf::CF_TIMELINE, [(key, value)])
            .with_context(|| format!("write CF_TIMELINE {kind:?} row ts_ns={ts_ns} seq={seq}"))?;
        self.rows_written.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            code = "TIMELINE_ROW_WRITTEN",
            kind = ?kind,
            ts_ns,
            seq,
            "timeline row written"
        );
        Ok(())
    }

    /// Syncs the storage WAL. `put_batch` already returns only after the
    /// row reaches RocksDB with a synced WAL; shutdown still performs an
    /// explicit sync at session boundaries.
    fn flush_checked(&self) -> Result<()> {
        self.db.flush().context("flush batched timeline writes")
    }

    fn flush_logged(&self) {
        if let Err(error) = self.flush_checked() {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                code = "TIMELINE_FLUSH_FAILED",
                detail = %error,
                "failed to flush batched timeline writes"
            );
        }
    }

    /// The pause/exclusion gate (#843). Checked by every steady-state write;
    /// suppression is counted and debug-logged, never silent.
    fn suppressed(&self, kind: TimelineKind, app: Option<&str>) -> bool {
        match self.control.suppress_reason(app) {
            None => false,
            Some(SuppressReason::Paused) => {
                self.rows_suppressed_paused.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    code = "TIMELINE_ROW_SUPPRESSED_PAUSED",
                    kind = ?kind,
                    "timeline row suppressed: recorder is paused"
                );
                true
            }
            Some(SuppressReason::ExcludedApp) => {
                self.rows_suppressed_excluded
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    code = "TIMELINE_ROW_SUPPRESSED_EXCLUDED",
                    kind = ?kind,
                    app = app.unwrap_or_default(),
                    "timeline row suppressed: process is excluded"
                );
                true
            }
        }
    }

    /// Write path for the steady-state worker: a failed row is a loud
    /// structured error plus a failure count (surfaced by `timeline_stats`,
    /// #842), never a panic that kills the recorder.
    fn write_logged(
        &self,
        ts_ns: u64,
        kind: TimelineKind,
        actor: TimelineActor,
        app: Option<String>,
        payload: serde_json::Value,
    ) {
        if self.suppressed(kind, app.as_deref()) {
            return;
        }
        if let Err(error) = self.try_write(ts_ns, kind, actor, app, payload) {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                code = "TIMELINE_WRITE_FAILED",
                kind = ?kind,
                ts_ns,
                detail = %format!("{error:#}"),
                "failed to persist timeline row"
            );
        }
    }
}

/// Last recorded foreground window; the dedup baseline for focus/title rows.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ForegroundSnapshot {
    hwnd: i64,
    pid: u32,
    process_name: String,
    process_path: String,
    title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForegroundTransition {
    Duplicate,
    TitleChurnSuppressed(TitleChurnReason),
    TitleChanged,
    Switched,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TitleChurnReason {
    VolatilePrefix,
    VolatileSuffix,
    ProgressPercent,
}

impl TitleChurnReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::VolatilePrefix => "volatile_prefix",
            Self::VolatileSuffix => "volatile_suffix",
            Self::ProgressPercent => "progress_percent",
        }
    }
}

fn classify_foreground_transition(
    prev: Option<&ForegroundSnapshot>,
    next: &ForegroundSnapshot,
) -> ForegroundTransition {
    match prev {
        Some(prev) if prev.hwnd == next.hwnd && prev.pid == next.pid => {
            // Same window: only the title can have moved.
            if prev.title == next.title {
                ForegroundTransition::Duplicate
            } else if let Some(reason) = classify_title_churn(&prev.title, &next.title) {
                ForegroundTransition::TitleChurnSuppressed(reason)
            } else {
                ForegroundTransition::TitleChanged
            }
        }
        _ => ForegroundTransition::Switched,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NormalizedTitle {
    semantic: String,
    reason: Option<TitleChurnReason>,
}

fn classify_title_churn(previous: &str, next: &str) -> Option<TitleChurnReason> {
    let previous = normalize_title_for_churn(previous);
    let next = normalize_title_for_churn(next);
    if previous.semantic.is_empty() || previous.semantic != next.semantic {
        return None;
    }
    previous.reason.or(next.reason)
}

fn normalize_title_for_churn(title: &str) -> NormalizedTitle {
    let trimmed = title.trim();
    if let Some(semantic) = strip_volatile_prefix(trimmed) {
        return NormalizedTitle {
            semantic,
            reason: Some(TitleChurnReason::VolatilePrefix),
        };
    }
    if let Some(semantic) = strip_volatile_suffix(trimmed) {
        return NormalizedTitle {
            semantic,
            reason: Some(TitleChurnReason::VolatileSuffix),
        };
    }
    if let Some(semantic) = strip_trailing_progress_percent(trimmed) {
        return NormalizedTitle {
            semantic,
            reason: Some(TitleChurnReason::ProgressPercent),
        };
    }
    NormalizedTitle {
        semantic: trimmed.to_owned(),
        reason: None,
    }
}

fn strip_volatile_prefix(title: &str) -> Option<String> {
    let mut chars = title.char_indices();
    let (_first_at, first) = chars.next()?;
    if !is_volatile_title_glyph(first) {
        return None;
    }
    let after_first = &title[first.len_utf8()..];
    if !after_first.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let semantic = after_first.trim_start();
    (!semantic.is_empty()).then(|| semantic.to_owned())
}

fn strip_volatile_suffix(title: &str) -> Option<String> {
    let (last_at, last) = title.char_indices().next_back()?;
    if !is_volatile_title_glyph(last) {
        return None;
    }
    let before_last = &title[..last_at];
    if !before_last
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace)
    {
        return None;
    }
    let semantic = before_last.trim_end();
    (!semantic.is_empty()).then(|| semantic.to_owned())
}

fn strip_trailing_progress_percent(title: &str) -> Option<String> {
    let before_percent = title.strip_suffix('%')?.trim_end();
    let digit_start = before_percent
        .char_indices()
        .rev()
        .find_map(|(index, ch)| {
            (!ch.is_ascii_digit() && ch != '.').then_some(index + ch.len_utf8())
        })
        .unwrap_or(0);
    let number = before_percent[digit_start..].trim();
    if number.is_empty() || !number.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let semantic = before_percent[..digit_start].trim_end();
    (!semantic.is_empty()).then(|| format!("{semantic} <%>"))
}

fn is_volatile_title_glyph(ch: char) -> bool {
    matches!(
        ch,
        '\u{2800}'
            ..='\u{28ff}'
                | '◐'
                | '◓'
                | '◑'
                | '◒'
                | '◴'
                | '◷'
                | '◶'
                | '◵'
                | '◜'
                | '◝'
                | '◞'
                | '◟'
                | '|'
                | '/'
                | '-'
                | '\\'
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdleEdge {
    Start,
    End,
}

const fn idle_transition(currently_idle: bool, idle_ms: u64, timeout_ms: u64) -> Option<IdleEdge> {
    if !currently_idle && idle_ms >= timeout_ms {
        Some(IdleEdge::Start)
    } else if currently_idle && idle_ms < timeout_ms {
        Some(IdleEdge::End)
    } else {
        None
    }
}

fn now_ts_ns() -> u64 {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    u64::try_from(nanos).unwrap_or(0)
}

/// Resolves who is driving the session right now. An agent session holding
/// the real-input lease owns foreground changes; the operator-preempt
/// sentinel and an unheld lease both mean the human.
fn current_actor() -> TimelineActor {
    let status = synapse_action::lease::status();
    match status.owner_session_id {
        Some(owner) if status.held && owner != synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID => {
            TimelineActor::Agent { session_id: owner }
        }
        _ => TimelineActor::Human,
    }
}

const INTERACTION_BUCKET_NS: u64 = 30_000_000_000;
const INJECTED_UNATTRIBUTED_SESSION_ID: &str = "injected-input";
const MAX_BROWSER_NAV_DEDUPE_KEYS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserNavigationEvent {
    pub actor: TimelineActor,
    pub app: Option<String>,
    pub source: String,
    pub event: String,
    pub action: Option<String>,
    pub url: String,
    pub title: String,
    pub tab_id: Option<u32>,
    pub chrome_window_id: Option<i64>,
    pub window_hwnd: Option<i64>,
    pub cdp_target_id: Option<String>,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub requested_url: Option<String>,
    pub before_url: Option<String>,
    pub before_title: Option<String>,
    pub ready_state: Option<String>,
    pub observed_at_unix_ms: Option<u64>,
    pub active: Option<bool>,
    pub highlighted: Option<bool>,
    pub pinned: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InteractionBucket {
    bucket_start_ns: u64,
    bucket_end_ns: u64,
    first_event_ns: u64,
    last_event_ns: u64,
    hwnd: i64,
    pid: u32,
    process_name: String,
    process_path: String,
    title_sha256: String,
    actor: TimelineActor,
    input_origin: &'static str,
    keystroke_count: u64,
    click_count: u64,
    vertical_scroll_delta: i64,
    horizontal_scroll_delta: i64,
    app_switch_count: u64,
}

impl InteractionBucket {
    fn new(
        ts_ns: u64,
        context: &synapse_core::ForegroundContext,
        actor: TimelineActor,
        input_origin: &'static str,
    ) -> Self {
        let bucket_start_ns = bucket_start(ts_ns);
        Self {
            bucket_start_ns,
            bucket_end_ns: bucket_start_ns.saturating_add(INTERACTION_BUCKET_NS),
            first_event_ns: ts_ns,
            last_event_ns: ts_ns,
            hwnd: context.hwnd,
            pid: context.pid,
            process_name: context.process_name.clone(),
            process_path: context.process_path.clone(),
            title_sha256: sha256_hex(&context.window_title),
            actor,
            input_origin,
            keystroke_count: 0,
            click_count: 0,
            vertical_scroll_delta: 0,
            horizontal_scroll_delta: 0,
            app_switch_count: 0,
        }
    }

    fn accepts(
        &self,
        ts_ns: u64,
        context: &synapse_core::ForegroundContext,
        actor: &TimelineActor,
        input_origin: &'static str,
    ) -> bool {
        self.bucket_start_ns == bucket_start(ts_ns)
            && self.hwnd == context.hwnd
            && self.pid == context.pid
            && self.process_name == context.process_name
            && &self.actor == actor
            && self.input_origin == input_origin
    }

    fn note_event_time(&mut self, ts_ns: u64) {
        self.first_event_ns = self.first_event_ns.min(ts_ns);
        self.last_event_ns = self.last_event_ns.max(ts_ns);
    }

    fn input_event_count(&self) -> u64 {
        self.keystroke_count
            .saturating_add(self.click_count)
            .saturating_add(u64::from(self.vertical_scroll_delta != 0))
            .saturating_add(u64::from(self.horizontal_scroll_delta != 0))
    }

    fn is_empty(&self) -> bool {
        self.keystroke_count == 0
            && self.click_count == 0
            && self.vertical_scroll_delta == 0
            && self.horizontal_scroll_delta == 0
            && self.app_switch_count == 0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct InteractionAccumulator {
    current: Option<InteractionBucket>,
}

impl InteractionAccumulator {
    fn record_input(
        &mut self,
        event: &InteractionEvent,
        context: &synapse_core::ForegroundContext,
        actor: TimelineActor,
        input_origin: &'static str,
        writer: &TimelineWriter,
    ) {
        self.ensure_bucket(event.ts_ns, context, actor, input_origin, writer);
        let Some(bucket) = self.current.as_mut() else {
            return;
        };
        bucket.note_event_time(event.ts_ns);
        match event.kind {
            InteractionEventKind::Keystroke => {
                bucket.keystroke_count = bucket.keystroke_count.saturating_add(1);
            }
            InteractionEventKind::Click => {
                bucket.click_count = bucket.click_count.saturating_add(1);
            }
            InteractionEventKind::VerticalScroll { delta } => {
                bucket.vertical_scroll_delta = bucket
                    .vertical_scroll_delta
                    .saturating_add(i64::from(delta));
            }
            InteractionEventKind::HorizontalScroll { delta } => {
                bucket.horizontal_scroll_delta = bucket
                    .horizontal_scroll_delta
                    .saturating_add(i64::from(delta));
            }
        }
    }

    fn record_app_switch(
        &mut self,
        ts_ns: u64,
        context: &synapse_core::ForegroundContext,
        actor: TimelineActor,
        writer: &TimelineWriter,
    ) {
        self.ensure_bucket(ts_ns, context, actor, "foreground", writer);
        let Some(bucket) = self.current.as_mut() else {
            return;
        };
        bucket.note_event_time(ts_ns);
        bucket.app_switch_count = bucket.app_switch_count.saturating_add(1);
    }

    fn ensure_bucket(
        &mut self,
        ts_ns: u64,
        context: &synapse_core::ForegroundContext,
        actor: TimelineActor,
        input_origin: &'static str,
        writer: &TimelineWriter,
    ) {
        let needs_new = self
            .current
            .as_ref()
            .is_none_or(|bucket| !bucket.accepts(ts_ns, context, &actor, input_origin));
        if needs_new {
            self.flush(writer);
            self.current = Some(InteractionBucket::new(ts_ns, context, actor, input_origin));
        }
    }

    fn flush(&mut self, writer: &TimelineWriter) {
        let Some(bucket) = self.current.take() else {
            return;
        };
        if bucket.is_empty() {
            return;
        }
        write_interaction_summary(writer, bucket);
    }
}

const ASSIST_UNDO_BURST_COUNT_ENV: &str = "SYNAPSE_ASSIST_UNDO_BURST_COUNT";
const ASSIST_UNDO_BURST_WINDOW_MS_ENV: &str = "SYNAPSE_ASSIST_UNDO_BURST_WINDOW_MS";
const ASSIST_RETYPE_DELETE_COUNT_ENV: &str = "SYNAPSE_ASSIST_RETYPE_DELETE_COUNT";
const ASSIST_RETYPE_TEXT_COUNT_ENV: &str = "SYNAPSE_ASSIST_RETYPE_TEXT_COUNT";
const ASSIST_RETYPE_WINDOW_MS_ENV: &str = "SYNAPSE_ASSIST_RETYPE_WINDOW_MS";
const ASSIST_REPEATED_CLICK_COUNT_ENV: &str = "SYNAPSE_ASSIST_REPEATED_CLICK_COUNT";
const ASSIST_REPEATED_CLICK_WINDOW_MS_ENV: &str = "SYNAPSE_ASSIST_REPEATED_CLICK_WINDOW_MS";
const ASSIST_DIALOG_REOPEN_COUNT_ENV: &str = "SYNAPSE_ASSIST_DIALOG_REOPEN_COUNT";
const ASSIST_DIALOG_REOPEN_WINDOW_MS_ENV: &str = "SYNAPSE_ASSIST_DIALOG_REOPEN_WINDOW_MS";
const ASSIST_COOLDOWN_MS_ENV: &str = "SYNAPSE_ASSIST_COOLDOWN_MS";
const ASSIST_HISTORY_CAP: usize = 256;
const ASSIST_INJECTED_VALUE_SUPPRESSION_NS: u64 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AssistDetectorConfig {
    undo_burst_count: u64,
    undo_burst_window_ns: u64,
    retype_delete_count: u64,
    retype_text_count: u64,
    retype_window_ns: u64,
    repeated_click_count: u64,
    repeated_click_window_ns: u64,
    dialog_reopen_count: u64,
    dialog_reopen_window_ns: u64,
    cooldown_ns: u64,
}

impl AssistDetectorConfig {
    fn from_env() -> Result<Self> {
        Ok(Self {
            undo_burst_count: env_u64(ASSIST_UNDO_BURST_COUNT_ENV, 3)?,
            undo_burst_window_ns: env_ms_as_ns(ASSIST_UNDO_BURST_WINDOW_MS_ENV, 10_000)?,
            retype_delete_count: env_u64(ASSIST_RETYPE_DELETE_COUNT_ENV, 3)?,
            retype_text_count: env_u64(ASSIST_RETYPE_TEXT_COUNT_ENV, 12)?,
            retype_window_ns: env_ms_as_ns(ASSIST_RETYPE_WINDOW_MS_ENV, 20_000)?,
            repeated_click_count: env_u64(ASSIST_REPEATED_CLICK_COUNT_ENV, 5)?,
            repeated_click_window_ns: env_ms_as_ns(ASSIST_REPEATED_CLICK_WINDOW_MS_ENV, 8_000)?,
            dialog_reopen_count: env_u64(ASSIST_DIALOG_REOPEN_COUNT_ENV, 3)?,
            dialog_reopen_window_ns: env_ms_as_ns(ASSIST_DIALOG_REOPEN_WINDOW_MS_ENV, 60_000)?,
            cooldown_ns: env_ms_as_ns(ASSIST_COOLDOWN_MS_ENV, 60_000)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AssistContext {
    hwnd: i64,
    pid: u32,
    process_name: String,
    window_title_sha256: String,
    focused_element_sha256: Option<String>,
    focused_role: Option<String>,
}

impl AssistContext {
    fn from_foreground(
        context: &synapse_core::ForegroundContext,
        focused: Option<&AccessibleNode>,
    ) -> Self {
        Self {
            hwnd: context.hwnd,
            pid: context.pid,
            process_name: context.process_name.clone(),
            window_title_sha256: sha256_hex(&context.window_title),
            focused_element_sha256: focused.map(focused_element_signature),
            focused_role: focused.map(|node| node.role.clone()),
        }
    }

    fn window_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.hwnd,
            self.pid,
            self.process_name,
            self.focused_element_sha256.as_deref().unwrap_or("window")
        )
    }

    fn evidence_json(&self) -> serde_json::Value {
        json!({
            "hwnd": self.hwnd,
            "pid": self.pid,
            "process_name": self.process_name,
            "window_title_sha256": self.window_title_sha256,
            "focused_element_sha256": self.focused_element_sha256,
            "focused_role": self.focused_role,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssistSignalKind {
    UndoCommand,
    DeleteCommand,
    TextLikeKey,
    Click,
}

#[derive(Clone, Debug)]
struct AssistSignal {
    ts_ns: u64,
    window_key: String,
    state_version: u64,
    kind: AssistSignalKind,
}

#[derive(Clone, Debug)]
struct DialogSeen {
    ts_ns: u64,
    process_name: String,
    title_sha256: String,
}

#[derive(Clone, Debug, Default)]
struct AssistDetector {
    interactions: VecDeque<AssistSignal>,
    dialogs: VecDeque<DialogSeen>,
    last_emitted: VecDeque<(String, u64)>,
    value_lengths: HashMap<String, usize>,
    last_injected_keyboard_ns: Option<u64>,
    state_version: u64,
}

impl AssistDetector {
    fn note_state_change(&mut self) {
        self.state_version = self.state_version.saturating_add(1);
    }

    fn record_interaction(
        &mut self,
        event: &InteractionEvent,
        context: &AssistContext,
        actor: &TimelineActor,
        input_origin: &'static str,
        config: AssistDetectorConfig,
        sink: &AssistEventSink,
    ) {
        let Some(kind) = signal_kind(event) else {
            return;
        };
        if input_origin == "injected"
            && matches!(
                kind,
                AssistSignalKind::UndoCommand
                    | AssistSignalKind::DeleteCommand
                    | AssistSignalKind::TextLikeKey
            )
        {
            self.last_injected_keyboard_ns = Some(event.ts_ns);
        }
        self.interactions.push_back(AssistSignal {
            ts_ns: event.ts_ns,
            window_key: context.window_key(),
            state_version: self.state_version,
            kind,
        });
        trim_interactions(&mut self.interactions, event.ts_ns, config);
        self.detect_interaction_loops(
            event.ts_ns,
            context,
            Some(actor),
            input_origin,
            config,
            sink,
        );
    }

    fn record_value_change(
        &mut self,
        ts_ns: u64,
        context: &AssistContext,
        value_len: usize,
        config: AssistDetectorConfig,
        sink: &AssistEventSink,
    ) {
        self.note_state_change();
        let window_key = context.window_key();
        let Some(previous_len) = self.value_lengths.insert(window_key.clone(), value_len) else {
            return;
        };
        if self.recent_injected_keyboard(ts_ns) {
            return;
        }
        let kind = match value_len.cmp(&previous_len) {
            std::cmp::Ordering::Greater => AssistSignalKind::TextLikeKey,
            std::cmp::Ordering::Less => AssistSignalKind::DeleteCommand,
            std::cmp::Ordering::Equal => return,
        };
        self.interactions.push_back(AssistSignal {
            ts_ns,
            window_key,
            state_version: self.state_version,
            kind,
        });
        trim_interactions(&mut self.interactions, ts_ns, config);
        self.detect_interaction_loops(ts_ns, context, None, "uia_value_change", config, sink);
    }

    fn recent_injected_keyboard(&self, ts_ns: u64) -> bool {
        self.last_injected_keyboard_ns.is_some_and(|last| {
            ts_ns >= last && ts_ns.saturating_sub(last) <= ASSIST_INJECTED_VALUE_SUPPRESSION_NS
        })
    }

    fn record_dialog_title(
        &mut self,
        ts_ns: u64,
        snapshot: &ForegroundSnapshot,
        config: AssistDetectorConfig,
        sink: &AssistEventSink,
    ) {
        if !dialog_like_title(&snapshot.title) {
            return;
        }
        let title_sha256 = sha256_hex(&snapshot.title);
        self.dialogs.push_back(DialogSeen {
            ts_ns,
            process_name: snapshot.process_name.clone(),
            title_sha256: title_sha256.clone(),
        });
        trim_dialogs(&mut self.dialogs, ts_ns, config.dialog_reopen_window_ns);
        let count = self
            .dialogs
            .iter()
            .filter(|seen| {
                seen.process_name == snapshot.process_name && seen.title_sha256 == title_sha256
            })
            .count() as u64;
        if count >= config.dialog_reopen_count {
            let context = AssistContext {
                hwnd: snapshot.hwnd,
                pid: snapshot.pid,
                process_name: snapshot.process_name.clone(),
                window_title_sha256: title_sha256,
                focused_element_sha256: None,
                focused_role: None,
            };
            self.emit(
                "dialog_reopen_loop",
                ts_ns,
                &context,
                None,
                "foreground_dialog_title",
                config.cooldown_ns,
                json!({
                    "dialog_reopen_count": count,
                    "window_ns": config.dialog_reopen_window_ns,
                    "threshold_count": config.dialog_reopen_count,
                }),
                sink,
            );
        }
    }

    fn detect_interaction_loops(
        &mut self,
        ts_ns: u64,
        context: &AssistContext,
        actor: Option<&TimelineActor>,
        input_origin: &'static str,
        config: AssistDetectorConfig,
        sink: &AssistEventSink,
    ) {
        let window_key = context.window_key();
        let undo_count = self.count_recent(
            ts_ns,
            config.undo_burst_window_ns,
            &window_key,
            None,
            AssistSignalKind::UndoCommand,
        );
        if undo_count >= config.undo_burst_count {
            self.emit(
                "undo_burst",
                ts_ns,
                context,
                actor,
                input_origin,
                config.cooldown_ns,
                json!({
                    "undo_command_count": undo_count,
                    "window_ns": config.undo_burst_window_ns,
                    "threshold_count": config.undo_burst_count,
                }),
                sink,
            );
        }

        let text_count = self.count_recent(
            ts_ns,
            config.retype_window_ns,
            &window_key,
            None,
            AssistSignalKind::TextLikeKey,
        );
        let delete_count = self.count_recent(
            ts_ns,
            config.retype_window_ns,
            &window_key,
            None,
            AssistSignalKind::DeleteCommand,
        );
        if text_count >= config.retype_text_count && delete_count >= config.retype_delete_count {
            self.emit(
                "retype_loop",
                ts_ns,
                context,
                actor,
                input_origin,
                config.cooldown_ns,
                json!({
                    "text_like_key_count": text_count,
                    "delete_command_count": delete_count,
                    "window_ns": config.retype_window_ns,
                    "text_threshold": config.retype_text_count,
                    "delete_threshold": config.retype_delete_count,
                }),
                sink,
            );
        }

        let click_count = self.count_recent(
            ts_ns,
            config.repeated_click_window_ns,
            &window_key,
            Some(self.state_version),
            AssistSignalKind::Click,
        );
        if click_count >= config.repeated_click_count {
            self.emit(
                "repeated_click_without_state_change",
                ts_ns,
                context,
                actor,
                input_origin,
                config.cooldown_ns,
                json!({
                    "click_count": click_count,
                    "window_ns": config.repeated_click_window_ns,
                    "threshold_count": config.repeated_click_count,
                    "state_version": self.state_version,
                }),
                sink,
            );
        }
    }

    fn count_recent(
        &self,
        ts_ns: u64,
        window_ns: u64,
        window_key: &str,
        state_version: Option<u64>,
        kind: AssistSignalKind,
    ) -> u64 {
        let start = ts_ns.saturating_sub(window_ns);
        self.interactions
            .iter()
            .filter(|signal| {
                signal.ts_ns >= start
                    && signal.window_key == window_key
                    && signal.kind == kind
                    && state_version.is_none_or(|version| signal.state_version == version)
            })
            .count() as u64
    }

    fn emit(
        &mut self,
        detector: &'static str,
        ts_ns: u64,
        context: &AssistContext,
        actor: Option<&TimelineActor>,
        input_origin: &'static str,
        cooldown_ns: u64,
        counts: serde_json::Value,
        sink: &AssistEventSink,
    ) {
        let cooldown_key = format!("{detector}:{}", context.window_key());
        trim_cooldowns(&mut self.last_emitted, ts_ns, cooldown_ns);
        if self.last_emitted.iter().any(|(key, last_ts)| {
            key == &cooldown_key && last_ts.saturating_add(cooldown_ns) > ts_ns
        }) {
            return;
        }
        self.last_emitted.push_back((cooldown_key, ts_ns));
        sink.emit(ts_ns, detector, context, actor, input_origin, counts);
    }
}

#[derive(Clone)]
struct AssistEventSink {
    db: Arc<Db>,
    event_bus: EventBus,
    event_seq: Arc<AtomicU64>,
    storage_seq: Arc<AtomicU32>,
}

impl AssistEventSink {
    fn emit(
        &self,
        ts_ns: u64,
        detector: &'static str,
        context: &AssistContext,
        actor: Option<&TimelineActor>,
        input_origin: &'static str,
        counts: serde_json::Value,
    ) {
        let seq = self.event_seq.fetch_add(1, Ordering::Relaxed);
        let data = json!({
            "opportunity_id": format!("assist-{ts_ns}-{seq}"),
            "detector": detector,
            "confidence": confidence_for_detector(detector),
            "trigger": {
                "actor": actor_evidence(actor),
                "input_origin": input_origin
            },
            "window": context.evidence_json(),
            "counts": counts,
            "privacy": {
                "raw_typed_text": false,
                "raw_key_names": false,
                "mouse_coordinates": false,
                "raw_window_title": false,
                "raw_element_value": false
            }
        });
        let at = chrono::DateTime::<Utc>::from_timestamp(
            i64::try_from(ts_ns / 1_000_000_000).unwrap_or(i64::MAX),
            u32::try_from(ts_ns % 1_000_000_000).unwrap_or(999_999_999),
        )
        .unwrap_or_else(Utc::now);
        let event = Event {
            seq,
            at,
            source: EventSource::System,
            kind: ASSIST_EVENT_KIND.to_owned(),
            data: data.clone(),
            correlations: Vec::new(),
        };
        let report = self.event_bus.publish(event);
        let stored = StoredEvent {
            schema_version: SCHEMA_VERSION,
            event_id: format!("assist-opportunity-{ts_ns}-{seq}"),
            ts_ns,
            session_id: None,
            audit_context: None,
            source: EventSource::System,
            kind: ASSIST_EVENT_KIND.to_owned(),
            data,
            window_id: Some(context.hwnd),
            element_id: None,
            redacted: false,
            redactions: Vec::new(),
        };
        if let Err(error) = self.write_stored_event(&stored) {
            tracing::error!(
                code = "ASSIST_OPPORTUNITY_EVENT_WRITE_FAILED",
                detector,
                detail = %format!("{error:#}"),
                "failed to persist assist opportunity event"
            );
            return;
        }
        tracing::info!(
            code = "ASSIST_OPPORTUNITY_EMITTED",
            detector,
            event_seq = seq,
            matched = report.matched,
            queued = report.queued,
            dropped = report.dropped,
            "assist opportunity emitted"
        );
    }

    fn write_stored_event(&self, event: &StoredEvent) -> Result<()> {
        let encoded = synapse_storage::encode_json(event)
            .context("encode assist opportunity CF_EVENTS row")?;
        self.db
            .put_batch(
                cf::CF_EVENTS,
                [(event_key(event.ts_ns, &self.storage_seq), encoded)],
            )
            .context("write assist opportunity CF_EVENTS row")?;
        self.db
            .flush()
            .context("flush assist opportunity CF_EVENTS row")
    }
}

fn bucket_start(ts_ns: u64) -> u64 {
    (ts_ns / INTERACTION_BUCKET_NS).saturating_mul(INTERACTION_BUCKET_NS)
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn focused_element_signature(node: &AccessibleNode) -> String {
    sha256_hex(&format!(
        "{}\n{}\n{}",
        node.element_id,
        node.role,
        node.automation_id.as_deref().unwrap_or_default()
    ))
}

fn focused_element_for_context(hwnd: i64) -> Option<AccessibleNode> {
    match synapse_a11y::focused_element_node_in_window(hwnd) {
        Ok(focused) => focused,
        Err(error) => {
            tracing::debug!(
                code = "ASSIST_FOCUSED_ELEMENT_UNAVAILABLE",
                hwnd,
                detail = %error,
                "assist detector could not resolve focused element; using window identity only"
            );
            None
        }
    }
}

fn value_len_for_event(event: &AccessibleEvent, focused: Option<&AccessibleNode>) -> Option<usize> {
    event
        .value
        .as_ref()
        .or_else(|| focused.and_then(|node| node.value.as_ref()))
        .map(|value| value.chars().count())
}

fn signal_kind(event: &InteractionEvent) -> Option<AssistSignalKind> {
    match event.kind {
        InteractionEventKind::Keystroke => match event.key_signal {
            Some(InteractionKeySignal::UndoCommand) => Some(AssistSignalKind::UndoCommand),
            Some(InteractionKeySignal::DeleteCommand) => Some(AssistSignalKind::DeleteCommand),
            Some(InteractionKeySignal::TextLikeKey) => Some(AssistSignalKind::TextLikeKey),
            Some(InteractionKeySignal::OtherKey) | None => None,
        },
        InteractionEventKind::Click => Some(AssistSignalKind::Click),
        InteractionEventKind::VerticalScroll { .. }
        | InteractionEventKind::HorizontalScroll { .. } => None,
    }
}

fn actor_evidence(actor: Option<&TimelineActor>) -> serde_json::Value {
    match actor {
        Some(TimelineActor::Human) => json!({ "kind": "human" }),
        Some(TimelineActor::Agent { session_id }) => {
            json!({ "kind": "agent", "session_id": session_id })
        }
        None => json!({ "kind": "unknown" }),
    }
}

fn trim_interactions(
    interactions: &mut VecDeque<AssistSignal>,
    now_ns: u64,
    config: AssistDetectorConfig,
) {
    let max_window = config
        .undo_burst_window_ns
        .max(config.retype_window_ns)
        .max(config.repeated_click_window_ns);
    let cutoff = now_ns.saturating_sub(max_window);
    while interactions
        .front()
        .is_some_and(|signal| signal.ts_ns < cutoff || interactions.len() > ASSIST_HISTORY_CAP)
    {
        interactions.pop_front();
    }
}

fn trim_dialogs(dialogs: &mut VecDeque<DialogSeen>, now_ns: u64, window_ns: u64) {
    let cutoff = now_ns.saturating_sub(window_ns);
    while dialogs
        .front()
        .is_some_and(|seen| seen.ts_ns < cutoff || dialogs.len() > ASSIST_HISTORY_CAP)
    {
        dialogs.pop_front();
    }
}

fn trim_cooldowns(cooldowns: &mut VecDeque<(String, u64)>, now_ns: u64, cooldown_ns: u64) {
    let cutoff = now_ns.saturating_sub(cooldown_ns);
    while cooldowns
        .front()
        .is_some_and(|(_key, ts_ns)| *ts_ns < cutoff || cooldowns.len() > ASSIST_HISTORY_CAP)
    {
        cooldowns.pop_front();
    }
}

fn dialog_like_title(title: &str) -> bool {
    let lowered = title.to_ascii_lowercase();
    ["error", "warning", "save", "open", "confirm", "dialog"]
        .iter()
        .any(|needle| lowered.contains(needle))
}

fn confidence_for_detector(detector: &str) -> f64 {
    match detector {
        "undo_burst" => 0.82,
        "retype_loop" => 0.78,
        "repeated_click_without_state_change" => 0.72,
        "dialog_reopen_loop" => 0.8,
        _ => 0.5,
    }
}

fn event_key(ts_ns: u64, seq: &AtomicU32) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    key
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    let Some(raw) = std::env::var(name).ok() else {
        return Ok(default);
    };
    let value = raw
        .trim()
        .parse::<u64>()
        .with_context(|| format!("{name} must be a positive integer, got {raw:?}"))?;
    if value == 0 {
        bail!("{name} must be at least 1, got 0");
    }
    Ok(value)
}

fn env_ms_as_ns(name: &str, default_ms: u64) -> Result<u64> {
    let ms = env_u64(name, default_ms)?;
    ms.checked_mul(1_000_000)
        .ok_or_else(|| anyhow::anyhow!("{name} is too large to convert from ms to ns: {ms}"))
}

fn redact_browser_navigation_event(mut event: BrowserNavigationEvent) -> BrowserNavigationEvent {
    event.url = redact_url_for_public_readback(&event.url);
    event.requested_url = redact_url_opt_for_public_readback(event.requested_url);
    event.before_url = redact_url_opt_for_public_readback(event.before_url);
    event
}

fn browser_nav_dedupe_key(event: &BrowserNavigationEvent) -> String {
    let url_sha256 = sha256_hex(event.url.trim());
    format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n{}\n{}",
        event.actor, event.tab_id, event.cdp_target_id, event.window_hwnd, url_sha256, event.title
    )
}

fn interaction_actor(injected: bool) -> (TimelineActor, &'static str) {
    if !injected {
        return (TimelineActor::Human, "physical");
    }
    match current_actor() {
        TimelineActor::Agent { session_id } => (TimelineActor::Agent { session_id }, "injected"),
        TimelineActor::Human => (
            TimelineActor::Agent {
                session_id: INJECTED_UNATTRIBUTED_SESSION_ID.to_owned(),
            },
            "injected",
        ),
    }
}

fn write_interaction_summary(writer: &TimelineWriter, bucket: InteractionBucket) {
    let duration_ms = bucket.last_event_ns.saturating_sub(bucket.first_event_ns) / 1_000_000;
    let payload = json!({
        "bucket_start_ns": bucket.bucket_start_ns,
        "bucket_end_ns": bucket.bucket_end_ns,
        "bucket_ms": INTERACTION_BUCKET_NS / 1_000_000,
        "first_event_ns": bucket.first_event_ns,
        "last_event_ns": bucket.last_event_ns,
        "duration_ms": duration_ms,
        "pid": bucket.pid,
        "hwnd": bucket.hwnd,
        "process_path": bucket.process_path,
        "window_title_sha256": bucket.title_sha256,
        "input_origin": bucket.input_origin,
        "keystroke_count": bucket.keystroke_count,
        "click_count": bucket.click_count,
        "scroll_vertical_delta": bucket.vertical_scroll_delta,
        "scroll_horizontal_delta": bucket.horizontal_scroll_delta,
        "app_switch_count": bucket.app_switch_count,
        "input_event_count": bucket.input_event_count(),
    });
    if let Err(error) = writer.try_write(
        bucket.last_event_ns,
        TimelineKind::InteractionSummary,
        bucket.actor,
        Some(bucket.process_name),
        payload,
    ) {
        writer.write_failures.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            code = "TIMELINE_INTERACTION_SUMMARY_WRITE_FAILED",
            detail = %format!("{error:#}"),
            "failed to persist interaction cadence summary row"
        );
    }
}

struct WorkerState {
    writer: TimelineWriter,
    config: RecorderConfig,
    foreground: Option<ForegroundSnapshot>,
    idle: bool,
    interactions: InteractionAccumulator,
    assist: AssistDetector,
    assist_sink: AssistEventSink,
}

impl WorkerState {
    fn handle_accessible(&mut self, event: &AccessibleEvent) {
        // Paused means *perceive nothing*: skip even the foreground/title
        // readbacks, not just the row writes. The snapshot is dropped so the
        // first post-resume trigger re-records reality from scratch.
        if self.writer.control.is_paused() {
            self.foreground = None;
            self.writer
                .rows_suppressed_paused
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        match event.kind {
            AccessibleEventKind::ForegroundChanged => self.handle_foreground(event.window_id),
            AccessibleEventKind::NameChanged => self.handle_name_change(event.window_id),
            AccessibleEventKind::ValueChanged => self.handle_value_change(event),
            _ => {}
        }
    }

    fn handle_interaction(&mut self, event: &InteractionEvent) {
        if self.writer.control.is_paused() {
            self.writer
                .rows_suppressed_paused
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let context = match synapse_a11y::current_foreground_context() {
            Ok(context) => context,
            Err(error) => {
                tracing::debug!(
                    code = "TIMELINE_INTERACTION_FOREGROUND_NONE",
                    detail = %error,
                    "interaction cadence event had no foreground context"
                );
                return;
            }
        };
        if self.writer.suppressed(
            TimelineKind::InteractionSummary,
            Some(&context.process_name),
        ) {
            return;
        }
        let (actor, input_origin) = interaction_actor(event.injected);
        self.interactions
            .record_input(event, &context, actor.clone(), input_origin, &self.writer);
        let focused = focused_element_for_context(context.hwnd);
        let assist_context = AssistContext::from_foreground(&context, focused.as_ref());
        self.assist.record_interaction(
            event,
            &assist_context,
            &actor,
            input_origin,
            self.config.assist,
            &self.assist_sink,
        );
    }

    fn handle_value_change(&mut self, event: &AccessibleEvent) {
        let context = match synapse_a11y::current_foreground_context() {
            Ok(context) => context,
            Err(error) => {
                tracing::debug!(
                    code = "ASSIST_VALUE_CHANGE_FOREGROUND_NONE",
                    event_hwnd = event.window_id,
                    detail = %error,
                    "value-change event had no foreground context"
                );
                return;
            }
        };
        if self.writer.suppressed(
            TimelineKind::InteractionSummary,
            Some(&context.process_name),
        ) {
            return;
        }
        let focused = focused_element_for_context(context.hwnd);
        let Some(value_len) = value_len_for_event(event, focused.as_ref()) else {
            return;
        };
        let assist_context = AssistContext::from_foreground(&context, focused.as_ref());
        self.assist.record_value_change(
            event.at_ms.saturating_mul(1_000_000),
            &assist_context,
            value_len,
            self.config.assist,
            &self.assist_sink,
        );
    }

    /// A `ForegroundChanged` WinEvent is a *trigger*, not the truth: it is
    /// delivered asynchronously, and its hwnd can be an Alt-Tab transient
    /// (`ForegroundStaging`), a window that has not been shown yet, or one
    /// that is already destroyed. When the event hwnd is not a usable visible
    /// window, the recorder re-reads the actual foreground window instead of
    /// dropping the trigger — otherwise a real app switch hiding behind a
    /// transient event would silently vanish from the timeline.
    fn handle_foreground(&mut self, window_id: i64) {
        let context = match self.resolve_foreground_trigger(window_id) {
            Some(context) => context,
            None => return,
        };
        self.apply_foreground(&context, "win_event");
    }

    fn resolve_foreground_trigger(
        &self,
        window_id: i64,
    ) -> Option<synapse_core::ForegroundContext> {
        match synapse_a11y::is_window_visible(window_id) {
            Ok(true) => match synapse_a11y::foreground_context(window_id) {
                Ok(context) => return Some(context),
                Err(error) => {
                    tracing::debug!(
                        code = "TIMELINE_FOREGROUND_EVENT_HWND_STALE",
                        hwnd = window_id,
                        detail = %error,
                        "event window vanished mid-resolve; re-reading the real foreground"
                    );
                }
            },
            Ok(false) => {
                tracing::debug!(
                    code = "TIMELINE_FOREGROUND_EVENT_HWND_INVISIBLE",
                    hwnd = window_id,
                    "event window is invisible (transient); re-reading the real foreground"
                );
            }
            Err(error) => {
                tracing::debug!(
                    code = "TIMELINE_FOREGROUND_EVENT_HWND_STALE",
                    hwnd = window_id,
                    detail = %error,
                    "event window vanished before visibility readback; re-reading the real foreground"
                );
            }
        }
        // Source of truth: whatever is actually foreground right now.
        match synapse_a11y::current_foreground_context() {
            Ok(context) => {
                if matches!(synapse_a11y::is_window_visible(context.hwnd), Ok(true)) {
                    Some(context)
                } else {
                    tracing::debug!(
                        code = "TIMELINE_FOREGROUND_UNSETTLED",
                        event_hwnd = window_id,
                        current_hwnd = context.hwnd,
                        "current foreground is itself transient; next trigger or poll will settle it"
                    );
                    None
                }
            }
            Err(error) => {
                tracing::debug!(
                    code = "TIMELINE_FOREGROUND_NONE",
                    event_hwnd = window_id,
                    detail = %error,
                    "no resolvable foreground window for this trigger"
                );
                None
            }
        }
    }

    /// Records the resolved foreground state, deduplicating against the last
    /// recorded snapshot. `source` records which trigger produced the row.
    fn apply_foreground(&mut self, context: &synapse_core::ForegroundContext, source: &str) {
        let next = ForegroundSnapshot {
            hwnd: context.hwnd,
            pid: context.pid,
            process_name: context.process_name.clone(),
            process_path: context.process_path.clone(),
            title: context.window_title.clone(),
        };
        // Excluded processes leave the dedup snapshot untouched: the moment
        // the exclusion lifts (or focus moves to a recordable app), the next
        // trigger classifies as a switch and records reality instead of
        // deduplicating against a window that was never written.
        if self
            .writer
            .suppressed(TimelineKind::FocusChange, Some(&next.process_name))
        {
            return;
        }
        match classify_foreground_transition(self.foreground.as_ref(), &next) {
            ForegroundTransition::Duplicate => {}
            ForegroundTransition::TitleChurnSuppressed(reason) => {
                self.record_suppressed_title_change(&next, reason);
            }
            ForegroundTransition::TitleChanged => {
                let ts_ns = now_ts_ns();
                self.write_title_change(&next, ts_ns);
                self.assist.note_state_change();
                self.assist.record_dialog_title(
                    ts_ns,
                    &next,
                    self.config.assist,
                    &self.assist_sink,
                );
            }
            ForegroundTransition::Switched => {
                let ts_ns = now_ts_ns();
                let actor = current_actor();
                self.writer.write_logged(
                    ts_ns,
                    TimelineKind::FocusChange,
                    actor.clone(),
                    Some(next.process_name.clone()),
                    json!({
                        "title": next.title,
                        "process_path": next.process_path,
                        "pid": next.pid,
                        "hwnd": next.hwnd,
                        "source": source,
                    }),
                );
                self.interactions
                    .record_app_switch(ts_ns, context, actor, &self.writer);
                self.assist.note_state_change();
                self.assist.record_dialog_title(
                    ts_ns,
                    &next,
                    self.config.assist,
                    &self.assist_sink,
                );
            }
        }
        self.foreground = Some(next);
    }

    fn handle_name_change(&mut self, window_id: i64) {
        let Some(previous) = self.foreground.clone() else {
            return;
        };
        if previous.hwnd != window_id {
            return;
        }
        // NAMECHANGE also fires for child objects of the same HWND; re-read
        // the top-level title and only record a real change.
        let context = match synapse_a11y::foreground_context(window_id) {
            Ok(context) => context,
            Err(error) => {
                tracing::debug!(
                    code = "TIMELINE_TITLE_CONTEXT_UNRESOLVED",
                    hwnd = window_id,
                    detail = %error,
                    "foreground window vanished before title readback"
                );
                return;
            }
        };
        let next = ForegroundSnapshot {
            hwnd: context.hwnd,
            pid: context.pid,
            process_name: context.process_name,
            process_path: context.process_path,
            title: context.window_title,
        };
        match classify_foreground_transition(Some(&previous), &next) {
            ForegroundTransition::Duplicate => return,
            ForegroundTransition::TitleChurnSuppressed(reason) => {
                self.record_suppressed_title_change(&next, reason);
                self.foreground = Some(next);
                return;
            }
            ForegroundTransition::Switched => {
                tracing::debug!(
                    code = "TIMELINE_NAMECHANGE_FOREGROUND_SWITCHED",
                    hwnd = window_id,
                    next_hwnd = next.hwnd,
                    next_pid = next.pid,
                    "NAMECHANGE readback no longer matches the previous foreground; next foreground poll will record the switch"
                );
                self.foreground = Some(next);
                return;
            }
            ForegroundTransition::TitleChanged => {}
        }
        let ts_ns = now_ts_ns();
        self.write_title_change(&next, ts_ns);
        self.assist.note_state_change();
        self.assist
            .record_dialog_title(ts_ns, &next, self.config.assist, &self.assist_sink);
        self.foreground = Some(next);
    }

    fn write_title_change(&self, next: &ForegroundSnapshot, ts_ns: u64) {
        let previous_title = self
            .foreground
            .as_ref()
            .map(|snapshot| snapshot.title.clone());
        self.writer.write_logged(
            ts_ns,
            TimelineKind::TitleChange,
            current_actor(),
            Some(next.process_name.clone()),
            json!({
                "title": next.title,
                "previous_title": previous_title,
                "pid": next.pid,
                "hwnd": next.hwnd,
            }),
        );
    }

    fn record_suppressed_title_change(&self, next: &ForegroundSnapshot, reason: TitleChurnReason) {
        synapse_telemetry::metrics::counter!(
            "timeline_title_changes_suppressed_total",
            "app" => next.process_name.clone(),
            "reason" => reason.as_str()
        )
        .increment(1);
        tracing::debug!(
            code = "TIMELINE_TITLE_CHANGE_SUPPRESSED",
            app = %next.process_name,
            hwnd = next.hwnd,
            pid = next.pid,
            reason = reason.as_str(),
            semantic_title_sha256 = %sha256_hex(&normalize_title_for_churn(&next.title).semantic),
            "suppressed volatile title-only churn; raw title omitted from metric labels and log"
        );
    }

    fn handle_idle_probe(&mut self, idle_ms: u64) {
        if self.writer.control.is_paused() {
            self.foreground = None;
            // The idle tick doubles as the auto-resume clock: a pause armed
            // with `duration_ms` reopens the gate within one poll interval.
            if self.writer.control.auto_resume_due(now_ts_ns()) {
                match resume_recording(&self.writer, "auto_resume") {
                    Ok(_state) => {
                        tracing::info!(
                            code = "TIMELINE_RECORDER_AUTO_RESUMED",
                            "timeline recorder auto-resumed: pause deadline passed"
                        );
                    }
                    Err(error) => {
                        tracing::error!(
                            code = "TIMELINE_RECORDER_AUTO_RESUME_FAILED",
                            detail = %format!("{error:#}"),
                            "timeline auto-resume failed; retrying next idle tick"
                        );
                        return;
                    }
                }
            } else {
                return;
            }
        }
        self.reconcile_foreground();
        let Some(edge) = idle_transition(self.idle, idle_ms, self.config.idle_timeout_ms) else {
            return;
        };
        // Backdate to the last-input instant: the timeline records when input
        // actually stopped/resumed, not when the coarse poll noticed.
        let ts_ns = now_ts_ns().saturating_sub(idle_ms.saturating_mul(1_000_000));
        match edge {
            IdleEdge::Start => {
                self.idle = true;
                self.writer.write_logged(
                    ts_ns,
                    TimelineKind::IdleStart,
                    TimelineActor::Human,
                    None,
                    json!({
                        "idle_ms_at_detection": idle_ms,
                        "idle_timeout_ms": self.config.idle_timeout_ms,
                    }),
                );
            }
            IdleEdge::End => {
                self.idle = false;
                self.writer.write_logged(
                    ts_ns,
                    TimelineKind::IdleEnd,
                    TimelineActor::Human,
                    None,
                    json!({ "idle_ms_at_detection": idle_ms }),
                );
            }
        }
    }

    /// Poll-driven safety net: if a foreground change was missed (hook
    /// hiccup, transient-only event stream), the next idle tick re-syncs the
    /// recorded state to reality, so the timeline can never silently diverge
    /// for longer than one poll interval.
    fn reconcile_foreground(&mut self) {
        let context = match synapse_a11y::current_foreground_context() {
            Ok(context) => context,
            Err(error) => {
                tracing::debug!(
                    code = "TIMELINE_FOREGROUND_NONE",
                    detail = %error,
                    "no foreground window at reconcile tick"
                );
                return;
            }
        };
        if !matches!(synapse_a11y::is_window_visible(context.hwnd), Ok(true)) {
            return;
        }
        self.apply_foreground(&context, "poll");
    }

    fn write_session_end(&self, edge: &str) -> Result<()> {
        if self.writer.suppressed(TimelineKind::SessionEnd, None) {
            return Ok(());
        }
        self.writer
            .try_write(
                now_ts_ns(),
                TimelineKind::SessionEnd,
                TimelineActor::Human,
                None,
                session_end_payload(&self.writer, edge),
            )
            .inspect_err(|error| {
                self.writer.write_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "TIMELINE_WRITE_FAILED",
                    kind = ?TimelineKind::SessionEnd,
                    detail = %format!("{error:#}"),
                    "failed to persist checked session_end row"
                );
            })
    }

    fn flush_interactions(&mut self) {
        self.interactions.flush(&self.writer);
    }
}

fn session_end_payload(writer: &TimelineWriter, edge: &str) -> serde_json::Value {
    json!({
        "pid": std::process::id(),
        "rows_written": writer.rows_written.load(Ordering::Relaxed),
        "write_failures": writer.write_failures.load(Ordering::Relaxed),
        "edge": edge,
    })
}

/// Outcome of a pause/resume control action, for tool readback (#843).
#[derive(Clone, Debug)]
pub struct RecorderControlOutcome {
    pub was_paused: bool,
    /// Whether a session boundary row was written (and flushed) for this
    /// transition. Re-pausing while paused / re-resuming while recording
    /// writes no row.
    pub boundary_row_written: bool,
    pub state: super::timeline_control::PersistedControlState,
}

/// Pause sequencing: boundary row while still recording, durable control row,
/// then the gate flips. A failure at any step propagates with the system left
/// in the last consistent state it reached.
fn pause_recording(
    writer: &TimelineWriter,
    paused_until_ns: Option<u64>,
    changed_by: &str,
) -> Result<RecorderControlOutcome> {
    let was_paused = writer.control.is_paused();
    let mut boundary_row_written = false;
    if !was_paused {
        writer
            .try_write(
                now_ts_ns(),
                TimelineKind::SessionEnd,
                TimelineActor::Human,
                None,
                json!({
                    "edge": "pause",
                    "by_session": changed_by,
                    "paused_until_ns": paused_until_ns,
                    "pid": std::process::id(),
                    "rows_written": writer.rows_written.load(Ordering::Relaxed),
                    "write_failures": writer.write_failures.load(Ordering::Relaxed),
                }),
            )
            .context("write session_end pause boundary row; recording is unchanged")?;
        writer
            .db
            .flush()
            .context("flush session_end pause boundary row; recording is unchanged")?;
        boundary_row_written = true;
    }
    let state =
        writer
            .control
            .persist_pause(&writer.db, paused_until_ns, now_ts_ns(), changed_by)?;
    tracing::info!(
        code = "TIMELINE_RECORDER_PAUSED",
        paused_until_ns,
        by_session = changed_by,
        "timeline recorder paused"
    );
    Ok(RecorderControlOutcome {
        was_paused,
        boundary_row_written,
        state,
    })
}

/// Resume sequencing: durable control row, the gate opens, then a
/// `session_start { edge: "resume" }` boundary row is written and flushed —
/// the resume-time proof that the write path works. A boundary failure is a
/// hard error: recording IS resumed at that point and the caller must know
/// the write path is broken.
fn resume_recording(writer: &TimelineWriter, changed_by: &str) -> Result<RecorderControlOutcome> {
    let was_paused = writer.control.is_paused();
    let state = writer
        .control
        .persist_resume(&writer.db, now_ts_ns(), changed_by)?;
    let mut boundary_row_written = false;
    if was_paused {
        writer
            .try_write(
                now_ts_ns(),
                TimelineKind::SessionStart,
                TimelineActor::Human,
                None,
                json!({
                    "edge": "resume",
                    "by_session": changed_by,
                    "pid": std::process::id(),
                }),
            )
            .context(
                "write session_start resume boundary row — recording IS resumed but the \
                 timeline write path is broken",
            )?;
        writer.db.flush().context(
            "flush session_start resume boundary row — recording IS resumed but the \
                 timeline write path is broken",
        )?;
        boundary_row_written = true;
        tracing::info!(
            code = "TIMELINE_RECORDER_RESUMED",
            by_session = changed_by,
            "timeline recorder resumed"
        );
    }
    Ok(RecorderControlOutcome {
        was_paused,
        boundary_row_written,
        state,
    })
}

async fn run_worker(
    mut receiver: mpsc::UnboundedReceiver<RecorderMessage>,
    mut state: WorkerState,
) {
    while let Some(message) = receiver.recv().await {
        match message {
            RecorderMessage::Accessible(event) => state.handle_accessible(&event),
            RecorderMessage::Interaction(event) => state.handle_interaction(&event),
            RecorderMessage::IdleProbe { idle_ms } => state.handle_idle_probe(idle_ms),
            RecorderMessage::FlushInteractions { done } => {
                state.flush_interactions();
                state.writer.flush_logged();
                let _ = done.send(());
            }
            RecorderMessage::Shutdown { done } => {
                state.flush_interactions();
                let storage_result = match state.write_session_end("shutdown") {
                    Ok(()) => state.writer.flush_checked().map_err(|error| {
                        state.writer.write_failures.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            code = "TIMELINE_FLUSH_FAILED",
                            detail = %format!("{error:#}"),
                            "failed to flush checked recorder shutdown boundary"
                        );
                        format!("{error:#}")
                    }),
                    Err(error) => Err(format!("{error:#}")),
                };
                let _ = done.send(storage_result);
                tracing::info!(
                    code = "TIMELINE_RECORDER_STOPPED",
                    rows_written = state.writer.rows_written.load(Ordering::Relaxed),
                    write_failures = state.writer.write_failures.load(Ordering::Relaxed),
                    "activity recorder stopped"
                );
                return;
            }
        }
    }
    tracing::warn!(
        code = "TIMELINE_RECORDER_CHANNEL_CLOSED",
        "activity recorder channel closed without shutdown; session_end is written by the drop backstop"
    );
}

async fn run_idle_probe(
    sender: mpsc::UnboundedSender<RecorderMessage>,
    poll_interval_ms: u64,
    cancel: CancellationToken,
) {
    let period = Duration::from_millis(poll_interval_ms.max(1));
    // First tick after one full period (not immediately): spawn already
    // probed the idle source, and the WinEvent path covers startup state.
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = interval.tick() => {}
        }
        match synapse_a11y::millis_since_last_input() {
            Ok(idle_ms) => {
                if sender.send(RecorderMessage::IdleProbe { idle_ms }).is_err() {
                    return;
                }
            }
            Err(error) => {
                tracing::error!(
                    code = "TIMELINE_IDLE_PROBE_FAILED",
                    detail = %error,
                    "idle probe failed; idle/active transitions are not being recorded this tick"
                );
            }
        }
    }
}

fn start_interaction_pipeline(
    recorder_sender: &mpsc::UnboundedSender<RecorderMessage>,
) -> Result<(InteractionHook, RecorderTaskShutdownOwner)> {
    let (interaction_tx, mut interaction_rx) = mpsc::unbounded_channel();
    let hook = InteractionHook::start(interaction_tx)?;
    let recorder_sender = recorder_sender.clone();
    let bridge = RecorderTaskShutdownOwner::new(
        "interaction_bridge",
        tokio::spawn(async move {
            while let Some(event) = interaction_rx.recv().await {
                if recorder_sender
                    .send(RecorderMessage::Interaction(event))
                    .is_err()
                {
                    return;
                }
            }
        }),
    );
    Ok((hook, bridge))
}

/// Always-on operator-activity recorder. One per daemon; owns the timeline
/// write path for foreground/title/idle/session rows.
pub struct ActivityRecorder {
    sender: mpsc::UnboundedSender<RecorderMessage>,
    writer: TimelineWriter,
    config: RecorderConfig,
    last_clipboard_sha256: Mutex<Option<String>>,
    browser_nav_dedupe_keys: Mutex<VecDeque<String>>,
    shutdown_requested: AtomicBool,
    sink_closed_logged: AtomicBool,
    producer_gate: RecorderProducerGate,
    idle_probe_cancel: CancellationToken,
    worker: Mutex<Option<RecorderTaskShutdownOwner>>,
    idle_probe: Mutex<Option<RecorderTaskShutdownOwner>>,
    interaction_hook: Mutex<Option<InteractionHook>>,
    interaction_bridge: Mutex<Option<RecorderTaskShutdownOwner>>,
    retired_interaction_bridges: Mutex<Vec<RecorderTaskShutdownOwner>>,
    interaction_hook_shutdown_reports:
        Mutex<Vec<super::interaction_cadence::InteractionHookShutdownReport>>,
    shutdown_report: Mutex<Option<ActivityRecorderShutdownReport>>,
    shutdown_supervisor: Mutex<Option<JoinHandle<()>>>,
    shutdown_supervisor_terminal: Mutex<Option<std::result::Result<(), String>>>,
    shutdown_completion: watch::Sender<Option<ActivityRecorderShutdownReport>>,
}

impl std::fmt::Debug for ActivityRecorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActivityRecorder")
            .field("config", &self.config)
            .field(
                "rows_written",
                &self.writer.rows_written.load(Ordering::Relaxed),
            )
            .field(
                "write_failures",
                &self.writer.write_failures.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl ActivityRecorder {
    /// Starts the recorder: probes the idle source once (fail-fast on a
    /// platform where idle tracking cannot work), writes the `session_start`
    /// row synchronously (fail-fast on a broken write path), then spawns the
    /// event worker and the idle-poll task.
    ///
    /// # Errors
    ///
    /// Returns an error when the idle probe or the `session_start` write
    /// fails; the daemon must refuse to start with a recorder that cannot
    /// record. A recorder hydrated into the paused state (#843) writes no
    /// `session_start` — paused means zero rows — unless its auto-resume
    /// deadline already passed while the daemon was down, in which case it
    /// resumes immediately.
    pub fn spawn(
        db: Arc<Db>,
        config: RecorderConfig,
        control: Arc<RecorderControl>,
        demo_recording: Arc<DemoRecordControl>,
        event_bus: EventBus,
    ) -> Result<Self> {
        let initial_idle_ms = synapse_a11y::millis_since_last_input()
            .context("probe GetLastInputInfo for the activity recorder idle source")?;
        if control.auto_resume_due(now_ts_ns()) {
            control
                .persist_resume(&db, now_ts_ns(), "startup_auto_resume")
                .context("auto-resume expired timeline pause at recorder startup")?;
            tracing::info!(
                code = "TIMELINE_RECORDER_AUTO_RESUMED",
                "timeline pause deadline passed while the daemon was down; resuming at startup"
            );
        }
        let writer = TimelineWriter {
            db,
            control,
            seq: Arc::new(AtomicU32::new(0)),
            rows_written: Arc::new(AtomicU64::new(0)),
            write_failures: Arc::new(AtomicU64::new(0)),
            rows_suppressed_paused: Arc::new(AtomicU64::new(0)),
            rows_suppressed_excluded: Arc::new(AtomicU64::new(0)),
            demo_recording,
        };
        if writer.control.is_paused() {
            tracing::info!(
                code = "TIMELINE_RECORDER_STARTED_PAUSED",
                paused_until_ns = writer.control.paused_until_ns(),
                "activity recorder started in the persisted paused state; no rows until resume"
            );
        } else {
            writer
                .try_write(
                    now_ts_ns(),
                    TimelineKind::SessionStart,
                    TimelineActor::Human,
                    None,
                    json!({
                        "edge": "startup",
                        "pid": std::process::id(),
                        "idle_timeout_ms": config.idle_timeout_ms,
                        "idle_poll_interval_ms": config.idle_poll_interval_ms,
                        "initial_idle_ms": initial_idle_ms,
                    }),
                )
                .context("write CF_TIMELINE session_start row at recorder startup")?;
            // Keep startup fail-loud by forcing an explicit WAL sync after
            // the initial session row is written.
            if let Err(primary) = writer
                .flush_checked()
                .context("flush CF_TIMELINE session_start row at recorder startup")
            {
                writer.write_failures.fetch_add(1, Ordering::Relaxed);
                let cleanup = writer
                    .try_write(
                        now_ts_ns(),
                        TimelineKind::SessionEnd,
                        TimelineActor::Human,
                        None,
                        session_end_payload(&writer, "startup_session_start_flush_failed"),
                    )
                    .context("write compensating session_end after startup flush failure")
                    .and_then(|()| {
                        writer
                            .flush_checked()
                            .context("flush compensating session_end after startup flush failure")
                    });
                match cleanup {
                    Ok(()) => anyhow::bail!(
                        "{primary:#}; compensating session_end was committed before startup unwind"
                    ),
                    Err(cleanup_error) => {
                        writer.write_failures.fetch_add(1, Ordering::Relaxed);
                        anyhow::bail!(
                            "{primary:#}; recorder startup storage cleanup also failed: {cleanup_error:#}"
                        );
                    }
                }
            }
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        let assist_sink = AssistEventSink {
            db: Arc::clone(&writer.db),
            event_bus,
            event_seq: Arc::new(AtomicU64::new(1)),
            storage_seq: Arc::new(AtomicU32::new(0)),
        };
        let state = WorkerState {
            writer: writer.clone(),
            config,
            foreground: None,
            idle: false,
            interactions: InteractionAccumulator::default(),
            assist: AssistDetector::default(),
            assist_sink,
        };
        // Interaction-hook installation is fallible and may itself create a
        // bridge task. Complete it before the infallible Tokio spawns below so
        // an installation error cannot detach already-started recorder owners.
        let (interaction_hook, interaction_bridge) = if writer.control.is_paused()
            || !config.interaction_hook_enabled
        {
            (None, None)
        } else {
            match start_interaction_pipeline(&sender) {
                Ok((hook, bridge)) => (Some(hook), Some(bridge)),
                Err(primary) => {
                    let cleanup = writer
                        .try_write(
                            now_ts_ns(),
                            TimelineKind::SessionEnd,
                            TimelineActor::Human,
                            None,
                            session_end_payload(&writer, "startup_interaction_hook_failed"),
                        )
                        .context("write session_end after interaction-hook startup failure")
                        .and_then(|()| {
                            writer
                                .flush_checked()
                                .context("flush session_end after interaction-hook startup failure")
                        });
                    if let Err(cleanup_error) = cleanup {
                        writer.write_failures.fetch_add(1, Ordering::Relaxed);
                        anyhow::bail!(
                            "start counts-only interaction cadence hook: {primary:#}; recorder startup storage cleanup also failed: {cleanup_error:#}"
                        );
                    }
                    return Err(primary).context("start counts-only interaction cadence hook");
                }
            }
        };
        let idle_probe_cancel = CancellationToken::new();
        let worker =
            RecorderTaskShutdownOwner::new("worker", tokio::spawn(run_worker(receiver, state)));
        let idle_probe = RecorderTaskShutdownOwner::new(
            "idle_probe",
            tokio::spawn(run_idle_probe(
                sender.clone(),
                config.idle_poll_interval_ms,
                idle_probe_cancel.clone(),
            )),
        );
        tracing::info!(
            code = "TIMELINE_RECORDER_STARTED",
            idle_timeout_ms = config.idle_timeout_ms,
            idle_poll_interval_ms = config.idle_poll_interval_ms,
            initial_idle_ms,
            interaction_hook_thread_id = interaction_hook
                .as_ref()
                .map(|hook| hook.readback().thread_id)
                .unwrap_or(0),
            "activity recorder started"
        );
        let (shutdown_completion, _shutdown_completion_rx) = watch::channel(None);
        Ok(Self {
            sender,
            writer,
            config,
            last_clipboard_sha256: Mutex::new(None),
            browser_nav_dedupe_keys: Mutex::new(VecDeque::new()),
            shutdown_requested: AtomicBool::new(false),
            sink_closed_logged: AtomicBool::new(false),
            producer_gate: RecorderProducerGate::default(),
            idle_probe_cancel,
            worker: Mutex::new(Some(worker)),
            idle_probe: Mutex::new(Some(idle_probe)),
            interaction_hook: Mutex::new(interaction_hook),
            interaction_bridge: Mutex::new(interaction_bridge),
            retired_interaction_bridges: Mutex::new(Vec::new()),
            interaction_hook_shutdown_reports: Mutex::new(Vec::new()),
            shutdown_report: Mutex::new(None),
            shutdown_supervisor: Mutex::new(None),
            shutdown_supervisor_terminal: Mutex::new(None),
            shutdown_completion,
        })
    }

    /// Cheap, non-blocking sink for the WinEvent bridge. Irrelevant kinds are
    /// filtered before crossing the channel.
    pub fn record_accessible_event(&self, event: &AccessibleEvent) {
        let Some(_producer_permit) = self.producer_gate.enter() else {
            return;
        };
        self.writer.demo_recording.record_accessible_event(event);
        if !matches!(
            event.kind,
            AccessibleEventKind::ForegroundChanged | AccessibleEventKind::NameChanged
        ) {
            return;
        }
        if self
            .sender
            .send(RecorderMessage::Accessible(event.clone()))
            .is_err()
            && !self.sink_closed_logged.swap(true, Ordering::Relaxed)
        {
            tracing::error!(
                code = "TIMELINE_RECORDER_DOWN",
                "activity recorder worker is gone; foreground timeline rows are no longer recorded"
            );
        }
    }

    /// Records observation-derived enrichment feeds (#839): plaintext
    /// clipboard snippets and full file-activity paths in `CF_TIMELINE`.
    ///
    /// Observation/audit CFs stay redacted; this method writes only the
    /// operator-decided plaintext timeline rows.
    pub fn record_observation_enrichment(
        &self,
        observation: &Observation,
        clipboard: Option<&ClipboardTimelineSample>,
        fs_events: &[FsTimelineEvent],
    ) {
        let Some(_producer_permit) = self.producer_gate.enter() else {
            return;
        };
        let mut wrote_any = false;
        if let Some(sample) = clipboard {
            wrote_any |= self.record_clipboard_sample(observation, sample);
        }
        for event in fs_events {
            wrote_any |= self.record_file_activity(observation, event);
        }
        if wrote_any {
            self.writer.flush_logged();
        }
    }

    pub fn record_browser_navigation(&self, event: BrowserNavigationEvent) -> bool {
        let Some(_producer_permit) = self.producer_gate.enter() else {
            return false;
        };
        let dedupe_key = browser_nav_dedupe_key(&event);
        let event = redact_browser_navigation_event(event);
        let url = event.url.trim();
        if url.is_empty() {
            tracing::warn!(
                code = "TIMELINE_BROWSER_NAV_EMPTY_URL",
                source = %event.source,
                "skipping browser navigation timeline row with empty URL"
            );
            return false;
        }
        let app = event
            .app
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| Some("chrome.exe".to_owned()));
        if self
            .writer
            .suppressed(TimelineKind::BrowserNav, app.as_deref())
        {
            return false;
        }
        if self.browser_nav_seen(&dedupe_key) {
            return false;
        }
        let mut payload = json!({
            "url": url,
            "title": event.title.as_str(),
            "tab_id": event.tab_id,
            "chrome_window_id": event.chrome_window_id,
            "window_hwnd": event.window_hwnd,
            "cdp_target_id": event.cdp_target_id.as_deref(),
            "endpoint": event.endpoint.as_deref(),
            "transport": event.transport.as_deref(),
            "source": event.source.as_str(),
            "event": event.event.as_str(),
            "action": event.action.as_deref(),
            "requested_url": event.requested_url.as_deref(),
            "before_url": event.before_url.as_deref(),
            "before_title": event.before_title.as_deref(),
            "ready_state": event.ready_state.as_deref(),
            "observed_at_unix_ms": event.observed_at_unix_ms,
            "active": event.active,
            "highlighted": event.highlighted,
            "pinned": event.pinned,
        });
        redact_url_fields_for_public_readback(&mut payload);
        match self.writer.try_write(
            now_ts_ns(),
            TimelineKind::BrowserNav,
            event.actor,
            app,
            payload,
        ) {
            Ok(()) => {
                self.remember_browser_nav_key(dedupe_key);
                self.writer.flush_logged();
                true
            }
            Err(error) => {
                self.writer.write_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "TIMELINE_BROWSER_NAV_WRITE_FAILED",
                    detail = %format!("{error:#}"),
                    "failed to persist browser navigation timeline row"
                );
                false
            }
        }
    }

    /// Graceful stop with terminal readback for every owned Tokio task.
    /// Repeated callers receive the same completed physical report.
    ///
    /// The exact-owner drain runs in a supervisor task holding its own `Arc`.
    /// Cancelling a caller therefore cannot drop the in-flight JoinHandles or
    /// detach recorder work. The supervisor's exact JoinHandle remains stored
    /// on the recorder, and every caller waits on the same completion channel.
    /// The wait itself is bounded: a supervisor that cannot reach a terminal
    /// result is aborted by exact handle, retained in the global owner ledger,
    /// and reported as non-graceful instead of hanging daemon shutdown.
    pub async fn shutdown(self: &Arc<Self>) -> ActivityRecorderShutdownReport {
        let mut completion = self.shutdown_completion.subscribe();
        {
            let mut supervisor = match self.shutdown_supervisor.lock() {
                Ok(supervisor) => supervisor,
                Err(poisoned) => poisoned.into_inner(),
            };
            let shutdown_already_terminal = self.cached_shutdown_report().is_some()
                || match self.shutdown_supervisor_terminal.lock() {
                    Ok(outcome) => outcome.is_some(),
                    Err(poisoned) => poisoned.into_inner().is_some(),
                };
            if supervisor.is_none() && !shutdown_already_terminal {
                let recorder = Arc::clone(self);
                *supervisor = Some(tokio::spawn(async move {
                    let outcome = AssertUnwindSafe(recorder.shutdown_inner())
                        .catch_unwind()
                        .await;
                    let report = match outcome {
                        Ok(report) => report,
                        Err(payload) => {
                            let detail = payload.downcast_ref::<&str>().map_or_else(
                                || {
                                    payload.downcast_ref::<String>().map_or_else(
                                        || "non-string panic payload".to_owned(),
                                        Clone::clone,
                                    )
                                },
                                |detail| (*detail).to_owned(),
                            );
                            recorder.shutdown_supervisor_failure_report(format!(
                                "recorder shutdown supervisor panicked: {detail}"
                            ))
                        }
                    };
                    recorder.publish_shutdown_report(report);
                }));
            }
        }

        let deadline = Instant::now() + RECORDER_SHUTDOWN_SUPERVISOR_TIMEOUT;
        loop {
            let cached_report = self.cached_shutdown_report();
            if let Some(supervisor_outcome) = self.account_shutdown_supervisor(false) {
                return match (cached_report, supervisor_outcome) {
                    (Some(report), Ok(())) => report,
                    (Some(report), Err(failure)) => {
                        let report = self.with_shutdown_supervisor_failure(report, failure);
                        self.publish_shutdown_report(report.clone());
                        report
                    }
                    (None, Ok(())) => {
                        let report = self.shutdown_supervisor_failure_report(
                            "recorder shutdown supervisor terminated without publishing a report"
                                .to_owned(),
                        );
                        self.publish_shutdown_report(report.clone());
                        report
                    }
                    (None, Err(failure)) => {
                        let report = self.shutdown_supervisor_failure_report(failure);
                        self.publish_shutdown_report(report.clone());
                        report
                    }
                };
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let failure = match self.account_shutdown_supervisor(true) {
                    Some(Ok(())) => {
                        return match self.cached_shutdown_report() {
                            Some(report) => report,
                            None => {
                                let report = self.shutdown_supervisor_failure_report(
                                    "recorder shutdown supervisor terminated without publishing a report"
                                        .to_owned(),
                                );
                                self.publish_shutdown_report(report.clone());
                                report
                            }
                        };
                    }
                    Some(Err(failure)) => failure,
                    None => format!(
                        "recorder shutdown supervisor exceeded its {} ms terminal deadline",
                        RECORDER_SHUTDOWN_SUPERVISOR_TIMEOUT.as_millis()
                    ),
                };
                let report = match self.cached_shutdown_report() {
                    Some(report) => self.with_shutdown_supervisor_failure(report, failure),
                    None => self.shutdown_supervisor_failure_report(failure),
                };
                self.publish_shutdown_report(report.clone());
                return report;
            }

            let poll = remaining.min(RECORDER_SHUTDOWN_SUPERVISOR_POLL_INTERVAL);
            match tokio::time::timeout(poll, completion.changed()).await {
                Ok(Ok(())) | Err(_) => {}
                Ok(Err(_closed)) => {
                    let failure =
                        "recorder shutdown completion channel closed before a report".to_owned();
                    let _ = self.account_shutdown_supervisor(true);
                    let report = match self.cached_shutdown_report() {
                        Some(report) => self.with_shutdown_supervisor_failure(report, failure),
                        None => self.shutdown_supervisor_failure_report(failure),
                    };
                    self.publish_shutdown_report(report.clone());
                    return report;
                }
            }
        }
    }

    fn cached_shutdown_report(&self) -> Option<ActivityRecorderShutdownReport> {
        match self.shutdown_report.lock() {
            Ok(report) => report.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn publish_shutdown_report(&self, report: ActivityRecorderShutdownReport) {
        match self.shutdown_report.lock() {
            Ok(mut cached) => *cached = Some(report.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(report.clone()),
        }
        self.shutdown_completion.send_replace(Some(report));
    }

    fn account_shutdown_supervisor(
        &self,
        abort_live: bool,
    ) -> Option<std::result::Result<(), String>> {
        let cached = match self.shutdown_supervisor_terminal.lock() {
            Ok(outcome) => outcome.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if cached.is_some() {
            return cached;
        }

        let mut supervisor = match self.shutdown_supervisor.lock() {
            Ok(supervisor) => supervisor,
            Err(poisoned) => poisoned.into_inner(),
        };
        let task_finished = supervisor.as_ref().is_some_and(JoinHandle::is_finished);
        if !task_finished && !abort_live {
            return None;
        }
        let Some(mut task) = supervisor.take() else {
            return None;
        };
        let outcome = if task_finished {
            match (&mut task).now_or_never() {
                Some(Ok(())) => Ok(()),
                Some(Err(error)) => Err(format!(
                    "recorder shutdown supervisor join failed after terminal readback: {error}"
                )),
                None => {
                    // `is_finished` is only a hint until the JoinHandle yields.
                    // Restore exact ownership and retry on the next poll.
                    *supervisor = Some(task);
                    return None;
                }
            }
        } else {
            task.abort();
            retain_recorder_task_owner("shutdown_supervisor_timeout", task);
            Err(format!(
                "recorder shutdown supervisor exceeded its {} ms terminal deadline; exact JoinHandle aborted and retained until physical termination",
                RECORDER_SHUTDOWN_SUPERVISOR_TIMEOUT.as_millis()
            ))
        };
        match self.shutdown_supervisor_terminal.lock() {
            Ok(mut cached) => *cached = Some(outcome.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(outcome.clone()),
        }
        drop(supervisor);
        Some(outcome)
    }

    fn with_shutdown_supervisor_failure(
        &self,
        mut report: ActivityRecorderShutdownReport,
        failure: String,
    ) -> ActivityRecorderShutdownReport {
        report.owner_accounting_complete = false;
        report.retained_task_owners = retained_task_owner_count();
        report.pipeline_task_owners_remaining = self.pipeline_task_owner_count();
        (report.producer_gate_closed, report.producer_gate_in_flight) =
            self.producer_gate.readback();
        if !report.failures.contains(&failure) {
            report.failures.push(failure.clone());
        }
        if !report.task_drain.failures.contains(&failure) {
            report.task_drain.failures.push(failure);
        }
        report
    }

    fn shutdown_supervisor_failure_report(
        &self,
        failure: String,
    ) -> ActivityRecorderShutdownReport {
        let (producer_gate_closed, producer_gate_in_flight) = self.producer_gate.readback();
        ActivityRecorderShutdownReport {
            shutdown_message_delivered: false,
            shutdown_reply_received: false,
            worker_boundary_committed: false,
            fallback_attempted: false,
            fallback_committed: false,
            producer_gate_closed,
            producer_gate_in_flight,
            pipeline_task_owners_remaining: self.pipeline_task_owner_count(),
            task_drain: ActivityRecorderTaskDrainReport {
                tasks_before: 0,
                graceful_joined: 0,
                abort_requests_sent: 0,
                joined_after_abort: 0,
                still_live_task_names: Vec::new(),
                failures: vec![failure.clone()],
            },
            owner_accounting_complete: false,
            retained_task_owners: retained_task_owner_count(),
            interaction_hook_owners_quiescent: false,
            rows_written: self.writer.rows_written.load(Ordering::Relaxed),
            write_failures: self.writer.write_failures.load(Ordering::Relaxed),
            failures: vec![failure],
        }
    }

    async fn shutdown_inner(&self) -> ActivityRecorderShutdownReport {
        // Close admission first, then wait for every synchronous writer or
        // pause/resume transition that entered before closure. A producer can
        // be inside storage or hook teardown, so this boundary is bounded too:
        // on timeout the recorder keeps every pipeline owner resident and
        // returns a fail-closed readback instead of killing work underneath a
        // still-active synchronous producer.
        let (producer_gate_closed, _producer_gate_in_flight_at_close) = self.producer_gate.close();
        self.shutdown_requested.store(true, Ordering::SeqCst);
        let (producer_gate_closed_after_drain, producer_gate_in_flight) = self
            .producer_gate
            .wait_for_quiescence_async(RECORDER_PRODUCER_DRAIN_TIMEOUT)
            .await;
        let producer_gate_closed = producer_gate_closed && producer_gate_closed_after_drain;
        if producer_gate_in_flight != 0 {
            return self.shutdown_supervisor_failure_report(format!(
                "{} synchronous recorder producer(s) remained in flight after the {} ms admission-drain deadline",
                producer_gate_in_flight,
                RECORDER_PRODUCER_DRAIN_TIMEOUT.as_millis()
            ));
        }
        debug_assert!(producer_gate_closed);
        self.idle_probe_cancel.cancel();
        let interaction_pipeline_expected =
            self.config.interaction_hook_enabled && !self.writer.control.is_paused();
        let current_hook_report_present = self.stop_interaction_hook("recorder_shutdown").is_some();
        let worker = self.take_task_owner(&self.worker);
        let idle_probe = self.take_task_owner(&self.idle_probe);
        let worker_owner_present = worker.is_some();
        let idle_probe_owner_present = idle_probe.is_some();
        let interaction_bridges = match self.retired_interaction_bridges.lock() {
            Ok(mut bridges) => std::mem::take(&mut *bridges),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        let interaction_bridge_owner_count = interaction_bridges.len();

        let mut failures = Vec::new();
        let mut producer_tasks = Vec::new();
        if let Some(idle_probe) = idle_probe {
            producer_tasks.push(idle_probe);
        } else {
            failures
                .push("activity recorder idle-probe JoinHandle was missing at shutdown".to_owned());
        }
        producer_tasks.extend(interaction_bridges);
        // Producers must reach a terminal state before the worker receives its
        // Shutdown boundary. Otherwise a bridge can enqueue cadence after the
        // worker has written session_end, silently reordering or dropping it.
        let producer_task_drain = drain_activity_recorder_tasks(producer_tasks).await;

        let (shutdown_message_delivered, mut shutdown_ack) = if worker.is_some() {
            let (done_tx, done_rx) = oneshot::channel();
            if self
                .sender
                .send(RecorderMessage::Shutdown { done: done_tx })
                .is_ok()
            {
                (true, Some(done_rx))
            } else {
                failures.push(
                    "activity recorder worker channel closed before shutdown message".to_owned(),
                );
                (false, None)
            }
        } else {
            failures.push("activity recorder worker JoinHandle was missing at shutdown".to_owned());
            tracing::error!(
                code = "TIMELINE_RECORDER_SHUTDOWN_WORKER_GONE",
                "activity recorder worker owner was missing at shutdown; direct boundary fallback is unsafe without terminal proof"
            );
            (false, None)
        };

        let mut worker_tasks = Vec::new();
        if let Some(worker) = worker {
            worker_tasks.push(worker);
        }
        let worker_task_drain = drain_activity_recorder_tasks(worker_tasks).await;
        let worker_proven_terminal = worker_owner_present && worker_task_drain.owners_quiescent();
        let task_drain = producer_task_drain.merge(worker_task_drain);
        let hook_reports = match self.interaction_hook_shutdown_reports.lock() {
            Ok(mut reports) => std::mem::take(&mut *reports),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        let interaction_hook_owners_quiescent = hook_reports
            .iter()
            .all(super::interaction_cadence::InteractionHookShutdownReport::owners_quiescent);
        let owner_accounting_complete = worker_owner_present
            && idle_probe_owner_present
            && (!interaction_pipeline_expected || current_hook_report_present)
            && hook_reports.len() == interaction_bridge_owner_count;
        for report in &hook_reports {
            if let Err(error) = report.verdict() {
                failures.push(format!("interaction hook: {error:#}"));
            }
        }

        let (shutdown_reply_received, worker_boundary_committed, fallback_edge) = match shutdown_ack
            .as_mut()
        {
            Some(receiver) => match receiver.try_recv() {
                Ok(Ok(())) => (true, true, None),
                Ok(Err(storage_error)) => {
                    failures.push(format!(
                        "activity recorder worker reported a failed shutdown storage boundary: {storage_error}"
                    ));
                    (true, false, Some("shutdown_storage_failed"))
                }
                Err(error) => {
                    failures.push(format!(
                        "activity recorder worker did not publish its shutdown reply after the join attempt: {error}"
                    ));
                    (false, false, Some("shutdown_unacknowledged"))
                }
            },
            None => (false, false, Some("shutdown_worker_gone")),
        };
        let mut fallback_attempted = false;
        let mut fallback_committed = false;
        if let Some(edge) = fallback_edge {
            if worker_proven_terminal {
                fallback_attempted = true;
                match self.write_session_end_direct(edge) {
                    Ok(()) => fallback_committed = true,
                    Err(fallback_error) => failures.push(format!(
                        "direct session_end fallback failed after worker boundary failure: {fallback_error:#}"
                    )),
                }
            } else {
                failures.push(
                    "direct session_end fallback was not attempted because the worker was not proven terminal"
                        .to_owned(),
                );
            }
        }
        let (rows_written, write_failures) = self.readback();
        let retained_task_owners = retained_task_owner_count();
        let report = ActivityRecorderShutdownReport {
            shutdown_message_delivered,
            shutdown_reply_received,
            worker_boundary_committed,
            fallback_attempted,
            fallback_committed,
            producer_gate_closed,
            producer_gate_in_flight,
            pipeline_task_owners_remaining: self.pipeline_task_owner_count(),
            task_drain,
            owner_accounting_complete,
            retained_task_owners,
            interaction_hook_owners_quiescent,
            rows_written,
            write_failures,
            failures,
        };
        tracing::info!(
            code = "TIMELINE_RECORDER_SHUTDOWN_READBACK",
            owners_quiescent = report.owners_quiescent(),
            report = ?report,
            "readback=activity_recorder_task_owners edge=shutdown after_join"
        );
        report
    }

    fn record_clipboard_sample(
        &self,
        observation: &Observation,
        sample: &ClipboardTimelineSample,
    ) -> bool {
        if !timeline_clipboard_enabled() {
            return false;
        }
        if self.writer.suppressed(
            TimelineKind::Clipboard,
            Some(&observation.foreground.process_name),
        ) {
            return false;
        }
        let mut last = match self.last_clipboard_sha256.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if last.as_deref() == Some(sample.text_sha256.as_str()) {
            return false;
        }
        let payload = json!({
            "snippet": sample.snippet.as_str(),
            "text_len": sample.text_len,
            "text_sha256": sample.text_sha256.as_str(),
            "formats": &sample.formats,
            "source_app": observation.foreground.process_name.as_str(),
            "source_process_path": observation.foreground.process_path.as_str(),
            "source_pid": observation.foreground.pid,
            "source_hwnd": observation.foreground.hwnd,
            "source_window_title": observation.foreground.window_title.as_str(),
            "observation_seq": observation.seq,
        });
        match self.writer.try_write(
            now_ts_ns(),
            TimelineKind::Clipboard,
            current_actor(),
            Some(observation.foreground.process_name.clone()),
            payload,
        ) {
            Ok(()) => {
                *last = Some(sample.text_sha256.clone());
                true
            }
            Err(error) => {
                self.writer.write_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "TIMELINE_CLIPBOARD_WRITE_FAILED",
                    detail = %format!("{error:#}"),
                    "failed to persist clipboard timeline row"
                );
                false
            }
        }
    }

    fn browser_nav_seen(&self, key: &str) -> bool {
        let guard = match self.browser_nav_dedupe_keys.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.iter().any(|existing| existing == key)
    }

    fn remember_browser_nav_key(&self, key: String) {
        let mut guard = match self.browser_nav_dedupe_keys.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.iter().any(|existing| existing == &key) {
            return;
        }
        guard.push_back(key);
        while guard.len() > MAX_BROWSER_NAV_DEDUPE_KEYS {
            let _ = guard.pop_front();
        }
    }

    fn record_file_activity(&self, observation: &Observation, event: &FsTimelineEvent) -> bool {
        if !timeline_file_activity_enabled() {
            return false;
        }
        if self.writer.suppressed(
            TimelineKind::FileActivity,
            Some(&observation.foreground.process_name),
        ) {
            return false;
        }
        let payload = json!({
            "path": event.path.as_str(),
            "event_kind": fs_event_kind_name(event.kind),
            "size_bytes": event.size_bytes,
            "observed_at": event.at.to_rfc3339(),
            "source_app": observation.foreground.process_name.as_str(),
            "source_process_path": observation.foreground.process_path.as_str(),
            "source_pid": observation.foreground.pid,
            "source_hwnd": observation.foreground.hwnd,
            "source_window_title": observation.foreground.window_title.as_str(),
            "observation_seq": observation.seq,
        });
        match self.writer.try_write(
            now_ts_ns(),
            TimelineKind::FileActivity,
            current_actor(),
            Some(observation.foreground.process_name.clone()),
            payload,
        ) {
            Ok(()) => true,
            Err(error) => {
                self.writer.write_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "TIMELINE_FILE_ACTIVITY_WRITE_FAILED",
                    detail = %format!("{error:#}"),
                    path = %event.path,
                    "failed to persist file-activity timeline row"
                );
                false
            }
        }
    }

    /// Live counters for health/manual FSV readback.
    #[must_use]
    pub fn readback(&self) -> (u64, u64) {
        (
            self.writer.rows_written.load(Ordering::Relaxed),
            self.writer.write_failures.load(Ordering::Relaxed),
        )
    }

    /// Suppressed-row counters: `(paused, excluded)` (#843 manual FSV readback).
    #[must_use]
    pub fn suppressed_counters(&self) -> (u64, u64) {
        (
            self.writer.rows_suppressed_paused.load(Ordering::Relaxed),
            self.writer.rows_suppressed_excluded.load(Ordering::Relaxed),
        )
    }

    /// Pauses recording: boundary row, durable control state, gate closed.
    ///
    /// # Errors
    ///
    /// Returns an error when the boundary row or the durable control write
    /// fails; the error states exactly which step failed and what state the
    /// recorder was left in.
    pub fn pause(
        &self,
        paused_until_ns: Option<u64>,
        changed_by: &str,
    ) -> Result<RecorderControlOutcome> {
        let _producer_permit = self
            .producer_gate
            .enter()
            .context("timeline recorder is shutting down; pause was not applied")?;
        if self.config.interaction_hook_enabled {
            self.flush_interactions_blocking();
        }
        let outcome = pause_recording(&self.writer, paused_until_ns, changed_by)?;
        if !outcome.was_paused {
            if let Some(report) = self.stop_interaction_hook("timeline_pause") {
                report
                    .verdict()
                    .context("timeline paused but interaction-hook shutdown was incomplete")?;
            }
        }
        Ok(outcome)
    }

    /// Resumes recording: durable control state, gate open, boundary row.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable control write fails (still paused)
    /// or when the boundary row fails (resumed, write path broken — the
    /// error says so explicitly).
    pub fn resume(&self, changed_by: &str) -> Result<RecorderControlOutcome> {
        let _producer_permit = self
            .producer_gate
            .enter()
            .context("timeline recorder is shutting down; resume was not applied")?;
        let outcome = resume_recording(&self.writer, changed_by)?;
        if outcome.was_paused && self.config.interaction_hook_enabled {
            self.start_interaction_hook()
                .context("timeline resumed but starting the interaction cadence hook failed")?;
        }
        Ok(outcome)
    }

    fn pipeline_task_owner_count(&self) -> usize {
        let worker = match self.worker.lock() {
            Ok(owner) => usize::from(owner.is_some()),
            Err(poisoned) => usize::from(poisoned.into_inner().is_some()),
        };
        let idle_probe = match self.idle_probe.lock() {
            Ok(owner) => usize::from(owner.is_some()),
            Err(poisoned) => usize::from(poisoned.into_inner().is_some()),
        };
        let interaction_bridge = match self.interaction_bridge.lock() {
            Ok(owner) => usize::from(owner.is_some()),
            Err(poisoned) => usize::from(poisoned.into_inner().is_some()),
        };
        let retired_interaction_bridges = match self.retired_interaction_bridges.lock() {
            Ok(owners) => owners.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        };
        worker + idle_probe + interaction_bridge + retired_interaction_bridges
    }

    fn take_task_owner(
        &self,
        slot: &Mutex<Option<RecorderTaskShutdownOwner>>,
    ) -> Option<RecorderTaskShutdownOwner> {
        match slot.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    fn start_interaction_hook(&self) -> Result<()> {
        let mut guard = match self.interaction_hook.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_some() {
            return Ok(());
        }
        let (hook, bridge) = start_interaction_pipeline(&self.sender)
            .context("start counts-only interaction cadence hook")?;
        tracing::info!(
            code = "TIMELINE_INTERACTION_HOOK_STARTED",
            thread_id = hook.readback().thread_id,
            keyboard_hook_installed = hook.readback().keyboard_hook_installed,
            mouse_hook_installed = hook.readback().mouse_hook_installed,
            "interaction cadence hook started"
        );
        *guard = Some(hook);
        match self.interaction_bridge.lock() {
            Ok(mut bridge_guard) => *bridge_guard = Some(bridge),
            Err(poisoned) => *poisoned.into_inner() = Some(bridge),
        }
        Ok(())
    }

    fn stop_interaction_hook(
        &self,
        reason: &'static str,
    ) -> Option<super::interaction_cadence::InteractionHookShutdownReport> {
        let mut guard = match self.interaction_hook.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let hook_report = guard
            .take()
            .map(|hook| hook.shutdown_checked(RECORDER_INTERACTION_HOOK_STOP_TIMEOUT, reason));
        drop(guard);
        if let Some(report) = hook_report.as_ref() {
            tracing::info!(
                code = "TIMELINE_INTERACTION_HOOK_STOPPED",
                owners_quiescent = report.owners_quiescent(),
                report = ?report,
                "interaction cadence hook shutdown completed with terminal readback"
            );
            match self.interaction_hook_shutdown_reports.lock() {
                Ok(mut reports) => reports.push(report.clone()),
                Err(poisoned) => poisoned.into_inner().push(report.clone()),
            }
        }
        let bridge = match self.interaction_bridge.lock() {
            Ok(mut bridge_guard) => bridge_guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(bridge) = bridge {
            // Dropping the hook closes its sender, so the bridge exits
            // cooperatively. Retain the exact JoinHandle until recorder
            // shutdown proves the owner terminal; abort-and-drop here would
            // detach it every time recording is paused.
            match self.retired_interaction_bridges.lock() {
                Ok(mut retired) => retired.push(bridge),
                Err(poisoned) => poisoned.into_inner().push(bridge),
            }
        }
        hook_report
    }

    fn flush_interactions_blocking(&self) {
        let (done_tx, mut done_rx) = oneshot::channel();
        if self
            .sender
            .send(RecorderMessage::FlushInteractions { done: done_tx })
            .is_err()
        {
            tracing::error!(
                code = "TIMELINE_INTERACTION_FLUSH_WORKER_GONE",
                "activity recorder worker is gone; interaction cadence bucket cannot be flushed"
            );
            return;
        }
        let deadline = Instant::now() + RECORDER_TASK_STOP_TIMEOUT;
        loop {
            match done_rx.try_recv() {
                Ok(()) => return,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        tracing::error!(
                            code = "TIMELINE_INTERACTION_FLUSH_TIMEOUT",
                            "activity recorder did not acknowledge interaction cadence flush"
                        );
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    tracing::error!(
                        code = "TIMELINE_INTERACTION_FLUSH_ACK_DROPPED",
                        "activity recorder did not acknowledge interaction cadence flush"
                    );
                    return;
                }
            }
        }
    }

    fn write_session_end_direct(&self, edge: &str) -> Result<()> {
        if !self.writer.suppressed(TimelineKind::SessionEnd, None) {
            self.writer
                .try_write(
                    now_ts_ns(),
                    TimelineKind::SessionEnd,
                    TimelineActor::Human,
                    None,
                    session_end_payload(&self.writer, edge),
                )
                .inspect_err(|error| {
                    self.writer.write_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        code = "TIMELINE_WRITE_FAILED",
                        kind = ?TimelineKind::SessionEnd,
                        detail = %format!("{error:#}"),
                        "failed to persist checked direct session_end row"
                    );
                })?;
        }
        self.writer.flush_checked().inspect_err(|error| {
            self.writer.write_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                code = "TIMELINE_FLUSH_FAILED",
                detail = %format!("{error:#}"),
                "failed to flush checked direct session_end boundary"
            );
        })
    }
}

const fn fs_event_kind_name(kind: FsEventKind) -> &'static str {
    match kind {
        FsEventKind::Created => "created",
        FsEventKind::Modified => "modified",
        FsEventKind::Deleted => "deleted",
        FsEventKind::Renamed => "renamed",
    }
}

impl Drop for ActivityRecorder {
    fn drop(&mut self) {
        let graceful_shutdown_started = self.shutdown_requested.swap(true, Ordering::SeqCst);
        // Drop must never wait for a synchronous producer: checked async
        // shutdown owns the bounded admission drain. Close admission, preserve
        // a process-global sticky incident for any unresolved permit, then
        // retain/abort the exact asynchronous owners below. The final daemon
        // lifetime-lock gate reads that global incident before admitting a
        // successor.
        let (producer_gate_closed, producer_gate_in_flight) =
            close_producer_gate_for_drop(&self.producer_gate);
        if producer_gate_in_flight != 0 {
            record_unresolved_drop_producers(producer_gate_in_flight);
            tracing::error!(
                code = "TIMELINE_RECORDER_DROP_PRODUCERS_UNRESOLVED",
                producer_gate_closed,
                producer_gate_in_flight,
                retained_owner_readback = ?retained_owner_readback(),
                "activity recorder Drop closed admission without blocking; unresolved synchronous producers remain a lifetime-lock incident"
            );
        }
        self.idle_probe_cancel.cancel();
        if let Some(mut probe) = self.take_task_owner(&self.idle_probe) {
            probe.abort_and_retain("recorder_drop");
        }
        if let Some(report) = self.stop_interaction_hook("recorder_drop")
            && let Err(error) = report.verdict()
        {
            tracing::error!(
                code = "TIMELINE_INTERACTION_HOOK_DROP_INCOMPLETE",
                detail = %format!("{error:#}"),
                report = ?report,
                "recorder drop could not prove the interaction-hook owner terminal"
            );
        }
        let retired = match self.retired_interaction_bridges.lock() {
            Ok(mut bridges) => std::mem::take(&mut *bridges),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for mut bridge in retired {
            bridge.abort_and_retain("recorder_drop");
        }
        if let Some(mut worker) = self.take_task_owner(&self.worker) {
            worker.abort_and_retain("recorder_drop");
        }
        let supervisor = match self.shutdown_supervisor.lock() {
            Ok(mut supervisor) => supervisor.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(mut supervisor) = supervisor {
            if supervisor.is_finished() {
                match (&mut supervisor).now_or_never() {
                    Some(Ok(())) => {}
                    Some(Err(error)) => tracing::error!(
                        code = "TIMELINE_RECORDER_SHUTDOWN_SUPERVISOR_DROP_JOIN_FAILED",
                        detail = %error,
                        "terminal recorder shutdown supervisor failed before recorder drop"
                    ),
                    None => {
                        supervisor.abort();
                        retain_recorder_task_owner(
                            "shutdown_supervisor_drop_readback_race",
                            supervisor,
                        );
                    }
                }
            } else {
                supervisor.abort();
                retain_recorder_task_owner("shutdown_supervisor_drop_backstop", supervisor);
            }
        }
        if !graceful_shutdown_started {
            // A synchronous Drop cannot await Tokio task termination. Writing
            // session_end here would falsely claim ordering while retained
            // producers or the worker may still run. Preserve exact owners and
            // emit a loud readback instead; checked shutdown owns the boundary.
            tracing::error!(
                code = "TIMELINE_RECORDER_DROP_WITHOUT_CHECKED_SHUTDOWN",
                retained_task_owners = retained_task_owner_count(),
                "activity recorder dropped without checked shutdown; no unordered session_end was written"
            );
        }
    }
}

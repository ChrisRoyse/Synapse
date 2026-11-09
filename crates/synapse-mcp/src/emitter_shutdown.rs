use std::{
    future::Future as _,
    io::Write as _,
    pin::Pin,
    sync::{Mutex, OnceLock},
    task::{Context, Poll, Waker},
    time::Duration,
};

use synapse_action::ActionStateSnapshot;
use tokio::{sync::watch, task::JoinHandle, time};

use crate::server::SynapseService;

pub(crate) const M2_EMITTER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const M2_EMITTER_TASK_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const M2_EMITTER_TASK_ABORT_TIMEOUT: Duration = Duration::from_secs(2);

trait ErasedShutdownTaskOwner: Send {
    fn poll_terminal_join(&mut self) -> Option<Result<(), String>>;
}

struct TypedShutdownTaskOwner<T: Send + 'static> {
    task: JoinHandle<T>,
}

impl<T: Send + 'static> ErasedShutdownTaskOwner for TypedShutdownTaskOwner<T> {
    fn poll_terminal_join(&mut self) -> Option<Result<(), String>> {
        if !self.task.is_finished() {
            return None;
        }
        let mut context = Context::from_waker(Waker::noop());
        match Pin::new(&mut self.task).poll(&mut context) {
            Poll::Ready(Ok(_output)) => Some(Ok(())),
            Poll::Ready(Err(error)) => Some(Err(error.to_string())),
            Poll::Pending => None,
        }
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Exact owner identity is retained for fatal shutdown diagnostics.
pub(crate) struct RetainedShutdownTaskOwner {
    pub(crate) owner_id: u64,
    pub(crate) task_label: &'static str,
    pub(crate) tokio_task_id: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Append-only evidence is emitted through the aggregate report.
pub(crate) struct RetainedShutdownTaskOwnerEvidence {
    pub(crate) sequence: u64,
    pub(crate) event: &'static str,
    pub(crate) owner: RetainedShutdownTaskOwner,
    pub(crate) detail: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Evidence vectors are intentionally read through diagnostic Debug output.
pub(crate) struct RetainedShutdownTaskOwnerReport {
    pub(crate) active_owner_count: usize,
    pub(crate) active_owners: Vec<RetainedShutdownTaskOwner>,
    pub(crate) retention_incident_count: usize,
    pub(crate) retention_incidents: Vec<RetainedShutdownTaskOwner>,
    pub(crate) evidence: Vec<RetainedShutdownTaskOwnerEvidence>,
}

impl RetainedShutdownTaskOwnerReport {
    /// A task reaches this ledger only after its surrounding shutdown owner was
    /// lost. Even a later successful join cannot reconstruct or validate the
    /// erased task output, so the incident permanently rejects a graceful
    /// lifetime-lock verdict for this process.
    pub(crate) const fn safe_to_unlock(&self) -> bool {
        self.active_owner_count == 0 && self.retention_incident_count == 0
    }
}

struct RetainedShutdownTask {
    owner: RetainedShutdownTaskOwner,
    task: Box<dyn ErasedShutdownTaskOwner>,
}

#[derive(Default)]
struct RetainedShutdownTaskRegistry {
    next_owner_id: u64,
    next_evidence_sequence: u64,
    active: Vec<RetainedShutdownTask>,
    retention_incidents: Vec<RetainedShutdownTaskOwner>,
    evidence: Vec<RetainedShutdownTaskOwnerEvidence>,
}

impl RetainedShutdownTaskRegistry {
    fn next_owner_id(&mut self) -> u64 {
        self.next_owner_id = self.next_owner_id.wrapping_add(1).max(1);
        self.next_owner_id
    }

    fn push_evidence(
        &mut self,
        event: &'static str,
        owner: RetainedShutdownTaskOwner,
        detail: String,
    ) {
        self.next_evidence_sequence = self.next_evidence_sequence.wrapping_add(1).max(1);
        let sequence = self.next_evidence_sequence;
        self.evidence.push(RetainedShutdownTaskOwnerEvidence {
            sequence,
            event,
            owner,
            detail,
        });
    }
}

fn retained_shutdown_task_registry() -> &'static Mutex<RetainedShutdownTaskRegistry> {
    static REGISTRY: OnceLock<Mutex<RetainedShutdownTaskRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(RetainedShutdownTaskRegistry::default()))
}

fn with_retained_shutdown_task_registry<R>(
    operation: impl FnOnce(&mut RetainedShutdownTaskRegistry) -> R,
) -> R {
    let mut registry = retained_shutdown_task_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    operation(&mut registry)
}

fn retain_shutdown_task<T: Send + 'static>(label: &'static str, task: JoinHandle<T>) {
    let tokio_task_id = format!("{:?}", task.id());
    let task: Box<dyn ErasedShutdownTaskOwner> = Box::new(TypedShutdownTaskOwner { task });
    with_retained_shutdown_task_registry(|registry| {
        let owner = RetainedShutdownTaskOwner {
            owner_id: registry.next_owner_id(),
            task_label: label,
            tokio_task_id,
        };
        registry.retention_incidents.push(owner.clone());
        registry.push_evidence(
            "retained_live_owner",
            owner.clone(),
            "surrounding shutdown future dropped before exact task reached a terminal join; this abnormal ownership transfer permanently rejects graceful lifetime-lock release"
                .to_owned(),
        );
        registry.active.push(RetainedShutdownTask { owner, task });
    });
}

fn record_unacknowledged_terminal_shutdown_task<T: Send + 'static>(
    label: &'static str,
    task: JoinHandle<T>,
) {
    let tokio_task_id = format!("{:?}", task.id());
    with_retained_shutdown_task_registry(|registry| {
        let owner = RetainedShutdownTaskOwner {
            owner_id: registry.next_owner_id(),
            task_label: label,
            tokio_task_id,
        };
        registry.retention_incidents.push(owner.clone());
        registry.push_evidence(
            "terminal_output_unacknowledged",
            owner,
            "the exact JoinHandle yielded Poll::Ready, but its surrounding shutdown phase was cancelled before acknowledging that the terminal output was incorporated into the cleanup verdict"
                .to_owned(),
        );
    });
    drop(task);
}

pub(crate) fn retained_shutdown_task_owner_report() -> RetainedShutdownTaskOwnerReport {
    with_retained_shutdown_task_registry(|registry| {
        let mut index = 0;
        while index < registry.active.len() {
            let terminal_join = registry.active[index].task.poll_terminal_join();
            let Some(terminal_join) = terminal_join else {
                index += 1;
                continue;
            };
            let retained = registry.active.swap_remove(index);
            let detail = match terminal_join {
                Ok(()) => "retained exact JoinHandle reached a successful terminal join, but its erased output cannot restore a graceful shutdown verdict".to_owned(),
                Err(error) => {
                    format!("retained exact JoinHandle reached terminal join error: {error}")
                }
            };
            registry.push_evidence("terminal_join_reaped", retained.owner, detail);
        }
        RetainedShutdownTaskOwnerReport {
            active_owner_count: registry.active.len(),
            active_owners: registry
                .active
                .iter()
                .map(|retained| retained.owner.clone())
                .collect(),
            retention_incident_count: registry.retention_incidents.len(),
            retention_incidents: registry.retention_incidents.clone(),
            evidence: registry.evidence.clone(),
        }
    })
}

/// Exact task owner that fails closed if its surrounding shutdown future is
/// cancelled. Tokio detaches a task when a live [`JoinHandle`] is dropped, so
/// shutdown supervisors must never keep a bare handle across an `.await`.
pub(crate) struct ShutdownTaskOwner<T: Send + 'static> {
    label: &'static str,
    task: Option<JoinHandle<T>>,
    terminal_join_observed: bool,
    terminal_outcome_acknowledged: bool,
}

impl<T: Send + 'static> ShutdownTaskOwner<T> {
    pub(crate) const fn new(label: &'static str, task: JoinHandle<T>) -> Self {
        Self {
            label,
            task: Some(task),
            terminal_join_observed: false,
            terminal_outcome_acknowledged: false,
        }
    }

    pub(crate) fn abort(&self) {
        let Some(task) = self.task.as_ref() else {
            unreachable!("shutdown task owner must contain its exact JoinHandle");
        };
        task.abort();
    }

    /// Returns true only after this wrapper has consumed the exact
    /// `JoinHandle` result. `JoinHandle::is_finished()` is deliberately not a
    /// substitute: a finished-but-unpolled handle can still carry a panic or
    /// cleanup output that the shutdown verdict has not observed.
    pub(crate) const fn terminal_join_observed(&self) -> bool {
        self.terminal_join_observed
    }

    /// Confirms that a previously observed `Poll::Ready` result has been
    /// synchronously incorporated into the caller's shutdown verdict. Merely
    /// polling the task terminal is insufficient: cancellation can otherwise
    /// drop the returned cleanup output before the report records it.
    pub(crate) fn acknowledge_terminal_outcome(&mut self) {
        assert!(
            self.terminal_join_observed,
            "shutdown task outcome cannot be acknowledged before Poll::Ready"
        );
        self.terminal_outcome_acknowledged = true;
    }
}

impl<T: Send + 'static> std::future::Future for ShutdownTaskOwner<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(task) = this.task.as_mut() else {
            unreachable!("shutdown task owner must contain its exact JoinHandle");
        };
        let result = Pin::new(task).poll(context);
        if result.is_ready() {
            this.terminal_join_observed = true;
        }
        result
    }
}

impl<T: Send + 'static> Drop for ShutdownTaskOwner<T> {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        if self.terminal_outcome_acknowledged {
            drop(task);
            return;
        }

        if self.terminal_join_observed {
            tracing::error!(
                code = "MCP_SHUTDOWN_TASK_TERMINAL_OUTPUT_UNACKNOWLEDGED",
                task = self.label,
                "shutdown owner reached a terminal join but its output was not acknowledged into the surrounding cleanup verdict"
            );
            record_unacknowledged_terminal_shutdown_task(self.label, task);
            return;
        }

        tracing::error!(
            code = "MCP_SHUTDOWN_TASK_OWNER_RETAINED",
            task = self.label,
            task_finished_hint = task.is_finished(),
            "shutdown owner was cancelled or timed out before its exact terminal join result was observed; retaining and polling the JoinHandle even when is_finished() is already true"
        );
        // `is_finished()` is only a scheduling hint, not consumption of the
        // task output. Move every unobserved owner into the process-global
        // registry so cancellation cannot erase a join failure or cleanup
        // verdict in the finished-before-wrapper-poll race.
        retain_shutdown_task(self.label, task);
    }
}

pub(crate) struct M2EmitterOwner {
    done: Option<watch::Receiver<Option<ActionStateSnapshot>>>,
    task: Option<ShutdownTaskOwner<ActionStateSnapshot>>,
    acquisition_failure: Option<String>,
}

impl M2EmitterOwner {
    pub(crate) fn done_receiver(&self) -> Option<watch::Receiver<Option<ActionStateSnapshot>>> {
        self.done.clone()
    }
}

#[derive(Debug)]
pub(crate) struct M2EmitterDrainReport {
    final_state_verified_empty: bool,
    task_owner_present: bool,
    task_terminal: bool,
    abort_requested: bool,
    failures: Vec<String>,
}

impl M2EmitterDrainReport {
    pub(crate) fn safe_to_unlock(&self) -> bool {
        self.final_state_verified_empty && self.task_owner_present && self.task_terminal
    }

    pub(crate) fn verdict(&self) -> anyhow::Result<()> {
        if self.failures.is_empty() && self.safe_to_unlock() && !self.abort_requested {
            Ok(())
        } else {
            anyhow::bail!(
                "M2 emitter shutdown failed: failures={:?}; readback={self:?}",
                self.failures
            )
        }
    }
}

pub(crate) fn take_m2_emitter_owner(service: &SynapseService) -> M2EmitterOwner {
    let done = service.m2_emitter_done_receiver();
    match service.take_m2_emitter_task() {
        Ok(task) => M2EmitterOwner {
            done,
            task: task.map(|task| ShutdownTaskOwner::new("m2_emitter", task)),
            acquisition_failure: None,
        },
        Err(error) => M2EmitterOwner {
            done,
            task: None,
            acquisition_failure: Some(error),
        },
    }
}

pub(crate) async fn drain_m2_emitter_owner(
    owner: Option<M2EmitterOwner>,
    transport: &'static str,
    source: &'static str,
) -> M2EmitterDrainReport {
    let Some(mut owner) = owner else {
        return M2EmitterDrainReport {
            final_state_verified_empty: false,
            task_owner_present: false,
            task_terminal: false,
            abort_requested: false,
            failures: vec![format!(
                "M2 emitter owner was already consumed before {transport} shutdown source={source}"
            )],
        };
    };
    let mut failures = Vec::new();
    if let Some(error) = owner.acquisition_failure.take() {
        failures.push(error);
    }
    let final_state_verified_empty =
        match wait_for_m2_emitter_done(owner.done, transport, source).await {
            Ok(()) => true,
            Err(error) => {
                failures.push(error.to_string());
                false
            }
        };
    let task_owner_present = owner.task.is_some();
    let (task_terminal, abort_requested) = match owner.task.take() {
        Some(mut task) => match time::timeout(M2_EMITTER_TASK_STOP_TIMEOUT, &mut task).await {
            Ok(result) => {
                let outcome = match result {
                    Ok(_snapshot) => (true, false),
                    Err(error) => {
                        failures.push(format!("M2 emitter task join failed: {error}"));
                        (true, false)
                    }
                };
                task.acknowledge_terminal_outcome();
                outcome
            }
            Err(_elapsed) => {
                task.abort();
                match time::timeout(M2_EMITTER_TASK_ABORT_TIMEOUT, &mut task).await {
                    Ok(result) => {
                        failures.push(format!(
                            "M2 emitter did not stop within {} ms after cancellation; abort_join={result:?}",
                            M2_EMITTER_TASK_STOP_TIMEOUT.as_millis()
                        ));
                        task.acknowledge_terminal_outcome();
                        (true, true)
                    }
                    Err(_elapsed) => {
                        failures.push(format!(
                            "M2 emitter did not stop within {} ms after cancellation and did not join within {} ms after abort; exact JoinHandle retained until process teardown",
                            M2_EMITTER_TASK_STOP_TIMEOUT.as_millis(),
                            M2_EMITTER_TASK_ABORT_TIMEOUT.as_millis()
                        ));
                        (false, true)
                    }
                }
            }
        },
        None => {
            failures.push("M2 emitter JoinHandle owner was missing at shutdown".to_owned());
            (false, false)
        }
    };
    M2EmitterDrainReport {
        final_state_verified_empty,
        task_owner_present,
        task_terminal,
        abort_requested,
        failures,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActionStateCounts {
    pub(crate) held_keys: usize,
    pub(crate) held_key_bits: usize,
    pub(crate) held_key_timer_keys: usize,
    pub(crate) held_key_timer_count: usize,
    pub(crate) held_buttons: usize,
    pub(crate) held_button_bits: usize,
    pub(crate) held_pads: usize,
    pub(crate) held_keys_by_backend_entries: usize,
    pub(crate) held_keys_by_backend_nonempty_entries: usize,
    pub(crate) held_keys_by_backend_values: usize,
    pub(crate) held_buttons_by_backend_entries: usize,
    pub(crate) held_buttons_by_backend_nonempty_entries: usize,
    pub(crate) held_buttons_by_backend_values: usize,
}

impl ActionStateCounts {
    fn from_snapshot(snapshot: &ActionStateSnapshot) -> Self {
        Self {
            held_keys: snapshot.held_keys.len(),
            held_key_bits: snapshot.held_key_bits.len(),
            held_key_timer_keys: snapshot.held_key_timer_keys.len(),
            held_key_timer_count: snapshot.held_key_timer_count,
            held_buttons: snapshot.held_buttons.len(),
            held_button_bits: snapshot.held_button_bits.len(),
            held_pads: snapshot.pad_state.len(),
            held_keys_by_backend_entries: snapshot.held_keys_by_backend.len(),
            held_keys_by_backend_nonempty_entries: snapshot
                .held_keys_by_backend
                .values()
                .filter(|values| !values.is_empty())
                .count(),
            held_keys_by_backend_values: snapshot.held_keys_by_backend.values().map(Vec::len).sum(),
            held_buttons_by_backend_entries: snapshot.held_buttons_by_backend.len(),
            held_buttons_by_backend_nonempty_entries: snapshot
                .held_buttons_by_backend
                .values()
                .filter(|values| !values.is_empty())
                .count(),
            held_buttons_by_backend_values: snapshot
                .held_buttons_by_backend
                .values()
                .map(Vec::len)
                .sum(),
        }
    }

    fn is_empty(self) -> bool {
        self.held_keys == 0
            && self.held_key_bits == 0
            && self.held_key_timer_keys == 0
            && self.held_key_timer_count == 0
            && self.held_buttons == 0
            && self.held_button_bits == 0
            && self.held_pads == 0
            && self.held_keys_by_backend_entries == 0
            && self.held_keys_by_backend_nonempty_entries == 0
            && self.held_keys_by_backend_values == 0
            && self.held_buttons_by_backend_entries == 0
            && self.held_buttons_by_backend_nonempty_entries == 0
            && self.held_buttons_by_backend_values == 0
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum M2EmitterDrainError {
    MissingReceiver {
        transport: &'static str,
        source: &'static str,
    },
    ChannelClosed {
        transport: &'static str,
        source: &'static str,
    },
    Timeout {
        transport: &'static str,
        source: &'static str,
        timeout_ms: u128,
    },
    NonEmptyFinalState {
        transport: &'static str,
        source: &'static str,
        counts: ActionStateCounts,
    },
}

impl M2EmitterDrainError {
    fn code(&self) -> &'static str {
        match self {
            Self::MissingReceiver { .. } => "MCP_M2_EMITTER_RECEIVER_MISSING",
            Self::ChannelClosed { .. } => "MCP_M2_EMITTER_CHANNEL_CLOSED",
            Self::Timeout { .. } => "MCP_M2_EMITTER_SHUTDOWN_TIMEOUT",
            Self::NonEmptyFinalState { .. } => "MCP_M2_EMITTER_FINAL_STATE_NONEMPTY",
        }
    }

    fn transport(&self) -> &'static str {
        match self {
            Self::MissingReceiver { transport, .. }
            | Self::ChannelClosed { transport, .. }
            | Self::Timeout { transport, .. }
            | Self::NonEmptyFinalState { transport, .. } => transport,
        }
    }

    fn source(&self) -> &'static str {
        match self {
            Self::MissingReceiver { source, .. }
            | Self::ChannelClosed { source, .. }
            | Self::Timeout { source, .. }
            | Self::NonEmptyFinalState { source, .. } => source,
        }
    }

    fn timeout_ms(&self) -> Option<u128> {
        match self {
            Self::Timeout { timeout_ms, .. } => Some(*timeout_ms),
            _ => None,
        }
    }

    fn final_state_counts(&self) -> Option<ActionStateCounts> {
        match self {
            Self::NonEmptyFinalState { counts, .. } => Some(*counts),
            _ => None,
        }
    }
}

impl std::fmt::Display for M2EmitterDrainError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingReceiver { transport, source } => write!(
                formatter,
                "{} transport={transport} source={source}: final M2 state cannot be observed during daemon shutdown",
                self.code()
            ),
            Self::ChannelClosed { transport, source } => write!(
                formatter,
                "{} transport={transport} source={source}: emitter ended without publishing its final M2 state",
                self.code()
            ),
            Self::Timeout {
                transport,
                source,
                timeout_ms,
            } => write!(
                formatter,
                "{} transport={transport} source={source} timeout_ms={timeout_ms}: emitter did not publish final M2 state before the shutdown deadline",
                self.code()
            ),
            Self::NonEmptyFinalState {
                transport,
                source,
                counts,
            } => write!(
                formatter,
                "{} transport={transport} source={source} counts={counts:?}: emitter published a nonempty final input state",
                self.code()
            ),
        }
    }
}

impl std::error::Error for M2EmitterDrainError {}

fn report_error(error: &M2EmitterDrainError) {
    let code = error.code();
    let transport = error.transport();
    let source = error.source();
    let timeout_ms = error.timeout_ms();
    let final_state_counts = error.final_state_counts();
    tracing::error!(
        code,
        transport,
        source,
        timeout_ms,
        final_state_counts = ?final_state_counts,
        error = %error,
        "daemon shutdown cannot verify an empty final M2 emitter state"
    );

    // This path can run while telemetry is itself shutting down. Preserve the
    // same actionable evidence on stderr without panicking if stderr is closed.
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(
        stderr,
        "synapse-mcp shutdown error: code={code} transport={transport} source={source} timeout_ms={timeout_ms:?} final_state_counts={final_state_counts:?} detail={error}"
    );
}

pub(crate) async fn wait_for_m2_emitter_done(
    done: Option<watch::Receiver<Option<ActionStateSnapshot>>>,
    transport: &'static str,
    source: &'static str,
) -> Result<(), M2EmitterDrainError> {
    wait_for_m2_emitter_done_with_timeout(done, transport, source, M2_EMITTER_SHUTDOWN_TIMEOUT)
        .await
}

async fn wait_for_m2_emitter_done_with_timeout(
    done: Option<watch::Receiver<Option<ActionStateSnapshot>>>,
    transport: &'static str,
    source: &'static str,
    timeout: Duration,
) -> Result<(), M2EmitterDrainError> {
    let Some(mut done) = done else {
        let error = M2EmitterDrainError::MissingReceiver { transport, source };
        report_error(&error);
        return Err(error);
    };

    let wait_result = tokio::time::timeout(timeout, async {
        loop {
            if done.borrow().is_some() {
                return Ok(());
            }
            if done.changed().await.is_err() {
                return Err(M2EmitterDrainError::ChannelClosed { transport, source });
            }
        }
    })
    .await;

    let snapshot = match wait_result {
        Ok(Ok(())) => {
            let done_snapshot = done.borrow().as_ref().cloned();
            match done_snapshot {
                Some(snapshot) => snapshot,
                None => {
                    let error = M2EmitterDrainError::ChannelClosed { transport, source };
                    report_error(&error);
                    return Err(error);
                }
            }
        }
        Ok(Err(error)) => {
            report_error(&error);
            return Err(error);
        }
        Err(_elapsed) => {
            let error = M2EmitterDrainError::Timeout {
                transport,
                source,
                timeout_ms: timeout.as_millis(),
            };
            report_error(&error);
            return Err(error);
        }
    };

    let counts = ActionStateCounts::from_snapshot(&snapshot);
    if !counts.is_empty() {
        let error = M2EmitterDrainError::NonEmptyFinalState {
            transport,
            source,
            counts,
        };
        report_error(&error);
        return Err(error);
    }

    tracing::info!(
        code = "MCP_M2_EMITTER_SHUTDOWN_DONE",
        transport,
        source,
        counts = ?counts,
        "readback=action_emitter_state edge=daemon_shutdown after_emitter_done"
    );
    Ok(())
}

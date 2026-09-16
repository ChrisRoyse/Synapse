use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::rate_limits::{BackendRateLimitControl, BackendRateLimits};
use super::{
    ActionEmitter, ActionEmitterSnapshotHandle, ActionSnapshotMessage, ActionStateSnapshot,
    Backends, EmitState,
};
use crate::{
    ACTION_QUEUE_CAPACITY, ActionBackend, ActionError, ActionHandle, ActionMessage, ActionResult,
    BackendResolutionPolicy,
};
use synapse_core::{Action, error_codes};

impl ActionEmitter {
    #[must_use]
    #[tracing::instrument(skip_all, fields(action_kind = "new"))]
    pub fn new(
        rx: mpsc::Receiver<ActionMessage>,
        snapshot_rx: mpsc::Receiver<ActionSnapshotMessage>,
    ) -> Self {
        Self::new_with_backends(rx, snapshot_rx, Backends::production())
    }

    #[must_use]
    pub fn new_with_backends(
        rx: mpsc::Receiver<ActionMessage>,
        snapshot_rx: mpsc::Receiver<ActionSnapshotMessage>,
        backends: Backends,
    ) -> Self {
        Self::new_with_backends_and_policy(
            rx,
            snapshot_rx,
            backends,
            Arc::new(RwLock::new(BackendResolutionPolicy::default())),
        )
    }

    #[must_use]
    pub fn new_with_backends_and_policy(
        rx: mpsc::Receiver<ActionMessage>,
        snapshot_rx: mpsc::Receiver<ActionSnapshotMessage>,
        backends: Backends,
        backend_resolution: Arc<RwLock<BackendResolutionPolicy>>,
    ) -> Self {
        Self::new_with_backends_and_policy_with_safety(
            rx,
            empty_safety_rx(),
            snapshot_rx,
            backends,
            backend_resolution,
        )
    }

    fn new_with_backends_and_policy_with_safety(
        rx: mpsc::Receiver<ActionMessage>,
        safety_rx: mpsc::UnboundedReceiver<ActionMessage>,
        snapshot_rx: mpsc::Receiver<ActionSnapshotMessage>,
        backends: Backends,
        backend_resolution: Arc<RwLock<BackendResolutionPolicy>>,
    ) -> Self {
        let (auto_release_tx, auto_release_rx) = mpsc::channel(ACTION_QUEUE_CAPACITY);
        Self {
            rx,
            safety_rx,
            snapshot_rx,
            auto_release_tx,
            auto_release_rx,
            state: EmitState::new(),
            backends,
            backend_resolution,
            rate_limits: BackendRateLimitControl::new(BackendRateLimits::new()),
            held_key_timers: HashMap::new(),
            held_key_timer_ids: HashMap::new(),
            next_held_key_timer_id: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn with_rate_limits(
        rx: mpsc::Receiver<ActionMessage>,
        safety_rx: mpsc::UnboundedReceiver<ActionMessage>,
        snapshot_rx: mpsc::Receiver<ActionSnapshotMessage>,
        backends: Backends,
        rate_limits: BackendRateLimits,
    ) -> Self {
        Self::with_rate_limits_and_policy(
            rx,
            safety_rx,
            snapshot_rx,
            backends,
            rate_limits,
            Arc::new(RwLock::new(BackendResolutionPolicy::default())),
        )
    }

    #[cfg(test)]
    pub(super) fn with_rate_limits_and_policy(
        rx: mpsc::Receiver<ActionMessage>,
        safety_rx: mpsc::UnboundedReceiver<ActionMessage>,
        snapshot_rx: mpsc::Receiver<ActionSnapshotMessage>,
        backends: Backends,
        rate_limits: BackendRateLimits,
        backend_resolution: Arc<RwLock<BackendResolutionPolicy>>,
    ) -> Self {
        let (auto_release_tx, auto_release_rx) = mpsc::channel(ACTION_QUEUE_CAPACITY);
        Self {
            rx,
            safety_rx,
            snapshot_rx,
            auto_release_tx,
            auto_release_rx,
            state: EmitState::new(),
            backends,
            backend_resolution,
            rate_limits: BackendRateLimitControl::new(rate_limits),
            held_key_timers: HashMap::new(),
            held_key_timer_ids: HashMap::new(),
            next_held_key_timer_id: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn channel_with_rate_limits(
        rate_limits: BackendRateLimits,
    ) -> (ActionHandle, ActionEmitterSnapshotHandle, Self) {
        let backends = Backends::all_routed_to(Arc::new(crate::RecordingBackend::new()));
        let (handle, rx, safety_rx) = ActionHandle::channel_with_safety_lane();
        let (snapshot_tx, snapshot_rx) = mpsc::channel(ACTION_QUEUE_CAPACITY);
        (
            handle,
            ActionEmitterSnapshotHandle::new(snapshot_tx),
            Self::with_rate_limits(rx, safety_rx, snapshot_rx, backends, rate_limits),
        )
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(action_kind = "channel"))]
    pub fn channel() -> (ActionHandle, ActionEmitterSnapshotHandle, Self) {
        Self::channel_with_backends(Backends::production())
    }

    #[must_use]
    pub fn channel_with_backends(
        backends: Backends,
    ) -> (ActionHandle, ActionEmitterSnapshotHandle, Self) {
        Self::channel_with_backends_and_policy(
            backends,
            Arc::new(RwLock::new(BackendResolutionPolicy::default())),
        )
    }

    #[must_use]
    pub fn channel_with_backends_and_policy(
        backends: Backends,
        backend_resolution: Arc<RwLock<BackendResolutionPolicy>>,
    ) -> (ActionHandle, ActionEmitterSnapshotHandle, Self) {
        let (handle, rx, safety_rx) = ActionHandle::channel_with_safety_lane();
        let (snapshot_tx, snapshot_rx) = mpsc::channel(ACTION_QUEUE_CAPACITY);
        (
            handle,
            ActionEmitterSnapshotHandle::new(snapshot_tx),
            Self::new_with_backends_and_policy_with_safety(
                rx,
                safety_rx,
                snapshot_rx,
                backends,
                backend_resolution,
            ),
        )
    }

    #[must_use]
    pub fn channel_with_backend(
        backend: Arc<dyn ActionBackend>,
    ) -> (ActionHandle, ActionEmitterSnapshotHandle, Self) {
        Self::channel_with_backends(Backends::all_routed_to(backend))
    }

    #[must_use]
    pub fn rate_limit_control(&self) -> BackendRateLimitControl {
        self.rate_limits.clone()
    }

    /// Spawns the action serialization actor on the current Tokio runtime.
    ///
    /// The returned join handle yields the actor's final held-state snapshot
    /// after shutdown release handling.
    #[must_use]
    #[tracing::instrument(skip_all, fields(action_kind = "spawn"))]
    pub fn spawn(
        cancel: CancellationToken,
    ) -> (
        ActionHandle,
        ActionEmitterSnapshotHandle,
        JoinHandle<ActionStateSnapshot>,
    ) {
        let (handle, snapshot_handle, emitter) = Self::channel();
        let join = tokio::spawn(emitter.run(cancel));
        (handle, snapshot_handle, join)
    }

    /// Spawns an actor wired with a single substitute backend across all
    /// resolved kinds. Intended for cross-platform tests that want to observe
    /// actor dispatch without driving real input devices.
    #[must_use]
    pub fn spawn_with_backend(
        cancel: CancellationToken,
        backend: Arc<dyn ActionBackend>,
    ) -> (
        ActionHandle,
        ActionEmitterSnapshotHandle,
        JoinHandle<ActionStateSnapshot>,
    ) {
        let (handle, snapshot_handle, emitter) = Self::channel_with_backend(backend);
        let join = tokio::spawn(emitter.run(cancel));
        (handle, snapshot_handle, join)
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(action_kind = "pending_len"))]
    pub fn pending_len(&self) -> usize {
        self.rx.len()
    }

    #[tracing::instrument(skip_all, fields(action_kind = "run"))]
    pub async fn run(self, cancel: CancellationToken) -> ActionStateSnapshot {
        self.run_with_connection_closed_cancel(cancel, None).await
    }

    #[tracing::instrument(skip_all, fields(action_kind = "run"))]
    pub async fn run_with_connection_closed_cancel(
        self,
        shutdown_cancel: CancellationToken,
        connection_closed_cancel: Option<CancellationToken>,
    ) -> ActionStateSnapshot {
        self.run_with_shutdown_reason(shutdown_cancel, "shutdown", connection_closed_cancel)
            .await
    }

    #[tracing::instrument(skip_all, fields(action_kind = "run"))]
    pub async fn run_with_shutdown_reason(
        mut self,
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: Option<CancellationToken>,
    ) -> ActionStateSnapshot {
        loop {
            tokio::select! {
                biased;
                Some((action, ack, operator_panic_epoch_at_enqueue)) = self.safety_rx.recv() => {
                    self.execute_actor_message(action, ack, operator_panic_epoch_at_enqueue).await;
                },
                Some(auto_release) = self.auto_release_rx.recv() => {
                    if let Some(emitted_action) = self.auto_release_held_key(&auto_release) {
                        let _release_result = self.execute(emitted_action).await;
                    }
                },
                Some(snapshot_ack) = self.snapshot_rx.recv() => {
                    let _send_result = snapshot_ack.send(self.snapshot());
                },
                () = shutdown_cancel.cancelled() => {
                    self.release_all(shutdown_reason).await;
                    return self.snapshot();
                },
                () = connection_closed_cancelled(connection_closed_cancel.as_ref()), if connection_closed_cancel.is_some() => {
                    self.release_all("connection_closed").await;
                    return self.snapshot();
                },
                Some((action, ack, operator_panic_epoch_at_enqueue)) = self.rx.recv() => {
                    self.execute_actor_message(action, ack, operator_panic_epoch_at_enqueue).await;
                },
                else => {
                    self.release_all("connection_closed").await;
                    return self.snapshot();
                }
            }
        }
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(action_kind = "snapshot_state"))]
    pub fn snapshot(&self) -> ActionStateSnapshot {
        let mut snapshot = self.state.snapshot();
        snapshot.held_key_timer_keys = self.held_key_timer_keys();
        snapshot.held_key_timer_count = self.held_key_timers.len();
        snapshot
    }

    async fn execute_actor_message(
        &mut self,
        action: Action,
        ack: tokio::sync::oneshot::Sender<ActionResult<()>>,
        operator_panic_epoch_at_enqueue: Option<u64>,
    ) {
        let is_release_all = matches!(action, Action::ReleaseAll);
        let result = match crate::handle::ensure_operator_panic_allows_action_since(
            &action,
            operator_panic_epoch_at_enqueue,
        ) {
            Ok(()) => self.execute(action).await,
            Err(error) => Err(error),
        };
        let _send_result = ack.send(result);
        if is_release_all {
            self.reject_pending_normal_actions_after_release_all();
        }
    }

    fn reject_pending_normal_actions_after_release_all(&mut self) {
        let mut rejected = 0_u32;
        while let Ok((action, ack, _operator_panic_epoch_at_enqueue)) = self.rx.try_recv() {
            rejected = rejected.saturating_add(1);
            let kind = super::routing::action_kind(&action);
            let _send_result = ack.send(Err(ActionError::SafetyReleaseAllFired {
                detail: format!(
                    "pending {kind} action discarded because release_all preempted the action queue"
                ),
            }));
        }
        if rejected > 0 {
            tracing::warn!(
                code = error_codes::SAFETY_RELEASE_ALL_FIRED,
                rejected_pending_actions = rejected,
                "release_all preempted and rejected pending normal action queue items"
            );
        }
    }
}
impl Drop for ActionEmitter {
    fn drop(&mut self) {
        self.abort_all_held_key_timers();
    }
}

fn empty_safety_rx() -> mpsc::UnboundedReceiver<ActionMessage> {
    let (_tx, rx) = mpsc::unbounded_channel();
    rx
}

async fn connection_closed_cancelled(cancel: Option<&CancellationToken>) {
    if let Some(cancel) = cancel {
        cancel.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

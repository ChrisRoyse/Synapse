use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use synapse_core::{
    Action, Backend, ButtonAction, ComboInput, ComboStep, GamepadController, GamepadReport, Key,
    MouseButton, PadId,
};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use crate::{ActionError, ActionResult, validate_action};

pub const ACTION_QUEUE_CAPACITY: usize = 256;

/// Emitter queue item carrying the operator-panic epoch from the enqueue edge.
///
/// A normal action must still match this epoch at physical dispatch; otherwise
/// an item queued before K1 could resume after a fully finalized K2 wave.
pub type ActionMessage = (Action, oneshot::Sender<ActionResult<()>>, Option<u64>);

pub static RELEASE_ALL_HANDLE: OnceLock<ActionHandle> = OnceLock::new();

pub trait ActionComboScheduler: Send + Sync {
    /// Schedules combo steps through an external scheduler.
    ///
    /// # Errors
    ///
    /// Returns an [`ActionError`] when the scheduler is unavailable or rejects
    /// the combo.
    fn schedule_combo(
        &self,
        steps: Vec<ComboStep>,
        backend: Backend,
        operator_panic_epoch_at_schedule: u64,
    ) -> ActionResult<()>;
}

#[derive(Clone)]
pub struct ActionHandle {
    tx: mpsc::Sender<ActionMessage>,
    safety_tx: Option<mpsc::UnboundedSender<ActionMessage>>,
    combo_scheduler: Arc<Mutex<Option<Arc<dyn ActionComboScheduler>>>>,
    session_id: Option<String>,
    session_inputs: Arc<Mutex<SessionInputOwnership>>,
    session_input_gate: Arc<AsyncMutex<()>>,
}

impl std::fmt::Debug for ActionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActionHandle")
            .finish_non_exhaustive()
    }
}

impl ActionHandle {
    #[must_use]
    pub fn new(tx: mpsc::Sender<ActionMessage>) -> Self {
        Self {
            tx,
            safety_tx: None,
            combo_scheduler: Arc::new(Mutex::new(None)),
            session_id: None,
            session_inputs: Arc::new(Mutex::new(SessionInputOwnership::default())),
            session_input_gate: Arc::new(AsyncMutex::new(())),
        }
    }

    #[must_use]
    pub fn channel() -> (Self, mpsc::Receiver<ActionMessage>) {
        let (tx, rx) = mpsc::channel(ACTION_QUEUE_CAPACITY);
        (Self::new(tx), rx)
    }

    #[must_use]
    pub(crate) fn channel_with_safety_lane() -> (
        Self,
        mpsc::Receiver<ActionMessage>,
        mpsc::UnboundedReceiver<ActionMessage>,
    ) {
        let (tx, rx) = mpsc::channel(ACTION_QUEUE_CAPACITY);
        let (safety_tx, safety_rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                safety_tx: Some(safety_tx),
                combo_scheduler: Arc::new(Mutex::new(None)),
                session_id: None,
                session_inputs: Arc::new(Mutex::new(SessionInputOwnership::default())),
                session_input_gate: Arc::new(AsyncMutex::new(())),
            },
            rx,
            safety_rx,
        )
    }

    #[must_use]
    pub fn with_session_id(&self, session_id: Option<String>) -> Self {
        Self {
            tx: self.tx.clone(),
            safety_tx: self.safety_tx.clone(),
            combo_scheduler: Arc::clone(&self.combo_scheduler),
            session_id,
            session_inputs: Arc::clone(&self.session_inputs),
            session_input_gate: Arc::clone(&self.session_input_gate),
        }
    }

    /// Installs the scheduler used to route [`Action::Combo`] through the
    /// reflex runtime instead of flattening it directly in the action emitter.
    ///
    /// # Errors
    ///
    /// Returns `ACTION_BACKEND_UNAVAILABLE` if the bridge slot is poisoned.
    pub fn install_combo_scheduler(
        &self,
        scheduler: Arc<dyn ActionComboScheduler>,
    ) -> ActionResult<()> {
        let mut combo_scheduler =
            self.combo_scheduler
                .lock()
                .map_err(|_err| ActionError::BackendUnavailable {
                    detail: "action combo scheduler bridge is poisoned".to_owned(),
                })?;
        *combo_scheduler = Some(scheduler);
        drop(combo_scheduler);
        Ok(())
    }

    /// Enqueues an action and waits for the emitter acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns `ACTION_QUEUE_FULL` when the bounded normal action queue is
    /// saturated, `ACTION_BACKEND_UNAVAILABLE` when the emitter channel or
    /// acknowledgement path is closed, or the emitter's own `ActionError`.
    pub async fn execute(&self, action: Action) -> ActionResult<()> {
        self.execute_with_owner(action, self.session_id.clone())
            .await
    }

    async fn execute_with_owner(
        &self,
        action: Action,
        session_id: Option<String>,
    ) -> ActionResult<()> {
        let _session_input_gate = self.session_input_gate.lock().await;
        self.execute_with_owner_gated(action, session_id).await
    }

    async fn execute_with_owner_gated(
        &self,
        action: Action,
        session_id: Option<String>,
    ) -> ActionResult<()> {
        validate_action(&action)?;
        ensure_operator_panic_allows_action(&action)?;
        if let Action::Combo { steps, backend } = &action
            && let Some(scheduler) = self.combo_scheduler()?
        {
            let epoch_at_schedule =
                capture_operator_panic_action_epoch(&action)?.ok_or_else(|| {
                    ActionError::BackendUnavailable {
                        detail: "combo scheduling unexpectedly lacked an operator-panic epoch"
                            .to_owned(),
                    }
                })?;
            scheduler.schedule_combo(steps.clone(), *backend, epoch_at_schedule)?;
            ensure_operator_panic_allows_action_since(&action, Some(epoch_at_schedule))?;
            return Ok(());
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        let epoch_at_enqueue = self.send_for_execution(action.clone(), ack_tx)?;
        let result = ack_rx
            .await
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "action emitter dropped acknowledgement".to_owned(),
            })?;
        if result.is_ok() {
            // Do not resurrect held-input ownership after K1 released the
            // physical state while an already-dispatched action was awaiting
            // its emitter acknowledgement.
            ensure_operator_panic_allows_action_since(&action, epoch_at_enqueue)?;
            self.record_successful_action_with_panic_guard(
                session_id.as_deref(),
                &action,
                epoch_at_enqueue,
            )?;
        }
        result
    }

    /// Attempts to enqueue an action without waiting for emitter completion.
    ///
    /// # Errors
    ///
    /// Returns `ACTION_QUEUE_FULL` when the bounded queue is saturated, or
    /// `ACTION_BACKEND_UNAVAILABLE` when the emitter channel is closed.
    pub fn try_execute(&self, action: Action) -> ActionResult<()> {
        validate_action(&action)?;
        ensure_operator_panic_allows_action(&action)?;
        if let Action::Combo { steps, backend } = &action
            && let Some(scheduler) = self.combo_scheduler()?
        {
            let epoch_at_schedule =
                capture_operator_panic_action_epoch(&action)?.ok_or_else(|| {
                    ActionError::BackendUnavailable {
                        detail: "combo scheduling unexpectedly lacked an operator-panic epoch"
                            .to_owned(),
                    }
                })?;
            scheduler.schedule_combo(steps.clone(), *backend, epoch_at_schedule)?;
            return ensure_operator_panic_allows_action_since(&action, Some(epoch_at_schedule));
        }
        let (ack_tx, _ack_rx) = oneshot::channel();
        let _epoch_at_enqueue = self.send_for_execution(action, ack_tx)?;
        Ok(())
    }

    /// Enqueues `ReleaseAll` and synchronously waits for its acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns `ACTION_QUEUE_FULL` if the fallback bounded queue is saturated,
    /// or `ACTION_BACKEND_UNAVAILABLE` if the acknowledgement closes or times
    /// out.
    pub fn fire_release_all_blocking_with_timeout(&self, timeout: Duration) -> ActionResult<()> {
        let (ack_tx, mut ack_rx) = oneshot::channel();
        self.send_release_all(Action::ReleaseAll, ack_tx)?;

        let deadline = Instant::now() + timeout;
        let result = loop {
            match ack_rx.try_recv() {
                Ok(result) => break result,
                Err(oneshot::error::TryRecvError::Closed) => {
                    return Err(ActionError::BackendUnavailable {
                        detail: "release_all acknowledgement channel closed".to_owned(),
                    });
                }
                Err(oneshot::error::TryRecvError::Empty) if Instant::now() >= deadline => {
                    return Err(ActionError::BackendUnavailable {
                        detail: format!("release_all acknowledgement timed out after {timeout:?}"),
                    });
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        };
        if result.is_ok() {
            self.record_successful_action(None, &Action::ReleaseAll)?;
        }
        result
    }

    /// Reads the per-HTTP-session held-input ownership ledger.
    ///
    /// This is runtime state used to release one HTTP MCP session without
    /// draining unrelated clients. The action emitter snapshot remains the
    /// physical held-state readback.
    ///
    /// # Errors
    ///
    /// Returns `ACTION_BACKEND_UNAVAILABLE` if the ownership ledger lock is
    /// poisoned.
    pub fn session_inputs_snapshot(&self) -> ActionResult<SessionInputSnapshot> {
        self.session_inputs
            .lock()
            .map(|inputs| inputs.snapshot())
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "session input ownership ledger is poisoned".to_owned(),
            })
    }

    /// Releases only inputs owned by `session_id`.
    ///
    /// Shared inputs are retained until their final owning session is released.
    /// This never sends [`Action::ReleaseAll`]; it emits targeted key-up,
    /// mouse-up, and neutral gamepad reports for inputs no other session owns.
    ///
    /// # Errors
    ///
    /// Returns an action error if the ownership ledger is poisoned or if any
    /// targeted release action cannot be emitted.
    pub async fn release_session_inputs(
        &self,
        session_id: &str,
    ) -> ActionResult<SessionReleaseSummary> {
        let _session_input_gate = self.session_input_gate.lock().await;
        let plan = self
            .session_inputs
            .lock()
            .map(|inputs| inputs.release_plan(session_id))
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "session input ownership ledger is poisoned".to_owned(),
            })?;
        self.session_inputs
            .lock()
            .map(|mut inputs| inputs.remove_retained_shared_owners(session_id))
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "session input ownership ledger is poisoned".to_owned(),
            })?;
        let mut first_error = None;
        for action in plan.actions {
            match self.execute_with_owner_gated(action.clone(), None).await {
                Ok(()) => {
                    if let Err(error) = self.confirm_session_release_action(session_id, &action)
                        && first_error.is_none()
                    {
                        first_error = Some(error);
                    }
                }
                Err(error) if first_error.is_none() => {
                    first_error = Some(error);
                }
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(plan.summary)
    }

    /// Releases a session's targeted held inputs, verifies the ownership ledger
    /// no longer contains that session, then releases/completes its input lease.
    ///
    /// This keeps the critical invariant for multi-agent foreground input:
    /// there must not be a moment where a session's lease is free while that
    /// same session still owns held keyboard, mouse, or pad state.
    ///
    /// # Errors
    ///
    /// Returns an action error if targeted release fails, if the ownership
    /// ledger cannot be read, or if the session still owns input afterward.
    pub async fn release_session_inputs_and_lease(
        &self,
        session_id: &str,
    ) -> ActionResult<SessionInputLeaseReleaseSummary> {
        let input_summary = self.release_session_inputs(session_id).await?;
        let after = self.session_inputs_snapshot()?;
        if after
            .sessions
            .iter()
            .any(|session| session.session_id == session_id)
        {
            return Err(ActionError::BackendUnavailable {
                detail: format!(
                    "session input cleanup for {session_id:?} left ownership in ledger; refusing to release input lease"
                ),
            });
        }
        let lease_released = crate::lease::release_if_owner(session_id);
        let expired_lease_cleanup_completed = crate::lease::complete_expired_cleanup(session_id);
        Ok(SessionInputLeaseReleaseSummary {
            session_id: session_id.to_owned(),
            input_summary,
            lease_released,
            expired_lease_cleanup_completed,
        })
    }

    fn combo_scheduler(&self) -> ActionResult<Option<Arc<dyn ActionComboScheduler>>> {
        self.combo_scheduler
            .lock()
            .map(|scheduler| scheduler.clone())
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "action combo scheduler bridge is poisoned".to_owned(),
            })
    }

    fn send_for_execution(
        &self,
        action: Action,
        ack_tx: oneshot::Sender<ActionResult<()>>,
    ) -> ActionResult<Option<u64>> {
        let operator_panic_epoch_at_enqueue = capture_operator_panic_action_epoch(&action)?;
        if matches!(action, Action::ReleaseAll) {
            crate::request_release_interrupt();
        }
        if is_safety_action(&action)
            && let Some(safety_tx) = &self.safety_tx
        {
            safety_tx
                .send((action, ack_tx, operator_panic_epoch_at_enqueue))
                .map_err(map_unbounded_send)?;
            return Ok(operator_panic_epoch_at_enqueue);
        }
        self.tx
            .try_send((action, ack_tx, operator_panic_epoch_at_enqueue))
            .map_err(map_try_send)?;
        Ok(operator_panic_epoch_at_enqueue)
    }

    fn send_release_all(
        &self,
        action: Action,
        ack_tx: oneshot::Sender<ActionResult<()>>,
    ) -> ActionResult<()> {
        crate::request_release_interrupt();
        if let Some(safety_tx) = &self.safety_tx {
            return safety_tx
                .send((action, ack_tx, None))
                .map_err(map_unbounded_send);
        }
        self.tx
            .try_send((action, ack_tx, None))
            .map_err(map_try_send)
    }

    fn record_successful_action(
        &self,
        session_id: Option<&str>,
        action: &Action,
    ) -> ActionResult<()> {
        self.session_inputs
            .lock()
            .map(|mut inputs| inputs.apply_success(session_id, action))
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "session input ownership ledger is poisoned".to_owned(),
            })
    }

    fn record_successful_action_with_panic_guard(
        &self,
        session_id: Option<&str>,
        action: &Action,
        epoch_at_enqueue: Option<u64>,
    ) -> ActionResult<()> {
        self.record_successful_action(session_id, action)?;
        if let Err(operator_panic_error) =
            ensure_operator_panic_allows_action_since(action, epoch_at_enqueue)
        {
            // K1 may have raced the ledger write after the caller's first
            // check: the physical ReleaseAll can clear ownership and then this
            // request can otherwise restore a stale held-input owner. The
            // caller still owns `session_input_gate`, so conservatively restore
            // the ledger to the same all-released state before returning.
            if let Err(cleanup_error) = self.record_successful_action(None, &Action::ReleaseAll) {
                crate::record_operator_panic_safety_incident();
                return Err(cleanup_error);
            }
            return Err(operator_panic_error);
        }
        Ok(())
    }

    fn confirm_session_release_action(
        &self,
        session_id: &str,
        action: &Action,
    ) -> ActionResult<()> {
        self.session_inputs
            .lock()
            .map(|mut inputs| inputs.confirm_session_release_action(session_id, action))
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "session input ownership ledger is poisoned".to_owned(),
            })
    }
}

fn map_try_send(error: mpsc::error::TrySendError<ActionMessage>) -> ActionError {
    match error {
        mpsc::error::TrySendError::Full(_message) => ActionError::QueueFull {
            detail: format!("action queue capacity {ACTION_QUEUE_CAPACITY} is full"),
        },
        mpsc::error::TrySendError::Closed(_message) => ActionError::BackendUnavailable {
            detail: "action emitter channel is closed".to_owned(),
        },
    }
}

fn map_unbounded_send(error: mpsc::error::SendError<ActionMessage>) -> ActionError {
    let _message = error.0;
    ActionError::BackendUnavailable {
        detail: "action emitter safety channel is closed".to_owned(),
    }
}

pub(crate) fn is_safety_action(action: &Action) -> bool {
    match action {
        Action::ReleaseAll
        | Action::KeyUp { .. }
        | Action::MouseButton {
            action: ButtonAction::Up,
            ..
        } => true,
        Action::PadReport { report, .. } => is_neutral_report(report),
        _ => false,
    }
}

pub(crate) fn ensure_operator_panic_allows_action(action: &Action) -> ActionResult<()> {
    capture_operator_panic_action_epoch(action).map(|_epoch| ())
}

fn capture_operator_panic_action_epoch(action: &Action) -> ActionResult<Option<u64>> {
    if is_safety_action(action) {
        return Ok(None);
    }
    let epoch_at_arm = crate::operator_panic_epoch();
    ensure_operator_panic_allows_action_since(action, Some(epoch_at_arm))?;
    Ok(Some(epoch_at_arm))
}

pub(crate) fn ensure_operator_panic_allows_action_since(
    action: &Action,
    epoch_at_enqueue: Option<u64>,
) -> ActionResult<()> {
    if is_safety_action(action) {
        return Ok(());
    }
    let readback = crate::operator_panic_safety_readback();
    if let Some(epoch_at_enqueue) = epoch_at_enqueue
        && !readback.pending
        && readback.epoch == epoch_at_enqueue
    {
        return Ok(());
    }
    Err(ActionError::SafetyOperatorHotkeyFired {
        detail: format!(
            "operator-panic safety admission is closed for a normal action: epoch_at_enqueue={epoch_at_enqueue:?} epoch_after={} publications_in_flight={} outstanding_generations={} outstanding_finalizations={} accounting_incident={}",
            readback.epoch,
            readback.publications_in_flight,
            readback.outstanding_generations,
            readback.outstanding_finalizations,
            readback.accounting_incident
        ),
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionInputSnapshot {
    pub sessions: Vec<SessionInputSessionSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInputSessionSnapshot {
    pub session_id: String,
    pub keys: Vec<SessionKeyInput>,
    pub mouse_buttons: Vec<SessionMouseButtonInput>,
    pub pads: Vec<SessionPadInput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionKeyInput {
    pub key: Key,
    pub backend: Backend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMouseButtonInput {
    pub button: MouseButton,
    pub backend: Backend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPadInput {
    pub pad: PadId,
    pub controller: GamepadController,
    pub backend: Backend,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionReleaseSummary {
    pub session_id: String,
    pub released_keys: u32,
    pub released_buttons: u32,
    pub neutralized_pads: u32,
    pub retained_shared_inputs: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionInputLeaseReleaseSummary {
    pub session_id: String,
    pub input_summary: SessionReleaseSummary,
    pub lease_released: bool,
    pub expired_lease_cleanup_completed: bool,
}

#[derive(Clone, Debug, Default)]
struct SessionReleasePlan {
    actions: Vec<Action>,
    summary: SessionReleaseSummary,
}

#[derive(Debug, Default)]
struct SessionInputOwnership {
    keys: Vec<OwnedKeyInput>,
    buttons: Vec<OwnedMouseButtonInput>,
    pads: Vec<OwnedPadInput>,
}

#[derive(Clone, Debug)]
struct OwnedKeyInput {
    key: Key,
    backend: Backend,
    owners: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct OwnedMouseButtonInput {
    button: MouseButton,
    backend: Backend,
    owners: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct OwnedPadInput {
    pad: PadId,
    controller: GamepadController,
    backend: Backend,
    owners: BTreeSet<String>,
}

impl SessionInputOwnership {
    fn apply_success(&mut self, session_id: Option<&str>, action: &Action) {
        if matches!(action, Action::ReleaseAll) {
            self.clear();
            return;
        }
        let Some(session_id) = session_id else {
            return;
        };
        match action {
            Action::KeyDown { key, backend } => self.hold_key(session_id, key.clone(), *backend),
            Action::KeyUp { key, backend } | Action::KeyPress { key, backend, .. } => {
                self.release_key(session_id, key, *backend);
            }
            Action::KeyChord { keys, backend, .. } => {
                for key in keys {
                    self.release_key(session_id, key, *backend);
                }
            }
            Action::MouseButton {
                button,
                action,
                backend,
                ..
            } => match action {
                ButtonAction::Down => self.hold_button(session_id, *button, *backend),
                ButtonAction::Up | ButtonAction::Press => {
                    self.release_button(session_id, *button, *backend);
                }
            },
            Action::PadReport { pad, report } => {
                if is_neutral_report(report) {
                    self.release_pad(session_id, *pad, Backend::Vigem);
                    self.release_pad(session_id, *pad, Backend::Hardware);
                } else {
                    self.hold_pad(session_id, *pad, report.controller, Backend::Vigem);
                }
            }
            Action::Combo { steps, backend } => {
                for step in steps {
                    self.apply_combo_input(session_id, &step.input, *backend);
                }
            }
            _ => {}
        }
    }

    fn snapshot(&self) -> SessionInputSnapshot {
        let mut sessions = BTreeMap::<String, SessionInputSessionSnapshot>::new();
        for input in &self.keys {
            for owner in &input.owners {
                sessions
                    .entry(owner.clone())
                    .or_insert_with(|| session_snapshot(owner))
                    .keys
                    .push(SessionKeyInput {
                        key: input.key.clone(),
                        backend: input.backend,
                    });
            }
        }
        for input in &self.buttons {
            for owner in &input.owners {
                sessions
                    .entry(owner.clone())
                    .or_insert_with(|| session_snapshot(owner))
                    .mouse_buttons
                    .push(SessionMouseButtonInput {
                        button: input.button,
                        backend: input.backend,
                    });
            }
        }
        for input in &self.pads {
            for owner in &input.owners {
                sessions
                    .entry(owner.clone())
                    .or_insert_with(|| session_snapshot(owner))
                    .pads
                    .push(SessionPadInput {
                        pad: input.pad,
                        controller: input.controller,
                        backend: input.backend,
                    });
            }
        }
        SessionInputSnapshot {
            sessions: sessions.into_values().collect(),
        }
    }

    fn release_plan(&self, session_id: &str) -> SessionReleasePlan {
        let mut plan = SessionReleasePlan {
            actions: Vec::new(),
            summary: SessionReleaseSummary {
                session_id: session_id.to_owned(),
                ..SessionReleaseSummary::default()
            },
        };

        for input in &self.keys {
            if !input.owners.contains(session_id) {
                continue;
            }
            if input.owners.len() == 1 {
                plan.summary.released_keys = plan.summary.released_keys.saturating_add(1);
                plan.actions.push(Action::KeyUp {
                    key: input.key.clone(),
                    backend: input.backend,
                });
            } else {
                plan.summary.retained_shared_inputs =
                    plan.summary.retained_shared_inputs.saturating_add(1);
            }
        }

        for input in &self.buttons {
            if !input.owners.contains(session_id) {
                continue;
            }
            if input.owners.len() == 1 {
                plan.summary.released_buttons = plan.summary.released_buttons.saturating_add(1);
                plan.actions.push(Action::MouseButton {
                    button: input.button,
                    action: ButtonAction::Up,
                    hold_ms: 0,
                    backend: input.backend,
                });
            } else {
                plan.summary.retained_shared_inputs =
                    plan.summary.retained_shared_inputs.saturating_add(1);
            }
        }

        for input in &self.pads {
            if !input.owners.contains(session_id) {
                continue;
            }
            if input.owners.len() == 1 {
                plan.summary.neutralized_pads = plan.summary.neutralized_pads.saturating_add(1);
                plan.actions.push(Action::PadReport {
                    pad: input.pad,
                    report: GamepadReport::neutral(input.controller),
                });
            } else {
                plan.summary.retained_shared_inputs =
                    plan.summary.retained_shared_inputs.saturating_add(1);
            }
        }

        plan
    }

    fn remove_retained_shared_owners(&mut self, session_id: &str) {
        remove_retained_shared_owner(&mut self.keys, session_id);
        remove_retained_shared_owner(&mut self.buttons, session_id);
        remove_retained_shared_owner(&mut self.pads, session_id);
    }

    fn confirm_session_release_action(&mut self, session_id: &str, action: &Action) {
        match action {
            Action::KeyUp { key, backend } => self.release_key(session_id, key, *backend),
            Action::MouseButton {
                button,
                action: ButtonAction::Up,
                backend,
                ..
            } => self.release_button(session_id, *button, *backend),
            Action::PadReport { pad, report } if is_neutral_report(report) => {
                self.release_pad(session_id, *pad, Backend::Vigem);
                self.release_pad(session_id, *pad, Backend::Hardware);
            }
            _ => {}
        }
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.buttons.clear();
        self.pads.clear();
    }

    fn apply_combo_input(&mut self, session_id: &str, input: &ComboInput, backend: Backend) {
        match input {
            ComboInput::KeyDown { key } => self.hold_key(session_id, key.clone(), backend),
            ComboInput::KeyUp { key } | ComboInput::KeyPress { key, .. } => {
                self.release_key(session_id, key, backend);
            }
            ComboInput::MouseButton { button, action } => match action {
                ButtonAction::Down => self.hold_button(session_id, *button, backend),
                ButtonAction::Up | ButtonAction::Press => {
                    self.release_button(session_id, *button, backend);
                }
            },
            _ => {}
        }
    }

    fn hold_key(&mut self, session_id: &str, key: Key, backend: Backend) {
        if let Some(input) = self
            .keys
            .iter_mut()
            .find(|input| input.key == key && input.backend == backend)
        {
            input.owners.insert(session_id.to_owned());
            return;
        }
        self.keys.push(OwnedKeyInput {
            key,
            backend,
            owners: owner_set(session_id),
        });
    }

    fn release_key(&mut self, session_id: &str, key: &Key, backend: Backend) {
        release_owned_input(&mut self.keys, session_id, |input| {
            input.key == *key && input.backend == backend
        });
    }

    fn hold_button(&mut self, session_id: &str, button: MouseButton, backend: Backend) {
        if let Some(input) = self
            .buttons
            .iter_mut()
            .find(|input| input.button == button && input.backend == backend)
        {
            input.owners.insert(session_id.to_owned());
            return;
        }
        self.buttons.push(OwnedMouseButtonInput {
            button,
            backend,
            owners: owner_set(session_id),
        });
    }

    fn release_button(&mut self, session_id: &str, button: MouseButton, backend: Backend) {
        release_owned_input(&mut self.buttons, session_id, |input| {
            input.button == button && input.backend == backend
        });
    }

    fn hold_pad(
        &mut self,
        session_id: &str,
        pad: PadId,
        controller: GamepadController,
        backend: Backend,
    ) {
        if let Some(input) = self
            .pads
            .iter_mut()
            .find(|input| input.pad == pad && input.backend == backend)
        {
            input.controller = controller;
            input.owners.insert(session_id.to_owned());
            return;
        }
        self.pads.push(OwnedPadInput {
            pad,
            controller,
            backend,
            owners: owner_set(session_id),
        });
    }

    fn release_pad(&mut self, session_id: &str, pad: PadId, backend: Backend) {
        release_owned_input(&mut self.pads, session_id, |input| {
            input.pad == pad && input.backend == backend
        });
    }
}

fn release_owned_input<T>(
    inputs: &mut Vec<T>,
    session_id: &str,
    mut matches_input: impl FnMut(&T) -> bool,
) where
    T: OwnedInputOwners,
{
    inputs.retain_mut(|input| {
        if matches_input(input) {
            input.owners_mut().remove(session_id);
        }
        !input.owners().is_empty()
    });
}

fn remove_retained_shared_owner<T>(inputs: &mut Vec<T>, session_id: &str)
where
    T: OwnedInputOwners,
{
    inputs.retain_mut(|input| {
        if input.owners().contains(session_id) && input.owners().len() > 1 {
            input.owners_mut().remove(session_id);
        }
        !input.owners().is_empty()
    });
}

trait OwnedInputOwners {
    fn owners(&self) -> &BTreeSet<String>;
    fn owners_mut(&mut self) -> &mut BTreeSet<String>;
}

impl OwnedInputOwners for OwnedKeyInput {
    fn owners(&self) -> &BTreeSet<String> {
        &self.owners
    }

    fn owners_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.owners
    }
}

impl OwnedInputOwners for OwnedMouseButtonInput {
    fn owners(&self) -> &BTreeSet<String> {
        &self.owners
    }

    fn owners_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.owners
    }
}

impl OwnedInputOwners for OwnedPadInput {
    fn owners(&self) -> &BTreeSet<String> {
        &self.owners
    }

    fn owners_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.owners
    }
}

fn session_snapshot(session_id: &str) -> SessionInputSessionSnapshot {
    SessionInputSessionSnapshot {
        session_id: session_id.to_owned(),
        keys: Vec::new(),
        mouse_buttons: Vec::new(),
        pads: Vec::new(),
    }
}

fn owner_set(session_id: &str) -> BTreeSet<String> {
    BTreeSet::from([session_id.to_owned()])
}

fn is_neutral_report(report: &GamepadReport) -> bool {
    report.buttons.is_empty()
        && report.thumb_l == (0.0, 0.0)
        && report.thumb_r == (0.0, 0.0)
        && report.lt == 0.0
        && report.rt == 0.0
}

#[cfg(test)]
mod operator_panic_queue_tests {
    use super::*;
    use synapse_core::KeyCode;

    #[test]
    fn enqueued_normal_action_keeps_original_epoch_across_completed_panic_wave() {
        crate::hotkey::isolate_interrupt_epochs_for_test();
        let action = Action::KeyDown {
            key: Key {
                code: KeyCode::Named {
                    value: "shift".to_owned(),
                },
                use_scancode: false,
            },
            backend: Backend::Software,
        };
        let epoch_at_enqueue = capture_operator_panic_action_epoch(&action)
            .expect("initial action admission")
            .expect("normal action must carry an enqueue epoch");

        let mut token = crate::request_operator_panic_interrupt();
        assert!(crate::acknowledge_operator_panic_preemption(&mut token));
        let crate::OperatorPanicSafetyCompletion::Finalize(finalization) =
            crate::complete_operator_panic_safety_generation(token)
                .unwrap_or_else(|detail| panic!("complete test panic: {detail}"))
        else {
            panic!("isolated generation must own finalization");
        };
        assert!(crate::finish_operator_panic_safety_finalization(
            finalization,
            true
        ));
        assert!(!crate::operator_panic_safety_pending());

        let error = ensure_operator_panic_allows_action_since(&action, Some(epoch_at_enqueue))
            .expect_err("queued pre-panic action must remain superseded after K2 finalizes");
        assert!(matches!(
            error,
            ActionError::SafetyOperatorHotkeyFired { .. }
        ));
    }

    #[test]
    fn normal_action_without_enqueue_epoch_fails_closed() {
        crate::hotkey::isolate_interrupt_epochs_for_test();
        let action = Action::KeyDown {
            key: Key {
                code: KeyCode::Named {
                    value: "shift".to_owned(),
                },
                use_scancode: false,
            },
            backend: Backend::Software,
        };

        let error = ensure_operator_panic_allows_action_since(&action, None)
            .expect_err("unattributed normal queue item must fail closed");
        assert!(matches!(
            error,
            ActionError::SafetyOperatorHotkeyFired { .. }
        ));
    }

    #[test]
    fn panic_crossing_after_ledger_write_clears_stale_ownership() {
        crate::hotkey::isolate_interrupt_epochs_for_test();
        let (handle, _rx) = ActionHandle::channel();
        let action = Action::KeyDown {
            key: Key {
                code: KeyCode::Named {
                    value: "shift".to_owned(),
                },
                use_scancode: false,
            },
            backend: Backend::Software,
        };
        let epoch_at_enqueue = capture_operator_panic_action_epoch(&action)
            .expect("initial action admission")
            .expect("normal action must carry an enqueue epoch");
        let mut token = crate::request_operator_panic_interrupt();
        assert!(crate::acknowledge_operator_panic_preemption(&mut token));

        let error = handle
            .record_successful_action_with_panic_guard(
                Some("stale-session"),
                &action,
                Some(epoch_at_enqueue),
            )
            .expect_err("post-ledger panic crossing must fail closed");
        assert!(matches!(
            error,
            ActionError::SafetyOperatorHotkeyFired { .. }
        ));
        assert!(
            handle
                .session_inputs_snapshot()
                .expect("ownership readback")
                .sessions
                .is_empty(),
            "panic rollback must leave the held-input ownership ledger empty"
        );

        let crate::OperatorPanicSafetyCompletion::Finalize(finalization) =
            crate::complete_operator_panic_safety_generation(token)
                .unwrap_or_else(|detail| panic!("complete test panic: {detail}"))
        else {
            panic!("isolated generation must own finalization");
        };
        assert!(crate::finish_operator_panic_safety_finalization(
            finalization,
            true
        ));
    }
}

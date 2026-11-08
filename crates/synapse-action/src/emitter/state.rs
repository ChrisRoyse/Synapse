use std::collections::{BTreeSet, HashMap};

use bit_set::BitSet;
use synapse_core::{ButtonAction, GamepadReport, Key, MouseButton, PadId};
use tokio::sync::{mpsc, oneshot};

use crate::{ActionResult, ResolvedBackend};

pub type ActionSnapshotMessage = oneshot::Sender<ActionStateSnapshot>;

#[derive(Clone, Debug, PartialEq)]
pub struct ActionStateSnapshot {
    pub held_keys: Vec<Key>,
    pub held_key_bits: Vec<usize>,
    pub held_key_timer_keys: Vec<Key>,
    pub held_key_timer_count: usize,
    pub held_buttons: Vec<MouseButton>,
    pub held_button_bits: Vec<usize>,
    pub pad_state: HashMap<PadId, GamepadReport>,
    pub held_keys_by_backend: HashMap<ResolvedBackend, Vec<Key>>,
    pub held_buttons_by_backend: HashMap<ResolvedBackend, Vec<MouseButton>>,
}

#[derive(Debug)]
pub struct EmitState {
    pub(crate) held_keys: BitSet,
    pub(crate) held_buttons: BitSet,
    pub(crate) key_indices: HashMap<Key, usize>,
    pub(crate) keys_by_index: Vec<Key>,
    held_key_backends: HashMap<usize, BTreeSet<ResolvedBackend>>,
    held_button_backends: HashMap<usize, BTreeSet<ResolvedBackend>>,
    pub(crate) pad_state: HashMap<PadId, GamepadReport>,
    active_backend: Option<ResolvedBackend>,
    delivery_target: Option<crate::InputDeliveryTarget>,
}

impl EmitState {
    #[must_use]
    #[tracing::instrument(skip_all, fields(action_kind = "emit_state_new"))]
    pub fn new() -> Self {
        Self {
            held_keys: BitSet::new(),
            held_buttons: BitSet::new(),
            key_indices: HashMap::new(),
            keys_by_index: Vec::new(),
            held_key_backends: HashMap::new(),
            held_button_backends: HashMap::new(),
            pad_state: HashMap::new(),
            active_backend: None,
            delivery_target: None,
        }
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(action_kind = "emit_state_snapshot"))]
    pub fn snapshot(&self) -> ActionStateSnapshot {
        ActionStateSnapshot {
            held_keys: self.held_keys(),
            held_key_bits: self.held_key_bits(),
            held_key_timer_keys: Vec::new(),
            held_key_timer_count: 0,
            held_buttons: self.held_buttons(),
            held_button_bits: self.held_button_bits(),
            pad_state: self.pad_state.clone(),
            held_keys_by_backend: self.held_keys_by_backend(),
            held_buttons_by_backend: self.held_buttons_by_backend(),
        }
    }

    fn held_keys(&self) -> Vec<Key> {
        self.active_backend.map_or_else(
            || {
                self.held_keys
                    .iter()
                    .filter_map(|index| self.keys_by_index.get(index).cloned())
                    .collect()
            },
            |backend| self.held_keys_for_backend(backend),
        )
    }

    fn held_key_bits(&self) -> Vec<usize> {
        self.active_backend.map_or_else(
            || self.held_keys.iter().collect(),
            |backend| {
                self.held_keys
                    .iter()
                    .filter(|index| self.key_is_held_by_backend(*index, backend))
                    .collect()
            },
        )
    }

    fn held_buttons(&self) -> Vec<MouseButton> {
        self.active_backend.map_or_else(
            || {
                self.held_buttons
                    .iter()
                    .filter_map(mouse_button_from_index)
                    .collect()
            },
            |backend| self.held_buttons_for_backend(backend),
        )
    }

    fn held_button_bits(&self) -> Vec<usize> {
        self.active_backend.map_or_else(
            || self.held_buttons.iter().collect(),
            |backend| {
                self.held_buttons
                    .iter()
                    .filter(|index| self.button_is_held_by_backend(*index, backend))
                    .collect()
            },
        )
    }

    pub(crate) fn release_all(&mut self) -> (usize, usize, usize) {
        if let Some(backend) = self.active_backend {
            self.release_all_for_backend(backend)
        } else {
            let released_keys = self.held_keys.count();
            let released_buttons = self.held_buttons.count();
            let released_pads = self.pad_state.len();
            self.held_keys.make_empty();
            self.held_buttons.make_empty();
            self.held_key_backends.clear();
            self.held_button_backends.clear();
            self.pad_state.clear();
            (released_keys, released_buttons, released_pads)
        }
    }

    pub(crate) fn hold_key(&mut self, key: &Key) {
        let index = self.key_index(key);
        self.held_keys.insert(index);
        if let Some(backend) = self.active_backend {
            self.held_key_backends
                .entry(index)
                .or_default()
                .insert(backend);
        }
    }

    pub(crate) fn release_key(&mut self, key: &Key) {
        if let Some(index) = self.key_indices.get(key) {
            self.release_key_index(*index);
        }
    }

    pub(crate) fn is_key_held_for_backend(&self, key: &Key, backend: ResolvedBackend) -> bool {
        self.key_indices
            .get(key)
            .is_some_and(|index| self.key_is_held_by_backend(*index, backend))
    }

    pub(crate) fn apply_mouse_button(&mut self, button: MouseButton, action: ButtonAction) {
        let index = mouse_button_index(button);
        match action {
            ButtonAction::Down => {
                self.held_buttons.insert(index);
                if let Some(backend) = self.active_backend {
                    self.held_button_backends
                        .entry(index)
                        .or_default()
                        .insert(backend);
                }
            }
            ButtonAction::Up | ButtonAction::Press => {
                self.release_button_index(index);
            }
        }
    }

    pub(crate) const fn set_active_backend(&mut self, backend: ResolvedBackend) {
        self.active_backend = Some(backend);
    }

    pub(crate) const fn clear_active_backend(&mut self) {
        self.active_backend = None;
    }

    /// Sets the intended OS-input window for the action being dispatched; the
    /// software backend verifies the live foreground against it before every
    /// input batch.
    pub const fn set_delivery_target(&mut self, target: Option<crate::InputDeliveryTarget>) {
        self.delivery_target = target;
    }

    #[must_use]
    pub const fn delivery_target(&self) -> Option<&crate::InputDeliveryTarget> {
        self.delivery_target.as_ref()
    }

    fn key_index(&mut self, key: &Key) -> usize {
        if let Some(index) = self.key_indices.get(key) {
            return *index;
        }
        let index = self.keys_by_index.len();
        self.keys_by_index.push(key.clone());
        self.key_indices.insert(key.clone(), index);
        index
    }

    fn held_keys_for_backend(&self, backend: ResolvedBackend) -> Vec<Key> {
        self.held_keys
            .iter()
            .filter(|index| self.key_is_held_by_backend(*index, backend))
            .filter_map(|index| self.keys_by_index.get(index).cloned())
            .collect()
    }

    fn held_buttons_for_backend(&self, backend: ResolvedBackend) -> Vec<MouseButton> {
        self.held_buttons
            .iter()
            .filter(|index| self.button_is_held_by_backend(*index, backend))
            .filter_map(mouse_button_from_index)
            .collect()
    }

    fn held_keys_by_backend(&self) -> HashMap<ResolvedBackend, Vec<Key>> {
        let mut by_backend = HashMap::new();
        for backend in [
            ResolvedBackend::Software,
            ResolvedBackend::Vigem,
            ResolvedBackend::Hardware,
        ] {
            let keys = self.held_keys_for_backend(backend);
            if !keys.is_empty() {
                by_backend.insert(backend, keys);
            }
        }
        by_backend
    }

    fn held_buttons_by_backend(&self) -> HashMap<ResolvedBackend, Vec<MouseButton>> {
        let mut by_backend = HashMap::new();
        for backend in [
            ResolvedBackend::Software,
            ResolvedBackend::Vigem,
            ResolvedBackend::Hardware,
        ] {
            let buttons = self.held_buttons_for_backend(backend);
            if !buttons.is_empty() {
                by_backend.insert(backend, buttons);
            }
        }
        by_backend
    }

    fn release_all_for_backend(&mut self, backend: ResolvedBackend) -> (usize, usize, usize) {
        let key_indices: Vec<_> = self
            .held_keys
            .iter()
            .filter(|index| self.key_is_held_by_backend(*index, backend))
            .collect();
        let button_indices: Vec<_> = self
            .held_buttons
            .iter()
            .filter(|index| self.button_is_held_by_backend(*index, backend))
            .collect();
        let released_keys = key_indices.len();
        let released_buttons = button_indices.len();

        for index in key_indices {
            self.release_key_index_for_backend(index, backend);
        }
        for index in button_indices {
            self.release_button_index_for_backend(index, backend);
        }

        let released_pads = if matches!(backend, ResolvedBackend::Hardware | ResolvedBackend::Vigem)
        {
            let released = self.pad_state.len();
            self.pad_state.clear();
            released
        } else {
            0
        };

        (released_keys, released_buttons, released_pads)
    }

    fn release_key_index(&mut self, index: usize) {
        if let Some(backend) = self.active_backend {
            self.release_key_index_for_backend(index, backend);
        } else {
            self.held_key_backends.remove(&index);
            self.held_keys.remove(index);
        }
    }

    fn release_key_index_for_backend(&mut self, index: usize, backend: ResolvedBackend) {
        if let Some(backends) = self.held_key_backends.get_mut(&index) {
            backends.remove(&backend);
            if backends.is_empty() {
                self.held_key_backends.remove(&index);
                self.held_keys.remove(index);
            }
        } else {
            self.held_keys.remove(index);
        }
    }

    fn release_button_index(&mut self, index: usize) {
        if let Some(backend) = self.active_backend {
            self.release_button_index_for_backend(index, backend);
        } else {
            self.held_button_backends.remove(&index);
            self.held_buttons.remove(index);
        }
    }

    fn release_button_index_for_backend(&mut self, index: usize, backend: ResolvedBackend) {
        if let Some(backends) = self.held_button_backends.get_mut(&index) {
            backends.remove(&backend);
            if backends.is_empty() {
                self.held_button_backends.remove(&index);
                self.held_buttons.remove(index);
            }
        } else {
            self.held_buttons.remove(index);
        }
    }

    fn key_is_held_by_backend(&self, index: usize, backend: ResolvedBackend) -> bool {
        if !self.held_keys.contains(index) {
            return false;
        }
        self.held_key_backends
            .get(&index)
            .is_none_or(|backends| backends.contains(&backend))
    }

    fn button_is_held_by_backend(&self, index: usize, backend: ResolvedBackend) -> bool {
        if !self.held_buttons.contains(index) {
            return false;
        }
        self.held_button_backends
            .get(&index)
            .is_none_or(|backends| backends.contains(&backend))
    }
}

impl Default for EmitState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct ActionEmitterSnapshotHandle {
    tx: mpsc::Sender<ActionSnapshotMessage>,
}

impl ActionEmitterSnapshotHandle {
    #[must_use]
    #[tracing::instrument(skip_all, fields(action_kind = "snapshot_handle_new"))]
    pub fn new(tx: mpsc::Sender<ActionSnapshotMessage>) -> Self {
        Self { tx }
    }

    /// Reads the emitter's held-state snapshot through the actor task.
    ///
    /// # Errors
    ///
    /// Returns `ACTION_BACKEND_UNAVAILABLE` when the snapshot request or
    /// response channel is closed.
    #[tracing::instrument(skip_all, fields(action_kind = "snapshot"))]
    pub async fn snapshot(&self) -> ActionResult<ActionStateSnapshot> {
        let (snapshot_tx, snapshot_rx) = oneshot::channel();
        self.tx
            .send(snapshot_tx)
            .await
            .map_err(|_err| crate::ActionError::BackendUnavailable {
                detail: "action emitter snapshot channel is closed".to_owned(),
            })?;
        snapshot_rx
            .await
            .map_err(|_err| crate::ActionError::BackendUnavailable {
                detail: "action emitter dropped snapshot response".to_owned(),
            })
    }
}

/// Snapshot of the three production backends the actor dispatches through.
///
/// Resolved per-action via [`resolve_backend`]. The actor itself stays the
const fn mouse_button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}

const fn mouse_button_from_index(index: usize) -> Option<MouseButton> {
    match index {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Right),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::X1),
        4 => Some(MouseButton::X2),
        _ => None,
    }
}

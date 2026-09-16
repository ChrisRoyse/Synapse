use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use tokio::{sync::mpsc, task::JoinHandle};

use crate::{ActionMessage, BackendResolutionPolicy};

mod backends;
mod dispatch;
mod keyboard;
mod lifecycle;
mod rate_limits;
mod routing;
mod state;

#[cfg(test)]
mod tests;

pub use backends::Backends;
pub use rate_limits::{
    BackendRateLimitControl, BackendRateLimitOverrideReadback, BackendRateLimitSnapshot,
};
pub use state::{
    ActionEmitterSnapshotHandle, ActionSnapshotMessage, ActionStateSnapshot, EmitState,
};

use keyboard::{HeldKeyAutoRelease, HeldKeyTimerKey};

pub const HELD_KEY_MAX_DURATION_MS: u64 = 30_000;

pub struct ActionEmitter {
    rx: mpsc::Receiver<ActionMessage>,
    safety_rx: mpsc::UnboundedReceiver<ActionMessage>,
    snapshot_rx: mpsc::Receiver<ActionSnapshotMessage>,
    auto_release_tx: mpsc::Sender<HeldKeyAutoRelease>,
    auto_release_rx: mpsc::Receiver<HeldKeyAutoRelease>,
    state: EmitState,
    backends: Backends,
    backend_resolution: Arc<RwLock<BackendResolutionPolicy>>,
    pub(super) rate_limits: BackendRateLimitControl,
    held_key_timers: HashMap<HeldKeyTimerKey, JoinHandle<()>>,
    held_key_timer_ids: HashMap<HeldKeyTimerKey, u64>,
    next_held_key_timer_id: u64,
}

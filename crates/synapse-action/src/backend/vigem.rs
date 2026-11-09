use std::sync::Mutex;

use synapse_core::Action;

use crate::{ActionBackend, ActionError, EmitState};

mod client;
mod error;
#[cfg(windows)]
mod pad;
mod reports;
mod state;

use client::VigemBackendInner;

#[cfg(windows)]
#[allow(unused_imports)]
pub(crate) use state::{apply_pad_button, apply_pad_report, apply_pad_stick, apply_pad_trigger};

/// Driver-backed `ViGEm` gamepad backend.
///
/// On Windows this lazily connects to `ViGEmBus` and plugs an X360 or DS4
/// target the first time a pad id is referenced. Other platforms fail closed
/// instead of pretending a virtual controller exists.
#[derive(Debug, Default)]
pub struct VigemBackend {
    inner: Mutex<VigemBackendInner>,
}

impl VigemBackend {
    #[must_use]
    #[tracing::instrument(fields(backend = "vigem"))]
    pub fn new() -> Self {
        Self::default()
    }

    /// Probes whether the backing `ViGEm` driver can be reached.
    ///
    /// # Errors
    ///
    /// Returns `ACTION_VIGEM_NOT_INSTALLED` on Windows when the `ViGEmBus`
    /// device interface is absent. Returns `ACTION_BACKEND_UNAVAILABLE` on
    /// non-Windows targets.
    #[tracing::instrument(skip_all, fields(backend = "vigem"))]
    pub fn ensure_ready(&self) -> Result<(), ActionError> {
        let mut inner = self.lock_inner()?;
        inner.ensure_ready()
    }

    fn lock_inner(&self) -> Result<std::sync::MutexGuard<'_, VigemBackendInner>, ActionError> {
        self.inner
            .lock()
            .map_err(|_err| ActionError::VigemPluginFailed {
                detail: "backend=vigem reason=backend mutex poisoned".to_owned(),
            })
    }
}

impl ActionBackend for VigemBackend {
    #[tracing::instrument(skip_all, fields(backend = "vigem"))]
    fn execute(&self, action: &Action, state: &mut EmitState) -> Result<(), ActionError> {
        crate::validate_action(action)?;
        let mut inner = self.lock_inner()?;
        inner.execute(action, state)
    }
}

#[cfg(windows)]
impl Drop for VigemBackend {
    fn drop(&mut self) {
        if let Ok(inner) = self.inner.get_mut() {
            inner.neutral_all_for_drop();
        }
    }
}

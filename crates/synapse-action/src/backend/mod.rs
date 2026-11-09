use synapse_core::{Action, Backend, ProfileBackends};

use crate::{ActionError, EmitState};

#[cfg(windows)]
pub(crate) mod mouse_coordinates;
pub mod recording;
#[cfg(windows)]
pub mod software;
#[cfg(not(windows))]
#[path = "software_non_windows.rs"]
pub mod software;
#[cfg(windows)]
pub(crate) mod text_dispatch;
pub mod unavailable;
pub mod vigem;

pub trait ActionBackend: Send + Sync {
    /// Executes one action against a concrete backend while updating emitter state.
    ///
    /// # Errors
    ///
    /// Returns an `ActionError` when the concrete backend cannot execute the
    /// action or when action validation fails.
    fn execute(&self, action: &Action, state: &mut EmitState) -> Result<(), ActionError>;
}

#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ResolvedBackend {
    Software,
    Vigem,
    Hardware,
}

impl ResolvedBackend {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Vigem => "vigem",
            Self::Hardware => "hardware",
        }
    }

    #[must_use]
    pub const fn to_backend(self) -> Backend {
        match self {
            Self::Software => Backend::Software,
            Self::Vigem => Backend::Vigem,
            Self::Hardware => Backend::Hardware,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BackendResolutionPolicy {
    pub default_backend: Backend,
    pub keyboard_default: Backend,
    pub mouse_default: Backend,
    pub pad_default: Backend,
}

impl BackendResolutionPolicy {
    #[must_use]
    pub const fn auto() -> Self {
        Self {
            default_backend: Backend::Auto,
            keyboard_default: Backend::Auto,
            mouse_default: Backend::Auto,
            pad_default: Backend::Auto,
        }
    }

    #[must_use]
    pub const fn from_profile_backends(backends: ProfileBackends) -> Self {
        Self {
            default_backend: backends.default,
            keyboard_default: backends.keyboard_default,
            mouse_default: backends.mouse_default,
            pad_default: backends.pad_default,
        }
    }

    #[must_use]
    pub fn auto_backend_for(self, action: &Action) -> ResolvedBackend {
        match action_backend_class(action) {
            ActionBackendClass::Keyboard => self.keyboard_auto_backend(),
            ActionBackendClass::Mouse => self.mouse_auto_backend(),
            ActionBackendClass::Pad => self.pad_auto_backend(),
            ActionBackendClass::ReleaseAll => self.release_all_auto_backend(),
        }
    }

    #[must_use]
    pub fn keyboard_auto_backend(self) -> ResolvedBackend {
        self.resolve_class_default(self.keyboard_default, ResolvedBackend::Software)
    }

    #[must_use]
    pub fn mouse_auto_backend(self) -> ResolvedBackend {
        self.resolve_class_default(self.mouse_default, ResolvedBackend::Software)
    }

    #[must_use]
    pub fn pad_auto_backend(self) -> ResolvedBackend {
        self.resolve_class_default(self.pad_default, ResolvedBackend::Vigem)
    }

    #[must_use]
    pub fn release_all_auto_backend(self) -> ResolvedBackend {
        self.resolve_default_or(ResolvedBackend::Software)
    }

    fn resolve_class_default(
        self,
        class_default: Backend,
        fallback: ResolvedBackend,
    ) -> ResolvedBackend {
        backend_to_resolved(class_default).unwrap_or_else(|| self.resolve_default_or(fallback))
    }

    fn resolve_default_or(self, fallback: ResolvedBackend) -> ResolvedBackend {
        backend_to_resolved(self.default_backend).unwrap_or(fallback)
    }
}

impl Default for BackendResolutionPolicy {
    fn default() -> Self {
        Self::auto()
    }
}

/// Resolves the concrete backend for one per-call backend request.
///
/// # Errors
///
/// Resolves `Backend::Hardware` to the retired fail-closed hardware slot.
#[tracing::instrument(skip_all, fields(requested_backend = ?requested))]
pub fn resolve_backend(
    requested: Backend,
    action: &Action,
) -> Result<ResolvedBackend, ActionError> {
    resolve_backend_with_policy(requested, action, BackendResolutionPolicy::default())
}

/// Resolves the concrete backend using the active profile/session policy.
///
/// # Errors
///
/// Resolves `Backend::Hardware` to the retired fail-closed hardware slot.
#[tracing::instrument(skip_all, fields(requested_backend = ?requested))]
pub fn resolve_backend_with_policy(
    requested: Backend,
    action: &Action,
    policy: BackendResolutionPolicy,
) -> Result<ResolvedBackend, ActionError> {
    match requested {
        Backend::Software => Ok(ResolvedBackend::Software),
        Backend::Vigem => Ok(ResolvedBackend::Vigem),
        Backend::Hardware => Ok(ResolvedBackend::Hardware),
        Backend::Auto => Ok(policy.auto_backend_for(action)),
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ActionBackendClass {
    Keyboard,
    Mouse,
    Pad,
    ReleaseAll,
}

const fn action_backend_class(action: &Action) -> ActionBackendClass {
    match action {
        Action::PadButton { .. }
        | Action::PadStick { .. }
        | Action::PadTrigger { .. }
        | Action::PadReport { .. } => ActionBackendClass::Pad,
        Action::KeyPress { .. }
        | Action::KeyDown { .. }
        | Action::KeyUp { .. }
        | Action::KeyChord { .. }
        | Action::TypeText { .. }
        | Action::Combo { .. } => ActionBackendClass::Keyboard,
        Action::MouseMove { .. }
        | Action::MouseMoveRelative { .. }
        | Action::MouseButton { .. }
        | Action::MouseDrag { .. }
        | Action::MouseStroke { .. }
        | Action::MouseScroll { .. }
        | Action::AimAt { .. } => ActionBackendClass::Mouse,
        Action::ReleaseAll => ActionBackendClass::ReleaseAll,
    }
}

const fn backend_to_resolved(backend: Backend) -> Option<ResolvedBackend> {
    match backend {
        Backend::Software => Some(ResolvedBackend::Software),
        Backend::Vigem => Some(ResolvedBackend::Vigem),
        Backend::Hardware => Some(ResolvedBackend::Hardware),
        Backend::Auto => None,
    }
}

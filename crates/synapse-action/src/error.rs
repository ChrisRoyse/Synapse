use synapse_core::{ElementId, error_codes};

pub type ActionResult<T> = Result<T, ActionError>;

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ActionError {
    #[error("action queue full: {detail}")]
    QueueFull { detail: String },
    #[error("action rate limited: {detail}")]
    RateLimited { detail: String, retry_after_ms: u64 },
    #[error("foreground input lease busy: {detail}")]
    ForegroundLeaseBusy {
        detail: String,
        holder_session_id: Option<String>,
        requesting_session_id: String,
        retry_after_ms: u64,
    },
    #[error("action backend unavailable: {detail}")]
    BackendUnavailable { detail: String },
    #[error("foreground activation refused: {detail}")]
    ForegroundActivationRefused { detail: String },
    #[error("action target invalid: {detail}")]
    TargetInvalid { detail: String },
    #[error("action hold exceeds max: {detail}")]
    HoldExceededMax { detail: String },
    #[error("ViGEm is not installed: {detail}")]
    VigemNotInstalled { detail: String },
    #[error("ViGEm plug-in failed: {detail}")]
    VigemPluginFailed { detail: String },
    #[error("action element not resolved: {detail}")]
    ElementNotResolved { detail: String },
    #[error("action element pattern unsupported: {detail}")]
    ElementPatternUnsupported {
        element_id: ElementId,
        detail: String,
    },
    #[error("transient element expired: {detail}")]
    TransientElementExpired {
        element_id: ElementId,
        detail: String,
    },
    #[error("action foreground lost: {detail}")]
    ForegroundLost { detail: String },
    #[error("action unsupported key: {detail}")]
    UnsupportedKey { detail: String },
    #[error("action observed no state delta: {detail}")]
    NoObservedDelta { detail: String },
    #[error("action drag distance exceeds limit: {detail}")]
    DragDistanceExceedsLimit { detail: String },
    #[error("stuck key auto-released: {detail}")]
    StuckKeyAutoReleased { detail: String },
    #[error("safety release-all fired: {detail}")]
    SafetyReleaseAllFired { detail: String },
    #[error("safety operator hotkey fired: {detail}")]
    SafetyOperatorHotkeyFired { detail: String },
}

impl ActionError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::QueueFull { .. } => error_codes::ACTION_QUEUE_FULL,
            Self::RateLimited { .. } => error_codes::ACTION_RATE_LIMITED,
            Self::ForegroundLeaseBusy { .. } => error_codes::ACTION_FOREGROUND_LEASE_BUSY,
            Self::BackendUnavailable { .. } => error_codes::ACTION_BACKEND_UNAVAILABLE,
            Self::ForegroundActivationRefused { .. } => error_codes::FOREGROUND_ACTIVATION_REFUSED,
            Self::TargetInvalid { .. } => error_codes::ACTION_TARGET_INVALID,
            Self::HoldExceededMax { .. } => error_codes::ACTION_HOLD_EXCEEDED_MAX,
            Self::VigemNotInstalled { .. } => error_codes::ACTION_VIGEM_NOT_INSTALLED,
            Self::VigemPluginFailed { .. } => error_codes::ACTION_VIGEM_PLUGIN_FAILED,
            Self::ElementNotResolved { .. } => error_codes::ACTION_ELEMENT_NOT_RESOLVED,
            Self::ElementPatternUnsupported { .. } => {
                error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED
            }
            Self::TransientElementExpired { .. } => error_codes::TRANSIENT_ELEMENT_EXPIRED,
            Self::ForegroundLost { .. } => error_codes::ACTION_FOREGROUND_LOST,
            Self::UnsupportedKey { .. } => error_codes::ACTION_UNSUPPORTED_KEY,
            Self::NoObservedDelta { .. } => error_codes::ACTION_NO_OBSERVED_DELTA,
            Self::DragDistanceExceedsLimit { .. } => {
                error_codes::ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT
            }
            Self::StuckKeyAutoReleased { .. } => error_codes::STUCK_KEY_AUTO_RELEASED,
            Self::SafetyReleaseAllFired { .. } => error_codes::SAFETY_RELEASE_ALL_FIRED,
            Self::SafetyOperatorHotkeyFired { .. } => error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
        }
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::QueueFull { detail }
            | Self::RateLimited { detail, .. }
            | Self::ForegroundLeaseBusy { detail, .. }
            | Self::BackendUnavailable { detail }
            | Self::ForegroundActivationRefused { detail }
            | Self::TargetInvalid { detail }
            | Self::HoldExceededMax { detail }
            | Self::VigemNotInstalled { detail }
            | Self::VigemPluginFailed { detail }
            | Self::ElementNotResolved { detail }
            | Self::ElementPatternUnsupported { detail, .. }
            | Self::TransientElementExpired { detail, .. }
            | Self::ForegroundLost { detail }
            | Self::UnsupportedKey { detail }
            | Self::NoObservedDelta { detail }
            | Self::DragDistanceExceedsLimit { detail }
            | Self::StuckKeyAutoReleased { detail }
            | Self::SafetyReleaseAllFired { detail }
            | Self::SafetyOperatorHotkeyFired { detail } => detail,
        }
    }

    #[must_use]
    pub fn with_detail(self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        match self {
            Self::QueueFull { .. } => Self::QueueFull { detail },
            Self::RateLimited { retry_after_ms, .. } => Self::RateLimited {
                detail,
                retry_after_ms,
            },
            Self::ForegroundLeaseBusy {
                holder_session_id,
                requesting_session_id,
                retry_after_ms,
                ..
            } => Self::ForegroundLeaseBusy {
                detail,
                holder_session_id,
                requesting_session_id,
                retry_after_ms,
            },
            Self::BackendUnavailable { .. } => Self::BackendUnavailable { detail },
            Self::ForegroundActivationRefused { .. } => {
                Self::ForegroundActivationRefused { detail }
            }
            Self::TargetInvalid { .. } => Self::TargetInvalid { detail },
            Self::HoldExceededMax { .. } => Self::HoldExceededMax { detail },
            Self::VigemNotInstalled { .. } => Self::VigemNotInstalled { detail },
            Self::VigemPluginFailed { .. } => Self::VigemPluginFailed { detail },
            Self::ElementNotResolved { .. } => Self::ElementNotResolved { detail },
            Self::ElementPatternUnsupported { element_id, .. } => {
                Self::ElementPatternUnsupported { element_id, detail }
            }
            Self::TransientElementExpired { element_id, .. } => {
                Self::TransientElementExpired { element_id, detail }
            }
            Self::ForegroundLost { .. } => Self::ForegroundLost { detail },
            Self::UnsupportedKey { .. } => Self::UnsupportedKey { detail },
            Self::NoObservedDelta { .. } => Self::NoObservedDelta { detail },
            Self::DragDistanceExceedsLimit { .. } => Self::DragDistanceExceedsLimit { detail },
            Self::StuckKeyAutoReleased { .. } => Self::StuckKeyAutoReleased { detail },
            Self::SafetyReleaseAllFired { .. } => Self::SafetyReleaseAllFired { detail },
            Self::SafetyOperatorHotkeyFired { .. } => Self::SafetyOperatorHotkeyFired { detail },
        }
    }

    #[must_use]
    pub const fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms, .. }
            | Self::ForegroundLeaseBusy { retry_after_ms, .. } => Some(*retry_after_ms),
            Self::QueueFull { .. }
            | Self::BackendUnavailable { .. }
            | Self::ForegroundActivationRefused { .. }
            | Self::TargetInvalid { .. }
            | Self::HoldExceededMax { .. }
            | Self::VigemNotInstalled { .. }
            | Self::VigemPluginFailed { .. }
            | Self::ElementNotResolved { .. }
            | Self::ElementPatternUnsupported { .. }
            | Self::TransientElementExpired { .. }
            | Self::ForegroundLost { .. }
            | Self::UnsupportedKey { .. }
            | Self::NoObservedDelta { .. }
            | Self::DragDistanceExceedsLimit { .. }
            | Self::StuckKeyAutoReleased { .. }
            | Self::SafetyReleaseAllFired { .. }
            | Self::SafetyOperatorHotkeyFired { .. } => None,
        }
    }
}

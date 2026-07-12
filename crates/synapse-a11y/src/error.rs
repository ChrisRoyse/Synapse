use synapse_core::error_codes;
use thiserror::Error;

pub type A11yResult<T> = Result<T, A11yError>;

#[derive(Debug, Error)]
pub enum A11yError {
    #[error("Windows UI Automation is not available: {detail}")]
    NotAvailable { detail: String },
    #[error("no foreground window is available: {detail}")]
    NoForeground { detail: String },
    #[error("UI Automation element is stale: {detail}")]
    ElementStale { detail: String },
    #[error("UI Automation element has no supported click control pattern: {detail}")]
    ElementPatternUnsupported { detail: String },
    #[error("UI Automation element does not support ValuePattern: {detail}")]
    ElementValueUnsupported { detail: String },
    #[error("UI Automation element ValuePattern is read-only: {detail}")]
    ElementValueReadOnly { detail: String },
    #[error("UI Automation element is not enabled: {detail}")]
    ElementNotEnabled { detail: String },
    #[error("Chromium DevTools Protocol is unreachable: {detail}")]
    CdpUnreachable { detail: String },
    #[error("Chromium DevTools Protocol attach failed: {detail}")]
    CdpAttachFailed { detail: String },
    #[error("Chromium accessibility tree retrieval failed: {detail}")]
    CdpAxtreeFailed { detail: String },
    #[error("browser wait timed out: {detail}")]
    BrowserWaitTimeout { detail: String },
    #[error("browser evaluate timed out: {detail}")]
    CdpEvaluateTimeout { detail: String },
    #[error("UI Automation worker job timed out: {detail}")]
    UiaWorkerTimeout { detail: String },
    #[error("foreground activation refused: {detail}")]
    ForegroundActivationRefused { detail: String },
    #[error("invalid element id: {detail}")]
    InvalidElementId { detail: String },
    #[error("accessibility backend failed: {detail}")]
    Internal { detail: String },
}

impl A11yError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotAvailable { .. } => error_codes::A11Y_NOT_AVAILABLE,
            Self::NoForeground { .. } => error_codes::A11Y_NO_FOREGROUND,
            Self::ElementStale { .. } => error_codes::A11Y_ELEMENT_STALE,
            Self::ElementPatternUnsupported { .. } => {
                error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED
            }
            Self::ElementValueUnsupported { .. } => error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED,
            Self::ElementValueReadOnly { .. } | Self::ElementNotEnabled { .. } => {
                error_codes::ACTION_TARGET_INVALID
            }
            Self::CdpUnreachable { .. } => error_codes::A11Y_CDP_UNREACHABLE,
            Self::CdpAttachFailed { .. } => error_codes::A11Y_CDP_ATTACH_FAILED,
            Self::CdpAxtreeFailed { .. } => error_codes::A11Y_CDP_AXTREE_FAILED,
            Self::BrowserWaitTimeout { .. } => error_codes::BROWSER_WAIT_TIMEOUT,
            Self::CdpEvaluateTimeout { .. } => error_codes::BROWSER_EVALUATE_TIMEOUT,
            Self::UiaWorkerTimeout { .. } => error_codes::A11Y_UIA_WORKER_TIMEOUT,
            Self::ForegroundActivationRefused { .. } => error_codes::FOREGROUND_ACTIVATION_REFUSED,
            Self::InvalidElementId { .. } | Self::Internal { .. } => error_codes::OBSERVE_INTERNAL,
        }
    }

    #[must_use]
    pub fn not_available(detail: impl Into<String>) -> Self {
        Self::NotAvailable {
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn foreground_activation_refused(detail: impl Into<String>) -> Self {
        Self::ForegroundActivationRefused {
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn uia_worker_timeout(detail: impl Into<String>) -> Self {
        Self::UiaWorkerTimeout {
            detail: detail.into(),
        }
    }
}

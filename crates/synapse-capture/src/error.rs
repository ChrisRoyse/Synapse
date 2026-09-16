use synapse_core::error_codes;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("CAPTURE_GRAPHICS_API_UNSUPPORTED: {detail}")]
    GraphicsApiUnsupported { detail: String },
    #[error("CAPTURE_PRINTWINDOW_DISABLED: {detail}")]
    PrintWindowDisabled { detail: String },
    #[error("CAPTURE_PRINTWINDOW_BLACK: {detail}")]
    PrintWindowBlack { detail: String },
    #[error("CAPTURE_TARGET_LOST: {detail}")]
    TargetLost { detail: String },
    #[error("CAPTURE_TARGET_INVALID: {detail}")]
    TargetInvalid { detail: String },
    #[error("CAPTURE_NO_DIRTY_REGIONS")]
    NoDirtyRegions,
    #[error("CAPTURE_THREAD_FAILED: {detail}")]
    ThreadFailed { detail: String },
}

impl CaptureError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::GraphicsApiUnsupported { .. } => error_codes::CAPTURE_GRAPHICS_API_UNSUPPORTED,
            Self::PrintWindowDisabled { .. } => error_codes::CAPTURE_PRINTWINDOW_DISABLED,
            Self::PrintWindowBlack { .. } => error_codes::CAPTURE_PRINTWINDOW_BLACK,
            Self::TargetLost { .. } => error_codes::CAPTURE_TARGET_LOST,
            Self::TargetInvalid { .. } => error_codes::CAPTURE_TARGET_INVALID,
            Self::NoDirtyRegions => error_codes::CAPTURE_NO_DIRTY_REGIONS,
            Self::ThreadFailed { .. } => "CAPTURE_THREAD_FAILED",
        }
    }
}

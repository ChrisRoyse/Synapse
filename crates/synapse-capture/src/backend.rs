#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CaptureBackend {
    GraphicsCaptureApi,
    DxgiDuplication,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CaptureBackendPreference {
    Auto,
    GraphicsCaptureApi,
    DxgiDuplication,
}

impl CaptureBackendPreference {
    #[must_use]
    pub fn from_force_dxgi_value(value: Option<&str>) -> Self {
        capture_backend_from_env(value)
    }
}

pub const fn resolved_backend(preference: CaptureBackendPreference) -> CaptureBackend {
    match preference {
        CaptureBackendPreference::Auto | CaptureBackendPreference::GraphicsCaptureApi => {
            CaptureBackend::GraphicsCaptureApi
        }
        CaptureBackendPreference::DxgiDuplication => CaptureBackend::DxgiDuplication,
    }
}

pub fn capture_backend_from_env(value: Option<&str>) -> CaptureBackendPreference {
    match value {
        Some("1" | "true" | "TRUE" | "yes" | "YES") => CaptureBackendPreference::DxgiDuplication,
        _ => CaptureBackendPreference::Auto,
    }
}

pub const fn backend_after_fallback(
    preference: CaptureBackendPreference,
    err: &crate::CaptureError,
) -> CaptureBackend {
    match (preference, err) {
        (CaptureBackendPreference::Auto, crate::CaptureError::GraphicsApiUnsupported { .. }) => {
            CaptureBackend::DxgiDuplication
        }
        _ => resolved_backend(preference),
    }
}

pub const fn should_fallback_to_dxgi(
    preference: CaptureBackendPreference,
    err: &crate::CaptureError,
) -> bool {
    matches!(
        (preference, err),
        (
            CaptureBackendPreference::Auto,
            crate::CaptureError::GraphicsApiUnsupported { .. }
        )
    )
}

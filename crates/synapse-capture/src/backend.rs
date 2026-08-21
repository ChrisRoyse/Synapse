#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CaptureBackend {
    GdiBitBlt,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CaptureBackendPreference {
    GdiBitBlt,
    GraphicsCaptureApi,
    DxgiDuplication,
    InvalidEnvironment,
}

pub const fn resolved_backend(preference: CaptureBackendPreference) -> CaptureBackend {
    let _ = preference;
    CaptureBackend::GdiBitBlt
}

/// Resolves the strict no-explicit-GPU-API capture policy from environment.
///
/// # Errors
///
/// Returns [`crate::CaptureError`] for non-Unicode values or any explicit
/// automatic/GPU-backed/unknown backend request.
pub fn capture_backend_preference_from_environment()
-> Result<CaptureBackendPreference, crate::CaptureError> {
    let explicit = std::env::var("SYNAPSE_CAPTURE_BACKEND");
    let explicit_preference = match explicit {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "cpu" | "gdi" | "gdi_bitblt" => Some(CaptureBackendPreference::GdiBitBlt),
            _ => {
                return Err(crate::CaptureError::UnsupportedSemantics {
                    detail: format!(
                        "SYNAPSE_CAPTURE_BACKEND={value:?} contradicts the no-explicit-GPU-API capture contract; accepted values are cpu, gdi, or gdi_bitblt"
                    ),
                });
            }
        },
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(crate::CaptureError::UnsupportedSemantics {
                detail: "SYNAPSE_CAPTURE_BACKEND is not valid Unicode; remove it or set it to cpu"
                    .to_owned(),
            });
        }
    };

    let legacy = std::env::var("SYNAPSE_CAPTURE_FORCE_DXGI");
    let legacy_preference = match legacy {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "no" => None,
            "1" | "true" | "yes" => {
                return Err(crate::CaptureError::UnsupportedSemantics {
                    detail: "SYNAPSE_CAPTURE_FORCE_DXGI requests an explicit GPU API prohibited by the capture contract; remove it or set it to false"
                        .to_owned(),
                });
            }
            _ => {
                return Err(crate::CaptureError::UnsupportedSemantics {
                    detail: format!(
                        "SYNAPSE_CAPTURE_FORCE_DXGI={value:?} is invalid; remove it or set it to false"
                    ),
                });
            }
        },
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(crate::CaptureError::UnsupportedSemantics {
                detail:
                    "SYNAPSE_CAPTURE_FORCE_DXGI is not valid Unicode; remove it or set it to false"
                        .to_owned(),
            });
        }
    };

    Ok(explicit_preference
        .or(legacy_preference)
        .unwrap_or(CaptureBackendPreference::GdiBitBlt))
}

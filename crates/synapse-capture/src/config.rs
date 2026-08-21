use crate::{
    CaptureBackend, CaptureBackendPreference, DEFAULT_CAPTURE_INTERVAL_MS,
    backend::resolved_backend,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CaptureTarget {
    #[default]
    Primary,
    Monitor {
        monitor_index: u32,
    },
    Window {
        hwnd: i64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureConfig {
    pub target: CaptureTarget,
    pub min_update_interval_ms: u64,
    pub cursor_visible: bool,
    pub secondary_windows: bool,
    pub dirty_region_only: bool,
    pub backend_preference: CaptureBackendPreference,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::Primary,
            min_update_interval_ms: DEFAULT_CAPTURE_INTERVAL_MS,
            cursor_visible: true,
            secondary_windows: false,
            dirty_region_only: false,
            backend_preference: CaptureBackendPreference::GdiBitBlt,
        }
    }
}

impl CaptureConfig {
    #[must_use]
    pub fn with_env_backend(mut self) -> Self {
        self.backend_preference = crate::backend::capture_backend_preference_from_environment()
            .unwrap_or(CaptureBackendPreference::InvalidEnvironment);
        self
    }

    #[must_use]
    pub const fn selected_backend(&self) -> CaptureBackend {
        resolved_backend(self.backend_preference)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCaptureTarget {
    pub target: CaptureTarget,
    pub backend: CaptureBackend,
}

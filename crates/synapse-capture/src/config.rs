use crate::{CaptureBackend, CaptureBackendPreference, backend::resolved_backend};

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
            min_update_interval_ms: 16,
            cursor_visible: true,
            secondary_windows: true,
            dirty_region_only: true,
            backend_preference: CaptureBackendPreference::Auto,
        }
    }
}

impl CaptureConfig {
    #[must_use]
    pub fn with_env_backend(mut self) -> Self {
        self.backend_preference = CaptureBackendPreference::from_force_dxgi_value(
            std::env::var("SYNAPSE_CAPTURE_FORCE_DXGI").ok().as_deref(),
        );
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

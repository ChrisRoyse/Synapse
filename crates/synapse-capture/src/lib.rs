#![allow(unsafe_code)]

mod backend;
mod bitmap;
mod config;
mod controller;
mod coords;
mod dpi;
mod error;
mod frame;
mod platform;
mod stats;

pub use backend::{CaptureBackend, CaptureBackendPreference};
// `screen_region_to_bgra_bitmap` is cross-platform (fails loud off Windows); the
// WinRT `SoftwareBitmap` helpers in `bitmap` stay `#[cfg(windows)]`, so off
// Windows this glob re-exports only the BGRA entry point that `synapse-mcp` calls.
pub use bitmap::*;
pub use config::{CaptureConfig, CaptureTarget, ResolvedCaptureTarget};
pub use controller::{
    CaptureController, CaptureHandle, register_capture_metrics, resolve_capture_target,
    spawn_capture_loop, validate_hwnd,
};
pub use coords::*;
pub use dpi::*;
pub use error::*;
pub use frame::*;
pub use stats::{CaptureStats, CaptureThreadPriority};

#[cfg(test)]
pub(crate) use backend::{backend_after_fallback, should_fallback_to_dxgi};

pub const CAPTURE_CHANNEL_CAPACITY: usize = 2;
pub const FRAMES_DROPPED_METRIC: &str = "synapse_capture_frames_dropped_total";

#[cfg(test)]
mod tests;

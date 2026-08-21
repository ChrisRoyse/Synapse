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

pub use backend::{
    CaptureBackend, CaptureBackendPreference, capture_backend_preference_from_environment,
};
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
pub use stats::{CaptureStats, CaptureTerminalError, CaptureThreadPriority};

pub const CAPTURE_CHANNEL_CAPACITY: usize = 1;
pub const DEFAULT_CAPTURE_INTERVAL_MS: u64 = 250;
pub const MIN_CAPTURE_INTERVAL_MS: u64 = 250;
pub const MAX_CAPTURE_INTERVAL_MS: u64 = 60_000;
pub const MAX_CAPTURE_BYTES: usize = 40 * 1024 * 1024;
pub const GPU_CAPTURE_BACKENDS_COMPILED: bool = false;
/// Canonical backend identity for the capture policy.
///
/// GDI `BitBlt` avoids explicit DXGI/D3D/Windows.Graphics.Capture APIs, but the
/// Windows compositor or display driver may still accelerate GDI internally.
/// This identity therefore makes no unsupported claim about physical GPU RAM.
pub const NO_EXPLICIT_GPU_API_CAPTURE_BACKEND: &str = "gdi_bitblt_no_explicit_gpu_api";
pub const FRAMES_DROPPED_METRIC: &str = "synapse_capture_frames_dropped_total";

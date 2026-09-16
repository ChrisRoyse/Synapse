use synapse_core::{Point, Rect};

use crate::{
    CaptureConfig, CaptureError, CaptureThreadPriority, CapturedBgraBitmap,
    CapturedWindowBgraBitmap, DpiAwarenessStatus, controller::CaptureThreadContext,
};

/// Builds the error returned by every capture entry point on non-Windows builds.
///
/// Real screen capture in Synapse is implemented only on Windows (DXGI Desktop
/// Duplication and `Windows.Graphics.Capture`). Earlier non-Windows builds
/// produced *synthetic* placeholder frames here, which silently fed fabricated
/// pixels into perception. That is mock data masquerading as a real capture and
/// is intentionally removed: a build that cannot see the screen must fail loudly
/// instead of pretending to succeed.
#[cfg(not(windows))]
fn capture_backend_unavailable() -> CaptureError {
    let detail = format!(
        "real screen capture is implemented only on Windows (DXGI Desktop Duplication / \
         Windows.Graphics.Capture); this {} build has no capture backend. Run the Windows \
         synapse-mcp build to perceive a real desktop. Synthetic/placeholder frames are \
         intentionally not produced so perception never reports fabricated pixels.",
        std::env::consts::OS
    );
    tracing::error!(
        code = synapse_core::error_codes::CAPTURE_GRAPHICS_API_UNSUPPORTED,
        platform = std::env::consts::OS,
        "screen capture requested on a non-Windows build that has no capture backend"
    );
    CaptureError::GraphicsApiUnsupported { detail }
}

#[cfg(not(windows))]
#[allow(clippy::needless_pass_by_value)]
pub fn run_graphics_capture(
    _config: CaptureConfig,
    _ctx: CaptureThreadContext,
) -> Result<(), CaptureError> {
    Err(capture_backend_unavailable())
}

#[cfg(not(windows))]
#[allow(clippy::needless_pass_by_value)]
pub fn run_dxgi_capture(
    _config: CaptureConfig,
    _ctx: CaptureThreadContext,
) -> Result<(), CaptureError> {
    Err(capture_backend_unavailable())
}

/// Non-Windows stand-in for the Windows GDI region grab. Real region capture
/// (`platform/windows/bitmap.rs`) exists only on Windows; off Windows this fails
/// loudly with `CAPTURE_GRAPHICS_API_UNSUPPORTED` rather than returning blank or
/// fabricated pixels, so OCR/detection callers never operate on mock image data.
#[cfg(not(windows))]
pub fn screen_region_to_bgra_bitmap(_region: Rect) -> Result<CapturedBgraBitmap, CaptureError> {
    Err(capture_backend_unavailable())
}

pub fn window_region_to_bgra_bitmap(
    _hwnd: i64,
    _region: Rect,
    _timeout_ms: u64,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    Err(capture_backend_unavailable())
}

pub fn window_full_frame_to_bgra_bitmap(
    _hwnd: i64,
    _timeout_ms: u64,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    Err(capture_backend_unavailable())
}

pub fn window_region_to_bgra_bitmap_printwindow(
    _hwnd: i64,
    _region: Rect,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    Err(capture_backend_unavailable())
}

pub fn window_capture_region(_hwnd: i64) -> Result<Rect, CaptureError> {
    Err(capture_backend_unavailable())
}

pub fn client_region_to_window_region(_hwnd: i64, _region: Rect) -> Result<Rect, CaptureError> {
    Err(capture_backend_unavailable())
}

#[cfg(not(windows))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
pub fn screen_to_window_impl(point: Point, _hwnd: i64) -> Result<Point, CaptureError> {
    Ok(point)
}

#[cfg(not(windows))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
pub fn window_to_screen_impl(point: Point, _hwnd: i64) -> Result<Point, CaptureError> {
    Ok(point)
}

#[cfg(not(windows))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
pub fn init_process_dpi_awareness_impl() -> Result<DpiAwarenessStatus, CaptureError> {
    Ok(DpiAwarenessStatus::Unsupported)
}

#[cfg(not(windows))]
pub const fn is_per_monitor_v2_dpi_aware_impl() -> bool {
    false
}

#[cfg(not(windows))]
pub const fn current_thread_priority_impl() -> CaptureThreadPriority {
    CaptureThreadPriority::Unsupported
}

#[cfg(not(windows))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
pub fn set_capture_thread_priority() -> Result<(), CaptureError> {
    Ok(())
}

#[cfg(not(windows))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
pub fn validate_hwnd_impl(_hwnd: i64) -> Result<(), CaptureError> {
    Ok(())
}

#[cfg(not(windows))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
pub fn validate_monitor_impl(_monitor_index: u32) -> Result<(), CaptureError> {
    Ok(())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    /// The non-Windows capture path must fail loudly with the
    /// `CAPTURE_GRAPHICS_API_UNSUPPORTED` code and a detail that explains the
    /// real cause, instead of fabricating synthetic frames.
    #[test]
    fn capture_backend_unavailable_reports_graphics_api_unsupported() {
        let err = capture_backend_unavailable();
        assert_eq!(
            err.code(),
            synapse_core::error_codes::CAPTURE_GRAPHICS_API_UNSUPPORTED
        );
        match err {
            CaptureError::GraphicsApiUnsupported { detail } => {
                assert!(
                    detail.contains("only on Windows"),
                    "detail should name the Windows-only constraint: {detail}"
                );
                assert!(
                    detail.to_lowercase().contains("synthetic"),
                    "detail should state synthetic frames are not produced: {detail}"
                );
                assert!(
                    detail.contains(std::env::consts::OS),
                    "detail should name the current platform: {detail}"
                );
            }
            other => panic!("expected GraphicsApiUnsupported, got {other:?}"),
        }
    }
}

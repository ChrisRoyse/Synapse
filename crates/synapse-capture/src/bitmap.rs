use synapse_core::Rect;

use crate::{CaptureError, CapturedBgraBitmap, CapturedWindowBgraBitmap, platform};

// `CapturedFrame`/`CapturedSoftwareBitmap` only feed the Windows-only WinRT
// `SoftwareBitmap` helpers below.
#[cfg(windows)]
use crate::{CapturedFrame, CapturedSoftwareBitmap};

#[cfg(windows)]
/// Copies a captured frame region into a `WinRT` `SoftwareBitmap`.
///
/// # Errors
///
/// Returns [`CaptureError`] when the region is empty/outside the frame, the
/// frame format is unsupported, or the D3D/WinRT copy fails.
pub fn captured_frame_region_to_software_bitmap(
    frame: &CapturedFrame,
    region: Rect,
) -> Result<CapturedSoftwareBitmap, CaptureError> {
    platform::captured_frame_region_to_software_bitmap(frame, region)
}

#[cfg(windows)]
/// Copies a captured frame region into raw BGRA bytes.
///
/// # Errors
///
/// Returns [`CaptureError`] when the region is empty/outside the frame, the
/// frame format is unsupported, or the D3D copy fails.
pub fn captured_frame_region_to_bgra_bitmap(
    frame: &CapturedFrame,
    region: Rect,
) -> Result<CapturedBgraBitmap, CaptureError> {
    platform::captured_frame_region_to_bgra_bitmap(frame, region)
}

#[cfg(windows)]
/// Captures a screen-coordinate region into a `WinRT` `SoftwareBitmap`.
///
/// # Errors
///
/// Returns [`CaptureError`] when the region is empty or the `GDI`/`WinRT`
/// copy fails.
pub fn screen_region_to_software_bitmap(
    region: Rect,
) -> Result<CapturedSoftwareBitmap, CaptureError> {
    platform::screen_region_to_software_bitmap(region)
}

/// Captures a screen-coordinate region into raw BGRA bytes.
///
/// Available on all platforms so `synapse-mcp`'s OCR/detection callers compile
/// everywhere. The real GDI capture exists only on Windows; on non-Windows the
/// platform impl returns `Err(GraphicsApiUnsupported)` (it never fabricates
/// pixels), so callers fail loudly instead of acting on mock image data.
///
/// # Errors
///
/// Returns [`CaptureError`] when the region is empty or the `GDI` capture fails
/// (Windows), or `GraphicsApiUnsupported` on any non-Windows build.
pub fn screen_region_to_bgra_bitmap(region: Rect) -> Result<CapturedBgraBitmap, CaptureError> {
    platform::screen_region_to_bgra_bitmap(region)
}

/// Captures a window-relative region into raw BGRA bytes. Windows uses passive
/// WGC `CreateForWindow` capture and reports that backend in the result.
/// Non-Windows builds fail loudly.
///
/// # Errors
///
/// Returns [`CaptureError`] when the HWND/region is invalid, no WGC frame
/// arrives, WGC returns blank output, or the bitmap copy fails. Synapse does
/// not automatically call `PrintWindow`, because Windows re-enters target
/// process `WM_PRINT`/`WM_PRINTCLIENT` handlers for that API.
pub fn window_region_to_bgra_bitmap(
    hwnd: i64,
    region: Rect,
    timeout_ms: u64,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    platform::window_region_to_bgra_bitmap(hwnd, region, timeout_ms)
}

/// Captures the entire window using the WGC frame's native dimensions, with no
/// client/window coordinate conversion.
///
/// This is the exact path for "capture/read the whole target window" because
/// it is immune to the `GetWindowRect` vs WGC-frame mismatch caused by
/// invisible DWM resize borders (#1203).
///
/// # Errors
///
/// Returns [`CaptureError`] when the HWND is invalid, no WGC frame arrives, WGC
/// returns blank output, or the bitmap copy fails (Windows), or
/// `GraphicsApiUnsupported` on non-Windows builds.
pub fn window_full_frame_to_bgra_bitmap(
    hwnd: i64,
    timeout_ms: u64,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    platform::window_full_frame_to_bgra_bitmap(hwnd, timeout_ms)
}

/// Explicit `PrintWindow` capture for a window-region bitmap.
///
/// This is intentionally separate from `window_region_to_bgra_bitmap`: callers
/// must opt in after proving the target is on a bounded worker desktop, because
/// Windows asks the target process to handle `WM_PRINT`/`WM_PRINTCLIENT`.
///
/// # Errors
///
/// Returns [`CaptureError`] when the HWND/region is invalid, `PrintWindow`
/// fails, returns all-zero pixels, or the `GDI` copy fails.
pub fn window_region_to_bgra_bitmap_printwindow(
    hwnd: i64,
    region: Rect,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    platform::window_region_to_bgra_bitmap_printwindow(hwnd, region)
}

/// Returns the full window bitmap region used by `window_region_to_bgra_bitmap`.
/// For minimized Windows targets this uses the restored placement extent rather
/// than the minimized icon rectangle.
///
/// # Errors
///
/// Returns [`CaptureError`] when the HWND cannot be resolved or the resulting
/// bitmap bounds are empty/invalid.
pub fn window_capture_region(hwnd: i64) -> Result<Rect, CaptureError> {
    platform::window_capture_region(hwnd)
}

/// Converts a client-relative region to the full-window coordinate space used
/// by per-window WGC frames for this HWND.
///
/// # Errors
///
/// Returns [`CaptureError`] when the HWND cannot be resolved.
pub fn client_region_to_window_region(hwnd: i64, region: Rect) -> Result<Rect, CaptureError> {
    platform::client_region_to_window_region(hwnd, region)
}

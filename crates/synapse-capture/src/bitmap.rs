use synapse_core::Rect;

use crate::{CaptureError, CapturedBgraBitmap, platform};

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

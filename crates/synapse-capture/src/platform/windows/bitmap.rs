use std::{
    cell::RefCell,
    ffi::c_void,
    mem::size_of,
    slice, thread,
    time::{Duration, Instant},
};

use synapse_core::Rect;
use windows::{
    Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
    Storage::Streams::DataWriter,
    Win32::Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ, RDW_ALLCHILDREN,
            RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, ReleaseDC, SRCCOPY, SelectObject,
        },
    },
    Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow},
    Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow},
    Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetMenu, GetWindowLongW, GetWindowPlacement,
        GetWindowRect, IsIconic, PW_RENDERFULLCONTENT, WINDOW_EX_STYLE, WINDOW_STYLE,
        WINDOWPLACEMENT,
    },
};

use crate::{
    CaptureBackendPreference, CaptureConfig, CaptureError, CaptureTarget, CapturedBgraBitmap,
    CapturedFrame, CapturedSoftwareBitmap, CapturedWindowBgraBitmap, DxgiFormat,
    spawn_capture_loop,
};

use super::common::{capture_unsupported, hwnd_from_i64};

thread_local! {
    static SCREEN_CAPTURE_SCRATCH: RefCell<Option<GdiCaptureScratch>> = const { RefCell::new(None) };
}

const WGC_WINDOW_FRAME_MAX_ATTEMPTS: u32 = 3;
const WGC_WINDOW_FRAME_RETRY_BACKOFF_MS: u64 = 75;

pub fn captured_frame_region_to_software_bitmap(
    frame: &CapturedFrame,
    region: Rect,
) -> Result<CapturedSoftwareBitmap, CaptureError> {
    let region = clamp_region_to_frame(frame, region)?;
    let bytes = copy_region_bgra(frame, region)?;
    let bitmap = software_bitmap_from_bgra(&bytes, region.w, region.h)?;
    Ok(CapturedSoftwareBitmap { region, bitmap })
}

pub fn captured_frame_region_to_bgra_bitmap(
    frame: &CapturedFrame,
    region: Rect,
) -> Result<CapturedBgraBitmap, CaptureError> {
    let region = clamp_region_to_frame(frame, region)?;
    let bytes = copy_region_bgra(frame, region)?;
    Ok(CapturedBgraBitmap {
        region,
        width: u32::try_from(region.w).unwrap_or_default(),
        height: u32::try_from(region.h).unwrap_or_default(),
        bytes,
    })
}

pub fn screen_region_to_software_bitmap(
    region: Rect,
) -> Result<CapturedSoftwareBitmap, CaptureError> {
    validate_bitmap_region(region)?;
    let bytes = copy_screen_region_bgra(region)?;
    let bitmap = software_bitmap_from_bgra(&bytes, region.w, region.h)?;
    Ok(CapturedSoftwareBitmap { region, bitmap })
}

pub fn screen_region_to_bgra_bitmap(region: Rect) -> Result<CapturedBgraBitmap, CaptureError> {
    validate_bitmap_region(region)?;
    let bytes = copy_screen_region_bgra(region)?;
    Ok(CapturedBgraBitmap {
        region,
        width: u32::try_from(region.w).unwrap_or_default(),
        height: u32::try_from(region.h).unwrap_or_default(),
        bytes,
    })
}

pub fn window_region_to_bgra_bitmap(
    hwnd: i64,
    region: Rect,
    timeout_ms: u64,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    validate_bitmap_region(region)?;
    match graphics_capture_window_region_to_bgra_bitmap(hwnd, region, timeout_ms) {
        Ok(capture) if !is_all_zero_bgra(&capture.bitmap.bytes) => Ok(CapturedWindowBgraBitmap {
            bitmap: capture.bitmap,
            capture_backend: "graphics_capture_window_bgra",
            capture_attempts: capture.attempts,
            capture_retry_count: capture.retry_count,
            capture_elapsed_ms: capture.elapsed_ms,
            capture_retry_backoff_ms: capture.retry_backoff_ms,
        }),
        Ok(_bitmap) => {
            tracing::error!(
                code = "CAPTURE_WGC_WINDOW_ALL_ZERO_PRINTWINDOW_DISABLED",
                hwnd,
                region = ?region,
                "WGC window capture returned all-zero pixels; refusing target-reentering PrintWindow fallback"
            );
            Err(CaptureError::PrintWindowDisabled {
                detail: format!(
                    "WGC window capture for hwnd {hwnd:#x} region {region:?} returned all-zero pixels; PrintWindow fallback is disabled because Windows asks the target process to handle WM_PRINT/WM_PRINTCLIENT and can surface app-visible failures"
                ),
            })
        }
        Err(wgc_error) => {
            tracing::error!(
                code = "CAPTURE_WGC_WINDOW_FAILED_PRINTWINDOW_DISABLED",
                hwnd,
                region = ?region,
                error = %wgc_error,
                "WGC window capture failed; refusing target-reentering PrintWindow fallback"
            );
            Err(CaptureError::PrintWindowDisabled {
                detail: format!(
                    "WGC window capture failed for hwnd {hwnd:#x} region {region:?}: {wgc_error}; PrintWindow fallback is disabled because Windows asks the target process to handle WM_PRINT/WM_PRINTCLIENT and can surface app-visible failures"
                ),
            })
        }
    }
}

/// Captures the entire window using the WGC frame's own native dimensions.
///
/// Unlike [`window_region_to_bgra_bitmap`], this performs no client/window
/// coordinate math: the returned bitmap is exactly the captured DWM surface, so
/// it is immune to the `GetWindowRect` vs WGC-frame size mismatch caused by
/// invisible resize borders (#1203).
///
/// # Errors
///
/// Returns [`CaptureError`] when the HWND is invalid, no WGC frame arrives, WGC
/// returns blank output, or the bitmap copy fails. `PrintWindow` fallback stays
/// disabled, identical to [`window_region_to_bgra_bitmap`].
pub fn window_full_frame_to_bgra_bitmap(
    hwnd: i64,
    timeout_ms: u64,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    match graphics_capture_window_full_frame_to_bgra_bitmap(hwnd, timeout_ms) {
        Ok(capture) if !is_all_zero_bgra(&capture.bitmap.bytes) => Ok(CapturedWindowBgraBitmap {
            bitmap: capture.bitmap,
            capture_backend: "graphics_capture_window_bgra",
            capture_attempts: capture.attempts,
            capture_retry_count: capture.retry_count,
            capture_elapsed_ms: capture.elapsed_ms,
            capture_retry_backoff_ms: capture.retry_backoff_ms,
        }),
        Ok(_bitmap) => {
            tracing::error!(
                code = "CAPTURE_WGC_WINDOW_ALL_ZERO_PRINTWINDOW_DISABLED",
                hwnd,
                "WGC whole-window capture returned all-zero pixels; refusing target-reentering PrintWindow fallback"
            );
            Err(CaptureError::PrintWindowDisabled {
                detail: format!(
                    "WGC whole-window capture for hwnd {hwnd:#x} returned all-zero pixels; PrintWindow fallback is disabled because Windows asks the target process to handle WM_PRINT/WM_PRINTCLIENT and can surface app-visible failures"
                ),
            })
        }
        Err(wgc_error) => {
            tracing::error!(
                code = "CAPTURE_WGC_WINDOW_FAILED_PRINTWINDOW_DISABLED",
                hwnd,
                error = %wgc_error,
                "WGC whole-window capture failed; refusing target-reentering PrintWindow fallback"
            );
            Err(wgc_error)
        }
    }
}

pub fn window_region_to_bgra_bitmap_printwindow(
    hwnd: i64,
    region: Rect,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    validate_bitmap_region(region)?;
    let hwnd_value = hwnd;
    let hwnd = hwnd_from_i64(hwnd)?;
    let (window_width, window_height) = window_capture_extent(hwnd)?;
    validate_region_inside_window(region, window_width, window_height)?;
    let bytes = printwindow_region_bgra(hwnd, hwnd_value, region, window_width, window_height)?;
    if is_all_zero_bgra(&bytes) {
        tracing::warn!(
            code = synapse_core::error_codes::CAPTURE_PRINTWINDOW_BLACK,
            hwnd = hwnd_value,
            region = ?region,
            "PrintWindow returned all-zero pixels"
        );
        return Err(CaptureError::PrintWindowBlack {
            detail: format!(
                "PrintWindow returned all-zero pixels for hwnd {hwnd_value:#x} region {region:?}; target likely does not render through WM_PRINT/WM_PRINTCLIENT"
            ),
        });
    }
    Ok(CapturedWindowBgraBitmap {
        bitmap: CapturedBgraBitmap {
            region,
            width: u32::try_from(region.w).unwrap_or_default(),
            height: u32::try_from(region.h).unwrap_or_default(),
            bytes,
        },
        capture_backend: "printwindow",
        capture_attempts: 1,
        capture_retry_count: 0,
        capture_elapsed_ms: 0,
        capture_retry_backoff_ms: 0,
    })
}

pub fn window_capture_region(hwnd: i64) -> Result<Rect, CaptureError> {
    let hwnd = hwnd_from_i64(hwnd)?;
    let (w, h) = window_capture_extent(hwnd)?;
    let region = Rect { x: 0, y: 0, w, h };
    validate_bitmap_region(region)?;
    Ok(region)
}

pub fn client_region_to_window_region(hwnd: i64, region: Rect) -> Result<Rect, CaptureError> {
    validate_bitmap_region(region)?;
    let hwnd = hwnd_from_i64(hwnd)?;

    if unsafe { IsIconic(hwnd) }.as_bool() {
        let (window_width, window_height) = window_capture_extent(hwnd)?;
        let Some((offset_x, offset_y)) =
            minimized_client_offset_in_window_bitmap(hwnd, window_width, window_height)?
        else {
            return Err(CaptureError::TargetInvalid {
                detail: "minimized target has no client extent for region conversion".to_owned(),
            });
        };
        let window_region = Rect {
            x: region.x.saturating_add(offset_x),
            y: region.y.saturating_add(offset_y),
            w: region.w,
            h: region.h,
        };
        validate_region_inside_window(window_region, window_width, window_height)?;
        return Ok(window_region);
    }

    let mut client_rect = windows::Win32::Foundation::RECT::default();
    unsafe { GetClientRect(hwnd, &raw mut client_rect) }.map_err(capture_unsupported)?;
    let client_width = client_rect.right.saturating_sub(client_rect.left);
    let client_height = client_rect.bottom.saturating_sub(client_rect.top);
    let frame_rect = dwm_extended_frame_bounds(hwnd)?;
    let (frame_width, frame_height) = rect_extent(&frame_rect);
    let mut client_origin = windows::Win32::Foundation::POINT { x: 0, y: 0 };
    if !unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &raw mut client_origin) }
        .as_bool()
    {
        return Err(CaptureError::TargetInvalid {
            detail: "ClientToScreen failed while converting screenshot region".to_owned(),
        });
    }
    let offset_x = client_origin.x.saturating_sub(frame_rect.left);
    let offset_y = client_origin.y.saturating_sub(frame_rect.top);
    let window_region = client_region_to_frame_region(
        region,
        client_width,
        client_height,
        offset_x,
        offset_y,
        frame_width,
        frame_height,
    )?;
    Ok(window_region)
}

#[derive(Debug)]
struct WgcWindowFrameCapture {
    bitmap: CapturedBgraBitmap,
    attempts: u32,
    retry_count: u32,
    elapsed_ms: u64,
    retry_backoff_ms: u64,
}

enum WgcWindowFrameAttemptError {
    Timeout { detail: String },
    Other(CaptureError),
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn graphics_capture_window_frame<F>(
    hwnd: i64,
    timeout_ms: u64,
    select_region: F,
) -> Result<WgcWindowFrameCapture, CaptureError>
where
    F: Fn(&CapturedFrame) -> Rect,
{
    let started = Instant::now();
    for attempt in 1..=WGC_WINDOW_FRAME_MAX_ATTEMPTS {
        match graphics_capture_window_frame_once(hwnd, timeout_ms, &select_region) {
            Ok(bitmap) => {
                let retry_count = attempt.saturating_sub(1);
                if retry_count > 0 {
                    tracing::info!(
                        code = "CAPTURE_WGC_WINDOW_FRAME_RETRY_SUCCEEDED",
                        hwnd,
                        attempts = attempt,
                        retry_count,
                        elapsed_ms = elapsed_ms(started),
                        "WGC window frame arrived after bounded retry"
                    );
                }
                return Ok(WgcWindowFrameCapture {
                    bitmap,
                    attempts: attempt,
                    retry_count,
                    elapsed_ms: elapsed_ms(started),
                    retry_backoff_ms: WGC_WINDOW_FRAME_RETRY_BACKOFF_MS,
                });
            }
            Err(WgcWindowFrameAttemptError::Timeout { detail })
                if attempt < WGC_WINDOW_FRAME_MAX_ATTEMPTS =>
            {
                tracing::warn!(
                    code = "CAPTURE_WGC_WINDOW_FRAME_TIMEOUT_RETRY",
                    hwnd,
                    attempt,
                    max_attempts = WGC_WINDOW_FRAME_MAX_ATTEMPTS,
                    timeout_ms,
                    retry_backoff_ms = WGC_WINDOW_FRAME_RETRY_BACKOFF_MS,
                    elapsed_ms = elapsed_ms(started),
                    detail = %detail,
                    "WGC window frame timed out; retrying after bounded backoff"
                );
                thread::sleep(Duration::from_millis(WGC_WINDOW_FRAME_RETRY_BACKOFF_MS));
            }
            Err(WgcWindowFrameAttemptError::Timeout { detail }) => {
                let total_elapsed_ms = elapsed_ms(started);
                tracing::error!(
                    code = "CAPTURE_WGC_WINDOW_FRAME_TIMEOUT_EXHAUSTED",
                    hwnd,
                    attempts = attempt,
                    timeout_ms,
                    retry_backoff_ms = WGC_WINDOW_FRAME_RETRY_BACKOFF_MS,
                    elapsed_ms = total_elapsed_ms,
                    detail = %detail,
                    "WGC window frame did not arrive after bounded retries"
                );
                return Err(CaptureError::ThreadFailed {
                    detail: format!(
                        "timed out after {timeout_ms} ms waiting for WGC window frame after {attempt} attempts over {total_elapsed_ms} ms for hwnd {hwnd:#x}; retry_backoff_ms={WGC_WINDOW_FRAME_RETRY_BACKOFF_MS}; last_timeout={detail}; recommended_next_action=retry capture after confirming the target window is live, visible, and visually stable"
                    ),
                });
            }
            Err(WgcWindowFrameAttemptError::Other(error)) => return Err(error),
        }
    }
    Err(CaptureError::ThreadFailed {
        detail: format!(
            "WGC window frame retry loop exhausted unexpectedly for hwnd {hwnd:#x}; max_attempts={WGC_WINDOW_FRAME_MAX_ATTEMPTS}"
        ),
    })
}

fn graphics_capture_window_frame_once<F>(
    hwnd: i64,
    timeout_ms: u64,
    select_region: &F,
) -> Result<CapturedBgraBitmap, WgcWindowFrameAttemptError>
where
    F: Fn(&CapturedFrame) -> Rect,
{
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let handle = spawn_capture_loop(CaptureConfig {
        target: CaptureTarget::Window { hwnd },
        min_update_interval_ms: 16,
        cursor_visible: false,
        secondary_windows: false,
        dirty_region_only: false,
        backend_preference: CaptureBackendPreference::GraphicsCaptureApi,
    })
    .map_err(WgcWindowFrameAttemptError::Other)?;
    let receiver = handle.receiver();
    let frame = match receiver.recv_timeout(timeout) {
        Ok(frame) => frame,
        Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
            let stop_result = handle.stop();
            return match stop_result {
                Ok(()) => Err(WgcWindowFrameAttemptError::Timeout {
                    detail: format!("timed out after {timeout_ms} ms waiting for WGC window frame"),
                }),
                Err(stop_error) => Err(WgcWindowFrameAttemptError::Other(stop_error)),
            };
        }
        Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
            let stop_result = handle.stop();
            return Err(WgcWindowFrameAttemptError::Other(match stop_result {
                Ok(()) => CaptureError::ThreadFailed {
                    detail: format!(
                        "WGC window frame channel disconnected before a frame arrived for hwnd {hwnd:#x}"
                    ),
                },
                Err(stop_error) => stop_error,
            }));
        }
    };
    let region = select_region(&frame);
    let result = captured_frame_region_to_bgra_bitmap(&frame, region);
    let stop_result = handle.stop();
    match stop_result {
        Ok(()) => result.map_err(WgcWindowFrameAttemptError::Other),
        Err(error) => Err(WgcWindowFrameAttemptError::Other(error)),
    }
}

fn graphics_capture_window_region_to_bgra_bitmap(
    hwnd: i64,
    region: Rect,
    timeout_ms: u64,
) -> Result<WgcWindowFrameCapture, CaptureError> {
    graphics_capture_window_frame(hwnd, timeout_ms, |_frame| region)
}

fn graphics_capture_window_full_frame_to_bgra_bitmap(
    hwnd: i64,
    timeout_ms: u64,
) -> Result<WgcWindowFrameCapture, CaptureError> {
    graphics_capture_window_frame(hwnd, timeout_ms, |frame| Rect {
        x: 0,
        y: 0,
        w: i32::try_from(frame.width).unwrap_or(i32::MAX),
        h: i32::try_from(frame.height).unwrap_or(i32::MAX),
    })
}

fn minimized_client_offset_in_window_bitmap(
    hwnd: windows::Win32::Foundation::HWND,
    full_width: i32,
    full_height: i32,
) -> Result<Option<(i32, i32)>, CaptureError> {
    let mut client_rect = windows::Win32::Foundation::RECT::default();
    unsafe { GetClientRect(hwnd, &raw mut client_rect) }.map_err(capture_unsupported)?;
    let client_width = client_rect.right.saturating_sub(client_rect.left);
    let client_height = client_rect.bottom.saturating_sub(client_rect.top);
    if client_width <= 0 || client_height <= 0 {
        return minimized_non_client_offset_from_style(hwnd);
    }

    let non_client_x = full_width.saturating_sub(client_width).max(0);
    let non_client_y = full_height.saturating_sub(client_height).max(0);
    let offset_x = non_client_x / 2;
    let offset_y = non_client_y.saturating_sub(offset_x).max(0);
    Ok(Some((offset_x, offset_y)))
}

fn minimized_non_client_offset_from_style(
    hwnd: windows::Win32::Foundation::HWND,
) -> Result<Option<(i32, i32)>, CaptureError> {
    let style_bits = unsafe { GetWindowLongW(hwnd, GWL_STYLE) };
    let ex_style_bits = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) };
    let menu = unsafe { GetMenu(hwnd) };
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let mut client_rect = windows::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 100,
        bottom: 100,
    };
    unsafe {
        AdjustWindowRectExForDpi(
            &raw mut client_rect,
            WINDOW_STYLE(style_bits.cast_unsigned()),
            !menu.is_invalid(),
            WINDOW_EX_STYLE(ex_style_bits.cast_unsigned()),
            dpi,
        )
    }
    .map_err(capture_unsupported)?;
    Ok(Some((
        client_rect.left.saturating_neg().max(0),
        client_rect.top.saturating_neg().max(0),
    )))
}

fn window_capture_extent(
    hwnd: windows::Win32::Foundation::HWND,
) -> Result<(i32, i32), CaptureError> {
    if !unsafe { IsIconic(hwnd) }.as_bool() {
        return Ok(rect_extent(&dwm_extended_frame_bounds(hwnd)?));
    }

    let mut window_rect = windows::Win32::Foundation::RECT::default();
    unsafe { GetWindowRect(hwnd, &raw mut window_rect) }.map_err(capture_unsupported)?;
    let window_width = window_rect.right.saturating_sub(window_rect.left);
    let window_height = window_rect.bottom.saturating_sub(window_rect.top);

    let mut placement = WINDOWPLACEMENT {
        length: u32::try_from(size_of::<WINDOWPLACEMENT>()).unwrap_or(u32::MAX),
        ..Default::default()
    };
    unsafe { GetWindowPlacement(hwnd, &raw mut placement) }.map_err(capture_unsupported)?;
    let normal_width = placement
        .rcNormalPosition
        .right
        .saturating_sub(placement.rcNormalPosition.left);
    let normal_height = placement
        .rcNormalPosition
        .bottom
        .saturating_sub(placement.rcNormalPosition.top);
    if normal_width > 0 && normal_height > 0 {
        return Ok((normal_width, normal_height));
    }
    Ok((window_width, window_height))
}

fn dwm_extended_frame_bounds(
    hwnd: windows::Win32::Foundation::HWND,
) -> Result<windows::Win32::Foundation::RECT, CaptureError> {
    let mut frame_rect = windows::Win32::Foundation::RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut frame_rect).cast::<c_void>(),
            u32::try_from(size_of::<windows::Win32::Foundation::RECT>()).unwrap_or(u32::MAX),
        )
    }
    .map_err(capture_unsupported)?;
    let (width, height) = rect_extent(&frame_rect);
    if width <= 0 || height <= 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!("DWM extended frame bounds are empty: {frame_rect:?}"),
        });
    }
    Ok(frame_rect)
}

const fn rect_extent(rect: &windows::Win32::Foundation::RECT) -> (i32, i32) {
    (
        rect.right.saturating_sub(rect.left),
        rect.bottom.saturating_sub(rect.top),
    )
}

fn client_region_to_frame_region(
    client_region: Rect,
    client_width: i32,
    client_height: i32,
    offset_x: i32,
    offset_y: i32,
    frame_width: i32,
    frame_height: i32,
) -> Result<Rect, CaptureError> {
    if client_width <= 0 || client_height <= 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!(
                "target window has no client area ({client_width}x{client_height}) for region conversion"
            ),
        });
    }
    if client_region.x < 0
        || client_region.y < 0
        || client_region.x.saturating_add(client_region.w) > client_width
        || client_region.y.saturating_add(client_region.h) > client_height
    {
        return Err(CaptureError::TargetInvalid {
            detail: format!(
                "client-relative region {client_region:?} is outside the target window client area {client_width}x{client_height}; pass a region within the client bounds, or omit region to OCR/capture the whole window"
            ),
        });
    }
    let frame_region = Rect {
        x: client_region.x.saturating_add(offset_x),
        y: client_region.y.saturating_add(offset_y),
        w: client_region.w,
        h: client_region.h,
    };
    validate_region_inside_window(frame_region, frame_width, frame_height)?;
    Ok(frame_region)
}

fn software_bitmap_from_bgra(
    bytes: &[u8],
    width: i32,
    height: i32,
) -> Result<SoftwareBitmap, CaptureError> {
    let writer = DataWriter::new().map_err(capture_unsupported)?;
    writer.WriteBytes(bytes).map_err(capture_unsupported)?;
    let buffer = writer.DetachBuffer().map_err(capture_unsupported)?;
    SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width,
        height,
        BitmapAlphaMode::Ignore,
    )
    .map_err(capture_unsupported)
}
fn copy_region_bgra(frame: &CapturedFrame, region: Rect) -> Result<Vec<u8>, CaptureError> {
    let convert_rgba_to_bgra = match frame.format {
        DxgiFormat::Bgra8 | DxgiFormat::Bgra8Srgb => false,
        DxgiFormat::Rgba8 | DxgiFormat::Rgba8Srgb => true,
        other => {
            return Err(CaptureError::GraphicsApiUnsupported {
                detail: format!("OCR bitmap copy does not support frame format {other:?}"),
            });
        }
    };
    if frame.pixels.bytes_per_pixel != 4 {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: format!(
                "OCR bitmap copy requires 4-byte pixels, frame has {} bytes per pixel",
                frame.pixels.bytes_per_pixel
            ),
        });
    }
    validate_region_inside_texture(region, frame.width, frame.height)?;
    copy_owned_frame_region_bgra(frame, region, convert_rgba_to_bgra)
}

fn copy_owned_frame_region_bgra(
    frame: &CapturedFrame,
    region: Rect,
    convert_rgba_to_bgra: bool,
) -> Result<Vec<u8>, CaptureError> {
    validate_owned_frame_buffer(frame)?;
    let source_x = usize::try_from(region.x).map_err(|err| CaptureError::TargetInvalid {
        detail: format!("invalid source region x {}: {err}", region.x),
    })?;
    let source_y = usize::try_from(region.y).map_err(|err| CaptureError::TargetInvalid {
        detail: format!("invalid source region y {}: {err}", region.y),
    })?;
    let width = usize::try_from(region.w).map_err(|err| CaptureError::TargetInvalid {
        detail: format!("invalid source region width {}: {err}", region.w),
    })?;
    let height = usize::try_from(region.h).map_err(|err| CaptureError::TargetInvalid {
        detail: format!("invalid source region height {}: {err}", region.h),
    })?;
    let bytes_per_pixel = usize::from(frame.pixels.bytes_per_pixel);
    let row_len =
        width
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| CaptureError::TargetInvalid {
                detail: format!("invalid OCR bitmap width {}", region.w),
            })?;
    let byte_len = row_len
        .checked_mul(height)
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid OCR bitmap dimensions {}x{}", region.w, region.h),
        })?;
    let mut output = Vec::with_capacity(byte_len);
    for row in 0..height {
        let source_row = source_y
            .checked_add(row)
            .ok_or_else(|| CaptureError::TargetInvalid {
                detail: format!("invalid OCR bitmap source row y={} row={row}", region.y),
            })?;
        let row_offset = source_row
            .checked_mul(frame.pixels.row_stride_bytes)
            .ok_or_else(|| CaptureError::TargetInvalid {
                detail: format!(
                    "invalid OCR bitmap row offset row={source_row} stride={}",
                    frame.pixels.row_stride_bytes
                ),
            })?;
        let col_offset =
            source_x
                .checked_mul(bytes_per_pixel)
                .ok_or_else(|| CaptureError::TargetInvalid {
                    detail: format!("invalid OCR bitmap source column x={}", region.x),
                })?;
        let start =
            row_offset
                .checked_add(col_offset)
                .ok_or_else(|| CaptureError::TargetInvalid {
                    detail: format!(
                        "invalid OCR bitmap source offset row={source_row} x={source_x}"
                    ),
                })?;
        let end = start
            .checked_add(row_len)
            .ok_or_else(|| CaptureError::TargetInvalid {
                detail: format!("invalid OCR bitmap source range start={start} len={row_len}"),
            })?;
        let source =
            frame
                .pixels
                .bytes
                .get(start..end)
                .ok_or_else(|| CaptureError::GraphicsApiUnsupported {
                    detail: format!(
                        "owned capture frame buffer too short for region {region:?}: need byte range {start}..{end}, have {}",
                        frame.pixels.bytes.len()
                    ),
                })?;
        output.extend_from_slice(source);
    }
    if convert_rgba_to_bgra {
        for pixel in output.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    Ok(output)
}

fn validate_owned_frame_buffer(frame: &CapturedFrame) -> Result<(), CaptureError> {
    let bytes_per_pixel = usize::from(frame.pixels.bytes_per_pixel);
    if bytes_per_pixel == 0 {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: "owned capture frame has zero bytes per pixel".to_owned(),
        });
    }
    let min_row_len = usize::try_from(frame.width)
        .ok()
        .and_then(|value| value.checked_mul(bytes_per_pixel))
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid capture frame width {}", frame.width),
        })?;
    if frame.pixels.row_stride_bytes < min_row_len {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: format!(
                "owned capture frame row stride {} is smaller than row length {min_row_len}",
                frame.pixels.row_stride_bytes
            ),
        });
    }
    let min_len = frame
        .pixels
        .row_stride_bytes
        .checked_mul(
            usize::try_from(frame.height).map_err(|err| CaptureError::TargetInvalid {
                detail: err.to_string(),
            })?,
        )
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!(
                "invalid capture frame dimensions {}x{}",
                frame.width, frame.height
            ),
        })?;
    if frame.pixels.bytes.len() < min_len {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: format!(
                "owned capture frame buffer too short: need {min_len} bytes, have {}",
                frame.pixels.bytes.len()
            ),
        });
    }
    Ok(())
}
fn validate_region_inside_texture(
    region: Rect,
    texture_width: u32,
    texture_height: u32,
) -> Result<(), CaptureError> {
    validate_bitmap_region(region)?;
    if region.x < 0 || region.y < 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!("source region {region:?} has negative coordinates"),
        });
    }
    let right = u32::try_from(region.x.saturating_add(region.w)).map_err(|err| {
        CaptureError::TargetInvalid {
            detail: format!("invalid source region right edge for {region:?}: {err}"),
        }
    })?;
    let bottom = u32::try_from(region.y.saturating_add(region.h)).map_err(|err| {
        CaptureError::TargetInvalid {
            detail: format!("invalid source region bottom edge for {region:?}: {err}"),
        }
    })?;
    if right > texture_width || bottom > texture_height {
        return Err(CaptureError::TargetInvalid {
            detail: format!(
                "source region {region:?} exceeds captured frame bounds {texture_width}x{texture_height}"
            ),
        });
    }
    Ok(())
}

fn copy_screen_region_bgra(region: Rect) -> Result<Vec<u8>, CaptureError> {
    let width = u32::try_from(region.w).map_err(|err| CaptureError::TargetInvalid {
        detail: err.to_string(),
    })?;
    let height = u32::try_from(region.h).map_err(|err| CaptureError::TargetInvalid {
        detail: err.to_string(),
    })?;
    let byte_len = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid screen capture region {region:?}"),
        })?;
    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.is_invalid() {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: "GetDC returned null".to_owned(),
        });
    }
    let memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    if memory_dc.is_invalid() {
        let _ = unsafe { ReleaseDC(None, screen_dc) };
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: "CreateCompatibleDC returned null".to_owned(),
        });
    }
    let result = SCREEN_CAPTURE_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let needs_recreate = scratch
            .as_ref()
            .is_none_or(|scratch| !scratch.matches(width, height, byte_len));
        if needs_recreate {
            *scratch = Some(GdiCaptureScratch::new(
                screen_dc, memory_dc, width, height, byte_len,
            )?);
        } else {
            let _ = unsafe { DeleteDC(memory_dc) };
        }
        let scratch = scratch
            .as_ref()
            .ok_or_else(|| CaptureError::GraphicsApiUnsupported {
                detail: "screen capture scratch buffer was not initialized".to_owned(),
            })?;
        let bitblt = unsafe {
            BitBlt(
                scratch.memory_dc,
                0,
                0,
                region.w,
                region.h,
                Some(screen_dc),
                region.x,
                region.y,
                SRCCOPY,
            )
        };
        bitblt.map_err(capture_unsupported)?;
        Ok(unsafe { slice::from_raw_parts(scratch.bits.cast::<u8>(), byte_len) }.to_vec())
    });
    let _ = unsafe { ReleaseDC(None, screen_dc) };
    result
}

fn validate_region_inside_window(
    region: Rect,
    window_width: i32,
    window_height: i32,
) -> Result<(), CaptureError> {
    validate_bitmap_region(region)?;
    if region.x < 0
        || region.y < 0
        || region.x.saturating_add(region.w) > window_width
        || region.y.saturating_add(region.h) > window_height
    {
        return Err(CaptureError::TargetInvalid {
            detail: format!(
                "window capture region {region:?} is outside window bitmap bounds {window_width}x{window_height}"
            ),
        });
    }
    Ok(())
}

fn printwindow_region_bgra(
    hwnd: windows::Win32::Foundation::HWND,
    hwnd_value: i64,
    region: Rect,
    window_width: i32,
    window_height: i32,
) -> Result<Vec<u8>, CaptureError> {
    let full_width = u32::try_from(window_width).map_err(|err| CaptureError::TargetInvalid {
        detail: format!("invalid PrintWindow bitmap width {window_width}: {err}"),
    })?;
    let full_height = u32::try_from(window_height).map_err(|err| CaptureError::TargetInvalid {
        detail: format!("invalid PrintWindow bitmap height {window_height}: {err}"),
    })?;
    let full_byte_len = usize::try_from(full_width)
        .ok()
        .and_then(|w| {
            usize::try_from(full_height)
                .ok()
                .and_then(|h| w.checked_mul(h))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid PrintWindow bitmap dimensions {window_width}x{window_height}"),
        })?;
    let window_dc = unsafe { GetDC(Some(hwnd)) };
    if window_dc.is_invalid() {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: format!("GetDC returned null for hwnd {hwnd_value:#x}"),
        });
    }
    let memory_dc = unsafe { CreateCompatibleDC(Some(window_dc)) };
    if memory_dc.is_invalid() {
        let _ = unsafe { ReleaseDC(Some(hwnd), window_dc) };
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: format!("CreateCompatibleDC returned null for hwnd {hwnd_value:#x}"),
        });
    }
    let scratch = match GdiCaptureScratch::new(
        window_dc,
        memory_dc,
        full_width,
        full_height,
        full_byte_len,
    ) {
        Ok(scratch) => scratch,
        Err(error) => {
            let _ = unsafe { ReleaseDC(Some(hwnd), window_dc) };
            return Err(error);
        }
    };
    let repaint_flags = RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW;
    let repainted = unsafe { RedrawWindow(Some(hwnd), None, None, repaint_flags) };
    if !repainted.as_bool() {
        tracing::debug!(
            hwnd = hwnd_value,
            region = ?region,
            "RedrawWindow before PrintWindow returned false; continuing with PrintWindow"
        );
    }
    let printed = unsafe {
        PrintWindow(
            hwnd,
            scratch.memory_dc,
            PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT),
        )
    };
    let _ = unsafe { ReleaseDC(Some(hwnd), window_dc) };
    if !printed.as_bool() {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: format!(
                "PrintWindow returned false for hwnd {hwnd_value:#x}; last_error={:?}",
                unsafe { windows::Win32::Foundation::GetLastError() }
            ),
        });
    }
    let full_bytes =
        unsafe { slice::from_raw_parts(scratch.bits.cast::<u8>(), full_byte_len) }.to_vec();
    copy_bgra_region_from_bytes(&full_bytes, full_width, region)
}

fn copy_bgra_region_from_bytes(
    full_bytes: &[u8],
    full_width: u32,
    region: Rect,
) -> Result<Vec<u8>, CaptureError> {
    let width = usize::try_from(region.w).map_err(|err| CaptureError::TargetInvalid {
        detail: err.to_string(),
    })?;
    let height = usize::try_from(region.h).map_err(|err| CaptureError::TargetInvalid {
        detail: err.to_string(),
    })?;
    let full_row_len = usize::try_from(full_width)
        .ok()
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid PrintWindow full width {full_width}"),
        })?;
    let row_len = width
        .checked_mul(4)
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid PrintWindow crop width {}", region.w),
        })?;
    let byte_len = row_len
        .checked_mul(height)
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid PrintWindow crop region {region:?}"),
        })?;
    let source_x = usize::try_from(region.x).map_err(|err| CaptureError::TargetInvalid {
        detail: format!("invalid PrintWindow crop x {}: {err}", region.x),
    })?;
    let source_y = usize::try_from(region.y).map_err(|err| CaptureError::TargetInvalid {
        detail: format!("invalid PrintWindow crop y {}: {err}", region.y),
    })?;
    let source_x_bytes = source_x
        .checked_mul(4)
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid PrintWindow crop x {}", region.x),
        })?;
    let mut output = vec![0_u8; byte_len];
    for row in 0..height {
        let source_offset = source_y
            .checked_add(row)
            .and_then(|source_row| source_row.checked_mul(full_row_len))
            .and_then(|source_row_offset| source_row_offset.checked_add(source_x_bytes))
            .ok_or_else(|| CaptureError::TargetInvalid {
                detail: format!("PrintWindow source offset overflow for {region:?}"),
            })?;
        let source_end =
            source_offset
                .checked_add(row_len)
                .ok_or_else(|| CaptureError::TargetInvalid {
                    detail: format!("PrintWindow source end overflow for {region:?}"),
                })?;
        if source_end > full_bytes.len() {
            return Err(CaptureError::TargetInvalid {
                detail: format!(
                    "PrintWindow source region {region:?} exceeds captured byte length {}",
                    full_bytes.len()
                ),
            });
        }
        let target_offset = row.saturating_mul(row_len);
        output[target_offset..target_offset + row_len]
            .copy_from_slice(&full_bytes[source_offset..source_end]);
    }
    Ok(output)
}

fn is_all_zero_bgra(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == 0)
}

struct GdiCaptureScratch {
    width: u32,
    height: u32,
    byte_len: usize,
    memory_dc: HDC,
    bitmap: HBITMAP,
    old_object: HGDIOBJ,
    bits: *mut c_void,
}

impl GdiCaptureScratch {
    fn new(
        screen_dc: HDC,
        memory_dc: HDC,
        width: u32,
        height: u32,
        byte_len: usize,
    ) -> Result<Self, CaptureError> {
        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(u32::MAX),
                biWidth: i32::try_from(width).unwrap_or(i32::MAX),
                biHeight: -i32::try_from(height).unwrap_or(i32::MAX),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: u32::try_from(byte_len).unwrap_or(u32::MAX),
                ..BITMAPINFOHEADER::default()
            },
            ..BITMAPINFO::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = unsafe {
            CreateDIBSection(
                Some(screen_dc),
                &raw const bitmap_info,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        }
        .map_err(capture_unsupported)?;
        if bits.is_null() {
            let _ = unsafe { DeleteObject(HGDIOBJ::from(bitmap)) };
            let _ = unsafe { DeleteDC(memory_dc) };
            return Err(CaptureError::GraphicsApiUnsupported {
                detail: "CreateDIBSection returned no bitmap bits".to_owned(),
            });
        }
        let old_object = unsafe { SelectObject(memory_dc, HGDIOBJ::from(bitmap)) };
        if old_object.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ::from(bitmap)) };
            let _ = unsafe { DeleteDC(memory_dc) };
            return Err(CaptureError::GraphicsApiUnsupported {
                detail: "SelectObject failed for screen capture bitmap".to_owned(),
            });
        }
        Ok(Self {
            width,
            height,
            byte_len,
            memory_dc,
            bitmap,
            old_object,
            bits,
        })
    }

    const fn matches(&self, width: u32, height: u32, byte_len: usize) -> bool {
        self.width == width && self.height == height && self.byte_len == byte_len
    }
}

impl Drop for GdiCaptureScratch {
    fn drop(&mut self) {
        let _ = unsafe { SelectObject(self.memory_dc, self.old_object) };
        let _ = unsafe { DeleteObject(HGDIOBJ::from(self.bitmap)) };
        let _ = unsafe { DeleteDC(self.memory_dc) };
    }
}

fn validate_bitmap_region(region: Rect) -> Result<(), CaptureError> {
    if region.w <= 0 || region.h <= 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!("empty bitmap capture region {region:?}"),
        });
    }
    Ok(())
}

fn clamp_region_to_frame(frame: &CapturedFrame, region: Rect) -> Result<Rect, CaptureError> {
    if region.w <= 0 || region.h <= 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!("empty OCR capture region {region:?}"),
        });
    }
    let frame_w = i64::from(frame.width);
    let frame_h = i64::from(frame.height);
    let left = i64::from(region.x).clamp(0, frame_w);
    let top = i64::from(region.y).clamp(0, frame_h);
    let right = i64::from(region.x)
        .saturating_add(i64::from(region.w))
        .clamp(0, frame_w);
    let bottom = i64::from(region.y)
        .saturating_add(i64::from(region.h))
        .clamp(0, frame_h);
    if right <= left || bottom <= top {
        return Err(CaptureError::TargetInvalid {
            detail: format!("OCR capture region {region:?} is outside frame bounds"),
        });
    }
    Ok(Rect {
        x: i32::try_from(left).unwrap_or(i32::MAX),
        y: i32::try_from(top).unwrap_or(i32::MAX),
        w: i32::try_from(right - left).unwrap_or(i32::MAX),
        h: i32::try_from(bottom - top).unwrap_or(i32::MAX),
    })
}

use std::{ffi::c_void, mem::size_of, slice, time::Instant};

use synapse_core::Rect;
use windows::{
    Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
    Storage::Streams::DataWriter,
    Win32::Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
            CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ,
            RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW, ROP_CODE, RedrawWindow, ReleaseDC,
            SRCCOPY, SelectObject,
        },
    },
    Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow},
    Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetClientRect, GetCursorInfo,
        GetIconInfo, HICON, IsIconic, PW_RENDERFULLCONTENT,
    },
};

use crate::{
    CaptureError, CapturedBgraBitmap, CapturedFrame, CapturedSoftwareBitmap,
    CapturedWindowBgraBitmap, DxgiFormat, MAX_CAPTURE_BYTES,
};

use super::common::{capture_unsupported, hwnd_from_i64};
use super::target::{validate_region_on_virtual_screen, visible_window_screen_region};

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
    validate_screen_region(region)?;
    let bytes = copy_screen_region_bgra(region, false)?;
    let bitmap = software_bitmap_from_bgra(&bytes, region.w, region.h)?;
    Ok(CapturedSoftwareBitmap { region, bitmap })
}

pub fn screen_region_to_bgra_bitmap(region: Rect) -> Result<CapturedBgraBitmap, CaptureError> {
    screen_region_to_bgra_bitmap_with_cursor(region, false)
}

pub(super) fn screen_region_to_bgra_bitmap_with_cursor(
    region: Rect,
    cursor_visible: bool,
) -> Result<CapturedBgraBitmap, CaptureError> {
    validate_screen_region(region)?;
    let bytes = copy_screen_region_bgra(region, cursor_visible)?;
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
    _timeout_ms: u64,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    validate_bitmap_region(region)?;
    let started = Instant::now();
    let window_screen_region = visible_window_screen_region(hwnd)?;
    validate_region_inside_window(region, window_screen_region.w, window_screen_region.h)?;
    let screen_region = Rect {
        x: window_screen_region.x.saturating_add(region.x),
        y: window_screen_region.y.saturating_add(region.y),
        w: region.w,
        h: region.h,
    };
    let mut bitmap = screen_region_to_bgra_bitmap_with_cursor(screen_region, false)?;
    bitmap.region = region;
    Ok(CapturedWindowBgraBitmap {
        bitmap,
        capture_backend: "gdi_bitblt_visible_window_bgra",
        capture_attempts: 1,
        capture_retry_count: 0,
        capture_elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        capture_retry_backoff_ms: 0,
    })
}

/// Captures the entire physically visible window rectangle through CPU/GDI.
///
/// The dimensions come from DWM extended-frame bounds, while pixels come from
/// the composited virtual desktop. Occluding windows are therefore present in
/// the returned physical-reality snapshot. Minimized, hidden, cloaked, or
/// partly off-screen windows fail closed.
///
/// # Errors
///
/// Returns [`CaptureError`] when the HWND is invalid or CPU/GDI cannot truthfully
/// observe its complete visible rectangle. There is no GPU or `PrintWindow`
/// fallback.
pub fn window_full_frame_to_bgra_bitmap(
    hwnd: i64,
    _timeout_ms: u64,
) -> Result<CapturedWindowBgraBitmap, CaptureError> {
    let full = visible_window_screen_region(hwnd)?;
    window_region_to_bgra_bitmap(
        hwnd,
        Rect {
            x: 0,
            y: 0,
            w: full.w,
            h: full.h,
        },
        0,
    )
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
    let screen_region = visible_window_screen_region(hwnd)?;
    let (w, h) = (screen_region.w, screen_region.h);
    let region = Rect { x: 0, y: 0, w, h };
    validate_bitmap_region(region)?;
    Ok(region)
}

pub fn window_printwindow_capture_region(hwnd: i64) -> Result<Rect, CaptureError> {
    let hwnd = hwnd_from_i64(hwnd)?;
    let (w, h) = window_capture_extent(hwnd)?;
    let region = Rect { x: 0, y: 0, w, h };
    validate_bitmap_region(region)?;
    Ok(region)
}

pub fn client_region_to_window_region(hwnd: i64, region: Rect) -> Result<Rect, CaptureError> {
    validate_bitmap_region(region)?;
    visible_window_screen_region(hwnd)?;
    let hwnd = hwnd_from_i64(hwnd)?;

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

fn window_capture_extent(
    hwnd: windows::Win32::Foundation::HWND,
) -> Result<(i32, i32), CaptureError> {
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(CaptureError::UnsupportedSemantics {
            detail: "minimized windows cannot be captured under the CPU/GDI visible-surface policy"
                .to_owned(),
        });
    }
    Ok(rect_extent(&dwm_extended_frame_bounds(hwnd)?))
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

fn copy_screen_region_bgra(region: Rect, cursor_visible: bool) -> Result<Vec<u8>, CaptureError> {
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
    validate_capture_byte_len(byte_len, region, "screen")?;
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
    let result = (|| {
        let scratch = GdiCaptureScratch::new(screen_dc, memory_dc, width, height, byte_len)?;
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
                ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
            )
        };
        bitblt.map_err(capture_unsupported)?;
        if cursor_visible {
            draw_cursor_on_capture(scratch.memory_dc, region)?;
        }
        Ok(unsafe { slice::from_raw_parts(scratch.bits.cast::<u8>(), byte_len) }.to_vec())
    })();
    let _ = unsafe { ReleaseDC(None, screen_dc) };
    result
}

fn draw_cursor_on_capture(memory_dc: HDC, region: Rect) -> Result<(), CaptureError> {
    let mut cursor = CURSORINFO {
        cbSize: u32::try_from(size_of::<CURSORINFO>()).unwrap_or(u32::MAX),
        ..CURSORINFO::default()
    };
    unsafe { GetCursorInfo(&raw mut cursor) }.map_err(capture_unsupported)?;
    if cursor.flags.0 & CURSOR_SHOWING.0 == 0 || cursor.hCursor.is_invalid() {
        return Ok(());
    }

    let icon = HICON(cursor.hCursor.0);
    let mut icon_info = windows::Win32::UI::WindowsAndMessaging::ICONINFO::default();
    unsafe { GetIconInfo(icon, &raw mut icon_info) }.map_err(capture_unsupported)?;
    let hotspot_x = i32::try_from(icon_info.xHotspot).unwrap_or(i32::MAX);
    let hotspot_y = i32::try_from(icon_info.yHotspot).unwrap_or(i32::MAX);
    let draw_x = cursor
        .ptScreenPos
        .x
        .saturating_sub(hotspot_x)
        .saturating_sub(region.x);
    let draw_y = cursor
        .ptScreenPos
        .y
        .saturating_sub(hotspot_y)
        .saturating_sub(region.y);
    let draw_result =
        unsafe { DrawIconEx(memory_dc, draw_x, draw_y, icon, 0, 0, 0, None, DI_NORMAL) }
            .map_err(capture_unsupported);
    if !icon_info.hbmMask.is_invalid() {
        let _ = unsafe { DeleteObject(HGDIOBJ::from(icon_info.hbmMask)) };
    }
    if !icon_info.hbmColor.is_invalid() {
        let _ = unsafe { DeleteObject(HGDIOBJ::from(icon_info.hbmColor)) };
    }
    draw_result
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
    validate_capture_byte_len(
        full_byte_len,
        Rect {
            x: 0,
            y: 0,
            w: window_width,
            h: window_height,
        },
        "PrintWindow",
    )?;
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
        let bitmap = match unsafe {
            CreateDIBSection(
                Some(screen_dc),
                &raw const bitmap_info,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        } {
            Ok(bitmap) => bitmap,
            Err(error) => {
                if !unsafe { DeleteDC(memory_dc) }.as_bool() {
                    tracing::error!(
                        code = "CAPTURE_GDI_CLEANUP_FAILED",
                        resource = "memory_dc",
                        original_error = %error,
                        cleanup_last_error = ?unsafe { windows::Win32::Foundation::GetLastError() },
                        "CreateDIBSection failed and its already-created memory DC could not be released"
                    );
                }
                return Err(capture_unsupported(error));
            }
        };
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
            memory_dc,
            bitmap,
            old_object,
            bits,
        })
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

fn validate_screen_region(region: Rect) -> Result<(), CaptureError> {
    validate_bitmap_region(region)?;
    validate_region_on_virtual_screen(region)?;
    let byte_len = usize::try_from(region.w)
        .ok()
        .and_then(|width| {
            usize::try_from(region.h)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("capture byte length overflow for region {region:?}"),
        })?;
    validate_capture_byte_len(byte_len, region, "screen")
}

fn validate_capture_byte_len(
    byte_len: usize,
    region: Rect,
    source: &str,
) -> Result<(), CaptureError> {
    if byte_len > MAX_CAPTURE_BYTES {
        return Err(CaptureError::UnsupportedSemantics {
            detail: format!(
                "{source} capture region {region:?} requires {byte_len} BGRA bytes, exceeding the bounded host-memory envelope of {MAX_CAPTURE_BYTES} bytes; request a smaller region"
            ),
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

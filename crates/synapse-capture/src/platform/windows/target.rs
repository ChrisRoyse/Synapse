use std::{ffi::c_void, mem::size_of};

use synapse_core::Rect;
use windows::Win32::{
    Foundation::{LPARAM, POINT, RECT},
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY,
            MONITORINFO, MONITORINFOEXW, MonitorFromPoint,
        },
    },
    UI::WindowsAndMessaging::{
        GetSystemMetrics, IsIconic, IsWindow, IsWindowVisible, MONITORINFOF_PRIMARY,
        SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    },
};
use windows::core::BOOL;

use crate::{CaptureError, CaptureTarget};

use super::common::{capture_unsupported, hwnd_from_i64};

pub fn validate_hwnd(hwnd: i64) -> Result<(), CaptureError> {
    visible_window_screen_region(hwnd).map(|_region| ())
}

pub fn validate_monitor(monitor_index: u32) -> Result<(), CaptureError> {
    monitor_screen_region(monitor_index).map(|_region| ())
}

pub fn capture_target_screen_region(target: &CaptureTarget) -> Result<Rect, CaptureError> {
    match target {
        CaptureTarget::Primary => primary_monitor_screen_region(),
        CaptureTarget::Monitor { monitor_index } => monitor_screen_region(*monitor_index),
        CaptureTarget::Window { hwnd } => visible_window_screen_region(*hwnd),
    }
}

pub fn visible_window_screen_region(hwnd_value: i64) -> Result<Rect, CaptureError> {
    let hwnd = hwnd_from_i64(hwnd_value)?;
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err(CaptureError::TargetInvalid {
            detail: format!("HWND {hwnd_value:#x} is not a live window"),
        });
    }
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err(CaptureError::UnsupportedSemantics {
            detail: format!(
                "HWND {hwnd_value:#x} is hidden; CPU/GDI capture can only read pixels physically visible on the virtual desktop"
            ),
        });
    }
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(CaptureError::UnsupportedSemantics {
            detail: format!(
                "HWND {hwnd_value:#x} is minimized; CPU/GDI capture cannot truthfully reconstruct a non-visible window surface"
            ),
        });
    }

    let mut cloaked = 0_u32;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast::<c_void>(),
            u32::try_from(size_of::<u32>()).unwrap_or(u32::MAX),
        )
    }
    .map_err(capture_unsupported)?;
    if cloaked != 0 {
        return Err(CaptureError::UnsupportedSemantics {
            detail: format!(
                "HWND {hwnd_value:#x} is DWM-cloaked; CPU/GDI capture can only read pixels physically visible on the virtual desktop"
            ),
        });
    }

    let mut frame = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut frame).cast::<c_void>(),
            u32::try_from(size_of::<RECT>()).unwrap_or(u32::MAX),
        )
    }
    .map_err(capture_unsupported)?;
    let region = rect_from_win32(frame, &format!("HWND {hwnd_value:#x}"))?;
    validate_region_on_virtual_screen(region)?;
    Ok(region)
}

pub fn validate_region_on_virtual_screen(region: Rect) -> Result<(), CaptureError> {
    if region.w <= 0 || region.h <= 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!("capture region is empty: {region:?}"),
        });
    }
    let virtual_screen = virtual_screen_region()?;
    let region_right = i64::from(region.x).saturating_add(i64::from(region.w));
    let region_bottom = i64::from(region.y).saturating_add(i64::from(region.h));
    let virtual_right = i64::from(virtual_screen.x).saturating_add(i64::from(virtual_screen.w));
    let virtual_bottom = i64::from(virtual_screen.y).saturating_add(i64::from(virtual_screen.h));
    if region.x < virtual_screen.x
        || region.y < virtual_screen.y
        || region_right > virtual_right
        || region_bottom > virtual_bottom
    {
        return Err(CaptureError::UnsupportedSemantics {
            detail: format!(
                "capture region {region:?} is partially or wholly outside physical virtual-screen bounds {virtual_screen:?}; CPU/GDI capture refuses unverifiable off-screen pixels"
            ),
        });
    }
    Ok(())
}

fn primary_monitor_screen_region() -> Result<Rect, CaptureError> {
    let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    if monitor.is_invalid() {
        return Err(CaptureError::TargetInvalid {
            detail: "Windows returned no primary monitor".to_owned(),
        });
    }
    monitor_rect(monitor)
}

fn monitor_screen_region(monitor_index: u32) -> Result<Rect, CaptureError> {
    let monitors = enumerate_monitors()?;
    let index = usize::try_from(monitor_index).map_err(|error| CaptureError::TargetInvalid {
        detail: format!("monitor index {monitor_index} cannot be represented: {error}"),
    })?;
    let monitor = monitors
        .get(index)
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!(
                "monitor index {monitor_index} is out of range; active monitor count is {}",
                monitors.len()
            ),
        })?;
    Ok(monitor.region)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MonitorDescriptor {
    region: Rect,
    primary: bool,
    device_name: String,
}

fn enumerate_monitors() -> Result<Vec<MonitorDescriptor>, CaptureError> {
    unsafe extern "system" fn collect_monitor(
        monitor: HMONITOR,
        _dc: HDC,
        _region: *mut RECT,
        state: LPARAM,
    ) -> BOOL {
        // SAFETY: `state` points to the stack-owned Vec for the duration of the
        // synchronous EnumDisplayMonitors call below.
        let monitors = unsafe { &mut *(state.0 as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
        BOOL(1)
    }

    let mut handles = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM((&raw mut handles).cast::<c_void>() as isize),
        )
    }
    .ok()
    .map_err(capture_unsupported)?;
    if handles.is_empty() {
        return Err(CaptureError::TargetInvalid {
            detail: "Windows enumerated zero active monitors".to_owned(),
        });
    }
    let mut monitors = handles
        .into_iter()
        .map(monitor_descriptor)
        .collect::<Result<Vec<_>, _>>()?;
    // EnumDisplayMonitors does not promise callback order. Persisted monitor
    // indices therefore use this frozen stable order: primary first, then
    // physical virtual-screen geometry, then the display-device identity.
    monitors.sort_by(|left, right| {
        right
            .primary
            .cmp(&left.primary)
            .then_with(|| left.region.x.cmp(&right.region.x))
            .then_with(|| left.region.y.cmp(&right.region.y))
            .then_with(|| left.region.w.cmp(&right.region.w))
            .then_with(|| left.region.h.cmp(&right.region.h))
            .then_with(|| left.device_name.cmp(&right.device_name))
    });
    for pair in monitors.windows(2) {
        if pair[0] == pair[1] {
            return Err(CaptureError::TargetInvalid {
                detail: format!(
                    "Windows returned duplicate monitor identity primary={} region={:?} device={:?}; monitor-index selection is ambiguous",
                    pair[0].primary, pair[0].region, pair[0].device_name
                ),
            });
        }
    }
    Ok(monitors)
}

fn monitor_rect(monitor: HMONITOR) -> Result<Rect, CaptureError> {
    monitor_descriptor(monitor).map(|descriptor| descriptor.region)
}

fn monitor_descriptor(monitor: HMONITOR) -> Result<MonitorDescriptor, CaptureError> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(size_of::<MONITORINFOEXW>()).map_err(|error| {
        CaptureError::TargetInvalid {
            detail: format!("MONITORINFOEXW size cannot be represented: {error}"),
        }
    })?;
    if !unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) }.as_bool() {
        return Err(CaptureError::TargetInvalid {
            detail: format!(
                "GetMonitorInfoW failed for monitor; last_error={:?}",
                unsafe { windows::Win32::Foundation::GetLastError() }
            ),
        });
    }
    let device_len = info
        .szDevice
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(info.szDevice.len());
    let device_name = String::from_utf16(&info.szDevice[..device_len]).map_err(|error| {
        CaptureError::TargetInvalid {
            detail: format!("monitor device identity is not valid UTF-16: {error}"),
        }
    })?;
    if device_name.is_empty() {
        return Err(CaptureError::TargetInvalid {
            detail: "Windows returned an empty monitor device identity".to_owned(),
        });
    }
    Ok(MonitorDescriptor {
        region: rect_from_win32(
            info.monitorInfo.rcMonitor,
            &format!("monitor {device_name}"),
        )?,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        device_name,
    })
}

fn virtual_screen_region() -> Result<Rect, CaptureError> {
    let region = Rect {
        x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        w: unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) },
        h: unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
    };
    if region.w <= 0 || region.h <= 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!("Windows virtual-screen bounds are empty: {region:?}"),
        });
    }
    Ok(region)
}

fn rect_from_win32(rect: RECT, source: &str) -> Result<Rect, CaptureError> {
    let region = Rect {
        x: rect.left,
        y: rect.top,
        w: rect.right.saturating_sub(rect.left),
        h: rect.bottom.saturating_sub(rect.top),
    };
    if region.w <= 0 || region.h <= 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!("{source} has empty physical bounds: {region:?}"),
        });
    }
    Ok(region)
}

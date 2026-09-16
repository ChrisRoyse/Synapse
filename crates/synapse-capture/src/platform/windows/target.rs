use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows_capture::monitor::Monitor;

use crate::CaptureError;

use super::common::hwnd_from_i64;

pub fn validate_hwnd(hwnd: i64) -> Result<(), CaptureError> {
    let hwnd = hwnd_from_i64(hwnd)?;
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        Ok(())
    } else {
        Err(CaptureError::TargetInvalid {
            detail: "HWND is not a live window".to_owned(),
        })
    }
}

pub fn validate_monitor(monitor_index: u32) -> Result<(), CaptureError> {
    let windows_capture_index =
        usize::try_from(monitor_index.saturating_add(1)).map_err(|err| {
            CaptureError::TargetInvalid {
                detail: err.to_string(),
            }
        })?;
    Monitor::from_index(windows_capture_index)
        .map(|_monitor| ())
        .map_err(|err| CaptureError::TargetInvalid {
            detail: err.to_string(),
        })
}

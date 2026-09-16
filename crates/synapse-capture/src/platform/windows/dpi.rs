use windows::Win32::{
    Foundation::E_ACCESSDENIED,
    System::Threading::{
        GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    },
    UI::HiDpi::{
        AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
    },
};

use crate::{CaptureError, CaptureThreadPriority, DpiAwarenessStatus};

pub fn init_process_dpi_awareness() -> Result<DpiAwarenessStatus, CaptureError> {
    match unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
        Ok(()) => Ok(DpiAwarenessStatus::Set),
        Err(err) if err.code() == E_ACCESSDENIED => Ok(DpiAwarenessStatus::AlreadySet),
        Err(err) => Err(CaptureError::ThreadFailed {
            detail: err.to_string(),
        }),
    }
}

pub fn is_per_monitor_v2_dpi_aware() -> bool {
    unsafe {
        AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    }
    .as_bool()
}

pub fn current_thread_priority() -> CaptureThreadPriority {
    let priority = unsafe { GetThreadPriority(GetCurrentThread()) };
    if priority == THREAD_PRIORITY_TIME_CRITICAL.0 {
        CaptureThreadPriority::TimeCritical
    } else {
        CaptureThreadPriority::Other(priority)
    }
}

pub fn set_capture_thread_priority() -> Result<(), CaptureError> {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }.map_err(|err| {
        CaptureError::ThreadFailed {
            detail: err.to_string(),
        }
    })
}

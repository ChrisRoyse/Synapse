use std::sync::OnceLock;

const DEFAULT_DOUBLE_CLICK_WINDOW_MS: u32 = 500;
const MIN_INTER_CLICK_DELAY_MS: u32 = 30;
const MAX_INTER_CLICK_DELAY_MS: u32 = 150;

static DOUBLE_CLICK_TIMING: OnceLock<DoubleClickTiming> = OnceLock::new();

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DoubleClickTiming {
    pub window_ms: u32,
    pub inter_click_delay_ms: u32,
    pub source: &'static str,
}

#[must_use]
pub fn initialize_double_click_timing_cache() -> DoubleClickTiming {
    *DOUBLE_CLICK_TIMING.get_or_init(query_double_click_timing)
}

#[must_use]
pub fn cached_double_click_timing() -> DoubleClickTiming {
    initialize_double_click_timing_cache()
}

#[must_use]
pub fn inter_click_delay_ms_for_window(window_ms: u32) -> u32 {
    let window_ms = window_ms.max(2);
    let delay = (window_ms / 4).clamp(MIN_INTER_CLICK_DELAY_MS, MAX_INTER_CLICK_DELAY_MS);
    delay.min(window_ms - 1)
}

#[cfg(windows)]
fn query_double_click_timing() -> DoubleClickTiming {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;

    let queried_window_ms = unsafe {
        // SAFETY: `GetDoubleClickTime` takes no pointers or handles and only
        // reads the current process-visible Windows mouse setting.
        GetDoubleClickTime()
    };
    let window_ms = if queried_window_ms == 0 {
        DEFAULT_DOUBLE_CLICK_WINDOW_MS
    } else {
        queried_window_ms
    };
    DoubleClickTiming {
        window_ms,
        inter_click_delay_ms: inter_click_delay_ms_for_window(window_ms),
        source: "windows_get_double_click_time",
    }
}

#[cfg(not(windows))]
fn query_double_click_timing() -> DoubleClickTiming {
    DoubleClickTiming {
        window_ms: DEFAULT_DOUBLE_CLICK_WINDOW_MS,
        inter_click_delay_ms: inter_click_delay_ms_for_window(DEFAULT_DOUBLE_CLICK_WINDOW_MS),
        source: "default_non_windows",
    }
}

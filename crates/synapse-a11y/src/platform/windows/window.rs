use std::{ffi::c_void, mem, path::Path};

use synapse_core::{
    AccessibleSubtree, ForegroundContext, Point, Rect,
    win32_hwnd::{hwnd_from_wire, hwnd_to_wire, native_hwnds_equal},
};
use uiautomation::{
    UIElement,
    types::{ElementMode, TreeScope},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, RECT, WPARAM},
        System::SystemInformation::GetTickCount,
        System::Threading::{
            AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
        UI::Input::KeyboardAndMouse::{
            GetLastInputInfo, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
            KEYEVENTF_KEYUP, LASTINPUTINFO, SendInput, VIRTUAL_KEY, VK_MENU,
        },
        UI::WindowsAndMessaging::{
            BringWindowToTop, EnumWindows, GA_ROOT, GetAncestor, GetForegroundWindow,
            GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
            IsWindowVisible, PostMessageW, SW_RESTORE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow,
            SwitchToThisWindow, WM_CLOSE,
        },
    },
    core::{BOOL, PWSTR},
};

use crate::{A11yError, A11yResult, ForegroundActivationIntent};

use super::common::{TreeView, cached_hwnd, create_cache_request, map_uia_error, with_automation};
pub fn focused_window() -> A11yResult<UIElement> {
    Err(A11yError::internal(
        "direct UIElement foreground lookup is disabled; use snapshot_focused_window so UIA stays on the dedicated MTA worker",
    ))
}

pub fn snapshot_focused_window(depth: u32) -> A11yResult<AccessibleSubtree> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return Err(A11yError::NoForeground {
            detail: "GetForegroundWindow returned null".to_owned(),
        });
    }
    super::snapshot::snapshot_window_from_hwnd(hwnd_to_wire(hwnd.0 as isize), depth)
}

pub fn current_foreground_context() -> A11yResult<ForegroundContext> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return Err(A11yError::NoForeground {
            detail: "GetForegroundWindow returned null".to_owned(),
        });
    }
    foreground_context(hwnd_to_wire(hwnd.0 as isize))
}

pub fn window_from_hwnd(hwnd: i64) -> A11yResult<UIElement> {
    let _hwnd = hwnd_from_wire_value(hwnd)?;
    Err(A11yError::internal(
        "direct UIElement HWND lookup is disabled; use snapshot_window_from_hwnd so UIA stays on the dedicated MTA worker",
    ))
}

pub fn focus_window_with_intent(hwnd: i64, intent: ForegroundActivationIntent) -> A11yResult<()> {
    let hwnd = hwnd_from_wire_value(hwnd)?;
    tracing::info!(
        code = "FOREGROUND_ACTIVATION_INTENT_ACCEPTED",
        caller = intent.caller(),
        intent = intent.reason(),
        hwnd = hwnd.0 as isize,
        "foreground activation entered explicit-intent guard"
    );
    restore_window_for_focus(hwnd);
    let _ = unsafe { SetForegroundWindow(hwnd) };
    if native_hwnds_equal(unsafe { GetForegroundWindow() }.0 as isize, hwnd.0 as isize) {
        Ok(())
    } else {
        let current_thread = unsafe { GetCurrentThreadId() };
        let foreground = unsafe { GetForegroundWindow() };
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            unsafe { GetWindowThreadProcessId(foreground, None) }
        };
        let target_thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
        if let Err(error) = send_foreground_activation_nudge() {
            tracing::debug!(
                error = %error,
                "foreground activation input nudge failed before SetForegroundWindow retry"
            );
        }
        let attached_foreground = foreground_thread != 0
            && foreground_thread != current_thread
            && unsafe { AttachThreadInput(current_thread, foreground_thread, true) }.as_bool();
        let attached_target = target_thread != 0
            && target_thread != current_thread
            && unsafe { AttachThreadInput(current_thread, target_thread, true) }.as_bool();

        restore_window_for_focus(hwnd);
        let _ = unsafe { BringWindowToTop(hwnd) };
        unsafe { SwitchToThisWindow(hwnd, true) };
        let focused = unsafe { SetForegroundWindow(hwnd) }.as_bool()
            || native_hwnds_equal(unsafe { GetForegroundWindow() }.0 as isize, hwnd.0 as isize);

        if attached_target {
            let _ = unsafe { AttachThreadInput(current_thread, target_thread, false) };
        }
        if attached_foreground {
            let _ = unsafe { AttachThreadInput(current_thread, foreground_thread, false) };
        }

        if focused {
            Ok(())
        } else {
            Err(A11yError::internal(format!(
                "SetForegroundWindow returned false for hwnd 0x{:x}",
                hwnd.0 as isize
            )))
        }
    }
}

fn send_foreground_activation_nudge() -> A11yResult<()> {
    let inputs = [
        virtual_key_input(VK_MENU, KEYBD_EVENT_FLAGS(0)),
        virtual_key_input(VK_MENU, KEYEVENTF_KEYUP),
    ];
    let cb_size = i32::try_from(mem::size_of::<INPUT>())
        .map_err(|_err| A11yError::internal("INPUT struct size does not fit SendInput cbSize"))?;
    // SAFETY: `inputs` contains initialized keyboard INPUT records and
    // `cb_size` is exactly `size_of::<INPUT>()`.
    let sent = unsafe { SendInput(&inputs, cb_size) };
    let expected = u32::try_from(inputs.len())
        .map_err(|_err| A11yError::internal("foreground nudge input count overflow"))?;
    if sent == expected {
        Ok(())
    } else {
        Err(A11yError::internal(format!(
            "SendInput inserted {sent}/{} events for foreground activation nudge",
            inputs.len()
        )))
    }
}

const fn virtual_key_input(vkey: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vkey,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn restore_window_for_focus(hwnd: HWND) {
    let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    if unsafe { IsIconic(hwnd) }.as_bool() {
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    }
}

pub fn is_window_minimized(hwnd: i64) -> A11yResult<bool> {
    let hwnd = valid_hwnd(hwnd)?;
    Ok(unsafe { IsIconic(hwnd) }.as_bool())
}

pub fn is_window_visible(hwnd: i64) -> A11yResult<bool> {
    let hwnd = valid_hwnd(hwnd)?;
    Ok(unsafe { IsWindowVisible(hwnd) }.as_bool())
}

pub fn millis_since_last_input() -> A11yResult<u64> {
    let mut info = LASTINPUTINFO {
        cbSize: u32::try_from(mem::size_of::<LASTINPUTINFO>())
            .map_err(|err| A11yError::internal(err.to_string()))?,
        dwTime: 0,
    };
    if !unsafe { GetLastInputInfo(&raw mut info) }.as_bool() {
        return Err(A11yError::internal(format!(
            "GetLastInputInfo failed: {:?}",
            windows::core::Error::from_thread()
        )));
    }
    // Both values are 32-bit session ticks that wrap every ~49.7 days;
    // wrapping subtraction yields the correct elapsed span across the wrap.
    let now_tick = unsafe { GetTickCount() };
    Ok(u64::from(now_tick.wrapping_sub(info.dwTime)))
}

pub fn is_top_level_window(hwnd: i64) -> A11yResult<bool> {
    let hwnd = valid_hwnd(hwnd)?;
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    Ok(native_hwnds_equal(root.0 as isize, hwnd.0 as isize))
}

pub fn top_level_root_hwnd(hwnd: i64) -> A11yResult<i64> {
    let seed = valid_hwnd(hwnd)?;
    let root = unsafe { GetAncestor(seed, GA_ROOT) };
    let root = if root.0.is_null() { seed } else { root };
    let root = valid_hwnd(hwnd_to_wire(root.0 as isize))?;
    Ok(hwnd_to_wire(root.0 as isize))
}

pub fn close_window(hwnd: i64) -> A11yResult<()> {
    let hwnd = hwnd_from_wire_value(hwnd)?;
    unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }.map_err(|err| {
        A11yError::internal(format!(
            "PostMessageW(WM_CLOSE) failed for hwnd 0x{:x}: {err}",
            hwnd.0 as isize
        ))
    })
}

pub fn window_for_process(pid: u32) -> A11yResult<UIElement> {
    let _ = pid;
    Err(A11yError::internal(
        "direct UIElement process-window lookup is disabled; use snapshot_window_for_process so UIA stays on the dedicated MTA worker",
    ))
}

pub fn snapshot_window_for_process(pid: u32, depth: u32) -> A11yResult<AccessibleSubtree> {
    let hwnd = find_window_for_pid(pid).ok_or_else(|| A11yError::NoForeground {
        detail: format!("no visible top-level window for pid {pid}"),
    })?;
    super::snapshot::snapshot_window_from_hwnd(hwnd_to_wire(hwnd.0 as isize), depth)
}

pub fn top_level_window_hwnd_by_name(name: String) -> A11yResult<Option<i64>> {
    if name.is_empty() {
        return Ok(None);
    }
    with_automation(move |automation| {
        let cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Raw)?;
        let root = automation
            .get_root_element_build_cache(&cache)
            .map_err(map_uia_error)?;
        let condition = automation.create_true_condition().map_err(map_uia_error)?;
        let children = root
            .find_all_build_cache(TreeScope::Children, &condition, &cache)
            .map_err(map_uia_error)?;
        Ok(children
            .into_iter()
            .find(|element| element.get_cached_name().unwrap_or_default() == name)
            .and_then(|element| cached_hwnd(&element)))
    })
}

pub fn foreground_context(hwnd: i64) -> A11yResult<ForegroundContext> {
    let hwnd = valid_hwnd(hwnd)?;
    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
    }
    let process_path = process_path(pid).unwrap_or_default();
    let process_name = Path::new(&process_path).file_name().map_or_else(
        || format!("pid-{pid}"),
        |name| name.to_string_lossy().into_owned(),
    );
    Ok(ForegroundContext {
        hwnd: hwnd_to_wire(hwnd.0 as isize),
        pid,
        process_name,
        process_path,
        window_title: window_title(hwnd),
        window_bounds: window_rect(hwnd)?,
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: None,
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: true,
    })
}

pub fn visible_top_level_window_contexts() -> A11yResult<Vec<ForegroundContext>> {
    Ok(visible_top_level_hwnds()?
        .into_iter()
        .filter_map(|hwnd| foreground_context(hwnd_to_wire(hwnd.0 as isize)).ok())
        .filter(|context| !context.window_title.is_empty())
        .collect())
}

pub fn focused_element() -> A11yResult<UIElement> {
    Err(A11yError::internal(
        "direct UIElement focused-element lookup is disabled; use focused_element_node so UIA stays on the dedicated MTA worker",
    ))
}

pub fn element_from_point(point: Point) -> A11yResult<UIElement> {
    let _ = point;
    Err(A11yError::internal(
        "direct UIElement point lookup is disabled; use element_node_from_point so UIA stays on the dedicated MTA worker",
    ))
}
fn window_title(hwnd: HWND) -> String {
    let mut buffer = vec![0_u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..usize::try_from(len).unwrap_or(0)])
}

fn window_rect(hwnd: HWND) -> A11yResult<Rect> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &raw mut rect) }.map_err(|err| {
        if is_invalid_window_handle_error(&err) {
            A11yError::NoForeground {
                detail: format!(
                    "HWND 0x{:x} became invalid while reading the foreground window: {err}",
                    hwnd.0 as isize
                ),
            }
        } else {
            A11yError::internal(err.to_string())
        }
    })?;
    Ok(Rect {
        x: rect.left,
        y: rect.top,
        w: rect.right.saturating_sub(rect.left),
        h: rect.bottom.saturating_sub(rect.top),
    })
}

/// Result of a background-safe window move/resize (#1349).
#[derive(Clone, Copy, Debug)]
pub struct WindowBoundsOutcome {
    /// Outer-window rectangle read back via `GetWindowRect` AFTER the move/resize.
    pub actual: Rect,
    /// `true` if the window is iconic (minimized) after the call.
    pub minimized: bool,
}

/// Move and/or resize a top-level window WITHOUT activating it (background-safe:
/// `SWP_NOACTIVATE | SWP_NOZORDER`, never seizes the human foreground) and read
/// the resulting outer rect back so the caller can compare requested vs actual
/// (Windows/app minimum-size constraints) and report minimized state (#1349).
///
/// `x`/`y` are screen pixels of the top-left; `width`/`height` are outer-window
/// size. Any axis left `None` is preserved (`SWP_NOMOVE` / `SWP_NOSIZE`). The
/// HWND is validated first.
///
/// # Errors
/// Returns an error if the HWND is not a live window, a requested dimension is
/// not positive, or `SetWindowPos`/`GetWindowRect` fails.
pub fn set_window_bounds(
    hwnd: i64,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
) -> A11yResult<WindowBoundsOutcome> {
    let handle = valid_hwnd(hwnd)?;
    // Preserve the current value on any axis the caller did not specify.
    let current = window_rect(handle)?;
    let pos_x = x.unwrap_or(current.x);
    let pos_y = y.unwrap_or(current.y);
    let cx = width.unwrap_or(current.w);
    let cy = height.unwrap_or(current.h);
    if cx <= 0 || cy <= 0 {
        return Err(A11yError::internal(format!(
            "set_window_bounds: requested size must be positive, got {cx}x{cy}"
        )));
    }
    let mut flags = SWP_NOZORDER | SWP_NOACTIVATE;
    if x.is_none() && y.is_none() {
        flags |= SWP_NOMOVE;
    }
    if width.is_none() && height.is_none() {
        flags |= SWP_NOSIZE;
    }
    unsafe { SetWindowPos(handle, None, pos_x, pos_y, cx, cy, flags) }.map_err(|err| {
        A11yError::internal(format!(
            "set_window_bounds: SetWindowPos failed for HWND 0x{hwnd:x}: {err}"
        ))
    })?;
    let actual = window_rect(handle)?;
    let minimized = unsafe { IsIconic(handle) }.as_bool();
    Ok(WindowBoundsOutcome { actual, minimized })
}

fn process_path(pid: u32) -> A11yResult<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|err| A11yError::internal(err.to_string()))?;
    let mut buffer = vec![0_u16; 32_768];
    let mut len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    result.map_err(|err| A11yError::internal(err.to_string()))?;
    Ok(String::from_utf16_lossy(
        &buffer[..usize::try_from(len).unwrap_or(0)],
    ))
}

fn find_window_for_pid(pid: u32) -> Option<HWND> {
    struct Search {
        pid: u32,
        hwnd: Option<HWND>,
    }

    unsafe extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut window_pid = 0_u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&raw mut window_pid));
        }
        if window_pid == search.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            search.hwnd = Some(hwnd);
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut search = Search { pid, hwnd: None };
    unsafe {
        let _ = EnumWindows(
            Some(enum_window),
            LPARAM((&raw mut search).cast::<core::ffi::c_void>() as isize),
        );
    }
    search.hwnd
}

fn visible_top_level_hwnds() -> A11yResult<Vec<HWND>> {
    struct Search {
        hwnds: Vec<HWND>,
    }

    unsafe extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            search.hwnds.push(hwnd);
        }
        BOOL(1)
    }

    let mut search = Search { hwnds: Vec::new() };
    unsafe {
        EnumWindows(
            Some(enum_window),
            LPARAM((&raw mut search).cast::<core::ffi::c_void>() as isize),
        )
    }
    .map_err(|err| A11yError::internal(format!("EnumWindows failed: {err}")))?;
    Ok(search.hwnds)
}

const fn is_invalid_window_handle_error(error: &windows::core::Error) -> bool {
    // ERROR_INVALID_WINDOW_HANDLE (1400) surfaced through HRESULT_FROM_WIN32.
    u32::from_ne_bytes(error.code().0.to_ne_bytes()) == 0x8007_0578
}

fn valid_hwnd(hwnd: i64) -> A11yResult<HWND> {
    let hwnd = hwnd_from_wire_value(hwnd)?;
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        Ok(hwnd)
    } else {
        Err(A11yError::NoForeground {
            detail: format!("HWND 0x{:x} is not a valid window", hwnd.0 as isize),
        })
    }
}

fn hwnd_from_wire_value(hwnd: i64) -> A11yResult<HWND> {
    let native = hwnd_from_wire(hwnd).ok_or_else(|| A11yError::NoForeground {
        detail: format!(
            "HWND wire value {hwnd} is outside the canonical Win32 USER-handle range 1..=4294967295"
        ),
    })?;
    Ok(HWND(native as *mut c_void))
}

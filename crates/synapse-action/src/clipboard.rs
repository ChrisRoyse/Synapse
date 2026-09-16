use crate::{ActionError, ActionResult};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClipboardFormat {
    Text,
    Unicode,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClipboardSnapshot {
    formats: Vec<ClipboardFormatSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardFormatSnapshot {
    format: u32,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClipboardRestoreReport {
    pub format_count: usize,
    pub byte_count: usize,
}

impl ClipboardSnapshot {
    #[must_use]
    pub const fn format_count(&self) -> usize {
        self.formats.len()
    }

    #[must_use]
    pub fn byte_count(&self) -> usize {
        self.formats.iter().map(|format| format.bytes.len()).sum()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }
}

/// Reads text from the system clipboard in the requested text format.
///
/// # Errors
///
/// Returns an [`ActionError`] when the platform clipboard backend is unavailable
/// or the clipboard data cannot be decoded as the requested text format.
pub fn read_text(format: ClipboardFormat) -> ActionResult<String> {
    platform::read_text(format)
}

/// Writes text to the system clipboard in the requested text format.
///
/// # Errors
///
/// Returns an [`ActionError`] when the platform clipboard backend is unavailable,
/// when `CF_TEXT` is requested for non-ASCII text, or when a Windows clipboard API
/// call fails.
pub fn write_text(format: ClipboardFormat, text: &str) -> ActionResult<()> {
    if matches!(format, ClipboardFormat::Text) && !text.is_ascii() {
        return Err(ActionError::BackendUnavailable {
            detail: "CF_TEXT clipboard writes are limited to ASCII in M2; use unicode format"
                .to_owned(),
        });
    }
    platform::write_text(format, text)
}

/// Clears the system clipboard.
///
/// # Errors
///
/// Returns an [`ActionError`] when the platform clipboard backend is unavailable
/// or the platform clear operation fails.
pub fn clear() -> ActionResult<()> {
    platform::clear()
}

/// Snapshots the current system clipboard formats that Synapse can restore exactly.
///
/// # Errors
///
/// Returns an [`ActionError`] without modifying the clipboard when any current
/// format cannot be copied into owned memory for later restore.
pub fn snapshot() -> ActionResult<ClipboardSnapshot> {
    platform::snapshot()
}

/// Restores a previously captured system clipboard snapshot.
///
/// # Errors
///
/// Returns an [`ActionError`] when opening, clearing, allocating, or setting any
/// clipboard format fails. The caller must treat that as a failed user-state
/// restoration, not a successful action.
pub fn restore(snapshot: &ClipboardSnapshot) -> ActionResult<ClipboardRestoreReport> {
    platform::restore(snapshot)
}

/// Runs an operation that may touch the system clipboard, then restores the
/// original clipboard even when the operation itself fails.
///
/// # Errors
///
/// Returns the operation error if the operation fails and restore succeeds.
/// Returns a restore error if restoration fails, including the operation error
/// detail when both fail.
pub fn with_restored_clipboard<T>(operation: impl FnOnce() -> ActionResult<T>) -> ActionResult<T> {
    let snapshot = snapshot()?;
    let operation_result = operation();
    let restore_result = restore(&snapshot);
    match (operation_result, restore_result) {
        (Ok(value), Ok(_report)) => Ok(value),
        (Err(operation_error), Ok(_report)) => Err(operation_error),
        (Ok(_value), Err(restore_error)) => {
            let restore_detail = restore_error.detail().to_owned();
            Err(restore_error.with_detail(format!(
                "system clipboard restore failed after clipboard-backed action succeeded: {restore_detail}"
            )))
        }
        (Err(operation_error), Err(restore_error)) => {
            let restore_detail = restore_error.detail().to_owned();
            Err(restore_error.with_detail(format!(
                "system clipboard restore failed after clipboard-backed action also failed; action_error_code={} action_error_detail={} restore_error_detail={restore_detail}",
                operation_error.code(),
                operation_error.detail()
            )))
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

    use arboard::Clipboard;

    use super::{
        ActionError, ActionResult, ClipboardFormat, ClipboardFormatSnapshot,
        ClipboardRestoreReport, ClipboardSnapshot,
    };

    const VIRTUAL_UNICODE_FORMAT: u32 = 13;

    static CLIPBOARD: OnceLock<Mutex<Option<Clipboard>>> = OnceLock::new();

    pub fn read_text(_format: ClipboardFormat) -> ActionResult<String> {
        let mut guard = clipboard_guard("read")?;
        match clipboard(&mut guard, "read")?.get_text() {
            Ok(text) => Ok(text),
            Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
            Err(err) => Err(arboard_error("read", &err)),
        }
    }

    pub fn write_text(_format: ClipboardFormat, text: &str) -> ActionResult<()> {
        let mut guard = clipboard_guard("write")?;
        clipboard(&mut guard, "write")?
            .set_text(text.to_owned())
            .map_err(|err| arboard_error("write", &err))
    }

    pub fn clear() -> ActionResult<()> {
        let mut guard = clipboard_guard("clear")?;
        clipboard(&mut guard, "clear")?
            .clear()
            .map_err(|err| arboard_error("clear", &err))
    }

    pub fn snapshot() -> ActionResult<ClipboardSnapshot> {
        let text = read_text(ClipboardFormat::Unicode)?;
        Ok(ClipboardSnapshot {
            formats: vec![ClipboardFormatSnapshot {
                format: VIRTUAL_UNICODE_FORMAT,
                bytes: text.into_bytes(),
            }],
        })
    }

    pub fn restore(snapshot: &ClipboardSnapshot) -> ActionResult<ClipboardRestoreReport> {
        let Some(format) = snapshot.formats.first() else {
            clear()?;
            return Ok(ClipboardRestoreReport::default());
        };
        if format.format != VIRTUAL_UNICODE_FORMAT || snapshot.formats.len() != 1 {
            return Err(ActionError::BackendUnavailable {
                detail:
                    "Linux clipboard restore supports only the text snapshot captured by Synapse"
                        .to_owned(),
            });
        }
        let text = String::from_utf8(format.bytes.clone()).map_err(|err| {
            ActionError::BackendUnavailable {
                detail: format!("Linux clipboard snapshot text was not UTF-8: {err}"),
            }
        })?;
        write_text(ClipboardFormat::Unicode, &text)?;
        Ok(ClipboardRestoreReport {
            format_count: snapshot.format_count(),
            byte_count: snapshot.byte_count(),
        })
    }

    fn clipboard_guard(
        context: &'static str,
    ) -> ActionResult<MutexGuard<'static, Option<Clipboard>>> {
        CLIPBOARD
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|err| poisoned_error(context, err))
    }

    fn clipboard<'a>(
        guard: &'a mut MutexGuard<'_, Option<Clipboard>>,
        context: &'static str,
    ) -> ActionResult<&'a mut Clipboard> {
        if guard.is_none() {
            **guard = Some(Clipboard::new().map_err(|err| arboard_error(context, &err))?);
        }
        guard
            .as_mut()
            .ok_or_else(|| ActionError::BackendUnavailable {
                detail: format!(
                    "Linux clipboard {context} could not initialize a clipboard handle"
                ),
            })
    }

    fn poisoned_error<T>(
        context: &'static str,
        _err: PoisonError<MutexGuard<'static, T>>,
    ) -> ActionError {
        ActionError::BackendUnavailable {
            detail: format!("Linux clipboard {context} lock is poisoned"),
        }
    }

    fn arboard_error(context: &'static str, err: &arboard::Error) -> ActionError {
        ActionError::BackendUnavailable {
            detail: format!("Linux clipboard {context} failed: {err}"),
        }
    }
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
mod platform {
    use super::{
        ActionError, ActionResult, ClipboardFormat, ClipboardRestoreReport, ClipboardSnapshot,
    };

    pub fn read_text(_format: ClipboardFormat) -> ActionResult<String> {
        Err(unavailable("read"))
    }

    pub fn write_text(_format: ClipboardFormat, _text: &str) -> ActionResult<()> {
        Err(unavailable("write"))
    }

    pub fn clear() -> ActionResult<()> {
        Err(unavailable("clear"))
    }

    pub fn snapshot() -> ActionResult<ClipboardSnapshot> {
        Err(unavailable("snapshot"))
    }

    pub fn restore(_snapshot: &ClipboardSnapshot) -> ActionResult<ClipboardRestoreReport> {
        Err(unavailable("restore"))
    }

    fn unavailable(verb: &'static str) -> ActionError {
        ActionError::BackendUnavailable {
            detail: format!("act_clipboard {verb} is implemented on Windows and Linux/X11 only"),
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::{
        ptr, slice, thread,
        time::{Duration, Instant},
    };

    use windows::{
        Win32::{
            Foundation::{ERROR_SUCCESS, GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND},
            System::{
                DataExchange::{
                    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
                    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
                },
                Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
            },
            UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
            },
        },
        core::w,
    };

    use super::{
        ActionError, ActionResult, ClipboardFormat, ClipboardFormatSnapshot,
        ClipboardRestoreReport, ClipboardSnapshot,
    };

    const CF_TEXT: u32 = 1;
    const CF_UNICODETEXT: u32 = 13;
    const OPEN_CLIPBOARD_RETRY_TIMEOUT: Duration = Duration::from_millis(250);
    const OPEN_CLIPBOARD_RETRY_DELAY: Duration = Duration::from_millis(10);

    pub fn read_text(format: ClipboardFormat) -> ActionResult<String> {
        let _clipboard = ClipboardGuard::open("read", false)?;
        if !format_available(format) {
            return Ok(String::new());
        }

        let handle = unsafe {
            // SAFETY: The clipboard is open for this thread, and the format code is
            // one of the standard text formats accepted by GetClipboardData.
            GetClipboardData(format_code(format))
        }
        .map_err(|err| windows_error("GetClipboardData", &err))?;
        let hglobal = HGLOBAL(handle.0);
        let byte_len = unsafe {
            // SAFETY: The clipboard owns this HGLOBAL while the clipboard remains open.
            GlobalSize(hglobal)
        };
        let locked = LockedGlobal::lock(hglobal, "read")?;
        match format {
            ClipboardFormat::Unicode => read_unicode(locked.ptr(), byte_len),
            ClipboardFormat::Text => Ok(read_text_bytes(locked.ptr(), byte_len)),
        }
    }

    pub fn write_text(format: ClipboardFormat, text: &str) -> ActionResult<()> {
        let _clipboard = ClipboardGuard::open("write", true)?;
        let memory = match format {
            ClipboardFormat::Unicode => GlobalMemory::from_bytes(&unicode_clipboard_bytes(text))?,
            ClipboardFormat::Text => GlobalMemory::from_bytes(&text_clipboard_bytes(text))?,
        };
        unsafe {
            // SAFETY: The clipboard is open for this thread.
            EmptyClipboard()
        }
        .map_err(|err| windows_error("EmptyClipboard", &err))?;
        memory.give_to_clipboard(format)?;
        if !format_available(format) {
            return Err(ActionError::BackendUnavailable {
                detail: format!(
                    "SetClipboardData reported success but {} was not available after write",
                    format_name(format)
                ),
            });
        }
        Ok(())
    }

    pub fn clear() -> ActionResult<()> {
        let _clipboard = ClipboardGuard::open("clear", true)?;
        unsafe {
            // SAFETY: The clipboard is open for this thread.
            EmptyClipboard()
        }
        .map_err(|err| windows_error("EmptyClipboard", &err))
    }

    pub fn snapshot() -> ActionResult<ClipboardSnapshot> {
        let _clipboard = ClipboardGuard::open("snapshot", false)?;
        let mut formats = Vec::new();
        let mut previous = 0_u32;
        loop {
            let format = unsafe {
                // SAFETY: The clipboard is open for this thread.
                EnumClipboardFormats(previous)
            };
            if format == 0 {
                let last_error = unsafe {
                    // SAFETY: GetLastError reads the calling thread's last-error code.
                    GetLastError()
                };
                if last_error == ERROR_SUCCESS {
                    break;
                }
                return Err(ActionError::BackendUnavailable {
                    detail: format!(
                        "EnumClipboardFormats failed while snapshotting Windows clipboard after format {previous}: last_error={}",
                        last_error.0
                    ),
                });
            }
            let handle = unsafe {
                // SAFETY: The clipboard is open for this thread, and the format
                // identifier was returned by EnumClipboardFormats.
                GetClipboardData(format)
            }
            .map_err(|err| windows_error("GetClipboardData snapshot", &err))?;
            let bytes = copy_hglobal_bytes(HGLOBAL(handle.0), format)?;
            formats.push(ClipboardFormatSnapshot { format, bytes });
            previous = format;
        }
        Ok(ClipboardSnapshot { formats })
    }

    pub fn restore(snapshot: &ClipboardSnapshot) -> ActionResult<ClipboardRestoreReport> {
        let _clipboard = ClipboardGuard::open("restore", true)?;
        unsafe {
            // SAFETY: The clipboard is open for this thread and this process owns it.
            EmptyClipboard()
        }
        .map_err(|err| windows_error("EmptyClipboard restore", &err))?;
        for format in &snapshot.formats {
            let memory = GlobalMemory::from_bytes(&format.bytes)?;
            memory.give_to_clipboard_raw(format.format)?;
        }
        Ok(ClipboardRestoreReport {
            format_count: snapshot.format_count(),
            byte_count: snapshot.byte_count(),
        })
    }

    struct ClipboardGuard {
        _owner: Option<ClipboardOwner>,
    }

    impl ClipboardGuard {
        fn open(context: &'static str, require_owner: bool) -> ActionResult<Self> {
            let owner = require_owner.then(ClipboardOwner::create).transpose()?;
            let hwnd = owner.as_ref().map(|owner| owner.hwnd);
            let started = Instant::now();
            let mut attempts = 0_u32;
            loop {
                attempts += 1;
                let result = unsafe {
                    // SAFETY: The optional owner HWND, when present, is kept alive by
                    // the guard until after CloseClipboard runs.
                    OpenClipboard(hwnd)
                };
                match result {
                    Ok(()) => return Ok(Self { _owner: owner }),
                    Err(err) if started.elapsed() < OPEN_CLIPBOARD_RETRY_TIMEOUT => {
                        tracing::debug!(
                            code = "ACTION_CLIPBOARD_OPEN_RETRY",
                            context,
                            attempts,
                            error = %err,
                            "readback=windows_clipboard open_retry"
                        );
                        thread::sleep(OPEN_CLIPBOARD_RETRY_DELAY);
                    }
                    Err(err) => {
                        return Err(windows_open_error(context, attempts, started, &err));
                    }
                }
            }
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                // SAFETY: This guard is created only after OpenClipboard succeeds.
                let _ = CloseClipboard();
            }
        }
    }

    struct ClipboardOwner {
        hwnd: HWND,
    }

    impl ClipboardOwner {
        fn create() -> ActionResult<Self> {
            let hwnd = unsafe {
                // SAFETY: The built-in STATIC class is registered by the system. The
                // message-only parent keeps the temporary owner hidden and local to this
                // process while the clipboard is open.
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("SynapseClipboardOwner"),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                )
            }
            .map_err(|err| windows_error("CreateWindowExW clipboard owner", &err))?;
            if hwnd.is_invalid() {
                return Err(ActionError::BackendUnavailable {
                    detail: "CreateWindowExW returned an invalid clipboard owner HWND".to_owned(),
                });
            }
            Ok(Self { hwnd })
        }
    }

    impl Drop for ClipboardOwner {
        fn drop(&mut self) {
            unsafe {
                // SAFETY: hwnd was returned by CreateWindowExW and is owned by this guard.
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    struct LockedGlobal {
        handle: HGLOBAL,
        ptr: *mut core::ffi::c_void,
    }

    impl LockedGlobal {
        fn lock(handle: HGLOBAL, context: &'static str) -> ActionResult<Self> {
            let ptr = unsafe {
                // SAFETY: Caller passes an HGLOBAL returned by clipboard/global APIs.
                GlobalLock(handle)
            };
            if ptr.is_null() {
                return Err(ActionError::BackendUnavailable {
                    detail: format!("GlobalLock failed during clipboard {context}"),
                });
            }
            Ok(Self { handle, ptr })
        }

        const fn ptr(&self) -> *const core::ffi::c_void {
            self.ptr.cast_const()
        }
    }

    impl Drop for LockedGlobal {
        fn drop(&mut self) {
            unsafe {
                // SAFETY: This guard is created only after GlobalLock returns non-null.
                let _ = GlobalUnlock(self.handle);
            }
        }
    }

    struct GlobalMemory {
        handle: HGLOBAL,
        owned: bool,
    }

    impl GlobalMemory {
        fn from_bytes(bytes: &[u8]) -> ActionResult<Self> {
            let handle = unsafe {
                // SAFETY: GlobalAlloc is called with GMEM_MOVEABLE as required by
                // SetClipboardData, and the byte length is derived from a slice.
                GlobalAlloc(GMEM_MOVEABLE, bytes.len())
            }
            .map_err(|err| windows_error("GlobalAlloc", &err))?;
            {
                let locked = LockedGlobal::lock(handle, "write")?;
                unsafe {
                    // SAFETY: The destination points to a GlobalAlloc block of
                    // bytes.len() bytes and the source slice is valid for that length.
                    ptr::copy_nonoverlapping(bytes.as_ptr(), locked.ptr.cast::<u8>(), bytes.len());
                }
            }
            Ok(Self {
                handle,
                owned: true,
            })
        }

        fn give_to_clipboard(self, format: ClipboardFormat) -> ActionResult<()> {
            self.give_to_clipboard_raw(format_code(format))
        }

        fn give_to_clipboard_raw(mut self, format: u32) -> ActionResult<()> {
            unsafe {
                // SAFETY: The clipboard is open and SetClipboardData takes ownership
                // of the movable memory handle on success.
                SetClipboardData(format, Some(HANDLE(self.handle.0)))
            }
            .map_err(|err| windows_error("SetClipboardData", &err))?;
            self.owned = false;
            Ok(())
        }
    }

    impl Drop for GlobalMemory {
        fn drop(&mut self) {
            if self.owned {
                unsafe {
                    // SAFETY: The handle is still owned by this process when owned=true.
                    let _ = GlobalFree(Some(self.handle));
                }
            }
        }
    }

    fn format_available(format: ClipboardFormat) -> bool {
        unsafe {
            // SAFETY: The clipboard is open for this thread, and the format code is
            // one of the standard clipboard text formats.
            IsClipboardFormatAvailable(format_code(format))
        }
        .is_ok()
    }

    const fn format_code(format: ClipboardFormat) -> u32 {
        match format {
            ClipboardFormat::Text => CF_TEXT,
            ClipboardFormat::Unicode => CF_UNICODETEXT,
        }
    }

    const fn format_name(format: ClipboardFormat) -> &'static str {
        match format {
            ClipboardFormat::Text => "CF_TEXT",
            ClipboardFormat::Unicode => "CF_UNICODETEXT",
        }
    }

    fn unicode_clipboard_bytes(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn text_clipboard_bytes(text: &str) -> Vec<u8> {
        text.bytes().chain(std::iter::once(0)).collect()
    }

    fn read_unicode(ptr: *const core::ffi::c_void, byte_len: usize) -> ActionResult<String> {
        let unit_len = byte_len / size_of::<u16>();
        let units = unsafe {
            // SAFETY: The pointer comes from a locked global memory block, and unit_len
            // is bounded by GlobalSize for that block.
            slice::from_raw_parts(ptr.cast::<u16>(), unit_len)
        };
        let nul = units.iter().position(|unit| *unit == 0).unwrap_or(unit_len);
        String::from_utf16(&units[..nul]).map_err(|err| ActionError::BackendUnavailable {
            detail: format!("clipboard unicode text is invalid UTF-16: {err}"),
        })
    }

    fn read_text_bytes(ptr: *const core::ffi::c_void, byte_len: usize) -> String {
        let bytes = unsafe {
            // SAFETY: The pointer comes from a locked global memory block, and byte_len
            // is exactly GlobalSize for that block.
            slice::from_raw_parts(ptr.cast::<u8>(), byte_len)
        };
        let nul = bytes.iter().position(|byte| *byte == 0).unwrap_or(byte_len);
        String::from_utf8_lossy(&bytes[..nul]).into_owned()
    }

    fn windows_error(context: &'static str, err: &windows::core::Error) -> ActionError {
        ActionError::BackendUnavailable {
            detail: format!("{context} failed for Windows clipboard: {err}"),
        }
    }

    fn windows_open_error(
        context: &'static str,
        attempts: u32,
        started: Instant,
        err: &windows::core::Error,
    ) -> ActionError {
        ActionError::BackendUnavailable {
            detail: format!(
                "{context} failed for Windows clipboard after {attempts} open attempts over {} ms: {err}",
                started.elapsed().as_millis()
            ),
        }
    }

    fn copy_hglobal_bytes(handle: HGLOBAL, format: u32) -> ActionResult<Vec<u8>> {
        let byte_len = unsafe {
            // SAFETY: The handle was returned by GetClipboardData for an enumerated format.
            GlobalSize(handle)
        };
        if byte_len == 0 {
            return Err(ActionError::BackendUnavailable {
                detail: format!(
                    "Windows clipboard format {format} has zero GlobalSize and cannot be restored byte-for-byte"
                ),
            });
        }
        let locked = LockedGlobal::lock(handle, "snapshot")?;
        let bytes = unsafe {
            // SAFETY: The pointer comes from a locked global memory block, and byte_len
            // is exactly GlobalSize for that block.
            slice::from_raw_parts(locked.ptr().cast::<u8>(), byte_len)
        };
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use synapse_core::error_codes;

    use super::*;

    #[test]
    fn cf_text_non_ascii_fails_as_backend_unavailable_before_platform_open() {
        let error = write_text(ClipboardFormat::Text, "unicode-clipboard-edge-雪")
            .expect_err("non-ASCII CF_TEXT writes must fail closed");

        assert_eq!(error.code(), error_codes::ACTION_BACKEND_UNAVAILABLE);
        assert!(matches!(error, ActionError::BackendUnavailable { .. }));
        assert!(error.detail().contains("CF_TEXT"));
    }
}

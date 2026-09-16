//! Counts-only low-level input hook for timeline interaction cadence (#838).
//!
//! The hook never records key names, characters, mouse coordinates, window
//! text, or clipboard content. It only emits event class counters plus the
//! OS-injected flag so the activity recorder can keep human cadence separate
//! from Synapse-generated input.

use anyhow::Result;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractionEvent {
    pub ts_ns: u64,
    pub kind: InteractionEventKind,
    pub injected: bool,
    pub key_signal: Option<InteractionKeySignal>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionEventKind {
    Keystroke,
    Click,
    VerticalScroll { delta: i32 },
    HorizontalScroll { delta: i32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionKeySignal {
    UndoCommand,
    DeleteCommand,
    TextLikeKey,
    OtherKey,
}

pub struct InteractionHook {
    inner: platform::InteractionHook,
}

impl InteractionHook {
    /// Starts the platform low-level input hook.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform hook cannot be installed. The daemon
    /// must fail closed rather than silently run without cadence rows.
    pub fn start(sender: mpsc::UnboundedSender<InteractionEvent>) -> Result<Self> {
        Ok(Self {
            inner: platform::InteractionHook::start(sender)?,
        })
    }

    #[must_use]
    pub const fn readback(&self) -> &InteractionHookReadback {
        self.inner.readback()
    }

    pub fn shutdown_checked(
        self,
        timeout: std::time::Duration,
        reason: &'static str,
    ) -> InteractionHookShutdownReport {
        self.inner.shutdown_checked(timeout, reason)
    }
}

/// Reaps terminal hook owners retained by a prior bounded shutdown and returns
/// the exact count that is still physically live. A nonzero count must prevent
/// daemon lifetime-lock release even when no `ActivityRecorder` was returned to
/// the caller (for example, a failed startup transaction).
#[must_use]
pub(crate) fn retained_live_owner_count() -> usize {
    platform::retained_live_owner_count()
}

#[derive(Clone, Debug)]
pub struct InteractionHookShutdownReport {
    pub reason: &'static str,
    pub stop_wake_sent: bool,
    pub sender_cleared: bool,
    pub thread_owner_present: bool,
    pub thread_terminal: bool,
    pub thread_joined: bool,
    pub exact_owner_retained: bool,
    pub failures: Vec<String>,
}

impl InteractionHookShutdownReport {
    #[must_use]
    pub const fn owners_quiescent(&self) -> bool {
        (!self.thread_owner_present || (self.thread_terminal && self.thread_joined))
            && self.sender_cleared
    }

    pub fn verdict(&self) -> anyhow::Result<()> {
        if self.failures.is_empty()
            && self.stop_wake_sent
            && self.owners_quiescent()
            && !self.exact_owner_retained
        {
            Ok(())
        } else {
            anyhow::bail!(
                "interaction hook shutdown failed at {}: {}; readback={self:?}",
                self.reason,
                self.failures.join("; ")
            )
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionHookReadback {
    pub thread_id: u32,
    pub keyboard_hook_installed: bool,
    pub mouse_hook_installed: bool,
}

#[cfg(windows)]
mod platform {
    use std::{
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
            mpsc as std_mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail};
    use tokio::sync::mpsc;
    use windows::Win32::{
        Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::Input::KeyboardAndMouse::GetAsyncKeyState,
        UI::WindowsAndMessaging::{
            CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
            PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
            WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_MBUTTONDOWN,
            WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_NULL, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_SYSKEYUP,
            WM_XBUTTONDOWN,
        },
    };

    use super::{
        InteractionEvent, InteractionEventKind, InteractionHookReadback,
        InteractionHookShutdownReport, InteractionKeySignal,
    };

    const HOOK_START_TIMEOUT: Duration = Duration::from_secs(3);

    const LLKHF_INJECTED_MASK: u32 = 0x12;
    const LLMHF_INJECTED_MASK: u32 = 0x03;
    const KEY_DOWN_MASK: u16 = 0x8000;
    const VK_BACK_CODE: u32 = 0x08;
    const VK_SPACE_CODE: u32 = 0x20;
    const VK_DELETE_CODE: u32 = 0x2e;
    const VK_CONTROL_CODE: u32 = 0x11;
    const VK_LCONTROL_CODE: u32 = 0xa2;
    const VK_RCONTROL_CODE: u32 = 0xa3;
    const VK_Z_CODE: u32 = 0x5a;
    const VK_PACKET_CODE: u32 = 0xe7;

    static HOOK_SENDER: OnceLock<Mutex<Option<mpsc::UnboundedSender<InteractionEvent>>>> =
        OnceLock::new();
    type HookThreadResult = std::result::Result<(), String>;
    type HookThreadOwner = thread::JoinHandle<HookThreadResult>;

    static RETAINED_HOOK_OWNERS: OnceLock<Mutex<Vec<HookThreadOwner>>> = OnceLock::new();
    static HOOK_OWNER_PHASE: OnceLock<Mutex<HookOwnerPhase>> = OnceLock::new();

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum HookOwnerPhase {
        Terminal,
        Starting,
        Running,
        Stopping,
        Retained,
    }

    fn hook_owner_phase() -> &'static Mutex<HookOwnerPhase> {
        HOOK_OWNER_PHASE.get_or_init(|| Mutex::new(HookOwnerPhase::Terminal))
    }

    fn set_hook_owner_phase(next: HookOwnerPhase) {
        match hook_owner_phase().lock() {
            Ok(mut phase) => *phase = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    const fn hook_start_allowed(phase: HookOwnerPhase, retained_live: usize) -> bool {
        matches!(phase, HookOwnerPhase::Terminal) && retained_live == 0
    }

    fn begin_hook_start() -> Result<()> {
        let retained_live = retained_live_owner_count();
        let mut phase = match hook_owner_phase().lock() {
            Ok(phase) => phase,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !hook_start_allowed(*phase, retained_live) {
            bail!(
                "interaction cadence hook owner is not terminal: phase={phase:?} retained_live={retained_live}"
            );
        }
        *phase = HookOwnerPhase::Starting;
        Ok(())
    }

    fn begin_hook_stop() -> Option<String> {
        let mut phase = match hook_owner_phase().lock() {
            Ok(phase) => phase,
            Err(poisoned) => poisoned.into_inner(),
        };
        let previous = *phase;
        *phase = HookOwnerPhase::Stopping;
        (!matches!(previous, HookOwnerPhase::Running | HookOwnerPhase::Starting)).then(|| {
            format!("interaction hook stop began from unexpected owner phase {previous:?}")
        })
    }

    fn finish_hook_stop(exact_owner_retained: bool) {
        set_hook_owner_phase(if exact_owner_retained {
            HookOwnerPhase::Retained
        } else {
            HookOwnerPhase::Terminal
        });
    }

    pub struct InteractionHook {
        readback: InteractionHookReadback,
        thread: Option<HookThreadOwner>,
        stop_requested: Arc<AtomicBool>,
        shutdown_complete: bool,
    }

    impl InteractionHook {
        pub fn start(sender: mpsc::UnboundedSender<InteractionEvent>) -> Result<Self> {
            begin_hook_start()?;
            {
                let mut slot = match hook_sender().lock() {
                    Ok(slot) => slot,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if slot.is_some() {
                    set_hook_owner_phase(HookOwnerPhase::Retained);
                    bail!("interaction cadence hook is already installed in this process");
                }
                *slot = Some(sender);
            }

            let (ready_tx, ready_rx) = std_mpsc::channel();
            let (thread_id_tx, thread_id_rx) = std_mpsc::channel();
            let stop_requested = Arc::new(AtomicBool::new(false));
            let thread_stop_requested = Arc::clone(&stop_requested);
            let thread = match thread::Builder::new()
                .name("synapse-interaction-cadence-hook".to_owned())
                .spawn(move || run_hook_thread(thread_id_tx, ready_tx, thread_stop_requested))
                .context("spawn interaction cadence hook thread")
            {
                Ok(thread) => thread,
                Err(error) => {
                    let _phase_failure = begin_hook_stop();
                    clear_sender();
                    finish_hook_stop(false);
                    return Err(error);
                }
            };
            let thread_id = match thread_id_rx.recv_timeout(HOOK_START_TIMEOUT) {
                Ok(thread_id) => thread_id,
                Err(error) => {
                    let _phase_failure = begin_hook_stop();
                    clear_sender();
                    stop_requested.store(true, Ordering::Release);
                    let (terminal, joined, retained, join_failure) =
                        join_thread_until(thread, HOOK_START_TIMEOUT);
                    finish_hook_stop(retained);
                    bail!(
                        "interaction cadence hook thread did not publish its identity: {error}; terminal={terminal} joined={joined} exact_owner_retained={retained} join_failure={join_failure:?}"
                    );
                }
            };
            let readback = match ready_rx.recv_timeout(HOOK_START_TIMEOUT) {
                Ok(Ok(readback)) => readback,
                Ok(Err(error)) => {
                    let _phase_failure = begin_hook_stop();
                    clear_sender();
                    stop_requested.store(true, Ordering::Release);
                    let _ = unsafe { PostThreadMessageW(thread_id, WM_NULL, WPARAM(0), LPARAM(0)) };
                    let (terminal, joined, retained, join_failure) =
                        join_thread_until(thread, HOOK_START_TIMEOUT);
                    finish_hook_stop(retained);
                    bail!(
                        "{error}; terminal={terminal} joined={joined} exact_owner_retained={retained} join_failure={join_failure:?}"
                    );
                }
                Err(error) => {
                    let _phase_failure = begin_hook_stop();
                    clear_sender();
                    stop_requested.store(true, Ordering::Release);
                    let _ = unsafe { PostThreadMessageW(thread_id, WM_NULL, WPARAM(0), LPARAM(0)) };
                    let (terminal, joined, retained, join_failure) =
                        join_thread_until(thread, HOOK_START_TIMEOUT);
                    finish_hook_stop(retained);
                    bail!(
                        "interaction cadence hook thread exited before readiness: {error}; terminal={terminal} joined={joined} exact_owner_retained={retained} join_failure={join_failure:?}"
                    );
                }
            };
            set_hook_owner_phase(HookOwnerPhase::Running);
            Ok(Self {
                readback,
                thread: Some(thread),
                stop_requested,
                shutdown_complete: false,
            })
        }

        pub const fn readback(&self) -> &InteractionHookReadback {
            &self.readback
        }

        pub fn shutdown_checked(
            mut self,
            timeout: Duration,
            reason: &'static str,
        ) -> InteractionHookShutdownReport {
            self.shutdown_inner(timeout, reason)
        }

        fn shutdown_inner(
            &mut self,
            timeout: Duration,
            reason: &'static str,
        ) -> InteractionHookShutdownReport {
            if self.shutdown_complete {
                return InteractionHookShutdownReport {
                    reason,
                    stop_wake_sent: true,
                    sender_cleared: true,
                    thread_owner_present: false,
                    thread_terminal: true,
                    thread_joined: true,
                    exact_owner_retained: false,
                    failures: Vec::new(),
                };
            }
            self.shutdown_complete = true;
            let mut failures = Vec::new();
            if let Some(failure) = begin_hook_stop() {
                failures.push(failure);
            }
            let sender_cleared = clear_sender();
            let thread_owner_present = self.thread.is_some();
            let thread_finished_before_stop = self
                .thread
                .as_ref()
                .is_some_and(thread::JoinHandle::is_finished);
            self.stop_requested.store(true, Ordering::Release);
            let stop_wake_sent = if !thread_owner_present {
                true
            } else if thread_finished_before_stop {
                failures.push(
                    "interaction hook thread terminated before the exact stop request".to_owned(),
                );
                false
            } else {
                // WM_NULL is deliberately benign. If the hook thread exited
                // between the terminal read and this call and Windows already
                // recycled its TID, the wake cannot terminate an unrelated
                // thread (the old WM_QUIT path could). The exact owner checks
                // `stop_requested` after GetMessageW wakes.
                match unsafe {
                    PostThreadMessageW(self.readback.thread_id, WM_NULL, WPARAM(0), LPARAM(0))
                } {
                    Ok(()) => true,
                    Err(error) => {
                        failures.push(format!("interaction hook stop wake failed: {error}"));
                        false
                    }
                }
            };
            let (thread_terminal, thread_joined, exact_owner_retained, join_failure) = self
                .thread
                .take()
                .map_or((true, true, false, None), |thread| {
                    join_thread_until(thread, timeout)
                });
            finish_hook_stop(exact_owner_retained);
            if let Some(failure) = join_failure {
                failures.push(failure);
            }
            InteractionHookShutdownReport {
                reason,
                stop_wake_sent,
                sender_cleared,
                thread_owner_present,
                thread_terminal,
                thread_joined,
                exact_owner_retained,
                failures,
            }
        }
    }

    impl Drop for InteractionHook {
        fn drop(&mut self) {
            let report = self.shutdown_inner(HOOK_START_TIMEOUT, "drop_backstop");
            if !report.owners_quiescent() || !report.failures.is_empty() {
                tracing::error!(
                    code = "TIMELINE_INTERACTION_HOOK_DROP_INCOMPLETE",
                    report = ?report,
                    "interaction hook drop backstop could not prove every owner terminal"
                );
            }
        }
    }

    struct HookGuard(HHOOK);

    impl Drop for HookGuard {
        fn drop(&mut self) {
            let _ = unsafe { UnhookWindowsHookEx(self.0) };
        }
    }

    fn hook_sender() -> &'static Mutex<Option<mpsc::UnboundedSender<InteractionEvent>>> {
        HOOK_SENDER.get_or_init(|| Mutex::new(None))
    }

    fn clear_sender() -> bool {
        match hook_sender().lock() {
            Ok(mut guard) => {
                *guard = None;
                true
            }
            Err(poisoned) => {
                *poisoned.into_inner() = None;
                tracing::error!(
                    code = "TIMELINE_INTERACTION_HOOK_SENDER_POISONED",
                    "recovered poisoned interaction-hook sender while clearing ownership"
                );
                true
            }
        }
    }

    fn join_thread_until(
        thread: HookThreadOwner,
        timeout: Duration,
    ) -> (bool, bool, bool, Option<String>) {
        let deadline = Instant::now() + timeout;
        while !thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !thread.is_finished() {
            retain_live_owner(thread);
            return (
                false,
                false,
                true,
                Some(format!(
                    "interaction hook thread did not terminate within {} ms; exact JoinHandle retained until process teardown",
                    timeout.as_millis()
                )),
            );
        }
        match thread.join() {
            Ok(Ok(())) => (true, true, false, None),
            Ok(Err(error)) => (
                true,
                true,
                false,
                Some(format!("interaction hook thread failed: {error}")),
            ),
            Err(_panic) => (
                true,
                true,
                false,
                Some("interaction hook thread panicked".to_owned()),
            ),
        }
    }

    fn retain_live_owner(thread: HookThreadOwner) {
        let owners = RETAINED_HOOK_OWNERS.get_or_init(|| Mutex::new(Vec::new()));
        match owners.lock() {
            Ok(mut owners) => owners.push(thread),
            Err(poisoned) => poisoned.into_inner().push(thread),
        }
    }

    pub(super) fn retained_live_owner_count() -> usize {
        let owners = RETAINED_HOOK_OWNERS.get_or_init(|| Mutex::new(Vec::new()));
        let mut owners = match owners.lock() {
            Ok(owners) => owners,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut still_live = Vec::with_capacity(owners.len());
        for owner in std::mem::take(&mut *owners) {
            if owner.is_finished() {
                match owner.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::error!(
                        code = "TIMELINE_INTERACTION_HOOK_RETAINED_OWNER_FAILED",
                        detail = %error,
                        "terminal retained interaction-hook owner failed while being reaped"
                    ),
                    Err(_panic) => tracing::error!(
                        code = "TIMELINE_INTERACTION_HOOK_RETAINED_OWNER_PANICKED",
                        "terminal retained interaction-hook owner panicked while being reaped"
                    ),
                }
            } else {
                still_live.push(owner);
            }
        }
        let count = still_live.len();
        *owners = still_live;
        drop(owners);
        if count == 0 {
            let mut phase = match hook_owner_phase().lock() {
                Ok(phase) => phase,
                Err(poisoned) => poisoned.into_inner(),
            };
            if *phase == HookOwnerPhase::Retained {
                *phase = HookOwnerPhase::Terminal;
            }
        }
        count
    }

    fn run_hook_thread(
        thread_id_tx: std_mpsc::Sender<u32>,
        ready_tx: std_mpsc::Sender<Result<InteractionHookReadback, String>>,
        stop_requested: Arc<AtomicBool>,
    ) -> HookThreadResult {
        let thread_id = unsafe { GetCurrentThreadId() };
        let _ = thread_id_tx.send(thread_id);
        if stop_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        let module = match unsafe { GetModuleHandleW(None) } {
            Ok(module) => module,
            Err(error) => {
                let detail =
                    format!("GetModuleHandleW failed for interaction cadence hook: {error}");
                let _ = ready_tx.send(Err(detail.clone()));
                return Err(detail);
            }
        };
        let keyboard_hook = match unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook_proc),
                Some(HINSTANCE(module.0)),
                0,
            )
        } {
            Ok(hook) => hook,
            Err(error) => {
                let detail = format!("SetWindowsHookExW(WH_KEYBOARD_LL) failed: {error}");
                let _ = ready_tx.send(Err(detail.clone()));
                return Err(detail);
            }
        };
        let mouse_hook = match unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_hook_proc),
                Some(HINSTANCE(module.0)),
                0,
            )
        } {
            Ok(hook) => hook,
            Err(error) => {
                let _keyboard_guard = HookGuard(keyboard_hook);
                let detail = format!("SetWindowsHookExW(WH_MOUSE_LL) failed: {error}");
                let _ = ready_tx.send(Err(detail.clone()));
                return Err(detail);
            }
        };
        let _keyboard_guard = HookGuard(keyboard_hook);
        let _mouse_guard = HookGuard(mouse_hook);
        // PostThreadMessageW fails until the target thread owns a message
        // queue. Create it before publishing readiness so an immediate stop
        // always has a real wake surface.
        let mut msg = MSG::default();
        let _ = unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE) };
        if stop_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        ready_tx
            .send(Ok(InteractionHookReadback {
                thread_id,
                keyboard_hook_installed: true,
                mouse_hook_installed: true,
            }))
            .map_err(|_| {
                "interaction hook readiness receiver closed before acknowledgement".to_owned()
            })?;

        loop {
            let result = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
            if result.0 == -1 {
                return Err(format!(
                    "GetMessageW failed for interaction cadence hook: {}",
                    windows::core::Error::from_thread()
                ));
            }
            if result.0 == 0 {
                return if stop_requested.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err("interaction hook message loop received WM_QUIT without an exact stop request"
                        .to_owned())
                };
            }
            if stop_requested.load(Ordering::Acquire) {
                return Ok(());
            }
        }
    }

    unsafe extern "system" fn keyboard_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code >= 0 {
            let message = u32::try_from(wparam.0).unwrap_or(0);
            if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) {
                let data = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
                emit(
                    InteractionEventKind::Keystroke,
                    data.flags.0 & LLKHF_INJECTED_MASK != 0,
                    Some(key_signal(data.vkCode)),
                );
            } else if matches!(message, WM_KEYUP | WM_SYSKEYUP) {
                // Key-up confirms release state but is not an interaction
                // count. Counting key-down only avoids doubling keystrokes.
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    unsafe extern "system" fn mouse_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code >= 0 {
            let message = u32::try_from(wparam.0).unwrap_or(0);
            let data = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
            let injected = data.flags & LLMHF_INJECTED_MASK != 0;
            match message {
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                    emit(InteractionEventKind::Click, injected, None);
                }
                WM_MOUSEWHEEL => {
                    emit(
                        InteractionEventKind::VerticalScroll {
                            delta: wheel_delta(data.mouseData),
                        },
                        injected,
                        None,
                    );
                }
                WM_MOUSEHWHEEL => {
                    emit(
                        InteractionEventKind::HorizontalScroll {
                            delta: wheel_delta(data.mouseData),
                        },
                        injected,
                        None,
                    );
                }
                _ => {}
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    fn emit(kind: InteractionEventKind, injected: bool, key_signal: Option<InteractionKeySignal>) {
        let event = InteractionEvent {
            ts_ns: super::super_now_ts_ns(),
            kind,
            injected,
            key_signal,
        };
        if let Ok(guard) = hook_sender().lock()
            && let Some(sender) = guard.as_ref()
        {
            let _ = sender.send(event);
        }
    }

    fn wheel_delta(mouse_data: u32) -> i32 {
        i32::from(((mouse_data >> 16) as u16) as i16)
    }

    fn key_signal(vk_code: u32) -> InteractionKeySignal {
        key_signal_with_ctrl(vk_code, ctrl_down())
    }

    fn key_signal_with_ctrl(vk_code: u32, ctrl_down: bool) -> InteractionKeySignal {
        if vk_code == VK_Z_CODE && ctrl_down {
            return InteractionKeySignal::UndoCommand;
        }
        if matches!(vk_code, VK_BACK_CODE | VK_DELETE_CODE) {
            return InteractionKeySignal::DeleteCommand;
        }
        if text_like_key(vk_code) {
            InteractionKeySignal::TextLikeKey
        } else {
            InteractionKeySignal::OtherKey
        }
    }

    fn ctrl_down() -> bool {
        key_down(VK_CONTROL_CODE) || key_down(VK_LCONTROL_CODE) || key_down(VK_RCONTROL_CODE)
    }

    fn key_down(vk_code: u32) -> bool {
        let state = unsafe { GetAsyncKeyState(i32::try_from(vk_code).unwrap_or(0)) };
        (state as u16 & KEY_DOWN_MASK) != 0
    }

    const fn text_like_key(vk_code: u32) -> bool {
        matches!(
            vk_code,
            VK_SPACE_CODE
                | VK_PACKET_CODE
                | 0x30..=0x39
                | 0x41..=0x5a
                | 0x60..=0x6f
                | 0xba..=0xc0
                | 0xdb..=0xdf
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn unicode_sendinput_packet_is_text_like_without_raw_character() {
            assert_eq!(
                key_signal_with_ctrl(VK_PACKET_CODE, false),
                InteractionKeySignal::TextLikeKey
            );
            assert_eq!(
                key_signal_with_ctrl(VK_BACK_CODE, false),
                InteractionKeySignal::DeleteCommand
            );
            assert_eq!(
                key_signal_with_ctrl(VK_Z_CODE, true),
                InteractionKeySignal::UndoCommand
            );
        }

        #[test]
        fn hook_restart_requires_terminal_phase_and_zero_retained_owners() {
            assert!(hook_start_allowed(HookOwnerPhase::Terminal, 0));
            for phase in [
                HookOwnerPhase::Starting,
                HookOwnerPhase::Running,
                HookOwnerPhase::Stopping,
                HookOwnerPhase::Retained,
            ] {
                assert!(!hook_start_allowed(phase, 0), "phase={phase:?}");
            }
            assert!(!hook_start_allowed(HookOwnerPhase::Terminal, 1));
        }

        #[test]
        fn terminal_hook_thread_error_is_not_a_successful_join() {
            let owner = thread::spawn(|| -> HookThreadResult {
                Err("synthetic message-loop failure".to_owned())
            });
            let (terminal, joined, retained, failure) =
                join_thread_until(owner, Duration::from_secs(1));
            assert!(terminal);
            assert!(joined);
            assert!(!retained);
            assert!(
                failure
                    .as_deref()
                    .is_some_and(|detail| detail.contains("synthetic message-loop failure")),
                "failure={failure:?}"
            );
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::{Result, bail};
    use tokio::sync::mpsc;

    use super::{InteractionEvent, InteractionHookReadback, InteractionHookShutdownReport};

    pub struct InteractionHook {
        readback: InteractionHookReadback,
    }

    impl InteractionHook {
        pub fn start(_sender: mpsc::UnboundedSender<InteractionEvent>) -> Result<Self> {
            bail!("interaction cadence hook requires Windows")
        }

        pub const fn readback(&self) -> &InteractionHookReadback {
            &self.readback
        }

        pub fn shutdown_checked(
            self,
            _timeout: std::time::Duration,
            reason: &'static str,
        ) -> InteractionHookShutdownReport {
            InteractionHookShutdownReport {
                reason,
                stop_wake_sent: true,
                sender_cleared: true,
                thread_owner_present: false,
                thread_terminal: true,
                thread_joined: true,
                exact_owner_retained: false,
                failures: Vec::new(),
            }
        }
    }

    pub(super) const fn retained_live_owner_count() -> usize {
        0
    }
}

fn super_now_ts_ns() -> u64 {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    u64::try_from(nanos).unwrap_or(0)
}

#![cfg(windows)]

use std::{
    error::Error,
    io::{self, Write},
    sync::{Arc, Mutex, MutexGuard as StdMutexGuard, OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use synapse_action::{
    ActionBackend, ActionEmitter, EmitState, HELD_KEY_MAX_DURATION_MS,
    backend::software::SoftwareBackend,
};
use synapse_core::{Action, Backend, Key, KeyCode, error_codes};
use tokio::sync::MutexGuard as TokioMutexGuard;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::MakeWriter;
use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, MSG, PostThreadMessageW,
        SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
        WM_SYSKEYDOWN, WM_SYSKEYUP,
    },
};

const VK_A: u32 = 0x41;
const AUTO_RELEASE_BUDGET_MS: i128 = 50;
const AUTO_RELEASE_EARLY_TOLERANCE_MS: i128 = 5;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires native Windows desktop, real keyboard hook, and 30s held-key timer"]
async fn wh_keyboard_ll_observes_auto_release_keyup_after_timer()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let _guard = desktop_input_lock().await;
    let trace_buffer = SharedTraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(trace_buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let hook = KeyboardHook::start()?;
    let key = key_named("a");
    let mut release_guard = SoftwareKeyReleaseGuard::new(key.clone());
    let cancel = CancellationToken::new();
    let (handle, snapshot_handle, emitter) = ActionEmitter::channel();
    let join = tokio::spawn(emitter.run(cancel.clone()));
    let before = snapshot_handle.snapshot().await?;

    println!(
        "readback=WH_KEYBOARD_LL edge=auto_release before={}",
        KeyboardHook::format_timeline()
    );
    handle
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await?;
    let timer_armed_elapsed_ms = KeyboardHook::elapsed_ms();
    let after_key_down = snapshot_handle.snapshot().await?;
    let key_down = hook
        .wait_for(KeyEventReadback::is_a_key_down, Duration::from_secs(2))
        .await
        .ok_or("WH_KEYBOARD_LL did not observe KeyDown(a)")?;
    println!(
        "readback=WH_KEYBOARD_LL edge=auto_release after_key_down={key_down:?} timeline={}",
        KeyboardHook::format_timeline()
    );
    println!(
        "readback=action_emitter_snapshot edge=auto_release after_key_down={after_key_down:?}"
    );

    let expected_expiry_ms =
        i128::try_from(timer_armed_elapsed_ms)? + i128::from(HELD_KEY_MAX_DURATION_MS);
    let earliest_counted_keyup_ms =
        u128::try_from(expected_expiry_ms.saturating_sub(AUTO_RELEASE_EARLY_TOLERANCE_MS))?;
    let key_up = hook
        .wait_for(
            |event| event.is_a_key_up() && event.elapsed_ms >= earliest_counted_keyup_ms,
            Duration::from_millis(HELD_KEY_MAX_DURATION_MS + 5_000),
        )
        .await
        .ok_or("WH_KEYBOARD_LL did not observe auto KeyUp(a)")?;
    let keyup_latency_ms = i128::try_from(key_up.elapsed_ms)? - expected_expiry_ms;
    let after_auto_release = wait_for_empty_snapshot(&snapshot_handle, Duration::from_secs(2))
        .await
        .ok_or("held key snapshot did not empty after auto-release")?;
    release_guard.disarm();

    cancel.cancel();
    let after_cancel = join.await?;
    let log_output = trace_buffer.text();
    let log_line = find_log_line(&log_output, error_codes::STUCK_KEY_AUTO_RELEASED)
        .ok_or("missing STUCK_KEY_AUTO_RELEASED log line")?;

    println!(
        "readback=WH_KEYBOARD_LL edge=auto_release after={key_up:?} expected_expiry_ms={expected_expiry_ms} keyup_latency_ms={keyup_latency_ms} result_value={}",
        KeyboardHook::format_timeline()
    );
    println!(
        "readback=action_emitter_snapshot edge=auto_release before={before:?} after_auto_release={after_auto_release:?} after_cancel={after_cancel:?} result_value=held_keys:{} held_key_timer_count:{}",
        after_auto_release.held_keys.len(),
        after_auto_release.held_key_timer_count
    );
    println!(
        "readback=trace_log edge=auto_release after={log_line} result_value=STUCK_KEY_AUTO_RELEASED"
    );

    assert!(before.held_keys.is_empty());
    assert_eq!(after_key_down.held_keys, vec![key]);
    assert!(after_auto_release.held_keys.is_empty());
    assert_eq!(after_auto_release.held_key_timer_count, 0);
    assert!(
        (-AUTO_RELEASE_EARLY_TOLERANCE_MS..=AUTO_RELEASE_BUDGET_MS).contains(&keyup_latency_ms),
        "expected KeyUp(a) within {AUTO_RELEASE_BUDGET_MS}ms of timer expiry, got {keyup_latency_ms}ms; timeline={}",
        KeyboardHook::format_timeline()
    );
    assert!(log_line.contains("code=STUCK_KEY_AUTO_RELEASED"));
    assert!(log_line.contains("held_ms=30000"));
    assert!(log_line.contains("key=a"));

    Ok(())
}

async fn wait_for_empty_snapshot(
    snapshot_handle: &synapse_action::ActionEmitterSnapshotHandle,
    timeout: Duration,
) -> Option<synapse_action::ActionStateSnapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = snapshot_handle.snapshot().await.ok()?;
        if snapshot.held_keys.is_empty() && snapshot.held_key_timer_count == 0 {
            return Some(snapshot);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn key_named(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

struct SoftwareKeyReleaseGuard {
    key: Key,
    active: bool,
}

impl SoftwareKeyReleaseGuard {
    const fn new(key: Key) -> Self {
        Self { key, active: true }
    }

    const fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for SoftwareKeyReleaseGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let backend = SoftwareBackend::new();
        let mut state = EmitState::new();
        let _ = backend.execute(
            &Action::KeyUp {
                key: self.key.clone(),
                backend: Backend::Software,
            },
            &mut state,
        );
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct KeyEventReadback {
    elapsed_ms: u128,
    vk_code: u32,
    message: u32,
    flags: u32,
}

impl KeyEventReadback {
    const fn is_a_key_down(&self) -> bool {
        self.vk_code == VK_A && (self.message == WM_KEYDOWN || self.message == WM_SYSKEYDOWN)
    }

    const fn is_a_key_up(&self) -> bool {
        self.vk_code == VK_A && (self.message == WM_KEYUP || self.message == WM_SYSKEYUP)
    }
}

#[derive(Default)]
struct HookState {
    start: Option<Instant>,
    events: Vec<KeyEventReadback>,
}

fn hook_state() -> &'static Mutex<HookState> {
    static HOOK_STATE: OnceLock<Mutex<HookState>> = OnceLock::new();
    HOOK_STATE.get_or_init(|| Mutex::new(HookState::default()))
}

struct KeyboardHook {
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

impl KeyboardHook {
    fn start() -> Result<Self, Box<dyn Error + Send + Sync>> {
        {
            let mut state = lock_hook_state();
            state.start = Some(Instant::now());
            state.events.clear();
        }

        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || run_hook_thread(ready_tx));
        let thread_id = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|err| io::Error::other(format!("wait for hook thread readiness: {err}")))?
            .map_err(|err| io::Error::other(format!("start WH_KEYBOARD_LL hook: {err}")))?;
        Ok(Self {
            thread_id,
            thread: Some(thread),
        })
    }

    fn elapsed_ms() -> u128 {
        let state = lock_hook_state();
        state.start.map_or(0, |start| start.elapsed().as_millis())
    }

    fn snapshot() -> Vec<KeyEventReadback> {
        lock_hook_state().events.clone()
    }

    async fn wait_for(
        &self,
        predicate: impl Fn(&KeyEventReadback) -> bool,
        timeout: Duration,
    ) -> Option<KeyEventReadback> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = Self::snapshot().into_iter().find(|event| predicate(event)) {
                return Some(event);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn format_timeline() -> String {
        let snapshot = Self::snapshot();
        if snapshot.is_empty() {
            return "<empty>".to_owned();
        }
        snapshot
            .into_iter()
            .map(|event| {
                format!(
                    "t={}ms vk=0x{:02x} message={} flags=0x{:x}",
                    event.elapsed_ms,
                    event.vk_code,
                    message_label(event.message),
                    event.flags
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let mut state = lock_hook_state();
        state.start = None;
    }
}

fn run_hook_thread(ready_tx: mpsc::Sender<Result<u32, String>>) {
    let thread_id = unsafe { GetCurrentThreadId() };
    let module = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => module,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("GetModuleHandleW failed: {error}")));
            drop(ready_tx);
            return;
        }
    };
    let hook = match unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook_proc),
            Some(HINSTANCE(module.0)),
            0,
        )
    } {
        Ok(hook) => hook,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("SetWindowsHookExW failed: {error}")));
            drop(ready_tx);
            return;
        }
    };
    let mut msg = MSG::default();
    let _hook_guard = HookGuard(hook);
    let _ = ready_tx.send(Ok(thread_id));
    let _ = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
    drop(ready_tx);
}

struct HookGuard(HHOOK);

impl Drop for HookGuard {
    fn drop(&mut self) {
        let _ = unsafe { UnhookWindowsHookEx(self.0) };
    }
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let message = u32::try_from(wparam.0).unwrap_or(0);
        if matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP) {
            let data = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if data.vkCode == VK_A {
                let mut state = lock_hook_state();
                let elapsed_ms = state.start.map_or(0, |start| start.elapsed().as_millis());
                state.events.push(KeyEventReadback {
                    elapsed_ms,
                    vk_code: data.vkCode,
                    message,
                    flags: data.flags.0,
                });
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

const fn message_label(message: u32) -> &'static str {
    match message {
        WM_KEYDOWN => "WM_KEYDOWN",
        WM_KEYUP => "WM_KEYUP",
        WM_SYSKEYDOWN => "WM_SYSKEYDOWN",
        WM_SYSKEYUP => "WM_SYSKEYUP",
        _ => "OTHER",
    }
}

async fn desktop_input_lock() -> TokioMutexGuard<'static, ()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn lock_hook_state() -> StdMutexGuard<'static, HookState> {
    match hook_state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn find_log_line<'a>(logs: &'a str, code: &str) -> Option<&'a str> {
    logs.lines().find(|line| line.contains(code))
}

#[derive(Clone, Default)]
struct SharedTraceBuffer {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedTraceBuffer {
    fn text(&self) -> String {
        let bytes = match self.bytes.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl<'a> MakeWriter<'a> for SharedTraceBuffer {
    type Writer = SharedTraceBufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedTraceBufferWriter {
            bytes: Arc::clone(&self.bytes),
        }
    }
}

struct SharedTraceBufferWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for SharedTraceBufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.bytes.lock() {
            Ok(mut guard) => guard.extend_from_slice(buf),
            Err(poisoned) => poisoned.into_inner().extend_from_slice(buf),
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

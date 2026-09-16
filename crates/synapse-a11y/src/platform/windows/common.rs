use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, TryLockError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use synapse_core::{Rect, UiaPattern, win32_hwnd::hwnd_to_wire};
use uiautomation::{
    UIAutomation, UIElement,
    core::UICacheRequest,
    types::{ControlType, ElementMode, TreeScope, UIProperty},
    variants::{Value, Variant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM},
    System::{
        Com::{
            APTTYPE, APTTYPE_MAINSTA, APTTYPE_MTA, APTTYPE_NA, APTTYPE_STA, APTTYPEQUALIFIER,
            COINIT_MULTITHREADED, CoGetApartmentType, CoInitializeEx, CoUninitialize,
        },
        Threading::GetCurrentThreadId,
    },
    UI::WindowsAndMessaging::EnumThreadWindows,
};
use windows::core::BOOL;

use crate::{
    A11yError, A11yResult, ComApartmentKind, ElementSearchScope, UiaWorkerReadback,
    ids::runtime_id_hex,
};

type UiaJob = Box<dyn FnOnce(&UIAutomation) + Send + 'static>;

static UIA_WORKER: OnceLock<Mutex<Option<Arc<ProcessUiaWorker>>>> = OnceLock::new();
const DEFAULT_UIA_WORKER_JOB_TIMEOUT: Duration = Duration::from_secs(10);
const UIA_WORKER_BUSY_POLL_INTERVAL: Duration = Duration::from_millis(10);
const FALLBACK_RUNTIME_ID_PREFIX: &str = "ffffffff";
const FNV64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV64_PRIME: u64 = 0x0100_0000_01b3;

#[derive(Debug)]
pub(super) struct RuntimeIdHexReadback {
    pub hex: String,
    pub used_fallback: bool,
}

struct ProcessUiaWorker {
    tx: mpsc::Sender<UiaJob>,
    timed_out: Arc<AtomicBool>,
    job_lock: Mutex<()>,
}

fn worker_slot() -> &'static Mutex<Option<Arc<ProcessUiaWorker>>> {
    UIA_WORKER.get_or_init(|| Mutex::new(None))
}

fn lock_worker_slot() -> A11yResult<MutexGuard<'static, Option<Arc<ProcessUiaWorker>>>> {
    worker_slot()
        .lock()
        .map_err(|err| A11yError::internal(err.to_string()))
}

fn worker() -> A11yResult<Arc<ProcessUiaWorker>> {
    let mut guard = lock_worker_slot()?;
    if let Some(worker) = guard.as_ref() {
        if !worker.timed_out.load(Ordering::SeqCst) {
            return Ok(Arc::clone(worker));
        }
        tracing::warn!(
            code = "A11Y_UIA_WORKER_RETIRE_TIMEOUT",
            "retiring timed-out UIA worker before accepting another job"
        );
        *guard = None;
    }

    let worker = Arc::new(start_worker()?);
    *guard = Some(Arc::clone(&worker));
    Ok(worker)
}

fn start_worker() -> A11yResult<ProcessUiaWorker> {
    let (tx, rx) = mpsc::channel::<UiaJob>();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("synapse-a11y-uia-mta".to_owned())
        .spawn(move || uia_worker_thread(rx, ready_tx))
        .map_err(|err| A11yError::internal(format!("spawn UIA worker thread failed: {err}")))?;

    let readback = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|err| A11yError::internal(format!("UIA worker did not initialize: {err}")))??;
    tracing::info!(
        code = "A11Y_UIA_WORKER_READY",
        thread_id = readback.thread_id,
        apartment = ?readback.apartment,
        owned_window_count = readback.owned_window_count,
        "UI Automation worker initialized"
    );
    Ok(ProcessUiaWorker {
        tx,
        timed_out: Arc::new(AtomicBool::new(false)),
        job_lock: Mutex::new(()),
    })
}

#[allow(clippy::needless_pass_by_value)]
fn uia_worker_thread(
    rx: mpsc::Receiver<UiaJob>,
    ready: mpsc::SyncSender<A11yResult<UiaWorkerReadback>>,
) {
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if init.is_err() {
        let _ = ready.send(Err(A11yError::internal(format!(
            "CoInitializeEx(COINIT_MULTITHREADED) failed: {init:?}"
        ))));
        return;
    }

    let automation = match UIAutomation::new_direct().map_err(map_uia_error) {
        Ok(automation) => automation,
        Err(err) => {
            let _ = ready.send(Err(err));
            unsafe {
                CoUninitialize();
            }
            return;
        }
    };

    let readback = read_current_worker_state();
    let _ = ready.send(Ok(readback));

    while let Ok(job) = rx.recv() {
        job(&automation);
    }

    unsafe {
        CoUninitialize();
    }
}

pub(super) fn with_automation<T: Send + 'static>(
    action: impl FnOnce(&UIAutomation) -> A11yResult<T> + Send + 'static,
) -> A11yResult<T> {
    with_automation_operation("uia_worker_job", DEFAULT_UIA_WORKER_JOB_TIMEOUT, action)
}

pub(super) fn with_automation_operation<T: Send + 'static>(
    operation: impl Into<String>,
    timeout: Duration,
    action: impl FnOnce(&UIAutomation) -> A11yResult<T> + Send + 'static,
) -> A11yResult<T> {
    let operation = operation.into();
    let worker = worker()?;
    if worker.timed_out.load(Ordering::SeqCst) {
        retire_worker(&worker, "pre_send_timeout");
        return Err(A11yError::uia_worker_timeout(format!(
            "operation={operation} phase=worker_unavailable previous_timeout=true timeout_ms={} remediation=retry; previous UIA worker job exceeded its bounded deadline and this worker was retired so the next request can use a fresh worker",
            timeout.as_millis()
        )));
    }

    let started = Instant::now();
    let _job_guard = acquire_job_slot(&worker, &operation, timeout, started)?;
    if worker.timed_out.load(Ordering::SeqCst) {
        retire_worker(&worker, "post_lock_timeout");
        return Err(A11yError::uia_worker_timeout(format!(
            "operation={operation} phase=worker_unavailable previous_timeout=true timeout_ms={} elapsed_ms={} remediation=retry; previous UIA worker job exceeded its bounded deadline and this worker was retired so the next request can use a fresh worker",
            timeout.as_millis(),
            started.elapsed().as_millis()
        )));
    }
    let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
        return Err(A11yError::uia_worker_timeout(format!(
            "operation={operation} phase=worker_busy timeout_ms={} elapsed_ms={} remediation=retry; UIA worker was already running a bounded job and this operation was not queued behind it",
            timeout.as_millis(),
            started.elapsed().as_millis()
        )));
    };

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    worker
        .tx
        .send(Box::new(move |automation| {
            let _ = reply_tx.send(action(automation));
        }))
        .map_err(|err| {
            retire_worker(&worker, "send_failed");
            A11yError::internal(format!("send UIA worker job failed: {err}"))
        })?;
    match reply_rx.recv_timeout(remaining) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            worker.timed_out.store(true, Ordering::SeqCst);
            retire_worker(&worker, "reply_timeout");
            Err(A11yError::uia_worker_timeout(format!(
                "operation={operation} phase=worker_reply_recv timeout_ms={} elapsed_ms={} remediation=retry; UIA provider call did not return before the bounded worker deadline, daemon stayed alive, and the worker was retired so unrelated later requests can use a fresh worker",
                timeout.as_millis(),
                started.elapsed().as_millis()
            )))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            retire_worker(&worker, "reply_disconnected");
            Err(A11yError::internal(format!(
                "receive UIA worker job failed for operation={operation}: channel disconnected"
            )))
        }
    }
}

fn acquire_job_slot<'a>(
    worker: &'a ProcessUiaWorker,
    operation: &str,
    timeout: Duration,
    started: Instant,
) -> A11yResult<MutexGuard<'a, ()>> {
    loop {
        if worker.timed_out.load(Ordering::SeqCst) {
            return Err(A11yError::uia_worker_timeout(format!(
                "operation={operation} phase=worker_unavailable previous_timeout=true timeout_ms={} elapsed_ms={} remediation=retry; previous UIA worker job exceeded its bounded deadline and this worker was retired so the next request can use a fresh worker",
                timeout.as_millis(),
                started.elapsed().as_millis()
            )));
        }
        match worker.job_lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(err)) => {
                return Err(A11yError::internal(format!(
                    "UIA worker job lock poisoned for operation={operation}: {err}"
                )));
            }
            Err(TryLockError::WouldBlock) => {
                let elapsed = started.elapsed();
                let Some(remaining) = timeout.checked_sub(elapsed) else {
                    return Err(A11yError::uia_worker_timeout(format!(
                        "operation={operation} phase=worker_busy timeout_ms={} elapsed_ms={} remediation=retry; UIA worker was already running a bounded job and this operation was not queued behind it",
                        timeout.as_millis(),
                        elapsed.as_millis()
                    )));
                };
                thread::sleep(remaining.min(UIA_WORKER_BUSY_POLL_INTERVAL));
            }
        }
    }
}

fn retire_worker(worker: &Arc<ProcessUiaWorker>, reason: &'static str) {
    match lock_worker_slot() {
        Ok(mut guard) => {
            if guard
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, worker))
            {
                *guard = None;
                tracing::warn!(
                    code = "A11Y_UIA_WORKER_RETIRED",
                    reason,
                    "retired UIA worker; a later UIA request will start a fresh worker"
                );
            }
        }
        Err(error) => {
            tracing::error!(
                code = "A11Y_UIA_WORKER_RETIRE_FAILED",
                reason,
                detail = %error,
                "could not lock UIA worker slot for retirement"
            );
        }
    }
}

pub(super) fn worker_readback() -> A11yResult<UiaWorkerReadback> {
    with_automation(|_automation| Ok(read_current_worker_state()))
}

fn read_current_worker_state() -> UiaWorkerReadback {
    let thread_id = unsafe { GetCurrentThreadId() };
    UiaWorkerReadback {
        thread_id,
        apartment: read_current_apartment(),
        owned_window_count: owned_window_count(thread_id),
    }
}

fn read_current_apartment() -> ComApartmentKind {
    let mut apartment = APTTYPE::default();
    let mut qualifier = APTTYPEQUALIFIER::default();
    if unsafe { CoGetApartmentType(&raw mut apartment, &raw mut qualifier) }.is_err() {
        return ComApartmentKind::Unknown;
    }
    match apartment {
        value if value == APTTYPE_STA => ComApartmentKind::Sta,
        value if value == APTTYPE_MTA => ComApartmentKind::Mta,
        value if value == APTTYPE_NA => ComApartmentKind::Neutral,
        value if value == APTTYPE_MAINSTA => ComApartmentKind::MainSta,
        _ => ComApartmentKind::Unknown,
    }
}

fn owned_window_count(thread_id: u32) -> usize {
    unsafe extern "system" fn enum_window(_hwnd: HWND, lparam: LPARAM) -> BOOL {
        let count = unsafe { &mut *(lparam.0 as *mut usize) };
        *count = count.saturating_add(1);
        BOOL(1)
    }

    let mut count = 0_usize;
    let _ = unsafe {
        EnumThreadWindows(
            thread_id,
            Some(enum_window),
            LPARAM((&raw mut count).cast::<c_void>() as isize),
        )
    };
    count
}

pub(super) fn create_cache_request(
    automation: &UIAutomation,
    depth: u32,
    element_mode: ElementMode,
    tree_view: TreeView,
) -> A11yResult<UICacheRequest> {
    let cache = automation.create_cache_request().map_err(map_uia_error)?;
    for property in [
        UIProperty::RuntimeId,
        UIProperty::BoundingRectangle,
        UIProperty::ProcessId,
        UIProperty::ControlType,
        UIProperty::LocalizedControlType,
        UIProperty::Name,
        UIProperty::HasKeyboardFocus,
        UIProperty::IsKeyboardFocusable,
        UIProperty::IsEnabled,
        UIProperty::AutomationId,
        UIProperty::ClassName,
        UIProperty::IsControlElement,
        UIProperty::IsContentElement,
        UIProperty::IsPassword,
        UIProperty::NativeWindowHandle,
        UIProperty::IsInvokePatternAvailable,
        UIProperty::IsTogglePatternAvailable,
        UIProperty::IsValuePatternAvailable,
        UIProperty::IsSelectionPatternAvailable,
        UIProperty::IsSelectionItemPatternAvailable,
        UIProperty::IsExpandCollapsePatternAvailable,
        UIProperty::IsLegacyIAccessiblePatternAvailable,
        UIProperty::IsScrollPatternAvailable,
        UIProperty::IsScrollItemPatternAvailable,
        UIProperty::IsTextPatternAvailable,
        UIProperty::IsWindowPatternAvailable,
        UIProperty::IsTransformPatternAvailable,
        UIProperty::IsRangeValuePatternAvailable,
        UIProperty::ValueValue,
        UIProperty::RangeValueValue,
    ] {
        cache.add_property(property).map_err(map_uia_error)?;
    }
    let tree_filter = match tree_view {
        TreeView::Control => automation.get_control_view_condition(),
        TreeView::Raw => automation.create_true_condition(),
    }
    .map_err(map_uia_error)?;
    cache.set_tree_filter(tree_filter).map_err(map_uia_error)?;
    let scope = if depth == 0 {
        TreeScope::Element
    } else {
        TreeScope::Subtree
    };
    cache.set_tree_scope(scope).map_err(map_uia_error)?;
    cache
        .set_element_mode(element_mode)
        .map_err(map_uia_error)?;
    Ok(cache)
}
pub(super) fn cached_patterns(element: &UIElement) -> Vec<UiaPattern> {
    let mut patterns = Vec::new();
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsInvokePatternAvailable,
        UiaPattern::Invoke,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsTogglePatternAvailable,
        UiaPattern::Toggle,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsValuePatternAvailable,
        UiaPattern::Value,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsSelectionPatternAvailable,
        UiaPattern::Selection,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsSelectionItemPatternAvailable,
        UiaPattern::SelectionItem,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsExpandCollapsePatternAvailable,
        UiaPattern::ExpandCollapse,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsLegacyIAccessiblePatternAvailable,
        UiaPattern::LegacyIAccessible,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsScrollPatternAvailable,
        UiaPattern::Scroll,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsScrollItemPatternAvailable,
        UiaPattern::ScrollItem,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsTextPatternAvailable,
        UiaPattern::Text,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsWindowPatternAvailable,
        UiaPattern::Window,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsTransformPatternAvailable,
        UiaPattern::Transform,
    );
    push_pattern(
        element,
        &mut patterns,
        UIProperty::IsRangeValuePatternAvailable,
        UiaPattern::RangeValue,
    );
    patterns
}

fn push_pattern(
    element: &UIElement,
    patterns: &mut Vec<UiaPattern>,
    property: UIProperty,
    pattern: UiaPattern,
) {
    if cached_bool(element, property) {
        patterns.push(pattern);
    }
}

pub(super) fn cached_bool(element: &UIElement, property: UIProperty) -> bool {
    element
        .get_cached_property_value(property)
        .ok()
        .and_then(|variant| <&Variant as TryInto<bool>>::try_into(&variant).ok())
        .unwrap_or(false)
}

/// On-screen value source-of-truth: prefer the `ValuePattern` string, then the
/// `RangeValuePattern` number. Returns `None` when neither pattern is exposed or
/// the value is empty, so `AccessibleNode.value` stays absent rather than blank.
pub(super) fn cached_value(element: &UIElement) -> Option<String> {
    if cached_bool(element, UIProperty::IsValuePatternAvailable)
        && let Ok(variant) = element.get_cached_property_value(UIProperty::ValueValue)
        && let Ok(text) = variant.get_string()
        && !text.is_empty()
    {
        return Some(text);
    }
    if cached_bool(element, UIProperty::IsRangeValuePatternAvailable)
        && let Ok(variant) = element.get_cached_property_value(UIProperty::RangeValueValue)
        && let Ok(Value::R8(number)) = variant.get_value()
    {
        return Some(format_range_value(number));
    }
    None
}

fn format_range_value(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{number:.0}")
    } else {
        format!("{number}")
    }
}

pub(super) fn cached_rect(element: &UIElement) -> Rect {
    element.get_cached_bounding_rectangle().map_or(
        Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        |rect| Rect {
            x: rect.get_left(),
            y: rect.get_top(),
            w: rect.get_right().saturating_sub(rect.get_left()),
            h: rect.get_bottom().saturating_sub(rect.get_top()),
        },
    )
}

pub(super) fn cached_role(element: &UIElement) -> String {
    let localized = element
        .get_cached_localized_control_type()
        .unwrap_or_default();
    if !localized.is_empty() {
        return localized;
    }

    element
        .get_cached_control_type()
        .map_or_else(|_err| "unknown".to_owned(), control_type_name)
}

fn control_type_name(control_type: ControlType) -> String {
    format!("{control_type:?}")
}

pub(super) fn cached_hwnd(element: &UIElement) -> Option<i64> {
    let handle = element.get_cached_native_window_handle().ok()?;
    let raw: isize = handle.into();
    Some(hwnd_to_wire(raw))
}

pub(super) fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

pub(super) const fn pattern_property(pattern: UiaPattern) -> UIProperty {
    match pattern {
        UiaPattern::Invoke => UIProperty::IsInvokePatternAvailable,
        UiaPattern::Toggle => UIProperty::IsTogglePatternAvailable,
        UiaPattern::Value => UIProperty::IsValuePatternAvailable,
        UiaPattern::Selection => UIProperty::IsSelectionPatternAvailable,
        UiaPattern::SelectionItem => UIProperty::IsSelectionItemPatternAvailable,
        UiaPattern::ExpandCollapse => UIProperty::IsExpandCollapsePatternAvailable,
        UiaPattern::LegacyIAccessible => UIProperty::IsLegacyIAccessiblePatternAvailable,
        UiaPattern::Scroll => UIProperty::IsScrollPatternAvailable,
        UiaPattern::ScrollItem => UIProperty::IsScrollItemPatternAvailable,
        UiaPattern::Text => UIProperty::IsTextPatternAvailable,
        UiaPattern::Window => UIProperty::IsWindowPatternAvailable,
        UiaPattern::Transform => UIProperty::IsTransformPatternAvailable,
        UiaPattern::RangeValue => UIProperty::IsRangeValuePatternAvailable,
    }
}

impl From<ElementSearchScope> for TreeScope {
    fn from(scope: ElementSearchScope) -> Self {
        match scope {
            ElementSearchScope::Children => Self::Children,
            ElementSearchScope::Descendants => Self::Descendants,
            ElementSearchScope::Subtree => Self::Subtree,
        }
    }
}
#[derive(Clone, Copy)]
pub(super) enum TreeView {
    Control,
    Raw,
}

pub(super) fn cached_runtime_id(element: &UIElement) -> A11yResult<Vec<i32>> {
    let value = element
        .get_cached_property_value(UIProperty::RuntimeId)
        .map_err(map_uia_error)?;
    match value.get_value().map_err(map_uia_error)? {
        Value::ArrayI4(items) => Ok(items),
        Value::SAFEARRAY(array) => array.try_into().map_err(map_uia_error),
        other => Err(A11yError::internal(format!(
            "cached RuntimeId had unexpected type {other}"
        ))),
    }
}

pub(super) fn cached_runtime_id_hex_or_fallback(
    element: &UIElement,
    hwnd: i64,
) -> A11yResult<RuntimeIdHexReadback> {
    match cached_runtime_id(element) {
        Ok(runtime_id) => Ok(RuntimeIdHexReadback {
            hex: runtime_id_hex(&runtime_id),
            used_fallback: false,
        }),
        Err(error) if runtime_id_is_unavailable(&error) => Ok(RuntimeIdHexReadback {
            hex: fallback_runtime_id_hex(element, hwnd),
            used_fallback: true,
        }),
        Err(error) => Err(error),
    }
}

pub(super) fn runtime_id_is_unavailable(error: &A11yError) -> bool {
    matches!(
        error,
        A11yError::Internal { detail } if detail.contains("cached RuntimeId had unexpected type EMPTY")
    )
}

pub(super) fn fallback_runtime_id_hex(element: &UIElement, hwnd: i64) -> String {
    let rect = cached_rect(element);
    let mut hash = FNV64_OFFSET_BASIS;
    hash_bytes(&mut hash, &hwnd.to_le_bytes());
    hash_bytes(
        &mut hash,
        &element.get_cached_process_id().unwrap_or(-1).to_le_bytes(),
    );
    hash_bytes(
        &mut hash,
        format!("{:?}", element.get_cached_control_type().ok()).as_bytes(),
    );
    hash_bytes(
        &mut hash,
        element
            .get_cached_classname()
            .unwrap_or_default()
            .as_bytes(),
    );
    hash_bytes(
        &mut hash,
        element
            .get_cached_automation_id()
            .unwrap_or_default()
            .as_bytes(),
    );
    hash_bytes(
        &mut hash,
        element.get_cached_name().unwrap_or_default().as_bytes(),
    );
    hash_bytes(&mut hash, &rect.x.to_le_bytes());
    hash_bytes(&mut hash, &rect.y.to_le_bytes());
    hash_bytes(&mut hash, &rect.w.to_le_bytes());
    hash_bytes(&mut hash, &rect.h.to_le_bytes());
    format!("{FALLBACK_RUNTIME_ID_PREFIX}{hash:016x}")
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV64_PRIME);
    }
}

#[allow(clippy::missing_const_for_fn)]
#[allow(clippy::needless_pass_by_value)]
pub(super) fn map_uia_error(err: uiautomation::Error) -> A11yError {
    A11yError::internal(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_worker(timed_out: bool) -> ProcessUiaWorker {
        let (tx, _rx) = mpsc::channel::<UiaJob>();
        ProcessUiaWorker {
            tx,
            timed_out: Arc::new(AtomicBool::new(timed_out)),
            job_lock: Mutex::new(()),
        }
    }

    #[test]
    fn acquire_job_slot_reports_busy_instead_of_queueing() {
        let worker = dummy_worker(false);
        let _held = worker
            .job_lock
            .lock()
            .unwrap_or_else(|error| panic!("test worker lock poisoned: {error}"));

        let result = acquire_job_slot(
            &worker,
            "test_busy",
            Duration::from_millis(1),
            Instant::now(),
        );

        let Err(error) = result else {
            panic!("busy worker should reject the second queued job");
        };
        assert_eq!(
            error.code(),
            synapse_core::error_codes::A11Y_UIA_WORKER_TIMEOUT
        );
        assert!(error.to_string().contains("phase=worker_busy"));
    }

    #[test]
    fn acquire_job_slot_reports_unavailable_after_timeout_flag() {
        let worker = dummy_worker(true);

        let result = acquire_job_slot(
            &worker,
            "test_timeout",
            Duration::from_millis(100),
            Instant::now(),
        );

        let Err(error) = result else {
            panic!("timed-out worker should reject new jobs");
        };
        assert_eq!(
            error.code(),
            synapse_core::error_codes::A11Y_UIA_WORKER_TIMEOUT
        );
        assert!(error.to_string().contains("phase=worker_unavailable"));
    }
}

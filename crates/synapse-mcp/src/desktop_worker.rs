use std::{
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::ValueEnum;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use synapse_core::{AccessibleSubtree, ForegroundContext, Rect, error_codes};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum DesktopWorkerOp {
    Context,
    Snapshot,
    Capture,
}

impl DesktopWorkerOp {
    const fn as_arg(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Snapshot => "snapshot",
            Self::Capture => "capture",
        }
    }
}

#[derive(Debug)]
pub(crate) struct DesktopWorkerCli {
    pub op: Option<DesktopWorkerOp>,
    pub hwnd: Option<i64>,
    pub region: Option<String>,
    pub client_region: bool,
    pub depth: Option<u32>,
    pub json_path: Option<PathBuf>,
    pub bgra_path: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct HiddenDesktopCapture {
    pub context: ForegroundContext,
    pub bitmap: synapse_capture::CapturedBgraBitmap,
    pub capture_backend: &'static str,
    pub capture_region: Rect,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HiddenDesktopSnapshot {
    pub context: ForegroundContext,
    pub tree: AccessibleSubtree,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkerEnvelope {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<WorkerPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerPayload {
    Context {
        context: ForegroundContext,
    },
    Snapshot {
        context: ForegroundContext,
        tree: AccessibleSubtree,
    },
    Capture {
        context: ForegroundContext,
        region: Rect,
        width: u32,
        height: u32,
        capture_backend: String,
        bgra_bytes: u64,
    },
}

#[cfg(windows)]
pub(crate) fn run_worker_from_cli(args: DesktopWorkerCli) -> anyhow::Result<ExitCode> {
    let json_path = args
        .json_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("--desktop-worker-json is required"))?;
    let envelope = match run_worker_operation(&args) {
        Ok(payload) => WorkerEnvelope {
            ok: true,
            payload: Some(payload),
            error_code: None,
            error_detail: None,
        },
        Err(error) => error,
    };
    write_worker_envelope(&json_path, &envelope)?;
    Ok(if envelope.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(not(windows))]
pub(crate) fn run_worker_from_cli(_args: DesktopWorkerCli) -> anyhow::Result<ExitCode> {
    anyhow::bail!("desktop-worker mode is only supported on Windows")
}

#[cfg(windows)]
fn run_worker_operation(args: &DesktopWorkerCli) -> Result<WorkerPayload, WorkerEnvelope> {
    let op = args.op.ok_or_else(|| worker_param_error("missing_op"))?;
    let hwnd = args
        .hwnd
        .ok_or_else(|| worker_param_error("missing_hwnd"))?;
    synapse_capture::init_process_dpi_awareness()
        .map_err(|error| worker_error(error.code(), error.to_string()))?;
    match op {
        DesktopWorkerOp::Context => {
            worker_context(hwnd).map(|context| WorkerPayload::Context { context })
        }
        DesktopWorkerOp::Snapshot => {
            let depth = args.depth.unwrap_or(2).min(16);
            let context = worker_context(hwnd)?;
            let tree = synapse_a11y::snapshot_window_from_hwnd(hwnd, depth)
                .map_err(|error| worker_error(error.code(), error.to_string()))?;
            Ok(WorkerPayload::Snapshot { context, tree })
        }
        DesktopWorkerOp::Capture => {
            let bgra_path = args
                .bgra_path
                .as_ref()
                .ok_or_else(|| worker_param_error("missing_bgra_path"))?;
            let requested_region = args.region.as_deref().map(parse_rect).transpose()?;
            let context = worker_context(hwnd)?;
            let region = worker_capture_region(hwnd, requested_region, args.client_region)?;
            let captured = synapse_capture::window_region_to_bgra_bitmap_printwindow(hwnd, region)
                .map_err(|error| worker_error(error.code(), error.to_string()))?;
            fs::write(bgra_path, &captured.bitmap.bytes).map_err(|error| {
                worker_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!("desktop worker could not write BGRA bytes: {error}"),
                )
            })?;
            Ok(WorkerPayload::Capture {
                context,
                region: captured.bitmap.region,
                width: captured.bitmap.width,
                height: captured.bitmap.height,
                capture_backend: captured.capture_backend.to_owned(),
                bgra_bytes: captured.bitmap.bytes.len() as u64,
            })
        }
    }
}

#[cfg(windows)]
fn worker_context(hwnd: i64) -> Result<ForegroundContext, WorkerEnvelope> {
    synapse_capture::validate_hwnd(hwnd).map_err(|error| {
        worker_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!("hidden desktop hwnd {hwnd:#x} is not a live window: {error}"),
        )
    })?;
    synapse_a11y::foreground_context(hwnd)
        .map_err(|error| worker_error(error.code(), error.to_string()))
}

#[cfg(windows)]
fn worker_capture_region(
    hwnd: i64,
    region: Option<Rect>,
    client_region: bool,
) -> Result<Rect, WorkerEnvelope> {
    let Some(region) = region else {
        return synapse_capture::window_capture_region(hwnd)
            .map_err(|error| worker_error(error.code(), error.to_string()));
    };
    if client_region {
        return synapse_capture::client_region_to_window_region(hwnd, region)
            .map_err(|error| worker_error(error.code(), error.to_string()));
    }
    Ok(region)
}

#[cfg(windows)]
fn parse_rect(raw: &str) -> Result<Rect, WorkerEnvelope> {
    let parts = raw
        .split(',')
        .map(str::trim)
        .map(str::parse::<i32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            worker_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("invalid --desktop-worker-region {raw:?}: {error}"),
            )
        })?;
    if parts.len() != 4 {
        return Err(worker_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("--desktop-worker-region must be x,y,w,h: {raw:?}"),
        ));
    }
    let rect = Rect {
        x: parts[0],
        y: parts[1],
        w: parts[2],
        h: parts[3],
    };
    if rect.w <= 0 || rect.h <= 0 {
        return Err(worker_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("empty desktop worker capture region {rect:?}"),
        ));
    }
    Ok(rect)
}

fn worker_param_error(reason: &str) -> WorkerEnvelope {
    worker_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("desktop worker parameter error: {reason}"),
    )
}

fn worker_error(code: &'static str, detail: impl Into<String>) -> WorkerEnvelope {
    WorkerEnvelope {
        ok: false,
        payload: None,
        error_code: Some(code.to_owned()),
        error_detail: Some(detail.into()),
    }
}

fn write_worker_envelope(path: &Path, envelope: &WorkerEnvelope) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_vec_pretty(envelope)?;
    fs::write(path, encoded)?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn hidden_desktop_window_context(
    desktop_name: &str,
    hwnd: i64,
) -> Result<ForegroundContext, rmcp::ErrorData> {
    match run_worker(
        desktop_name,
        DesktopWorkerOp::Context,
        hwnd,
        None,
        false,
        None,
    )? {
        WorkerPayload::Context { context } => Ok(context),
        payload => Err(crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("desktop worker returned unexpected context payload: {payload:?}"),
        )),
    }
}

#[cfg(not(windows))]
pub(crate) fn hidden_desktop_window_context(
    _desktop_name: &str,
    _hwnd: i64,
) -> Result<ForegroundContext, rmcp::ErrorData> {
    Err(crate::m1::mcp_error(
        error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "hidden desktop workers are only supported on Windows",
    ))
}

#[cfg(windows)]
pub(crate) fn hidden_desktop_window_snapshot(
    desktop_name: &str,
    hwnd: i64,
    depth: u32,
) -> Result<HiddenDesktopSnapshot, rmcp::ErrorData> {
    match run_worker(
        desktop_name,
        DesktopWorkerOp::Snapshot,
        hwnd,
        None,
        false,
        Some(depth),
    )? {
        WorkerPayload::Snapshot { context, tree } => Ok(HiddenDesktopSnapshot { context, tree }),
        payload => Err(crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("desktop worker returned unexpected snapshot payload: {payload:?}"),
        )),
    }
}

#[cfg(windows)]
pub(crate) fn hidden_desktop_window_hwnds(desktop_name: &str) -> Result<Vec<i64>, rmcp::ErrorData> {
    use windows::{
        Win32::{
            Foundation::{LPARAM, SetLastError, WIN32_ERROR},
            System::StationsAndDesktops::{
                CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_ENUMERATE, DESKTOP_READ_CONTROL,
                DESKTOP_READOBJECTS, EnumDesktopWindows, OpenDesktopW,
            },
        },
        core::{BOOL, PCWSTR},
    };

    struct Search {
        hwnds: Vec<i64>,
    }

    unsafe extern "system" fn enum_window(
        hwnd: windows::Win32::Foundation::HWND,
        lparam: LPARAM,
    ) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        search
            .hwnds
            .push(synapse_core::win32_hwnd::hwnd_to_wire(hwnd.0 as isize));
        BOOL(1)
    }

    let desktop_wide = wide_null(desktop_name);
    let access = DESKTOP_ENUMERATE.0 | DESKTOP_READOBJECTS.0 | DESKTOP_READ_CONTROL.0;
    let handle = unsafe {
        OpenDesktopW(
            PCWSTR(desktop_wide.as_ptr()),
            DESKTOP_CONTROL_FLAGS::default(),
            false,
            access,
        )
    }
    .map_err(|error| {
        crate::m1::mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("hidden desktop HWND enumeration failed to open {desktop_name:?}: {error}"),
        )
    })?;

    let mut search = Search { hwnds: Vec::new() };
    unsafe {
        SetLastError(WIN32_ERROR(0));
    }
    let enum_result = unsafe {
        EnumDesktopWindows(
            Some(handle),
            Some(enum_window),
            LPARAM((&raw mut search).cast::<core::ffi::c_void>() as isize),
        )
    };
    let close_result = unsafe { CloseDesktop(handle) };
    if let Err(error) = close_result {
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("hidden desktop HWND enumeration failed to close {desktop_name:?}: {error}"),
        ));
    }
    if let Err(error) = enum_result {
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("EnumDesktopWindows failed for hidden desktop {desktop_name:?}: {error}"),
        ));
    }
    search.hwnds.sort_unstable();
    search.hwnds.dedup();
    Ok(search.hwnds)
}

#[cfg(not(windows))]
pub(crate) fn hidden_desktop_window_hwnds(
    _desktop_name: &str,
) -> Result<Vec<i64>, rmcp::ErrorData> {
    Err(crate::m1::mcp_error(
        error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "hidden desktop HWND enumeration is only supported on Windows",
    ))
}

#[cfg(not(windows))]
pub(crate) fn hidden_desktop_window_snapshot(
    _desktop_name: &str,
    _hwnd: i64,
    _depth: u32,
) -> Result<HiddenDesktopSnapshot, rmcp::ErrorData> {
    Err(crate::m1::mcp_error(
        error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "hidden desktop workers are only supported on Windows",
    ))
}

#[cfg(windows)]
pub(crate) fn hidden_desktop_window_capture(
    desktop_name: &str,
    hwnd: i64,
    region: Option<Rect>,
    client_region: bool,
) -> Result<HiddenDesktopCapture, rmcp::ErrorData> {
    crate::m1::validate_window_hwnd_shape("hidden_desktop_worker", hwnd)?;
    let mut temp = WorkerTempPaths::new(true)?;
    let result = (|| {
        let payload = run_worker_with_paths(
            desktop_name,
            DesktopWorkerOp::Capture,
            hwnd,
            region,
            client_region,
            None,
            &temp,
        )?;
        let WorkerPayload::Capture {
            context,
            region,
            width,
            height,
            capture_backend,
            bgra_bytes,
        } = payload
        else {
            return Err(crate::m1::mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("desktop worker returned unexpected capture payload: {payload:?}"),
            ));
        };
        let bgra_path = temp.bgra_path.as_ref().ok_or_else(|| {
            crate::m1::mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "desktop worker capture temp path was not allocated",
            )
        })?;
        let bytes = fs::read(bgra_path).map_err(|error| {
            crate::m1::mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "desktop worker BGRA readback failed for {}: {error}",
                    bgra_path.display()
                ),
            )
        })?;
        if bytes.len() as u64 != bgra_bytes {
            return Err(crate::m1::mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "desktop worker BGRA length mismatch: metadata={bgra_bytes} actual={}",
                    bytes.len()
                ),
            ));
        }
        let capture_backend = match capture_backend.as_str() {
            "printwindow" => "printwindow",
            other => {
                return Err(crate::m1::mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("desktop worker returned unsupported capture backend {other:?}"),
                ));
            }
        };
        Ok(HiddenDesktopCapture {
            context,
            bitmap: synapse_capture::CapturedBgraBitmap {
                region,
                width,
                height,
                bytes,
            },
            capture_backend,
            capture_region: region,
        })
    })();
    finish_worker_temp_cleanup(result, &mut temp)
}

#[cfg(not(windows))]
pub(crate) fn hidden_desktop_window_capture(
    _desktop_name: &str,
    _hwnd: i64,
    _region: Option<Rect>,
    _client_region: bool,
) -> Result<HiddenDesktopCapture, rmcp::ErrorData> {
    Err(crate::m1::mcp_error(
        error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "hidden desktop workers are only supported on Windows",
    ))
}

#[cfg(windows)]
fn run_worker(
    desktop_name: &str,
    op: DesktopWorkerOp,
    hwnd: i64,
    region: Option<Rect>,
    client_region: bool,
    depth: Option<u32>,
) -> Result<WorkerPayload, rmcp::ErrorData> {
    crate::m1::validate_window_hwnd_shape("hidden_desktop_worker", hwnd)?;
    let mut temp = WorkerTempPaths::new(matches!(op, DesktopWorkerOp::Capture))?;
    let result = run_worker_with_paths(desktop_name, op, hwnd, region, client_region, depth, &temp);
    finish_worker_temp_cleanup(result, &mut temp)
}

#[cfg(windows)]
fn run_worker_with_paths(
    desktop_name: &str,
    op: DesktopWorkerOp,
    hwnd: i64,
    region: Option<Rect>,
    client_region: bool,
    depth: Option<u32>,
    temp: &WorkerTempPaths,
) -> Result<WorkerPayload, rmcp::ErrorData> {
    crate::m1::validate_window_hwnd_shape("hidden_desktop_worker", hwnd)?;
    let process_verdict =
        launch_worker_process(desktop_name, op, hwnd, region, client_region, depth, temp)?;
    let envelope: WorkerEnvelope = read_json(&temp.json_path)?;
    validate_worker_exit_envelope(process_verdict, &envelope)?;
    if !envelope.ok {
        let code = envelope
            .error_code
            .as_deref()
            .map(worker_code_static)
            .unwrap_or(error_codes::TOOL_INTERNAL_ERROR);
        return Err(crate::m1::mcp_error(
            code,
            envelope
                .error_detail
                .unwrap_or_else(|| "desktop worker failed without detail".to_owned()),
        ));
    }
    envelope.payload.ok_or_else(|| {
        crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "desktop worker succeeded without payload",
        )
    })
}

#[cfg(windows)]
const WORKER_TERMINATION_READBACK_TIMEOUT_MS: u32 = 5_000;

#[cfg(windows)]
const STILL_ACTIVE_EXIT_CODE: u32 = 259;

#[cfg(windows)]
struct WorkerProcessHandles {
    process: Option<windows::Win32::Foundation::HANDLE>,
    thread: Option<windows::Win32::Foundation::HANDLE>,
    job: Option<windows::Win32::Foundation::HANDLE>,
    job_assigned: bool,
    terminal_verified: bool,
    terminal_exit_code: Option<u32>,
    child_created: bool,
    retained_owner_id: Option<u64>,
    pid: u32,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Fields are intentionally consumed through fail-closed diagnostic Debug output.
pub(crate) struct DesktopWorkerRetainedOwner {
    // Windows HANDLE is !Send in the bindings. Raw values are safe here
    // because kernel handles are process-wide; the registry remains their
    // exact logical owner and reconstructs them only for checked Win32 calls.
    pub(crate) owner_id: u64,
    pub(crate) pid: u32,
    pub(crate) process_handle: Option<isize>,
    pub(crate) thread_handle: Option<isize>,
    pub(crate) job_handle: Option<isize>,
    pub(crate) job_assigned: bool,
    pub(crate) terminal_verified: bool,
    pub(crate) terminal_exit_code: Option<u32>,
    pub(crate) child_created: bool,
    pub(crate) last_failure: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Append-only evidence is emitted in shutdown diagnostics and inspected in tests.
pub(crate) struct DesktopWorkerRetainedOwnerEvidence {
    pub(crate) sequence: u64,
    pub(crate) event: &'static str,
    pub(crate) owner: DesktopWorkerRetainedOwner,
    pub(crate) detail: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Non-count fields preserve exact owner evidence for fatal shutdown reports.
pub(crate) struct DesktopWorkerRetainedOwnerReport {
    pub(crate) active_owner_count: usize,
    pub(crate) reap_in_progress_count: usize,
    pub(crate) active_owners: Vec<DesktopWorkerRetainedOwner>,
    pub(crate) evidence: Vec<DesktopWorkerRetainedOwnerEvidence>,
}

#[cfg(windows)]
fn retained_worker_process_handles() -> &'static std::sync::Mutex<Vec<DesktopWorkerRetainedOwner>> {
    static RETAINED: std::sync::OnceLock<std::sync::Mutex<Vec<DesktopWorkerRetainedOwner>>> =
        std::sync::OnceLock::new();
    RETAINED.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(windows)]
fn lock_retained_worker_process_handles()
-> std::sync::MutexGuard<'static, Vec<DesktopWorkerRetainedOwner>> {
    retained_worker_process_handles()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(windows)]
fn retained_worker_reaps_in_progress() -> &'static std::sync::atomic::AtomicUsize {
    static IN_PROGRESS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    &IN_PROGRESS
}

#[cfg(windows)]
struct RetainedWorkerReapGuard;

#[cfg(windows)]
impl Drop for RetainedWorkerReapGuard {
    fn drop(&mut self) {
        retained_worker_reaps_in_progress().fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(windows)]
fn take_retained_worker_owner_for_retry()
-> Option<(DesktopWorkerRetainedOwner, RetainedWorkerReapGuard)> {
    // Increment while holding the same registry lock used by the shutdown
    // readback. Thus an observer sees either the retained registry entry or
    // the in-progress owner, never a false zero between those two states.
    let mut owners = lock_retained_worker_process_handles();
    let owner = owners.pop()?;
    retained_worker_reaps_in_progress().fetch_add(1, std::sync::atomic::Ordering::Release);
    Some((owner, RetainedWorkerReapGuard))
}

#[cfg(windows)]
fn retained_worker_owner_evidence()
-> &'static std::sync::Mutex<Vec<DesktopWorkerRetainedOwnerEvidence>> {
    static EVIDENCE: std::sync::OnceLock<
        std::sync::Mutex<Vec<DesktopWorkerRetainedOwnerEvidence>>,
    > = std::sync::OnceLock::new();
    EVIDENCE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(windows)]
fn append_retained_worker_owner_evidence(
    event: &'static str,
    owner: DesktopWorkerRetainedOwner,
    detail: String,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_EVIDENCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT_EVIDENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    retained_worker_owner_evidence()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(DesktopWorkerRetainedOwnerEvidence {
            sequence,
            event,
            owner,
            detail,
        });
}

#[cfg(windows)]
pub(crate) fn desktop_worker_retained_owner_report() -> DesktopWorkerRetainedOwnerReport {
    let active_owners = lock_retained_worker_process_handles().clone();
    let reap_in_progress_count =
        retained_worker_reaps_in_progress().load(std::sync::atomic::Ordering::Acquire);
    let evidence = retained_worker_owner_evidence()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    DesktopWorkerRetainedOwnerReport {
        active_owner_count: active_owners.len().saturating_add(reap_in_progress_count),
        reap_in_progress_count,
        active_owners,
        evidence,
    }
}

#[cfg(not(windows))]
pub(crate) fn desktop_worker_retained_owner_report() -> DesktopWorkerRetainedOwnerReport {
    DesktopWorkerRetainedOwnerReport {
        active_owner_count: 0,
        reap_in_progress_count: 0,
        active_owners: Vec::new(),
        evidence: Vec::new(),
    }
}

#[cfg(windows)]
impl WorkerProcessHandles {
    fn from_process_info(
        process_info: windows::Win32::System::Threading::PROCESS_INFORMATION,
        job: windows::Win32::Foundation::HANDLE,
    ) -> Self {
        Self {
            process: (!process_info.hProcess.0.is_null()).then_some(process_info.hProcess),
            thread: (!process_info.hThread.0.is_null()).then_some(process_info.hThread),
            job: (!job.0.is_null()).then_some(job),
            job_assigned: false,
            terminal_verified: false,
            terminal_exit_code: None,
            child_created: true,
            retained_owner_id: None,
            pid: process_info.dwProcessId,
        }
    }

    fn from_retained(retained: DesktopWorkerRetainedOwner) -> Self {
        fn restore(raw: isize) -> windows::Win32::Foundation::HANDLE {
            windows::Win32::Foundation::HANDLE(raw as *mut core::ffi::c_void)
        }

        Self {
            process: retained.process_handle.map(restore),
            thread: retained.thread_handle.map(restore),
            job: retained.job_handle.map(restore),
            job_assigned: retained.job_assigned,
            terminal_verified: retained.terminal_verified,
            terminal_exit_code: retained.terminal_exit_code,
            child_created: retained.child_created,
            retained_owner_id: Some(retained.owner_id),
            pid: retained.pid,
        }
    }

    fn from_standalone_job(job: windows::Win32::Foundation::HANDLE) -> Self {
        Self {
            process: None,
            thread: None,
            job: (!job.0.is_null()).then_some(job),
            job_assigned: false,
            terminal_verified: false,
            terminal_exit_code: None,
            child_created: false,
            retained_owner_id: None,
            pid: 0,
        }
    }

    const fn has_handles(&self) -> bool {
        self.process.is_some() || self.thread.is_some() || self.job.is_some()
    }

    fn mark_terminal(&mut self, exit_code: u32) {
        self.terminal_verified = true;
        self.terminal_exit_code = Some(exit_code);
    }

    fn retain_remaining(&mut self, stage: &'static str, last_failure: String) -> bool {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_RETAINED_OWNER_ID: AtomicU64 = AtomicU64::new(1);

        if !self.has_handles() {
            return false;
        }
        let event = if self.retained_owner_id.is_some() {
            "retry_retained"
        } else {
            "retained"
        };
        let retained = DesktopWorkerRetainedOwner {
            owner_id: self
                .retained_owner_id
                .unwrap_or_else(|| NEXT_RETAINED_OWNER_ID.fetch_add(1, Ordering::Relaxed)),
            pid: self.pid,
            process_handle: self.process.map(|handle| handle.0 as isize),
            thread_handle: self.thread.map(|handle| handle.0 as isize),
            job_handle: self.job.map(|handle| handle.0 as isize),
            job_assigned: self.job_assigned,
            terminal_verified: self.terminal_verified,
            terminal_exit_code: self.terminal_exit_code,
            child_created: self.child_created,
            last_failure,
        };
        lock_retained_worker_process_handles().push(retained.clone());
        append_retained_worker_owner_evidence(
            event,
            retained.clone(),
            format!("exact desktop-worker kernel handles retained at {stage}"),
        );
        self.process = None;
        self.thread = None;
        self.job = None;
        self.job_assigned = false;
        self.retained_owner_id = None;
        report_worker_process_lifecycle_failure(
            "MCP_DESKTOP_WORKER_HANDLES_RETAINED",
            self.pid,
            stage,
            &format!(
                "retained exact desktop-worker kernel handles for later checked cleanup; process={:?}; thread={:?}; job={:?}; terminal_verified={}; terminal_exit_code={:?}",
                retained.process_handle,
                retained.thread_handle,
                retained.job_handle,
                retained.terminal_verified,
                retained.terminal_exit_code
            ),
        );
        true
    }

    fn process(&self) -> Result<windows::Win32::Foundation::HANDLE, String> {
        self.process.ok_or_else(|| {
            format!(
                "desktop worker CreateProcessW returned no process handle for pid {}",
                self.pid
            )
        })
    }

    fn thread(&self) -> Result<windows::Win32::Foundation::HANDLE, String> {
        self.thread.ok_or_else(|| {
            format!(
                "desktop worker CreateProcessW returned no primary thread handle for pid {}",
                self.pid
            )
        })
    }

    fn job(&self) -> Result<windows::Win32::Foundation::HANDLE, String> {
        self.job.ok_or_else(|| {
            format!(
                "desktop worker has no kill-on-close job handle for pid {}",
                self.pid
            )
        })
    }

    fn close_checked(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        // Callers may release process/thread ownership only after terminal
        // state was independently verified. The finalizer below establishes
        // that precondition before using this mechanical close helper.
        self.close_one("job", &mut failures);
        self.close_one("thread", &mut failures);
        self.close_one("process", &mut failures);
        failures
    }

    fn close_one(&mut self, kind: &'static str, failures: &mut Vec<String>) {
        let pid = self.pid;
        let slot = match kind {
            "job" => &mut self.job,
            "thread" => &mut self.thread,
            _ => &mut self.process,
        };
        let Some(handle) = *slot else {
            return;
        };
        if let Err(error) = unsafe { windows::Win32::Foundation::CloseHandle(handle) } {
            let detail =
                format!("CloseHandle({kind}) failed for desktop worker pid {pid}: {error}");
            report_worker_process_lifecycle_failure(
                "MCP_DESKTOP_WORKER_HANDLE_CLOSE_FAILED",
                pid,
                kind,
                &detail,
            );
            failures.push(detail);
            // CloseHandle failure is not evidence that kernel ownership was
            // released. Keep the exact handle (and job-assignment fact) for
            // the finalizer or retained-owner registry instead of silently
            // discarding the only owner of a potentially live worker tree.
            return;
        }
        *slot = None;
        if kind == "job" {
            self.job_assigned = false;
        }
    }
}

#[cfg(windows)]
impl Drop for WorkerProcessHandles {
    fn drop(&mut self) {
        if !self.has_handles() {
            return;
        }
        // Panic/unwind follows the same bounded kill -> exact wait -> exit-code
        // readback state machine as ordinary returns. If even that cannot
        // establish safe release, the process-global registry becomes the
        // durable owner of every remaining raw handle.
        let report = finalize_worker_process_handles(self, "drop_unwind");
        if !report.failures.is_empty() {
            report_worker_process_lifecycle_failure(
                "MCP_DESKTOP_WORKER_HANDLE_DROP_FAILED",
                self.pid,
                "drop",
                &report.failures.join("; "),
            );
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkerProcessVerdict {
    pid: u32,
    timed_out: bool,
    exit_code: u32,
}

#[cfg(windows)]
fn report_worker_process_lifecycle_failure(
    code: &'static str,
    pid: u32,
    stage: &'static str,
    detail: &str,
) {
    tracing::error!(
        code,
        pid,
        stage,
        detail,
        "desktop worker process lifecycle operation failed"
    );
    use std::io::Write as _;
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(
        stderr,
        "synapse-mcp desktop worker lifecycle error: code={code} pid={pid} stage={stage} detail={detail}"
    );
}

#[cfg(windows)]
fn validate_worker_exit_envelope(
    verdict: WorkerProcessVerdict,
    envelope: &WorkerEnvelope,
) -> Result<(), rmcp::ErrorData> {
    if (verdict.exit_code == 0) == envelope.ok {
        return Ok(());
    }

    let detail = format!(
        "desktop worker pid {} returned contradictory terminal evidence: kernel exit_code={} requires envelope.ok={}, but JSON envelope.ok={}; refusing an untrustworthy worker result",
        verdict.pid,
        verdict.exit_code,
        verdict.exit_code == 0,
        envelope.ok
    );
    report_worker_process_lifecycle_failure(
        "MCP_DESKTOP_WORKER_EXIT_ENVELOPE_MISMATCH",
        verdict.pid,
        "json_envelope_readback",
        &detail,
    );
    Err(crate::m1::mcp_error(
        error_codes::TOOL_INTERNAL_ERROR,
        detail,
    ))
}

#[cfg(all(windows, test))]
fn close_standalone_worker_handle(
    handle: windows::Win32::Foundation::HANDLE,
    kind: &'static str,
) -> Result<(), String> {
    unsafe { windows::Win32::Foundation::CloseHandle(handle) }
        .map_err(|error| format!("CloseHandle({kind}) failed: {error}"))
}

#[cfg(windows)]
fn close_or_retain_standalone_worker_job(
    handle: windows::Win32::Foundation::HANDLE,
    stage: &'static str,
) -> Result<(), String> {
    let mut handles = WorkerProcessHandles::from_standalone_job(handle);
    let report = finalize_worker_process_handles(&mut handles, stage);
    if report.failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "standalone desktop-worker job cleanup failed at {stage}: {}; exact_handle_retained={}",
            report.failures.join("; "),
            report.retained
        ))
    }
}

#[cfg(windows)]
fn create_worker_kill_on_close_job() -> Result<windows::Win32::Foundation::HANDLE, String> {
    use windows::{
        Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject,
        },
        core::PCWSTR,
    };

    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
        .map_err(|error| format!("CreateJobObjectW for desktop worker failed: {error}"))?;
    let limit_size = match u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
    {
        Ok(size) => size,
        Err(error) => {
            let close = close_or_retain_standalone_worker_job(job, "job_limit_size_conversion");
            return Err(format!(
                "desktop worker job limit size conversion failed: {error}; close_readback={close:?}"
            ));
        }
    };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if let Err(error) = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            limit_size,
        )
    } {
        let close = close_or_retain_standalone_worker_job(job, "job_limit_configuration");
        return Err(format!(
            "SetInformationJobObject(KILL_ON_JOB_CLOSE) for desktop worker failed: {error}; close_readback={close:?}"
        ));
    }
    let mut readback = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let mut returned_bytes = 0_u32;
    if let Err(error) = unsafe {
        QueryInformationJobObject(
            Some(job),
            JobObjectExtendedLimitInformation,
            (&raw mut readback).cast(),
            limit_size,
            Some(&raw mut returned_bytes),
        )
    } {
        let close = close_or_retain_standalone_worker_job(job, "job_limit_query");
        return Err(format!(
            "QueryInformationJobObject(KILL_ON_JOB_CLOSE) readback for desktop worker failed: {error}; close_readback={close:?}"
        ));
    }
    if returned_bytes != limit_size {
        let close = close_or_retain_standalone_worker_job(job, "job_limit_query_size_readback");
        return Err(format!(
            "QueryInformationJobObject returned {returned_bytes} bytes for desktop worker kill-on-close limits; expected {limit_size}; limit_flags={:#x}; close_readback={close:?}",
            readback.BasicLimitInformation.LimitFlags.0
        ));
    }
    if !readback
        .BasicLimitInformation
        .LimitFlags
        .contains(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
    {
        let close = close_or_retain_standalone_worker_job(job, "job_limit_flag_readback");
        return Err(format!(
            "QueryInformationJobObject readback omitted KILL_ON_JOB_CLOSE for desktop worker; limit_flags={:#x}; close_readback={close:?}",
            readback.BasicLimitInformation.LimitFlags.0
        ));
    }
    tracing::info!(
        code = "MCP_DESKTOP_WORKER_JOB_LIMIT_VERIFIED",
        limit_flags = readback.BasicLimitInformation.LimitFlags.0,
        returned_bytes,
        "desktop worker kill-on-close job limit independently read back before child creation"
    );
    Ok(job)
}

#[cfg(windows)]
fn assign_worker_job_and_resume(handles: &mut WorkerProcessHandles) -> Result<(), String> {
    use windows::{
        Win32::System::{
            JobObjects::{AssignProcessToJobObject, IsProcessInJob},
            Threading::ResumeThread,
        },
        core::BOOL,
    };

    let job = handles.job()?;
    let process = handles.process()?;
    let thread = handles.thread()?;
    unsafe { AssignProcessToJobObject(job, process) }.map_err(|error| {
        format!(
            "AssignProcessToJobObject failed for suspended desktop worker pid {}: {error}",
            handles.pid
        )
    })?;
    // Assignment succeeded even if its independent readback fails. Preserve
    // this fact so every later error path closes the kill-on-close owner.
    handles.job_assigned = true;
    let mut in_job = BOOL::default();
    unsafe { IsProcessInJob(process, Some(job), &raw mut in_job) }.map_err(|error| {
        format!(
            "IsProcessInJob readback failed for suspended desktop worker pid {}: {error}",
            handles.pid
        )
    })?;
    if !in_job.as_bool() {
        return Err(format!(
            "IsProcessInJob readback was false after assigning suspended desktop worker pid {}",
            handles.pid
        ));
    }
    let previous_suspend_count = unsafe { ResumeThread(thread) };
    if previous_suspend_count == u32::MAX {
        return Err(format!(
            "ResumeThread failed for job-owned desktop worker pid {}: {}",
            handles.pid,
            windows::core::Error::from_thread()
        ));
    }
    if previous_suspend_count != 1 {
        return Err(format!(
            "ResumeThread returned unexpected prior suspend count {previous_suspend_count} for desktop worker pid {}; expected exactly 1",
            handles.pid,
        ));
    }
    tracing::info!(
        code = "MCP_DESKTOP_WORKER_JOB_OWNERSHIP_VERIFIED",
        pid = handles.pid,
        previous_suspend_count,
        "desktop worker was assigned to a kill-on-close job, independently read back, and resumed"
    );
    Ok(())
}

#[cfg(windows)]
fn read_worker_exit_code(
    process: windows::Win32::Foundation::HANDLE,
    pid: u32,
    stage: &'static str,
) -> Result<u32, String> {
    let mut exit_code = 0_u32;
    unsafe { windows::Win32::System::Threading::GetExitCodeProcess(process, &raw mut exit_code) }
        .map_err(|error| {
        format!("GetExitCodeProcess failed for desktop worker pid {pid} at {stage}: {error}")
    })?;
    Ok(exit_code)
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerTerminalProbe {
    Running,
    Terminal(u32),
}

#[cfg(windows)]
#[derive(Debug)]
struct WorkerHandleFinalization {
    terminal_exit_code: Option<u32>,
    failures: Vec<String>,
    retained: bool,
    job_close_triggered: bool,
}

#[cfg(windows)]
fn reconcile_provisional_terminate_process_failure(
    failures: &mut Vec<String>,
    provisional_failure: Option<String>,
    exact_terminal_exit_code: Option<u32>,
    pid: u32,
    stage: &'static str,
) {
    let Some(provisional_failure) = provisional_failure else {
        return;
    };
    if let Some(exit_code) = exact_terminal_exit_code {
        // TerminateProcess is asynchronous with respect to the independent job
        // termination trigger. Windows explicitly returns ERROR_ACCESS_DENIED
        // when the exact process has already terminated. Preserve that raced
        // syscall as diagnostic evidence, but let the subsequent wait on this
        // same process handle plus non-STILL_ACTIVE exit-code readback decide
        // the terminal verdict.
        tracing::warn!(
            code = "MCP_DESKTOP_WORKER_TERMINATE_PROCESS_RACE_RESOLVED",
            pid,
            stage,
            exit_code,
            provisional_failure,
            source_of_truth =
                "exact process HANDLE signaled + GetExitCodeProcess non-STILL_ACTIVE readback",
            "desktop worker exact-process termination syscall raced with an independently verified terminal transition"
        );
    } else {
        failures.push(provisional_failure);
    }
}

#[cfg(windows)]
fn probe_worker_terminal_state(
    handles: &mut WorkerProcessHandles,
    timeout_ms: u32,
    stage: &'static str,
) -> Result<WorkerTerminalProbe, String> {
    use windows::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::WaitForSingleObject,
    };

    if handles.terminal_verified {
        return handles
            .terminal_exit_code
            .map(WorkerTerminalProbe::Terminal)
            .ok_or_else(|| {
                format!(
                    "desktop worker pid {} marked terminal without an exit-code readback at {stage}",
                    handles.pid
                )
            });
    }
    let process = handles.process()?;
    let wait = unsafe { WaitForSingleObject(process, timeout_ms) };
    if wait == WAIT_TIMEOUT {
        return Ok(WorkerTerminalProbe::Running);
    }
    if wait != WAIT_OBJECT_0 {
        let last_error = (wait == WAIT_FAILED)
            .then(windows::core::Error::from_thread)
            .map_or_else(|| "not available".to_owned(), |error| error.to_string());
        return Err(format!(
            "desktop worker pid {} terminal probe at {stage} returned {wait:?}; timeout_ms={timeout_ms}; last_error={last_error}",
            handles.pid
        ));
    }
    let exit_code = read_worker_exit_code(process, handles.pid, stage)?;
    if exit_code == STILL_ACTIVE_EXIT_CODE {
        return Err(format!(
            "desktop worker pid {} was signaled at {stage}, but GetExitCodeProcess returned STILL_ACTIVE ({STILL_ACTIVE_EXIT_CODE})",
            handles.pid
        ));
    }
    handles.mark_terminal(exit_code);
    Ok(WorkerTerminalProbe::Terminal(exit_code))
}

#[cfg(windows)]
fn finalize_worker_process_handles(
    handles: &mut WorkerProcessHandles,
    stage: &'static str,
) -> WorkerHandleFinalization {
    use windows::Win32::System::Threading::TerminateProcess;

    let mut failures = Vec::new();
    let mut job_close_triggered = false;
    if !handles.child_created {
        if handles.process.is_some() || handles.thread.is_some() || handles.job_assigned {
            failures.push(format!(
                "childless desktop-worker owner unexpectedly contained process/thread/job-assignment state at {stage}; process={}; thread={}; job_assigned={}",
                handles.process.is_some(),
                handles.thread.is_some(),
                handles.job_assigned
            ));
        }
        handles.close_one("job", &mut failures);
        let retained = if handles.has_handles() {
            let last_failure = if failures.is_empty() {
                format!("childless desktop-worker job handle remained owned after {stage}")
            } else {
                failures.join("; ")
            };
            let retained = handles.retain_remaining(stage, last_failure);
            failures.push(format!(
                "childless desktop-worker exact job handle retained after {stage}"
            ));
            retained
        } else {
            false
        };
        return WorkerHandleFinalization {
            terminal_exit_code: None,
            failures,
            retained,
            job_close_triggered,
        };
    }
    match probe_worker_terminal_state(handles, 0, "finalize_pre_close") {
        Ok(WorkerTerminalProbe::Terminal(_)) => {}
        Ok(WorkerTerminalProbe::Running) => {}
        Err(error) => failures.push(error),
    }

    if !handles.terminal_verified {
        let job_was_assigned = handles.job_assigned;
        if handles.job.is_some() {
            let failure_count = failures.len();
            handles.close_one("job", &mut failures);
            let job_close_succeeded = handles.job.is_none();
            if job_was_assigned && job_close_succeeded {
                job_close_triggered = true;
                match probe_worker_terminal_state(
                    handles,
                    WORKER_TERMINATION_READBACK_TIMEOUT_MS,
                    "post_kill_on_close_job_wait",
                ) {
                    Ok(WorkerTerminalProbe::Terminal(_)) => {}
                    Ok(WorkerTerminalProbe::Running) => failures.push(format!(
                        "desktop worker pid {} remained live for {} ms after the verified kill-on-close job handle was closed",
                        handles.pid, WORKER_TERMINATION_READBACK_TIMEOUT_MS
                    )),
                    Err(error) => failures.push(error),
                }
            } else if job_was_assigned && failures.len() == failure_count {
                failures.push(format!(
                    "desktop worker pid {} lost its recorded job assignment without a checked job-close result",
                    handles.pid
                ));
            }
        }
    }

    if !handles.terminal_verified {
        let mut provisional_terminate_process_failure = None;
        match handles.process() {
            Ok(process) => {
                if let Err(error) = unsafe { TerminateProcess(process, 1) } {
                    provisional_terminate_process_failure = Some(format!(
                        "TerminateProcess exact-child finalizer failed for desktop worker pid {} at {stage}: {error}",
                        handles.pid
                    ));
                }
            }
            Err(error) => failures.push(error),
        }
        let exact_terminal_exit_code = match probe_worker_terminal_state(
            handles,
            WORKER_TERMINATION_READBACK_TIMEOUT_MS,
            "post_exact_process_termination_wait",
        ) {
            Ok(WorkerTerminalProbe::Terminal(exit_code)) => Some(exit_code),
            Ok(WorkerTerminalProbe::Running) => {
                failures.push(format!(
                    "desktop worker pid {} remained live for {} ms after the exact-process termination fallback at {stage}",
                    handles.pid, WORKER_TERMINATION_READBACK_TIMEOUT_MS
                ));
                None
            }
            Err(error) => {
                failures.push(error);
                None
            }
        };
        reconcile_provisional_terminate_process_failure(
            &mut failures,
            provisional_terminate_process_failure,
            exact_terminal_exit_code,
            handles.pid,
            stage,
        );
    }

    if handles.terminal_verified {
        failures.extend(handles.close_checked());
    } else {
        failures.push(format!(
            "desktop worker pid {} has no verified signaled-state plus exit-code readback at {stage}; process/thread ownership will not be relinquished",
            handles.pid
        ));
    }

    let retained = if handles.has_handles() {
        let process_retained = handles.process.is_some();
        let thread_retained = handles.thread.is_some();
        let job_retained = handles.job.is_some();
        let last_failure = if failures.is_empty() {
            format!(
                "desktop worker pid {} still owned handles after checked finalization at {stage}",
                handles.pid
            )
        } else {
            failures.join("; ")
        };
        let retained = handles.retain_remaining(stage, last_failure);
        failures.push(format!(
            "desktop worker pid {} exact handles retained after {stage}: process={process_retained}; thread={thread_retained}; job={job_retained}",
            handles.pid
        ));
        retained
    } else {
        false
    };

    WorkerHandleFinalization {
        terminal_exit_code: handles.terminal_exit_code,
        failures,
        retained,
        job_close_triggered,
    }
}

#[cfg(windows)]
fn retry_retained_worker_process_handles() -> Result<(), String> {
    static RETRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _retry_guard = RETRY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let Some((retained, _reap_guard)) = take_retained_worker_owner_for_retry() else {
            return Ok(());
        };
        let pid = retained.pid;
        let retained_evidence = retained.clone();
        let mut handles = WorkerProcessHandles::from_retained(retained);
        let report = finalize_worker_process_handles(&mut handles, "retained_handle_retry");
        if report.retained {
            return Err(format!(
                "desktop worker pid {pid} retained-handle retry still owns unresolved handles: {}",
                report.failures.join("; ")
            ));
        }
        let recovery_detail = if report.failures.is_empty() {
            format!(
                "retained desktop-worker owner {} was reaped with terminal_exit_code={:?}",
                retained_evidence.owner_id, report.terminal_exit_code
            )
        } else {
            format!(
                "retained desktop-worker owner {} was reaped after checked recovery failures: {}",
                retained_evidence.owner_id,
                report.failures.join("; ")
            )
        };
        append_retained_worker_owner_evidence("reaped", retained_evidence, recovery_detail.clone());
        if !report.failures.is_empty() {
            report_worker_process_lifecycle_failure(
                "MCP_DESKTOP_WORKER_RETAINED_HANDLE_RETRY_RECOVERED",
                pid,
                "retained_handle_retry",
                &recovery_detail,
            );
        }
    }
}

#[cfg(windows)]
fn wait_for_worker_process(
    handles: &mut WorkerProcessHandles,
    timeout_ms: u32,
) -> Result<WorkerProcessVerdict, String> {
    use windows::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::WaitForSingleObject,
    };

    let process = handles.process()?;
    let wait = unsafe { WaitForSingleObject(process, timeout_ms) };
    if wait == WAIT_OBJECT_0 {
        let exit_code = read_worker_exit_code(process, handles.pid, "natural_exit")?;
        handles.mark_terminal(exit_code);
        return Ok(WorkerProcessVerdict {
            pid: handles.pid,
            timed_out: false,
            exit_code,
        });
    }
    if wait != WAIT_TIMEOUT {
        let last_error = (wait == WAIT_FAILED)
            .then(windows::core::Error::from_thread)
            .map_or_else(|| "not available".to_owned(), |error| error.to_string());
        let primary = format!(
            "WaitForSingleObject returned {wait:?} for desktop worker pid {}; last_error={last_error}",
            handles.pid
        );
        let cleanup = terminate_worker_process_and_readback(handles, timeout_ms).map_or_else(
            |cleanup_error| format!("cleanup_failed={cleanup_error}"),
            |exit_code| format!("cleanup_verified_exit_code={exit_code}"),
        );
        return Err(format!("{primary}; {cleanup}"));
    }

    // Termination is asynchronous. A successful syscall is not the verdict:
    // wait on the exact process handle, then read its kernel exit code through
    // a separate operation before reporting the timeout to the caller.
    let exit_code = terminate_worker_process_and_readback(handles, timeout_ms)?;
    Ok(WorkerProcessVerdict {
        pid: handles.pid,
        timed_out: true,
        exit_code,
    })
}

#[cfg(windows)]
fn terminate_worker_process_and_readback(
    handles: &mut WorkerProcessHandles,
    initial_timeout_ms: u32,
) -> Result<u32, String> {
    use windows::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            JobObjects::TerminateJobObject,
            Threading::{TerminateProcess, WaitForSingleObject},
        },
    };

    let process = handles.process()?;
    let pid = handles.pid;
    let mut failures = Vec::new();
    if handles.job_assigned {
        match handles.job() {
            Ok(job) => {
                if let Err(error) = unsafe { TerminateJobObject(job, 1) } {
                    failures.push(format!(
                        "TerminateJobObject failed after desktop worker pid {pid} did not stop within {initial_timeout_ms} ms: {error}"
                    ));
                    // Closing a configured kill-on-close job is an independent
                    // kernel termination trigger. Attempt it even when the
                    // explicit job termination fails.
                    let mut close_failures = Vec::new();
                    handles.close_one("job", &mut close_failures);
                    failures.extend(close_failures);
                }
            }
            Err(error) => failures.push(error),
        }
    }
    // Job termination is asynchronous, and an assignment readback failure
    // makes its membership untrustworthy. If the exact process is not already
    // signaled, independently trigger exact-process termination as well.
    let pre_fallback_wait = unsafe { WaitForSingleObject(process, 0) };
    if pre_fallback_wait != WAIT_OBJECT_0 && pre_fallback_wait != WAIT_TIMEOUT {
        let last_error = (pre_fallback_wait == WAIT_FAILED)
            .then(windows::core::Error::from_thread)
            .map_or_else(|| "not available".to_owned(), |error| error.to_string());
        failures.push(format!(
            "desktop worker pid {pid} exact-process pre-fallback wait returned {pre_fallback_wait:?}; last_error={last_error}"
        ));
    }
    let provisional_terminate_process_failure = if pre_fallback_wait == WAIT_OBJECT_0 {
        None
    } else {
        unsafe { TerminateProcess(process, 1) }
            .err()
            .map(|error| {
                format!(
                    "TerminateProcess exact-child fallback failed for desktop worker pid {pid} after {initial_timeout_ms} ms: {error}; pre_fallback_wait={pre_fallback_wait:?}"
                )
            })
    };
    let termination_wait =
        unsafe { WaitForSingleObject(process, WORKER_TERMINATION_READBACK_TIMEOUT_MS) };
    if termination_wait != WAIT_OBJECT_0 {
        let last_error = (termination_wait == WAIT_FAILED)
            .then(windows::core::Error::from_thread)
            .map_or_else(|| "not available".to_owned(), |error| error.to_string());
        failures.push(format!(
            "desktop worker pid {pid} did not reach signaled state after job/process termination within {WORKER_TERMINATION_READBACK_TIMEOUT_MS} ms; wait={termination_wait:?}; last_error={last_error}"
        ));
    }
    let mut exact_terminal_exit_code = None;
    let exit_code = match read_worker_exit_code(process, pid, "post_termination_readback") {
        Ok(exit_code) => {
            if exit_code == STILL_ACTIVE_EXIT_CODE {
                failures.push(format!(
                    "desktop worker pid {pid} remained STILL_ACTIVE after termination wait; wait={termination_wait:?}"
                ));
            } else if termination_wait == WAIT_OBJECT_0 {
                handles.mark_terminal(exit_code);
                exact_terminal_exit_code = Some(exit_code);
            }
            Some(exit_code)
        }
        Err(error) => {
            failures.push(error);
            None
        }
    };
    reconcile_provisional_terminate_process_failure(
        &mut failures,
        provisional_terminate_process_failure,
        exact_terminal_exit_code,
        pid,
        "post_termination_readback",
    );
    if failures.is_empty() {
        exit_code.ok_or_else(|| {
            format!("desktop worker pid {pid} had no exit-code readback after termination")
        })
    } else {
        Err(format!(
            "desktop worker pid {pid} termination/readback failed: {}; exit_code_readback={exit_code:?}",
            failures.join("; ")
        ))
    }
}

#[cfg(windows)]
fn launch_worker_process(
    desktop_name: &str,
    op: DesktopWorkerOp,
    hwnd: i64,
    region: Option<Rect>,
    client_region: bool,
    depth: Option<u32>,
    temp: &WorkerTempPaths,
) -> Result<WorkerProcessVerdict, rmcp::ErrorData> {
    use windows::{
        Win32::System::Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            PROCESS_INFORMATION, STARTUPINFOW,
        },
        core::{PCWSTR, PWSTR},
    };

    const WORKER_TIMEOUT_MS: u32 = 10_000;

    // Keep a final invariant check at the process-creation boundary. Callers
    // validate earlier for stable classification before temp allocation, but
    // this function must never be able to encode a noncanonical HWND into a
    // child command line if a new caller bypasses those outer seams.
    crate::m1::validate_window_hwnd_shape("hidden_desktop_worker", hwnd)?;
    retry_retained_worker_process_handles().map_err(|error| {
        report_worker_process_lifecycle_failure(
            "MCP_DESKTOP_WORKER_RETAINED_HANDLE_RETRY_FAILED",
            0,
            "before_new_worker_spawn",
            &error,
        );
        crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "desktop worker cannot spawn while prior exact child handles remain unresolved: {error}"
            ),
        )
    })?;
    let exe = std::env::current_exe().map_err(|error| {
        crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("desktop worker could not resolve current executable: {error}"),
        )
    })?;
    let mut args = vec![
        exe.to_string_lossy().into_owned(),
        "--mode".to_owned(),
        "desktop-worker".to_owned(),
        "--desktop-worker-op".to_owned(),
        op.as_arg().to_owned(),
        "--desktop-worker-hwnd".to_owned(),
        hwnd.to_string(),
        "--desktop-worker-json".to_owned(),
        temp.json_path.to_string_lossy().into_owned(),
    ];
    if let Some(region) = region {
        args.push("--desktop-worker-region".to_owned());
        args.push(format!(
            "{},{},{},{}",
            region.x, region.y, region.w, region.h
        ));
    }
    if client_region {
        args.push("--desktop-worker-client-region".to_owned());
    }
    if let Some(depth) = depth {
        args.push("--desktop-worker-depth".to_owned());
        args.push(depth.to_string());
    }
    if let Some(bgra_path) = temp.bgra_path.as_ref() {
        args.push("--desktop-worker-bgra".to_owned());
        args.push(bgra_path.to_string_lossy().into_owned());
    }
    let command_line = args
        .iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command_line_wide = wide_null(&command_line);
    let desktop_wide = wide_null(desktop_name);
    let startup_info = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>()).unwrap_or(u32::MAX),
        lpDesktop: PWSTR(desktop_wide.as_ptr().cast_mut()),
        ..Default::default()
    };
    let job = create_worker_kill_on_close_job().map_err(|error| {
        report_worker_process_lifecycle_failure(
            "MCP_DESKTOP_WORKER_JOB_CREATE_FAILED",
            0,
            "create_kill_on_close_job",
            &error,
        );
        crate::m1::mcp_error(error_codes::TOOL_INTERNAL_ERROR, error)
    })?;
    let mut process_info = PROCESS_INFORMATION::default();
    let create_result = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command_line_wide.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            None,
            PCWSTR::null(),
            &raw const startup_info,
            &raw mut process_info,
        )
    };
    if let Err(error) = create_result {
        let job_close =
            close_or_retain_standalone_worker_job(job, "job_after_create_process_failure");
        let detail = format!(
            "desktop worker CreateProcessW failed for desktop {desktop_name:?}: {error}; job_close_readback={job_close:?}"
        );
        report_worker_process_lifecycle_failure(
            "MCP_DESKTOP_WORKER_CREATE_PROCESS_FAILED",
            0,
            "create_suspended_process",
            &detail,
        );
        return Err(crate::m1::mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            detail,
        ));
    }

    let mut handles = WorkerProcessHandles::from_process_info(process_info, job);
    if let Err(start_error) = assign_worker_job_and_resume(&mut handles) {
        let termination_readback = terminate_worker_process_and_readback(&mut handles, 0);
        let finalization =
            finalize_worker_process_handles(&mut handles, "assign_verify_resume_failure");
        let detail = format!(
            "desktop worker pid {} could not establish verified kill-on-close ownership and resume: {start_error}; termination_readback={termination_readback:?}; final_terminal_exit_code={:?}; job_close_triggered={}; finalization_failures={:?}; exact_handles_retained={}",
            handles.pid,
            finalization.terminal_exit_code,
            finalization.job_close_triggered,
            finalization.failures,
            finalization.retained
        );
        report_worker_process_lifecycle_failure(
            "MCP_DESKTOP_WORKER_JOB_START_FAILED",
            handles.pid,
            "assign_verify_resume",
            &detail,
        );
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            detail,
        ));
    }
    tracing::info!(
        code = "MCP_DESKTOP_WORKER_PROCESS_STARTED",
        pid = handles.pid,
        desktop_name,
        operation = op.as_arg(),
        hwnd,
        "hidden-desktop worker process started"
    );
    let process_result = wait_for_worker_process(&mut handles, WORKER_TIMEOUT_MS);
    let finalization = finalize_worker_process_handles(&mut handles, "launch_result_cleanup");
    let verdict = match process_result {
        Ok(verdict) if finalization.failures.is_empty() => verdict,
        Ok(verdict) => {
            let detail = format!(
                "desktop worker pid {} reached terminal exit_code={} timed_out={}, but checked handle finalization failed: {}; final_terminal_exit_code={:?}; job_close_triggered={}; exact_handles_retained={}",
                verdict.pid,
                verdict.exit_code,
                verdict.timed_out,
                finalization.failures.join("; "),
                finalization.terminal_exit_code,
                finalization.job_close_triggered,
                finalization.retained
            );
            report_worker_process_lifecycle_failure(
                "MCP_DESKTOP_WORKER_HANDLE_CLEANUP_FAILED",
                verdict.pid,
                "close_after_wait",
                &detail,
            );
            return Err(crate::m1::mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                detail,
            ));
        }
        Err(error) => {
            let detail = if finalization.failures.is_empty() {
                error
            } else {
                format!(
                    "{error}; checked_handle_finalization_failures={}; final_terminal_exit_code={:?}; job_close_triggered={}; exact_handles_retained={}",
                    finalization.failures.join("; "),
                    finalization.terminal_exit_code,
                    finalization.job_close_triggered,
                    finalization.retained
                )
            };
            report_worker_process_lifecycle_failure(
                "MCP_DESKTOP_WORKER_PROCESS_WAIT_FAILED",
                handles.pid,
                "wait_or_terminate",
                &detail,
            );
            return Err(crate::m1::mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                detail,
            ));
        }
    };
    if verdict.timed_out {
        tracing::error!(
            code = "MCP_DESKTOP_WORKER_TIMEOUT_REAPED",
            pid = verdict.pid,
            timeout_ms = WORKER_TIMEOUT_MS,
            exit_code = verdict.exit_code,
            desktop_name,
            "desktop worker timed out and the exact child was terminated, joined, and read back"
        );
        return Err(crate::m1::mcp_error(
            error_codes::A11Y_UIA_WORKER_TIMEOUT,
            format!(
                "desktop worker pid {} timed out after {WORKER_TIMEOUT_MS} ms for desktop {desktop_name:?}; exact process termination was verified with exit_code={} and both Win32 handles were closed",
                verdict.pid, verdict.exit_code
            ),
        ));
    }
    tracing::info!(
        code = "MCP_DESKTOP_WORKER_PROCESS_REAPED",
        pid = verdict.pid,
        exit_code = verdict.exit_code,
        desktop_name,
        operation = op.as_arg(),
        "hidden-desktop worker reached kernel-signaled terminal state and both handles closed"
    );
    if verdict.exit_code != 0 && !temp.json_path.exists() {
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "desktop worker pid {} exited {} without JSON readback for desktop {desktop_name:?}",
                verdict.pid, verdict.exit_code
            ),
        ));
    }
    Ok(verdict)
}

#[cfg(windows)]
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, rmcp::ErrorData> {
    let bytes = fs::read(path).map_err(|error| {
        crate::m1::mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "desktop worker JSON readback failed for {}: {error}",
                path.display()
            ),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        crate::m1::mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "desktop worker JSON decode failed for {}: {error}",
                path.display()
            ),
        )
    })
}

#[cfg(windows)]
fn quote_windows_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
struct WorkerTempPaths {
    json_path: PathBuf,
    bgra_path: Option<PathBuf>,
    cleaned: bool,
}

#[cfg(windows)]
impl WorkerTempPaths {
    fn new(include_bgra: bool) -> Result<Self, rmcp::ErrorData> {
        let dir = std::env::temp_dir().join("synapse-desktop-worker");
        fs::create_dir_all(&dir).map_err(|error| {
            crate::m1::mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "could not create desktop worker temp dir {}: {error}",
                    dir.display()
                ),
            )
        })?;
        let id = Uuid::new_v4();
        Ok(Self {
            json_path: dir.join(format!("{id}.json")),
            bgra_path: include_bgra.then(|| dir.join(format!("{id}.bgra"))),
            cleaned: false,
        })
    }

    fn cleanup_checked(&mut self) -> Result<(), rmcp::ErrorData> {
        let mut failures = Vec::new();
        for (kind, path) in std::iter::once(("json", &self.json_path))
            .chain(self.bgra_path.as_ref().map(|path| ("bgra", path)))
        {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    let detail = format!(
                        "remove desktop worker {kind} temp file {}: {error}; kind={:?}; raw_os_error={:?}",
                        path.display(),
                        error.kind(),
                        error.raw_os_error()
                    );
                    report_worker_process_lifecycle_failure(
                        "MCP_DESKTOP_WORKER_TEMP_CLEANUP_FAILED",
                        std::process::id(),
                        kind,
                        &detail,
                    );
                    failures.push(detail);
                }
            }
        }
        if failures.is_empty() {
            self.cleaned = true;
            Ok(())
        } else {
            Err(crate::m1::mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "desktop worker temporary artifact cleanup failed: {}",
                    failures.join("; ")
                ),
            ))
        }
    }
}

#[cfg(windows)]
impl Drop for WorkerTempPaths {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        if let Err(error) = self.cleanup_checked() {
            report_worker_process_lifecycle_failure(
                "MCP_DESKTOP_WORKER_TEMP_DROP_FAILED",
                std::process::id(),
                "drop",
                &format!("{error:?}"),
            );
        }
    }
}

#[cfg(windows)]
fn finish_worker_temp_cleanup<T>(
    result: Result<T, rmcp::ErrorData>,
    temp: &mut WorkerTempPaths,
) -> Result<T, rmcp::ErrorData> {
    let cleanup = temp.cleanup_checked();
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => {
            let detail = format!(
                "desktop worker operation and temporary artifact cleanup both failed; primary={primary:?}; cleanup={cleanup:?}"
            );
            report_worker_process_lifecycle_failure(
                "MCP_DESKTOP_WORKER_OPERATION_AND_TEMP_CLEANUP_FAILED",
                std::process::id(),
                "finish",
                &detail,
            );
            Err(crate::m1::mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                detail,
            ))
        }
    }
}

fn worker_code_static(code: &str) -> &'static str {
    match code {
        error_codes::ACTION_TARGET_INVALID => error_codes::ACTION_TARGET_INVALID,
        error_codes::A11Y_NOT_AVAILABLE => error_codes::A11Y_NOT_AVAILABLE,
        error_codes::A11Y_NO_FOREGROUND => error_codes::A11Y_NO_FOREGROUND,
        error_codes::A11Y_UIA_WORKER_TIMEOUT => error_codes::A11Y_UIA_WORKER_TIMEOUT,
        error_codes::CAPTURE_GRAPHICS_API_UNSUPPORTED => {
            error_codes::CAPTURE_GRAPHICS_API_UNSUPPORTED
        }
        error_codes::CAPTURE_PRINTWINDOW_BLACK => error_codes::CAPTURE_PRINTWINDOW_BLACK,
        error_codes::CAPTURE_PRINTWINDOW_DISABLED => error_codes::CAPTURE_PRINTWINDOW_DISABLED,
        error_codes::CAPTURE_TARGET_INVALID => error_codes::CAPTURE_TARGET_INVALID,
        error_codes::CAPTURE_TARGET_LOST => error_codes::CAPTURE_TARGET_LOST,
        error_codes::OBSERVE_INTERNAL => error_codes::OBSERVE_INTERNAL,
        error_codes::STORAGE_CORRUPTED => error_codes::STORAGE_CORRUPTED,
        error_codes::STORAGE_READ_FAILED => error_codes::STORAGE_READ_FAILED,
        error_codes::STORAGE_WRITE_FAILED => error_codes::STORAGE_WRITE_FAILED,
        error_codes::TARGET_WINDOW_NOT_FOUND => error_codes::TARGET_WINDOW_NOT_FOUND,
        error_codes::TOOL_PARAMS_INVALID => error_codes::TOOL_PARAMS_INVALID,
        _ => error_codes::TOOL_INTERNAL_ERROR,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn retained_registry_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn spawn_real_process(executable: &Path, args: &[&str]) -> WorkerProcessHandles {
        use windows::{
            Win32::System::Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
                PROCESS_INFORMATION, STARTUPINFOW,
            },
            core::{PCWSTR, PWSTR},
        };

        assert!(
            executable.is_file(),
            "real process executable is missing: {}",
            executable.display()
        );
        let mut command = vec![executable.to_string_lossy().into_owned()];
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        let mut command_line = wide_null(
            &command
                .iter()
                .map(|arg| quote_windows_arg(arg))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let startup = STARTUPINFOW {
            cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>())
                .expect("STARTUPINFOW size fits u32"),
            ..Default::default()
        };
        let job = create_worker_kill_on_close_job()
            .unwrap_or_else(|error| panic!("create real process job: {error}"));
        let mut process = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                None,
                PCWSTR::null(),
                &raw const startup,
                &raw mut process,
            )
        }
        .unwrap_or_else(|error| panic!("spawn real process {}: {error}", executable.display()));
        let mut handles = WorkerProcessHandles::from_process_info(process, job);
        if let Err(error) = assign_worker_job_and_resume(&mut handles) {
            let termination = terminate_worker_process_and_readback(&mut handles, 0);
            let finalization =
                finalize_worker_process_handles(&mut handles, "test_spawn_assign_resume_failure");
            panic!(
                "assign/resume real process {}: {error}; termination={termination:?}; finalization={finalization:?}",
                executable.display()
            );
        }
        handles
    }

    #[test]
    fn terminate_process_error_is_demoted_only_by_exact_terminal_readback() {
        let mut independently_verified = vec!["prior diagnostic".to_owned()];
        reconcile_provisional_terminate_process_failure(
            &mut independently_verified,
            Some("TerminateProcess failed: access denied".to_owned()),
            Some(1),
            4242,
            "causal_terminal_readback",
        );
        assert_eq!(
            independently_verified,
            ["prior diagnostic"],
            "the raced syscall is diagnostic-only after exact terminal proof"
        );

        let mut unresolved = vec!["prior diagnostic".to_owned()];
        reconcile_provisional_terminate_process_failure(
            &mut unresolved,
            Some("TerminateProcess failed: access denied".to_owned()),
            None,
            4242,
            "causal_terminal_readback",
        );
        assert_eq!(
            unresolved,
            ["prior diagnostic", "TerminateProcess failed: access denied"],
            "without exact terminal proof the syscall failure remains fatal"
        );
    }

    #[test]
    fn failed_job_handle_close_retains_kernel_ownership_for_retry() {
        use windows::Win32::{
            Foundation::{
                HANDLE_FLAG_PROTECT_FROM_CLOSE, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
            },
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                WaitForSingleObject,
            },
        };

        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut handles = spawn_real_process(
            &powershell,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ],
        );
        let job = handles.job().expect("real worker job handle");
        let readback_handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                handles.pid,
            )
        }
        .unwrap_or_else(|error| panic!("open exact protected-close worker: {error}"));
        unsafe {
            SetHandleInformation(
                job,
                HANDLE_FLAG_PROTECT_FROM_CLOSE.0,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        }
        .expect("protect real job handle from close");

        let protected_close_failures = handles.close_checked();
        let job_retained_after_failed_close = handles.job.is_some();
        let assignment_retained_after_failed_close = handles.job_assigned;
        unsafe { SetHandleInformation(job, HANDLE_FLAG_PROTECT_FROM_CLOSE.0, HANDLE_FLAGS(0)) }
            .expect("clear real job handle close protection");
        let retry_failures = handles.close_checked();
        let wait = unsafe { WaitForSingleObject(readback_handle, 10_000) };
        let exit_code = read_worker_exit_code(
            readback_handle,
            handles.pid,
            "protected_job_close_retry_readback",
        )
        .unwrap_or_else(|error| panic!("read protected-close worker exit: {error}"));
        let readback_close = close_standalone_worker_handle(readback_handle, "readback_process");

        assert_eq!(protected_close_failures.len(), 1);
        assert!(protected_close_failures[0].contains("CloseHandle(job) failed"));
        assert!(
            job_retained_after_failed_close,
            "failed close must retain the exact kernel handle for Drop/backstop retry"
        );
        assert!(
            assignment_retained_after_failed_close,
            "failed job close is not evidence that kill-on-close ownership ended"
        );
        assert!(retry_failures.is_empty(), "{retry_failures:?}");
        assert!(handles.job.is_none());
        assert!(!handles.job_assigned);
        assert_eq!(wait, WAIT_OBJECT_0);
        assert_ne!(exit_code, 259, "exact worker must not remain STILL_ACTIVE");
        assert!(readback_close.is_ok(), "{readback_close:?}");
    }

    #[test]
    fn worker_process_lifecycle_reads_real_natural_exit_state() {
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut handles = spawn_real_process(
            &powershell,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "exit 23",
            ],
        );
        let verdict = wait_for_worker_process(&mut handles, 10_000)
            .unwrap_or_else(|error| panic!("wait for real cmd.exe child: {error}"));
        let close_failures = handles.close_checked();

        assert_eq!(
            verdict,
            WorkerProcessVerdict {
                pid: verdict.pid,
                timed_out: false,
                exit_code: 23,
            }
        );
        assert!(verdict.pid > 0);
        assert!(
            close_failures.is_empty(),
            "real cmd.exe handles must close: {close_failures:?}"
        );
    }

    #[test]
    fn worker_process_timeout_terminates_joins_and_reads_real_child() {
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut handles = spawn_real_process(
            &powershell,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ],
        );
        let verdict = wait_for_worker_process(&mut handles, 0)
            .unwrap_or_else(|error| panic!("terminate and read real sleeper: {error}"));
        let close_failures = handles.close_checked();

        assert!(verdict.pid > 0);
        assert!(verdict.timed_out);
        assert_eq!(
            verdict.exit_code, 1,
            "TerminateJobObject exit code must be read from the real process object"
        );
        assert!(
            close_failures.is_empty(),
            "real sleeper handles must close: {close_failures:?}"
        );
    }

    #[test]
    fn finalizer_reads_exact_terminal_state_after_kill_on_close_before_handle_release() {
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut handles = spawn_real_process(
            &powershell,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ],
        );

        let report = finalize_worker_process_handles(&mut handles, "causal_job_close_test");

        assert!(
            report.job_close_triggered,
            "live assigned child must use the verified kill-on-close job backstop"
        );
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!report.retained);
        assert_ne!(report.terminal_exit_code, None);
        assert_ne!(
            report.terminal_exit_code,
            Some(STILL_ACTIVE_EXIT_CODE),
            "process handle must not be released on a STILL_ACTIVE claim"
        );
        assert!(
            !handles.has_handles(),
            "job/process/thread handles may close only after exact terminal readback"
        );
        assert!(handles.terminal_verified);
    }

    #[test]
    fn terminal_process_and_thread_close_failures_retain_exact_owners_for_retry() {
        let _serial = retained_registry_test_guard();
        use windows::Win32::Foundation::{
            HANDLE_FLAG_PROTECT_FROM_CLOSE, HANDLE_FLAGS, SetHandleInformation,
        };

        retry_retained_worker_process_handles()
            .expect("prior desktop-worker retained owners must be clear");
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut handles = spawn_real_process(
            &powershell,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "exit 17",
            ],
        );
        let verdict = wait_for_worker_process(&mut handles, 10_000)
            .unwrap_or_else(|error| panic!("wait for protected-handle worker: {error}"));
        let process = handles.process().expect("terminal worker process handle");
        let thread = handles.thread().expect("terminal worker thread handle");
        for handle in [process, thread] {
            unsafe {
                SetHandleInformation(
                    handle,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE.0,
                    HANDLE_FLAG_PROTECT_FROM_CLOSE,
                )
            }
            .expect("protect terminal worker handle from close");
        }

        let finalization =
            finalize_worker_process_handles(&mut handles, "causal_process_thread_close_failure");
        let retained = desktop_worker_retained_owner_report()
            .active_owners
            .into_iter()
            .find(|owner| owner.pid == verdict.pid)
            .expect("failed process/thread closes must retain their exact raw handles");
        for handle in [process, thread] {
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_PROTECT_FROM_CLOSE.0, HANDLE_FLAGS(0))
            }
            .expect("clear terminal worker close protection");
        }
        let retry = retry_retained_worker_process_handles();
        let after = desktop_worker_retained_owner_report();

        assert!(finalization.retained, "{finalization:?}");
        assert!(retained.terminal_verified, "{retained:?}");
        assert_eq!(retained.terminal_exit_code, Some(17), "{retained:?}");
        assert!(retained.process_handle.is_some(), "{retained:?}");
        assert!(retained.thread_handle.is_some(), "{retained:?}");
        assert!(
            finalization
                .failures
                .iter()
                .any(|failure| failure.contains("CloseHandle(process) failed")),
            "{finalization:?}"
        );
        assert!(
            finalization
                .failures
                .iter()
                .any(|failure| failure.contains("CloseHandle(thread) failed")),
            "{finalization:?}"
        );
        assert!(retry.is_ok(), "{retry:?}");
        assert_eq!(after.active_owner_count, 0, "{after:?}");
    }

    #[test]
    fn drop_transfers_repeated_close_failure_to_process_global_exact_owner() {
        let _serial = retained_registry_test_guard();
        use windows::Win32::{
            Foundation::{
                HANDLE_FLAG_PROTECT_FROM_CLOSE, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
            },
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                WaitForSingleObject,
            },
        };

        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let handles = spawn_real_process(
            &powershell,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ],
        );
        let pid = handles.pid;
        let job = handles.job().expect("real worker job handle");
        let readback_handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .unwrap_or_else(|error| panic!("open exact drop-retention worker: {error}"));
        unsafe {
            SetHandleInformation(
                job,
                HANDLE_FLAG_PROTECT_FROM_CLOSE.0,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        }
        .expect("protect real job handle from close");

        drop(handles);
        let wait = unsafe { WaitForSingleObject(readback_handle, 10_000) };
        let exit_code =
            read_worker_exit_code(readback_handle, pid, "drop_retention_process_readback")
                .unwrap_or_else(|error| panic!("read exact drop-retention worker exit: {error}"));
        let retained_before_retry = lock_retained_worker_process_handles()
            .iter()
            .filter(|retained| retained.pid == pid)
            .cloned()
            .collect::<Vec<_>>();
        unsafe { SetHandleInformation(job, HANDLE_FLAG_PROTECT_FROM_CLOSE.0, HANDLE_FLAGS(0)) }
            .expect("clear retained job handle close protection");
        let retry = retry_retained_worker_process_handles();
        let retained_after_retry = lock_retained_worker_process_handles()
            .iter()
            .filter(|retained| retained.pid == pid)
            .count();
        let owner_report = desktop_worker_retained_owner_report();
        let readback_close = close_standalone_worker_handle(readback_handle, "readback_process");

        assert_eq!(wait, WAIT_OBJECT_0);
        assert_ne!(exit_code, STILL_ACTIVE_EXIT_CODE);
        assert_eq!(retained_before_retry.len(), 1, "{retained_before_retry:?}");
        assert!(retained_before_retry[0].job_handle.is_some());
        assert!(retained_before_retry[0].terminal_verified);
        assert!(retry.is_ok(), "{retry:?}");
        assert_eq!(retained_after_retry, 0);
        assert_eq!(owner_report.active_owner_count, 0);
        assert!(owner_report.active_owners.is_empty());
        let owner_id = retained_before_retry[0].owner_id;
        let owner_events = owner_report
            .evidence
            .iter()
            .filter(|evidence| evidence.owner.owner_id == owner_id)
            .collect::<Vec<_>>();
        assert_eq!(owner_events.len(), 2, "{owner_events:?}");
        assert_eq!(owner_events[0].event, "retained");
        assert_eq!(owner_events[1].event, "reaped");
        assert!(owner_events[0].owner.job_handle.is_some());
        assert!(!owner_events[0].owner.last_failure.is_empty());
        assert!(!owner_events[1].detail.is_empty());
        assert!(readback_close.is_ok(), "{readback_close:?}");
    }

    #[test]
    fn childless_job_close_failure_is_retained_and_reaped_before_spawn() {
        let _serial = retained_registry_test_guard();
        use windows::Win32::Foundation::{
            HANDLE_FLAG_PROTECT_FROM_CLOSE, HANDLE_FLAGS, SetHandleInformation,
        };

        retry_retained_worker_process_handles()
            .expect("prior desktop-worker retained owners must be clear");
        let job = create_worker_kill_on_close_job().expect("create childless worker job");
        unsafe {
            SetHandleInformation(
                job,
                HANDLE_FLAG_PROTECT_FROM_CLOSE.0,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        }
        .expect("protect childless job from close");

        let close = close_or_retain_standalone_worker_job(job, "causal_childless_job_test");
        let retained = desktop_worker_retained_owner_report()
            .active_owners
            .into_iter()
            .find(|owner| owner.job_handle == Some(job.0 as isize))
            .expect("failed childless job close must preserve its exact raw handle");
        unsafe { SetHandleInformation(job, HANDLE_FLAG_PROTECT_FROM_CLOSE.0, HANDLE_FLAGS(0)) }
            .expect("clear childless job close protection");
        let retry = retry_retained_worker_process_handles();
        let after = desktop_worker_retained_owner_report();

        assert!(close.is_err(), "protected close cannot claim success");
        assert!(!retained.child_created);
        assert_eq!(retained.pid, 0);
        assert!(retained.process_handle.is_none());
        assert!(retained.thread_handle.is_none());
        assert!(retry.is_ok(), "{retry:?}");
        assert!(
            after
                .active_owners
                .iter()
                .all(|owner| owner.owner_id != retained.owner_id),
            "successful retry must remove the exact childless owner"
        );
        assert!(after.evidence.iter().any(|evidence| {
            evidence.owner.owner_id == retained.owner_id && evidence.event == "reaped"
        }));
    }

    #[test]
    fn retained_owner_remains_visible_while_exact_handle_reap_is_in_progress() {
        let _serial = retained_registry_test_guard();
        use windows::Win32::Foundation::{
            HANDLE_FLAG_PROTECT_FROM_CLOSE, HANDLE_FLAGS, SetHandleInformation,
        };

        retry_retained_worker_process_handles()
            .expect("prior desktop-worker retained owners must be clear");
        let job = create_worker_kill_on_close_job().expect("create childless worker job");
        unsafe {
            SetHandleInformation(
                job,
                HANDLE_FLAG_PROTECT_FROM_CLOSE.0,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        }
        .expect("protect childless job from close");
        close_or_retain_standalone_worker_job(job, "causal_retry_visibility_setup")
            .expect_err("protected close must enter the retained-owner registry");

        let (retained, reap_guard) = take_retained_worker_owner_for_retry()
            .expect("retained owner must transfer into an observable reap attempt");
        let during = desktop_worker_retained_owner_report();
        let mut handles = WorkerProcessHandles::from_retained(retained);
        unsafe { SetHandleInformation(job, HANDLE_FLAG_PROTECT_FROM_CLOSE.0, HANDLE_FLAGS(0)) }
            .expect("clear childless job close protection");
        let finalization =
            finalize_worker_process_handles(&mut handles, "causal_retry_visibility_cleanup");
        drop(reap_guard);
        let after = desktop_worker_retained_owner_report();

        assert_eq!(during.reap_in_progress_count, 1, "{during:?}");
        assert_eq!(during.active_owner_count, 1, "{during:?}");
        assert!(
            during.active_owners.is_empty(),
            "the exact owner is local to the reap, not falsely absent: {during:?}"
        );
        assert!(!finalization.retained, "{finalization:?}");
        assert!(finalization.failures.is_empty(), "{finalization:?}");
        assert_eq!(after.reap_in_progress_count, 0, "{after:?}");
        assert_eq!(after.active_owner_count, 0, "{after:?}");
    }

    #[test]
    fn closing_real_worker_job_kills_tree_and_preserves_process_readback() {
        use windows::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                WaitForSingleObject,
            },
        };

        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut handles = spawn_real_process(
            &powershell,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ],
        );
        assert!(handles.job_assigned);
        let readback_handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                handles.pid,
            )
        }
        .unwrap_or_else(|error| {
            panic!(
                "open exact worker pid {} for readback: {error}",
                handles.pid
            )
        });

        let mut job_close_failures = Vec::new();
        handles.close_one("job", &mut job_close_failures);
        let wait = unsafe { WaitForSingleObject(readback_handle, 10_000) };
        let exit_code = read_worker_exit_code(readback_handle, handles.pid, "job_close_readback")
            .unwrap_or_else(|error| panic!("read exact job-closed worker: {error}"));
        let readback_close = close_standalone_worker_handle(readback_handle, "readback_process");
        let remaining_close_failures = handles.close_checked();

        assert!(job_close_failures.is_empty(), "{job_close_failures:?}");
        assert_eq!(
            wait, WAIT_OBJECT_0,
            "kill-on-close job must signal exact child"
        );
        assert_ne!(exit_code, 259, "exact child must not remain STILL_ACTIVE");
        assert!(readback_close.is_ok(), "{readback_close:?}");
        assert!(
            remaining_close_failures.is_empty(),
            "{remaining_close_failures:?}"
        );
    }

    #[test]
    fn desktop_worker_rejects_noncanonical_hwnd_before_process_dispatch() {
        for hwnd in [-1, 0, i64::from(u32::MAX) + 1, i64::MAX] {
            let error = run_worker(
                "desktop-name-must-not-be-opened",
                DesktopWorkerOp::Context,
                hwnd,
                None,
                false,
                None,
            )
            .expect_err("noncanonical HWND must fail before CreateProcessW");
            let data = error.data.expect("structured HWND validation data");
            assert_eq!(
                data.get("code").and_then(serde_json::Value::as_str),
                Some(error_codes::TOOL_PARAMS_INVALID)
            );
            assert_eq!(
                data.get("tool").and_then(serde_json::Value::as_str),
                Some("hidden_desktop_worker")
            );
        }
    }

    #[test]
    fn desktop_worker_launch_guard_rejects_noncanonical_hwnd_before_create_process() {
        let temp = WorkerTempPaths::new(false).expect("allocate launch-guard temp paths");
        for hwnd in [-1, 0, i64::from(u32::MAX) + 1, i64::MAX] {
            let error = launch_worker_process(
                "desktop-name-must-not-be-opened",
                DesktopWorkerOp::Context,
                hwnd,
                None,
                false,
                None,
                &temp,
            )
            .expect_err("final launch seam must reject noncanonical HWND");
            let data = error.data.expect("structured launch-guard validation data");
            assert_eq!(
                data.get("code").and_then(serde_json::Value::as_str),
                Some(error_codes::TOOL_PARAMS_INVALID)
            );
        }
    }

    #[test]
    fn desktop_worker_parse_rect_rejects_empty_capture_region_as_capture_invalid() {
        let error = parse_rect("0,0,0,10").expect_err("empty region should fail");

        assert_eq!(
            error.error_code.as_deref(),
            Some(error_codes::CAPTURE_TARGET_INVALID)
        );
        assert!(
            error
                .error_detail
                .as_deref()
                .is_some_and(|detail| detail.contains("empty desktop worker capture region"))
        );
    }

    #[test]
    fn desktop_worker_parse_rect_rejects_malformed_region_as_params_invalid() {
        let error = parse_rect("0,0,10").expect_err("malformed region should fail");

        assert_eq!(
            error.error_code.as_deref(),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn desktop_worker_requires_kernel_exit_and_json_envelope_to_agree() {
        let success = WorkerEnvelope {
            ok: true,
            payload: None,
            error_code: None,
            error_detail: None,
        };
        let failure = worker_error(error_codes::TOOL_INTERNAL_ERROR, "synthetic failure");

        validate_worker_exit_envelope(
            WorkerProcessVerdict {
                pid: 101,
                timed_out: false,
                exit_code: 0,
            },
            &success,
        )
        .expect("zero exit and success envelope must agree");
        validate_worker_exit_envelope(
            WorkerProcessVerdict {
                pid: 102,
                timed_out: false,
                exit_code: 7,
            },
            &failure,
        )
        .expect("nonzero exit and failure envelope must agree");

        let nonzero_success = validate_worker_exit_envelope(
            WorkerProcessVerdict {
                pid: 103,
                timed_out: false,
                exit_code: 9,
            },
            &success,
        )
        .expect_err("nonzero exit must never accept a success envelope");
        assert!(
            nonzero_success
                .message
                .contains("exit_code=9 requires envelope.ok=false, but JSON envelope.ok=true")
        );

        let zero_failure = validate_worker_exit_envelope(
            WorkerProcessVerdict {
                pid: 104,
                timed_out: false,
                exit_code: 0,
            },
            &failure,
        )
        .expect_err("zero exit must never accept a failure envelope");
        assert!(
            zero_failure
                .message
                .contains("exit_code=0 requires envelope.ok=true, but JSON envelope.ok=false")
        );
    }
}

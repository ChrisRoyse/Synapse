//! #2090 — OS-initiated shutdown handling for the supervised Windows daemon.
//!
//! # The gap this closes
//!
//! Before this module the daemon's only graceful exits were `POST /shutdown`
//! and a console Ctrl+C/Ctrl+Break that a hidden, supervisor-launched process
//! never receives. Every user logoff, restart and shutdown therefore killed the
//! daemon mid-flight: the Calyx vault was never flushed or closed, the
//! `daemon.pid` / `shell-job-store.pid` lifetime-lock sidecars survived, and
//! `daemon-run-current.json` kept `ended_at_unix_ms: null`. Since #2083 added a
//! boot verdict, that also meant **every** post-reboot boot logged
//! `previous_shutdown=dirty`, which would have made the signal worthless for
//! detecting real crashes.
//!
//! # Which notification actually fires for THIS process shape
//!
//! The daemon is a console-subsystem process launched hidden
//! (`wscript` → `powershell` → `Start-Process -WindowStyle Hidden`), running as
//! the interactive user under Task Scheduler. Two mechanisms are installed
//! because neither one alone covers the cases:
//!
//! 1. **`SetConsoleCtrlHandler`** — covers `CTRL_CLOSE_EVENT`, which the system
//!    sends "to all processes attached to a console when the user closes the
//!    console … or by clicking the **End Task** button command from Task
//!    Manager" ([HandlerRoutine], Microsoft Learn). Trigger name
//!    `os_console_close`.
//!
//!    Note what FSV showed about `taskkill /PID <pid>` (no `/F`): once the
//!    hidden window below exists, taskkill reaches the daemon through
//!    **`WM_CLOSE` on that window**, not through the console handler. The two
//!    arms therefore carry different trigger names (`os_window_close` vs
//!    `os_console_close`) so the exit record states which mechanism actually
//!    fired instead of implying one.
//!
//! 2. **A hidden top-level window handling `WM_QUERYENDSESSION` /
//!    `WM_ENDSESSION`** — required for logoff/shutdown. Microsoft is explicit
//!    that the console handler is *not* enough here:
//!
//!    > **Windows 7, Windows 8, Windows 8.1 and Windows 10:** If a console
//!    > application loads the gdi32.dll or user32.dll library, the
//!    > **HandlerRoutine** function that you specify when you call
//!    > **SetConsoleCtrlHandler** does not get called for the
//!    > **CTRL_LOGOFF_EVENT** and **CTRL_SHUTDOWN_EVENT** events. The operating
//!    > system recognizes processes that load gdi32.dll or user32.dll as
//!    > Windows applications rather than console applications. […] To receive
//!    > events when a user signs out or the device shuts down in these
//!    > circumstances, create a hidden window in your console application, and
//!    > then handle the **WM_QUERYENDSESSION** and **WM_ENDSESSION** window
//!    > messages that the hidden window receives. You can create a hidden
//!    > window by calling the **CreateWindowEx** method with the *dwExStyle*
//!    > parameter set to 0.
//!    >
//!    > — [SetConsoleCtrlHandler], Microsoft Learn
//!
//!    The Synapse daemon loads user32.dll unconditionally (`SendInput`, window
//!    enumeration, UI Automation), so it is one of the processes that clause
//!    describes. `HandlerRoutine`'s own reference agrees from the other side:
//!    `CTRL_LOGOFF_EVENT` "is received only by services. Interactive
//!    applications are terminated at logoff". The window is created as a real
//!    top-level window (parent `HWND_DESKTOP`), **not** a message-only
//!    (`HWND_MESSAGE`) window, because session-end messages are broadcast to
//!    top-level windows only.
//!
//! # Budgets (adopted from the documented table, read live from the host)
//!
//! [HandlerRoutine]'s "Timeouts" table:
//!
//! | Event | Circumstances | Timeout |
//! |---|---|---|
//! | `CTRL_CLOSE_EVENT` | *any* | `SPI_GETHUNGAPPTIMEOUT`, 5000 ms |
//! | `CTRL_LOGOFF_EVENT` | non-service | `SPI_GETWAITTOKILLTIMEOUT`, 5000 ms |
//! | `CTRL_SHUTDOWN_EVENT` | service process | `SPI_GETWAITTOKILLSERVICETIMEOUT`, 20000 ms |
//! | `CTRL_SHUTDOWN_EVENT` | non-service | `SPI_GETWAITTOKILLTIMEOUT`, 5000 ms |
//! | `CTRL_C`, `CTRL_BREAK` | *any* | **no timeout** |
//!
//! The daemon is **not** a service, so the honest ceiling is 5 s, not 20 s.
//! Rather than hard-code 5000, the real system parameter is read at drain time
//! (`SystemParametersInfoW(SPI_GETHUNGAPPTIMEOUT | SPI_GETWAITTOKILLTIMEOUT)`),
//! because both are operator-tunable (`HKCU\Control Panel\Desktop`
//! `HungAppTimeout` / `WaitToKillAppTimeout`). A safety margin is subtracted so
//! the final log line and process exit still land inside the window.
//!
//! `SetProcessShutdownParameters(0x300, SHUTDOWN_NORETRY)` is called at install
//! time. `0x300-0x3FF` is the documented "application reserved first shutdown
//! range" and the system shuts processes down from high level to low, so the
//! daemon is asked to stop *before* ordinary applications — it wants to flush
//! its vault while the machine is still healthy. `SHUTDOWN_NORETRY` suppresses
//! the "this app is preventing shutdown" retry dialog.
//!
//! `ShutdownBlockReasonCreate` is deliberately **not** used. It buys time by
//! parking the operator's shutdown behind a UI screen naming Synapse, which is
//! the wrong trade for a background daemon: a truthful partial drain that names
//! exactly what did and did not land is better than blocking a reboot.
//!
//! # Ordering inside the budget
//!
//! Deliberately *not* the HTTP drain's order. When the budget cannot fit
//! everything, what lands first must be what matters most:
//!
//! 1. **Release synthetic input.** `SendInput` key/button state is global to the
//!    OS input queue, not owned by this process. A key stranded down at
//!    shutdown outlives the daemon. This runs first, unconditionally, with no
//!    lock to wait on — operator safety before forensics (same rule as the
//!    #2082 startup sweep and the panic hook).
//! 2. **Flush + close the Calyx vault.** The only step that prevents WAL
//!    recovery on the next boot.
//! 3. **Write the graceful lifecycle exit record**, `ended_reason` naming the
//!    trigger (`os_console_close` / `os_window_close` / `os_logoff` / `os_shutdown` / `os_session_end`),
//!    carrying a per-step completion map so a partial drain is legible.
//! 4. **Release the lifetime locks** (PID sidecars + the shell-job clean
//!    shutdown marker), then append the release readback as a diagnostic event.
//!
//! The process then exits with code **0**, which the #2083 supervisor reads as
//! "stop, do not relaunch" — the right answer while the OS is going down.

#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use serde_json::json;

/// Everything the out-of-band drain needs, captured while the runtime is being
/// built. The handler runs on an OS-created thread that cannot reach the main
/// future's owned guards, so the reachable state is registered here instead.
///
/// # `Weak`, not `Arc` — this is load-bearing
///
/// The HTTP shutdown asserts an exact ownership postcondition on the m3 service
/// state (`M3StorageOwnerReadback { strong_owner_count, expected_owner_count: 1 }`)
/// before it will release the daemon lifetime locks. A process-global strong
/// clone parked in a `OnceLock` lives forever, so it makes that count 2 and
/// turns EVERY graceful `/shutdown` into
/// `MCP_HTTP_SHUTDOWN_POSTCONDITION_FAILED` + retained lock sidecars +
/// `ended_reason=top_level_error` — i.e. exactly the dirty shutdown #2090 and
/// #2092 exist to eliminate. FSV caught this. The OS-shutdown drain is an
/// observer of the runtime, never an owner of it, and a failed `upgrade()` is
/// reported truthfully instead of being papered over.
#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct OsShutdownDrainContext {
    pub(crate) m3_state: std::sync::Weak<std::sync::Mutex<crate::m3::M3State>>,
    pub(crate) db_path: PathBuf,
    pub(crate) shell_job_root: PathBuf,
}

#[cfg(windows)]
static CONTEXT: OnceLock<OsShutdownDrainContext> = OnceLock::new();
#[cfg(windows)]
static DRAIN_STARTED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
mod ffi {
    use std::ffi::c_void;

    pub(super) const WM_CLOSE: u32 = 0x0010;
    pub(super) const WM_QUERYENDSESSION: u32 = 0x0011;
    pub(super) const WM_ENDSESSION: u32 = 0x0016;
    pub(super) const WS_OVERLAPPED: u32 = 0x0000_0000;

    pub(super) const ENDSESSION_CLOSEAPP: usize = 0x0000_0001;
    pub(super) const ENDSESSION_CRITICAL: usize = 0x4000_0000;
    pub(super) const ENDSESSION_LOGOFF: usize = 0x8000_0000;

    pub(super) const SPI_GETHUNGAPPTIMEOUT: u32 = 0x0078;
    pub(super) const SPI_GETWAITTOKILLTIMEOUT: u32 = 0x007A;

    pub(super) const SHUTDOWN_NORETRY: u32 = 0x0000_0001;
    /// Application reserved *first* shutdown range (system stops high levels
    /// first). All processes start at 0x280.
    pub(super) const SHUTDOWN_LEVEL_FIRST_APPLICATION: u32 = 0x0000_0300;

    pub(super) const CTRL_C_EVENT: u32 = 0;
    pub(super) const CTRL_BREAK_EVENT: u32 = 1;
    pub(super) const CTRL_CLOSE_EVENT: u32 = 2;
    pub(super) const CTRL_LOGOFF_EVENT: u32 = 5;
    pub(super) const CTRL_SHUTDOWN_EVENT: u32 = 6;

    #[repr(C)]
    pub(super) struct WndClassExW {
        pub(super) cb_size: u32,
        pub(super) style: u32,
        pub(super) wnd_proc: Option<unsafe extern "system" fn(isize, u32, usize, isize) -> isize>,
        pub(super) cb_cls_extra: i32,
        pub(super) cb_wnd_extra: i32,
        pub(super) h_instance: isize,
        pub(super) h_icon: isize,
        pub(super) h_cursor: isize,
        pub(super) hbr_background: isize,
        pub(super) menu_name: *const u16,
        pub(super) class_name: *const u16,
        pub(super) h_icon_sm: isize,
    }

    #[repr(C)]
    pub(super) struct Point {
        pub(super) x: i32,
        pub(super) y: i32,
    }

    #[repr(C)]
    pub(super) struct Msg {
        pub(super) hwnd: isize,
        pub(super) message: u32,
        pub(super) w_param: usize,
        pub(super) l_param: isize,
        pub(super) time: u32,
        pub(super) pt: Point,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub(super) fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
        pub(super) fn SetProcessShutdownParameters(level: u32, flags: u32) -> i32;
        pub(super) fn GetModuleHandleW(module_name: *const u16) -> isize;
        pub(super) fn GetLastError() -> u32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        pub(super) fn RegisterClassExW(class: *const WndClassExW) -> u16;
        #[allow(clippy::too_many_arguments)]
        pub(super) fn CreateWindowExW(
            ex_style: u32,
            class_name: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: isize,
            menu: isize,
            instance: isize,
            param: *mut c_void,
        ) -> isize;
        pub(super) fn DefWindowProcW(
            hwnd: isize,
            message: u32,
            w_param: usize,
            l_param: isize,
        ) -> isize;
        pub(super) fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
        pub(super) fn TranslateMessage(msg: *const Msg) -> i32;
        pub(super) fn DispatchMessageW(msg: *const Msg) -> isize;
        pub(super) fn SystemParametersInfoW(
            action: u32,
            ui_param: u32,
            pv_param: *mut c_void,
            win_ini: u32,
        ) -> i32;
    }
}

#[cfg(windows)]
fn wide_nul(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads an operator-tunable shutdown timeout from the live system rather than
/// assuming the documented default, then subtracts a margin so the exit record
/// and final log line still land inside the OS window.
#[cfg(windows)]
fn budget_for(action: u32, documented_default_ms: u32) -> Duration {
    const MARGIN: Duration = Duration::from_millis(900);
    const FLOOR: Duration = Duration::from_millis(1200);
    const CEILING: Duration = Duration::from_secs(20);

    let mut raw_ms: u32 = 0;
    let ok = unsafe {
        ffi::SystemParametersInfoW(
            action,
            0,
            std::ptr::from_mut::<u32>(&mut raw_ms).cast::<std::ffi::c_void>(),
            0,
        )
    };
    let observed_ms = if ok != 0 && raw_ms > 0 {
        raw_ms
    } else {
        documented_default_ms
    };
    let budget = Duration::from_millis(u64::from(observed_ms))
        .min(CEILING)
        .saturating_sub(MARGIN)
        .max(FLOOR);
    tracing::info!(
        code = "MCP_DAEMON_OS_SHUTDOWN_BUDGET",
        spi_action = format!("0x{action:04x}"),
        spi_read_ok = ok != 0,
        observed_timeout_ms = observed_ms,
        documented_default_ms,
        margin_ms = MARGIN.as_millis(),
        drain_budget_ms = budget.as_millis(),
        "OS shutdown drain budget resolved from the live system parameter"
    );
    budget
}

/// The bounded drain. Never returns — it either exits the process after the
/// ordered drain, or (for a second concurrent trigger) parks until the OS ends
/// the process, because two concurrent drains over one vault is worse than one.
/// The `ended_reason` for an OS-triggered drain that did **not** get the vault
/// closed (#2131).
///
/// One static per trigger rather than a formatted string, because
/// `record_os_shutdown_exit` takes a `&'static str` cause — the lifecycle ledger
/// deliberately admits only compile-time-known causes so the boot verdict's
/// classification table can be exhaustive. Every value here ends in
/// `_vault_not_closed`, which is the suffix
/// `daemon_lifecycle::classify_exit_cause` reads as a forced exit.
#[cfg(windows)]
fn os_shutdown_cause_vault_not_closed(trigger: &'static str) -> &'static str {
    match trigger {
        "os_console_close" => "os_console_close_vault_not_closed",
        "os_window_close" => "os_window_close_vault_not_closed",
        "os_logoff" => "os_logoff_vault_not_closed",
        "os_shutdown" => "os_shutdown_vault_not_closed",
        "os_session_end" => "os_session_end_vault_not_closed",
        // Unreachable for the five call sites in this module. Deliberately not
        // the bare trigger: an unmapped trigger reaching here must not be able
        // to buy a `clean` verdict by falling through.
        _ => "os_unknown_trigger_vault_not_closed",
    }
}

#[cfg(windows)]
fn run_bounded_drain(trigger: &'static str, budget: Duration) -> ! {
    if DRAIN_STARTED.swap(true, Ordering::SeqCst) {
        tracing::warn!(
            code = "MCP_DAEMON_OS_SHUTDOWN_DRAIN_ALREADY_RUNNING",
            trigger,
            "a second OS shutdown notification arrived while the bounded drain was already running; \
             this thread parks instead of racing the first drain over the same vault"
        );
        std::thread::sleep(budget + Duration::from_secs(30));
        std::process::exit(0);
    }

    let started = Instant::now();
    let deadline = started + budget;
    tracing::warn!(
        code = "MCP_DAEMON_OS_SHUTDOWN_DRAIN_STARTED",
        trigger,
        pid = std::process::id(),
        budget_ms = budget.as_millis(),
        "OS-initiated shutdown observed; running the bounded drain \
         (synthetic input, then Calyx vault, then exit record, then lifetime locks)"
    );

    // --- 1. operator safety: one raw synthetic-input sweep, first, always ----
    let input_report = synapse_action::release_all_synthetic_input();
    let input_found_down = input_report.found_anything_down();
    tracing::warn!(
        code = "MCP_DAEMON_OS_SHUTDOWN_INPUT_RELEASED",
        trigger,
        found_anything_down = input_found_down,
        modifiers_found_down = %input_report.modifiers_found_down_labels(),
        modifier_releases_emitted = input_report.modifier_releases_emitted,
        button_releases_emitted = input_report.button_releases_emitted,
        tracked_strands_released = input_report.tracked_strands_released,
        emission_failures = input_report.emission_failures,
        elapsed_ms = started.elapsed().as_millis(),
        "synthetic input released before any storage work in the OS shutdown drain"
    );
    let input_released = input_report.emission_failures == 0;

    let context = CONTEXT.get();
    if context.is_none() {
        tracing::error!(
            code = "MCP_DAEMON_OS_SHUTDOWN_CONTEXT_MISSING",
            trigger,
            "the OS shutdown drain context was never registered; storage cannot be flushed from \
             this thread and the exit record will say so"
        );
    }

    // --- 2. Calyx vault flush + close ---------------------------------------
    let vault_deadline = std::cmp::min(deadline, Instant::now() + budget.mul_f32(0.6));
    let mut vault_status = "skipped_no_context".to_owned();
    let mut vault_latest_seq: Option<u64> = None;
    let mut vault_safe_to_unlock = false;
    // Upgraded only for the duration of the close: the strong reference must
    // never outlive this drain (see `OsShutdownDrainContext`).
    let m3_state = context.and_then(|context| context.m3_state.upgrade());
    if context.is_some() && m3_state.is_none() {
        vault_status = "storage_owner_already_released".to_owned();
        tracing::warn!(
            code = "MCP_DAEMON_OS_SHUTDOWN_VAULT_OWNER_GONE",
            trigger,
            "the m3 service state was already dropped when the OS shutdown drain ran; the vault \
             was closed by the runtime's own shutdown, not by this drain"
        );
    }
    if let Some(m3_state) = m3_state.as_ref() {
        vault_status = "lock_unavailable_before_deadline".to_owned();
        loop {
            match m3_state.try_lock() {
                Ok(mut state) => {
                    match state.close_calyx_vault_for_shutdown(trigger, false) {
                        Ok(readback) => {
                            vault_safe_to_unlock = readback.safe_to_unlock;
                            vault_latest_seq = readback.latest_seq;
                            vault_status = "closed".to_owned();
                            tracing::warn!(
                                code = "MCP_DAEMON_OS_SHUTDOWN_VAULT_CLOSED",
                                trigger,
                                safe_to_unlock = readback.safe_to_unlock,
                                latest_seq = ?readback.latest_seq,
                                elapsed_ms = started.elapsed().as_millis(),
                                "Calyx vault flushed and closed inside the OS shutdown budget"
                            );
                            if let Err(error) =
                                crate::m3::record_calyx_vault_close_event(&readback, "closed")
                            {
                                tracing::error!(
                                    code = "MCP_DAEMON_OS_SHUTDOWN_VAULT_EVENT_FAILED",
                                    trigger,
                                    error = %error,
                                    "the Calyx vault closed but its close event could not be recorded"
                                );
                            }
                        }
                        Err(error) => {
                            vault_status = format!("close_failed: {error}");
                            tracing::error!(
                                code = "MCP_DAEMON_OS_SHUTDOWN_VAULT_CLOSE_FAILED",
                                trigger,
                                error = %error,
                                "Calyx vault close failed during the OS shutdown drain; the next \
                                 boot will recover through the WAL"
                            );
                        }
                    }
                    break;
                }
                Err(std::sync::TryLockError::Poisoned(_poisoned)) => {
                    vault_status = "m3_state_lock_poisoned".to_owned();
                    tracing::error!(
                        code = "MCP_DAEMON_OS_SHUTDOWN_VAULT_LOCK_POISONED",
                        trigger,
                        "the m3 service-state lock is poisoned; the vault cannot be closed"
                    );
                    break;
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= vault_deadline {
                        tracing::error!(
                            code = "MCP_DAEMON_OS_SHUTDOWN_VAULT_LOCK_TIMEOUT",
                            trigger,
                            waited_ms = started.elapsed().as_millis(),
                            "the m3 service-state lock stayed held for the whole vault slice of the \
                             OS shutdown budget; the vault was NOT closed"
                        );
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
    drop(m3_state);

    // --- 3. graceful lifecycle exit record ----------------------------------
    //
    // #2131: the trigger name is only the whole truth when the vault actually
    // closed. This drain records an exit for every outcome — including the ones
    // where the m3 lock never came free, the close returned an error, or no
    // context was ever registered — and the boot verdict reads `ended_reason`
    // to decide `clean`. Naming the bare trigger in those cases would recreate
    // the exact defect #2131 exists to remove one layer down: a rollup that
    // says "clean" while the completion map beside it says the vault was never
    // closed. So a partial drain gets a distinct, unmistakable cause.
    let vault_closed = vault_status == "closed" || vault_status == "storage_owner_already_released";
    let recorded_cause = if vault_closed {
        trigger
    } else {
        os_shutdown_cause_vault_not_closed(trigger)
    };
    if !vault_closed {
        tracing::error!(
            code = "MCP_DAEMON_OS_SHUTDOWN_VAULT_NOT_CLOSED",
            trigger,
            recorded_cause,
            vault_status = %vault_status,
            "the OS shutdown drain did not get the Calyx vault closed; the exit record names a \
             vault_not_closed cause so the next boot cannot read this stop as clean"
        );
    }
    let record_detail = json!({
        "source": "os_shutdown_handler",
        "trigger": trigger,
        "recorded_cause": recorded_cause,
        "vault_closed": vault_closed,
        "budget_ms": budget.as_millis(),
        "elapsed_ms_at_record": started.elapsed().as_millis(),
        "completed": {
            "synthetic_input_released": input_released,
            "synthetic_input_found_down": input_found_down,
            "calyx_vault": vault_status,
            "calyx_vault_safe_to_unlock": vault_safe_to_unlock,
            "calyx_vault_latest_seq": vault_latest_seq,
        },
        "not_completed": {
            "http_session_close": "skipped: the OS budget cannot fit the /shutdown drain's session/socket quiescence phases",
            "server_task_stop": "skipped: same reason",
            "lifetime_lock_release": "attempted after this record; see the os_shutdown_lock_release diagnostic event",
        },
    });
    let record_result = crate::daemon_lifecycle::record_os_shutdown_exit(
        recorded_cause,
        record_detail,
        deadline.min(Instant::now() + Duration::from_millis(1500)),
    );
    let record_written = match record_result {
        Ok(()) => {
            tracing::warn!(
                code = "MCP_DAEMON_OS_SHUTDOWN_EXIT_RECORD_WRITTEN",
                trigger,
                recorded_cause,
                vault_closed,
                elapsed_ms = started.elapsed().as_millis(),
                "lifecycle exit record written with ended_reason naming the OS trigger, and \
                 whether the vault actually closed under it"
            );
            true
        }
        Err(error) => {
            tracing::error!(
                code = "MCP_DAEMON_OS_SHUTDOWN_EXIT_RECORD_FAILED",
                trigger,
                error = %error,
                "the graceful lifecycle exit record could not be written inside the OS budget; the \
                 next boot will report previous_shutdown=dirty, truthfully"
            );
            false
        }
    };

    // --- 4. lifetime locks ---------------------------------------------------
    let mut sidecar_results = Vec::new();
    if let Some(context) = context {
        crate::m4::mark_shell_job_supervisor_clean_shutdown();
        for (kind, path) in [
            (
                "storage_single_instance",
                context
                    .db_path
                    .join(crate::single_instance::DAEMON_PID_FILE),
            ),
            (
                "shell_job_store",
                context
                    .shell_job_root
                    .join(crate::single_instance::SHELL_JOB_STORE_PID_FILE),
            ),
        ] {
            let outcome = match std::fs::remove_file(&path) {
                Ok(()) => "removed".to_owned(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => "absent".to_owned(),
                Err(error) => format!("remove_failed: {error}"),
            };
            let absent_after = !path.exists();
            tracing::warn!(
                code = "MCP_DAEMON_OS_SHUTDOWN_LIFETIME_LOCK_SIDECAR",
                trigger,
                guard_kind = kind,
                pid_path = %path.display(),
                outcome = %outcome,
                absent_after,
                "daemon lifetime-lock PID sidecar handled during the OS shutdown drain; the advisory \
                 file locks themselves are released by the kernel at process exit"
            );
            sidecar_results.push(json!({
                "guard_kind": kind,
                "pid_path": path.display().to_string(),
                "outcome": outcome,
                "absent_after": absent_after,
            }));
        }
    }
    let locks_released = !sidecar_results.is_empty()
        && sidecar_results.iter().all(|entry| {
            entry
                .get("absent_after")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        });
    if let Err(error) = crate::daemon_lifecycle::record_os_shutdown_diagnostic(
        trigger,
        json!({
            "trigger": trigger,
            "lifetime_lock_sidecars": sidecar_results,
            "locks_released": locks_released,
            "exit_record_written": record_written,
            "elapsed_ms": started.elapsed().as_millis(),
        }),
    ) {
        tracing::error!(
            code = "MCP_DAEMON_OS_SHUTDOWN_LOCK_DIAGNOSTIC_FAILED",
            trigger,
            error = %error,
            "the lifetime-lock release readback could not be appended to the exit ledger"
        );
    }

    let elapsed_ms = started.elapsed().as_millis();
    tracing::warn!(
        code = "MCP_DAEMON_OS_SHUTDOWN_DRAIN_COMPLETE",
        trigger,
        pid = std::process::id(),
        budget_ms = budget.as_millis(),
        elapsed_ms,
        within_budget = started.elapsed() <= budget,
        synthetic_input_released = input_released,
        calyx_vault = %vault_status,
        exit_record_written = record_written,
        lifetime_locks_released = locks_released,
        "OS shutdown drain finished; exiting 0 so the supervisor parks instead of relaunching"
    );
    std::process::exit(0);
}

#[cfg(windows)]
unsafe extern "system" fn console_ctrl_handler(control_type: u32) -> i32 {
    match control_type {
        // Ctrl+C / Ctrl+Break already have a first-class in-runtime owner
        // (`wait_for_shutdown_signal`) that runs the FULL drain with no OS
        // deadline. Returning FALSE keeps this handler out of that path instead
        // of pre-empting it with the bounded one.
        ffi::CTRL_C_EVENT | ffi::CTRL_BREAK_EVENT => 0,
        ffi::CTRL_CLOSE_EVENT => {
            // Distinct from the window's WM_CLOSE trigger on purpose: the two
            // arrive by different OS mechanisms with different guarantees, and
            // an exit record that says which one fired is the difference
            // between evidence and a guess.
            run_bounded_drain(
                "os_console_close",
                budget_for(ffi::SPI_GETHUNGAPPTIMEOUT, 5000),
            );
        }
        // Documented as service-only for interactive processes, and suppressed
        // entirely once user32.dll is loaded — kept as a belt-and-braces arm so
        // a host where it DOES fire is drained rather than killed.
        ffi::CTRL_LOGOFF_EVENT => {
            run_bounded_drain("os_logoff", budget_for(ffi::SPI_GETWAITTOKILLTIMEOUT, 5000));
        }
        ffi::CTRL_SHUTDOWN_EVENT => {
            run_bounded_drain(
                "os_shutdown",
                budget_for(ffi::SPI_GETWAITTOKILLTIMEOUT, 5000),
            );
        }
        _ => 0,
    }
}

#[cfg(windows)]
unsafe extern "system" fn shutdown_window_proc(
    hwnd: isize,
    message: u32,
    w_param: usize,
    l_param: isize,
) -> isize {
    match message {
        // Answer "yes, I can end" immediately and do the work in WM_ENDSESSION,
        // per the Win32 session-end contract: a session that another app vetoes
        // must not have already cost this daemon its vault.
        ffi::WM_QUERYENDSESSION => {
            tracing::warn!(
                code = "MCP_DAEMON_OS_SESSION_END_QUERY",
                l_param = format!("0x{:08x}", l_param as usize),
                logoff = (l_param as usize & ffi::ENDSESSION_LOGOFF) != 0,
                close_app = (l_param as usize & ffi::ENDSESSION_CLOSEAPP) != 0,
                critical = (l_param as usize & ffi::ENDSESSION_CRITICAL) != 0,
                "WM_QUERYENDSESSION received on the hidden shutdown window; consenting to end"
            );
            1
        }
        ffi::WM_ENDSESSION => {
            if w_param == 0 {
                tracing::warn!(
                    code = "MCP_DAEMON_OS_SESSION_END_CANCELLED",
                    "WM_ENDSESSION(FALSE): the session end was cancelled; the daemon keeps running"
                );
                return 0;
            }
            let flags = l_param as usize;
            let trigger = if flags & ffi::ENDSESSION_LOGOFF != 0 {
                "os_logoff"
            } else if flags & ffi::ENDSESSION_CLOSEAPP != 0 {
                "os_session_end"
            } else {
                "os_shutdown"
            };
            run_bounded_drain(trigger, budget_for(ffi::SPI_GETWAITTOKILLTIMEOUT, 5000));
        }
        // `taskkill /PID <pid>` without `/F` posts WM_CLOSE to a process's
        // top-level windows. Treating it as a graceful stop is the same
        // contract an operator expects from a graceful taskkill.
        ffi::WM_CLOSE => {
            run_bounded_drain(
                "os_window_close",
                budget_for(ffi::SPI_GETHUNGAPPTIMEOUT, 5000),
            );
        }
        _ => unsafe { ffi::DefWindowProcW(hwnd, message, w_param, l_param) },
    }
}

#[cfg(windows)]
fn spawn_hidden_shutdown_window() {
    let builder = std::thread::Builder::new().name("synapse-os-shutdown-window".to_owned());
    let spawned = builder.spawn(|| {
        let class_name = wide_nul("SynapseDaemonOsShutdownWindow");
        let window_name = wide_nul("Synapse daemon OS shutdown listener");
        let instance = unsafe { ffi::GetModuleHandleW(std::ptr::null()) };
        let class = ffi::WndClassExW {
            cb_size: u32::try_from(std::mem::size_of::<ffi::WndClassExW>()).unwrap_or(80),
            style: 0,
            wnd_proc: Some(shutdown_window_proc),
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: instance,
            h_icon: 0,
            h_cursor: 0,
            hbr_background: 0,
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
            h_icon_sm: 0,
        };
        let atom = unsafe { ffi::RegisterClassExW(&raw const class) };
        if atom == 0 {
            tracing::error!(
                code = "MCP_DAEMON_OS_SHUTDOWN_WINDOW_CLASS_FAILED",
                last_error = unsafe { ffi::GetLastError() },
                "could not register the hidden shutdown window class; OS logoff/shutdown will NOT \
                 be drained gracefully on this host"
            );
            return;
        }
        // Parent HWND_DESKTOP (0), never HWND_MESSAGE: session-end messages are
        // broadcast to top-level windows only, and a message-only window is not
        // one. The window is created with dwExStyle 0 and never shown.
        let hwnd = unsafe {
            ffi::CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_name.as_ptr(),
                ffi::WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                0,
                0,
                instance,
                std::ptr::null_mut(),
            )
        };
        if hwnd == 0 {
            tracing::error!(
                code = "MCP_DAEMON_OS_SHUTDOWN_WINDOW_CREATE_FAILED",
                last_error = unsafe { ffi::GetLastError() },
                "could not create the hidden shutdown window; OS logoff/shutdown will NOT be \
                 drained gracefully on this host"
            );
            return;
        }
        tracing::info!(
            code = "MCP_DAEMON_OS_SHUTDOWN_WINDOW_READY",
            hwnd = format!("0x{hwnd:x}"),
            class = "SynapseDaemonOsShutdownWindow",
            "hidden top-level shutdown window created; WM_QUERYENDSESSION/WM_ENDSESSION/WM_CLOSE \
             now reach the bounded OS shutdown drain"
        );
        let mut message = ffi::Msg {
            hwnd: 0,
            message: 0,
            w_param: 0,
            l_param: 0,
            time: 0,
            pt: ffi::Point { x: 0, y: 0 },
        };
        loop {
            let result = unsafe { ffi::GetMessageW(&raw mut message, 0, 0, 0) };
            if result <= 0 {
                tracing::warn!(
                    code = "MCP_DAEMON_OS_SHUTDOWN_WINDOW_PUMP_ENDED",
                    result,
                    last_error = unsafe { ffi::GetLastError() },
                    "the hidden shutdown window message pump ended; OS session-end notifications \
                     are no longer observed"
                );
                return;
            }
            unsafe {
                ffi::TranslateMessage(&raw const message);
                ffi::DispatchMessageW(&raw const message);
            }
        }
    });
    if let Err(error) = spawned {
        tracing::error!(
            code = "MCP_DAEMON_OS_SHUTDOWN_WINDOW_THREAD_FAILED",
            error = %error,
            "could not spawn the hidden shutdown window thread"
        );
    }
}

/// Registers the drain context and installs both notification paths.
///
/// Idempotent per process: the context is a `OnceLock`, and a second call logs
/// the collision rather than installing a second handler.
#[cfg(windows)]
pub(crate) fn install(context: OsShutdownDrainContext) {
    let db_path = context.db_path.display().to_string();
    let shell_job_root = context.shell_job_root.display().to_string();
    if CONTEXT.set(context).is_err() {
        tracing::warn!(
            code = "MCP_DAEMON_OS_SHUTDOWN_ALREADY_INSTALLED",
            "the OS shutdown drain was already installed in this process; keeping the first context"
        );
        return;
    }

    let shutdown_params = unsafe {
        ffi::SetProcessShutdownParameters(
            ffi::SHUTDOWN_LEVEL_FIRST_APPLICATION,
            ffi::SHUTDOWN_NORETRY,
        )
    };
    let console_handler = unsafe { ffi::SetConsoleCtrlHandler(Some(console_ctrl_handler), 1) };
    if console_handler == 0 {
        tracing::error!(
            code = "MCP_DAEMON_OS_SHUTDOWN_CONSOLE_HANDLER_FAILED",
            last_error = unsafe { ffi::GetLastError() },
            "SetConsoleCtrlHandler failed; CTRL_CLOSE (console close / graceful taskkill / Task \
             Manager End Task) will NOT be drained gracefully"
        );
    }
    spawn_hidden_shutdown_window();

    tracing::info!(
        code = "MCP_DAEMON_OS_SHUTDOWN_HANDLERS_INSTALLED",
        pid = std::process::id(),
        console_ctrl_handler_installed = console_handler != 0,
        shutdown_parameters_set = shutdown_params != 0,
        shutdown_level = format!("0x{:03x}", ffi::SHUTDOWN_LEVEL_FIRST_APPLICATION),
        shutdown_noretry = true,
        db_path = %db_path,
        shell_job_root = %shell_job_root,
        "OS shutdown handling installed (#2090): console control handler for CTRL_CLOSE plus a \
         hidden top-level window for WM_QUERYENDSESSION/WM_ENDSESSION, because a process that \
         loads user32.dll never receives CTRL_LOGOFF/CTRL_SHUTDOWN"
    );
}

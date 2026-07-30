//! The daemon's declaration of its own scheduling Quality of Service (#1910).
//!
//! # Why the daemon has to say this itself
//!
//! Windows decides two separate things about this process: **when** it runs
//! (priority class) and, on a hybrid part, **where** it runs (core class, via
//! Intel Thread Director and the energy/performance preference). Both were
//! being decided by defaults nobody chose:
//!
//! * Task Scheduler's default `Priority` is 7, which maps to
//!   `BELOW_NORMAL_PRIORITY_CLASS`, and priority class is inherited by every
//!   child — so the whole chain (task -> wscript wrapper -> supervisor ->
//!   `synapse-mcp.exe`) ran below every ordinary process on the machine.
//! * With `ControlMask = 0`, the process has neither opted in nor out of power
//!   throttling, so Windows applies *its own heuristics* to infer a QoS level.
//!   Microsoft's guidance is explicit that this should be a decision, not an
//!   inference: "If an application does not explicitly enable
//!   `THREAD_POWER_THROTTLING_EXECUTION_SPEED`, the system will use its own
//!   heuristics to automatically infer a Quality of Service level."
//!
//! `scripts/synapse-setup.ps1` now registers the task at Priority 5, but that
//! only fixes the launch path it controls. This module makes the invariant hold
//! *however the daemon was started* — the scheduled task, the supervisor, a
//! manual run, a debugger — because the daemon serving interactive MCP calls is
//! latency-relevant regardless of who started it.
//!
//! # What it deliberately does not do
//!
//! It never *lowers* the priority class. If an operator has deliberately raised
//! this process to AboveNormal or High, that is their decision and this must not
//! quietly undo it. The assertion is a floor, not an assignment.
//!
//! It does not raise the daemon above Normal either. The user's foreground
//! application should win a tie; the defect was that the daemon could not even
//! draw.
//!
//! # Measured
//!
//! On the i7-1355U this was found on (2 P-cores + 8 E-cores, 12 logical), with
//! 6 competing Normal-priority CPU burners and an identical query over an
//! identical corpus, A/B/A on the same process:
//!
//! ```text
//! BelowNormal   p95 = 965 / 319 / 463 / 428 ms
//! Normal        p95 = 227 / 255 ms
//! ```
//!
//! The median barely moves and the tail moves a lot, which is the signature of
//! priority inversion rather than of slow work.

/// What the daemon observed and asserted about its own scheduling QoS.
///
/// Reported through `health` so a wrong state is readable from the surface the
/// rest of the system is verified through, instead of requiring an out-of-band
/// `Get-Process` the way #1910 did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProcessQosReport {
    /// Priority class observed before this module touched anything.
    pub priority_class_before: &'static str,
    /// Priority class read back from the OS after the assertion.
    pub priority_class_after: &'static str,
    /// Whether a raise was actually performed.
    pub priority_raised: bool,
    /// `ControlMask` read back from `GetProcessInformation`. A `1` in the
    /// execution-speed bit means the process has made an explicit choice; `0`
    /// means Windows is inferring one.
    pub power_throttling_control_mask: u32,
    /// `StateMask` read back. With the control bit set, `0` here means "do not
    /// throttle execution speed" — i.e. not EcoQoS.
    pub power_throttling_state_mask: u32,
    /// True once the readback proves execution-speed throttling is explicitly
    /// controlled and disabled.
    pub execution_speed_throttling_disabled: bool,
    /// Structured code for anything that did not reach the intended state.
    /// `None` when every assertion landed and read back correctly.
    pub failure_code: Option<&'static str>,
    /// Exactly what failed and what to do about it. `None` on success.
    pub failure_detail: Option<String>,
}

#[cfg(windows)]
mod imp {
    use super::ProcessQosReport;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess,
        GetPriorityClass, GetProcessInformation, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
        NORMAL_PRIORITY_CLASS, PROCESS_INFORMATION_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, REALTIME_PRIORITY_CLASS, SetPriorityClass, SetProcessInformation,
    };

    fn priority_name(value: u32) -> &'static str {
        match value {
            v if v == IDLE_PRIORITY_CLASS.0 => "Idle",
            v if v == BELOW_NORMAL_PRIORITY_CLASS.0 => "BelowNormal",
            v if v == NORMAL_PRIORITY_CLASS.0 => "Normal",
            v if v == ABOVE_NORMAL_PRIORITY_CLASS.0 => "AboveNormal",
            v if v == HIGH_PRIORITY_CLASS.0 => "High",
            v if v == REALTIME_PRIORITY_CLASS.0 => "Realtime",
            0 => "unreadable",
            _ => "unknown",
        }
    }

    /// Ordering for "is this at least Normal?". Deliberately explicit rather
    /// than comparing the raw constants, whose numeric values are not ordered
    /// the way the priority band is.
    fn rank(value: u32) -> u8 {
        match value {
            v if v == IDLE_PRIORITY_CLASS.0 => 0,
            v if v == BELOW_NORMAL_PRIORITY_CLASS.0 => 1,
            v if v == NORMAL_PRIORITY_CLASS.0 => 2,
            v if v == ABOVE_NORMAL_PRIORITY_CLASS.0 => 3,
            v if v == HIGH_PRIORITY_CLASS.0 => 4,
            v if v == REALTIME_PRIORITY_CLASS.0 => 5,
            // An unreadable class must not read as "already high enough".
            _ => 0,
        }
    }

    fn read_power_throttling(process: HANDLE) -> Option<PROCESS_POWER_THROTTLING_STATE> {
        let mut state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: 0,
            StateMask: 0,
        };
        let size = u32::try_from(std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>()).ok()?;
        // SAFETY: `state` is a correctly-sized, correctly-versioned struct of
        // exactly the class being requested, and the handle is the pseudo-handle
        // for the current process, which is always valid.
        let ok = unsafe {
            GetProcessInformation(
                process,
                PROCESS_INFORMATION_CLASS(ProcessPowerThrottling.0),
                std::ptr::from_mut(&mut state).cast(),
                size,
            )
        };
        ok.is_ok().then_some(state)
    }

    pub fn assert_interactive_qos() -> ProcessQosReport {
        // SAFETY: returns a pseudo-handle that needs no close and is always valid.
        let process = unsafe { GetCurrentProcess() };

        // SAFETY: pseudo-handle for the current process. Returns 0 on failure,
        // which `priority_name`/`rank` treat as unreadable rather than as low.
        let before_raw = unsafe { GetPriorityClass(process) };
        let before = priority_name(before_raw);
        let mut failure_code: Option<&'static str> = None;
        let mut failure_detail: Option<String> = None;

        // A floor, not an assignment: never undo an operator's deliberate raise.
        let mut raised = false;
        if rank(before_raw) < rank(NORMAL_PRIORITY_CLASS.0) {
            // SAFETY: pseudo-handle plus a documented priority-class constant.
            if let Err(error) = unsafe { SetPriorityClass(process, NORMAL_PRIORITY_CLASS) } {
                failure_code = Some("SYNAPSE_PROCESS_QOS_PRIORITY_RAISE_FAILED");
                failure_detail = Some(format!(
                    "SetPriorityClass(NORMAL) failed from {before}: {error}; the daemon will keep serving \
                     interactive MCP calls in a background priority band, where the request tail degrades \
                     sharply under competing load; remediation=confirm the process token may raise its own \
                     priority class and that no job object caps it, then restart the daemon"
                ));
            } else {
                raised = true;
            }
        }

        // SAFETY: pseudo-handle for the current process.
        let after_raw = unsafe { GetPriorityClass(process) };
        let after = priority_name(after_raw);
        if failure_code.is_none() && rank(after_raw) < rank(NORMAL_PRIORITY_CLASS.0) {
            failure_code = Some("SYNAPSE_PROCESS_QOS_PRIORITY_NOT_DURABLE");
            failure_detail = Some(format!(
                "SetPriorityClass reported success but the readback is still {after}; something outside this \
                 process is holding the priority class down; remediation=inspect for a job object or policy \
                 constraining this process, then restart the daemon"
            ));
        }

        // Opt OUT of execution-speed throttling. Control bit set + state bit
        // clear is the documented way to say "this process is latency-serving,
        // do not EcoQoS it" — as opposed to ControlMask=0, which asks Windows
        // to guess.
        let request = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        let size = u32::try_from(std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>())
            .unwrap_or_default();
        // SAFETY: correctly-sized, correctly-versioned struct of exactly the
        // class being set, with the current-process pseudo-handle.
        let set_result = unsafe {
            SetProcessInformation(
                process,
                PROCESS_INFORMATION_CLASS(ProcessPowerThrottling.0),
                std::ptr::from_ref(&request).cast(),
                size,
            )
        };
        if let Err(error) = set_result
            && failure_code.is_none()
        {
            failure_code = Some("SYNAPSE_PROCESS_QOS_POWER_THROTTLING_SET_FAILED");
            failure_detail = Some(format!(
                "SetProcessInformation(ProcessPowerThrottling) failed: {error}; Windows will keep inferring \
                 this process's QoS heuristically and may schedule it as EcoQoS on efficiency cores; \
                 remediation=this API needs Windows 11 or Server 2022 — confirm the host build, then restart \
                 the daemon"
            ));
        }

        // Read back from the OS rather than trusting the write.
        let (control_mask, state_mask) = read_power_throttling(process)
            .map_or((0, 0), |state| (state.ControlMask, state.StateMask));
        let disabled = control_mask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED != 0
            && state_mask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED == 0;
        if !disabled && failure_code.is_none() {
            failure_code = Some("SYNAPSE_PROCESS_QOS_POWER_THROTTLING_NOT_DURABLE");
            failure_detail = Some(format!(
                "power-throttling readback is control_mask=0x{control_mask:X} state_mask=0x{state_mask:X}, \
                 which does not prove execution-speed throttling is explicitly disabled; remediation=inspect \
                 the host's power policy and Windows build, then restart the daemon"
            ));
        }

        ProcessQosReport {
            priority_class_before: before,
            priority_class_after: after,
            priority_raised: raised,
            power_throttling_control_mask: control_mask,
            power_throttling_state_mask: state_mask,
            execution_speed_throttling_disabled: disabled,
            failure_code,
            failure_detail,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::ProcessQosReport;

    pub fn assert_interactive_qos() -> ProcessQosReport {
        ProcessQosReport {
            priority_class_before: "not_applicable",
            priority_class_after: "not_applicable",
            priority_raised: false,
            power_throttling_control_mask: 0,
            power_throttling_state_mask: 0,
            execution_speed_throttling_disabled: false,
            failure_code: None,
            failure_detail: None,
        }
    }
}

/// Asserts the daemon's interactive scheduling QoS and reports what landed.
///
/// Never fails the process: a daemon that cannot raise its own priority is
/// degraded, not broken, and refusing to start would be a worse outcome than
/// serving slowly. The failure is loud in the log and readable in `health`
/// rather than silent — which is the whole point, since the defect this closes
/// (#1910) was invisible on every surface the system reports through.
#[must_use]
pub fn assert_interactive_qos() -> ProcessQosReport {
    let report = imp::assert_interactive_qos();
    if let (Some(code), Some(detail)) = (report.failure_code, report.failure_detail.as_deref()) {
        tracing::error!(
            code,
            priority_class_before = report.priority_class_before,
            priority_class_after = report.priority_class_after,
            power_throttling_control_mask = report.power_throttling_control_mask,
            power_throttling_state_mask = report.power_throttling_state_mask,
            detail,
            "daemon scheduling QoS assertion did not reach the intended state"
        );
    } else {
        tracing::info!(
            code = "SYNAPSE_PROCESS_QOS_ASSERTED",
            priority_class_before = report.priority_class_before,
            priority_class_after = report.priority_class_after,
            priority_raised = report.priority_raised,
            power_throttling_control_mask = report.power_throttling_control_mask,
            power_throttling_state_mask = report.power_throttling_state_mask,
            execution_speed_throttling_disabled = report.execution_speed_throttling_disabled,
            "daemon asserted interactive scheduling QoS"
        );
    }
    report
}

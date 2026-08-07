//! Exact-target fence at the lowest global-input emission boundary (#2057).
//!
//! # Why this exists at all
//!
//! `SendInput` has no destination argument. Raymond Chen's description of the
//! API is exact: it "operates at the bottom level of the input stack … it is
//! just a backdoor into the same input mechanism that the keyboard and mouse
//! drivers use", and "doesn't know what will happen to the input" — routing is
//! decided later, by the window manager, from whatever holds the foreground at
//! *delivery* time. A successful return therefore proves nothing about where
//! the input landed.
//!
//! # Why the per-action fence was not enough
//!
//! Issue #1830 added a foreground fence in the MCP layer, checked once
//! immediately before each high-level `Action`. That closes the gap between
//! *request* and *dispatch*, but not the gap *inside* a dispatch. One action is
//! many OS emissions spread over real time:
//!
//! * `type_text` sleeps a sampled inter-keystroke interval and calls
//!   `SendInput` once per UTF-16 unit — a 200-character string is 200+ separate
//!   emissions across several seconds.
//! * `mouse_stroke` / `mouse_drag` / curved `mouse_move` emit one absolute move
//!   per timed sample, with a sleep between samples, bracketed by button
//!   down/up.
//! * `ViGEm` pad actions push one HID report per state change, with a hold sleep
//!   in the middle of a `Press`.
//!
//! If the human takes the foreground after the MCP check but before a later
//! emission, a *prefix* reaches the bound target and the *suffix* reaches the
//! human's window. That is a time-of-check/time-of-use bug, and the accepted
//! answer to "Best practices for using the `SendInput` API" states the same
//! failure mode plainly: "the input focus could change between when you send
//! the input and when the input is received by the target application …
//! There's no difference between doing a successive series of `SendInput`s vs a
//! single `SendInput` in this scenario." Batching does not help. Only checking
//! at every emission does.
//!
//! # What this module does
//!
//! [`arm`] records the exact bound target identity (HWND, PID, window class,
//! process name, title). [`guard_emission`] is then called immediately before
//! every global-input emission — every `SendInput` batch, every physical cursor
//! mutation, every `ViGEm` HID report — and re-reads the live foreground. On
//! drift it refuses *that* emission, so the sequence stops at the first
//! boundary after the foreground moved.
//!
//! The residual window (between the check and the window manager's routing
//! decision) cannot be closed from user mode; no Win32 API accepts a
//! destination for synthesized input. This shrinks it from "one action" to
//! "one `SendInput` call", which is the smallest achievable unit. That is
//! stated here rather than hidden, because a timing/timeout workaround would
//! be a lie about a guarantee the OS does not offer.
//!
//! # Identity, not handle value
//!
//! Comparing HWND values alone is not enough: USER handles are recycled. A
//! destroyed target whose handle is reissued to a different window would
//! compare equal. Every check therefore verifies `IsWindow`, the owning process
//! id, and the window class against the values captured at arm time — the same
//! snapshot discipline the `win-text-inject` crate documents for this exact
//! hazard ("capture this at hotkey press, not at injection time … injecting
//! into whatever happens to be foreground later is how text ends up in the
//! wrong application").
//!
//! # Cost
//!
//! The hot path is `GetForegroundWindow` + `IsWindow` +
//! `GetWindowThreadProcessId` + `GetClassNameW`, all user32 calls against
//! window-manager state with no cross-process message pump involved, plus one
//! uncontended lease mutex. The expensive foreground reads — `GetWindowTextW`
//! against a foreign message pump, `OpenProcess` +
//! `QueryFullProcessImageNameW` — are built **only on the refusal path**, where
//! a few hundred microseconds buy a diagnosable error. Per-UTF-16-unit checking
//! is therefore affordable against a >= 1 ms inter-keystroke interval.
//!
//! # Release cleanup is deliberately allowed through
//!
//! A refusal must not strand a held modifier or mouse button: a stuck `Ctrl` or
//! a stuck left button is a worse outcome for the human than a stray key-up
//! event in their window. Releases are themselves emissions, so this has to be
//! an explicit decision rather than an accident. Emissions are therefore
//! classified:
//!
//! * [`EmissionKind::Delivery`] — anything that *conveys intent* (key down,
//!   character, mouse down, motion, scroll, non-neutral pad report). Refused on
//!   drift.
//! * [`EmissionKind::Release`] — the up/neutral half of input we are holding.
//!   Evaluated against the fence exactly the same way, but **permitted to
//!   flow** when the fence is tripped, and logged at `warn` with
//!   `M2_FOREGROUND_FENCE_RELEASE_THROUGH_DRIFT` so the stray event is
//!   attributable. This extends the existing `release_all` doctrine (which
//!   already emits real key-ups for everything held, unconditionally) down to
//!   the per-emission boundary.
//!
//! # Layering: unarmed is not this module's refusal to make
//!
//! When no target is armed, [`guard_emission`] permits the emission. Arming is
//! what creates an exact-target expectation, and refusing unarmed *global*
//! input is the MCP foreground lane's job — it already fails closed there
//! (#1830), before any backend is reached. Duplicating that policy here would
//! also refuse internal emitters that legitimately have no bound target
//! (crash-recovery release, operator-panic release, cursor restore), which is
//! not what #2057 asks for. This module's contract is narrower and exact:
//! *if a target is armed, no emission may leave for anything else.*
//!
//! Background CDP/UIA/`PostMessage`/hidden-desktop routes never reach these
//! emission sites, so they remain independent and unfenced, as before.
//!
//! # The lease check is a hard per-emission gate, not a grace window (#2065)
//!
//! `evaluate` reads `crate::lease::status().held` before *every* emission and
//! refuses `Delivery` the instant it is false. #2065 — long `act_type` payloads
//! truncating at ~70 characters — was fixed entirely on the *lease* side (size
//! the lease from the planned emission timeline at the MCP acquisition site, and
//! heartbeat it from inside the emission loop within an armed ceiling), and this
//! module's decision path was deliberately left byte-for-byte unchanged. No
//! grace period, no "recently held" tolerance, no retry: an expired, released,
//! or operator-preempted lease still refuses at the exact next boundary, and so
//! does foreground drift. Any future fix that is tempted to soften this check
//! is fixing the wrong layer.

use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, json};

use crate::ActionError;

/// Human-readable provenance of every fence decision.
pub const SOURCE_OF_TRUTH: &str = "GetForegroundWindow + IsWindow + GetWindowThreadProcessId + GetClassNameW read immediately before the OS emission call";

/// The exact window that global input is currently bound to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForegroundTarget {
    /// Canonical wire HWND of the bound top-level window.
    pub hwnd: i64,
    /// Owning process id captured at arm time; guards HWND recycling.
    pub pid: u32,
    /// Window class captured at arm time; guards same-process HWND recycling.
    pub class_name: String,
    /// Executable file name, for operator-readable refusals.
    pub process_name: String,
    /// Window title, for operator-readable refusals.
    pub window_title: String,
    /// Which call site armed the fence.
    pub armed_by: &'static str,
}

/// A live window identity read back at refusal time.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WindowIdentity {
    pub hwnd: i64,
    pub pid: u32,
    pub class_name: String,
    pub process_name: String,
    pub window_title: String,
}

impl WindowIdentity {
    fn to_json(&self) -> Value {
        json!({
            "hwnd": self.hwnd,
            "pid": self.pid,
            "class_name": self.class_name,
            "process_name": self.process_name,
            "window_title": self.window_title,
        })
    }
}

/// Whether an emission conveys intent or only gives held input back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmissionKind {
    /// Conveys intent. Refused when the fence has drifted.
    Delivery,
    /// Returns input we are holding. Allowed through a tripped fence, logged.
    Release,
}

impl EmissionKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivery => "delivery",
            Self::Release => "release",
        }
    }
}

/// One global-input emission boundary: what is about to hit the OS, and where
/// it sits in the action's internal emission timeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmissionSite {
    /// Stable name of the physical emission step.
    pub stage: &'static str,
    pub kind: EmissionKind,
    /// Index of this emission within the action's own timeline, when the
    /// caller has one (UTF-16 unit index, stroke sample index, …).
    pub unit_index: Option<usize>,
    /// Total planned emissions in that timeline, when known.
    pub unit_total: Option<usize>,
}

impl EmissionSite {
    /// An emission that conveys intent and must be refused on drift.
    #[must_use]
    pub const fn delivery(stage: &'static str) -> Self {
        Self {
            stage,
            kind: EmissionKind::Delivery,
            unit_index: None,
            unit_total: None,
        }
    }

    /// An emission that returns held input; allowed through a tripped fence.
    #[must_use]
    pub const fn release(stage: &'static str) -> Self {
        Self {
            stage,
            kind: EmissionKind::Release,
            unit_index: None,
            unit_total: None,
        }
    }

    /// Records this emission's position in the action's internal timeline.
    #[must_use]
    pub const fn at(mut self, unit_index: usize) -> Self {
        self.unit_index = Some(unit_index);
        self
    }

    /// Records how many emissions the action planned in total.
    #[must_use]
    pub const fn of(mut self, unit_total: usize) -> Self {
        self.unit_total = Some(unit_total);
        self
    }
}

/// Why the fence refused, in the order the checks run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriftReason {
    /// The input lease that authorizes foreground delivery is gone.
    LeaseNotHeld,
    /// `GetForegroundWindow` returned null — no window owns the foreground.
    ForegroundNull,
    /// The foreground is a different window than the armed target.
    ForegroundWindowMismatch,
    /// The armed HWND value is no longer a live window.
    TargetWindowGone,
    /// The HWND value matches but the window behind it is not the armed one.
    TargetIdentityRecycled,
    /// The live foreground could not be read back at all.
    ForegroundUnreadable,
}

impl DriftReason {
    #[must_use]
    pub const fn detail_code(self) -> &'static str {
        match self {
            Self::LeaseNotHeld => "M2_EMISSION_FENCE_LEASE_NOT_HELD",
            Self::ForegroundNull => "M2_EMISSION_FENCE_FOREGROUND_NULL",
            Self::ForegroundWindowMismatch => "M2_EMISSION_FENCE_FOREGROUND_MISMATCH",
            Self::TargetWindowGone => "M2_EMISSION_FENCE_TARGET_WINDOW_GONE",
            Self::TargetIdentityRecycled => "M2_EMISSION_FENCE_TARGET_IDENTITY_RECYCLED",
            Self::ForegroundUnreadable => "M2_EMISSION_FENCE_FOREGROUND_UNREADABLE",
        }
    }

    #[must_use]
    pub const fn remediation(self) -> &'static str {
        match self {
            Self::LeaseNotHeld => {
                "re-acquire the foreground input lease for this session, re-bind the exact target, then re-issue the remaining input"
            }
            Self::TargetWindowGone | Self::TargetIdentityRecycled => {
                "the bound window no longer exists; re-resolve the target window and re-bind before re-issuing the remaining input"
            }
            _ => {
                "the human took the foreground; do not re-activate the target automatically — hand control back, then re-run act operation=invoke verb=focus_window and re-issue only the undelivered remainder"
            }
        }
    }
}

/// A refused (or logged-through) emission boundary, fully attributable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForegroundDrift {
    pub stage: &'static str,
    pub kind: EmissionKind,
    pub unit_index: Option<usize>,
    pub unit_total: Option<usize>,
    /// Monotonic count of gated emissions since the fence was armed.
    pub emission_index: u64,
    pub reason: DriftReason,
    pub expected: ForegroundTarget,
    pub actual: Option<WindowIdentity>,
    pub readback_error: Option<String>,
}

impl ForegroundDrift {
    /// One-line operator-facing explanation naming both windows.
    #[must_use]
    pub fn message(&self) -> String {
        let position = match (self.unit_index, self.unit_total) {
            (Some(index), Some(total)) => format!(" unit_index={index}/{total}"),
            (Some(index), None) => format!(" unit_index={index}"),
            _ => String::new(),
        };
        let actual = self.actual.as_ref().map_or_else(
            || "unreadable".to_owned(),
            |actual| {
                format!(
                    "hwnd 0x{:x} ({} [{}] — {:?})",
                    actual.hwnd, actual.process_name, actual.class_name, actual.window_title
                )
            },
        );
        format!(
            "global input refused at emission boundary {stage}{position} (emission_index={emission_index}, {reason}): the foreground is {actual}, not the bound target hwnd 0x{expected_hwnd:x} ({expected_process} [{expected_class}] — {expected_title:?}). Emission stopped before delivery; nothing further was sent.",
            stage = self.stage,
            emission_index = self.emission_index,
            reason = self.reason.detail_code(),
            expected_hwnd = self.expected.hwnd,
            expected_process = self.expected.process_name,
            expected_class = self.expected.class_name,
            expected_title = self.expected.window_title,
        )
    }

    /// Structured diagnostics carried through to the MCP error payload.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "detail_code": self.reason.detail_code(),
            "refused_before_delivery": true,
            "stage": self.stage,
            "emission_kind": self.kind.as_str(),
            "emission_index": self.emission_index,
            "unit_index": self.unit_index,
            "unit_total": self.unit_total,
            "expected": {
                "hwnd": self.expected.hwnd,
                "pid": self.expected.pid,
                "class_name": self.expected.class_name,
                "process_name": self.expected.process_name,
                "window_title": self.expected.window_title,
                "armed_by": self.expected.armed_by,
            },
            "actual": self.actual.as_ref().map_or(Value::Null, WindowIdentity::to_json),
            "readback_error": self.readback_error,
            "source_of_truth": SOURCE_OF_TRUTH,
            "remediation": self.reason.remediation(),
        })
    }

    fn log_refusal(&self) {
        tracing::error!(
            code = synapse_core::error_codes::ACTION_FOREGROUND_LOST,
            detail_code = self.reason.detail_code(),
            stage = self.stage,
            emission_kind = self.kind.as_str(),
            emission_index = self.emission_index,
            unit_index = ?self.unit_index,
            unit_total = ?self.unit_total,
            expected_hwnd = self.expected.hwnd,
            expected_pid = self.expected.pid,
            expected_class = %self.expected.class_name,
            expected_process = %self.expected.process_name,
            expected_title = %self.expected.window_title,
            actual_hwnd = self.actual.as_ref().map_or(0, |actual| actual.hwnd),
            actual_pid = self.actual.as_ref().map_or(0, |actual| actual.pid),
            actual_class = %self.actual.as_ref().map_or("", |actual| actual.class_name.as_str()),
            actual_process = %self.actual.as_ref().map_or("", |actual| actual.process_name.as_str()),
            actual_title = %self.actual.as_ref().map_or("", |actual| actual.window_title.as_str()),
            refused_before_delivery = true,
            "refused global input at the OS emission boundary: the foreground is not the bound target"
        );
    }

    fn log_release_through_drift(&self) {
        tracing::warn!(
            code = "M2_FOREGROUND_FENCE_RELEASE_THROUGH_DRIFT",
            drift_detail_code = self.reason.detail_code(),
            stage = self.stage,
            emission_index = self.emission_index,
            unit_index = ?self.unit_index,
            expected_hwnd = self.expected.hwnd,
            expected_process = %self.expected.process_name,
            actual_hwnd = self.actual.as_ref().map_or(0, |actual| actual.hwnd),
            actual_process = %self.actual.as_ref().map_or("", |actual| actual.process_name.as_str()),
            "emitting a held-input release through a tripped foreground fence: stranding a held key or button is worse than one stray release event in the window that took the foreground"
        );
    }
}

fn cell() -> &'static Mutex<Option<ForegroundTarget>> {
    static CELL: OnceLock<Mutex<Option<ForegroundTarget>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn guard() -> std::sync::MutexGuard<'static, Option<ForegroundTarget>> {
    cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static EMISSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Binds global input to one exact window.
///
/// The caller supplies an identity read from live Win32 state (a verified
/// `GetForegroundWindow` readback, or a resolved agent-owned target). The
/// emission counter restarts so an emission index is always relative to the
/// current binding.
pub fn arm(target: ForegroundTarget) {
    EMISSION_SEQ.store(0, Ordering::SeqCst);
    tracing::info!(
        code = "M2_EMISSION_FENCE_ARMED",
        hwnd = target.hwnd,
        pid = target.pid,
        class_name = %target.class_name,
        process_name = %target.process_name,
        window_title = %target.window_title,
        armed_by = target.armed_by,
        "global-input emission fence bound to an exact target window"
    );
    *guard() = Some(target);
}

/// Releases the binding. Returns the target that was armed, if any.
pub fn disarm(reason: &str) -> Option<ForegroundTarget> {
    let previous = guard().take();
    if let Some(previous) = &previous {
        tracing::info!(
            code = "M2_EMISSION_FENCE_DISARMED",
            reason,
            hwnd = previous.hwnd,
            armed_by = previous.armed_by,
            emissions_gated = EMISSION_SEQ.load(Ordering::SeqCst),
            "global-input emission fence unbound"
        );
    }
    EMISSION_SEQ.store(0, Ordering::SeqCst);
    previous
}

/// The currently bound target, for diagnostics and response readback.
#[must_use]
pub fn armed() -> Option<ForegroundTarget> {
    guard().clone()
}

/// Number of emissions gated since the fence was armed.
#[must_use]
pub fn emissions_gated() -> u64 {
    EMISSION_SEQ.load(Ordering::SeqCst)
}

/// Evaluates the fence at a named boundary **without** consuming an emission
/// index, for callers that gate a whole action rather than one OS emission.
///
/// Returns `Ok(())` when unarmed — see the module docs on layering.
///
/// # Errors
///
/// Returns the structured [`ForegroundDrift`] when a target is armed and the
/// live foreground is not that target.
pub fn check(stage: &'static str) -> Result<(), Box<ForegroundDrift>> {
    let Some(expected) = armed() else {
        return Ok(());
    };
    evaluate(&expected).map_or(Ok(()), |finding| {
        Err(Box::new(finding.into_drift(
            EmissionSite::delivery(stage),
            EMISSION_SEQ.load(Ordering::SeqCst),
            expected,
        )))
    })
}

/// Gates one OS emission on the exact bound target.
///
/// Call this immediately before the `SendInput` / cursor-mutation / HID-report
/// call itself — not before the loop that contains it.
///
/// # Errors
///
/// Returns [`ActionError::ForegroundEmissionRefused`] when the emission
/// conveys intent and the live foreground is no longer the bound target.
/// [`EmissionKind::Release`] emissions never return an error; they are logged
/// and allowed through so held input is not stranded.
pub fn guard_emission(site: EmissionSite) -> Result<(), ActionError> {
    let emission_index = EMISSION_SEQ.fetch_add(1, Ordering::SeqCst);
    let Some(expected) = armed() else {
        return Ok(());
    };
    let Some(finding) = evaluate(&expected) else {
        return Ok(());
    };
    let drift = finding.into_drift(site, emission_index, expected);
    if site.kind == EmissionKind::Release {
        drift.log_release_through_drift();
        return Ok(());
    }
    drift.log_refusal();
    Err(ActionError::ForegroundEmissionRefused {
        detail: drift.message(),
        drift: Box::new(drift),
    })
}

struct Finding {
    reason: DriftReason,
    actual: Option<WindowIdentity>,
    readback_error: Option<String>,
}

impl Finding {
    fn into_drift(
        self,
        site: EmissionSite,
        emission_index: u64,
        expected: ForegroundTarget,
    ) -> ForegroundDrift {
        ForegroundDrift {
            stage: site.stage,
            kind: site.kind,
            unit_index: site.unit_index,
            unit_total: site.unit_total,
            emission_index,
            reason: self.reason,
            expected,
            actual: self.actual,
            readback_error: self.readback_error,
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use synapse_core::win32_hwnd::{hwnd_from_wire, hwnd_to_wire};
    use windows::Win32::{
        Foundation::{CloseHandle, HWND},
        System::Threading::{
            OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{
            GetClassNameW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
        },
    };
    use windows::core::PWSTR;

    use super::{DriftReason, Finding, ForegroundTarget, WindowIdentity};

    const CLASS_NAME_MAX: usize = 256;

    fn native(wire: i64) -> Option<HWND> {
        hwnd_from_wire(wire).map(|value| HWND(value as *mut core::ffi::c_void))
    }

    fn class_name_utf16(hwnd: HWND) -> Option<([u16; CLASS_NAME_MAX], usize)> {
        let mut buffer = [0_u16; CLASS_NAME_MAX];
        // SAFETY: `buffer` is a valid writable UTF-16 buffer for the call.
        let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
        if len <= 0 {
            return None;
        }
        Some((buffer, usize::try_from(len).unwrap_or(0)))
    }

    fn class_name(hwnd: HWND) -> String {
        class_name_utf16(hwnd)
            .map(|(buffer, len)| String::from_utf16_lossy(&buffer[..len]))
            .unwrap_or_default()
    }

    /// Compares the live class against the armed one without allocating; the
    /// hot path runs this once per emission.
    fn class_name_matches(hwnd: HWND, expected: &str) -> bool {
        if expected.is_empty() {
            // Nothing was captured at arm time, so there is nothing to
            // contradict. HWND + PID still carry the identity check.
            return true;
        }
        let Some((buffer, len)) = class_name_utf16(hwnd) else {
            return false;
        };
        buffer[..len].iter().copied().eq(expected.encode_utf16())
    }

    fn window_title(hwnd: HWND) -> String {
        let mut buffer = vec![0_u16; 512];
        // SAFETY: `buffer` is a valid writable UTF-16 buffer for the call.
        let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        String::from_utf16_lossy(&buffer[..usize::try_from(len).unwrap_or(0)])
    }

    fn pid_of(hwnd: HWND) -> u32 {
        let mut pid = 0_u32;
        // SAFETY: `pid` is a valid writable u32 for the duration of the call.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
        }
        pid
    }

    fn process_name(pid: u32) -> String {
        if pid == 0 {
            return String::new();
        }
        // SAFETY: handle is closed on every path below.
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return format!("pid-{pid}");
        };
        let mut buffer = vec![0_u16; 1024];
        let mut len = u32::try_from(buffer.len()).unwrap_or(0);
        // SAFETY: `buffer`/`len` describe the same writable UTF-16 buffer.
        let query = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_FORMAT(0),
                PWSTR(buffer.as_mut_ptr()),
                &raw mut len,
            )
        };
        // SAFETY: `handle` came from a successful OpenProcess and is not reused.
        let _close = unsafe { CloseHandle(handle) };
        if query.is_err() {
            return format!("pid-{pid}");
        }
        let path = String::from_utf16_lossy(&buffer[..usize::try_from(len).unwrap_or(0)]);
        Path::new(&path).file_name().map_or_else(
            || format!("pid-{pid}"),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// Full identity read. Only used on the refusal path.
    pub(super) fn identity(hwnd: HWND) -> WindowIdentity {
        let pid = pid_of(hwnd);
        WindowIdentity {
            hwnd: hwnd_to_wire(hwnd.0 as isize),
            pid,
            class_name: class_name(hwnd),
            process_name: process_name(pid),
            window_title: window_title(hwnd),
        }
    }

    /// Resolves the full identity of a wire HWND for arming.
    pub(super) fn identity_of_wire(wire: i64) -> Option<WindowIdentity> {
        let hwnd = native(wire)?;
        // SAFETY: `hwnd` is a candidate USER handle; IsWindow tolerates stale ones.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return None;
        }
        Some(identity(hwnd))
    }

    /// The whole per-emission hot path. Returns `None` when the bound target
    /// still holds the foreground.
    #[allow(
        clippy::needless_pass_by_ref_mut,
        reason = "no mutable borrows are taken; signature kept by-ref for the caller"
    )]
    pub(super) fn evaluate(expected: &ForegroundTarget) -> Option<Finding> {
        if !crate::lease::status().held {
            return Some(Finding {
                reason: DriftReason::LeaseNotHeld,
                actual: None,
                readback_error: Some(
                    "synapse_action::lease::status().held was false at the emission boundary"
                        .to_owned(),
                ),
            });
        }
        // SAFETY: no arguments; returns a borrowed window-manager handle.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.0.is_null() {
            return Some(Finding {
                reason: DriftReason::ForegroundNull,
                actual: None,
                readback_error: Some(
                    "GetForegroundWindow returned null at the emission boundary".to_owned(),
                ),
            });
        }
        let Some(expected_native) = native(expected.hwnd) else {
            return Some(Finding {
                reason: DriftReason::TargetWindowGone,
                actual: Some(identity(foreground)),
                readback_error: Some(format!(
                    "armed hwnd {} is not a canonical USER handle",
                    expected.hwnd
                )),
            });
        };
        if hwnd_to_wire(foreground.0 as isize) != hwnd_to_wire(expected_native.0 as isize) {
            return Some(Finding {
                reason: DriftReason::ForegroundWindowMismatch,
                actual: Some(identity(foreground)),
                readback_error: None,
            });
        }
        // Same handle value. Prove it is still the same *window*: USER handles
        // are recycled, so a destroyed target reissued to another window would
        // compare equal above.
        // SAFETY: `foreground` is a window-manager handle; IsWindow tolerates stale ones.
        if !unsafe { IsWindow(Some(foreground)) }.as_bool() {
            return Some(Finding {
                reason: DriftReason::TargetWindowGone,
                actual: None,
                readback_error: Some(format!(
                    "IsWindow(0x{:x}) is false: the bound window was destroyed",
                    expected.hwnd
                )),
            });
        }
        let live_pid = pid_of(foreground);
        if live_pid != expected.pid {
            return Some(Finding {
                reason: DriftReason::TargetIdentityRecycled,
                actual: Some(identity(foreground)),
                readback_error: Some(format!(
                    "hwnd 0x{:x} now belongs to pid {live_pid}, not the bound pid {}",
                    expected.hwnd, expected.pid
                )),
            });
        }
        if !class_name_matches(foreground, &expected.class_name) {
            return Some(Finding {
                reason: DriftReason::TargetIdentityRecycled,
                actual: Some(identity(foreground)),
                readback_error: Some(format!(
                    "hwnd 0x{:x} now has a different window class than the bound class {:?}",
                    expected.hwnd, expected.class_name
                )),
            });
        }
        None
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Finding, ForegroundTarget, WindowIdentity};

    pub(super) fn identity_of_wire(_wire: i64) -> Option<WindowIdentity> {
        None
    }

    pub(super) fn evaluate(_expected: &ForegroundTarget) -> Option<Finding> {
        // No Win32 foreground exists here, and no global-input emission site is
        // compiled in either, so there is nothing to fence.
        None
    }
}

fn evaluate(expected: &ForegroundTarget) -> Option<Finding> {
    platform::evaluate(expected)
}

/// Reads the live identity (HWND, PID, class, process, title) of a window, for
/// callers that need to arm the fence with a verified snapshot.
#[must_use]
pub fn window_identity(hwnd: i64) -> Option<WindowIdentity> {
    platform::identity_of_wire(hwnd)
}

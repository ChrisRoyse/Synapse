use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use serde::Serialize;

use crate::ActionResult;

/// Interrupt generations are deliberately split.
///
/// `release` wakes any interrupt-aware software input path for ordinary
/// release-all/foreground loss as well as the operator hotkey.
/// `operator_panic` advances only for the physical operator panic control and
/// is therefore safe to use as an authority-supersession signal.
#[derive(Debug)]
struct InterruptEpochs {
    release: AtomicU64,
    operator_panic: AtomicU64,
    operator_panic_publications_in_flight: AtomicU64,
    operator_panic_outstanding: AtomicU64,
    operator_panic_finalizations_outstanding: AtomicU64,
    operator_panic_accounting_incident: AtomicBool,
}

impl InterruptEpochs {
    const fn new() -> Self {
        Self {
            release: AtomicU64::new(0),
            operator_panic: AtomicU64::new(0),
            operator_panic_publications_in_flight: AtomicU64::new(0),
            operator_panic_outstanding: AtomicU64::new(0),
            operator_panic_finalizations_outstanding: AtomicU64::new(0),
            operator_panic_accounting_incident: AtomicBool::new(false),
        }
    }
}

/// Unique ownership token for one published physical operator-panic event.
///
/// The token is intentionally not `Clone`: only the callback that owns the
/// exact published event can acknowledge K1 and consume it at K2 completion.
#[derive(Debug)]
pub struct OperatorPanicSafetyToken {
    epochs: &'static InterruptEpochs,
    generation: u64,
    k1_preemption_acknowledged: bool,
    accounting_consumed: bool,
}

impl OperatorPanicSafetyToken {
    /// Exact monotonically increasing generation carried by this owner.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for OperatorPanicSafetyToken {
    fn drop(&mut self) {
        if !self.accounting_consumed {
            record_operator_panic_safety_incident_for(self.epochs);
        }
    }
}

/// Unique owner of the physical lease reconciliation required after a K2 wave.
///
/// Overlapping waves receive separate finalization owners and cannot consume
/// each other's accounting.
#[derive(Debug)]
pub struct OperatorPanicSafetyFinalization {
    epochs: &'static InterruptEpochs,
    generation: u64,
}

impl OperatorPanicSafetyFinalization {
    /// Exact operator-lease generation this owner must reconcile.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Result of consuming one exact K2 generation.
#[derive(Debug)]
pub enum OperatorPanicSafetyCompletion {
    /// Other published generations still own K2 work.
    Pending,
    /// This completion closed a wave and exclusively owns its lease readback.
    Finalize(OperatorPanicSafetyFinalization),
}

/// Process-global safety accounting readback used by shutdown verdicts.
#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorPanicSafetyReadback {
    pub epoch: u64,
    pub publications_in_flight: u64,
    pub outstanding_generations: u64,
    pub outstanding_finalizations: u64,
    pub accounting_incident: bool,
    pub pending: bool,
}

static GLOBAL_INTERRUPT_EPOCHS: InterruptEpochs = InterruptEpochs::new();

fn interrupt_epochs() -> &'static InterruptEpochs {
    &GLOBAL_INTERRUPT_EPOCHS
}

pub const OPERATOR_HOTKEY_ENV: &str = "SYNAPSE_OPERATOR_HOTKEY";
pub const OPERATOR_HOTKEY_COMPAT_ENV: &str = "SYNAPSE_MCP_OPERATOR_HOTKEY";
pub const DEFAULT_OPERATOR_HOTKEY: &str = "ctrl+alt+shift+p";

/// Result of a checked, bounded operator-hotkey shutdown.
///
/// A stop request is not a terminal-state verdict. Callers that own daemon
/// lifetime locks must use [`Self::owners_quiescent`] and retain those locks
/// until process teardown whenever either owned thread remains live or kernel
/// unregister state is unresolved.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorHotkeyShutdownReport {
    pub reason: &'static str,
    pub owner_id: u64,
    pub timeout_ms: u64,
    #[serde(flatten)]
    pub stop: OperatorHotkeyStopReport,
    #[serde(flatten)]
    pub wake_observation: OperatorHotkeyWakeObservationReport,
    #[serde(flatten)]
    pub wake_message: OperatorHotkeyWakeMessageReport,
    #[serde(flatten)]
    pub kernel: OperatorHotkeyKernelReport,
    #[serde(flatten)]
    pub threads: OperatorHotkeyThreadOwnersReport,
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorHotkeyStopReport {
    pub stop_requested: bool,
    pub signal_sender_cleared: bool,
    /// True only after the exact installation reservation was released. The
    /// sender is disconnected first so the worker can stop, but that alone
    /// must never admit a replacement while either old thread remains live.
    pub install_slot_released: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorHotkeyWakeObservationReport {
    pub hook_owner_observed_live_before_wake: bool,
    pub hook_owner_observed_live_after_wake: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorHotkeyWakeMessageReport {
    pub wake_message_attempted: bool,
    pub wake_message_sent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorHotkeyKernelReport {
    pub low_level_hook_was_installed: Option<bool>,
    pub low_level_hook_unregistered: Option<bool>,
    pub register_hotkey_backup_was_registered: Option<bool>,
    pub register_hotkey_backup_unregistered: Option<bool>,
    /// False when any installed kernel registration could not be proven
    /// released, including a hook-thread panic with unobservable cleanup.
    pub kernel_owners_released: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorHotkeyThreadOwnersReport {
    #[serde(flatten)]
    pub hook: OperatorHotkeyHookThreadReport,
    #[serde(flatten)]
    pub worker: OperatorHotkeyWorkerThreadReport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorHotkeyHookThreadReport {
    pub hook_thread_terminal: bool,
    pub hook_thread_joined: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorHotkeyWorkerThreadReport {
    pub worker_thread_terminal: bool,
    pub worker_thread_joined: bool,
}

impl OperatorHotkeyShutdownReport {
    /// True only when both exact thread owners have reached a terminal state,
    /// their owned `JoinHandle`s were joined, and every installed kernel
    /// registration was proven released. A liveness observation alone never
    /// satisfies this verdict when join or unregister state is unproven.
    #[must_use]
    pub const fn owners_quiescent(&self) -> bool {
        self.threads.hook.hook_thread_terminal
            && self.threads.hook.hook_thread_joined
            && self.threads.worker.worker_thread_terminal
            && self.threads.worker.worker_thread_joined
            && self.kernel.kernel_owners_released
            && self.stop.install_slot_released
    }

    /// Converts every cleanup failure into the action-layer error contract.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ActionError::BackendUnavailable`] when an owner remains
    /// live or any signal, unregister, or join postcondition failed.
    pub fn verdict(&self) -> ActionResult<()> {
        if self.owners_quiescent() && self.failures.is_empty() {
            return Ok(());
        }
        Err(crate::ActionError::BackendUnavailable {
            detail: format!(
                "operator hotkey shutdown failed: reason={} owners_quiescent={} failures={:?} report={self:?}",
                self.reason,
                self.owners_quiescent(),
                self.failures
            ),
        })
    }
}

/// Process-global record of how the operator panic hotkey resolved at startup,
/// so liveness/health surfaces can report a degraded kill-switch instead of the
/// failure being invisible. Lock-free: written once during startup, read by
/// `/health`.
static OPERATOR_HOTKEY_STATUS: AtomicU8 = AtomicU8::new(OperatorHotkeyStatus::Unknown as u8);

/// Resolution of the operator panic hotkey for this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorHotkeyStatus {
    /// Startup has not recorded a result yet.
    Unknown = 0,
    /// The global hotkey is registered and the kill-switch is armed.
    Registered = 1,
    /// Disabled by explicit operator environment override.
    DisabledByEnv = 2,
    /// Registration failed (e.g. another process owns the combo). The
    /// kill-switch is NOT armed; this is a degraded safety state.
    Unavailable = 3,
}

/// Records the resolved operator hotkey status for later health readback.
pub fn set_operator_hotkey_status(status: OperatorHotkeyStatus) {
    OPERATOR_HOTKEY_STATUS.store(status as u8, Ordering::Release);
}

/// Reads the resolved operator hotkey status.
#[must_use]
pub fn operator_hotkey_status() -> OperatorHotkeyStatus {
    match OPERATOR_HOTKEY_STATUS.load(Ordering::Acquire) {
        1 => OperatorHotkeyStatus::Registered,
        2 => OperatorHotkeyStatus::DisabledByEnv,
        3 => OperatorHotkeyStatus::Unavailable,
        _ => OperatorHotkeyStatus::Unknown,
    }
}

impl OperatorHotkeyStatus {
    /// Stable lowercase label for health/diagnostics output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Registered => "registered",
            Self::DisabledByEnv => "disabled_by_env",
            Self::Unavailable => "unavailable",
        }
    }
}

#[must_use]
pub fn operator_release_epoch() -> u64 {
    interrupt_epochs().release.load(Ordering::Acquire)
}

#[must_use]
pub fn operator_release_requested_since(epoch: u64) -> bool {
    interrupt_epochs().release.load(Ordering::Acquire) != epoch
}

pub fn request_release_interrupt() {
    interrupt_epochs().release.fetch_add(1, Ordering::AcqRel);
}

/// Generation of the physical operator panic control only.
#[must_use]
pub fn operator_panic_epoch() -> u64 {
    interrupt_epochs().operator_panic.load(Ordering::Acquire)
}

/// True only when the operator panic control fired after `epoch` was armed.
#[must_use]
pub fn operator_panic_requested_since(epoch: u64) -> bool {
    interrupt_epochs().operator_panic.load(Ordering::Acquire) != epoch
}

/// True from physical panic publication until every admitted generation has
/// completed K1 preemption and its exact K2 fleet-kill transaction.
#[must_use]
pub fn operator_panic_safety_pending() -> bool {
    operator_panic_safety_readback().pending
}

/// Exact process-global safety accounting readback.
#[must_use]
pub fn operator_panic_safety_readback() -> OperatorPanicSafetyReadback {
    let epochs = interrupt_epochs();
    loop {
        // Publication increments the outstanding owner before advancing the
        // epoch. Bracketing the counter reads with the epoch prevents a torn
        // snapshot of old counters plus a newly published epoch from falsely
        // reporting pending=false. A publisher paused between those two
        // stores is also safe: the outstanding count is already non-zero.
        let publications_before = epochs
            .operator_panic_publications_in_flight
            .load(Ordering::Acquire);
        let epoch_before = epochs.operator_panic.load(Ordering::Acquire);
        let outstanding_generations = epochs.operator_panic_outstanding.load(Ordering::Acquire);
        let outstanding_finalizations = epochs
            .operator_panic_finalizations_outstanding
            .load(Ordering::Acquire);
        let accounting_incident = epochs
            .operator_panic_accounting_incident
            .load(Ordering::Acquire);
        let epoch_after = epochs.operator_panic.load(Ordering::Acquire);
        let publications_after = epochs
            .operator_panic_publications_in_flight
            .load(Ordering::Acquire);
        // A complete publication can otherwise fit between `epoch_after` and
        // `publications_after` (0→1→0), yielding old epoch/counters plus a
        // false pending verdict. The final epoch closes that ABA window.
        let epoch_final = epochs.operator_panic.load(Ordering::Acquire);
        if operator_panic_readback_epochs_stable(epoch_before, epoch_after, epoch_final) {
            let publications_in_flight = publications_before.max(publications_after);
            return OperatorPanicSafetyReadback {
                epoch: epoch_final,
                publications_in_flight,
                outstanding_generations,
                outstanding_finalizations,
                accounting_incident,
                pending: publications_in_flight != 0
                    || outstanding_generations != 0
                    || outstanding_finalizations != 0
                    || accounting_incident,
            };
        }
        std::hint::spin_loop();
    }
}

const fn operator_panic_readback_epochs_stable(
    epoch_before: u64,
    epoch_after: u64,
    epoch_final: u64,
) -> bool {
    epoch_before == epoch_after && epoch_after == epoch_final
}

/// Marks an unrecoverable dispatch, handler, or accounting failure. The flag is
/// sticky so shutdown and all later action admission remain fail-closed.
pub fn record_operator_panic_safety_incident() {
    record_operator_panic_safety_incident_for(interrupt_epochs());
}

fn record_operator_panic_safety_incident_for(epochs: &'static InterruptEpochs) {
    epochs
        .operator_panic_accounting_incident
        .store(true, Ordering::Release);
}

/// Acknowledges this exact physical panic generation through the K1 boundary.
///
/// Callers must invoke this only after atomically reading back its tagged
/// operator lease (or a safely newer published generation that superseded it)
/// and proving the synchronous `ReleaseAll` result.
pub fn acknowledge_operator_panic_preemption(token: &mut OperatorPanicSafetyToken) -> bool {
    let epochs = token.epochs;
    if token.k1_preemption_acknowledged
        || token.generation == 0
        || epochs.operator_panic.load(Ordering::Acquire) < token.generation
    {
        record_operator_panic_safety_incident_for(epochs);
        return false;
    }
    token.k1_preemption_acknowledged = true;
    true
}

const fn operator_panic_finalization_generation_after_cas(
    generation_before_cas: u64,
    generation_after_cas: u64,
    outstanding_after_cas: u64,
) -> u64 {
    if outstanding_after_cas == 0 {
        generation_after_cas
    } else {
        generation_before_cas
    }
}

/// Consumes one exact physical panic owner only after K1 and K2 succeeded.
///
/// The non-clone token prevents stale or duplicate completions from aliasing a
/// later generation. When this closes a wave, the returned unique finalization
/// owner keeps admission closed across the separate physical lease readback.
///
/// # Errors
///
/// Returns an error and records a sticky safety incident when K1 was not
/// acknowledged or the process-global owner count is internally inconsistent.
pub fn complete_operator_panic_safety_generation(
    mut token: OperatorPanicSafetyToken,
) -> Result<OperatorPanicSafetyCompletion, &'static str> {
    if !token.k1_preemption_acknowledged {
        record_operator_panic_safety_incident_for(token.epochs);
        return Err("operator panic K1 preemption was not acknowledged");
    }
    let epochs = token.epochs;
    let outstanding = &epochs.operator_panic_outstanding;
    let mut observed = outstanding.load(Ordering::Acquire);
    loop {
        if observed == 0 {
            record_operator_panic_safety_incident_for(epochs);
            return Err("operator panic generation had no outstanding owner");
        }
        let finalization_generation_before_cas = if observed == 1 {
            let latest_generation = epochs.operator_panic.load(Ordering::Acquire);
            // Publish the finalization owner before removing the last K2 owner,
            // so admission can never observe an open gap between the phases.
            epochs
                .operator_panic_finalizations_outstanding
                .fetch_add(1, Ordering::AcqRel);
            latest_generation
        } else {
            0
        };
        match outstanding.compare_exchange_weak(
            observed,
            observed - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                token.accounting_consumed = true;
                return if observed == 1 {
                    // The outstanding count can undergo an ABA (1→2→1)
                    // between our load and CAS. If it is still zero after the
                    // successful CAS, the latest published epoch belongs to
                    // the wave we just closed. If a newer request is live,
                    // retain the pre-CAS generation and let its own owner keep
                    // admission closed. A newer wave that both starts and
                    // finishes here creates its own finalizer, so targeting
                    // the same latest generation is harmless and exact-clear
                    // remains idempotent.
                    let finalization_generation_after_cas =
                        epochs.operator_panic.load(Ordering::Acquire);
                    let outstanding_after_cas = outstanding.load(Ordering::Acquire);
                    let finalization_generation = operator_panic_finalization_generation_after_cas(
                        finalization_generation_before_cas,
                        finalization_generation_after_cas,
                        outstanding_after_cas,
                    );
                    Ok(OperatorPanicSafetyCompletion::Finalize(
                        OperatorPanicSafetyFinalization {
                            epochs,
                            generation: finalization_generation,
                        },
                    ))
                } else {
                    Ok(OperatorPanicSafetyCompletion::Pending)
                };
            }
            Err(current) => {
                if observed == 1 {
                    epochs
                        .operator_panic_finalizations_outstanding
                        .fetch_sub(1, Ordering::AcqRel);
                }
                observed = current;
            }
        }
    }
}

/// Completes the final K2/lease-readback handshake.
///
/// A failed or stale postcondition is a sticky accounting incident. Consuming
/// a unique owner remains exact across concurrently finalized hotkey waves.
#[must_use]
#[allow(clippy::needless_pass_by_value)] // ownership is the one-shot proof
pub fn finish_operator_panic_safety_finalization(
    finalization: OperatorPanicSafetyFinalization,
    lease_postcondition_ok: bool,
) -> bool {
    let epochs = finalization.epochs;
    if finalization.generation == 0 || !lease_postcondition_ok {
        record_operator_panic_safety_incident_for(epochs);
        return false;
    }
    let decremented = epochs
        .operator_panic_finalizations_outstanding
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count != 0).then_some(count - 1)
        })
        .is_ok();
    if !decremented {
        record_operator_panic_safety_incident_for(epochs);
        return false;
    }
    !epochs
        .operator_panic_accounting_incident
        .load(Ordering::Acquire)
}

/// Records a physical operator panic event and also wakes generic
/// interrupt-aware software input. Ordinary release paths must call
/// [`request_release_interrupt`] instead.
#[must_use]
pub fn request_operator_panic_interrupt() -> OperatorPanicSafetyToken {
    let epochs = interrupt_epochs();
    epochs
        .operator_panic_publications_in_flight
        .fetch_add(1, Ordering::AcqRel);
    epochs
        .operator_panic_outstanding
        .fetch_add(1, Ordering::AcqRel);
    let generation = epochs.operator_panic.fetch_add(1, Ordering::AcqRel) + 1;
    epochs.release.fetch_add(1, Ordering::AcqRel);
    epochs
        .operator_panic_publications_in_flight
        .fetch_sub(1, Ordering::AcqRel);
    if generation == 0 {
        record_operator_panic_safety_incident_for(epochs);
    }
    OperatorPanicSafetyToken {
        epochs,
        generation,
        k1_preemption_acknowledged: false,
        accounting_consumed: false,
    }
}

#[cfg(windows)]
mod platform {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{
            Arc, Mutex, OnceLock, TryLockError,
            atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
            mpsc,
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use synapse_core::error_codes;
    use windows::Win32::{
        Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            SystemInformation::GetTickCount64,
            Threading::{
                GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
            },
        },
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey,
                UnregisterHotKey,
            },
            WindowsAndMessaging::{
                CallNextHookEx, DispatchMessageW, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE,
                PM_REMOVE, PeekMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
                WH_KEYBOARD_LL, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
                WM_SYSKEYUP,
            },
        },
    };

    use crate::{ActionError, ActionResult};

    const HOTKEY_ID: i32 = 0x5359;
    const HOTKEY_WPARAM: usize = 0x5359;
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
    const INSTALL_UNWIND_TIMEOUT: Duration = Duration::from_secs(2);
    const THREAD_TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(10);
    const KEY_DOWN_MASK: i16 = i16::MIN;
    const HOTKEY_SIGNAL_DEBOUNCE_MS: u64 = 750;
    const HOTKEY_HOOK_REARM_INTERVAL_MS: u64 = 500;
    const HOTKEY_MESSAGE_POLL_MS: u64 = 25;
    const UNSOLICITED_WM_QUIT_FAILURE: &str =
        "operator hotkey hook received WM_QUIT without an exact guard-owned stop request";
    const VK_CONTROL_CODE: i32 = 0x11;
    const VK_MENU_CODE: i32 = 0x12;
    const VK_SHIFT_CODE: i32 = 0x10;

    static HOTKEY_INSTALL_STATE: OnceLock<Mutex<HotkeyInstallState>> = OnceLock::new();
    static NEXT_HOTKEY_OWNER_ID: AtomicU64 = AtomicU64::new(1);
    static HOTKEY_KEY_VK: AtomicU32 = AtomicU32::new(0);
    static CHORD_DOWN: AtomicBool = AtomicBool::new(false);
    static LAST_SIGNAL_TICK_MS: AtomicU64 = AtomicU64::new(0);
    static INSTALL_UNWIND_RETAINED_LIVE_OWNER: AtomicBool = AtomicBool::new(false);
    static RETAINED_OPERATOR_HOTKEY_OWNERS: AtomicPtr<RetainedOperatorHotkeyOwners> =
        AtomicPtr::new(std::ptr::null_mut());
    static RETAINED_OPERATOR_HOTKEY_OWNER_COUNT: AtomicU64 = AtomicU64::new(0);
    static RETAINED_OR_UNRESOLVED_OPERATOR_HOTKEY_OWNER: AtomicBool = AtomicBool::new(false);
    static LAST_INSTALL_UNWIND_REPORT: OnceLock<
        Mutex<Option<super::OperatorHotkeyShutdownReport>>,
    > = OnceLock::new();

    pub struct OperatorHotkeyGuard {
        owner_id: u64,
        hook_thread_id: Arc<AtomicU32>,
        stop_requested: Arc<AtomicBool>,
        hook_terminal_rx: mpsc::Receiver<HookThreadTerminal>,
        hook_terminal_report: Option<HookThreadTerminal>,
        hook_join: Option<JoinHandle<()>>,
        worker_join: Option<JoinHandle<()>>,
        shutdown_started: bool,
        shutdown_report: Option<super::OperatorHotkeyShutdownReport>,
    }

    struct HotkeyInstallState {
        owner_id: Option<u64>,
        last_released_owner_id: Option<u64>,
        sender: Option<mpsc::Sender<HotkeySignal>>,
    }

    impl HotkeyInstallState {
        const fn empty() -> Self {
            Self {
                owner_id: None,
                last_released_owner_id: None,
                sender: None,
            }
        }
    }

    /// Append-only process-teardown registry for exact thread owners that a
    /// bounded shutdown could not join. Nodes are intentionally never reclaimed:
    /// retaining their `JoinHandle`s is the safety property. The atomic stack
    /// keeps `Drop` nonblocking even if another guard is being retained at the
    /// same time.
    struct RetainedOperatorHotkeyOwners {
        next: *mut Self,
        _reason: &'static str,
        _owner_id: u64,
        _hook_thread_id: Arc<AtomicU32>,
        _stop_requested: Arc<AtomicBool>,
        _hook_terminal_rx: mpsc::Receiver<HookThreadTerminal>,
        _hook_terminal_report: Option<HookThreadTerminal>,
        _hook_join: Option<JoinHandle<()>>,
        _worker_join: Option<JoinHandle<()>>,
    }

    #[derive(Clone, Debug)]
    struct HookThreadTerminal {
        low_level_hook_was_installed: Option<bool>,
        low_level_hook_unregistered: Option<bool>,
        register_hotkey_backup_was_registered: Option<bool>,
        register_hotkey_backup_unregistered: Option<bool>,
        kernel_owners_released: bool,
        failures: Vec<String>,
    }

    impl HookThreadTerminal {
        const fn before_hook_install() -> Self {
            Self {
                low_level_hook_was_installed: Some(false),
                low_level_hook_unregistered: None,
                register_hotkey_backup_was_registered: Some(false),
                register_hotkey_backup_unregistered: None,
                kernel_owners_released: true,
                failures: Vec::new(),
            }
        }

        fn panic() -> Self {
            let mut terminal = Self {
                low_level_hook_was_installed: None,
                low_level_hook_unregistered: None,
                register_hotkey_backup_was_registered: None,
                register_hotkey_backup_unregistered: None,
                kernel_owners_released: false,
                failures: Vec::new(),
            };
            terminal.failures.push(
                "operator hotkey hook thread panicked; unregister state is unobservable".to_owned(),
            );
            terminal
        }
    }

    #[derive(Clone, Debug, Default)]
    struct HookWakeObservation {
        owner: HookWakeOwnerObservation,
        message: HookWakeMessageObservation,
        failure: Option<String>,
    }

    #[derive(Clone, Debug, Default)]
    struct HookWakeOwnerObservation {
        live_before_wake: bool,
        live_after_wake: bool,
    }

    #[derive(Clone, Debug, Default)]
    struct HookWakeMessageObservation {
        attempted: bool,
        sent: bool,
    }

    pub fn install_operator_hotkey<F>(handler: F) -> ActionResult<OperatorHotkeyGuard>
    where
        F: Fn(super::OperatorPanicSafetyToken) + Send + 'static,
    {
        let config = HotkeyConfig::from_env()?;
        install_operator_hotkey_with_config(config, handler)
    }

    fn install_operator_hotkey_with_config<F>(
        config: HotkeyConfig,
        handler: F,
    ) -> ActionResult<OperatorHotkeyGuard>
    where
        F: Fn(super::OperatorPanicSafetyToken) + Send + 'static,
    {
        reject_retained_install_unwind()?;
        store_install_unwind_report(None);
        let owner_id = allocate_hotkey_owner_id()?;
        let (signal_tx, signal_rx) = mpsc::channel::<HotkeySignal>();
        reserve_install_state(owner_id, signal_tx.clone())?;
        reset_hotkey_signal_state(config.key_vk);

        let worker_join = spawn_hotkey_worker(owner_id, signal_rx, handler)?;
        let stop_requested = Arc::new(AtomicBool::new(false));
        let hook_thread_id = Arc::new(AtomicU32::new(0));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<HookReady, String>>();
        let (hook_terminal_tx, hook_terminal_rx) = mpsc::channel::<HookThreadTerminal>();
        let hook_join = match spawn_hotkey_hook_thread(
            config,
            &stop_requested,
            &hook_thread_id,
            ready_tx,
            hook_terminal_tx,
        ) {
            Ok(join) => join,
            Err(error) => {
                drop(signal_tx);
                let guard = operator_hotkey_guard(
                    owner_id,
                    hook_thread_id,
                    stop_requested,
                    hook_terminal_rx,
                    Some(HookThreadTerminal::before_hook_install()),
                    None,
                    Some(worker_join),
                );
                return Err(install_unwind_error(
                    &format!("operator hotkey thread spawn failed: {error}"),
                    guard,
                ));
            }
        };

        let guard = operator_hotkey_guard(
            owner_id,
            hook_thread_id,
            stop_requested,
            hook_terminal_rx,
            None,
            Some(hook_join),
            Some(worker_join),
        );
        finish_hotkey_install_ready(&ready_rx, signal_tx, guard)
    }

    fn reject_retained_install_unwind() -> ActionResult<()> {
        if install_unwind_retained_live_owner() {
            return Err(ActionError::BackendUnavailable {
                detail: "a prior operator-hotkey installation unwind retained an exact live thread owner or reported unresolved kernel ownership until process teardown"
                    .to_owned(),
            });
        }
        Ok(())
    }

    fn allocate_hotkey_owner_id() -> ActionResult<u64> {
        let owner_id = NEXT_HOTKEY_OWNER_ID.fetch_add(1, Ordering::Relaxed);
        if owner_id == 0 {
            return Err(ActionError::BackendUnavailable {
                detail: "operator hotkey owner identity space exhausted".to_owned(),
            });
        }
        Ok(owner_id)
    }

    fn reset_hotkey_signal_state(key_vk: u32) {
        HOTKEY_KEY_VK.store(key_vk, Ordering::Release);
        CHORD_DOWN.store(false, Ordering::Release);
        LAST_SIGNAL_TICK_MS.store(0, Ordering::Release);
    }

    fn spawn_hotkey_worker<F>(
        owner_id: u64,
        signal_rx: mpsc::Receiver<HotkeySignal>,
        handler: F,
    ) -> ActionResult<JoinHandle<()>>
    where
        F: Fn(super::OperatorPanicSafetyToken) + Send + 'static,
    {
        match thread::Builder::new()
            .name("synapse-operator-hotkey-worker".to_owned())
            .spawn(move || run_hotkey_worker(signal_rx, handler))
        {
            Ok(join) => Ok(join),
            Err(error) => {
                let release = disconnect_and_release_unstarted_owner(owner_id);
                if release.is_err() {
                    INSTALL_UNWIND_RETAINED_LIVE_OWNER.store(true, Ordering::Release);
                }
                Err(ActionError::BackendUnavailable {
                    detail: format!(
                        "operator hotkey worker thread spawn failed: {error}; installation_slot_cleanup={release:?}"
                    ),
                })
            }
        }
    }

    fn spawn_hotkey_hook_thread(
        config: HotkeyConfig,
        stop_requested: &Arc<AtomicBool>,
        hook_thread_id: &Arc<AtomicU32>,
        ready_tx: mpsc::Sender<Result<HookReady, String>>,
        hook_terminal_tx: mpsc::Sender<HookThreadTerminal>,
    ) -> std::io::Result<JoinHandle<()>> {
        let thread_stop_requested = Arc::clone(stop_requested);
        let published_thread_id = Arc::clone(hook_thread_id);
        thread::Builder::new()
            .name("synapse-operator-hotkey".to_owned())
            .spawn(move || {
                let terminal = catch_unwind(AssertUnwindSafe(|| {
                    run_hotkey_thread(
                        &config,
                        &ready_tx,
                        &thread_stop_requested,
                        &published_thread_id,
                    )
                }))
                .unwrap_or_else(|_panic| HookThreadTerminal::panic());
                published_thread_id.store(0, Ordering::Release);
                if hook_terminal_tx.send(terminal).is_err() {
                    tracing::error!(
                        component = "operator_hotkey",
                        "operator hotkey hook terminal report receiver disappeared"
                    );
                }
            })
    }

    const fn operator_hotkey_guard(
        owner_id: u64,
        hook_thread_id: Arc<AtomicU32>,
        stop_requested: Arc<AtomicBool>,
        hook_terminal_rx: mpsc::Receiver<HookThreadTerminal>,
        hook_terminal_report: Option<HookThreadTerminal>,
        hook_join: Option<JoinHandle<()>>,
        worker_join: Option<JoinHandle<()>>,
    ) -> OperatorHotkeyGuard {
        OperatorHotkeyGuard {
            owner_id,
            hook_thread_id,
            stop_requested,
            hook_terminal_rx,
            hook_terminal_report,
            hook_join,
            worker_join,
            shutdown_started: false,
            shutdown_report: None,
        }
    }

    fn finish_hotkey_install_ready(
        ready_rx: &mpsc::Receiver<Result<HookReady, String>>,
        signal_tx: mpsc::Sender<HotkeySignal>,
        guard: OperatorHotkeyGuard,
    ) -> ActionResult<OperatorHotkeyGuard> {
        match ready_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(ready)) => {
                guard
                    .hook_thread_id
                    .store(ready.thread_id, Ordering::Release);
                Ok(guard)
            }
            Ok(Err(detail)) => {
                drop(signal_tx);
                Err(install_unwind_error(&detail, guard))
            }
            Err(error) => {
                drop(signal_tx);
                let detail = format!("operator hotkey registration readiness failed: {error}");
                Err(install_unwind_error(&detail, guard))
            }
        }
    }

    fn install_unwind_error(primary: &str, mut guard: OperatorHotkeyGuard) -> ActionError {
        let cleanup = guard.shutdown_checked(INSTALL_UNWIND_TIMEOUT, "installation_unwind");
        store_install_unwind_report(Some(cleanup.clone()));
        if !cleanup.owners_quiescent() {
            INSTALL_UNWIND_RETAINED_LIVE_OWNER.store(true, Ordering::Release);
            retain_remaining_thread_owners(
                &mut guard,
                "installation_unwind",
                "ACTION_OPERATOR_HOTKEY_INSTALL_UNWIND_OWNER_RETAINED",
            );
        }
        ActionError::BackendUnavailable {
            detail: format!(
                "{primary}; bounded installation unwind report={cleanup:?}; cleanup_failures={:?}",
                cleanup.failures
            ),
        }
    }

    pub fn install_unwind_report() -> Option<super::OperatorHotkeyShutdownReport> {
        let slot = LAST_INSTALL_UNWIND_REPORT.get_or_init(|| Mutex::new(None));
        match slot.lock() {
            Ok(report) => report.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn install_unwind_retained_live_owner() -> bool {
        INSTALL_UNWIND_RETAINED_LIVE_OWNER.load(Ordering::Acquire)
            || RETAINED_OR_UNRESOLVED_OPERATOR_HOTKEY_OWNER.load(Ordering::Acquire)
            || RETAINED_OPERATOR_HOTKEY_OWNER_COUNT.load(Ordering::Acquire) != 0
    }

    fn store_install_unwind_report(report: Option<super::OperatorHotkeyShutdownReport>) {
        let slot = LAST_INSTALL_UNWIND_REPORT.get_or_init(|| Mutex::new(None));
        match slot.lock() {
            Ok(mut current) => *current = report,
            Err(poisoned) => *poisoned.into_inner() = report,
        }
    }

    fn retain_remaining_thread_owners(
        guard: &mut OperatorHotkeyGuard,
        reason: &'static str,
        error_code: &'static str,
    ) {
        RETAINED_OR_UNRESOLVED_OPERATOR_HOTKEY_OWNER.store(true, Ordering::Release);
        let hook_join = guard.hook_join.take();
        let worker_join = guard.worker_join.take();
        if hook_join.is_none() && worker_join.is_none() {
            tracing::error!(
                code = error_code,
                component = "operator_hotkey",
                reason,
                "operator hotkey ownership is unsafe, but no exact thread handle remained to retain; consult the shutdown report for unresolved kernel ownership"
            );
            return;
        }

        let hook_thread_finished = hook_join.as_ref().is_none_or(JoinHandle::is_finished);
        let worker_thread_finished = worker_join.as_ref().is_none_or(JoinHandle::is_finished);
        let hook_thread_id = guard.hook_thread_id.load(Ordering::Acquire);
        let (_terminal_placeholder_tx, terminal_placeholder_rx) = mpsc::channel();
        let hook_terminal_rx =
            std::mem::replace(&mut guard.hook_terminal_rx, terminal_placeholder_rx);
        let node = Box::new(RetainedOperatorHotkeyOwners {
            next: std::ptr::null_mut(),
            _reason: reason,
            _owner_id: guard.owner_id,
            _hook_thread_id: Arc::clone(&guard.hook_thread_id),
            _stop_requested: Arc::clone(&guard.stop_requested),
            _hook_terminal_rx: hook_terminal_rx,
            _hook_terminal_report: guard.hook_terminal_report.take(),
            _hook_join: hook_join,
            _worker_join: worker_join,
        });
        let node = Box::into_raw(node);
        let mut head = RETAINED_OPERATOR_HOTKEY_OWNERS.load(Ordering::Acquire);
        loop {
            // SAFETY: `node` is exclusively owned by this function until the
            // successful CAS publishes it. Published nodes are never mutated
            // or reclaimed for the remainder of the process.
            unsafe { (*node).next = head };
            match RETAINED_OPERATOR_HOTKEY_OWNERS.compare_exchange_weak(
                head,
                node,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => head = actual,
            }
        }
        let retained_owner_count = RETAINED_OPERATOR_HOTKEY_OWNER_COUNT
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        tracing::error!(
            code = error_code,
            component = "operator_hotkey",
            reason,
            hook_thread_id,
            hook_thread_finished,
            worker_thread_finished,
            retained_owner_count,
            "operator hotkey guard retained exact still-owned thread handles in the process-teardown registry"
        );
    }

    #[derive(Clone, Debug)]
    struct HotkeyConfig {
        key_vk: u32,
        label: String,
    }

    impl HotkeyConfig {
        fn default() -> Self {
            Self {
                key_vk: u32::from(b'P'),
                label: super::DEFAULT_OPERATOR_HOTKEY.to_owned(),
            }
        }

        fn from_env() -> ActionResult<Self> {
            let raw = std::env::var_os(super::OPERATOR_HOTKEY_ENV)
                .or_else(|| std::env::var_os(super::OPERATOR_HOTKEY_COMPAT_ENV));
            let Some(raw) = raw else {
                return Ok(Self::default());
            };
            let value = raw.to_string_lossy();
            Self::parse(&value)
        }

        fn parse(raw: &str) -> ActionResult<Self> {
            let mut ctrl = false;
            let mut alt = false;
            let mut shift = false;
            let mut key_vk = None;

            for token in raw
                .split('+')
                .map(str::trim)
                .filter(|token| !token.is_empty())
            {
                match token.to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => ctrl = true,
                    "alt" => alt = true,
                    "shift" => shift = true,
                    other => {
                        let upper = other.to_ascii_uppercase();
                        let bytes = upper.as_bytes();
                        if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
                            if key_vk.replace(u32::from(bytes[0])).is_some() {
                                return Err(invalid_hotkey(raw, "multiple non-modifier keys"));
                            }
                        } else {
                            return Err(invalid_hotkey(raw, "unsupported token"));
                        }
                    }
                }
            }

            if !ctrl || !alt || !shift {
                return Err(invalid_hotkey(raw, "must include Ctrl+Alt+Shift"));
            }
            let Some(key_vk) = key_vk else {
                return Err(invalid_hotkey(raw, "missing non-modifier key"));
            };
            let key = char::from_u32(key_vk).unwrap_or('P').to_ascii_lowercase();
            Ok(Self {
                key_vk,
                label: format!("ctrl+alt+shift+{key}"),
            })
        }
    }

    #[derive(Clone, Debug)]
    struct HookReady {
        thread_id: u32,
    }

    #[derive(Debug)]
    struct HotkeySignal {
        source: &'static str,
        operator_panic_token: super::OperatorPanicSafetyToken,
    }

    struct HookGuard(Option<HHOOK>);

    impl HookGuard {
        fn close(&mut self) -> Result<(), String> {
            let Some(hook) = self.0 else {
                return Ok(());
            };
            match unsafe { UnhookWindowsHookEx(hook) } {
                Ok(()) => {
                    self.0 = None;
                    Ok(())
                }
                Err(error) => Err(format!(
                    "operator low-level keyboard hook unregister failed; kernel ownership remains unresolved: {error}"
                )),
            }
        }
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            if self.0.is_none() {
                return;
            }
            if let Err(error) = self.close() {
                tracing::error!(
                    code = "ACTION_OPERATOR_HOTKEY_HOOK_OWNER_UNRESOLVED",
                    component = "operator_hotkey",
                    detail = %error,
                    "operator low-level keyboard hook unregister retry failed; unresolved kernel ownership is being surfaced"
                );
            }
        }
    }

    struct RegisteredHotkeyGuard(bool);

    impl RegisteredHotkeyGuard {
        fn close(&mut self) -> Result<(), String> {
            if !self.0 {
                return Ok(());
            }
            match unsafe { UnregisterHotKey(None, HOTKEY_ID) } {
                Ok(()) => {
                    self.0 = false;
                    Ok(())
                }
                Err(error) => Err(format!(
                    "operator RegisterHotKey backup unregister failed; kernel ownership remains unresolved: {error}"
                )),
            }
        }
    }

    impl Drop for RegisteredHotkeyGuard {
        fn drop(&mut self) {
            if !self.0 {
                return;
            }
            if let Err(error) = self.close() {
                tracing::error!(
                    code = "ACTION_OPERATOR_HOTKEY_REGISTERED_OWNER_UNRESOLVED",
                    component = "operator_hotkey",
                    detail = %error,
                    "operator RegisterHotKey backup unregister retry failed; unresolved kernel ownership is being surfaced"
                );
            }
        }
    }

    fn run_hotkey_worker<F>(receiver: mpsc::Receiver<HotkeySignal>, handler: F)
    where
        F: Fn(super::OperatorPanicSafetyToken) + Send + 'static,
    {
        let priority_high = set_current_thread_high_priority("worker");
        tracing::info!(
            component = "operator_hotkey",
            worker_thread_priority_high = priority_high,
            "operator hotkey worker thread started"
        );
        for signal in receiver {
            let generation = signal.operator_panic_token.generation();
            let result = catch_unwind(AssertUnwindSafe(|| {
                handler(signal.operator_panic_token);
            }));
            if result.is_err() {
                emergency_operator_panic_k1(generation, signal.source, "handler_panic");
                tracing::error!(
                    code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                    component = "operator_hotkey",
                    source = signal.source,
                    operator_panic_generation = generation,
                    result = "handler_panic",
                    "operator hotkey handler panicked"
                );
            }
        }
    }

    /// Last-resort physical K1 for a published panic generation whose normal
    /// worker dispatch or handler failed. The release is deliberately the
    /// first potentially blocking operation: even a poisoned/contended
    /// dispatch lock must not leave software-held keys down. The sticky
    /// accounting incident remains set, so this fallback never pretends that
    /// the missing K2 transaction completed successfully.
    fn emergency_operator_panic_k1(generation: u64, source: &'static str, failure: &'static str) {
        const RELEASE_TIMEOUT: Duration = Duration::from_millis(50);

        let release_started = Instant::now();
        let release_result = crate::RELEASE_ALL_HANDLE.get().map_or_else(
            || {
                Err(crate::ActionError::BackendUnavailable {
                    detail: "global ReleaseAll handle was not installed".to_owned(),
                })
            },
            |handle| handle.fire_release_all_blocking_with_timeout(RELEASE_TIMEOUT),
        );
        let release_elapsed = release_started.elapsed();

        let prior =
            crate::lease::force_preempt_operator_panic("operator_hotkey_emergency_k1", generation);
        let lease_snapshot_after = crate::lease::safety_snapshot();
        let lease_after = lease_snapshot_after.status;
        let tagged_generation_after = lease_snapshot_after.operator_panic_generation;
        let exact_tag_installed = lease_after.owner_session_id.as_deref()
            == Some(crate::lease::OPERATOR_LEASE_OWNER_SESSION_ID)
            && tagged_generation_after == Some(generation);

        super::record_operator_panic_safety_incident();
        tracing::error!(
            code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
            component = "operator_hotkey",
            source,
            failure,
            operator_panic_generation = generation,
            release_all_result = if release_result.is_ok() { "ok" } else { "error" },
            release_all_detail = ?release_result.as_ref().err(),
            release_all_elapsed_ms = release_elapsed.as_millis(),
            release_all_within_budget = release_elapsed <= RELEASE_TIMEOUT,
            prior_lease = ?prior,
            lease_after = ?lease_after,
            tagged_generation_after,
            exact_tag_installed,
            safety_pending = super::operator_panic_safety_pending(),
            "operator hotkey emergency K1 ran after normal dispatch failed; safety accounting remains fail-closed"
        );
    }

    fn run_hotkey_thread(
        config: &HotkeyConfig,
        ready: &mpsc::Sender<Result<HookReady, String>>,
        stop_requested: &AtomicBool,
        published_thread_id: &AtomicU32,
    ) -> HookThreadTerminal {
        let startup =
            match prepare_hotkey_thread(config, ready, stop_requested, published_thread_id) {
                Ok(startup) => startup,
                Err(terminal) => return terminal,
            };
        run_hotkey_message_loop(config, stop_requested, startup)
    }

    struct HotkeyThreadStartup {
        module: windows::Win32::Foundation::HMODULE,
        hook_guard: HookGuard,
        registered_hotkey_guard: Option<RegisteredHotkeyGuard>,
        priority_high: bool,
    }

    fn prepare_hotkey_thread(
        config: &HotkeyConfig,
        ready: &mpsc::Sender<Result<HookReady, String>>,
        stop_requested: &AtomicBool,
        published_thread_id: &AtomicU32,
    ) -> Result<HotkeyThreadStartup, HookThreadTerminal> {
        let thread_id = unsafe { GetCurrentThreadId() };
        published_thread_id.store(thread_id, Ordering::Release);
        let priority_high = set_current_thread_high_priority("hook");
        let mut msg = MSG::default();
        unsafe {
            let _queue_created = PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE);
        }

        if stop_requested.load(Ordering::Acquire) {
            let detail =
                "operator hotkey hook startup was cancelled before registration".to_owned();
            let _send_result = ready.send(Err(detail.clone()));
            let mut terminal = HookThreadTerminal::before_hook_install();
            terminal.failures.push(detail);
            return Err(terminal);
        }

        let module = match unsafe { GetModuleHandleW(None) } {
            Ok(module) => module,
            Err(error) => {
                let detail = format!("GetModuleHandleW failed for operator hotkey hook: {error}");
                let _send_result = ready.send(Err(detail.clone()));
                let mut terminal = HookThreadTerminal::before_hook_install();
                terminal.failures.push(detail);
                return Err(terminal);
            }
        };
        let hook_guard = match install_keyboard_hook(module, config) {
            Ok(hook_guard) => hook_guard,
            Err(error) => {
                let _send_result = ready.send(Err(error.clone()));
                let mut terminal = HookThreadTerminal::before_hook_install();
                terminal.failures.push(error);
                return Err(terminal);
            }
        };

        let registered_hotkey_guard = match unsafe {
            RegisterHotKey(
                None,
                HOTKEY_ID,
                MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT,
                config.key_vk,
            )
        } {
            Ok(()) => Some(RegisteredHotkeyGuard(true)),
            Err(error) => {
                tracing::warn!(
                    component = "operator_hotkey",
                    hotkey = config.label.as_str(),
                    detail = %error,
                    "operator RegisterHotKey backup unavailable; WH_KEYBOARD_LL hook remains armed"
                );
                None
            }
        };

        let mut failures = Vec::new();
        let kernel_owners_released = true;
        if ready.send(Ok(HookReady { thread_id })).is_err() {
            failures.push(
                "operator hotkey readiness receiver disappeared after kernel registration"
                    .to_owned(),
            );
            return Err(finish_hook_thread(
                hook_guard,
                registered_hotkey_guard,
                kernel_owners_released,
                failures,
            ));
        }
        tracing::info!(
            component = "operator_hotkey",
            hotkey = config.label.as_str(),
            low_level_hook_installed = true,
            rearm_interval_ms = HOTKEY_HOOK_REARM_INTERVAL_MS,
            register_hotkey_backup = registered_hotkey_guard.is_some(),
            hook_thread_priority_high = priority_high,
            "operator panic hotkey armed"
        );
        Ok(HotkeyThreadStartup {
            module,
            hook_guard,
            registered_hotkey_guard,
            priority_high,
        })
    }

    fn run_hotkey_message_loop(
        config: &HotkeyConfig,
        stop_requested: &AtomicBool,
        startup: HotkeyThreadStartup,
    ) -> HookThreadTerminal {
        let HotkeyThreadStartup {
            module,
            mut hook_guard,
            registered_hotkey_guard,
            priority_high: _priority_high,
        } = startup;
        let mut msg = MSG::default();
        let mut failures = Vec::new();
        let mut kernel_owners_released = true;
        let mut last_rearm_ms = unsafe { GetTickCount64() };
        'run: loop {
            if stop_requested.load(Ordering::Acquire) {
                break;
            }
            while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                if msg.message == WM_QUIT {
                    if let Some(failure) = wm_quit_failure(stop_requested.load(Ordering::Acquire)) {
                        tracing::error!(
                            code = "ACTION_OPERATOR_HOTKEY_UNSOLICITED_QUIT",
                            component = "operator_hotkey",
                            detail = failure,
                            "operator hotkey hook loop terminated without a guard-owned stop request"
                        );
                        failures.push(failure.to_owned());
                    }
                    break 'run;
                }

                if msg.message == WM_HOTKEY && msg.wParam.0 == HOTKEY_WPARAM {
                    maybe_send_hotkey_signal("register_hotkey_backup");
                    continue;
                }

                unsafe {
                    let _translated = TranslateMessage(&raw const msg);
                    let _dispatch_result = DispatchMessageW(&raw const msg);
                }
            }

            let now_ms = unsafe { GetTickCount64() };
            if now_ms.saturating_sub(last_rearm_ms) >= HOTKEY_HOOK_REARM_INTERVAL_MS {
                match install_keyboard_hook(module, config) {
                    Ok(new_hook_guard) => {
                        let mut old_hook_guard = std::mem::replace(&mut hook_guard, new_hook_guard);
                        if let Err(error) = old_hook_guard.close() {
                            kernel_owners_released = false;
                            tracing::error!(
                                component = "operator_hotkey",
                                detail = %error,
                                "operator superseded low-level keyboard hook unregister failed"
                            );
                            failures.push(error);
                        }
                        last_rearm_ms = now_ms;
                        tracing::debug!(
                            component = "operator_hotkey",
                            hotkey = config.label.as_str(),
                            "operator low-level keyboard hook re-armed at hook-chain head"
                        );
                    }
                    Err(error) => {
                        last_rearm_ms = now_ms;
                        tracing::error!(
                            component = "operator_hotkey",
                            hotkey = config.label.as_str(),
                            detail = error,
                            "operator low-level keyboard hook re-arm failed"
                        );
                    }
                }
            }

            thread::sleep(Duration::from_millis(HOTKEY_MESSAGE_POLL_MS));
        }
        finish_hook_thread(
            hook_guard,
            registered_hotkey_guard,
            kernel_owners_released,
            failures,
        )
    }

    const fn wm_quit_failure(stop_requested: bool) -> Option<&'static str> {
        if stop_requested {
            None
        } else {
            Some(UNSOLICITED_WM_QUIT_FAILURE)
        }
    }

    fn finish_hook_thread(
        mut hook_guard: HookGuard,
        mut registered_hotkey_guard: Option<RegisteredHotkeyGuard>,
        mut kernel_owners_released: bool,
        mut failures: Vec<String>,
    ) -> HookThreadTerminal {
        let register_hotkey_backup_was_registered = registered_hotkey_guard.is_some();
        let register_hotkey_backup_unregistered = registered_hotkey_guard.as_mut().map(|guard| {
            if let Err(error) = guard.close() {
                failures.push(error);
                kernel_owners_released = false;
                false
            } else {
                true
            }
        });
        let low_level_hook_unregistered = if let Err(error) = hook_guard.close() {
            failures.push(error);
            kernel_owners_released = false;
            false
        } else {
            true
        };
        HookThreadTerminal {
            low_level_hook_was_installed: Some(true),
            low_level_hook_unregistered: Some(low_level_hook_unregistered),
            register_hotkey_backup_was_registered: Some(register_hotkey_backup_was_registered),
            register_hotkey_backup_unregistered,
            kernel_owners_released,
            failures,
        }
    }

    fn install_keyboard_hook(
        module: windows::Win32::Foundation::HMODULE,
        config: &HotkeyConfig,
    ) -> Result<HookGuard, String> {
        match unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook_proc),
                Some(HINSTANCE(module.0)),
                0,
            )
        } {
            Ok(hook) => Ok(HookGuard(Some(hook))),
            Err(error) => Err(format!(
                "SetWindowsHookExW(WH_KEYBOARD_LL) failed for {}: {error}",
                config.label
            )),
        }
    }

    impl Drop for OperatorHotkeyGuard {
        fn drop(&mut self) {
            if self.hook_join.is_none() && self.worker_join.is_none() {
                return;
            }

            if !self.shutdown_started {
                // This hook loop polls the stop atomic every 25 ms. Never post
                // to its numeric TID: an `is_finished` observation cannot pin
                // an OS thread between the observation and PostThreadMessageW.
                let wake = self.observe_hook_owner_for_atomic_stop();
                self.stop_requested.store(true, Ordering::Release);
                if let Err(error) = disconnect_install_sender_checked(self.owner_id) {
                    tracing::error!(
                        code = "ACTION_OPERATOR_HOTKEY_STATE_CLEAR_FAILED",
                        component = "operator_hotkey",
                        owner_id = self.owner_id,
                        detail = %error,
                        "unchecked hotkey Drop could not disconnect the exact worker sender"
                    );
                }
                super::set_operator_hotkey_status(super::OperatorHotkeyStatus::Unknown);
                tracing::error!(
                    code = "ACTION_OPERATOR_HOTKEY_UNCHECKED_DROP",
                    component = "operator_hotkey",
                    hook_thread_id = self.hook_thread_id.load(Ordering::Acquire),
                    hook_owner_observed_live_before_wake = wake.owner.live_before_wake,
                    hook_owner_observed_live_after_wake = wake.owner.live_after_wake,
                    wake_message_attempted = wake.message.attempted,
                    wake_message_sent = wake.message.sent,
                    wake_failure = ?wake.failure,
                    "operator hotkey guard dropped without checked shutdown; exact owners will be retained after the non-TID atomic stop request"
                );
                retain_remaining_thread_owners(
                    self,
                    "unchecked_drop",
                    "ACTION_OPERATOR_HOTKEY_UNCHECKED_DROP_OWNER_RETAINED",
                );
                return;
            }

            // A handle can become terminal (or reveal a panic) after the last
            // bounded observation. Retain every still-owned handle regardless
            // of its current `is_finished` readback; dropping it would silently
            // detach the exact owner and destroy the ability to join it later.
            retain_remaining_thread_owners(
                self,
                "checked_shutdown_guard_drop",
                "ACTION_OPERATOR_HOTKEY_CHECKED_DROP_OWNER_RETAINED",
            );
        }
    }

    impl OperatorHotkeyGuard {
        fn observe_hook_owner_for_atomic_stop(&self) -> HookWakeObservation {
            let Some(hook_owner) = self.hook_join.as_ref() else {
                return HookWakeObservation::default();
            };
            let owner_live_before_wake = !hook_owner.is_finished();
            HookWakeObservation {
                owner: HookWakeOwnerObservation {
                    live_before_wake: owner_live_before_wake,
                    live_after_wake: !hook_owner.is_finished(),
                },
                message: HookWakeMessageObservation::default(),
                failure: None,
            }
        }

        /// Requests stop, observes both exact thread owners for at most
        /// `timeout`, and joins only threads already proven terminal.
        #[must_use]
        pub fn shutdown_checked(
            &mut self,
            timeout: Duration,
            reason: &'static str,
        ) -> super::OperatorHotkeyShutdownReport {
            if let Some(previous) = self.shutdown_report.as_ref() {
                let mut report = previous.clone();
                report.reason = reason;
                return report;
            }
            self.shutdown_started = true;
            let mut failures = Vec::new();
            let stop_readback = self.request_stop_and_read_sender(&mut failures);
            let hook_owner_present = self.hook_join.is_some();
            let worker_owner_present = self.worker_join.is_some();
            self.wait_for_thread_terminals(timeout);
            let (hook_thread_terminal, hook_thread_joined) = join_if_terminal(
                "hook",
                hook_owner_present,
                &mut self.hook_join,
                &mut failures,
            );
            let (worker_thread_terminal, worker_thread_joined) = join_if_terminal(
                "worker",
                worker_owner_present,
                &mut self.worker_join,
                &mut failures,
            );
            if hook_owner_present && hook_thread_terminal && self.hook_terminal_report.is_none() {
                failures.push(
                    "operator hotkey hook thread terminated without publishing unregister state"
                        .to_owned(),
                );
            }
            if !hook_thread_terminal {
                failures.push(format!(
                    "operator hotkey hook thread remained live after bounded {} ms shutdown observation",
                    timeout.as_millis()
                ));
            }
            if !worker_thread_terminal {
                failures.push(format!(
                    "operator hotkey worker thread remained live after bounded {} ms shutdown observation",
                    timeout.as_millis()
                ));
            }
            let kernel_report = self.kernel_report(&mut failures);
            let install_slot_released = release_install_slot_if_quiescent(
                self.owner_id,
                stop_readback.signal_sender_cleared,
                &kernel_report,
                &super::OperatorHotkeyThreadOwnersReport {
                    hook: super::OperatorHotkeyHookThreadReport {
                        hook_thread_terminal,
                        hook_thread_joined,
                    },
                    worker: super::OperatorHotkeyWorkerThreadReport {
                        worker_thread_terminal,
                        worker_thread_joined,
                    },
                },
                &mut failures,
            );
            super::set_operator_hotkey_status(super::OperatorHotkeyStatus::Unknown);
            let report = build_shutdown_report(ShutdownReportParts {
                reason,
                owner_id: self.owner_id,
                timeout,
                stop_readback: &stop_readback,
                kernel: kernel_report,
                threads: super::OperatorHotkeyThreadOwnersReport {
                    hook: super::OperatorHotkeyHookThreadReport {
                        hook_thread_terminal,
                        hook_thread_joined,
                    },
                    worker: super::OperatorHotkeyWorkerThreadReport {
                        worker_thread_terminal,
                        worker_thread_joined,
                    },
                },
                install_slot_released,
                failures,
            });
            if report.owners_quiescent() {
                self.shutdown_report = Some(report.clone());
            }
            report
        }

        fn request_stop_and_read_sender(&self, failures: &mut Vec<String>) -> HotkeyStopReadback {
            let wake = self.observe_hook_owner_for_atomic_stop();
            // The non-TID stop primitive is the atomic itself. The hook loop
            // never blocks in GetMessageW and polls this flag every 25 ms.
            self.stop_requested.store(true, Ordering::Release);
            if let Err(error) = disconnect_install_sender_checked(self.owner_id) {
                failures.push(error);
            }
            let signal_sender_cleared = match signal_sender_cleared_readback(self.owner_id) {
                Ok(cleared) => {
                    if !cleared {
                        failures.push(
                            "operator hotkey signal sender remained installed after clear"
                                .to_owned(),
                        );
                    }
                    cleared
                }
                Err(error) => {
                    failures.push(error);
                    false
                }
            };
            HotkeyStopReadback {
                wake,
                signal_sender_cleared,
            }
        }

        fn wait_for_thread_terminals(&mut self, timeout: Duration) {
            let deadline = Instant::now().checked_add(timeout);
            loop {
                self.try_recv_hook_terminal_report();
                let hook_terminal = self.hook_join.as_ref().is_none_or(JoinHandle::is_finished);
                let worker_terminal = self
                    .worker_join
                    .as_ref()
                    .is_none_or(JoinHandle::is_finished);
                if hook_terminal && worker_terminal {
                    break;
                }
                let Some(deadline) = deadline else {
                    break;
                };
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                thread::sleep(
                    deadline
                        .saturating_duration_since(now)
                        .min(THREAD_TERMINAL_POLL_INTERVAL),
                );
            }
            self.try_recv_hook_terminal_report();
        }

        fn try_recv_hook_terminal_report(&mut self) {
            if self.hook_terminal_report.is_none()
                && let Ok(report) = self.hook_terminal_rx.try_recv()
            {
                self.hook_terminal_report = Some(report);
            }
        }

        fn kernel_report(&self, failures: &mut Vec<String>) -> super::OperatorHotkeyKernelReport {
            self.hook_terminal_report.as_ref().map_or(
                super::OperatorHotkeyKernelReport {
                    low_level_hook_was_installed: None,
                    low_level_hook_unregistered: None,
                    register_hotkey_backup_was_registered: None,
                    register_hotkey_backup_unregistered: None,
                    kernel_owners_released: false,
                },
                |terminal| {
                    failures.extend(terminal.failures.iter().cloned());
                    super::OperatorHotkeyKernelReport {
                        low_level_hook_was_installed: terminal.low_level_hook_was_installed,
                        low_level_hook_unregistered: terminal.low_level_hook_unregistered,
                        register_hotkey_backup_was_registered: terminal
                            .register_hotkey_backup_was_registered,
                        register_hotkey_backup_unregistered: terminal
                            .register_hotkey_backup_unregistered,
                        kernel_owners_released: terminal.kernel_owners_released,
                    }
                },
            )
        }
    }

    struct HotkeyStopReadback {
        wake: HookWakeObservation,
        signal_sender_cleared: bool,
    }

    fn release_install_slot_if_quiescent(
        owner_id: u64,
        signal_sender_cleared: bool,
        kernel: &super::OperatorHotkeyKernelReport,
        threads: &super::OperatorHotkeyThreadOwnersReport,
        failures: &mut Vec<String>,
    ) -> bool {
        let physical_owners_quiescent = threads.hook.hook_thread_terminal
            && threads.hook.hook_thread_joined
            && threads.worker.worker_thread_terminal
            && threads.worker.worker_thread_joined
            && kernel.kernel_owners_released;
        if !physical_owners_quiescent || !signal_sender_cleared {
            return false;
        }
        match release_install_slot_checked(owner_id) {
            Ok(()) => true,
            Err(error) => {
                failures.push(error);
                false
            }
        }
    }

    struct ShutdownReportParts<'a> {
        reason: &'static str,
        owner_id: u64,
        timeout: Duration,
        stop_readback: &'a HotkeyStopReadback,
        kernel: super::OperatorHotkeyKernelReport,
        threads: super::OperatorHotkeyThreadOwnersReport,
        install_slot_released: bool,
        failures: Vec<String>,
    }

    fn build_shutdown_report(
        parts: ShutdownReportParts<'_>,
    ) -> super::OperatorHotkeyShutdownReport {
        super::OperatorHotkeyShutdownReport {
            reason: parts.reason,
            owner_id: parts.owner_id,
            timeout_ms: u64::try_from(parts.timeout.as_millis()).unwrap_or(u64::MAX),
            stop: super::OperatorHotkeyStopReport {
                stop_requested: true,
                signal_sender_cleared: parts.stop_readback.signal_sender_cleared,
                install_slot_released: parts.install_slot_released,
            },
            wake_observation: super::OperatorHotkeyWakeObservationReport {
                hook_owner_observed_live_before_wake: parts
                    .stop_readback
                    .wake
                    .owner
                    .live_before_wake,
                hook_owner_observed_live_after_wake: parts.stop_readback.wake.owner.live_after_wake,
            },
            wake_message: super::OperatorHotkeyWakeMessageReport {
                wake_message_attempted: parts.stop_readback.wake.message.attempted,
                wake_message_sent: parts.stop_readback.wake.message.sent,
            },
            kernel: parts.kernel,
            threads: parts.threads,
            failures: parts.failures,
        }
    }

    fn join_if_terminal(
        role: &'static str,
        owner_present: bool,
        join: &mut Option<JoinHandle<()>>,
        failures: &mut Vec<String>,
    ) -> (bool, bool) {
        if !owner_present {
            return (true, true);
        }
        if !join.as_ref().is_some_and(JoinHandle::is_finished) {
            return (false, false);
        }
        let Some(join) = join.take() else {
            failures.push(format!(
                "operator hotkey {role} thread owner disappeared before terminal join"
            ));
            return (true, false);
        };
        if join.join().is_err() {
            failures.push(format!("operator hotkey {role} thread panicked"));
        }
        (true, true)
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
                if data.vkCode == HOTKEY_KEY_VK.load(Ordering::Acquire)
                    && modifiers_down()
                    && !CHORD_DOWN.swap(true, Ordering::AcqRel)
                {
                    maybe_send_hotkey_signal("wh_keyboard_ll");
                }
            } else if matches!(message, WM_KEYUP | WM_SYSKEYUP) {
                let data = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
                if data.vkCode == HOTKEY_KEY_VK.load(Ordering::Acquire) {
                    CHORD_DOWN.store(false, Ordering::Release);
                }
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    fn modifiers_down() -> bool {
        key_down(VK_CONTROL_CODE) && key_down(VK_MENU_CODE) && key_down(VK_SHIFT_CODE)
    }

    fn key_down(vk: i32) -> bool {
        (unsafe { GetAsyncKeyState(vk) } & KEY_DOWN_MASK) != 0
    }

    fn maybe_send_hotkey_signal(source: &'static str) {
        let now_ms = unsafe { GetTickCount64() };
        if !claim_hotkey_signal_slot(now_ms, source) {
            return;
        }
        let operator_panic_token = super::request_operator_panic_interrupt();
        let operator_panic_generation = operator_panic_token.generation();
        let sender = match hotkey_install_state().try_lock() {
            Ok(guard) => guard.sender.clone(),
            Err(TryLockError::Poisoned(_error)) => {
                tracing::error!(
                    code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                    component = "operator_hotkey",
                    source,
                    result = "sender_lock_poisoned",
                    "operator hotkey signal sender lock poisoned"
                );
                None
            }
            Err(TryLockError::WouldBlock) => {
                tracing::error!(
                    code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                    component = "operator_hotkey",
                    source,
                    result = "sender_lock_contended",
                    "operator hotkey signal sender lock was busy; refusing to block the OS hook thread"
                );
                None
            }
        };
        let Some(sender) = sender else {
            emergency_operator_panic_k1(operator_panic_generation, source, "sender_missing");
            tracing::error!(
                code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                component = "operator_hotkey",
                source,
                result = "sender_missing",
                "operator hotkey fired but worker sender is missing"
            );
            return;
        };
        if let Err(error) = sender.send(HotkeySignal {
            source,
            operator_panic_token,
        }) {
            emergency_operator_panic_k1(operator_panic_generation, source, "send_failed");
            tracing::error!(
                code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                component = "operator_hotkey",
                source,
                operator_panic_generation,
                detail = %error,
                result = "send_failed",
                "operator hotkey fired but worker dispatch failed"
            );
        }
    }

    fn claim_hotkey_signal_slot(now_ms: u64, source: &'static str) -> bool {
        let mut previous = LAST_SIGNAL_TICK_MS.load(Ordering::Acquire);
        loop {
            if previous != 0 && now_ms >= previous && now_ms - previous < HOTKEY_SIGNAL_DEBOUNCE_MS
            {
                tracing::debug!(
                    component = "operator_hotkey",
                    source,
                    elapsed_ms = now_ms - previous,
                    debounce_ms = HOTKEY_SIGNAL_DEBOUNCE_MS,
                    "operator hotkey duplicate signal ignored"
                );
                return false;
            }
            match LAST_SIGNAL_TICK_MS.compare_exchange_weak(
                previous,
                now_ms,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => previous = actual,
            }
        }
    }

    fn hotkey_install_state() -> &'static Mutex<HotkeyInstallState> {
        HOTKEY_INSTALL_STATE.get_or_init(|| Mutex::new(HotkeyInstallState::empty()))
    }

    fn reserve_install_state(
        owner_id: u64,
        sender: mpsc::Sender<HotkeySignal>,
    ) -> ActionResult<()> {
        let mut state =
            hotkey_install_state()
                .try_lock()
                .map_err(|error| ActionError::BackendUnavailable {
                    detail: match error {
                        TryLockError::Poisoned(_) => {
                            "operator hotkey installation-state lock poisoned".to_owned()
                        }
                        TryLockError::WouldBlock => {
                            "operator hotkey installation-state lock contended during reservation"
                                .to_owned()
                        }
                    },
                })?;
        if state.owner_id.is_some() || state.sender.is_some() {
            let detail = format!(
                "operator hotkey is already installed or retained in this process: owner_id={:?} sender_present={}",
                state.owner_id,
                state.sender.is_some()
            );
            drop(state);
            return Err(ActionError::BackendUnavailable { detail });
        }
        state.owner_id = Some(owner_id);
        state.sender = Some(sender);
        drop(state);
        Ok(())
    }

    fn disconnect_and_release_unstarted_owner(owner_id: u64) -> Result<(), String> {
        disconnect_install_sender_checked(owner_id)?;
        release_install_slot_checked(owner_id)
    }

    fn disconnect_install_sender_checked(owner_id: u64) -> Result<(), String> {
        let mut state = match hotkey_install_state().try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(_)) => {
                return Err(
                    "operator hotkey installation-state lock was poisoned; exact sender ownership is unobservable"
                        .to_owned(),
                );
            }
            Err(TryLockError::WouldBlock) => {
                return Err(
                    "operator hotkey installation-state lock was contended; bounded shutdown refused to wait"
                        .to_owned(),
                );
            }
        };
        if state.owner_id == Some(owner_id) {
            state.sender = None;
        } else if !(state.owner_id.is_none()
            && state.last_released_owner_id == Some(owner_id)
            && state.sender.is_none())
        {
            let detail = format!(
                "operator hotkey sender disconnect owner mismatch: expected={owner_id} actual={:?}",
                state.owner_id
            );
            drop(state);
            return Err(detail);
        }
        drop(state);
        HOTKEY_KEY_VK.store(0, Ordering::Release);
        CHORD_DOWN.store(false, Ordering::Release);
        LAST_SIGNAL_TICK_MS.store(0, Ordering::Release);
        Ok(())
    }

    fn signal_sender_cleared_readback(owner_id: u64) -> Result<bool, String> {
        match hotkey_install_state().try_lock() {
            Ok(state) if state.owner_id == Some(owner_id) => Ok(state.sender.is_none()),
            Ok(state)
                if state.owner_id.is_none()
                    && state.last_released_owner_id == Some(owner_id)
                    && state.sender.is_none() =>
            {
                Ok(true)
            }
            Ok(state) => Err(format!(
                "operator hotkey sender readback owner mismatch: expected={owner_id} actual={:?}",
                state.owner_id
            )),
            Err(TryLockError::Poisoned(_)) => Err(
                "operator hotkey sender readback lock was poisoned; state is unobservable"
                    .to_owned(),
            ),
            Err(TryLockError::WouldBlock) => Err(
                "operator hotkey sender readback lock was contended; state is unobservable"
                    .to_owned(),
            ),
        }
    }

    fn release_install_slot_checked(owner_id: u64) -> Result<(), String> {
        let mut state = match hotkey_install_state().try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(_)) => {
                return Err(
                    "operator hotkey installation-state lock was poisoned; exact slot release is unobservable"
                        .to_owned(),
                );
            }
            Err(TryLockError::WouldBlock) => {
                return Err(
                    "operator hotkey installation-state lock was contended; exact slot release is unobservable"
                        .to_owned(),
                );
            }
        };
        if state.owner_id == Some(owner_id) {
            if state.sender.is_some() {
                let detail = format!(
                    "operator hotkey installation slot for owner {owner_id} still owns a worker sender"
                );
                drop(state);
                return Err(detail);
            }
            state.owner_id = None;
            state.last_released_owner_id = Some(owner_id);
            drop(state);
            return Ok(());
        }
        if state.owner_id.is_none()
            && state.last_released_owner_id == Some(owner_id)
            && state.sender.is_none()
        {
            drop(state);
            return Ok(());
        }
        let detail = format!(
            "operator hotkey installation-slot release owner mismatch: expected={owner_id} actual={:?} last_released={:?}",
            state.owner_id, state.last_released_owner_id
        );
        drop(state);
        Err(detail)
    }

    fn set_current_thread_high_priority(role: &'static str) -> bool {
        let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) }.is_ok();
        if !ok {
            tracing::warn!(
                component = "operator_hotkey",
                role,
                "failed to raise operator hotkey thread priority"
            );
        }
        ok
    }

    fn invalid_hotkey(raw: &str, reason: &str) -> ActionError {
        ActionError::BackendUnavailable {
            detail: format!(
                "{} / {} must be Ctrl+Alt+Shift+<A-Z|0-9>; got {:?}: {reason}",
                super::OPERATOR_HOTKEY_ENV,
                super::OPERATOR_HOTKEY_COMPAT_ENV,
                raw
            ),
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use std::time::Duration;

    use crate::{ActionResult, hotkey::OperatorHotkeyShutdownReport};

    pub struct OperatorHotkeyGuard;

    impl OperatorHotkeyGuard {
        #[must_use]
        pub fn shutdown_checked(
            &mut self,
            timeout: Duration,
            reason: &'static str,
        ) -> OperatorHotkeyShutdownReport {
            OperatorHotkeyShutdownReport {
                reason,
                owner_id: 0,
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                stop: super::OperatorHotkeyStopReport {
                    stop_requested: true,
                    signal_sender_cleared: true,
                    install_slot_released: true,
                },
                wake_observation: super::OperatorHotkeyWakeObservationReport {
                    hook_owner_observed_live_before_wake: false,
                    hook_owner_observed_live_after_wake: false,
                },
                wake_message: super::OperatorHotkeyWakeMessageReport {
                    wake_message_attempted: false,
                    wake_message_sent: false,
                },
                kernel: super::OperatorHotkeyKernelReport {
                    low_level_hook_was_installed: Some(false),
                    low_level_hook_unregistered: None,
                    register_hotkey_backup_was_registered: Some(false),
                    register_hotkey_backup_unregistered: None,
                    kernel_owners_released: true,
                },
                threads: super::OperatorHotkeyThreadOwnersReport {
                    hook: super::OperatorHotkeyHookThreadReport {
                        hook_thread_terminal: true,
                        hook_thread_joined: true,
                    },
                    worker: super::OperatorHotkeyWorkerThreadReport {
                        worker_thread_terminal: true,
                        worker_thread_joined: true,
                    },
                },
                failures: Vec::new(),
            }
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn install_operator_hotkey<F>(_handler: F) -> ActionResult<OperatorHotkeyGuard>
    where
        F: Fn(super::OperatorPanicSafetyToken) + Send + 'static,
    {
        tracing::warn!(
            component = "operator_hotkey",
            "operator hotkey is only registered on Windows"
        );
        Ok(OperatorHotkeyGuard)
    }

    pub const fn install_unwind_report() -> Option<OperatorHotkeyShutdownReport> {
        None
    }

    pub const fn install_unwind_retained_live_owner() -> bool {
        false
    }
}

pub use platform::OperatorHotkeyGuard;

/// Last bounded cleanup report produced by a failed hotkey installation.
#[must_use]
pub fn operator_hotkey_install_unwind_report() -> Option<OperatorHotkeyShutdownReport> {
    platform::install_unwind_report()
}

/// Whether a failed installation retained an exact live thread owner or
/// reported unresolved kernel ownership until process teardown. Daemon
/// transports must retain their lifetime locks when this is true.
#[must_use]
pub fn operator_hotkey_install_unwind_retained_live_owner() -> bool {
    platform::install_unwind_retained_live_owner()
}

/// Registers the system-wide operator panic hotkey.
///
/// # Errors
///
/// Returns a [`crate::ActionError`] when the platform hotkey thread or
/// low-level keyboard hook cannot start.
pub fn install_operator_hotkey<F>(handler: F) -> ActionResult<OperatorHotkeyGuard>
where
    F: Fn(OperatorPanicSafetyToken) + Send + 'static,
{
    platform::install_operator_hotkey(handler)
}

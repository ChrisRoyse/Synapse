use std::{
    error::Error,
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use criterion::Criterion;
use synapse_action::{
    ActionBackend, ActionEmitter, ActionEmitterSnapshotHandle, ActionHandle, ActionStateSnapshot,
    RecordedInput, RecordingBackend, VigemBackend,
};
#[cfg(not(windows))]
use synapse_core::error_codes;
use synapse_core::{Action, GamepadReport, PadButton};
use tokio::{runtime::Runtime, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const BENCH_NAME: &str = "action_vigem_pad_report";
const PAD_ID: u8 = 0;
const RECORDING_ITERATIONS: usize = 500;
#[cfg(windows)]
const WINDOWS_ITERATIONS: usize = 300;
const VIGEM_TARGET_P99_NS: u64 = 5_000_000;
const VIGEM_TARGET_REPORTS_PER_S: u64 = 500;
const VIGEM_RATE_LIMIT_SAFE_PACE: Duration = Duration::from_millis(1);
#[cfg(windows)]
const XINPUT_GAMEPAD_A_RAW: u16 = 0x1000;
#[cfg(windows)]
const XINPUT_GAMEPAD_B_RAW: u16 = 0x2000;
#[cfg(windows)]
const ERROR_SUCCESS: u32 = 0;
#[cfg(windows)]
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;
#[cfg(windows)]
const XINPUT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(windows)]
const XINPUT_POLL_INTERVAL: Duration = Duration::from_millis(1);
#[cfg(windows)]
const REAL_VIGEM_ENV: &str = "SYNAPSE_ACTION_VIGEM_PAD_REAL";

fn main() -> Result<(), Box<dyn Error>> {
    {
        let mut criterion = Criterion::default()
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_secs(1))
            .sample_size(20)
            .configure_from_args();

        bench_action_vigem_pad_report_recording(&mut criterion);
        #[cfg(windows)]
        if real_vigem_enabled() {
            bench_action_vigem_pad_report_driver(&mut criterion);
        }
        criterion.final_summary();
    }

    for report in manual_reports()? {
        report.print();
        assert!(
            report.pass,
            "action_vigem_pad_report {} {} did not pass",
            report.mode, report.edge
        );
        if report.enforces_driver_target {
            let p99 = report
                .p99_report_ns
                .ok_or("driver target report missing p99")?;
            assert!(
                p99 <= u128::from(VIGEM_TARGET_P99_NS),
                "action_vigem_pad_report p99 {p99} ns exceeded {VIGEM_TARGET_P99_NS} ns"
            );
            let reports_per_s = report
                .reports_per_s
                .ok_or("driver target report missing throughput")?;
            assert!(
                reports_per_s >= VIGEM_TARGET_REPORTS_PER_S,
                "action_vigem_pad_report throughput {reports_per_s} reports/s below {VIGEM_TARGET_REPORTS_PER_S}"
            );
        }
    }

    Ok(())
}

fn bench_action_vigem_pad_report_recording(criterion: &mut Criterion) {
    let harness = PadHarness::recording()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} recording harness should start: {err}"));

    criterion.bench_function(BENCH_NAME, |bencher| {
        bencher.iter_custom(|iterations| {
            let mut total_report_ns = 0_u128;
            for iteration in 0..iterations {
                let readback = harness.report_once(report_for_iteration(iteration));
                let readback = readback
                    .unwrap_or_else(|err| panic!("{BENCH_NAME} recording iteration failed: {err}"));
                total_report_ns = total_report_ns.saturating_add(readback.report_ns);
                black_box(readback.actor_has_pad);
                harness
                    .release_all()
                    .unwrap_or_else(|err| panic!("{BENCH_NAME} recording release failed: {err}"));
                std::thread::sleep(VIGEM_RATE_LIMIT_SAFE_PACE);
            }
            duration_from_nanos_saturating(total_report_ns)
        });
    });

    harness
        .shutdown()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} recording harness shutdown failed: {err}"));
}

#[cfg(windows)]
fn bench_action_vigem_pad_report_driver(criterion: &mut Criterion) {
    ensure_vigem_ready().unwrap_or_else(|err| panic!("{BENCH_NAME} ViGEm readiness failed: {err}"));
    let harness = PadHarness::production()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} ViGEm harness should start: {err}"));

    criterion.bench_function("action_vigem_pad_report_driver", |bencher| {
        bencher.iter_custom(|iterations| {
            let mut total_report_ns = 0_u128;
            for iteration in 0..iterations {
                let readback = harness.report_once(report_for_iteration(iteration));
                let readback = readback
                    .unwrap_or_else(|err| panic!("{BENCH_NAME} ViGEm iteration failed: {err}"));
                total_report_ns = total_report_ns.saturating_add(readback.report_ns);
                black_box(readback.actor_has_pad);
                harness
                    .release_all()
                    .unwrap_or_else(|err| panic!("{BENCH_NAME} ViGEm release failed: {err}"));
                std::thread::sleep(VIGEM_RATE_LIMIT_SAFE_PACE);
            }
            duration_from_nanos_saturating(total_report_ns)
        });
    });

    harness
        .shutdown()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} ViGEm harness shutdown failed: {err}"));
}

fn manual_reports() -> Result<Vec<BenchReport>, Box<dyn Error>> {
    let mut reports = vec![measure_recording_reference()?];
    platform_report(&mut reports)?;
    Ok(reports)
}

fn measure_recording_reference() -> Result<BenchReport, Box<dyn Error>> {
    let harness = PadHarness::recording()?;
    let before_event_count = harness.recording_event_count();
    let before_snapshot = harness.snapshot()?;
    let mut elapsed = Vec::with_capacity(RECORDING_ITERATIONS);
    let throughput_started = Instant::now();

    for iteration in 0..RECORDING_ITERATIONS {
        let readback = harness.report_once(report_for_iteration_usize(iteration))?;
        elapsed.push(readback.report_ns);
        std::thread::sleep(VIGEM_RATE_LIMIT_SAFE_PACE);
    }

    let throughput_elapsed = throughput_started.elapsed();
    let held_snapshot = harness.snapshot()?;
    harness.release_all()?;
    let release_snapshot = harness.snapshot()?;
    let new_events = harness.recording_events_since(before_event_count);
    let final_snapshot = harness.shutdown()?;
    elapsed.sort_unstable();
    let reports_per_s = reports_per_second(RECORDING_ITERATIONS, throughput_elapsed);

    Ok(BenchReport {
        mode: "recording",
        edge: "pad_report_ack_then_release_all_cleanup",
        iterations: RECORDING_ITERATIONS,
        before: format!("events:{before_event_count} snapshot:{before_snapshot:?}"),
        after: format!(
            "new_events:{} first_event:{} last_event:{} held_snapshot:{held_snapshot:?} release_snapshot:{release_snapshot:?} final_snapshot:{final_snapshot:?}",
            new_events.len(),
            new_events
                .first()
                .map_or_else(|| "<none>".to_owned(), event_label),
            new_events
                .last()
                .map_or_else(|| "<none>".to_owned(), event_label)
        ),
        p50_report_ns: Some(percentile(&elapsed, 50)),
        p99_report_ns: Some(percentile(&elapsed, 99)),
        max_report_ns: elapsed.last().copied(),
        reports_per_s: Some(reports_per_s),
        pass: held_snapshot.pad_state.contains_key(&PAD_ID)
            && actor_is_empty(&release_snapshot)
            && actor_is_empty(&final_snapshot)
            && new_events.len() == RECORDING_ITERATIONS.saturating_add(2),
        enforces_driver_target: false,
    })
}

#[cfg(not(windows))]
fn platform_report(reports: &mut Vec<BenchReport>) -> Result<(), Box<dyn Error>> {
    reports.push(measure_non_windows_fail_closed()?);
    Ok(())
}

#[cfg(windows)]
fn platform_report(reports: &mut Vec<BenchReport>) -> Result<(), Box<dyn Error>> {
    if real_vigem_enabled() {
        reports.push(measure_windows_vigem()?);
    } else {
        reports.push(BenchReport {
            mode: "windows_vigem",
            edge: "real_vigem_opt_in",
            iterations: 0,
            before: format!("{REAL_VIGEM_ENV}=unset"),
            after: "skipped_real_vigem_to_avoid_unrequested_virtual_controller_events".to_owned(),
            p50_report_ns: None,
            p99_report_ns: None,
            max_report_ns: None,
            reports_per_s: None,
            pass: true,
            enforces_driver_target: false,
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn measure_non_windows_fail_closed() -> Result<BenchReport, Box<dyn Error>> {
    let backend = VigemBackend::new();
    let before_error = backend.ensure_ready().err();
    let before_code = before_error
        .as_ref()
        .map_or("<none>", synapse_action::ActionError::code);

    let harness = PadHarness::production()?;
    let before = harness.snapshot()?;
    let error = harness
        .execute(pad_report_action(report_for_iteration_usize(0)))
        .err();
    let after = harness.snapshot()?;
    let final_snapshot = harness.shutdown()?;
    let code = error
        .as_ref()
        .map_or("<none>", synapse_action::ActionError::code);

    Ok(BenchReport {
        mode: "production",
        edge: "non_windows_vigem_fails_closed",
        iterations: 1,
        before: format!("ensure_ready_code:{before_code} snapshot:{before:?}"),
        after: format!("error_code:{code} snapshot:{after:?} final_snapshot:{final_snapshot:?}"),
        p50_report_ns: None,
        p99_report_ns: None,
        max_report_ns: None,
        reports_per_s: None,
        pass: before_code == error_codes::ACTION_BACKEND_UNAVAILABLE
            && code == error_codes::ACTION_BACKEND_UNAVAILABLE
            && actor_is_empty(&after)
            && actor_is_empty(&final_snapshot),
        enforces_driver_target: false,
    })
}

#[cfg(windows)]
fn measure_windows_vigem() -> Result<BenchReport, Box<dyn Error>> {
    ensure_vigem_ready()?;
    let before_slots = read_all_slots();
    let harness = PadHarness::production()?;
    let before_snapshot = harness.snapshot()?;
    let mut elapsed = Vec::with_capacity(WINDOWS_ITERATIONS);
    let throughput_started = Instant::now();

    for iteration in 0..WINDOWS_ITERATIONS {
        let readback = harness.report_once(report_for_iteration_usize(iteration))?;
        elapsed.push(readback.report_ns);
        std::thread::sleep(VIGEM_RATE_LIMIT_SAFE_PACE);
    }

    let throughput_elapsed = throughput_started.elapsed();
    let after_reports = poll_xinput_until(XINPUT_POLL_TIMEOUT, |states| {
        find_new_button_slot(&before_slots, states, expected_final_button()).is_some()
    });
    let observed_slot =
        find_new_button_slot(&before_slots, &after_reports, expected_final_button());
    let held_snapshot = harness.snapshot()?;
    harness.release_all()?;
    let after_release = observed_slot.map_or_else(read_all_slots, |slot| {
        poll_xinput_until(XINPUT_POLL_TIMEOUT, |states| {
            slot_connected_neutral(states, slot)
        })
    });
    let release_snapshot = harness.snapshot()?;
    let final_snapshot = harness.shutdown()?;
    elapsed.sort_unstable();
    let p99 = percentile(&elapsed, 99);
    let reports_per_s = reports_per_second(WINDOWS_ITERATIONS, throughput_elapsed);

    Ok(BenchReport {
        mode: "windows_vigem",
        edge: "xinput_observes_report_and_release",
        iterations: WINDOWS_ITERATIONS,
        before: format!(
            "ensure_ready:ok xinput:{} snapshot:{before_snapshot:?}",
            format_slots(&before_slots)
        ),
        after: format!(
            "observed_slot:{} after_reports:{} held_snapshot:{held_snapshot:?} after_release:{} release_snapshot:{release_snapshot:?} final_snapshot:{final_snapshot:?}",
            observed_slot.map_or_else(|| "none".to_owned(), |slot| slot.to_string()),
            format_slots(&after_reports),
            format_slots(&after_release)
        ),
        p50_report_ns: Some(percentile(&elapsed, 50)),
        p99_report_ns: Some(p99),
        max_report_ns: elapsed.last().copied(),
        reports_per_s: Some(reports_per_s),
        pass: observed_slot.is_some()
            && observed_slot.is_none_or(|slot| slot_connected_neutral(&after_release, slot))
            && held_snapshot.pad_state.contains_key(&PAD_ID)
            && actor_is_empty(&release_snapshot)
            && actor_is_empty(&final_snapshot)
            && p99 <= u128::from(VIGEM_TARGET_P99_NS)
            && reports_per_s >= VIGEM_TARGET_REPORTS_PER_S,
        enforces_driver_target: true,
    })
}

#[derive(Debug)]
struct PadHarness {
    runtime: Runtime,
    cancel: CancellationToken,
    handle: ActionHandle,
    snapshot_handle: ActionEmitterSnapshotHandle,
    join: JoinHandle<ActionStateSnapshot>,
    recording: Option<Arc<RecordingBackend>>,
}

impl PadHarness {
    fn recording() -> Result<Self, Box<dyn Error>> {
        let runtime = runtime()?;
        let cancel = CancellationToken::new();
        let recording = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) = runtime.block_on(async {
            ActionEmitter::spawn_with_backend(
                cancel.clone(),
                Arc::<RecordingBackend>::clone(&recording) as Arc<dyn ActionBackend>,
            )
        });
        Ok(Self {
            runtime,
            cancel,
            handle,
            snapshot_handle,
            join,
            recording: Some(recording),
        })
    }

    fn production() -> Result<Self, Box<dyn Error>> {
        let runtime = runtime()?;
        let cancel = CancellationToken::new();
        let (handle, snapshot_handle, join) =
            runtime.block_on(async { ActionEmitter::spawn(cancel.clone()) });
        Ok(Self {
            runtime,
            cancel,
            handle,
            snapshot_handle,
            join,
            recording: None,
        })
    }

    fn report_once(&self, report: GamepadReport) -> Result<PadReadback, Box<dyn Error>> {
        let started = Instant::now();
        self.execute(pad_report_action(report))?;
        let report_ns = started.elapsed().as_nanos();
        let snapshot = self.snapshot()?;

        Ok(PadReadback {
            report_ns,
            actor_has_pad: snapshot.pad_state.contains_key(&PAD_ID),
        })
    }

    fn release_all(&self) -> Result<(), synapse_action::ActionError> {
        self.execute(Action::ReleaseAll)
    }

    fn execute(&self, action: Action) -> Result<(), synapse_action::ActionError> {
        self.runtime.block_on(self.handle.execute(action))
    }

    fn snapshot(&self) -> Result<ActionStateSnapshot, synapse_action::ActionError> {
        self.runtime.block_on(self.snapshot_handle.snapshot())
    }

    fn recording_event_count(&self) -> usize {
        self.recording
            .as_ref()
            .map_or(0, |recording| recording.event_count())
    }

    fn recording_events_since(&self, event_count: usize) -> Vec<RecordedInput> {
        self.recording
            .as_ref()
            .map_or_else(Vec::new, |recording| recording.events_since(event_count))
    }

    fn shutdown(self) -> Result<ActionStateSnapshot, Box<dyn Error>> {
        self.cancel.cancel();
        Ok(self.runtime.block_on(self.join)?)
    }
}

#[derive(Debug)]
struct PadReadback {
    report_ns: u128,
    actor_has_pad: bool,
}

#[derive(Debug)]
struct BenchReport {
    mode: &'static str,
    edge: &'static str,
    iterations: usize,
    before: String,
    after: String,
    p50_report_ns: Option<u128>,
    p99_report_ns: Option<u128>,
    max_report_ns: Option<u128>,
    reports_per_s: Option<u64>,
    pass: bool,
    enforces_driver_target: bool,
}

impl BenchReport {
    fn print(&self) {
        println!(
            "readback=action_vigem_pad_report mode={} edge={} before={} after={} iterations:{} p50_report_ns:{} p99_report_ns:{} max_report_ns:{} reports_per_s:{} target_p99_ns:{} target_reports_per_s:{} result_value={}",
            self.mode,
            self.edge,
            self.before,
            self.after,
            self.iterations,
            display_opt(self.p50_report_ns),
            display_opt(self.p99_report_ns),
            display_opt(self.max_report_ns),
            self.reports_per_s
                .map_or_else(|| "n/a".to_owned(), |value| value.to_string()),
            if self.enforces_driver_target {
                u128::from(VIGEM_TARGET_P99_NS).to_string()
            } else {
                "not_enforced_for_this_mode".to_owned()
            },
            if self.enforces_driver_target {
                VIGEM_TARGET_REPORTS_PER_S.to_string()
            } else {
                "not_enforced_for_this_mode".to_owned()
            },
            if self.pass { "pass" } else { "fail" }
        );
    }
}

fn runtime() -> Result<Runtime, Box<dyn Error>> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?)
}

fn duration_from_nanos_saturating(nanos: u128) -> Duration {
    let capped = nanos.min(u128::from(u64::MAX));
    let nanos = u64::try_from(capped).unwrap_or(u64::MAX);
    Duration::from_nanos(nanos)
}

fn percentile(values: &[u128], percentile: usize) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let index = (values.len().saturating_sub(1) * percentile) / 100;
    values[index]
}

fn reports_per_second(iterations: usize, elapsed: Duration) -> u64 {
    if elapsed.is_zero() {
        return u64::MAX;
    }
    let reports = u128::try_from(iterations).unwrap_or(u128::MAX);
    let per_s = (reports * 1_000_000_000) / elapsed.as_nanos().max(1);
    u64::try_from(per_s).unwrap_or(u64::MAX)
}

fn actor_is_empty(snapshot: &ActionStateSnapshot) -> bool {
    snapshot.held_keys.is_empty()
        && snapshot.held_buttons.is_empty()
        && snapshot.pad_state.is_empty()
        && snapshot.held_key_timer_count == 0
}

fn event_label(event: &RecordedInput) -> String {
    format!("{event:?}")
}

const fn pad_report_action(report: GamepadReport) -> Action {
    Action::PadReport {
        pad: PAD_ID,
        report,
    }
}

fn report_for_iteration(iteration: u64) -> GamepadReport {
    report_for_button(if iteration.is_multiple_of(2) {
        PadButton::A
    } else {
        PadButton::B
    })
}

fn report_for_iteration_usize(iteration: usize) -> GamepadReport {
    report_for_button(if iteration.is_multiple_of(2) {
        PadButton::A
    } else {
        PadButton::B
    })
}

fn report_for_button(button: PadButton) -> GamepadReport {
    GamepadReport {
        buttons: vec![button],
        ..GamepadReport::default()
    }
}

fn display_opt(value: Option<u128>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| value.to_string())
}

#[cfg(windows)]
fn real_vigem_enabled() -> bool {
    std::env::var_os(REAL_VIGEM_ENV).is_some_and(|value| value == "1")
}

#[cfg(windows)]
fn ensure_vigem_ready() -> Result<(), Box<dyn Error>> {
    let backend = VigemBackend::new();
    backend.ensure_ready().map_err(|err| {
        format!(
            "ViGEm backend is not ready; install/repair ViGEmBus on this host, code={} detail={}",
            err.code(),
            err
        )
        .into()
    })
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct XInputSlotState {
    slot: u32,
    rc: u32,
    packet: u32,
    buttons: u16,
}

#[cfg(windows)]
impl XInputSlotState {
    const fn connected(slot: u32, packet: u32, buttons: u16) -> Self {
        Self {
            slot,
            rc: ERROR_SUCCESS,
            packet,
            buttons,
        }
    }

    const fn has_button(&self, button: u16) -> bool {
        self.rc == ERROR_SUCCESS && (self.buttons & button) == button
    }

    const fn is_connected_neutral(&self) -> bool {
        self.rc == ERROR_SUCCESS && self.buttons == 0
    }
}

#[cfg(windows)]
const fn expected_final_button() -> u16 {
    if (WINDOWS_ITERATIONS - 1).is_multiple_of(2) {
        XINPUT_GAMEPAD_A_RAW
    } else {
        XINPUT_GAMEPAD_B_RAW
    }
}

#[cfg(windows)]
fn read_all_slots() -> Vec<XInputSlotState> {
    (0..4).map(read_slot).collect()
}

#[cfg(windows)]
fn read_slot(slot: u32) -> XInputSlotState {
    use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

    let mut state = XINPUT_STATE::default();
    let rc = unsafe { XInputGetState(slot, &raw mut state) };
    if rc == ERROR_SUCCESS {
        XInputSlotState::connected(slot, state.dwPacketNumber, state.Gamepad.wButtons.0)
    } else {
        XInputSlotState {
            slot,
            rc: if rc == ERROR_DEVICE_NOT_CONNECTED {
                ERROR_DEVICE_NOT_CONNECTED
            } else {
                rc
            },
            packet: 0,
            buttons: 0,
        }
    }
}

#[cfg(windows)]
fn poll_xinput_until<F>(timeout: Duration, mut predicate: F) -> Vec<XInputSlotState>
where
    F: FnMut(&[XInputSlotState]) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let states = read_all_slots();
        if predicate(&states) || Instant::now() >= deadline {
            return states;
        }
        std::thread::sleep(XINPUT_POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn find_new_button_slot(
    before: &[XInputSlotState],
    after: &[XInputSlotState],
    button: u16,
) -> Option<u32> {
    after
        .iter()
        .find(|current| {
            current.has_button(button)
                && !before
                    .iter()
                    .any(|previous| previous.slot == current.slot && previous.has_button(button))
        })
        .map(|state| state.slot)
}

#[cfg(windows)]
fn slot_connected_neutral(states: &[XInputSlotState], slot: u32) -> bool {
    states
        .iter()
        .find(|state| state.slot == slot)
        .is_some_and(XInputSlotState::is_connected_neutral)
}

#[cfg(windows)]
fn format_slots(states: &[XInputSlotState]) -> String {
    states
        .iter()
        .map(|state| {
            format!(
                "slot={} rc={} packet={} wButtons=0x{:04x}",
                state.slot, state.rc, state.packet, state.buttons
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

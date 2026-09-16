use std::{
    error::Error,
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use criterion::Criterion;
use synapse_action::{
    ActionBackend, ActionEmitter, ActionEmitterSnapshotHandle, ActionHandle, ActionStateSnapshot,
    RecordedInput, RecordingBackend,
};
#[cfg(not(windows))]
use synapse_core::error_codes;
use synapse_core::{Action, Backend, ButtonAction, MouseButton};
use tokio::{runtime::Runtime, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const BENCH_NAME: &str = "action_software_click";
const RECORDING_ITERATIONS: usize = 2_000;
#[cfg(windows)]
const WINDOWS_ITERATIONS: usize = 200;
const WINDOWS_TARGET_P99_NS: u64 = 5_000_000;
const RATE_LIMIT_SAFE_PACE: Duration = Duration::from_micros(250);
#[cfg(windows)]
const WINDOWS_BUTTON_STATE_TIMEOUT: Duration = Duration::from_nanos(WINDOWS_TARGET_P99_NS);
#[cfg(windows)]
const LEFT_BUTTON_LABEL: &str = "LeftButton";
#[cfg(windows)]
const LEFT_BUTTON_VK: i32 = 0x01;
#[cfg(windows)]
const REAL_SENDINPUT_ENV: &str = "SYNAPSE_ACTION_SOFTWARE_CLICK_REAL";

fn main() -> Result<(), Box<dyn Error>> {
    {
        let mut criterion = Criterion::default()
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_secs(1))
            .sample_size(20)
            .configure_from_args();

        bench_action_software_click_recording(&mut criterion);
        #[cfg(windows)]
        if real_sendinput_enabled() {
            bench_action_software_click_sendinput(&mut criterion);
        }
        criterion.final_summary();
    }

    for report in manual_reports()? {
        report.print();
        assert!(
            report.pass,
            "action_software_click {} {} did not pass",
            report.mode, report.edge
        );
        if report.enforces_windows_target {
            let p99 = report
                .p99_down_ns
                .ok_or("windows target report missing p99")?;
            assert!(
                p99 <= u128::from(WINDOWS_TARGET_P99_NS),
                "action_software_click windows p99 {p99} ns exceeded {WINDOWS_TARGET_P99_NS} ns"
            );
        }
    }

    Ok(())
}

fn bench_action_software_click_recording(criterion: &mut Criterion) {
    let harness = ClickHarness::recording()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} recording harness should start: {err}"));

    criterion.bench_function(BENCH_NAME, |bencher| {
        bencher.iter_custom(|iterations| {
            let mut total_down_ns = 0_u128;
            for _ in 0..iterations {
                let readback = harness
                    .click_once()
                    .unwrap_or_else(|err| panic!("{BENCH_NAME} recording iteration failed: {err}"));
                total_down_ns = total_down_ns.saturating_add(readback.down_ns);
                black_box(readback.actor_empty);
                std::thread::sleep(RATE_LIMIT_SAFE_PACE);
            }
            duration_from_nanos_saturating(total_down_ns)
        });
    });

    harness
        .shutdown()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} recording harness shutdown failed: {err}"));
}

#[cfg(windows)]
fn bench_action_software_click_sendinput(criterion: &mut Criterion) {
    let harness = ClickHarness::production()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} SendInput harness should start: {err}"));

    criterion.bench_function("action_software_click_sendinput", |bencher| {
        bencher.iter_custom(|iterations| {
            let mut total_down_ns = 0_u128;
            for _ in 0..iterations {
                let readback = harness
                    .click_once_observing_button()
                    .unwrap_or_else(|err| panic!("{BENCH_NAME} SendInput iteration failed: {err}"));
                total_down_ns = total_down_ns.saturating_add(readback.down_ns);
                black_box(readback.actor_empty);
                std::thread::sleep(RATE_LIMIT_SAFE_PACE);
            }
            duration_from_nanos_saturating(total_down_ns)
        });
    });

    harness
        .shutdown()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} SendInput harness shutdown failed: {err}"));
}

fn manual_reports() -> Result<Vec<BenchReport>, Box<dyn Error>> {
    let mut reports = vec![measure_recording_reference()?];
    platform_report(&mut reports)?;
    Ok(reports)
}

fn measure_recording_reference() -> Result<BenchReport, Box<dyn Error>> {
    let harness = ClickHarness::recording()?;
    let mut elapsed = Vec::with_capacity(RECORDING_ITERATIONS);
    let mut latest = None;

    for _ in 0..RECORDING_ITERATIONS {
        let readback = harness.click_once()?;
        elapsed.push(readback.down_ns);
        latest = Some(readback);
        std::thread::sleep(RATE_LIMIT_SAFE_PACE);
    }

    let final_snapshot = harness.shutdown()?;
    assert!(
        actor_is_empty(&final_snapshot),
        "recording final snapshot was not empty"
    );
    elapsed.sort_unstable();
    let latest = latest.ok_or("recording bench produced no samples")?;

    Ok(BenchReport {
        mode: "recording",
        edge: "left_down_ack_then_up_cleanup",
        iterations: RECORDING_ITERATIONS,
        before: "events:0 actor_empty:true".to_owned(),
        after: format!(
            "new_events:{} first_event:{} last_event:{} actor_empty:{}",
            latest.new_event_count, latest.first_event, latest.last_event, latest.actor_empty
        ),
        p50_down_ns: Some(percentile(&elapsed, 50)),
        p99_down_ns: Some(percentile(&elapsed, 99)),
        max_down_ns: elapsed.last().copied(),
        pass: latest.actor_empty && latest.new_event_count == 2,
        enforces_windows_target: false,
    })
}

#[cfg(not(windows))]
fn platform_report(reports: &mut Vec<BenchReport>) -> Result<(), Box<dyn Error>> {
    reports.push(measure_non_windows_fail_closed()?);
    Ok(())
}

#[cfg(windows)]
fn platform_report(reports: &mut Vec<BenchReport>) -> Result<(), Box<dyn Error>> {
    if real_sendinput_enabled() {
        reports.push(measure_windows_sendinput()?);
    } else {
        reports.push(BenchReport {
            mode: "windows_sendinput",
            edge: "real_sendinput_opt_in",
            iterations: 0,
            before: format!("{REAL_SENDINPUT_ENV}=unset"),
            after: "skipped_real_input_to_avoid_unrequested_desktop_clicks".to_owned(),
            p50_down_ns: None,
            p99_down_ns: None,
            max_down_ns: None,
            pass: true,
            enforces_windows_target: false,
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn measure_non_windows_fail_closed() -> Result<BenchReport, Box<dyn Error>> {
    let harness = ClickHarness::production()?;
    let before = harness.snapshot()?;
    let error = harness.execute(button_down_action()).err();
    let after = harness.snapshot()?;
    let final_snapshot = harness.shutdown()?;
    let code = error
        .as_ref()
        .map_or("<none>", synapse_action::ActionError::code);

    Ok(BenchReport {
        mode: "production",
        edge: "non_windows_software_fails_closed",
        iterations: 1,
        before: format!("snapshot:{before:?}"),
        after: format!("error_code:{code} snapshot:{after:?} final_snapshot:{final_snapshot:?}"),
        p50_down_ns: None,
        p99_down_ns: None,
        max_down_ns: None,
        pass: code == error_codes::ACTION_BACKEND_UNAVAILABLE
            && actor_is_empty(&after)
            && actor_is_empty(&final_snapshot),
        enforces_windows_target: false,
    })
}

#[cfg(windows)]
fn measure_windows_sendinput() -> Result<BenchReport, Box<dyn Error>> {
    let harness = ClickHarness::production()?;
    let before_down = left_button_is_down();
    if before_down {
        return Err(format!(
            "{LEFT_BUTTON_LABEL} is already down before action_software_click bench"
        )
        .into());
    }

    let mut elapsed = Vec::with_capacity(WINDOWS_ITERATIONS);
    let mut observed_down_count = 0_usize;
    let mut after_up_down_count = 0_usize;
    let mut actor_empty_count = 0_usize;
    for _ in 0..WINDOWS_ITERATIONS {
        let readback = harness.click_once_observing_button()?;
        elapsed.push(readback.down_ns);
        if readback.observed_down {
            observed_down_count = observed_down_count.saturating_add(1);
        }
        if readback.after_up_down {
            after_up_down_count = after_up_down_count.saturating_add(1);
        }
        if readback.actor_empty {
            actor_empty_count = actor_empty_count.saturating_add(1);
        }
        std::thread::sleep(RATE_LIMIT_SAFE_PACE);
    }

    let after_down = left_button_is_down();
    let final_snapshot = harness.shutdown()?;
    elapsed.sort_unstable();
    let p99 = percentile(&elapsed, 99);

    Ok(BenchReport {
        mode: "windows_sendinput",
        edge: "left_button_down_ack",
        iterations: WINDOWS_ITERATIONS,
        before: format!("GetAsyncKeyState({LEFT_BUTTON_LABEL}).down:{before_down}"),
        after: format!(
            "observed_down_count:{observed_down_count} after_up_down_count:{after_up_down_count} actor_empty_count:{actor_empty_count} GetAsyncKeyState({LEFT_BUTTON_LABEL}).down:{after_down} final_snapshot:{final_snapshot:?}"
        ),
        p50_down_ns: Some(percentile(&elapsed, 50)),
        p99_down_ns: Some(p99),
        max_down_ns: elapsed.last().copied(),
        pass: !after_down
            && actor_is_empty(&final_snapshot)
            && observed_down_count == WINDOWS_ITERATIONS
            && after_up_down_count == 0
            && actor_empty_count == WINDOWS_ITERATIONS
            && p99 <= u128::from(WINDOWS_TARGET_P99_NS),
        enforces_windows_target: true,
    })
}

#[derive(Debug)]
struct ClickHarness {
    runtime: Runtime,
    cancel: CancellationToken,
    handle: ActionHandle,
    snapshot_handle: ActionEmitterSnapshotHandle,
    join: JoinHandle<ActionStateSnapshot>,
    recording: Option<Arc<RecordingBackend>>,
}

impl ClickHarness {
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

    fn click_once(&self) -> Result<ClickReadback, Box<dyn Error>> {
        let before_event_count = self.recording_event_count();
        let started = Instant::now();
        self.execute(button_down_action())?;
        let down_ns = started.elapsed().as_nanos();
        self.execute(button_up_action())?;
        let new_events = self.recording_events_since(before_event_count);
        let snapshot = self.snapshot()?;

        Ok(ClickReadback {
            down_ns,
            new_event_count: new_events.len(),
            first_event: new_events
                .first()
                .map_or_else(|| "<none>".to_owned(), event_label),
            last_event: new_events
                .last()
                .map_or_else(|| "<none>".to_owned(), event_label),
            actor_empty: actor_is_empty(&snapshot),
        })
    }

    #[cfg(windows)]
    fn click_once_observing_button(&self) -> Result<WindowsClickReadback, Box<dyn Error>> {
        let started = Instant::now();
        self.execute(button_down_action())?;
        let observed_down = wait_for_left_button_state(true, started, WINDOWS_BUTTON_STATE_TIMEOUT);
        let down_ns = started.elapsed().as_nanos();
        self.execute(button_up_action())?;
        let up_started = Instant::now();
        let observed_up =
            wait_for_left_button_state(false, up_started, WINDOWS_BUTTON_STATE_TIMEOUT);
        let after_up_down = !observed_up && left_button_is_down();
        let snapshot = self.snapshot()?;

        Ok(WindowsClickReadback {
            down_ns,
            observed_down,
            after_up_down,
            actor_empty: actor_is_empty(&snapshot),
        })
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
struct ClickReadback {
    down_ns: u128,
    new_event_count: usize,
    first_event: String,
    last_event: String,
    actor_empty: bool,
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsClickReadback {
    down_ns: u128,
    observed_down: bool,
    after_up_down: bool,
    actor_empty: bool,
}

#[derive(Debug)]
struct BenchReport {
    mode: &'static str,
    edge: &'static str,
    iterations: usize,
    before: String,
    after: String,
    p50_down_ns: Option<u128>,
    p99_down_ns: Option<u128>,
    max_down_ns: Option<u128>,
    pass: bool,
    enforces_windows_target: bool,
}

impl BenchReport {
    fn print(&self) {
        println!(
            "readback=action_software_click mode={} edge={} before={} after={} iterations:{} p50_down_ns:{} p99_down_ns:{} max_down_ns:{} target_p99_ns:{} result_value={}",
            self.mode,
            self.edge,
            self.before,
            self.after,
            self.iterations,
            display_opt(self.p50_down_ns),
            display_opt(self.p99_down_ns),
            display_opt(self.max_down_ns),
            if self.enforces_windows_target {
                u128::from(WINDOWS_TARGET_P99_NS).to_string()
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

fn actor_is_empty(snapshot: &ActionStateSnapshot) -> bool {
    snapshot.held_keys.is_empty()
        && snapshot.held_buttons.is_empty()
        && snapshot.pad_state.is_empty()
        && snapshot.held_key_timer_count == 0
}

fn event_label(event: &RecordedInput) -> String {
    format!("{event:?}")
}

const fn button_down_action() -> Action {
    Action::MouseButton {
        button: MouseButton::Left,
        action: ButtonAction::Down,
        hold_ms: 0,
        backend: Backend::Software,
    }
}

const fn button_up_action() -> Action {
    Action::MouseButton {
        button: MouseButton::Left,
        action: ButtonAction::Up,
        hold_ms: 0,
        backend: Backend::Software,
    }
}

fn display_opt(value: Option<u128>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| value.to_string())
}

#[cfg(windows)]
fn real_sendinput_enabled() -> bool {
    std::env::var_os(REAL_SENDINPUT_ENV).is_some_and(|value| value == "1")
}

#[cfg(windows)]
fn left_button_is_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    let state = unsafe { GetAsyncKeyState(LEFT_BUTTON_VK) };
    (u16::from_ne_bytes(state.to_ne_bytes()) & 0x8000) != 0
}

#[cfg(windows)]
fn wait_for_left_button_state(expected_down: bool, started: Instant, timeout: Duration) -> bool {
    loop {
        if left_button_is_down() == expected_down {
            return true;
        }
        if started.elapsed() >= timeout {
            return left_button_is_down() == expected_down;
        }
        std::thread::yield_now();
    }
}

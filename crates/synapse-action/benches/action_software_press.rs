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
use synapse_core::{Action, Backend, Key, KeyCode};
use tokio::{runtime::Runtime, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const BENCH_NAME: &str = "action_software_press";
const PRESS_KEY_NAME: &str = "shift";
const RECORDING_ITERATIONS: usize = 2_000;
#[cfg(windows)]
const WINDOWS_ITERATIONS: usize = 200;
const WINDOWS_TARGET_P99_NS: u64 = 3_000_000;
const RATE_LIMIT_SAFE_PACE: Duration = Duration::from_micros(250);
#[cfg(windows)]
const WINDOWS_KEY_STATE_TIMEOUT: Duration = Duration::from_nanos(WINDOWS_TARGET_P99_NS);
#[cfg(windows)]
const PRESS_KEY_LABEL: &str = "Shift";
#[cfg(windows)]
const PRESS_KEY_VK: i32 = 0x10;
#[cfg(windows)]
const REAL_SENDINPUT_ENV: &str = "SYNAPSE_ACTION_SOFTWARE_PRESS_REAL";

fn main() -> Result<(), Box<dyn Error>> {
    {
        let mut criterion = Criterion::default()
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_secs(1))
            .sample_size(20)
            .configure_from_args();

        bench_action_software_press_recording(&mut criterion);
        #[cfg(windows)]
        if real_sendinput_enabled() {
            bench_action_software_press_sendinput(&mut criterion);
        }
        criterion.final_summary();
    }

    for report in manual_reports()? {
        report.print();
        assert!(
            report.pass,
            "action_software_press {} {} did not pass",
            report.mode, report.edge
        );
        if report.enforces_windows_target {
            let p99 = report
                .p99_keydown_ns
                .ok_or("windows target report missing p99")?;
            assert!(
                p99 <= u128::from(WINDOWS_TARGET_P99_NS),
                "action_software_press windows p99 {p99} ns exceeded {WINDOWS_TARGET_P99_NS} ns"
            );
        }
    }

    Ok(())
}

fn bench_action_software_press_recording(criterion: &mut Criterion) {
    let harness = PressHarness::recording()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} recording harness should start: {err}"));
    let key = bench_key();

    criterion.bench_function(BENCH_NAME, |bencher| {
        bencher.iter_custom(|iterations| {
            let mut total_keydown_ns = 0_u128;
            for _ in 0..iterations {
                let readback = harness
                    .press_once(black_box(&key))
                    .unwrap_or_else(|err| panic!("{BENCH_NAME} recording iteration failed: {err}"));
                total_keydown_ns = total_keydown_ns.saturating_add(readback.keydown_ns);
                black_box(readback.actor_empty);
                std::thread::sleep(RATE_LIMIT_SAFE_PACE);
            }
            duration_from_nanos_saturating(total_keydown_ns)
        });
    });

    harness
        .shutdown()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} recording harness shutdown failed: {err}"));
}

#[cfg(windows)]
fn bench_action_software_press_sendinput(criterion: &mut Criterion) {
    let harness = PressHarness::production()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} SendInput harness should start: {err}"));
    let key = bench_key();

    criterion.bench_function("action_software_press_sendinput", |bencher| {
        bencher.iter_custom(|iterations| {
            let mut total_keydown_ns = 0_u128;
            for _ in 0..iterations {
                let readback = harness
                    .press_once(black_box(&key))
                    .unwrap_or_else(|err| panic!("{BENCH_NAME} SendInput iteration failed: {err}"));
                total_keydown_ns = total_keydown_ns.saturating_add(readback.keydown_ns);
                black_box(readback.actor_empty);
                std::thread::sleep(RATE_LIMIT_SAFE_PACE);
            }
            duration_from_nanos_saturating(total_keydown_ns)
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
    let harness = PressHarness::recording()?;
    let key = bench_key();
    let mut elapsed = Vec::with_capacity(RECORDING_ITERATIONS);
    let mut latest = None;

    for _ in 0..RECORDING_ITERATIONS {
        let readback = harness.press_once(&key)?;
        elapsed.push(readback.keydown_ns);
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
        edge: "keydown_ack_then_keyup_cleanup",
        iterations: RECORDING_ITERATIONS,
        before: "events:0 actor_empty:true".to_owned(),
        after: format!(
            "new_events:{} first_event:{} last_event:{} actor_empty:{}",
            latest.new_event_count, latest.first_event, latest.last_event, latest.actor_empty
        ),
        p50_keydown_ns: Some(percentile(&elapsed, 50)),
        p99_keydown_ns: Some(percentile(&elapsed, 99)),
        max_keydown_ns: elapsed.last().copied(),
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
            after: "skipped_real_input_to_avoid_unrequested_desktop_events".to_owned(),
            p50_keydown_ns: None,
            p99_keydown_ns: None,
            max_keydown_ns: None,
            pass: true,
            enforces_windows_target: false,
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn measure_non_windows_fail_closed() -> Result<BenchReport, Box<dyn Error>> {
    let harness = PressHarness::production()?;
    let key = bench_key();
    let before = harness.snapshot()?;
    let error = harness.execute(key_down_action(&key)).err();
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
        p50_keydown_ns: None,
        p99_keydown_ns: None,
        max_keydown_ns: None,
        pass: code == error_codes::ACTION_BACKEND_UNAVAILABLE
            && actor_is_empty(&after)
            && actor_is_empty(&final_snapshot),
        enforces_windows_target: false,
    })
}

#[cfg(windows)]
fn measure_windows_sendinput() -> Result<BenchReport, Box<dyn Error>> {
    let harness = PressHarness::production()?;
    let key = bench_key();
    let before_down = press_key_is_down();
    if before_down {
        return Err(format!(
            "{PRESS_KEY_LABEL} is already down before action_software_press bench"
        )
        .into());
    }

    let mut elapsed = Vec::with_capacity(WINDOWS_ITERATIONS);
    let mut observed_down_count = 0_usize;
    let mut after_up_down_count = 0_usize;
    let mut actor_empty_count = 0_usize;
    for _ in 0..WINDOWS_ITERATIONS {
        let readback = harness.press_once_observing_key(&key)?;
        elapsed.push(readback.keydown_ns);
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

    let after_down = press_key_is_down();
    let final_snapshot = harness.shutdown()?;
    elapsed.sort_unstable();
    let p99 = percentile(&elapsed, 99);

    Ok(BenchReport {
        mode: "windows_sendinput",
        edge: "shift_keydown_ack",
        iterations: WINDOWS_ITERATIONS,
        before: format!("GetAsyncKeyState({PRESS_KEY_LABEL}).down:{before_down}"),
        after: format!(
            "observed_down_count:{observed_down_count} after_up_down_count:{after_up_down_count} actor_empty_count:{actor_empty_count} GetAsyncKeyState({PRESS_KEY_LABEL}).down:{after_down} final_snapshot:{final_snapshot:?}"
        ),
        p50_keydown_ns: Some(percentile(&elapsed, 50)),
        p99_keydown_ns: Some(p99),
        max_keydown_ns: elapsed.last().copied(),
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
struct PressHarness {
    runtime: Runtime,
    cancel: CancellationToken,
    handle: ActionHandle,
    snapshot_handle: ActionEmitterSnapshotHandle,
    join: JoinHandle<ActionStateSnapshot>,
    recording: Option<Arc<RecordingBackend>>,
}

impl PressHarness {
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

    fn press_once(&self, key: &Key) -> Result<PressReadback, Box<dyn Error>> {
        let before_event_count = self.recording_event_count();
        let started = Instant::now();
        self.execute(key_down_action(key))?;
        let keydown_ns = started.elapsed().as_nanos();
        self.execute(key_up_action(key))?;
        let new_events = self.recording_events_since(before_event_count);
        let snapshot = self.snapshot()?;

        Ok(PressReadback {
            keydown_ns,
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
    fn press_once_observing_key(&self, key: &Key) -> Result<WindowsPressReadback, Box<dyn Error>> {
        let started = Instant::now();
        self.execute(key_down_action(key))?;
        let observed_down = wait_for_press_key_state(true, started, WINDOWS_KEY_STATE_TIMEOUT);
        let keydown_ns = started.elapsed().as_nanos();
        self.execute(key_up_action(key))?;
        let keyup_started = Instant::now();
        let observed_up = wait_for_press_key_state(false, keyup_started, WINDOWS_KEY_STATE_TIMEOUT);
        let after_up_down = !observed_up && press_key_is_down();
        let snapshot = self.snapshot()?;

        Ok(WindowsPressReadback {
            keydown_ns,
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
struct PressReadback {
    keydown_ns: u128,
    new_event_count: usize,
    first_event: String,
    last_event: String,
    actor_empty: bool,
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsPressReadback {
    keydown_ns: u128,
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
    p50_keydown_ns: Option<u128>,
    p99_keydown_ns: Option<u128>,
    max_keydown_ns: Option<u128>,
    pass: bool,
    enforces_windows_target: bool,
}

impl BenchReport {
    fn print(&self) {
        println!(
            "readback=action_software_press mode={} edge={} before={} after={} iterations:{} p50_keydown_ns:{} p99_keydown_ns:{} max_keydown_ns:{} target_p99_ns:{} result_value={}",
            self.mode,
            self.edge,
            self.before,
            self.after,
            self.iterations,
            display_opt(self.p50_keydown_ns),
            display_opt(self.p99_keydown_ns),
            display_opt(self.max_keydown_ns),
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

fn key_down_action(key: &Key) -> Action {
    Action::KeyDown {
        key: key.clone(),
        backend: Backend::Software,
    }
}

fn key_up_action(key: &Key) -> Action {
    Action::KeyUp {
        key: key.clone(),
        backend: Backend::Software,
    }
}

fn bench_key() -> Key {
    Key {
        code: KeyCode::Named {
            value: PRESS_KEY_NAME.to_owned(),
        },
        use_scancode: false,
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
fn press_key_is_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    let state = unsafe { GetAsyncKeyState(PRESS_KEY_VK) };
    (u16::from_ne_bytes(state.to_ne_bytes()) & 0x8000) != 0
}

#[cfg(windows)]
fn wait_for_press_key_state(expected_down: bool, started: Instant, timeout: Duration) -> bool {
    loop {
        if press_key_is_down() == expected_down {
            return true;
        }
        if started.elapsed() >= timeout {
            return press_key_is_down() == expected_down;
        }
        std::thread::yield_now();
    }
}

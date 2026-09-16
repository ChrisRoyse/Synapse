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
use synapse_core::{
    Action, Backend, ButtonAction, GamepadReport, Key, KeyCode, KeystrokeDynamics, MouseButton,
    PadButton,
};
use tokio::{runtime::Runtime, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const BENCH_NAME: &str = "action_recording_round_trip";
const DEFAULT_ITERATIONS: usize = 2_000;

fn main() -> Result<(), Box<dyn Error>> {
    {
        let mut criterion = Criterion::default()
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_secs(1))
            .sample_size(20)
            .configure_from_args();

        bench_action_recording_round_trip(&mut criterion);
        criterion.final_summary();
    }

    for report in manual_reports()? {
        report.print();
        assert!(
            report.actor_empty,
            "action_recording_round_trip {} actor held state was not empty",
            report.edge
        );
        assert_eq!(
            report.new_event_count, report.expected_new_event_count,
            "action_recording_round_trip {} event count drifted",
            report.edge
        );
    }

    Ok(())
}

fn bench_action_recording_round_trip(criterion: &mut Criterion) {
    let harness = RoundTripHarness::new()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} harness should start: {err}"));
    let action = key_up_action("a");

    criterion.bench_function(BENCH_NAME, |bencher| {
        bencher.iter(|| {
            let readback = harness
                .round_trip(black_box(&action), &[])
                .unwrap_or_else(|err| panic!("{BENCH_NAME} iteration failed: {err}"));
            black_box(readback.new_event_count);
            black_box(readback.actor_empty);
        });
    });

    harness
        .shutdown()
        .unwrap_or_else(|err| panic!("{BENCH_NAME} harness shutdown failed: {err}"));
}

fn manual_reports() -> Result<Vec<BenchReport>, Box<dyn Error>> {
    let cases = [
        BenchCase {
            edge: "key_up_unheld",
            action: key_up_action("a"),
            setup: Vec::new(),
            iterations: DEFAULT_ITERATIONS,
            expected_new_event_count: 1,
        },
        BenchCase {
            edge: "empty_release_all",
            action: Action::ReleaseAll,
            setup: Vec::new(),
            iterations: DEFAULT_ITERATIONS,
            expected_new_event_count: 1,
        },
        BenchCase {
            edge: "preheld_release_all",
            action: Action::ReleaseAll,
            setup: vec![
                key_down_action("shift"),
                Action::MouseButton {
                    button: MouseButton::Left,
                    action: ButtonAction::Down,
                    hold_ms: 0,
                    backend: Backend::Software,
                },
                Action::PadReport {
                    pad: 2,
                    report: pad_report(vec![PadButton::A]),
                },
            ],
            iterations: 20,
            expected_new_event_count: 1,
        },
        BenchCase {
            edge: "type_text_burst",
            action: Action::TypeText {
                text: "Synapse".to_owned(),
                dynamics: KeystrokeDynamics::Burst,
                backend: Backend::Software,
            },
            setup: Vec::new(),
            iterations: 50,
            expected_new_event_count: 16,
        },
    ];

    cases.iter().map(measure_case).collect()
}

#[derive(Clone, Debug)]
struct BenchCase {
    edge: &'static str,
    action: Action,
    setup: Vec<Action>,
    iterations: usize,
    expected_new_event_count: usize,
}

#[derive(Debug)]
struct RoundTripHarness {
    runtime: Runtime,
    cancel: CancellationToken,
    handle: ActionHandle,
    snapshot_handle: ActionEmitterSnapshotHandle,
    join: JoinHandle<ActionStateSnapshot>,
    recording: Arc<RecordingBackend>,
}

impl RoundTripHarness {
    fn new() -> Result<Self, Box<dyn Error>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
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
            recording,
        })
    }

    fn round_trip(
        &self,
        action: &Action,
        setup: &[Action],
    ) -> Result<RoundTripReadback, Box<dyn Error>> {
        for setup_action in setup {
            self.runtime
                .block_on(self.handle.execute(setup_action.clone()))?;
        }
        let before_event_count = self.recording.event_count();

        self.runtime.block_on(self.handle.execute(action.clone()))?;
        let events = self.recording.events_since(before_event_count);
        let snapshot = self.runtime.block_on(self.snapshot_handle.snapshot())?;

        Ok(RoundTripReadback::from_events(&events, &snapshot))
    }

    fn shutdown(self) -> Result<ActionStateSnapshot, Box<dyn Error>> {
        self.cancel.cancel();
        Ok(self.runtime.block_on(self.join)?)
    }
}

#[derive(Debug)]
struct RoundTripReadback {
    new_event_count: usize,
    first_event: String,
    last_event: String,
    actor_empty: bool,
}

impl RoundTripReadback {
    fn from_events(new_events: &[RecordedInput], snapshot: &ActionStateSnapshot) -> Self {
        Self {
            new_event_count: new_events.len(),
            first_event: new_events
                .first()
                .map_or_else(|| "<none>".to_owned(), event_label),
            last_event: new_events
                .last()
                .map_or_else(|| "<none>".to_owned(), event_label),
            actor_empty: actor_is_empty(snapshot),
        }
    }
}

#[derive(Debug)]
struct BenchReport {
    edge: &'static str,
    iterations: usize,
    expected_new_event_count: usize,
    new_event_count: usize,
    first_event: String,
    last_event: String,
    p50_ns: u128,
    p99_ns: u128,
    max_ns: u128,
    actor_empty: bool,
}

impl BenchReport {
    fn print(&self) {
        println!(
            "readback=action_recording_round_trip edge={} before=iterations:{} expected_new_events:{} after=new_events:{} first_event:{} last_event:{} actor_empty:{} p50_ns:{} p99_ns:{} max_ns:{} result_value=pass",
            self.edge,
            self.iterations,
            self.expected_new_event_count,
            self.new_event_count,
            self.first_event,
            self.last_event,
            self.actor_empty,
            self.p50_ns,
            self.p99_ns,
            self.max_ns
        );
    }
}

fn measure_case(case: &BenchCase) -> Result<BenchReport, Box<dyn Error>> {
    let harness = RoundTripHarness::new()?;
    let mut elapsed = Vec::with_capacity(case.iterations);
    let mut latest = None;

    for _ in 0..case.iterations {
        let started = Instant::now();
        let readback = harness.round_trip(&case.action, &case.setup)?;
        elapsed.push(started.elapsed().as_nanos());
        latest = Some(readback);
    }

    let shutdown_snapshot = harness.shutdown()?;
    assert!(
        actor_is_empty(&shutdown_snapshot),
        "actor final snapshot was not empty for {}",
        case.edge
    );
    elapsed.sort_unstable();
    let latest = latest.ok_or("manual bench case had zero iterations")?;

    Ok(BenchReport {
        edge: case.edge,
        iterations: case.iterations,
        expected_new_event_count: case.expected_new_event_count,
        new_event_count: latest.new_event_count,
        first_event: latest.first_event,
        last_event: latest.last_event,
        p50_ns: percentile(&elapsed, 50),
        p99_ns: percentile(&elapsed, 99),
        max_ns: elapsed.last().copied().unwrap_or_default(),
        actor_empty: latest.actor_empty,
    })
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

fn key_up_action(value: &str) -> Action {
    Action::KeyUp {
        key: key(value),
        backend: Backend::Software,
    }
}

fn key_down_action(value: &str) -> Action {
    Action::KeyDown {
        key: key(value),
        backend: Backend::Software,
    }
}

fn key(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

const fn pad_report(buttons: Vec<PadButton>) -> GamepadReport {
    GamepadReport {
        controller: synapse_core::GamepadController::X360,
        buttons,
        thumb_l: (0.5, -0.5),
        thumb_r: (0.0, 0.0),
        lt: 0.0,
        rt: 0.25,
    }
}

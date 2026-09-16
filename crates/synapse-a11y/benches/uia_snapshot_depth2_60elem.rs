use criterion::Criterion;

fn main() {
    if std::env::var_os("SYNAPSE_A11Y_MANUAL_BENCH").is_some() {
        run_manual_bench();
        return;
    }

    let mut criterion = Criterion::default().configure_from_args();
    bench_uia_snapshot_depth2_60elem(&mut criterion);
    criterion.final_summary();
}

fn bench_uia_snapshot_depth2_60elem(c: &mut Criterion) {
    #[cfg(windows)]
    {
        use std::hint::black_box;

        if synapse_a11y::snapshot_focused_window(0).is_ok() {
            c.bench_function("uia_snapshot_depth2_60elem", |bencher| {
                bencher.iter(|| black_box(synapse_a11y::snapshot_focused_window(2)));
            });
        }
    }

    #[cfg(not(windows))]
    {
        use synapse_a11y::{AccessibleEvent, AccessibleEventKind, coalesce_events};
        let events: Vec<_> = (0..60)
            .map(|index| AccessibleEvent {
                seq: index,
                at_ms: index,
                window_id: 0x1234,
                element_id: Some(synapse_core::element_id(0x1234, "00000001")),
                kind: AccessibleEventKind::NameChanged,
                name: None,
                value: None,
            })
            .collect();
        c.bench_function(
            "uia_snapshot_depth2_60elem_non_windows_compile_guard",
            |bencher| {
                bencher
                    .iter(|| coalesce_events(events.clone(), std::time::Duration::from_millis(50)));
            },
        );
    }
}

fn run_manual_bench() {
    #[cfg(windows)]
    {
        use std::{hint::black_box, time::Instant};

        let iterations = std::env::var("SYNAPSE_A11Y_BENCH_ITERS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(300);
        if synapse_a11y::snapshot_focused_window(0).is_err() {
            println!("readback=a11y_snapshot_bench after=status:no_foreground");
            std::process::exit(2);
        }

        for _ in 0..10 {
            let _ = black_box(synapse_a11y::snapshot_focused_window(2));
        }

        let mut samples = Vec::with_capacity(iterations);
        let mut last_nodes = 0_usize;
        for _ in 0..iterations {
            let start = Instant::now();
            match black_box(synapse_a11y::snapshot_focused_window(2)) {
                Ok(tree) => {
                    last_nodes = tree.nodes.len();
                    samples.push(start.elapsed());
                }
                Err(err) => {
                    println!(
                        "readback=a11y_snapshot_bench after=status:error code:{} detail:{err}",
                        err.code()
                    );
                    std::process::exit(3);
                }
            }
        }
        samples.sort_unstable();
        let p99_index = ((samples.len().saturating_sub(1)) * 99) / 100;
        let p99_ms = samples[p99_index].as_secs_f64() * 1000.0;
        let max_ms = samples
            .last()
            .map_or(0.0, |sample| sample.as_secs_f64() * 1000.0);
        println!(
            "readback=a11y_snapshot_bench after=status:ok iterations:{iterations} nodes:{last_nodes} p99_ms:{p99_ms:.3} max_ms:{max_ms:.3}"
        );
        if p99_ms > 10.0 {
            std::process::exit(4);
        }
    }

    #[cfg(not(windows))]
    {
        println!("readback=a11y_snapshot_bench after=status:unsupported");
    }
}

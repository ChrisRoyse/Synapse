use std::time::Duration;

use synapse_telemetry::{TelemetryConfig, init_tracing};
use tempfile::TempDir;

/// Periodic GC `state`: synthesise a too-old file in the log dir, init telemetry with
/// `keep_days=0` and a very short `gc_interval`, wait one tick + slack, assert the
/// file is gone. Proves the background worker actually re-runs `run_log_gc`
/// mid-uptime (the previous implementation only ran GC at startup, so a daemon
/// that lived past `keep_days` would silently retain files past the cap).
///
/// This test owns its own integration-test binary because `try_init` installs a
/// process-wide tracing subscriber and cannot be re-init'd in the same process.
#[test]
fn periodic_gc_evicts_files_past_keep_days() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let stale = dir.path().join("synapse.log.1970-01-01");
    std::fs::write(&stale, b"ancient log line\n")?;
    assert!(stale.exists(), "fixture: stale file should exist pre-init");
    println!(
        "regression_state=periodic_gc edge=startup before=stale_exists:{} path={}",
        stale.exists(),
        stale.display()
    );

    let cfg = TelemetryConfig {
        log_dir: Some(dir.path().to_path_buf()),
        keep_days: 0,
        max_dir_bytes: 1024 * 1024,
        gc_interval: Some(Duration::from_millis(75)),
        ..TelemetryConfig::default()
    };
    let guard = init_tracing(cfg)?;

    // `run_log_gc` also runs at init and evicts the first stale file immediately,
    // so we synthesise a SECOND stale file AFTER init to prove the interval-driven
    // loop fires at least once during the daemon's uptime.
    let stale_after_init = dir.path().join("synapse.log.1970-01-02");
    std::fs::write(&stale_after_init, b"second ancient log line\n")?;
    let pre_periodic = stale_after_init.exists();
    println!(
        "regression_state=periodic_gc edge=mid_run before=stale_after_init_exists:{} path={}",
        pre_periodic,
        stale_after_init.display()
    );

    std::thread::sleep(Duration::from_millis(400));

    let post_periodic = stale_after_init.exists();
    println!(
        "regression_state=periodic_gc edge=mid_run after=stale_after_init_exists:{post_periodic}"
    );

    drop(guard);
    assert!(
        pre_periodic,
        "fixture: stale_after_init should have existed before the periodic tick"
    );
    assert!(
        !post_periodic,
        "periodic GC did not evict stale_after_init within the tick window"
    );
    Ok(())
}

use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use synapse_telemetry::{TelemetryConfig, init_tracing};
use tempfile::TempDir;

/// Periodic GC size-cap `state`: start telemetry with an intentionally tiny
/// `max_dir_bytes`, create oversized log files *after* init, then poll the log
/// directory metadata until the background GC worker trims the directory below
/// the cap.
#[test]
fn periodic_gc_enforces_max_dir_bytes_after_startup() -> Result<(), Box<dyn std::error::Error>> {
    const MAX_DIR_BYTES: u64 = 64;

    let dir = TempDir::new()?;
    let cfg = TelemetryConfig {
        log_dir: Some(dir.path().to_path_buf()),
        keep_days: 365,
        max_dir_bytes: MAX_DIR_BYTES,
        gc_interval: Some(Duration::from_millis(75)),
        ..TelemetryConfig::default()
    };
    let guard = init_tracing(cfg)?;

    for idx in 0..3 {
        let path = dir.path().join(format!("synapse.log.synthetic-{idx}"));
        fs::write(path, vec![b'x'; 40])?;
        std::thread::sleep(Duration::from_millis(10));
    }

    let before = log_dir_state(dir.path())?;
    println!(
        "regression_state=log_dir_size edge=max_cap before_bytes:{} before_files:{:?} cap_bytes:{}",
        before.total_bytes, before.files, MAX_DIR_BYTES
    );
    assert!(
        before.total_bytes > MAX_DIR_BYTES,
        "fixture must start over cap"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut samples = Vec::new();
    let final_state = loop {
        let state = log_dir_state(dir.path())?;
        samples.push(state.total_bytes);
        if state.total_bytes <= MAX_DIR_BYTES || Instant::now() >= deadline {
            break state;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    println!(
        "regression_state=log_dir_size edge=max_cap after_samples:{samples:?} after_bytes:{} after_files:{:?} observed:{}",
        final_state.total_bytes, final_state.files, final_state.total_bytes
    );

    drop(guard);
    assert!(
        final_state.total_bytes <= MAX_DIR_BYTES,
        "periodic GC did not trim log dir below cap"
    );
    Ok(())
}

#[derive(Debug)]
struct LogDirState {
    total_bytes: u64,
    files: Vec<String>,
}

fn log_dir_state(path: &Path) -> Result<LogDirState, Box<dyn std::error::Error>> {
    let mut total_bytes = 0;
    let mut files = Vec::new();

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total_bytes += metadata.len();
            files.push(format!(
                "{}:{}",
                entry.file_name().to_string_lossy(),
                metadata.len()
            ));
        }
    }

    files.sort();
    Ok(LogDirState { total_bytes, files })
}

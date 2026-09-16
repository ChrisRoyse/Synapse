use synapse_telemetry::{TelemetryConfig, TelemetryError, init_tracing};
use tempfile::TempDir;
use tracing::{error, info};

#[test]
fn synthetic_emit_lands_in_jsonl_file() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let cfg = TelemetryConfig::default_with_log_dir(dir.path().to_path_buf());

    let guard = init_tracing(cfg)?;

    info!(field_a = "v_a", field_b = 42, "happy_path_event");
    error!(
        field_c = "oops",
        error.kind = "TELEMETRY_GC_FAILED",
        "edge_case_event"
    );
    info!("plain_message_event");

    drop(guard);

    let logs = read_log_dir(dir.path())?;
    let values = logs
        .lines()
        .filter(|line| !line.is_empty())
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let lines = values
        .iter()
        .filter(|value| {
            matches!(
                value["fields"]["message"].as_str(),
                Some("happy_path_event" | "edge_case_event" | "plain_message_event")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        3,
        "expected 3 synthetic events, got {}",
        lines.len()
    );

    let first = lines[0];
    assert_eq!(first["fields"]["field_a"], "v_a");
    assert_eq!(first["fields"]["field_b"], 42);
    assert_eq!(first["fields"]["message"], "happy_path_event");
    assert_eq!(first["level"], "INFO");

    let second = lines[1];
    assert_eq!(second["fields"]["field_c"], "oops");
    assert_eq!(second["fields"]["error.kind"], "TELEMETRY_GC_FAILED");
    assert_eq!(second["level"], "ERROR");

    let third = lines[2];
    assert_eq!(third["fields"]["message"], "plain_message_event");
    Ok(())
}

#[test]
fn synthetic_file_path_log_dir_returns_error() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let file_path = dir.path().join("not-a-directory");
    std::fs::write(&file_path, b"not a directory")?;

    let cfg = TelemetryConfig::default_with_log_dir(file_path);
    let res = init_tracing(cfg);
    assert!(matches!(res, Err(TelemetryError::LogDirNotWritable(_, _))));
    Ok(())
}

fn read_log_dir(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut contents = String::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.metadata()?.is_file() {
            contents.push_str(&std::fs::read_to_string(entry.path())?);
        }
    }
    assert!(!contents.is_empty(), "no log file content produced");
    Ok(contents)
}

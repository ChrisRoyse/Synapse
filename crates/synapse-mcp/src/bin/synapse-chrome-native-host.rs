#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, bail};
use synapse_telemetry::{TelemetryConfig, TelemetryGuard, init_tracing};
use tracing_subscriber::filter::LevelFilter;

#[allow(dead_code)]
#[path = "../chrome_debugger_bridge/mod.rs"]
mod chrome_debugger_bridge;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            let _ = record_startup_error(&format!("{error:#}"));
            ExitCode::from(1)
        }
    }
}

async fn run() -> anyhow::Result<ExitCode> {
    let invocation =
        chrome_debugger_bridge::native_host_invocation_from_args(std::env::args_os().skip(1))
            .context(
                "SYNAPSE_CHROME_NATIVE_HOST_INVOCATION_INVALID expected chrome-extension origin",
            )?;
    let bind = std::env::var("SYNAPSE_BIND").unwrap_or_else(|_| "127.0.0.1:7700".to_owned());
    let telemetry_guard = configure_telemetry_from_level(
        &std::env::var("SYNAPSE_LOG_LEVEL").unwrap_or_else(|_| "info".to_owned()),
    )?;
    let result = chrome_debugger_bridge::run_native_host(&bind, invocation).await;
    drop(telemetry_guard);
    result
}

fn configure_telemetry_from_level(log_level: &str) -> anyhow::Result<TelemetryGuard> {
    let level = log_level
        .parse::<LevelFilter>()
        .with_context(|| format!("invalid log level {log_level}"))?;
    let log_dir = std::env::var_os("SYNAPSE_LOG_DIR").map(PathBuf::from);
    init_tracing(TelemetryConfig {
        log_dir,
        file_level: level,
        console_level: LevelFilter::OFF,
        ..TelemetryConfig::default()
    })
    .context("initialize telemetry")
}

fn record_startup_error(detail: &str) -> anyhow::Result<()> {
    let appdata = std::env::var_os("APPDATA").context("APPDATA is unset")?;
    let log_dir = PathBuf::from(appdata)
        .join("synapse")
        .join("chrome-debugger");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("create native-host log dir {}", log_dir.display()))?;
    let log_path = log_dir.join("native-host-startup-error.log");
    if detail.trim().is_empty() {
        bail!("empty startup error detail")
    }
    std::fs::write(&log_path, detail)
        .with_context(|| format!("write native-host startup error {}", log_path.display()))
}

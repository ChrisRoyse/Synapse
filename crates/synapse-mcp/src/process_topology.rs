//! Daemon scheduling for the bounded periodic process-topology observation
//! (#2097).
//!
//! The observation itself — what is read, what a row claims, row identity, the
//! refresh/dedup rule and the bounds — lives in
//! [`synapse_reflex::process_topology`], next to the `CF_PROCESS_HISTORY` writer
//! it feeds. This module owns only the parts
//! that need the daemon: the tick schedule, the blocking-pool hop, the
//! reflex-runtime lock discipline, and shutdown.
//!
//! Why the daemon needs this at all: `CF_PROCESS_HISTORY` had exactly one
//! writer, `act_launch`, so every row described a process the daemon had itself
//! created and the `syn-graphpos-process-v1` lane could derive nothing but a
//! star centred on the daemon's own pid. This is the second writer.

use std::{sync::Arc, time::Duration};

use synapse_reflex::process_topology::{
    MAX_ROWS_PER_TICK, ProcessTopologyObserver, ProcessTopologyTickReport, ROW_REFRESH_INTERVAL_MS,
};

use crate::server::SynapseService;

pub const PROCESS_TOPOLOGY_INTERVAL_ENV: &str = "SYNAPSE_PROCESS_TOPOLOGY_INTERVAL_SECS";
pub const PROCESS_TOPOLOGY_STARTUP_DELAY_ENV: &str = "SYNAPSE_PROCESS_TOPOLOGY_STARTUP_DELAY_SECS";

/// The bounded default cadence manually exercised by #2097. The #2130 solver
/// repair makes the real forest publishable, so a normal daemon now records the
/// host topology needed by `syn-graphpos-process-v1` without an opt-in override.
/// Setting [`PROCESS_TOPOLOGY_INTERVAL_ENV`] to zero remains an explicit local
/// disable for diagnosis.
pub const DEFAULT_PROCESS_TOPOLOGY_INTERVAL_SECS: u64 = 120;
pub const DEFAULT_PROCESS_TOPOLOGY_STARTUP_DELAY_SECS: u64 = 20;

/// Run one observation tick against the daemon's reflex runtime.
///
/// The runtime mutex is acquired **per chunk** rather than held for the whole
/// census: a full process table can be hundreds of rows and each row costs a
/// Calyx constellation measurement, so holding the lock across the batch would
/// make an observation tick a source of daemon-wide congestion.
fn run_tick(
    observer: &ProcessTopologyObserver,
    service: &SynapseService,
) -> Result<ProcessTopologyTickReport, String> {
    let runtime = service
        .reflex_runtime()
        .map_err(|error| format!("reflex runtime unavailable for process topology: {error}"))?;
    observer.run_once(|batch| {
        let guard = runtime.lock().map_err(|_error| {
            "reflex runtime lock poisoned while writing process topology rows".to_owned()
        })?;
        guard
            .storage_put_process_history_rows(batch)
            .map_err(|error| error.to_string())
    })
}

/// Start the bounded periodic process-topology observation.
///
/// Returns `Ok(None)` when the interval is configured to zero, which disables
/// the observer explicitly and loudly rather than silently.
///
/// # Errors
///
/// Returns an error when the interval or startup-delay environment overrides are
/// not unsigned integers of seconds.
pub(crate) fn spawn_periodic_process_topology_observer(
    service: SynapseService,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let interval_secs = parse_secs_env(
        PROCESS_TOPOLOGY_INTERVAL_ENV,
        DEFAULT_PROCESS_TOPOLOGY_INTERVAL_SECS,
    )?;
    let startup_delay_secs = parse_secs_env(
        PROCESS_TOPOLOGY_STARTUP_DELAY_ENV,
        DEFAULT_PROCESS_TOPOLOGY_STARTUP_DELAY_SECS,
    )?;
    if interval_secs == 0 {
        tracing::info!(
            code = "PROCESS_TOPOLOGY_PERIODIC_DISABLED",
            enable_with = PROCESS_TOPOLOGY_INTERVAL_ENV,
            "periodic process topology observation was explicitly disabled, so \
             CF_PROCESS_HISTORY has only its act_launch writer and the process graph lane can \
             derive nothing but a star centred on this daemon's pid"
        );
        return Ok(None);
    }
    tracing::info!(
        code = "PROCESS_TOPOLOGY_PERIODIC_SCHEDULED",
        interval_secs,
        startup_delay_secs,
        max_rows_per_tick = MAX_ROWS_PER_TICK,
        row_refresh_interval_ms = ROW_REFRESH_INTERVAL_MS,
        "periodic process topology observation scheduled"
    );
    let observer = Arc::new(ProcessTopologyObserver::new());
    let handle = tokio::spawn(async move {
        let mut delay = Duration::from_secs(startup_delay_secs);
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!(
                        code = "PROCESS_TOPOLOGY_PERIODIC_STOPPED",
                        "periodic process topology observation stopped by daemon shutdown"
                    );
                    return;
                }
                () = tokio::time::sleep(delay) => {}
            }
            // The snapshot walk plus per-row Calyx measurement is synchronous
            // and can take real time; it does not belong on an async worker.
            let observer = Arc::clone(&observer);
            let service = service.clone();
            let outcome = tokio::task::spawn_blocking(move || run_tick(&observer, &service)).await;
            report_tick(outcome);
            delay = Duration::from_secs(interval_secs);
        }
    });
    Ok(Some(handle))
}

fn report_tick(outcome: Result<Result<ProcessTopologyTickReport, String>, tokio::task::JoinError>) {
    match outcome {
        Ok(Ok(report)) => tracing::info!(
            code = "PROCESS_TOPOLOGY_PERIODIC_OK",
            processes_observed = report.processes_observed,
            edge_bearing = report.edge_bearing,
            rows_written = report.rows_written,
            rows_skipped_fresh = report.rows_skipped_fresh,
            rows_deferred_over_bound = report.rows_deferred_over_bound,
            identities_retired = report.identities_retired,
            "process topology observation tick completed"
        ),
        Ok(Err(detail)) => tracing::error!(
            code = "PROCESS_TOPOLOGY_PERIODIC_FAILED",
            detail = %detail,
            "process topology observation tick failed; the next tick keeps the schedule and the \
             unwritten identities are retried"
        ),
        Err(error) => tracing::error!(
            code = "PROCESS_TOPOLOGY_PERIODIC_FAILED",
            detail = %error,
            "process topology observation tick could not be joined"
        ),
    }
}

fn parse_secs_env(name: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => anyhow::bail!("{name} is not valid unicode: {error}"),
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(default)
            } else {
                trimmed.parse::<u64>().map_err(|error| {
                    anyhow::anyhow!(
                        "{name} must be an unsigned integer of seconds; got {value:?}: {error}"
                    )
                })
            }
        }
    }
}

//! Periodic in-daemon routine mining batch job (#848).
//!
//! Re-mines `CF_ROUTINES` from `CF_EPISODES` on a fixed interval so the
//! routine store tracks the operator's evolving behavior without anyone
//! calling `routine_mine` by hand. The job shares
//! [`super::routines::mine_and_store_routines`] (and its process-wide
//! mining lock) with the MCP tool, so the two paths can never interleave a
//! replace-all.
//!
//! Configuration (read once at daemon startup; invalid values are a startup
//! error, not a silent default):
//!
//! - `SYNAPSE_ROUTINE_MINE_INTERVAL_SECS` — seconds between runs
//!   (default 21600 = 6 h; `0` disables the job).
//! - `SYNAPSE_ROUTINE_MINE_STARTUP_DELAY_SECS` — delay before the first run
//!   (default 300 s, so startup cost never lands on the recorder's warmup).
//!
//! A failed run is logged loudly (`ROUTINE_MINE_PERIODIC_FAILED`) and the
//! job keeps its schedule — one bad run must not stop future mining — but
//! the failure is never swallowed: the structured log carries the exact
//! error for diagnosis.

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use super::M3State;
use super::routines::{RoutineMineParams, mine_and_store_routines};

/// Environment variable: seconds between periodic mining runs.
pub const INTERVAL_ENV: &str = "SYNAPSE_ROUTINE_MINE_INTERVAL_SECS";
/// Environment variable: seconds before the first run.
pub const STARTUP_DELAY_ENV: &str = "SYNAPSE_ROUTINE_MINE_STARTUP_DELAY_SECS";
/// Default interval: 6 hours.
pub const DEFAULT_INTERVAL_SECS: u64 = 21_600;
/// Default startup delay: 5 minutes.
pub const DEFAULT_STARTUP_DELAY_SECS: u64 = 300;

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

/// Spawns the periodic miner. Returns `Ok(None)` when disabled by
/// configuration (interval `0`); the decision is logged either way.
///
/// # Errors
///
/// Returns an error when an environment override is present but
/// unparseable — a misconfigured daemon must fail at startup, not run with
/// a silently substituted schedule.
pub fn spawn_periodic_miner(
    m3_state: Arc<Mutex<M3State>>,
    cancel: CancellationToken,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let interval_secs = parse_secs_env(INTERVAL_ENV, DEFAULT_INTERVAL_SECS)?;
    let startup_delay_secs = parse_secs_env(STARTUP_DELAY_ENV, DEFAULT_STARTUP_DELAY_SECS)?;
    if interval_secs == 0 {
        tracing::info!(
            code = "ROUTINE_MINE_PERIODIC_DISABLED",
            "periodic routine mining disabled via {INTERVAL_ENV}=0"
        );
        return Ok(None);
    }
    tracing::info!(
        code = "ROUTINE_MINE_PERIODIC_SCHEDULED",
        interval_secs,
        startup_delay_secs,
        "periodic routine mining scheduled"
    );
    let handle = tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(startup_delay_secs);
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!(
                        code = "ROUTINE_MINE_PERIODIC_STOPPED",
                        "periodic routine mining stopped by daemon shutdown"
                    );
                    return;
                }
                () = tokio::time::sleep(delay) => {}
            }
            run_once(&m3_state);
            delay = std::time::Duration::from_secs(interval_secs);
        }
    });
    Ok(Some(handle))
}

/// One periodic mining run: full default-window mine over the live store.
fn run_once(m3_state: &Arc<Mutex<M3State>>) {
    let db = {
        let mut state = match m3_state.lock() {
            Ok(state) => state,
            Err(_poisoned) => {
                tracing::error!(
                    code = "ROUTINE_MINE_PERIODIC_FAILED",
                    detail = "m3 state lock poisoned",
                    "periodic routine mining could not access storage"
                );
                return;
            }
        };
        match state.ensure_storage() {
            Ok(db) => db,
            Err(error) => {
                tracing::error!(
                    code = "ROUTINE_MINE_PERIODIC_FAILED",
                    detail = %error,
                    "periodic routine mining could not open storage"
                );
                return;
            }
        }
    };
    match mine_and_store_routines(&db, &RoutineMineParams::default()) {
        Ok(response) => {
            tracing::info!(
                code = "ROUTINE_MINE_PERIODIC_OK",
                routines_written = response.routines_written,
                routines_deleted = response.routines_deleted,
                active_days = response.active_days,
                eligible_episodes = response.eligible_episodes,
                candidates = response.candidates_evaluated,
                "periodic routine mining completed"
            );
        }
        Err(error) => {
            tracing::error!(
                code = "ROUTINE_MINE_PERIODIC_FAILED",
                error_code = %error.code.0,
                detail = %error.message,
                "periodic routine mining failed; next run keeps the schedule"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_secs_env_accepts_default_and_rejects_garbage() {
        // SAFETY: test-local env mutation; no other test reads this name.
        unsafe { std::env::remove_var("SYNAPSE_TEST_MINE_SECS") };
        assert_eq!(
            parse_secs_env("SYNAPSE_TEST_MINE_SECS", 7).expect("absent -> default"),
            7
        );
        unsafe { std::env::set_var("SYNAPSE_TEST_MINE_SECS", "120") };
        assert_eq!(
            parse_secs_env("SYNAPSE_TEST_MINE_SECS", 7).expect("number parses"),
            120
        );
        unsafe { std::env::set_var("SYNAPSE_TEST_MINE_SECS", "six hours") };
        let error = parse_secs_env("SYNAPSE_TEST_MINE_SECS", 7).expect_err("garbage rejected");
        assert!(
            error.to_string().contains("SYNAPSE_TEST_MINE_SECS"),
            "{error}"
        );
        unsafe { std::env::remove_var("SYNAPSE_TEST_MINE_SECS") };
    }
}

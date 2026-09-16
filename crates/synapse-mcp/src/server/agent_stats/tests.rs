//! Regression tests for `agent_stats` (#903).
//!
//! Integration tests write real rows to a real RocksDB temp instance through
//! the journal write path, then read them back through `collect_anchors` and
//! compare every figure to a hand-computed expected value — no mocks, no
//! synthetic return values. Manual Full State Verification is performed
//! separately against the live daemon and physical storage.

use serde_json::json;
use synapse_core::{AgentEndState, AgentEventKind, AgentEventRecord};
use synapse_storage::Db;

use super::super::agent_events::record_agent_events_unobserved;
use super::*;

/// 1000 seconds, in ns — a base timestamp clear of zero so `validate()` and
/// the TTL filter are both happy.
const BASE_NS: u64 = 1_000_000_000_000;

fn open_temp_db() -> (tempfile::TempDir, Db) {
    let temp = tempfile::tempdir().expect("tempdir");
    let db =
        Db::open(&temp.path().join("db"), synapse_core::SCHEMA_VERSION).expect("temp DB must open");
    (temp, db)
}

/// Persists a batch of pre-built rows through the raw journal path (no state
/// machine projection, so the rows are exactly what the test wrote) and
/// flushes so a subsequent scan sees them.
fn write_rows(db: &Db, rows: &[AgentEventRecord]) {
    record_agent_events_unobserved(db, rows).expect("rows must write");
    db.flush().expect("flush");
}

fn spawn_event(spawn: &str, ts_ns: u64, kind: AgentEventKind) -> AgentEventRecord {
    let mut record = AgentEventRecord::new(ts_ns, kind);
    record.spawn_id = Some(spawn.to_owned());
    record
}

/// A `tool_call_finished` row carrying a `duration_ms` (Claude PostToolUse),
/// optionally tagged with an `error.type`.
fn finished_call(
    spawn: &str,
    ts_ns: u64,
    duration_ms: u64,
    error: Option<&str>,
) -> AgentEventRecord {
    let mut record = spawn_event(spawn, ts_ns, AgentEventKind::ToolCallFinished);
    record.payload = json!({ "duration_ms": duration_ms });
    record.attributes.error_type = error.map(ToOwned::to_owned);
    record
}

/// An authoritative machine-emitted `state_changed` row (origin marker set), as
/// the #898 state machine writes them.
fn machine_state(spawn: &str, ts_ns: u64, from: &str, to: &str) -> AgentEventRecord {
    let mut record = spawn_event(spawn, ts_ns, AgentEventKind::StateChanged);
    record.state_from = Some(from.to_owned());
    record.state_to = Some(to.to_owned());
    record.payload = json!({ "origin": "agent_state_machine" });
    record
}

/// The canonical synthetic agent: a full lifecycle whose every derived figure
/// is hand-computable. Returns (rows, spawn_id).
fn synthetic_agent(spawn: &str) -> Vec<AgentEventRecord> {
    let secs = |n: u64| BASE_NS + n * 1_000_000_000;
    vec![
        spawn_event(spawn, secs(0), AgentEventKind::SpawnRequested),
        machine_state(spawn, secs(1), "spawning", "working"),
        spawn_event(spawn, secs(2), AgentEventKind::ToolCallStarted),
        finished_call(spawn, secs(5), 3000, None),
        spawn_event(spawn, secs(6), AgentEventKind::ToolCallStarted),
        finished_call(spawn, secs(7), 1000, Some("tool_failure")),
        spawn_event(spawn, secs(8), AgentEventKind::ToolCallStarted),
        finished_call(spawn, secs(9), 5000, None),
        spawn_event(spawn, secs(10), AgentEventKind::LeaseAcquired),
        spawn_event(spawn, secs(11), AgentEventKind::LeaseReleased),
        machine_state(spawn, secs(12), "working", "needs_input"),
        {
            let mut exited = spawn_event(spawn, secs(20), AgentEventKind::Exited);
            exited.end_state = Some(AgentEndState::Success);
            exited
        },
    ]
}

fn fleet_params() -> AgentStatsParams {
    AgentStatsParams {
        since_ns: None,
        until_ns: None,
        spawn_id: None,
        session_id: None,
        group_by: None,
    }
}

/// Happy path: one synthetic agent, every figure verified against physical rows.
#[test]
fn single_agent_stats_match_hand_computed_values() {
    let (_temp, db) = open_temp_db();
    let spawn = "agent-spawn-stats-it-happy";
    write_rows(&db, &synthetic_agent(spawn));

    let (anchors, scanned_rows) =
        collect_anchors(&db, &fleet_params(), MAX_SCAN_ROWS_PER_CALL).expect("scan");
    assert_eq!(scanned_rows, 12, "all 12 rows folded");
    assert_eq!(anchors.len(), 1, "one anchor");
    let stats = anchors.get(spawn).expect("anchor present").scope.finish();

    // Foundational counts.
    assert_eq!(stats.events_total, 12);
    assert_eq!(stats.events_by_kind["tool_call_started"], 3);
    assert_eq!(stats.events_by_kind["tool_call_finished"], 3);
    assert_eq!(stats.events_by_kind["state_changed"], 2);
    assert_eq!(stats.events_by_kind["exited"], 1);
    // Invariant: events_by_kind sums to events_total == scanned_rows.
    let kind_sum: u64 = stats.events_by_kind.values().sum();
    assert_eq!(kind_sum, stats.events_total);
    assert_eq!(stats.events_total, scanned_rows);

    // Actions: 3 started over a 20s span = 9/min.
    assert_eq!(stats.actions.tool_calls_started, 3);
    assert_eq!(stats.actions.tool_calls_finished, 3);
    assert_eq!(stats.observed_span_ms, 20_000);
    assert!(
        (stats.actions.tool_calls_started_per_min.unwrap() - 9.0).abs() < 1e-9,
        "{:?}",
        stats.actions.tool_calls_started_per_min
    );

    // Latency over [1000, 3000, 5000].
    let lat = &stats.tool_latency_ms;
    assert_eq!(lat.count, 3);
    assert_eq!(lat.min_ms, Some(1000));
    assert_eq!(lat.max_ms, Some(5000));
    assert_eq!(lat.p50_ms, Some(3000)); // rank ceil(1.5)=2 -> sorted[1]
    assert_eq!(lat.p95_ms, Some(5000)); // rank ceil(2.85)=3 -> sorted[2]
    assert_eq!(lat.p99_ms, Some(5000));
    assert!((lat.mean_ms.unwrap() - 3000.0).abs() < 1e-9);

    // Errors: 1 of 3 finished calls failed.
    assert_eq!(stats.errors.errored_tool_calls, 1);
    assert!((stats.errors.error_rate.unwrap() - 1.0 / 3.0).abs() < 1e-9);
    assert_eq!(stats.errors.by_type["tool_failure"], 1);

    // Leases.
    assert_eq!(stats.leases.acquired, 1);
    assert_eq!(stats.leases.released, 1);
    assert_eq!(stats.leases.held_open, 0);

    // End states.
    assert_eq!(stats.end_states["success"], 1);

    // Time-in-state: working from t1->t12 = 11s; needs_input from t12 to the
    // last journal row (Exited @ t20) = 8s.
    assert_eq!(stats.time_in_state_ms["working"], 11_000);
    assert_eq!(stats.time_in_state_ms["needs_input"], 8_000);

    // No usage attributes on hook rows -> zero, honestly.
    assert_eq!(stats.tokens.total, 0);
}

/// The error-rate breakdown must explain the same numerator as
/// `errored_tool_calls`; lifecycle rows with `error.type` are separate signals
/// and must not make the tool-call error breakdown fail to reconcile.
#[test]
fn non_tool_error_type_does_not_pollute_tool_call_error_breakdown() {
    let (_temp, db) = open_temp_db();
    let spawn = "agent-spawn-stats-it-non-tool-error";
    let secs = |n: u64| BASE_NS + n * 1_000_000_000;
    let mut lifecycle_error = spawn_event(spawn, secs(0), AgentEventKind::StateChanged);
    lifecycle_error.attributes.error_type = Some("LOCAL_AGENT_TIMEOUT".to_owned());
    lifecycle_error.reason_code = Some("LOCAL_AGENT_TIMEOUT".to_owned());
    write_rows(
        &db,
        &[
            lifecycle_error,
            spawn_event(spawn, secs(1), AgentEventKind::ToolCallStarted),
            finished_call(spawn, secs(2), 1000, None),
        ],
    );

    let (anchors, scanned_rows) =
        collect_anchors(&db, &fleet_params(), MAX_SCAN_ROWS_PER_CALL).expect("scan");
    assert_eq!(scanned_rows, 3);
    let stats = anchors.get(spawn).expect("anchor").scope.finish();
    assert_eq!(stats.errors.errored_tool_calls, 0);
    assert_eq!(stats.errors.error_rate, Some(0.0));
    assert!(stats.errors.by_type.is_empty());
}

/// Fleet aggregation over two agents: counts sum, percentile is over the pooled
/// sample, and time-in-state sums each agent's intervals. group_by=agent splits.
#[test]
fn fleet_aggregates_two_agents_and_group_by_agent_splits() {
    let (_temp, db) = open_temp_db();
    let a = "agent-spawn-stats-it-a";
    let b = "agent-spawn-stats-it-b";
    write_rows(&db, &synthetic_agent(a));
    write_rows(&db, &synthetic_agent(b));

    let (anchors, scanned_rows) =
        collect_anchors(&db, &fleet_params(), MAX_SCAN_ROWS_PER_CALL).expect("scan");
    assert_eq!(scanned_rows, 24);
    assert_eq!(anchors.len(), 2);

    // Fold the fleet exactly as the impl does.
    let mut fleet = ScopeAccumulator::default();
    for acc in anchors.values() {
        fleet.merge(&acc.scope);
    }
    let fleet = fleet.finish();

    assert_eq!(fleet.events_total, 24);
    assert_eq!(fleet.actions.tool_calls_started, 6);
    // Pooled latency sample = two copies of [1000,3000,5000] =
    // [1000,1000,3000,3000,5000,5000]; n=6.
    assert_eq!(fleet.tool_latency_ms.count, 6);
    // p50: rank ceil(3.0)=3 -> sorted[2]=3000.
    assert_eq!(fleet.tool_latency_ms.p50_ms, Some(3000));
    // p95: rank ceil(5.7)=6 -> sorted[5]=5000.
    assert_eq!(fleet.tool_latency_ms.p95_ms, Some(5000));
    assert_eq!(fleet.errors.errored_tool_calls, 2);
    // Time-in-state sums both agents: working 22s, needs_input 16s.
    assert_eq!(fleet.time_in_state_ms["working"], 22_000);
    assert_eq!(fleet.time_in_state_ms["needs_input"], 16_000);
    assert_eq!(fleet.end_states["success"], 2);
}

/// Boundary audit: a `until_ns` window stops the scan at the boundary, and the
/// folded rows / derived numbers reflect only the in-window rows.
#[test]
fn until_window_excludes_rows_at_and_after_the_boundary() {
    let (_temp, db) = open_temp_db();
    let spawn = "agent-spawn-stats-it-window";
    write_rows(&db, &synthetic_agent(spawn));

    // Cut at t=8s (exclusive): keeps rows at t0,t1,t2,t5,t6,t7 (6 rows), drops
    // the second ToolCallStarted @ t8 and everything after.
    let secs = |n: u64| BASE_NS + n * 1_000_000_000;
    let params = AgentStatsParams {
        until_ns: Some(secs(8)),
        ..fleet_params()
    };
    let (anchors, scanned_rows) =
        collect_anchors(&db, &params, MAX_SCAN_ROWS_PER_CALL).expect("scan");
    assert_eq!(scanned_rows, 6, "only the 6 pre-boundary rows");
    let stats = anchors.get(spawn).expect("anchor").scope.finish();
    assert_eq!(stats.events_total, 6);
    assert_eq!(stats.actions.tool_calls_started, 2);
    assert_eq!(stats.actions.tool_calls_finished, 2);
    // Latencies in window: [3000, 1000].
    assert_eq!(stats.tool_latency_ms.count, 2);
    assert_eq!(stats.tool_latency_ms.max_ms, Some(3000));
    // No exited row in window -> no end state.
    assert!(stats.end_states.is_empty());
}

/// Filtering to one spawn id keeps only that agent's rows.
#[test]
fn spawn_id_filter_isolates_one_agent() {
    let (_temp, db) = open_temp_db();
    let a = "agent-spawn-stats-it-filter-a";
    let b = "agent-spawn-stats-it-filter-b";
    write_rows(&db, &synthetic_agent(a));
    write_rows(&db, &synthetic_agent(b));

    let params = AgentStatsParams {
        spawn_id: Some(a.to_owned()),
        ..fleet_params()
    };
    let (anchors, scanned_rows) =
        collect_anchors(&db, &params, MAX_SCAN_ROWS_PER_CALL).expect("scan");
    assert_eq!(scanned_rows, 12, "only agent a's rows");
    assert_eq!(anchors.len(), 1);
    assert!(anchors.contains_key(a));
    assert!(!anchors.contains_key(b));
}

/// Empty window: no rows, no panic, honest zeroes and `None` rates.
#[test]
fn empty_window_yields_zeroed_stats_without_panicking() {
    let (_temp, db) = open_temp_db();
    let (anchors, scanned_rows) =
        collect_anchors(&db, &fleet_params(), MAX_SCAN_ROWS_PER_CALL).expect("scan");
    assert_eq!(scanned_rows, 0);
    assert!(anchors.is_empty());

    let stats = ScopeAccumulator::default().finish();
    assert_eq!(stats.events_total, 0);
    assert_eq!(stats.observed_span_ms, 0);
    assert_eq!(stats.tool_latency_ms.count, 0);
    assert_eq!(stats.tool_latency_ms.p50_ms, None);
    assert_eq!(stats.actions.tool_calls_started_per_min, None);
    assert_eq!(stats.errors.error_rate, None);
    assert_eq!(stats.tokens.tokens_per_min, None);
    assert!(stats.time_in_state_ms.is_empty());
}

/// Budget exhaustion is a loud error, never a silent truncation.
#[test]
fn scan_budget_exhaustion_errors_loudly() {
    let (_temp, db) = open_temp_db();
    let spawn = "agent-spawn-stats-it-budget";
    write_rows(&db, &synthetic_agent(spawn)); // 12 rows

    // Budget of 3 in-window rows must refuse rather than truncate to 3.
    let error = collect_anchors(&db, &fleet_params(), 3).expect_err("budget must trip");
    assert!(
        error.message.contains("AGENT_STATS_SCAN_BUDGET_EXHAUSTED"),
        "structured budget error expected: {}",
        error.message
    );
}

/// Pure percentile math: nearest-rank against a 1..=100 sample with known ranks.
#[test]
fn percentile_nearest_rank_matches_definition() {
    let sample: Vec<u64> = (1..=100).collect();
    assert_eq!(percentile_nearest_rank(&sample, 50), Some(50)); // rank 50
    assert_eq!(percentile_nearest_rank(&sample, 95), Some(95)); // rank 95
    assert_eq!(percentile_nearest_rank(&sample, 99), Some(99)); // rank 99
    assert_eq!(percentile_nearest_rank(&sample, 100), Some(100)); // rank 100

    // Single sample: every percentile is that value.
    assert_eq!(percentile_nearest_rank(&[42], 50), Some(42));
    assert_eq!(percentile_nearest_rank(&[42], 99), Some(42));

    // Empty: no value.
    assert_eq!(percentile_nearest_rank(&[], 50), None);
}

/// `latency_stats` on an empty sample is all-`None`, count 0.
#[test]
fn latency_stats_empty_sample_is_all_none() {
    let lat = latency_stats(&[]);
    assert_eq!(lat.count, 0);
    assert_eq!(lat.p50_ms, None);
    assert_eq!(lat.min_ms, None);
    assert_eq!(lat.max_ms, None);
    assert_eq!(lat.mean_ms, None);
}

/// Param contract: inverted window and unknown group_by are refused; valid
/// group_by values resolve the per-agent flag.
#[test]
fn resolve_options_enforces_param_contract() {
    let inverted = AgentStatsParams {
        since_ns: Some(100),
        until_ns: Some(100),
        ..fleet_params()
    };
    let error = resolve_options(&inverted).expect_err("since==until must refuse");
    assert!(
        error.message.contains("AGENT_STATS_RANGE_INVALID"),
        "{}",
        error.message
    );

    let bad_group = AgentStatsParams {
        group_by: Some("by_model".to_owned()),
        ..fleet_params()
    };
    let error = resolve_options(&bad_group).expect_err("unknown group_by must refuse");
    assert!(
        error.message.contains("AGENT_STATS_GROUP_BY_INVALID"),
        "{}",
        error.message
    );

    assert!(!resolve_options(&fleet_params()).expect("default ok"));
    assert!(
        resolve_options(&AgentStatsParams {
            group_by: Some("agent".to_owned()),
            ..fleet_params()
        })
        .expect("agent ok")
    );
}

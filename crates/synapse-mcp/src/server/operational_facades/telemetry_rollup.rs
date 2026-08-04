//! Native Aster TimeSeries storage for telemetry samples and bounded trends.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use synapse_core::{AgentEndState, AgentEventRecord, error_codes};
use synapse_storage::{Db, SynapseCalyxRollupWindow, cf};
use tokio::sync::Semaphore;

use crate::m1::mcp_error;
use crate::server::ErrorData;

const COLLECTION: &str = "syn-telemetry-v3";
const METRIC_OWNER_PREFIX: &str = "telemetry/native/v3/series/";
const EVENT_PROGRESS_KEY: &[u8] = b"telemetry/native/v3/agent-event-progress";
const EVENT_SCAN_ROWS: usize = 4_096;
const NANOS_PER_HOUR: u64 = 60 * 60 * 1_000_000_000;
const NANOS_PER_DAY: u64 = 24 * NANOS_PER_HOUR;
const TELEMETRY_SEAL_GRACE_NS: u64 = NANOS_PER_HOUR;
const TELEMETRY_MAX_TREND_WINDOWS: u64 = 366 * 24;

pub(crate) const METRIC_AGENT_EVENTS_TOTAL: &str = "agent_events.total";
pub(crate) const METRIC_AGENT_EVENTS_ERROR: &str = "agent_events.error";

static TELEMETRY_MATERIALIZE_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(1)));

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TelemetryTrendWindow {
    pub window_start_ns: u64,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TelemetryTrend {
    pub metric: String,
    pub tier: String,
    pub effective_since_ns: u64,
    pub effective_until_ns: u64,
    pub materialized_through_ns: u64,
    pub windows_read: u64,
    pub windows: Vec<TelemetryTrendWindow>,
}

#[derive(Clone, Debug)]
pub(crate) struct TelemetryMaterializeReport {
    pub built_at_ns: u64,
    pub sealed_horizon_ns: u64,
    pub materialized_through_ns: u64,
    pub points_scanned: u64,
    pub cells_written: u64,
}

pub(crate) fn record_telemetry_sample(
    db: &Db,
    metric: &str,
    value: f64,
    ts_ns: u64,
) -> Result<(), ErrorData> {
    if metric.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "TELEMETRY_METRIC_EMPTY: metric must be non-blank",
        ));
    }
    if !value.is_finite() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("TELEMETRY_SAMPLE_NONFINITE: metric {metric} value {value} is not finite"),
        ));
    }
    let series = metric_series(metric);
    verify_metric_owner(db, series, metric)?;
    db.timeseries_write(COLLECTION, series, ts_ns, value)
        .map(|_| ())
        .map_err(|error| mcp_error(error.code(), error.to_string()))
}

pub(crate) fn materialize_telemetry_rollups_if_idle(
    db: &Db,
) -> Option<Result<TelemetryMaterializeReport, ErrorData>> {
    let permit = TELEMETRY_MATERIALIZE_PERMITS.try_acquire().ok()?;
    let _permit = permit;
    Some(materialize_telemetry_rollups(db))
}

/// Native rollups are folded atomically during `record_telemetry_sample`.
pub(crate) fn materialize_telemetry_rollups(
    db: &Db,
) -> Result<TelemetryMaterializeReport, ErrorData> {
    db.ensure_timeseries_collection(COLLECTION)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let built_at_ns = now_ns();
    let sealed_horizon_ns = floor(
        built_at_ns.saturating_sub(TELEMETRY_SEAL_GRACE_NS),
        NANOS_PER_HOUR,
    );
    let mut start = db
        .get_cf(cf::CF_KV, EVENT_PROGRESS_KEY)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .map_or_else(Vec::new, |key| key_after(&key));
    let mut points_scanned = 0_u64;
    let mut hours = BTreeMap::<u64, (u64, u64, Vec<u8>)>::new();
    'pages: loop {
        let (rows, more) = db
            .scan_cf_from(cf::CF_AGENT_EVENTS, &start, EVENT_SCAN_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            let record: AgentEventRecord = serde_json::from_slice(value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "TELEMETRY_AGENT_EVENT_CORRUPT: key={} failed to decode: {error}",
                        encode_hex(key)
                    ),
                )
            })?;
            if record.ts_ns >= sealed_horizon_ns {
                break 'pages;
            }
            let hour = floor(record.ts_ns, NANOS_PER_HOUR);
            let entry = hours.entry(hour).or_insert_with(|| (0, 0, key.clone()));
            entry.0 = entry.0.saturating_add(1);
            if record.end_state == Some(AgentEndState::Error) {
                entry.1 = entry.1.saturating_add(1);
            }
            key.clone_into(&mut entry.2);
            points_scanned = points_scanned.saturating_add(1);
        }
        let Some((last_key, _)) = rows.last() else {
            break;
        };
        start = key_after(last_key);
        if !more {
            break;
        }
    }
    let mut cells_written = 0_u64;
    for (hour, (total, errors, last_key)) in hours {
        publish_sealed_event_hour(db, METRIC_AGENT_EVENTS_TOTAL, hour, total)?;
        cells_written = cells_written.saturating_add(1);
        if errors != 0 {
            publish_sealed_event_hour(db, METRIC_AGENT_EVENTS_ERROR, hour, errors)?;
            cells_written = cells_written.saturating_add(1);
        }
        db.put_batch_pressure_bypass(cf::CF_KV, [(EVENT_PROGRESS_KEY.to_vec(), last_key)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    }
    Ok(TelemetryMaterializeReport {
        built_at_ns,
        sealed_horizon_ns,
        materialized_through_ns: sealed_horizon_ns,
        points_scanned,
        cells_written,
    })
}

fn publish_sealed_event_hour(
    db: &Db,
    metric: &str,
    hour_start_ns: u64,
    count: u64,
) -> Result<(), ErrorData> {
    if count > (1_u64 << 53) {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "TELEMETRY_EVENT_COUNT_INEXACT: metric={metric} hour={hour_start_ns} count={count} exceeds exact f64 integer range"
            ),
        ));
    }
    let series = metric_series(metric);
    verify_metric_owner(db, series, metric)?;
    db.timeseries_write(COLLECTION, series, hour_start_ns, count as f64)
        .map(|_| ())
        .map_err(|error| mcp_error(error.code(), error.to_string()))
}

pub(crate) fn telemetry_trend(
    db: &Db,
    metric: &str,
    since_ns: u64,
    until_ns: u64,
    tier: u8,
) -> Result<TelemetryTrend, ErrorData> {
    let (span, window, tier_name) = match tier {
        0 => (NANOS_PER_HOUR, SynapseCalyxRollupWindow::Hour, "hour"),
        1 => (NANOS_PER_DAY, SynapseCalyxRollupWindow::Day, "day"),
        _ => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("TELEMETRY_ROLLUP_TIER_INVALID: tier {tier} is not hour(0) or day(1)"),
            ));
        }
    };
    if since_ns >= until_ns {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "TELEMETRY_ROLLUP_RANGE_INVALID: since_ns must be less than until_ns",
        ));
    }
    let materialized_through_ns = floor(
        now_ns().saturating_sub(TELEMETRY_SEAL_GRACE_NS),
        NANOS_PER_HOUR,
    );
    let effective_since_ns = floor(since_ns, span);
    let effective_until_ns = ceil(until_ns, span).min(materialized_through_ns);
    if effective_until_ns <= effective_since_ns {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "TELEMETRY_ROLLUP_WINDOW_UNSEALED: window [{since_ns}, {until_ns}) is at or after the sealed watermark {materialized_through_ns}"
            ),
        ));
    }
    let window_count = (effective_until_ns - effective_since_ns) / span;
    if window_count > TELEMETRY_MAX_TREND_WINDOWS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "TELEMETRY_ROLLUP_WINDOW_TOO_WIDE: effective window spans {window_count} cells (> {TELEMETRY_MAX_TREND_WINDOWS}); narrow since_ns/until_ns"
            ),
        ));
    }
    let series = metric_series(metric);
    verify_metric_owner(db, series, metric)?;
    let mut windows = Vec::new();
    let mut cursor = effective_since_ns;
    while cursor < effective_until_ns {
        if let Some(value) = db
            .timeseries_rollup(COLLECTION, series, window, cursor)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
        {
            windows.push(TelemetryTrendWindow {
                window_start_ns: cursor,
                count: value.count,
                sum: value.sum,
                min: value.min,
                max: value.max,
                mean: value.sum / value.count as f64,
            });
        }
        cursor = cursor.checked_add(span).ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "TELEMETRY_ROLLUP_RANGE_OVERFLOW: window cursor overflowed u64",
            )
        })?;
    }
    Ok(TelemetryTrend {
        metric: metric.to_owned(),
        tier: tier_name.to_owned(),
        effective_since_ns,
        effective_until_ns,
        materialized_through_ns,
        windows_read: window_count,
        windows,
    })
}

fn verify_metric_owner(db: &Db, series: u64, metric: &str) -> Result<(), ErrorData> {
    let key = format!("{METRIC_OWNER_PREFIX}{series:016x}");
    match db
        .get_cf(synapse_storage::cf::CF_KV, key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
    {
        Some(owner) if owner == metric.as_bytes() => Ok(()),
        Some(owner) => Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "TELEMETRY_SERIES_COLLISION: series {series:016x} is owned by {:?}, not {metric:?}; change the series hash domain",
                String::from_utf8_lossy(&owner)
            ),
        )),
        None => {
            db.put_batch_pressure_bypass(
                synapse_storage::cf::CF_KV,
                [(key.into_bytes(), metric.as_bytes().to_vec())],
            )
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            let owner = db
                .get_cf(
                    synapse_storage::cf::CF_KV,
                    format!("{METRIC_OWNER_PREFIX}{series:016x}").as_bytes(),
                )
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            if owner.as_deref() != Some(metric.as_bytes()) {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "TELEMETRY_SERIES_OWNER_READBACK_MISMATCH: owner row did not match immediately after write",
                ));
            }
            Ok(())
        }
    }
}

fn metric_series(metric: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse:telemetry:series:v3\0");
    hasher.update(metric.as_bytes());
    let digest = hasher.finalize();
    let mut id = [0_u8; 8];
    id.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(id)
}

fn floor(value: u64, span: u64) -> u64 {
    value - value % span
}

fn ceil(value: u64, span: u64) -> u64 {
    floor(value.saturating_add(span - 1), span)
}

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = Vec::with_capacity(key.len() + 1);
    next.extend_from_slice(key);
    next.push(0);
    next
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

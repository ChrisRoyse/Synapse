//! #1688 — Telemetry TimeSeries rollups (bounded telemetry/health trend readbacks).
//!
//! The cost half of #1688 (see `server::agent_cost`) moved fleet cost analytics
//! off a full transcript scan onto materialized hour/day rollups in the vault.
//! This module does the same for **telemetry**: it captures metric samples as
//! durable series points and materializes windowed rollups (count / sum / min /
//! max, from which mean is derived) per hour and per day, so a telemetry-ring or
//! health-trend readback is a bounded rollup read — O(windows) — rather than a
//! scan over a growing event/metric stream.
//!
//! Substrate & doctrine
//! --------------------
//! * **Raw sample points** live in `CF_TELEMETRY` under `tsample/v1/` keyed
//!   `metric_hash(8) ‖ ts_ns_be(8) ‖ seq_be(8)`. `CF_TELEMETRY` carries a short
//!   TTL (6h) — raw samples are transient, exactly aster's "rollup-only"
//!   retention idea.
//! * **Rollup cells** live in the GC-protected `CF_KV` family under
//!   `telemetry/rollup/v1/cell/` keyed `tier(1) ‖ metric_hash(8) ‖
//!   window_start_be(8)`, so one metric's windows are contiguous and a trend
//!   read seeks directly to the window range. This mirrors the cost rollup key
//!   layout and how aster encodes native TimeSeries rows.
//!
//! Everything is a pure, rebuildable function of the sample points — no side
//! store. Sample capture is maintained at ingest (the agent-event ingress path
//! and the telemetry `status` handler record samples); the rollups are then
//! **materialized forward off the MCP runtime**: each pass seals every window
//! older than a grace horizon exactly once and advances a `materialized_through`
//! watermark, so re-runs and crashes are idempotent (a sealed window is
//! recomputed only until it is finalized) and a raw sample may age out of its
//! short-TTL CF once its window is sealed. Trend reads only ever serve windows
//! at or before the watermark, so they never observe a half-materialized window.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;
use synapse_storage::{Db, cf};
use tokio::sync::Semaphore;

use crate::m1::mcp_error;
use crate::server::ErrorData;

const SAMPLE_KEY_PREFIX: &[u8] = b"tsample/v1/";
const ROLLUP_KEY_PREFIX: &str = "telemetry/rollup/v1/";
const ROLLUP_META_KEY: &str = "telemetry/rollup/v1/__meta";
const ROLLUP_CELL_INFIX: &[u8] = b"cell/";
const ROLLUP_SCHEMA_VERSION: u32 = 1;

const TELEMETRY_TIER_HOUR: u8 = 0;
const TELEMETRY_TIER_DAY: u8 = 1;
const NANOS_PER_HOUR: u64 = 60 * 60 * 1_000_000_000;
const NANOS_PER_DAY: u64 = 24 * NANOS_PER_HOUR;
/// A window is sealed (final) once older than this grace period — comfortably
/// under the `CF_TELEMETRY` TTL so raw points survive until their window seals.
const TELEMETRY_SEAL_GRACE_NS: u64 = NANOS_PER_HOUR;
/// Refuse an absurdly wide trend window rather than read tens of thousands of
/// cells; the caller must narrow the window.
const TELEMETRY_MAX_TREND_WINDOWS: u64 = 366 * 24;
const SCAN_CHUNK_ROWS: usize = 4_096;

/// Well-known telemetry metric ids recorded by the ingest producers.
pub(crate) const METRIC_AGENT_EVENTS_TOTAL: &str = "agent_events.total";
pub(crate) const METRIC_AGENT_EVENTS_ERROR: &str = "agent_events.error";

/// Process-lifetime disambiguator for same-timestamp samples of one metric.
static TELEMETRY_SAMPLE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Single-permit admission gate: one telemetry rollup materialization at a time
/// per process, shared by the periodic maintenance hook so passes never race on
/// the same cells.
static TELEMETRY_MATERIALIZE_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// One durable telemetry sample point.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetrySamplePoint {
    schema_version: u32,
    metric: String,
    value: f64,
    ts_ns: u64,
}

/// Windowed rollup accumulator for one (metric, tier, window). `mean` is derived
/// at read time as `sum / count`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetryRollupCell {
    schema_version: u32,
    metric: String,
    tier: u8,
    window_start_ns: u64,
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetryRollupMeta {
    schema_version: u32,
    built_at_ns: u64,
    /// Rollup windows strictly before this are final; trend reads clamp here.
    materialized_through_ns: u64,
    sealed_horizon_ns: u64,
    points_scanned: u64,
}

/// One window of a trend readback.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TelemetryTrendWindow {
    pub window_start_ns: u64,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
}

/// A bounded, rollup-served trend readback for one metric.
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

/// Outcome of one materialization pass.
#[derive(Clone, Debug)]
pub(crate) struct TelemetryMaterializeReport {
    pub built_at_ns: u64,
    pub sealed_horizon_ns: u64,
    pub materialized_through_ns: u64,
    pub points_scanned: u64,
    pub cells_written: u64,
}

// ---------------------------------------------------------------------------
// Ingest capture (maintained at ingest)
// ---------------------------------------------------------------------------

/// Durably captures one telemetry metric sample. Called on the ingest paths
/// (agent-event ingress, telemetry status). The rollups themselves are
/// materialized forward off the runtime from these points.
///
/// # Errors
///
/// Returns a structured error when the value is non-finite (which would corrupt
/// a rollup) or the durable write fails. Producers treat capture as best-effort
/// and log failures rather than failing their own operation.
pub(crate) fn record_telemetry_sample(
    db: &Db,
    metric: &str,
    value: f64,
    ts_ns: u64,
) -> Result<(), ErrorData> {
    if !value.is_finite() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("TELEMETRY_SAMPLE_NONFINITE: metric {metric} value {value} is not finite"),
        ));
    }
    let seq = TELEMETRY_SAMPLE_SEQ.fetch_add(1, Ordering::Relaxed);
    let key = sample_key(metric, ts_ns, seq);
    let point = TelemetrySamplePoint {
        schema_version: ROLLUP_SCHEMA_VERSION,
        metric: metric.to_owned(),
        value,
        ts_ns,
    };
    db.put_batch_pressure_bypass(cf::CF_TELEMETRY, [(key, encode_json(&point)?)])
        .map_err(|error| mcp_error(error.code(), error.to_string()))
}

// ---------------------------------------------------------------------------
// Forward materialization (off-runtime)
// ---------------------------------------------------------------------------

/// Runs a telemetry rollup materialization pass only if no other pass holds the
/// admission permit. Returns `None` when one is already in flight. Used by the
/// periodic maintenance hook so rollups stay current without an operator call.
pub(crate) fn materialize_telemetry_rollups_if_idle(
    db: &Db,
) -> Option<Result<TelemetryMaterializeReport, ErrorData>> {
    let permit = TELEMETRY_MATERIALIZE_PERMITS.try_acquire().ok()?;
    let _permit = permit;
    Some(materialize_telemetry_rollups(db))
}

/// Materializes every telemetry rollup window that has sealed since the last
/// pass, advancing the `materialized_through` watermark.
///
/// Forward-only and idempotent: only windows in `[materialized_through,
/// sealed_horizon)` are (re)computed from the durable sample points, then the
/// watermark advances to the sealed horizon. A crash before the watermark
/// advances simply recomputes the same windows (overwrite is a no-op), and a
/// window is finalized exactly once, so a raw point may age out of its short-TTL
/// CF after its window seals without corrupting the rollup.
pub(crate) fn materialize_telemetry_rollups(
    db: &Db,
) -> Result<TelemetryMaterializeReport, ErrorData> {
    let built_at_ns = now_ns();
    let sealed_horizon_ns = floor_tier(
        built_at_ns.saturating_sub(TELEMETRY_SEAL_GRACE_NS),
        TELEMETRY_TIER_HOUR,
    );
    let previous_through = read_meta(db)?
        .map(|meta| meta.materialized_through_ns)
        .unwrap_or(0);

    let mut points_scanned: u64 = 0;
    let mut cells_written: u64 = 0;

    if sealed_horizon_ns > previous_through {
        // Accumulate per (metric_hash, tier, window) for windows newly sealed in
        // [previous_through, sealed_horizon). CF_TELEMETRY is small (short TTL),
        // so a full scan per pass is cheap and bounded.
        let mut accumulators: BTreeMap<(u64, u8, u64), Accumulator> = BTreeMap::new();
        let mut metric_names: BTreeMap<u64, String> = BTreeMap::new();
        let mut start: Vec<u8> = SAMPLE_KEY_PREFIX.to_vec();
        'scan: loop {
            let (rows, more) = db
                .scan_cf_from(cf::CF_TELEMETRY, &start, SCAN_CHUNK_ROWS)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            if rows.is_empty() {
                break;
            }
            for (key, value) in &rows {
                if !key.starts_with(SAMPLE_KEY_PREFIX) {
                    break 'scan;
                }
                points_scanned = points_scanned.saturating_add(1);
                let Some(metric_hash) = decode_sample_metric_hash(key) else {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "TELEMETRY_SAMPLE_KEY_CORRUPT: telemetry sample key is malformed",
                    ));
                };
                let point: TelemetrySamplePoint =
                    serde_json::from_slice(value).map_err(|error| {
                        mcp_error(
                            error_codes::TOOL_INTERNAL_ERROR,
                            format!(
                                "TELEMETRY_SAMPLE_ROW_CORRUPT: sample failed to decode: {error}"
                            ),
                        )
                    })?;
                metric_names.entry(metric_hash).or_insert(point.metric);
                for tier in [TELEMETRY_TIER_HOUR, TELEMETRY_TIER_DAY] {
                    let window_start_ns = floor_tier(point.ts_ns, tier);
                    if window_start_ns < previous_through || window_start_ns >= sealed_horizon_ns {
                        continue;
                    }
                    accumulators
                        .entry((metric_hash, tier, window_start_ns))
                        .or_default()
                        .push(point.value);
                }
            }
            let Some((last_key, _value)) = rows.last() else {
                break;
            };
            start = key_after(last_key);
            if !more {
                break;
            }
        }

        for ((metric_hash, tier, window_start_ns), accumulator) in accumulators {
            let metric = metric_names
                .get(&metric_hash)
                .cloned()
                .unwrap_or_else(|| format!("{metric_hash:016x}"));
            let cell = TelemetryRollupCell {
                schema_version: ROLLUP_SCHEMA_VERSION,
                metric,
                tier,
                window_start_ns,
                count: accumulator.count,
                sum: accumulator.sum,
                min: accumulator.min,
                max: accumulator.max,
            };
            let key = rollup_cell_key(tier, metric_hash, window_start_ns);
            db.put_batch_pressure_bypass(cf::CF_KV, [(key, encode_json(&cell)?)])
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            cells_written = cells_written.saturating_add(1);
        }
    }

    let materialized_through_ns = previous_through.max(sealed_horizon_ns);
    let meta = TelemetryRollupMeta {
        schema_version: ROLLUP_SCHEMA_VERSION,
        built_at_ns,
        materialized_through_ns,
        sealed_horizon_ns,
        points_scanned,
    };
    db.put_batch_pressure_bypass(
        cf::CF_KV,
        [(ROLLUP_META_KEY.as_bytes().to_vec(), encode_json(&meta)?)],
    )
    .map_err(|error| mcp_error(error.code(), error.to_string()))?;

    tracing::info!(
        code = "TELEMETRY_ROLLUP_MATERIALIZED",
        built_at_ns,
        sealed_horizon_ns,
        materialized_through_ns,
        points_scanned,
        cells_written,
        "materialized telemetry TimeSeries rollups off the MCP runtime"
    );

    Ok(TelemetryMaterializeReport {
        built_at_ns,
        sealed_horizon_ns,
        materialized_through_ns,
        points_scanned,
        cells_written,
    })
}

// ---------------------------------------------------------------------------
// Bounded trend readback (served from rollups)
// ---------------------------------------------------------------------------

/// Answers a telemetry/health trend for one metric from the materialized
/// rollups with a bounded read (O(windows)), never a sample scan.
///
/// # Errors
///
/// Fails closed when the rollups are not materialized, the window is entirely
/// unsealed, or the window is absurdly wide — never a silent sample scan.
pub(crate) fn telemetry_trend(
    db: &Db,
    metric: &str,
    since_ns: u64,
    until_ns: u64,
    tier: u8,
) -> Result<TelemetryTrend, ErrorData> {
    let Some(meta) = read_meta(db)? else {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "TELEMETRY_ROLLUP_NOT_MATERIALIZED: telemetry trends are served from materialized \
             rollups, never a sample scan. Wait for the periodic materializer to seal a window, \
             then retry.",
        ));
    };
    let span = tier_span(tier)?;
    let metric_hash = metric_hash_u64(metric);
    let effective_since_ns = floor_tier(since_ns, tier);
    let effective_until_ns = ceil_tier(until_ns, tier).min(meta.materialized_through_ns);
    if effective_until_ns <= effective_since_ns {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "TELEMETRY_ROLLUP_WINDOW_UNSEALED: window [{since_ns}, {until_ns}) is at or after \
                 the materialized watermark {}; query a window that ends before it.",
                meta.materialized_through_ns
            ),
        ));
    }
    let window_count = (effective_until_ns - effective_since_ns) / span;
    if window_count > TELEMETRY_MAX_TREND_WINDOWS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "TELEMETRY_ROLLUP_WINDOW_TOO_WIDE: effective window spans {window_count} cells \
                 (> {TELEMETRY_MAX_TREND_WINDOWS}); narrow since_ns/until_ns"
            ),
        ));
    }

    let mut windows = Vec::new();
    let mut windows_read: u64 = 0;
    let mut start = rollup_cell_key(tier, metric_hash, effective_since_ns);
    'scan: loop {
        let (rows, more) = db
            .scan_cf_from(cf::CF_KV, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            let Some((row_tier, row_metric_hash, window_start_ns)) = decode_rollup_cell_key(key)
            else {
                break 'scan;
            };
            if row_tier != tier || row_metric_hash != metric_hash {
                break 'scan;
            }
            if window_start_ns >= effective_until_ns {
                break 'scan;
            }
            if window_start_ns < effective_since_ns {
                continue;
            }
            let cell: TelemetryRollupCell = serde_json::from_slice(value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("TELEMETRY_ROLLUP_CELL_CORRUPT: rollup cell failed to decode: {error}"),
                )
            })?;
            let mean = if cell.count == 0 {
                0.0
            } else {
                cell.sum / cell.count as f64
            };
            windows_read = windows_read.saturating_add(1);
            windows.push(TelemetryTrendWindow {
                window_start_ns: cell.window_start_ns,
                count: cell.count,
                sum: cell.sum,
                min: cell.min,
                max: cell.max,
                mean,
            });
        }
        let Some((last_key, _value)) = rows.last() else {
            break;
        };
        start = key_after(last_key);
        if !more {
            break;
        }
    }

    Ok(TelemetryTrend {
        metric: metric.to_owned(),
        tier: tier_label(tier).to_owned(),
        effective_since_ns,
        effective_until_ns,
        materialized_through_ns: meta.materialized_through_ns,
        windows_read,
        windows,
    })
}

// ---------------------------------------------------------------------------
// Codec + helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Accumulator {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Default for Accumulator {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
}

impl Accumulator {
    fn push(&mut self, value: f64) {
        self.count = self.count.saturating_add(1);
        self.sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }
}

fn read_meta(db: &Db) -> Result<Option<TelemetryRollupMeta>, ErrorData> {
    let Some(value) = db
        .get_cf(cf::CF_KV, ROLLUP_META_KEY.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
    else {
        return Ok(None);
    };
    let meta: TelemetryRollupMeta = serde_json::from_slice(&value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("TELEMETRY_ROLLUP_META_CORRUPT: {ROLLUP_META_KEY}: {error}"),
        )
    })?;
    if meta.schema_version != ROLLUP_SCHEMA_VERSION {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "TELEMETRY_ROLLUP_SCHEMA_UNSUPPORTED: {ROLLUP_META_KEY} version {} != expected {ROLLUP_SCHEMA_VERSION}",
                meta.schema_version
            ),
        ));
    }
    Ok(Some(meta))
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ErrorData> {
    serde_json::to_vec(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("TELEMETRY_ROLLUP_ENCODE_FAILED: {error}"),
        )
    })
}

fn metric_hash_bytes(metric: &str) -> [u8; 8] {
    let digest = Sha256::digest(metric.as_bytes());
    let mut out = [0_u8; 8];
    out.copy_from_slice(&digest[0..8]);
    out
}

fn metric_hash_u64(metric: &str) -> u64 {
    u64::from_be_bytes(metric_hash_bytes(metric))
}

fn sample_key(metric: &str, ts_ns: u64, seq: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(SAMPLE_KEY_PREFIX.len() + 8 + 8 + 8);
    key.extend_from_slice(SAMPLE_KEY_PREFIX);
    key.extend_from_slice(&metric_hash_bytes(metric));
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn decode_sample_metric_hash(key: &[u8]) -> Option<u64> {
    let rest = key.strip_prefix(SAMPLE_KEY_PREFIX)?;
    let hash_bytes = rest.get(0..8)?;
    Some(u64::from_be_bytes(hash_bytes.try_into().ok()?))
}

fn rollup_cell_scan_prefix() -> Vec<u8> {
    let mut key = ROLLUP_KEY_PREFIX.as_bytes().to_vec();
    key.extend_from_slice(ROLLUP_CELL_INFIX);
    key
}

fn rollup_cell_key(tier: u8, metric_hash: u64, window_start_ns: u64) -> Vec<u8> {
    let mut key = rollup_cell_scan_prefix();
    key.push(tier);
    key.extend_from_slice(&metric_hash.to_be_bytes());
    key.extend_from_slice(&window_start_ns.to_be_bytes());
    key
}

fn decode_rollup_cell_key(key: &[u8]) -> Option<(u8, u64, u64)> {
    let prefix = rollup_cell_scan_prefix();
    let rest = key.strip_prefix(prefix.as_slice())?;
    let tier = *rest.first()?;
    let metric_hash = u64::from_be_bytes(rest.get(1..9)?.try_into().ok()?);
    let window_start_ns = u64::from_be_bytes(rest.get(9..17)?.try_into().ok()?);
    Some((tier, metric_hash, window_start_ns))
}

fn tier_span(tier: u8) -> Result<u64, ErrorData> {
    match tier {
        TELEMETRY_TIER_HOUR => Ok(NANOS_PER_HOUR),
        TELEMETRY_TIER_DAY => Ok(NANOS_PER_DAY),
        other => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("TELEMETRY_ROLLUP_TIER_INVALID: tier {other} is not hour(0) or day(1)"),
        )),
    }
}

fn tier_label(tier: u8) -> &'static str {
    match tier {
        TELEMETRY_TIER_DAY => "day",
        _ => "hour",
    }
}

fn floor_tier(ts_ns: u64, tier: u8) -> u64 {
    let span = if tier == TELEMETRY_TIER_DAY {
        NANOS_PER_DAY
    } else {
        NANOS_PER_HOUR
    };
    ts_ns - (ts_ns % span)
}

fn ceil_tier(ts_ns: u64, tier: u8) -> u64 {
    let span = if tier == TELEMETRY_TIER_DAY {
        NANOS_PER_DAY
    } else {
        NANOS_PER_HOUR
    };
    let remainder = ts_ns % span;
    if remainder == 0 {
        ts_ns
    } else {
        ts_ns.saturating_sub(remainder).saturating_add(span)
    }
}

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

fn now_ns() -> u64 {
    crate::server::agent_events::unix_time_ns_now()
}

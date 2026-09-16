use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rocksdb::{ColumnFamilyRef, DB, IteratorMode};
use synapse_core::{error_codes, retention::DEFAULTS};

use crate::{StorageError, StorageResult};

const GC_INTERVAL: Duration = Duration::from_mins(5);
const MIB: u64 = 1024 * 1024;
const ESTIMATE_LIVE_DATA_SIZE: &str = "rocksdb.estimate-live-data-size";
const ESTIMATE_NUM_KEYS: &str = "rocksdb.estimate-num-keys";
const CACHE_EVICTIONS_TOTAL: &str = "cache_evictions_total";
const SOFT_CAP_REASON: &str = "soft_cap";
const MAX_EVICT_KEYS_PER_CF_PER_PASS: usize = 4096;
const MAX_ROW_CAP_EVICT_PASSES_PER_CF: usize = 8;
const UNSAFE_LEXICAL_EVICTION_REFUSED: &str = "unsafe_lexical_eviction_refused";
const UNSUPPORTED_BYTE_CAP_POLICY_SKIPPED: &str = "unsupported_byte_cap_policy_skipped";

/// One storage GC pass across all configured column families.
#[derive(Debug, Default)]
pub struct GcReport {
    pub cf_reports: Vec<GcCfReport>,
}

impl GcReport {
    /// Total rows evicted by this pass.
    #[must_use]
    pub fn total_evicted_rows(&self) -> u64 {
        self.cf_reports
            .iter()
            .map(|report| report.evicted_rows)
            .sum()
    }

    /// Finds the report for one column family.
    #[must_use]
    pub fn cf(&self, cf_name: &str) -> Option<&GcCfReport> {
        self.cf_reports
            .iter()
            .find(|report| report.cf_name == cf_name)
    }
}

/// Per-column-family GC outcome.
#[derive(Debug)]
pub struct GcCfReport {
    pub cf_name: String,
    pub before_value: u64,
    pub after_value: u64,
    pub before_estimated_num_keys: Option<u64>,
    pub after_estimated_num_keys: Option<u64>,
    pub examined_rows: u64,
    pub scan_limited: bool,
    pub evicted_rows: u64,
    pub eviction_skipped_reason: Option<&'static str>,
    pub hard_cap_reached: bool,
    pub hard_cap_code: Option<&'static str>,
}

#[derive(Clone, Debug, Default)]
pub struct GcTaskReadback {
    pub running: bool,
    pub last_started_unix_ms: Option<u64>,
    pub last_completed_unix_ms: Option<u64>,
    pub last_duration_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_unsupported_policy_skips: Vec<String>,
}

#[derive(Debug, Default)]
struct GcTaskState {
    readback: Mutex<GcTaskReadback>,
}

/// Handle for the periodic storage GC task.
#[derive(Debug)]
pub struct GcTask {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
    state: Arc<GcTaskState>,
}

impl Drop for GcTask {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.handle.abort();
    }
}

impl GcTask {
    #[must_use]
    pub fn readback(&self) -> GcTaskReadback {
        let mut readback = self
            .state
            .readback
            .lock()
            .map_or_else(|_error| GcTaskReadback::default(), |guard| guard.clone());
        readback.running = !self.handle.is_finished();
        readback
    }
}

#[derive(Clone, Debug)]
pub struct GcConfig {
    interval: Duration,
    budgets: Vec<GcBudget>,
    unsupported_byte_cap_budgets: Vec<GcBudget>,
}

impl GcConfig {
    pub fn from_retention_defaults() -> Self {
        let mut budgets = Vec::new();
        let mut unsupported_byte_cap_budgets = Vec::new();
        for default in DEFAULTS {
            let budget = GcBudget {
                cf_name: default.cf,
                soft_cap: default.soft_cap_mb.saturating_mul(MIB),
                hard_cap: default.hard_cap_mb.saturating_mul(MIB),
                unit: CapUnit::Bytes,
            };
            if supports_chronological_eviction(default.cf) {
                budgets.push(budget);
            } else {
                unsupported_byte_cap_budgets.push(budget);
            }
        }
        Self {
            interval: GC_INTERVAL,
            budgets,
            unsupported_byte_cap_budgets,
        }
    }
}

impl GcConfig {
    pub(crate) fn for_row_caps(
        interval: Duration,
        cf_name: &'static str,
        soft_cap: u64,
        hard_cap: u64,
    ) -> Self {
        Self {
            interval,
            budgets: vec![GcBudget {
                cf_name,
                soft_cap,
                hard_cap,
                unit: CapUnit::Rows,
            }],
            unsupported_byte_cap_budgets: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_byte_caps(
        interval: Duration,
        cf_name: &'static str,
        soft_cap: u64,
        hard_cap: u64,
    ) -> Self {
        Self {
            interval,
            budgets: vec![GcBudget {
                cf_name,
                soft_cap,
                hard_cap,
                unit: CapUnit::Bytes,
            }],
            unsupported_byte_cap_budgets: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct GcBudget {
    cf_name: &'static str,
    soft_cap: u64,
    hard_cap: u64,
    unit: CapUnit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapUnit {
    Bytes,
    Rows,
}

pub fn spawn(db: Arc<DB>, config: GcConfig) -> StorageResult<GcTask> {
    let handle =
        tokio::runtime::Handle::try_current().map_err(|error| StorageError::WriteFailed {
            cf_name: "storage_gc".to_owned(),
            detail: error.to_string(),
        })?;
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let state = Arc::new(GcTaskState::default());
    let task_state = Arc::clone(&state);
    let task = handle.spawn(async move {
        let mut interval = tokio::time::interval(config.interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let started = mark_gc_tick_started(&task_state);
                    let result = run_once(&db, &config);
                    mark_gc_tick_completed(&task_state, started, &result);
                    if let Err(error) = result {
                        tracing::warn!(error = %error, "storage GC tick failed");
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });
    Ok(GcTask {
        shutdown: Some(shutdown),
        handle: task,
        state,
    })
}

pub fn run_once(db: &DB, config: &GcConfig) -> StorageResult<GcReport> {
    let mut cf_reports =
        Vec::with_capacity(config.budgets.len() + config.unsupported_byte_cap_budgets.len());
    for budget in &config.budgets {
        cf_reports.push(run_cf(db, *budget)?);
    }
    for budget in &config.unsupported_byte_cap_budgets {
        cf_reports.push(report_skipped_unsupported_byte_cap(db, *budget)?);
    }
    Ok(GcReport { cf_reports })
}

fn run_cf(db: &DB, budget: GcBudget) -> StorageResult<GcCfReport> {
    let cf = cf_handle(db, budget.cf_name)?;
    let before_estimated_num_keys = cf_property(db, &cf, budget.cf_name, ESTIMATE_NUM_KEYS)?;
    let before_measurement = measured_value(db, &cf, budget)?;
    let mut examined_rows = before_measurement.examined_rows;
    let mut scan_limited = before_measurement.scan_limited;
    let before_value = before_measurement.value;
    let hard_cap_reached = before_value >= budget.hard_cap;
    if hard_cap_reached {
        tracing::warn!(
            code = error_codes::STORAGE_CF_HARD_CAP_REACHED,
            cf = budget.cf_name,
            before_value,
            hard_cap = budget.hard_cap,
            "storage column family hard cap reached"
        );
    }

    let mut evicted_rows = 0_u64;
    let mut after_value = before_value;
    let mut after_estimated_num_keys = before_estimated_num_keys;
    if before_value > budget.soft_cap {
        let outcome =
            evict_over_soft_cap(db, &cf, budget, before_value, before_estimated_num_keys)?;
        evicted_rows = outcome.evicted_rows;
        after_value = outcome.after_value;
        after_estimated_num_keys = outcome.after_estimated_num_keys;
        examined_rows = examined_rows.saturating_add(outcome.examined_rows);
        scan_limited |= outcome.scan_limited;
    }

    record_eviction_metric(budget, evicted_rows, before_value, after_value);

    Ok(GcCfReport {
        cf_name: budget.cf_name.to_owned(),
        before_value,
        after_value,
        before_estimated_num_keys,
        after_estimated_num_keys,
        examined_rows,
        scan_limited,
        evicted_rows,
        eviction_skipped_reason: None,
        hard_cap_reached,
        hard_cap_code: hard_cap_reached.then_some(error_codes::STORAGE_CF_HARD_CAP_REACHED),
    })
}

#[derive(Debug)]
struct GcEvictionOutcome {
    evicted_rows: u64,
    after_value: u64,
    after_estimated_num_keys: Option<u64>,
    examined_rows: u64,
    scan_limited: bool,
}

fn evict_over_soft_cap(
    db: &DB,
    cf: &ColumnFamilyRef<'_>,
    budget: GcBudget,
    before_value: u64,
    before_estimated_num_keys: Option<u64>,
) -> StorageResult<GcEvictionOutcome> {
    refuse_unsafe_byte_eviction(budget, before_value)?;

    let mut outcome = GcEvictionOutcome {
        evicted_rows: 0,
        after_value: before_value,
        after_estimated_num_keys: before_estimated_num_keys,
        examined_rows: 0,
        scan_limited: false,
    };
    let max_passes = match budget.unit {
        CapUnit::Bytes => 1,
        CapUnit::Rows => MAX_ROW_CAP_EVICT_PASSES_PER_CF,
    };
    for pass_index in 0..max_passes {
        let planned_remove_count = remove_count(
            budget,
            outcome.after_value,
            outcome.after_estimated_num_keys,
        );
        if planned_remove_count == 0 {
            break;
        }
        let collect_limit = planned_remove_count
            .saturating_add(1)
            .min(MAX_EVICT_KEYS_PER_CF_PER_PASS.saturating_add(1));
        let (keys, more) = collect_oldest_keys(db, cf, budget.cf_name, collect_limit)?;
        outcome.examined_rows = outcome
            .examined_rows
            .saturating_add(usize_to_u64(keys.len()));
        outcome.scan_limited |= more;
        let pass_evicted = evict_oldest(db, cf, budget, &keys, planned_remove_count)?;
        outcome.evicted_rows = outcome.evicted_rows.saturating_add(pass_evicted);
        if pass_evicted == 0 {
            break;
        }

        let after_measurement = measured_value(db, cf, budget)?;
        outcome.examined_rows = outcome
            .examined_rows
            .saturating_add(after_measurement.examined_rows);
        outcome.scan_limited |= after_measurement.scan_limited;
        outcome.after_value = after_measurement.value;
        outcome.after_estimated_num_keys = cf_property(db, cf, budget.cf_name, ESTIMATE_NUM_KEYS)?;

        if budget.unit == CapUnit::Bytes || outcome.after_value <= budget.soft_cap {
            break;
        }
        if pass_index + 1 == max_passes {
            log_bounded_pass_limit_reached(budget, &outcome, max_passes);
        }
    }
    Ok(outcome)
}

fn report_skipped_unsupported_byte_cap(db: &DB, budget: GcBudget) -> StorageResult<GcCfReport> {
    debug_assert_eq!(budget.unit, CapUnit::Bytes);
    debug_assert!(!supports_chronological_eviction(budget.cf_name));
    let cf = cf_handle(db, budget.cf_name)?;
    let before_estimated_num_keys = cf_property(db, &cf, budget.cf_name, ESTIMATE_NUM_KEYS)?;
    let before_value =
        cf_property(db, &cf, budget.cf_name, ESTIMATE_LIVE_DATA_SIZE)?.unwrap_or_default();
    let hard_cap_reached = before_value >= budget.hard_cap;
    tracing::warn!(
        code = "STORAGE_GC_UNSUPPORTED_BYTE_CAP_POLICY_SKIPPED",
        cf = budget.cf_name,
        before_value,
        soft_cap = budget.soft_cap,
        hard_cap = budget.hard_cap,
        hard_cap_reached,
        reason = UNSUPPORTED_BYTE_CAP_POLICY_SKIPPED,
        "storage GC skipped default byte-cap policy because this column family's key order is not an oldest-first retention order; add schema-aware retention before enabling eviction"
    );
    Ok(GcCfReport {
        cf_name: budget.cf_name.to_owned(),
        before_value,
        after_value: before_value,
        before_estimated_num_keys,
        after_estimated_num_keys: before_estimated_num_keys,
        examined_rows: 0,
        scan_limited: false,
        evicted_rows: 0,
        eviction_skipped_reason: Some(UNSUPPORTED_BYTE_CAP_POLICY_SKIPPED),
        hard_cap_reached,
        hard_cap_code: hard_cap_reached.then_some(error_codes::STORAGE_CF_HARD_CAP_REACHED),
    })
}

fn refuse_unsafe_byte_eviction(budget: GcBudget, before_value: u64) -> StorageResult<()> {
    if budget.unit != CapUnit::Bytes || supports_chronological_eviction(budget.cf_name) {
        return Ok(());
    }

    let detail = format!(
        "refusing generic byte-cap eviction for {cf}: key order is not an oldest-first retention order; implement a schema-aware retention index before byte eviction",
        cf = budget.cf_name
    );
    tracing::error!(
        code = error_codes::STORAGE_GC_UNSAFE_EVICTION_REFUSED,
        cf = budget.cf_name,
        before_value,
        soft_cap = budget.soft_cap,
        hard_cap = budget.hard_cap,
        reason = UNSAFE_LEXICAL_EVICTION_REFUSED,
        detail = %detail,
        "refusing generic byte-cap eviction because this column family's key order is not an oldest-first retention order"
    );
    Err(StorageError::UnsafeGcEvictionRefused {
        cf_name: budget.cf_name.to_owned(),
        detail,
    })
}

fn log_bounded_pass_limit_reached(
    budget: GcBudget,
    outcome: &GcEvictionOutcome,
    max_passes: usize,
) {
    tracing::warn!(
        code = "STORAGE_GC_BOUNDED_PASS_LIMIT_REACHED",
        cf = budget.cf_name,
        after_value = outcome.after_value,
        soft_cap = budget.soft_cap,
        hard_cap = budget.hard_cap,
        evicted_rows = outcome.evicted_rows,
        max_passes,
        scan_limited = outcome.scan_limited,
        "storage GC stopped after bounded passes; retry later or use schema-specific retention if the CF remains over cap"
    );
}

fn record_eviction_metric(
    budget: GcBudget,
    evicted_rows: u64,
    before_value: u64,
    after_value: u64,
) {
    if evicted_rows == 0 {
        return;
    }
    synapse_telemetry::metrics::counter!(
        CACHE_EVICTIONS_TOTAL,
        "cf" => budget.cf_name,
        "reason" => SOFT_CAP_REASON
    )
    .increment(evicted_rows);
    tracing::info!(
        code = "STORAGE_CACHE_EVICTIONS_TOTAL_INCREMENTED",
        metric_name = CACHE_EVICTIONS_TOTAL,
        cf = budget.cf_name,
        reason = SOFT_CAP_REASON,
        delta = evicted_rows,
        before_value,
        after_value,
        "storage cache eviction counter incremented"
    );
}

fn evict_oldest(
    db: &DB,
    cf: &ColumnFamilyRef<'_>,
    budget: GcBudget,
    keys: &[Vec<u8>],
    planned_remove_count: usize,
) -> StorageResult<u64> {
    let remove_count = planned_remove_count.min(keys.len());
    if remove_count == 0 {
        return Ok(0);
    }

    let start = keys
        .first()
        .ok_or_else(|| read_failed(budget.cf_name, "missing first key for GC range".to_owned()))?;
    let end = if keys.len() > remove_count {
        keys[remove_count].clone()
    } else {
        keys.last().map_or_else(Vec::new, |last| key_after(last))
    };
    db.delete_range_cf(cf, start, &end)
        .map_err(|error| write_failed(budget.cf_name, error.to_string()))?;
    db.flush_cf(cf)
        .map_err(|error| write_failed(budget.cf_name, error.to_string()))?;
    db.compact_range_cf(cf, None::<&[u8]>, None::<&[u8]>);
    Ok(usize_to_u64(remove_count))
}

fn remove_count(
    budget: GcBudget,
    before_value: u64,
    before_estimated_num_keys: Option<u64>,
) -> usize {
    let count = match budget.unit {
        // Explicit row cap (storage_gc_once / dashboard "cap at N"): evict exactly
        // the overage so the column family lands precisely at `soft_cap`. This is
        // an operator-driven, one-shot action — there is no minimum batch, so
        // capping a 140-row store at 136 removes 4 rows, not a forced quarter.
        CapUnit::Rows => {
            usize::try_from(before_value.saturating_sub(budget.soft_cap)).unwrap_or(usize::MAX)
        }
        // Byte/disk-pressure GC cannot map a byte overage onto an exact row count,
        // so it evicts a bounded 25%-of-estimated-rows batch per pass to amortize
        // compaction across periodic ticks without materializing the whole CF.
        CapUnit::Bytes => before_estimated_num_keys
            .unwrap_or_default()
            .div_ceil(4)
            .try_into()
            .unwrap_or(usize::MAX),
    };
    count.min(MAX_EVICT_KEYS_PER_CF_PER_PASS)
}

#[derive(Clone, Copy, Debug)]
struct Measurement {
    value: u64,
    examined_rows: u64,
    scan_limited: bool,
}

fn measured_value(
    db: &DB,
    cf: &ColumnFamilyRef<'_>,
    budget: GcBudget,
) -> StorageResult<Measurement> {
    match budget.unit {
        CapUnit::Rows => {
            let limit = budget.hard_cap.saturating_add(1);
            let (value, scan_limited) = bounded_count_rows(db, cf, budget.cf_name, limit)?;
            Ok(Measurement {
                value,
                examined_rows: value,
                scan_limited,
            })
        }
        CapUnit::Bytes => {
            cf_property(db, cf, budget.cf_name, ESTIMATE_LIVE_DATA_SIZE).map(|value| Measurement {
                value: value.unwrap_or_default(),
                examined_rows: 0,
                scan_limited: false,
            })
        }
    }
}

fn collect_oldest_keys(
    db: &DB,
    cf: &ColumnFamilyRef<'_>,
    cf_name: &str,
    limit: usize,
) -> StorageResult<(Vec<Vec<u8>>, bool)> {
    let mut keys = Vec::new();
    let mut more = false;
    for item in db.iterator_cf(cf, IteratorMode::Start) {
        let (key, _value) = item.map_err(|error| read_failed(cf_name, error.to_string()))?;
        if keys.len() >= limit {
            more = true;
            break;
        }
        keys.push(key.to_vec());
    }
    Ok((keys, more))
}

fn bounded_count_rows(
    db: &DB,
    cf: &ColumnFamilyRef<'_>,
    cf_name: &str,
    limit: u64,
) -> StorageResult<(u64, bool)> {
    let mut count = 0_u64;
    for item in db.iterator_cf(cf, IteratorMode::Start) {
        let (_key, _value) = item.map_err(|error| read_failed(cf_name, error.to_string()))?;
        if count >= limit {
            return Ok((count, true));
        }
        count = count.saturating_add(1);
    }
    Ok((count, false))
}

fn cf_property(
    db: &DB,
    cf: &ColumnFamilyRef<'_>,
    cf_name: &str,
    property: &str,
) -> StorageResult<Option<u64>> {
    db.property_int_value_cf(cf, property)
        .map_err(|error| read_failed(cf_name, error.to_string()))
}

fn cf_handle<'db>(db: &'db DB, cf_name: &str) -> StorageResult<ColumnFamilyRef<'db>> {
    db.cf_handle(cf_name)
        .ok_or_else(|| read_failed(cf_name, "column family handle missing".to_owned()))
}

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut end = key.to_vec();
    end.push(0);
    end
}

fn supports_chronological_eviction(cf_name: &str) -> bool {
    matches!(
        cf_name,
        crate::cf::CF_EVENTS
            | crate::cf::CF_ACTION_LOG
            | crate::cf::CF_TIMELINE
            | crate::cf::CF_EPISODES
            | crate::cf::CF_AGENT_EVENTS
    )
}

#[derive(Clone, Copy, Debug)]
struct TickStarted {
    unix_ms: u64,
    instant: Instant,
}

fn mark_gc_tick_started(state: &GcTaskState) -> TickStarted {
    let started = TickStarted {
        unix_ms: unix_time_ms_now(),
        instant: Instant::now(),
    };
    if let Ok(mut readback) = state.readback.lock() {
        readback.running = true;
        readback.last_started_unix_ms = Some(started.unix_ms);
    }
    started
}

fn mark_gc_tick_completed(
    state: &GcTaskState,
    started: TickStarted,
    result: &StorageResult<GcReport>,
) {
    if let Ok(mut readback) = state.readback.lock() {
        readback.last_completed_unix_ms = Some(unix_time_ms_now());
        readback.last_duration_ms = Some(duration_millis_u64(started.instant.elapsed()));
        readback.last_error = result.as_ref().err().map(ToString::to_string);
        readback.last_unsupported_policy_skips = result
            .as_ref()
            .ok()
            .map(|report| {
                report
                    .cf_reports
                    .iter()
                    .filter(|cf| {
                        cf.eviction_skipped_reason == Some(UNSUPPORTED_BYTE_CAP_POLICY_SKIPPED)
                    })
                    .map(|cf| cf.cf_name.clone())
                    .collect()
            })
            .unwrap_or_default();
    }
}

fn unix_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn read_failed(cf_name: &str, detail: String) -> StorageError {
    StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail,
    }
}

fn write_failed(cf_name: &str, detail: String) -> StorageError {
    StorageError::WriteFailed {
        cf_name: cf_name.to_owned(),
        detail,
    }
}

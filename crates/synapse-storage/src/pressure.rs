use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rocksdb::DB;
use synapse_core::error_codes;

use crate::{StorageError, StorageResult, cf};

#[cfg(test)]
use std::collections::VecDeque;

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const GB: u64 = 1_000_000_000;
const MB: u64 = 1_000_000;
const LEVEL_1_FREE_BYTES: u64 = 2 * GB;
const LEVEL_2_FREE_BYTES: u64 = GB;
const LEVEL_3_FREE_BYTES: u64 = 500 * MB;
const LEVEL_4_FREE_BYTES: u64 = 200 * MB;
const PRESSURE_CF: &str = "storage_disk_pressure";
const STORAGE_DISK_PRESSURE_LEVEL: &str = "storage_disk_pressure_level";

/// Current DB-volume pressure level.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum DiskPressureLevel {
    Normal = 0,
    Level1 = 1,
    Level2 = 2,
    Level3 = 3,
    Level4 = 4,
}

impl DiskPressureLevel {
    /// Stable storage code emitted when entering this pressure level.
    #[must_use]
    pub const fn code(self) -> Option<&'static str> {
        match self {
            Self::Normal => None,
            Self::Level1 => Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_1),
            Self::Level2 => Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_2),
            Self::Level3 => Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_3),
            Self::Level4 => Some(error_codes::STORAGE_DISK_PRESSURE_LEVEL_4),
        }
    }

    const fn from_u8(level: u8) -> Self {
        match level {
            1 => Self::Level1,
            2 => Self::Level2,
            3 => Self::Level3,
            4 => Self::Level4,
            _ => Self::Normal,
        }
    }
}

/// One disk-pressure poll result.
#[derive(Debug)]
pub struct PressureReport {
    pub free_bytes: u64,
    pub previous_level: DiskPressureLevel,
    pub current_level: DiskPressureLevel,
    pub emitted_code: Option<&'static str>,
    pub compacted_cfs: Vec<&'static str>,
    pub gc_advised: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PressureProbeReadback {
    pub observed: bool,
    pub last_free_bytes: Option<u64>,
    pub last_level: Option<DiskPressureLevel>,
    pub last_started_unix_ms: Option<u64>,
    pub last_completed_unix_ms: Option<u64>,
    pub last_duration_ms: Option<u64>,
    pub last_error: Option<String>,
}

/// Handle for the periodic disk-pressure task.
#[derive(Debug)]
pub struct PressureTask {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for PressureTask {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.handle.abort();
    }
}

impl PressureTask {
    #[must_use]
    pub fn running(&self) -> bool {
        !self.handle.is_finished()
    }
}

#[derive(Clone, Debug)]
pub struct PressureConfig {
    interval: Duration,
    thresholds: PressureThresholds,
}

impl Default for PressureConfig {
    fn default() -> Self {
        Self {
            interval: POLL_INTERVAL,
            thresholds: PressureThresholds {
                level1: LEVEL_1_FREE_BYTES,
                level2: LEVEL_2_FREE_BYTES,
                level3: LEVEL_3_FREE_BYTES,
                level4: LEVEL_4_FREE_BYTES,
            },
        }
    }
}

#[cfg(test)]
impl PressureConfig {
    pub fn with_thresholds(
        interval: Duration,
        level1_free_bytes: u64,
        level2_free_bytes: u64,
        level3_free_bytes: u64,
        level4_free_bytes: u64,
    ) -> Self {
        Self {
            interval,
            thresholds: PressureThresholds {
                level1: level1_free_bytes,
                level2: level2_free_bytes,
                level3: level3_free_bytes,
                level4: level4_free_bytes,
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PressureThresholds {
    level1: u64,
    level2: u64,
    level3: u64,
    level4: u64,
}

impl PressureThresholds {
    const fn level_for(self, free_bytes: u64) -> DiskPressureLevel {
        if free_bytes < self.level4 {
            DiskPressureLevel::Level4
        } else if free_bytes < self.level3 {
            DiskPressureLevel::Level3
        } else if free_bytes < self.level2 {
            DiskPressureLevel::Level2
        } else if free_bytes < self.level1 {
            DiskPressureLevel::Level1
        } else {
            DiskPressureLevel::Normal
        }
    }
}

#[derive(Debug, Default)]
pub struct PressureState {
    level: AtomicU8,
    emitted_codes: Mutex<Vec<&'static str>>,
    probe_readback: Mutex<PressureProbeReadback>,
}

impl PressureState {
    /// Current pressure level.
    #[must_use]
    pub fn level(&self) -> DiskPressureLevel {
        DiskPressureLevel::from_u8(self.level.load(Ordering::SeqCst))
    }

    pub fn transition_codes(&self) -> StorageResult<Vec<&'static str>> {
        self.emitted_codes
            .lock()
            .map(|codes| codes.clone())
            .map_err(|error| read_failed(format!("pressure code lock poisoned: {error}")))
    }

    pub fn probe_readback(&self) -> StorageResult<PressureProbeReadback> {
        self.probe_readback
            .lock()
            .map(|readback| readback.clone())
            .map_err(|error| read_failed(format!("pressure probe lock poisoned: {error}")))
    }

    /// Whether a new write to `cf_name` is accepted at the current level.
    #[must_use]
    pub fn permits_write(&self, cf_name: &str) -> bool {
        permits_write_at(self.level(), cf_name)
    }

    fn transition_to(
        &self,
        next: DiskPressureLevel,
    ) -> StorageResult<(DiskPressureLevel, Option<&'static str>)> {
        let previous = DiskPressureLevel::from_u8(self.level.swap(next as u8, Ordering::SeqCst));
        let emitted_code = (previous != next).then(|| next.code()).flatten();
        if let Some(code) = emitted_code {
            self.emitted_codes
                .lock()
                .map_err(|error| read_failed(format!("pressure code lock poisoned: {error}")))?
                .push(code);
        }
        Ok((previous, emitted_code))
    }
}

pub fn spawn(
    db: Arc<DB>,
    state: Arc<PressureState>,
    path: PathBuf,
    config: PressureConfig,
) -> StorageResult<PressureTask> {
    spawn_with_probe(db, state, path, config, Arc::new(Fs2DiskProbe))
}

pub fn run_once(
    db: &DB,
    state: &PressureState,
    path: &Path,
    config: &PressureConfig,
) -> StorageResult<PressureReport> {
    let started = mark_pressure_probe_started(state);
    let result = Fs2DiskProbe
        .available_space(path)
        .and_then(|free_bytes| apply_free_bytes(db, state, config, free_bytes));
    mark_pressure_probe_completed(state, started, result.as_ref());
    result
}

pub fn run_once_with_free_bytes(
    db: &DB,
    state: &PressureState,
    config: &PressureConfig,
    free_bytes: u64,
) -> StorageResult<PressureReport> {
    let started = mark_pressure_probe_started(state);
    let result = apply_free_bytes(db, state, config, free_bytes);
    mark_pressure_probe_completed(state, started, result.as_ref());
    result
}

#[cfg(test)]
pub fn spawn_with_free_bytes(
    db: Arc<DB>,
    state: Arc<PressureState>,
    path: PathBuf,
    config: PressureConfig,
    values: Vec<u64>,
) -> StorageResult<PressureTask> {
    let fallback = values.last().copied().unwrap_or(u64::MAX);
    spawn_with_probe(
        db,
        state,
        path,
        config,
        Arc::new(SequenceDiskProbe {
            values: Mutex::new(values.into_iter().collect()),
            fallback,
        }),
    )
}

fn spawn_with_probe(
    db: Arc<DB>,
    state: Arc<PressureState>,
    path: PathBuf,
    config: PressureConfig,
    probe: Arc<dyn DiskProbe>,
) -> StorageResult<PressureTask> {
    let handle =
        tokio::runtime::Handle::try_current().map_err(|error| StorageError::WriteFailed {
            cf_name: PRESSURE_CF.to_owned(),
            detail: error.to_string(),
        })?;
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let task = handle.spawn(async move {
        let mut interval = tokio::time::interval(config.interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let started = mark_pressure_probe_started(&state);
                    let result = match probe.available_space(&path) {
                        Ok(free_bytes) => apply_free_bytes(&db, &state, &config, free_bytes),
                        Err(error) => Err(error),
                    };
                    mark_pressure_probe_completed(&state, started, result.as_ref());
                    if let Err(error) = result {
                        tracing::warn!(error = %error, "storage disk-pressure tick failed");
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });
    Ok(PressureTask {
        shutdown: Some(shutdown),
        handle: task,
    })
}

fn apply_free_bytes(
    db: &DB,
    state: &PressureState,
    config: &PressureConfig,
    free_bytes: u64,
) -> StorageResult<PressureReport> {
    let current_level = config.thresholds.level_for(free_bytes);
    synapse_telemetry::metrics::gauge!(STORAGE_DISK_PRESSURE_LEVEL)
        .set(f64::from(current_level as u8));
    let (previous_level, emitted_code) = state.transition_to(current_level)?;
    let transitioned = previous_level != current_level;
    let gc_advised = transitioned && current_level >= DiskPressureLevel::Level1;
    let compacted_cfs = if transitioned && current_level >= DiskPressureLevel::Level2 {
        compact_all(db)?
    } else {
        Vec::new()
    };

    if let Some(code) = emitted_code {
        tracing::warn!(
            code,
            free_bytes,
            previous_level = ?previous_level,
            current_level = ?current_level,
            "storage disk-pressure transition"
        );
    }
    if gc_advised {
        tracing::warn!(
            free_bytes,
            current_level = ?current_level,
            "storage disk pressure advises next GC tick"
        );
    }

    Ok(PressureReport {
        free_bytes,
        previous_level,
        current_level,
        emitted_code,
        compacted_cfs,
        gc_advised,
    })
}

fn compact_all(db: &DB) -> StorageResult<Vec<&'static str>> {
    let mut compacted = Vec::with_capacity(cf::ALL_COLUMN_FAMILIES.len());
    for cf_name in cf::ALL_COLUMN_FAMILIES {
        let handle = db
            .cf_handle(cf_name)
            .ok_or_else(|| read_failed(format!("column family handle missing: {cf_name}")))?;
        db.compact_range_cf(&handle, None::<&[u8]>, None::<&[u8]>);
        compacted.push(cf_name);
    }
    Ok(compacted)
}

fn permits_write_at(level: DiskPressureLevel, cf_name: &str) -> bool {
    match level {
        DiskPressureLevel::Normal | DiskPressureLevel::Level1 | DiskPressureLevel::Level2 => true,
        // Level3 sheds rebuildable/cache CFs first. CF_AGENT_EVENTS stays
        // writable like CF_ACTION_LOG: it is the control-plane audit journal
        // (#897) and losing lifecycle events would blind fleet supervision
        // exactly when the system is degrading.
        // CF_AGENT_TRANSCRIPTS sheds with the rebuildable set: rows are
        // re-ingestable from the spawn log files on disk, and the ingester
        // (#900) gates on `pressure_permits_write` BEFORE advancing its
        // cursor, so a shed cycle is a loud deferral, never silent loss.
        DiskPressureLevel::Level3 => !matches!(
            cf_name,
            cf::CF_OBSERVATIONS
                | cf::CF_OCR_CACHE
                | cf::CF_TELEMETRY
                | cf::CF_MODEL_CACHE
                | cf::CF_PROCESS_HISTORY
                | cf::CF_TIMELINE
                | cf::CF_EPISODES
                | cf::CF_ROUTINES
                | cf::CF_AGENT_TRANSCRIPTS
        ),
        DiskPressureLevel::Level4 => matches!(cf_name, cf::CF_REFLEX_AUDIT | cf::CF_SESSIONS),
    }
}

#[derive(Clone, Copy, Debug)]
struct ProbeStarted {
    unix_ms: u64,
    instant: Instant,
}

fn mark_pressure_probe_started(state: &PressureState) -> ProbeStarted {
    let started = ProbeStarted {
        unix_ms: unix_time_ms_now(),
        instant: Instant::now(),
    };
    if let Ok(mut readback) = state.probe_readback.lock() {
        readback.last_started_unix_ms = Some(started.unix_ms);
    }
    started
}

fn mark_pressure_probe_completed(
    state: &PressureState,
    started: ProbeStarted,
    result: Result<&PressureReport, &StorageError>,
) {
    if let Ok(mut readback) = state.probe_readback.lock() {
        readback.last_completed_unix_ms = Some(unix_time_ms_now());
        readback.last_duration_ms = Some(duration_millis_u64(started.instant.elapsed()));
        match result {
            Ok(report) => {
                readback.observed = true;
                readback.last_free_bytes = Some(report.free_bytes);
                readback.last_level = Some(report.current_level);
                readback.last_error = None;
            }
            Err(error) => {
                readback.last_error = Some(error.to_string());
            }
        }
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

trait DiskProbe: Send + Sync {
    fn available_space(&self, path: &Path) -> StorageResult<u64>;
}

struct Fs2DiskProbe;

impl DiskProbe for Fs2DiskProbe {
    fn available_space(&self, path: &Path) -> StorageResult<u64> {
        fs2::available_space(path).map_err(|error| read_failed(error.to_string()))
    }
}

#[cfg(test)]
struct SequenceDiskProbe {
    values: Mutex<VecDeque<u64>>,
    fallback: u64,
}

#[cfg(test)]
impl DiskProbe for SequenceDiskProbe {
    fn available_space(&self, _path: &Path) -> StorageResult<u64> {
        let mut values = self
            .values
            .lock()
            .map_err(|error| read_failed(format!("pressure sequence lock poisoned: {error}")))?;
        Ok(values.pop_front().unwrap_or(self.fallback))
    }
}

fn read_failed(detail: String) -> StorageError {
    StorageError::ReadFailed {
        cf_name: PRESSURE_CF.to_owned(),
        detail,
    }
}

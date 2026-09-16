use std::{
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Arc, Once,
        mpsc::{self, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime},
};

use thiserror::Error;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter, Layer, Registry, filter::LevelFilter, fmt, layer::SubscriberExt,
    registry::LookupSpan, util::SubscriberInitExt,
};

pub mod metrics;

const DEFAULT_MAX_DIR_BYTES: u64 = 500 * 1024 * 1024;
const DEFAULT_KEEP_DAYS: u32 = 7;
const DEFAULT_GC_INTERVAL: Duration = Duration::from_hours(6);
const GC_INTERVAL_ENV: &str = "SYNAPSE_LOG_GC_INTERVAL_S";
const PAYLOAD_LOG_TARGETS: &[&str] = &["rmcp", "rmcp::service", "rmcp::transport"];

#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    pub log_dir: Option<PathBuf>,
    pub file_level: LevelFilter,
    pub console_level: LevelFilter,
    pub max_dir_bytes: u64,
    pub keep_days: u32,
    /// How often to re-run log-dir GC while the daemon is alive. `None` skips the
    /// background worker entirely (use for short-lived test inits). Defaults to 6 h
    /// for `Default`/`default_with_log_dir`; overridable via `SYNAPSE_LOG_GC_INTERVAL_S`.
    /// `Some(Duration::ZERO)` is treated as "disabled".
    pub gc_interval: Option<Duration>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_dir: None,
            file_level: LevelFilter::INFO,
            console_level: LevelFilter::INFO,
            max_dir_bytes: DEFAULT_MAX_DIR_BYTES,
            keep_days: DEFAULT_KEEP_DAYS,
            gc_interval: Some(DEFAULT_GC_INTERVAL),
        }
    }
}

impl TelemetryConfig {
    #[must_use]
    pub fn default_with_log_dir(log_dir: PathBuf) -> Self {
        Self {
            log_dir: Some(log_dir),
            ..Self::default()
        }
    }
}

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("TELEMETRY_LOG_DIR_NOT_WRITABLE: {0} ({1})")]
    LogDirNotWritable(PathBuf, String),
    #[error("TELEMETRY_SUBSCRIBER_INIT_FAILED: {0}")]
    SubscriberInit(String),
    #[error("TELEMETRY_METRICS_RECORDER_FAILED: {0}")]
    MetricsRecorder(String),
    #[error("TELEMETRY_GC_FAILED: {0}")]
    Gc(String),
}

impl TelemetryError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::LogDirNotWritable(_, _) => "TELEMETRY_LOG_DIR_NOT_WRITABLE",
            Self::SubscriberInit(_) => "TELEMETRY_SUBSCRIBER_INIT_FAILED",
            Self::MetricsRecorder(_) => "TELEMETRY_METRICS_RECORDER_FAILED",
            Self::Gc(_) => "TELEMETRY_GC_FAILED",
        }
    }
}

#[derive(Debug)]
pub struct TelemetryGuard {
    _file_guard: WorkerGuard,
    _gc_worker: Option<GcWorker>,
}

/// Background thread that re-runs `run_log_gc` on a fixed interval. Cleanly
/// shuts down when the parent `TelemetryGuard` drops (channel disconnect →
/// `recv_timeout` returns `Disconnected` and the loop breaks).
#[derive(Debug)]
struct GcWorker {
    shutdown: Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl GcWorker {
    fn spawn(
        log_dir: PathBuf,
        keep_days: u32,
        max_dir_bytes: u64,
        interval: Duration,
    ) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("synapse-telemetry-gc".into())
            .spawn(move || {
                loop {
                    match rx.recv_timeout(interval) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            if let Err(err) = run_log_gc(&log_dir, keep_days, max_dir_bytes) {
                                tracing::warn!(
                                    code = "TELEMETRY_GC_PERIODIC_FAILED",
                                    log_dir = %log_dir.display(),
                                    err = %err,
                                    "periodic log GC failed"
                                );
                            } else {
                                tracing::debug!(
                                    code = "TELEMETRY_GC_PERIODIC_OK",
                                    log_dir = %log_dir.display(),
                                    "periodic log GC completed"
                                );
                            }
                        }
                    }
                }
            })
            .ok()?;
        Some(Self {
            shutdown: tx,
            handle: Some(handle),
        })
    }
}

impl Drop for GcWorker {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[must_use]
pub fn json_layer<S>() -> impl Layer<S> + Send + Sync + 'static
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
{
    fmt::layer()
        .json()
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_current_span(true)
        .with_span_list(true)
}

#[must_use]
pub fn console_layer<S>() -> impl Layer<S> + Send + Sync + 'static
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
{
    fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_thread_ids(false)
        .with_ansi(false)
}

/// Initialize the process-wide tracing subscriber.
///
/// # Errors
///
/// Returns a [`TelemetryError`] when the log directory cannot be created,
/// opened, or when another global tracing subscriber is already installed.
pub fn init() -> Result<TelemetryGuard, TelemetryError> {
    init_tracing(TelemetryConfig::default())
}

/// Initialize the process-wide tracing subscriber with explicit configuration.
///
/// # Errors
///
/// Returns a [`TelemetryError`] when the log directory cannot be created,
/// opened, garbage collection fails, or when another global tracing subscriber
/// is already installed.
pub fn init_tracing(cfg: TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    let log_dir = cfg.log_dir.unwrap_or_else(default_log_dir);
    prepare_log_dir(&log_dir)?;
    run_log_gc(&log_dir, cfg.keep_days, cfg.max_dir_bytes).map_err(TelemetryError::Gc)?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "synapse.log");
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);
    let file_filter = payload_safe_filter(level_directive(cfg.file_level));
    let file_layer = fmt::layer()
        .json()
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_current_span(true)
        .with_span_list(true)
        .with_writer(file_writer)
        .with_filter(file_filter);

    let env_filter = payload_safe_filter(
        &env::var("RUST_LOG").unwrap_or_else(|_| level_directive(cfg.console_level).to_owned()),
    );
    let console_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_thread_ids(false)
        .with_ansi(false)
        .with_filter(env_filter);

    Registry::default()
        .with(file_layer)
        .with(console_layer)
        .try_init()
        .map_err(|err| TelemetryError::SubscriberInit(err.to_string()))?;

    install_panic_hook();
    let _metrics_handle = metrics::install_prometheus_recorder()
        .map_err(|error| TelemetryError::MetricsRecorder(error.to_string()))?;
    metrics::register_m3_metrics();

    let gc_interval = effective_gc_interval(cfg.gc_interval);
    let gc_worker = gc_interval.and_then(|interval| {
        GcWorker::spawn(log_dir.clone(), cfg.keep_days, cfg.max_dir_bytes, interval)
    });

    Ok(TelemetryGuard {
        _file_guard: file_guard,
        _gc_worker: gc_worker,
    })
}

fn payload_safe_filter(base_directives: &str) -> EnvFilter {
    let mut directives = base_directives.trim().to_owned();
    if directives.is_empty() {
        directives.push_str("info");
    }
    let dependency_level = payload_dependency_level(default_level_from_directives(&directives));
    for target in PAYLOAD_LOG_TARGETS {
        directives.push(',');
        directives.push_str(target);
        directives.push('=');
        directives.push_str(dependency_level);
    }
    EnvFilter::try_new(&directives).unwrap_or_else(|_| EnvFilter::new("info,rmcp=info"))
}

fn default_level_from_directives(directives: &str) -> LevelFilter {
    directives
        .split(',')
        .find_map(|directive| {
            let trimmed = directive.trim();
            if trimmed.is_empty() || trimmed.contains('=') {
                return None;
            }
            trimmed.parse::<LevelFilter>().ok()
        })
        .unwrap_or(LevelFilter::INFO)
}

const fn payload_dependency_level(default: LevelFilter) -> &'static str {
    match default {
        LevelFilter::OFF => "off",
        LevelFilter::ERROR => "error",
        LevelFilter::WARN => "warn",
        LevelFilter::INFO | LevelFilter::DEBUG | LevelFilter::TRACE => "info",
    }
}

const fn level_directive(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::OFF => "off",
        LevelFilter::ERROR => "error",
        LevelFilter::WARN => "warn",
        LevelFilter::INFO => "info",
        LevelFilter::DEBUG => "debug",
        LevelFilter::TRACE => "trace",
    }
}

/// Pick the effective GC interval: explicit `Some(non-zero)` wins, otherwise the
/// `SYNAPSE_LOG_GC_INTERVAL_S` env var overrides at runtime. `Some(ZERO)` or env
/// value `0` disables periodic GC entirely.
fn effective_gc_interval(configured: Option<Duration>) -> Option<Duration> {
    let env_override = env::var(GC_INTERVAL_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let candidate = env_override.or(configured)?;
    if candidate.is_zero() {
        None
    } else {
        Some(candidate)
    }
}

static PANIC_HOOK_INSTALLED: Once = Once::new();

/// Install a panic hook that forwards panic payload + location to `tracing`
/// (which lands in the rotated log file) before delegating to the existing hook.
/// Idempotent — repeated calls are a no-op.
pub fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        let previous = Arc::new(previous);
        std::panic::set_hook(Box::new(move |info| {
            let payload = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_owned());
            let location = info.location().map_or_else(
                || "<unknown>".to_owned(),
                |loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
            );
            tracing::error!(
                code = "TELEMETRY_PANIC_HOOK_FIRED",
                panic_payload = %payload,
                panic_location = %location,
                "process panic captured by synapse-telemetry hook"
            );
            (previous)(info);
        }));
    });
}

#[must_use]
pub fn default_log_dir() -> PathBuf {
    if cfg!(windows) {
        return env::var_os("LOCALAPPDATA")
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
            .join("synapse")
            .join("logs");
    }

    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".synapse-state"))
        .join("synapse")
        .join("logs")
}

fn prepare_log_dir(log_dir: &Path) -> Result<(), TelemetryError> {
    fs::create_dir_all(log_dir).map_err(|error| {
        TelemetryError::LogDirNotWritable(log_dir.to_path_buf(), format!("create_dir_all: {error}"))
    })?;
    // Probe filename must be unique per process: concurrent daemons share
    // this directory, and a fixed name makes one daemon's create/remove race
    // another's, killing it at startup with a phantom "not writable".
    let probe = log_dir.join(format!(".synapse-write-probe-{}", std::process::id()));
    File::create(&probe).map_err(|error| {
        TelemetryError::LogDirNotWritable(log_dir.to_path_buf(), format!("probe create: {error}"))
    })?;
    fs::remove_file(probe).map_err(|error| {
        TelemetryError::LogDirNotWritable(log_dir.to_path_buf(), format!("probe remove: {error}"))
    })
}

fn run_log_gc(log_dir: &Path, keep_days: u32, max_dir_bytes: u64) -> Result<(), String> {
    let keep = Duration::from_secs(u64::from(keep_days) * 24 * 60 * 60);
    let now = SystemTime::now();
    let mut entries = Vec::new();

    for entry in fs::read_dir(log_dir).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let metadata = entry.metadata().map_err(|err| err.to_string())?;
        if !metadata.is_file() {
            continue;
        }

        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if now.duration_since(modified).unwrap_or_default() > keep {
            fs::remove_file(entry.path()).map_err(|err| err.to_string())?;
            continue;
        }

        entries.push((entry.path(), modified, metadata.len()));
    }

    let mut total: u64 = entries.iter().map(|(_, _, len)| *len).sum();
    if total <= max_dir_bytes {
        return Ok(());
    }

    entries.sort_by_key(|(_, modified, _)| *modified);
    for (path, _, len) in entries {
        fs::remove_file(path).map_err(|err| err.to_string())?;
        total = total.saturating_sub(len);
        if total <= max_dir_bytes {
            break;
        }
    }

    Ok(())
}

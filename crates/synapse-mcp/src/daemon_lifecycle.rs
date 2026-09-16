use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, bail};
use fs2::FileExt as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use synapse_core::SubsystemHealth;

const SCHEMA_VERSION: u32 = 1;
const RUN_CURRENT_FILE: &str = "daemon-run-current.json";
const TOOL_LAST_FILE: &str = "daemon-tool-last.json";
const TOOL_EVENTS_FILE: &str = "daemon-tool-events.jsonl";
const EXIT_EVENTS_FILE: &str = "daemon-exit.jsonl";
const LIFECYCLE_LOCK_FILE: &str = "daemon-lifecycle.lock";

/// Maximum size in bytes the active daemon tool-event ledger
/// (`daemon-tool-events.jsonl`) may reach before it is rotated to a numbered
/// segment. Set to 8 MiB: small enough that a single segment opens and scans
/// quickly, large enough that rotation stays rare on the hot append path.
///
/// Before this cap existed the ledger grew unbounded (~141 MiB in five weeks);
/// segmented rotation plus [`MAX_LEDGER_SEGMENTS`] now bounds total disk usage.
const MAX_LEDGER_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum number of rotated tool-event segments retained on disk
/// (`daemon-tool-events.jsonl.1` .. `.5`, newest suffix `.1`). Older segments
/// are pruned during rotation, so total retained ledger bytes are bounded by
/// roughly `MAX_LEDGER_SEGMENT_BYTES * (MAX_LEDGER_SEGMENTS + 1)`.
const MAX_LEDGER_SEGMENTS: usize = 5;
const MAX_RETAINED_LEDGER_FILES: usize = MAX_LEDGER_SEGMENTS + 1;

static STATE: OnceLock<Mutex<Option<DaemonLifecycleState>>> = OnceLock::new();
static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

#[cfg(test)]
static TEST_MAX_LEDGER_SEGMENT_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Debug)]
pub(crate) struct DaemonLifecycleConfig {
    pub mode: &'static str,
    pub bind_addr: Option<String>,
    pub db_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "explicit source-of-truth path names make health and error evidence unambiguous"
)]
pub(crate) struct DaemonLifecyclePaths {
    pub db_path: String,
    pub run_current_path: String,
    pub tool_last_path: String,
    pub tool_events_path: String,
    pub exit_events_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RunRecord {
    schema_version: u32,
    run_id: String,
    pid: u32,
    mode: String,
    bind_addr: Option<String>,
    db_path: String,
    started_at_unix_ms: u64,
    ended_at_unix_ms: Option<u64>,
    ended_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ToolCallStart {
    pub tool: String,
    pub operation: Option<String>,
    pub route_id: Option<String>,
    pub profile: Option<String>,
    pub tool_surface_sha256: Option<String>,
    pub tool_profile_read_error: Option<Value>,
    pub mcp_session_id: Option<String>,
    pub audit_context: Option<Value>,
    pub audit_context_read_error: Option<Value>,
    pub foreground: Option<Value>,
    pub foreground_read_error: Option<Value>,
    pub session_target: Option<Value>,
    pub session_target_read_error: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InFlightToolCallRead {
    pub seq: u64,
    pub tool: String,
    pub mcp_session_id: Option<String>,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolUsageAggregate {
    pub tool: String,
    pub operation: Option<String>,
    pub route_id: Option<String>,
    pub profile: Option<String>,
    pub tool_surface_sha256: Option<String>,
    pub calls_total: u64,
    pub ok_total: u64,
    pub error_total: u64,
    pub panic_total: u64,
    pub total_duration_ms: u64,
    pub max_duration_ms: u64,
    pub latest_status: String,
    pub latest_error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolUsageTelemetry {
    pub source_of_truth: String,
    pub max_rows: usize,
    pub rows_scanned: usize,
    pub segment_count: usize,
    pub aggregates: Vec<ToolUsageAggregate>,
    pub read_error: Option<String>,
}

type ToolUsageKey = (String, Option<String>, Option<String>, Option<String>);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ToolEvent {
    schema_version: u32,
    run_id: String,
    pid: u32,
    seq: u64,
    event_kind: String,
    tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_surface_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_profile_read_error: Option<Value>,
    status: String,
    started_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_context: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_context_read_error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    foreground: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    foreground_read_error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_target: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_target_read_error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_target: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    panic: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExitEvent {
    schema_version: u32,
    run_id: String,
    pid: u32,
    event_kind: String,
    cause: String,
    detail: Value,
    recorded_at_unix_ms: u64,
    run: Option<RunRecord>,
    last_tool_event: Option<ToolEvent>,
    in_flight_tool_events: Vec<ToolEvent>,
    paths: DaemonLifecyclePaths,
}

#[derive(Clone, Debug)]
struct DaemonLifecycleState {
    run: RunRecord,
    paths: DaemonLifecyclePaths,
    in_flight: BTreeMap<u64, ToolEvent>,
    seq: u64,
    last_error: Option<String>,
    /// Current byte size of the active `daemon-tool-events.jsonl` segment,
    /// tracked in memory so the append hot path never stats the file. Seeded
    /// from the existing file size at [`configure`] and updated after each
    /// append and reset to zero on rotation.
    tool_events_bytes: u64,
    /// Current byte size of the active `daemon-exit.jsonl` segment. Exit events
    /// share the same bounded JSONL ledger implementation as tool events so
    /// daemon lifecycle diagnostics cannot grow without retention.
    exit_events_bytes: u64,
    /// Size cap the active tool-event segment may reach before rotation. Seeded
    /// from [`MAX_LEDGER_SEGMENT_BYTES`]; overridable only in tests via
    /// [`set_max_segment_bytes_for_test`] to force rotation without writing MiB.
    max_segment_bytes: u64,
}

#[derive(Clone, Debug)]
struct LedgerSource {
    path: PathBuf,
    suffix: Option<usize>,
}

#[derive(Clone, Debug)]
struct StagedLedgerSegment {
    path: PathBuf,
    bytes: u64,
    records: u64,
    oversized_records: u64,
}

#[derive(Debug)]
struct LedgerRewrite {
    segments: Vec<StagedLedgerSegment>,
    source_bytes: u64,
    source_records: u64,
    missing_newline_repairs: u64,
}

#[derive(Debug)]
pub(crate) struct ToolCallGuard {
    run_id: String,
    seq: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ContextEvent {
    pub event_kind: &'static str,
    pub tool: &'static str,
    pub status: &'static str,
    pub mcp_session_id: Option<String>,
    pub foreground: Option<Value>,
    pub foreground_read_error: Option<Value>,
    pub detail: Value,
}

/// Holds the lifecycle transaction and in-process lifecycle state across the
/// exact daemon-lifetime-lock release boundary. A successor may acquire the
/// daemon lock once the caller closes it, but its lifecycle `configure` call
/// cannot inspect `daemon-run-current.json` until this guard has durably
/// published the predecessor's graceful exit and released the transaction.
pub(crate) struct GracefulExitFinalizationGuard {
    // Keep the ledger field first: on unwind its Drop unlocks the cross-process
    // transaction before the state mutex is released to another local writer.
    ledger: LifecycleLedgerLock,
    state: MutexGuard<'static, Option<DaemonLifecycleState>>,
}

struct LifecycleLedgerLock {
    file: Option<File>,
    path: PathBuf,
    operation: &'static str,
}

impl LifecycleLedgerLock {
    fn acquire(db_path: &Path, operation: &'static str) -> anyhow::Result<Self> {
        fs::create_dir_all(db_path)
            .with_context(|| format!("create lifecycle lock directory {}", db_path.display()))?;
        let path = db_path.join(LIFECYCLE_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open lifecycle transaction lock {}", path.display()))?;
        file.lock_exclusive().with_context(|| {
            format!(
                "lock lifecycle transaction {} for {operation}",
                path.display()
            )
        })?;
        Ok(Self {
            file: Some(file),
            path,
            operation,
        })
    }

    fn unlock_checked(&mut self) -> anyhow::Result<()> {
        let Some(file) = self.file.as_ref() else {
            return Ok(());
        };
        fs2::FileExt::unlock(file).with_context(|| {
            format!(
                "unlock lifecycle transaction {} after {}",
                self.path.display(),
                self.operation
            )
        })?;
        self.file = None;
        Ok(())
    }
}

impl Drop for LifecycleLedgerLock {
    fn drop(&mut self) {
        if self.file.is_none() {
            return;
        }
        if let Err(error) = self.unlock_checked() {
            tracing::error!(
                code = "MCP_DAEMON_LIFECYCLE_LOCK_DROP_FAILED",
                operation = self.operation,
                lock_path = %self.path.display(),
                error = %error,
                "failed to unlock daemon lifecycle transaction during Drop; closing the owned file handle as the final OS-lock backstop"
            );
            eprintln!(
                "synapse-mcp daemon lifecycle lock cleanup failed: operation={} path={} error={error:#}",
                self.operation,
                self.path.display()
            );
        }
    }
}

fn combine_lifecycle_action_and_unlock<T>(
    operation: &'static str,
    action_result: anyhow::Result<T>,
    unlock_result: anyhow::Result<()>,
) -> anyhow::Result<T> {
    match (action_result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_value), Err(unlock_error)) => Err(unlock_error),
        (Err(error), Err(unlock_error)) => Err(anyhow::anyhow!(
            "{operation} failed: {error:#}; lifecycle transaction unlock also failed: {unlock_error:#}"
        )),
    }
}

fn with_lifecycle_ledger_lock<T>(
    db_path: &Path,
    operation: &'static str,
    action: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut lock = LifecycleLedgerLock::acquire(db_path, operation)?;
    let action_result = action();
    let unlock_result = lock.unlock_checked();
    combine_lifecycle_action_and_unlock(operation, action_result, unlock_result)
}

pub(crate) fn configure(config: DaemonLifecycleConfig) -> anyhow::Result<DaemonLifecyclePaths> {
    fs::create_dir_all(&config.db_path).with_context(|| {
        format!(
            "create daemon lifecycle db directory {}",
            config.db_path.display()
        )
    })?;
    let paths = DaemonLifecyclePaths {
        db_path: config.db_path.display().to_string(),
        run_current_path: config.db_path.join(RUN_CURRENT_FILE).display().to_string(),
        tool_last_path: config.db_path.join(TOOL_LAST_FILE).display().to_string(),
        tool_events_path: config.db_path.join(TOOL_EVENTS_FILE).display().to_string(),
        exit_events_path: config.db_path.join(EXIT_EVENTS_FILE).display().to_string(),
    };

    let run = RunRecord {
        schema_version: SCHEMA_VERSION,
        run_id: format!(
            "{}-{}-{}",
            now_unix_ms(),
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ),
        pid: std::process::id(),
        mode: config.mode.to_owned(),
        bind_addr: config.bind_addr,
        db_path: paths.db_path.clone(),
        started_at_unix_ms: now_unix_ms(),
        ended_at_unix_ms: None,
        ended_reason: None,
    };
    let max_segment_bytes = configured_max_segment_bytes();
    let (tool_events_bytes, exit_events_bytes) = with_lifecycle_ledger_lock(
        &config.db_path,
        "configure daemon lifecycle",
        || {
            let tool_events_bytes = reconcile_jsonl_ledger(
                Path::new(&paths.tool_events_path),
                max_segment_bytes,
                "tool_events",
            )
            .with_context(|| {
                format!(
                    "reconcile daemon tool-event ledger {}",
                    paths.tool_events_path
                )
            })?;
            let mut exit_events_bytes = reconcile_jsonl_ledger(
                Path::new(&paths.exit_events_path),
                max_segment_bytes,
                "exit_events",
            )
            .with_context(|| format!("reconcile daemon exit ledger {}", paths.exit_events_path))?;
            let previous_run = read_optional_json::<RunRecord>(Path::new(&paths.run_current_path))
                .with_context(|| {
                    format!(
                        "read daemon lifecycle current run {}",
                        paths.run_current_path
                    )
                })?;
            let previous_last_tool = read_optional_json::<ToolEvent>(Path::new(
                &paths.tool_last_path,
            ))
            .with_context(|| format!("read daemon lifecycle last tool {}", paths.tool_last_path))?;

            if let Some(previous) = previous_run.as_ref()
                && previous.ended_at_unix_ms.is_none()
            {
                append_bounded_json_line(
                Path::new(&paths.exit_events_path),
                &ExitEvent {
                    schema_version: SCHEMA_VERSION,
                    run_id: previous.run_id.clone(),
                    pid: previous.pid,
                    event_kind: "previous_run_unclean".to_owned(),
                    cause: "process_missing_on_startup".to_owned(),
                    detail: json!({
                        "new_pid": std::process::id(),
                        "new_run_id": run.run_id.clone(),
                        "reason": "daemon-run-current had no ended_at_unix_ms when this daemon acquired the DB lock",
                    }),
                    recorded_at_unix_ms: now_unix_ms(),
                    run: Some(previous.clone()),
                    last_tool_event: previous_last_tool.clone(),
                    in_flight_tool_events: previous_last_tool
                        .iter()
                        .filter(|event| event.status == "started")
                        .cloned()
                        .collect(),
                    paths: paths.clone(),
                },
                &mut exit_events_bytes,
                max_segment_bytes,
                "exit_events",
            )
            .with_context(|| {
                format!(
                    "append previous unclean daemon exit event {}",
                    paths.exit_events_path
                )
            })?;
            }

            write_json_atomic(Path::new(&paths.run_current_path), &run)
                .with_context(|| format!("write daemon current run {}", paths.run_current_path))?;
            Ok((tool_events_bytes, exit_events_bytes))
        },
    )?;
    let state = DaemonLifecycleState {
        run,
        paths: paths.clone(),
        in_flight: BTreeMap::new(),
        seq: 0,
        last_error: None,
        tool_events_bytes,
        exit_events_bytes,
        max_segment_bytes,
    };
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    *guard = Some(state);
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_CONFIGURED",
        run_current_path = %paths.run_current_path,
        tool_last_path = %paths.tool_last_path,
        tool_events_path = %paths.tool_events_path,
        exit_events_path = %paths.exit_events_path,
        "daemon lifecycle ledger configured"
    );
    Ok(paths)
}

pub(crate) fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Err(error) = record_panic(info) {
                eprintln!("synapse-mcp daemon lifecycle panic record failed: {error:#}");
            }
            previous(info);
        }));
    });
}

pub(crate) fn begin_tool_call(start: ToolCallStart) -> anyhow::Result<ToolCallGuard> {
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
        bail!("daemon lifecycle ledger is not configured");
    };
    state.seq = state.seq.saturating_add(1);
    let seq = state.seq;
    let event = ToolEvent {
        schema_version: SCHEMA_VERSION,
        run_id: state.run.run_id.clone(),
        pid: state.run.pid,
        seq,
        event_kind: "tool_call".to_owned(),
        tool: start.tool,
        operation: start.operation,
        route_id: start.route_id,
        profile: start.profile,
        tool_surface_sha256: start.tool_surface_sha256,
        tool_profile_read_error: start.tool_profile_read_error,
        status: "started".to_owned(),
        started_at_unix_ms: now_unix_ms(),
        finished_at_unix_ms: None,
        duration_ms: None,
        mcp_session_id: start.mcp_session_id,
        audit_context: start.audit_context,
        audit_context_read_error: start.audit_context_read_error,
        foreground: start.foreground,
        foreground_read_error: start.foreground_read_error,
        session_target: start.session_target,
        session_target_read_error: start.session_target_read_error,
        effective_target: None,
        error: None,
        panic: None,
        detail: None,
    };
    let mut started_event = event.clone();
    started_event.audit_context = None;
    started_event.audit_context_read_error = None;
    started_event.foreground = None;
    started_event.foreground_read_error = None;
    started_event.session_target = None;
    started_event.session_target_read_error = None;
    write_tool_event(state, &started_event)?;
    state.in_flight.insert(seq, event);
    Ok(ToolCallGuard {
        run_id: state.run.run_id.clone(),
        seq: Some(seq),
    })
}

pub(crate) fn record_context_event(input: ContextEvent) -> anyhow::Result<u64> {
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
        bail!("daemon lifecycle ledger is not configured");
    };
    state.seq = state.seq.saturating_add(1);
    let seq = state.seq;
    let recorded_at_unix_ms = now_unix_ms();
    let event = ToolEvent {
        schema_version: SCHEMA_VERSION,
        run_id: state.run.run_id.clone(),
        pid: state.run.pid,
        seq,
        event_kind: input.event_kind.to_owned(),
        tool: input.tool.to_owned(),
        operation: None,
        route_id: None,
        profile: None,
        tool_surface_sha256: None,
        tool_profile_read_error: None,
        status: input.status.to_owned(),
        started_at_unix_ms: recorded_at_unix_ms,
        finished_at_unix_ms: Some(recorded_at_unix_ms),
        duration_ms: Some(0),
        mcp_session_id: input.mcp_session_id,
        audit_context: None,
        audit_context_read_error: None,
        foreground: input.foreground,
        foreground_read_error: input.foreground_read_error,
        session_target: None,
        session_target_read_error: None,
        effective_target: None,
        error: None,
        panic: None,
        detail: Some(input.detail),
    };
    write_tool_event(state, &event)?;
    Ok(seq)
}

impl ToolCallGuard {
    pub(crate) fn finish_ok_with_effective_target(
        mut self,
        effective_target: Option<Value>,
    ) -> anyhow::Result<()> {
        self.finish("ok", None, None, effective_target)
    }

    pub(crate) fn finish_error(mut self, error: Value) -> anyhow::Result<()> {
        self.finish("error", Some(error), None, None)
    }

    pub(crate) fn finish_error_with_effective_target(
        mut self,
        error: Value,
        effective_target: Option<Value>,
    ) -> anyhow::Result<()> {
        self.finish("error", Some(error), None, effective_target)
    }

    pub(crate) fn finish_panic(mut self, panic: Value) -> anyhow::Result<()> {
        self.finish("panic", None, Some(panic), None)
    }

    fn finish(
        &mut self,
        status: &'static str,
        error: Option<Value>,
        panic: Option<Value>,
        effective_target: Option<Value>,
    ) -> anyhow::Result<()> {
        let seq = self
            .seq
            .ok_or_else(|| anyhow::anyhow!("daemon lifecycle tool guard is already terminal"))?;
        let result = finish_tool_call(&self.run_id, seq, status, error, panic, effective_target);
        if result.is_ok() {
            self.seq = None;
        }
        result
    }
}

impl Drop for ToolCallGuard {
    fn drop(&mut self) {
        let Some(seq) = self.seq.take() else {
            return;
        };
        let run_id = self.run_id.clone();
        let fallback = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            finish_tool_call(
                &run_id,
                seq,
                "error",
                Some(json!({
                    "code": synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "detail_code": "MCP_TOOL_CALL_GUARD_DROPPED_UNFINISHED",
                    "detail": "the routed MCP call owner was dropped before explicit lifecycle finalization",
                    "source_of_truth": "daemon lifecycle ToolCallGuard Drop backstop",
                })),
                None,
                None,
            )
        }));
        match fallback {
            Ok(Ok(())) => {
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    detail_code = "MCP_TOOL_CALL_GUARD_DROPPED_UNFINISHED",
                    run_id,
                    seq,
                    "an unfinished MCP tool lifecycle owner was finalized by its Drop backstop"
                );
            }
            Ok(Err(error)) => {
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    detail_code = "MCP_TOOL_CALL_GUARD_DROP_FINALIZATION_FAILED",
                    run_id,
                    seq,
                    error = %error,
                    "an unfinished MCP tool lifecycle owner could not publish its Drop backstop"
                );
                eprintln!(
                    "synapse-mcp unfinished tool lifecycle cleanup failed: run_id={run_id} seq={seq} error={error:#}"
                );
            }
            Err(payload) => {
                let detail = consume_panic_payload(payload);
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    detail_code = "MCP_TOOL_CALL_GUARD_DROP_PANICKED",
                    run_id,
                    seq,
                    detail,
                    "an unfinished MCP tool lifecycle Drop backstop panicked"
                );
                eprintln!(
                    "synapse-mcp unfinished tool lifecycle cleanup panicked: run_id={run_id} seq={seq} detail={detail}"
                );
            }
        }
    }
}

pub(crate) fn begin_graceful_exit_finalization() -> anyhow::Result<GracefulExitFinalizationGuard> {
    let state = state_slot()
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let configured = state
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("daemon lifecycle ledger is not configured"))?;
    let ledger = LifecycleLedgerLock::acquire(
        Path::new(&configured.paths.db_path),
        "finalize graceful daemon exit across lifetime-lock release",
    )?;
    Ok(GracefulExitFinalizationGuard { ledger, state })
}

pub(crate) fn record_graceful_exit_after_lifetime_lock_close(
    mut finalization: GracefulExitFinalizationGuard,
    source: &'static str,
) -> anyhow::Result<()> {
    let action_result = finalization
        .state
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("daemon lifecycle ledger became unconfigured"))
        .and_then(|state| {
            record_exit_for_state_locked(
                state,
                "daemon_exit",
                "graceful",
                json!({
                    "source": source,
                }),
            )
        });
    let unlock_result = finalization.ledger.unlock_checked();
    combine_lifecycle_action_and_unlock(
        "finalize graceful daemon exit across lifetime-lock release",
        action_result,
        unlock_result,
    )
}

pub(crate) fn record_startup_exit(cause: &'static str, detail: Value) -> anyhow::Result<()> {
    record_exit("daemon_exit", cause, detail)
}

pub(crate) fn record_top_level_error(detail: &str) -> anyhow::Result<()> {
    record_exit(
        "daemon_exit",
        "top_level_error",
        json!({
            "error": detail,
        }),
    )
}

pub(crate) fn health_subsystem() -> SubsystemHealth {
    let slot = state_slot();
    let guard = match slot.lock() {
        Ok(guard) => guard,
        Err(_error) => {
            return SubsystemHealth {
                status: "error".to_owned(),
                detail: Some("daemon lifecycle state lock poisoned".to_owned()),
                ..SubsystemHealth::default()
            };
        }
    };
    let Some(state) = guard.as_ref() else {
        return SubsystemHealth {
            status: "not_configured".to_owned(),
            detail: Some("daemon lifecycle ledger not configured in this process".to_owned()),
            ..SubsystemHealth::default()
        };
    };
    let status = if state.last_error.is_some() {
        "error"
    } else {
        "ok"
    };
    SubsystemHealth {
        status: status.to_owned(),
        detail: Some(health_detail_for_state(state)),
        ..SubsystemHealth::default()
    }
}

pub(crate) fn diagnostic_value() -> Value {
    let slot = state_slot();
    let Ok(guard) = slot.lock() else {
        return json!({
            "status": "error",
            "detail": "daemon lifecycle state lock poisoned",
        });
    };
    match guard.as_ref() {
        Some(state) => {
            let tool_ledger = ledger_diagnostic_value(
                Path::new(&state.paths.tool_events_path),
                state.max_segment_bytes,
                "tool_events",
            );
            let exit_ledger = ledger_diagnostic_value(
                Path::new(&state.paths.exit_events_path),
                state.max_segment_bytes,
                "exit_events",
            );
            json!({
                "status": if state.last_error.is_some() { "error" } else { "ok" },
                "run_id": state.run.run_id,
                "pid": state.run.pid,
                "paths": state.paths,
                "last_error": state.last_error,
                "in_flight_count": state.in_flight.len(),
                "ledgers": {
                    "tool_events": tool_ledger,
                    "exit_events": exit_ledger,
                },
            })
        }
        None => json!({
            "status": "not_configured",
            "detail": "daemon lifecycle ledger not configured in this process",
        }),
    }
}

pub(crate) fn current_paths() -> Option<DaemonLifecyclePaths> {
    let slot = state_slot();
    let guard = slot.lock().ok()?;
    guard.as_ref().map(|state| state.paths.clone())
}

pub(crate) fn current_run_id() -> Option<String> {
    let slot = state_slot();
    let guard = slot.lock().ok()?;
    guard.as_ref().map(|state| state.run.run_id.clone())
}

pub(crate) fn recent_tool_usage(max_rows: usize, max_aggregates: usize) -> ToolUsageTelemetry {
    let Some(paths) = current_paths() else {
        return ToolUsageTelemetry {
            source_of_truth: "daemon lifecycle ledger not configured".to_owned(),
            max_rows,
            rows_scanned: 0,
            segment_count: 0,
            aggregates: Vec::new(),
            read_error: Some("daemon lifecycle ledger not configured".to_owned()),
        };
    };
    let active = PathBuf::from(&paths.tool_events_path);
    let ledger_paths = match lifecycle_ledger_paths_oldest_first(&active) {
        Ok(paths) => paths,
        Err(error) => {
            return ToolUsageTelemetry {
                source_of_truth: active.display().to_string(),
                max_rows,
                rows_scanned: 0,
                segment_count: 0,
                aggregates: Vec::new(),
                read_error: Some(format!("{error:#}")),
            };
        }
    };
    let mut rows_scanned = 0_usize;
    let mut aggregates: BTreeMap<ToolUsageKey, ToolUsageAggregate> = BTreeMap::new();
    for path in ledger_paths.iter().rev() {
        if rows_scanned >= max_rows {
            break;
        }
        let lines = match File::open(path).map(BufReader::new) {
            Ok(reader) => reader.lines().collect::<Result<Vec<_>, _>>(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        };
        let lines = match lines {
            Ok(lines) => lines,
            Err(error) => {
                return ToolUsageTelemetry {
                    source_of_truth: active.display().to_string(),
                    max_rows,
                    rows_scanned,
                    segment_count: ledger_paths.len(),
                    aggregates: aggregates.into_values().collect(),
                    read_error: Some(format!("read {}: {error}", path.display())),
                };
            }
        };
        for line in lines.into_iter().rev() {
            if rows_scanned >= max_rows {
                break;
            }
            let Ok(event) = serde_json::from_str::<ToolEvent>(&line) else {
                rows_scanned = rows_scanned.saturating_add(1);
                continue;
            };
            rows_scanned = rows_scanned.saturating_add(1);
            if event.event_kind != "tool_call" || event.status == "started" {
                continue;
            }
            let key = (
                event.tool.clone(),
                event.operation.clone(),
                event.route_id.clone(),
                event.profile.clone(),
            );
            let error_code = event
                .error
                .as_ref()
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| {
                    event
                        .error
                        .as_ref()
                        .and_then(|error| error.get("detail_code"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                });
            let entry = aggregates.entry(key).or_insert_with(|| ToolUsageAggregate {
                tool: event.tool.clone(),
                operation: event.operation.clone(),
                route_id: event.route_id.clone(),
                profile: event.profile.clone(),
                tool_surface_sha256: event.tool_surface_sha256.clone(),
                calls_total: 0,
                ok_total: 0,
                error_total: 0,
                panic_total: 0,
                total_duration_ms: 0,
                max_duration_ms: 0,
                latest_status: event.status.clone(),
                latest_error_code: error_code.clone(),
            });
            entry.calls_total = entry.calls_total.saturating_add(1);
            match event.status.as_str() {
                "ok" => entry.ok_total = entry.ok_total.saturating_add(1),
                "panic" => entry.panic_total = entry.panic_total.saturating_add(1),
                _ => entry.error_total = entry.error_total.saturating_add(1),
            }
            let duration_ms = event.duration_ms.unwrap_or(0);
            entry.total_duration_ms = entry.total_duration_ms.saturating_add(duration_ms);
            entry.max_duration_ms = entry.max_duration_ms.max(duration_ms);
        }
    }
    let mut values = aggregates.into_values().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .calls_total
            .cmp(&left.calls_total)
            .then(left.tool.cmp(&right.tool))
            .then(left.operation.cmp(&right.operation))
    });
    values.truncate(max_aggregates);
    ToolUsageTelemetry {
        source_of_truth: active.display().to_string(),
        max_rows,
        rows_scanned,
        segment_count: ledger_paths.len(),
        aggregates: values,
        read_error: None,
    }
}

pub(crate) fn in_flight_tool_calls_for_session(
    session_id: &str,
) -> anyhow::Result<Vec<InFlightToolCallRead>> {
    let slot = state_slot();
    let guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_ref() else {
        bail!("daemon lifecycle ledger is not configured");
    };
    let now = now_unix_ms();
    Ok(state
        .in_flight
        .values()
        .filter(|event| event.mcp_session_id.as_deref() == Some(session_id))
        .map(|event| InFlightToolCallRead {
            seq: event.seq,
            tool: event.tool.clone(),
            mcp_session_id: event.mcp_session_id.clone(),
            started_at_unix_ms: event.started_at_unix_ms,
            elapsed_ms: now.saturating_sub(event.started_at_unix_ms),
            status: event.status.clone(),
        })
        .collect())
}

pub(crate) fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

/// Extract a useful panic diagnostic and consume the payload. Unknown payloads
/// are explicitly dropped under a second unwind boundary because user-defined
/// payload destructors can themselves panic. Only an unknown *secondary* panic
/// payload is leaked, after it has been logged, to prevent recursive destructor
/// panics from aborting the process during safety cleanup.
pub(crate) fn consume_panic_payload(payload: Box<dyn std::any::Any + Send>) -> String {
    let payload = match payload.downcast::<String>() {
        Ok(message) => return *message,
        Err(payload) => payload,
    };
    let payload = match payload.downcast::<&'static str>() {
        Ok(message) => return (*message).to_owned(),
        Err(payload) => payload,
    };
    let original_type_id = format!("{:?}", payload.as_ref().type_id());
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(payload))) {
        Ok(()) => format!("non-string panic payload (type_id={original_type_id})"),
        Err(secondary) => {
            let secondary_text = secondary
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| secondary.downcast_ref::<&'static str>().copied())
                .map(str::to_owned);
            let secondary_type_id = format!("{:?}", secondary.as_ref().type_id());
            tracing::error!(
                code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                detail_code = "PANIC_PAYLOAD_DROP_PANICKED",
                original_type_id = %original_type_id,
                secondary_type_id = %secondary_type_id,
                secondary = secondary_text.as_deref().unwrap_or("non-string panic payload"),
                "dropping a caught panic payload panicked; preserving process safety"
            );
            if secondary_text.is_some() {
                drop(secondary);
            } else {
                // Log first, then leak only this unknown secondary payload. Its
                // destructor just panicked and retrying it risks process abort.
                std::mem::forget(secondary);
            }
            format!(
                "non-string panic payload (type_id={original_type_id}); payload Drop panicked: {}",
                secondary_text.unwrap_or_else(|| {
                    format!("non-string secondary payload (type_id={secondary_type_id})")
                })
            )
        }
    }
}

fn finish_tool_call(
    run_id: &str,
    seq: u64,
    status: &'static str,
    error: Option<Value>,
    panic: Option<Value>,
    effective_target: Option<Value>,
) -> anyhow::Result<()> {
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
        bail!("daemon lifecycle ledger is not configured");
    };
    if state.run.run_id != run_id {
        bail!(
            "daemon lifecycle tool event {seq} belongs to superseded run {run_id}, current run is {}",
            state.run.run_id
        );
    }
    let Some(mut event) = state.in_flight.get(&seq).cloned() else {
        bail!("daemon lifecycle in-flight tool event {seq} is missing");
    };
    let finished_at_unix_ms = now_unix_ms();
    status.clone_into(&mut event.status);
    event.finished_at_unix_ms = Some(finished_at_unix_ms);
    event.duration_ms = Some(finished_at_unix_ms.saturating_sub(event.started_at_unix_ms));
    event.effective_target = effective_target;
    event.error = error;
    event.panic = panic;
    write_tool_event(state, &event)?;
    state.in_flight.remove(&seq);
    Ok(())
}

fn record_panic(info: &std::panic::PanicHookInfo<'_>) -> anyhow::Result<()> {
    let location = info.location().map(|location| {
        json!({
            "file": location.file(),
            "line": location.line(),
            "column": location.column(),
        })
    });
    let payload = panic_payload_message(info.payload());
    append_diagnostic_event(
        "panic",
        "panic",
        json!({
            "payload": payload,
            "location": location,
            "thread": std::thread::current().name(),
        }),
    )
}

fn append_diagnostic_event(
    event_kind: &'static str,
    cause: &'static str,
    detail: Value,
) -> anyhow::Result<()> {
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
        bail!("daemon lifecycle ledger is not configured");
    };
    let event = ExitEvent {
        schema_version: SCHEMA_VERSION,
        run_id: state.run.run_id.clone(),
        pid: state.run.pid,
        event_kind: event_kind.to_owned(),
        cause: cause.to_owned(),
        detail,
        recorded_at_unix_ms: now_unix_ms(),
        run: Some(state.run.clone()),
        last_tool_event: read_optional_json(Path::new(&state.paths.tool_last_path))
            .with_context(|| format!("read last tool event {}", state.paths.tool_last_path))?,
        in_flight_tool_events: state.in_flight.values().cloned().collect(),
        paths: state.paths.clone(),
    };
    let db_path = PathBuf::from(&state.paths.db_path);
    let exit_events_path = state.paths.exit_events_path.clone();
    with_lifecycle_ledger_lock(&db_path, "append daemon diagnostic event", || {
        append_exit_event(state, &event)
            .with_context(|| format!("append daemon diagnostic event {exit_events_path}"))
    })
}

fn record_exit(event_kind: &'static str, cause: &'static str, detail: Value) -> anyhow::Result<()> {
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
        bail!("daemon lifecycle ledger is not configured");
    };
    record_exit_for_state(state, event_kind, cause, detail)
}

fn record_exit_for_state(
    state: &mut DaemonLifecycleState,
    event_kind: &'static str,
    cause: &'static str,
    detail: Value,
) -> anyhow::Result<()> {
    let db_path = PathBuf::from(&state.paths.db_path);
    with_lifecycle_ledger_lock(&db_path, "record daemon exit", || {
        record_exit_for_state_locked(state, event_kind, cause, detail)
    })
}

fn record_exit_for_state_locked(
    state: &mut DaemonLifecycleState,
    event_kind: &'static str,
    cause: &'static str,
    detail: Value,
) -> anyhow::Result<()> {
    let mut run = state.run.clone();
    run.ended_at_unix_ms = Some(now_unix_ms());
    run.ended_reason = Some(cause.to_owned());
    let event = ExitEvent {
        schema_version: SCHEMA_VERSION,
        run_id: state.run.run_id.clone(),
        pid: state.run.pid,
        event_kind: event_kind.to_owned(),
        cause: cause.to_owned(),
        detail,
        recorded_at_unix_ms: now_unix_ms(),
        run: Some(run.clone()),
        last_tool_event: read_optional_json(Path::new(&state.paths.tool_last_path))
            .with_context(|| format!("read last tool event {}", state.paths.tool_last_path))?,
        in_flight_tool_events: state.in_flight.values().cloned().collect(),
        paths: state.paths.clone(),
    };
    let current = read_optional_json::<RunRecord>(Path::new(&state.paths.run_current_path))
        .with_context(|| {
            format!(
                "read daemon current run before exit finalization {}",
                state.paths.run_current_path
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "daemon current run disappeared before exit finalization: {}",
                state.paths.run_current_path
            )
        })?;
    let owns_run_current = current.run_id == state.run.run_id;
    append_exit_event(state, &event)
        .with_context(|| format!("append daemon exit event {}", state.paths.exit_events_path))?;
    if owns_run_current {
        write_json_atomic(Path::new(&state.paths.run_current_path), &run).with_context(|| {
            format!(
                "write daemon ended current run {}",
                state.paths.run_current_path
            )
        })?;
    }
    if !owns_run_current {
        tracing::info!(
            code = "MCP_DAEMON_LIFECYCLE_RUN_CURRENT_SUPERSEDED",
            run_id = %state.run.run_id,
            run_current_path = %state.paths.run_current_path,
            "recorded this daemon's exit event without overwriting a successor's current-run record"
        );
    }
    state.run = run;
    Ok(())
}

fn write_tool_event(state: &mut DaemonLifecycleState, event: &ToolEvent) -> anyhow::Result<()> {
    match write_tool_event_inner(state, event) {
        Ok(()) => {
            state.last_error = None;
            tracing::info!(
                code = "MCP_DAEMON_LIFECYCLE_TOOL_EVENT_RECORDED",
                tool = %event.tool,
                status = %event.status,
                seq = event.seq,
                mcp_session_id = event.mcp_session_id.as_deref().unwrap_or("<none>"),
                "daemon lifecycle tool event recorded"
            );
            Ok(())
        }
        Err(error) => {
            let detail = format!("{error:#}");
            state.last_error = Some(detail.clone());
            tracing::error!(
                code = "MCP_DAEMON_LIFECYCLE_WRITE_FAILED",
                tool = %event.tool,
                status = %event.status,
                seq = event.seq,
                detail = %detail,
                "daemon lifecycle tool event write failed"
            );
            Err(error)
        }
    }
}

fn write_tool_event_inner(
    state: &mut DaemonLifecycleState,
    event: &ToolEvent,
) -> anyhow::Result<()> {
    append_tool_event(state, event)?;
    let tool_last_path = state.paths.tool_last_path.clone();
    write_json_atomic(Path::new(&tool_last_path), event)
        .with_context(|| format!("write daemon last tool {tool_last_path}"))
}

fn append_tool_event(state: &mut DaemonLifecycleState, event: &ToolEvent) -> anyhow::Result<()> {
    let tool_events_path = state.paths.tool_events_path.clone();
    append_bounded_json_line(
        Path::new(&tool_events_path),
        event,
        &mut state.tool_events_bytes,
        state.max_segment_bytes,
        "tool_events",
    )
    .with_context(|| format!("append daemon tool event {tool_events_path}"))
}

fn append_exit_event(state: &mut DaemonLifecycleState, event: &ExitEvent) -> anyhow::Result<()> {
    let exit_events_path = state.paths.exit_events_path.clone();
    match append_bounded_json_line(
        Path::new(&exit_events_path),
        event,
        &mut state.exit_events_bytes,
        state.max_segment_bytes,
        "exit_events",
    ) {
        Ok(()) => {
            state.last_error = None;
            tracing::info!(
                code = "MCP_DAEMON_LIFECYCLE_EXIT_EVENT_RECORDED",
                cause = %event.cause,
                event_kind = %event.event_kind,
                "daemon lifecycle exit event recorded"
            );
            Ok(())
        }
        Err(error) => {
            let detail = format!("{error:#}");
            state.last_error = Some(detail.clone());
            tracing::error!(
                code = "MCP_DAEMON_LIFECYCLE_EXIT_WRITE_FAILED",
                cause = %event.cause,
                event_kind = %event.event_kind,
                detail = %detail,
                "daemon lifecycle exit event write failed"
            );
            Err(error)
        }
    }
}

/// Append one JSON line to a bounded lifecycle ledger, rotating the active
/// segment first when this record would push it past the size cap.
///
/// The active byte counter is tracked in memory so the append hot path never
/// stats the file. Rotation runs before the active file is opened, so Windows
/// never has to rename a file with an open append handle. If a single record is
/// larger than the cap, it is written to an empty segment and reported as an
/// explicit oversize exception instead of being dropped.
fn append_bounded_json_line<T: Serialize>(
    path: &Path,
    value: &T,
    active_bytes: &mut u64,
    max_segment_bytes: u64,
    ledger_name: &'static str,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(value)
        .with_context(|| format!("encode JSON line {}", path.display()))?;
    line.push(b'\n');
    let line_len = u64::try_from(line.len()).unwrap_or(u64::MAX);

    if *active_bytes > 0 && active_bytes.saturating_add(line_len) > max_segment_bytes {
        if let Err(error) = rotate_ledger(path, ledger_name) {
            let detail = format!("{error:#}");
            tracing::error!(
                code = "DAEMON_LEDGER_ROTATE_FAILED",
                ledger = ledger_name,
                path = %path.display(),
                active_bytes = *active_bytes,
                next_record_bytes = line_len,
                max_segment_bytes,
                detail = %detail,
                "daemon lifecycle ledger rotation failed"
            );
            return Err(error);
        }
        *active_bytes = 0;
        tracing::info!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_ROTATED",
            ledger = ledger_name,
            path = %path.display(),
            max_segment_bytes,
            max_segments = MAX_LEDGER_SEGMENTS,
            "daemon lifecycle ledger rotated"
        );
    }

    if line_len > max_segment_bytes {
        tracing::warn!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_OVERSIZED_RECORD",
            ledger = ledger_name,
            path = %path.display(),
            record_bytes = line_len,
            max_segment_bytes,
            "daemon lifecycle ledger record exceeds the segment cap and is retained as an explicit oversize exception"
        );
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open append {}", path.display()))?;
    file.write_all(&line)
        .with_context(|| format!("write daemon lifecycle ledger {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("sync {}", path.display()))?;
    *active_bytes = active_bytes.saturating_add(line_len);
    Ok(())
}

fn reconcile_jsonl_ledger(
    active: &Path,
    max_segment_bytes: u64,
    ledger_name: &'static str,
) -> anyhow::Result<u64> {
    let sources = discover_ledger_sources(active)?;
    if sources.is_empty() {
        return Ok(0);
    }
    let parent = ledger_parent(active)?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let staging_dir = unique_ledger_dir(active, "rewrite")?;
    fs::create_dir(&staging_dir).with_context(|| {
        format!(
            "create ledger rewrite staging dir {}",
            staging_dir.display()
        )
    })?;

    let rewrite =
        match stage_rewritten_ledger(&sources, &staging_dir, max_segment_bytes, ledger_name) {
            Ok(rewrite) => rewrite,
            Err(error) => {
                remove_dir_all_best_effort(&staging_dir, "rewrite staging", ledger_name);
                return Err(error);
            }
        };

    let retained_start = rewrite
        .segments
        .len()
        .saturating_sub(MAX_RETAINED_LEDGER_FILES);
    let pruned = &rewrite.segments[..retained_start];
    let pruned_bytes: u64 = pruned.iter().map(|segment| segment.bytes).sum();
    let pruned_records: u64 = pruned.iter().map(|segment| segment.records).sum();
    let pruned_oversized_records: u64 =
        pruned.iter().map(|segment| segment.oversized_records).sum();
    let retained = &rewrite.segments[retained_start..];
    let retained_oversized_records: u64 = retained
        .iter()
        .map(|segment| segment.oversized_records)
        .sum();
    let active_bytes = install_reconciled_ledger(active, &sources, retained, ledger_name)
        .with_context(|| {
            format!(
                "install reconciled daemon lifecycle {ledger_name} ledger {}",
                active.display()
            )
        })?;
    remove_dir_all_best_effort(&staging_dir, "rewrite staging", ledger_name);

    if pruned_records > 0 || pruned_bytes > 0 {
        tracing::warn!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_RETENTION_PRUNED",
            ledger = ledger_name,
            active_path = %active.display(),
            pruned_records,
            pruned_bytes,
            pruned_oversized_records,
            retained_files = retained.len(),
            max_retained_files = MAX_RETAINED_LEDGER_FILES,
            "daemon lifecycle ledger startup reconciliation pruned records outside retention"
        );
    }
    if rewrite.missing_newline_repairs > 0 {
        tracing::warn!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_MISSING_NEWLINE_REPAIRED",
            ledger = ledger_name,
            active_path = %active.display(),
            repaired_lines = rewrite.missing_newline_repairs,
            "daemon lifecycle ledger startup reconciliation repaired unterminated JSONL records before future appends"
        );
    }
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_LEDGER_RECONCILED",
        ledger = ledger_name,
        active_path = %active.display(),
        source_files = sources.len(),
        source_records = rewrite.source_records,
        source_bytes = rewrite.source_bytes,
        retained_files = retained.len(),
        retained_oversized_records,
        active_bytes,
        max_segment_bytes,
        max_retained_files = MAX_RETAINED_LEDGER_FILES,
        "daemon lifecycle ledger startup reconciliation complete"
    );
    Ok(active_bytes)
}

fn stage_rewritten_ledger(
    sources: &[LedgerSource],
    staging_dir: &Path,
    max_segment_bytes: u64,
    ledger_name: &'static str,
) -> anyhow::Result<LedgerRewrite> {
    let mut writer = LedgerRewriteWriter::new(staging_dir, max_segment_bytes, ledger_name);
    let mut source_bytes = 0_u64;
    let mut source_records = 0_u64;
    let mut missing_newline_repairs = 0_u64;

    for source in sources {
        let file = File::open(&source.path)
            .with_context(|| format!("open lifecycle ledger segment {}", source.path.display()))?;
        let mut reader = BufReader::new(file);
        loop {
            let mut line = Vec::new();
            let read = reader.read_until(b'\n', &mut line).with_context(|| {
                format!("read lifecycle ledger segment {}", source.path.display())
            })?;
            if read == 0 {
                break;
            }
            source_records = source_records.saturating_add(1);
            source_bytes = source_bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            if !line.ends_with(b"\n") {
                line.push(b'\n');
                missing_newline_repairs = missing_newline_repairs.saturating_add(1);
            }
            writer.append_line(&line)?;
        }
    }

    Ok(LedgerRewrite {
        segments: writer.finish()?,
        source_bytes,
        source_records,
        missing_newline_repairs,
    })
}

struct LedgerRewriteWriter<'a> {
    dir: &'a Path,
    max_segment_bytes: u64,
    ledger_name: &'static str,
    next_index: usize,
    current_file: Option<File>,
    current_path: PathBuf,
    current_bytes: u64,
    current_records: u64,
    current_oversized_records: u64,
    segments: Vec<StagedLedgerSegment>,
}

impl<'a> LedgerRewriteWriter<'a> {
    fn new(dir: &'a Path, max_segment_bytes: u64, ledger_name: &'static str) -> Self {
        Self {
            dir,
            max_segment_bytes,
            ledger_name,
            next_index: 0,
            current_file: None,
            current_path: PathBuf::new(),
            current_bytes: 0,
            current_records: 0,
            current_oversized_records: 0,
            segments: Vec::new(),
        }
    }

    fn append_line(&mut self, line: &[u8]) -> anyhow::Result<()> {
        let line_len = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if self.current_bytes > 0
            && self.current_bytes.saturating_add(line_len) > self.max_segment_bytes
        {
            self.finish_current()?;
        }
        if self.current_file.is_none() {
            self.start_segment()?;
        }
        let file = self
            .current_file
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ledger rewrite segment was not opened"))?;
        file.write_all(line).with_context(|| {
            format!(
                "write staged lifecycle ledger {}",
                self.current_path.display()
            )
        })?;
        self.current_bytes = self.current_bytes.saturating_add(line_len);
        self.current_records = self.current_records.saturating_add(1);
        if line_len > self.max_segment_bytes {
            self.current_oversized_records = self.current_oversized_records.saturating_add(1);
            tracing::warn!(
                code = "MCP_DAEMON_LIFECYCLE_LEDGER_OVERSIZED_RECORD",
                ledger = self.ledger_name,
                staged_path = %self.current_path.display(),
                record_bytes = line_len,
                max_segment_bytes = self.max_segment_bytes,
                "daemon lifecycle ledger reconciliation retained a single record larger than the segment cap"
            );
            self.finish_current()?;
        }
        Ok(())
    }

    fn start_segment(&mut self) -> anyhow::Result<()> {
        let path = self.dir.join(format!(
            "segment-{index:020}.jsonl",
            index = self.next_index
        ));
        self.next_index = self.next_index.saturating_add(1);
        let file = File::create(&path)
            .with_context(|| format!("create staged ledger {}", path.display()))?;
        self.current_file = Some(file);
        self.current_path = path;
        self.current_bytes = 0;
        self.current_records = 0;
        self.current_oversized_records = 0;
        Ok(())
    }

    fn finish_current(&mut self) -> anyhow::Result<()> {
        let Some(mut file) = self.current_file.take() else {
            return Ok(());
        };
        file.flush()
            .with_context(|| format!("flush staged ledger {}", self.current_path.display()))?;
        file.sync_data()
            .with_context(|| format!("sync staged ledger {}", self.current_path.display()))?;
        self.segments.push(StagedLedgerSegment {
            path: self.current_path.clone(),
            bytes: self.current_bytes,
            records: self.current_records,
            oversized_records: self.current_oversized_records,
        });
        self.current_path = PathBuf::new();
        self.current_bytes = 0;
        self.current_records = 0;
        self.current_oversized_records = 0;
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<Vec<StagedLedgerSegment>> {
        self.finish_current()?;
        Ok(self.segments)
    }
}

fn install_reconciled_ledger(
    active: &Path,
    sources: &[LedgerSource],
    retained: &[StagedLedgerSegment],
    ledger_name: &'static str,
) -> anyhow::Result<u64> {
    let backup_dir = unique_ledger_dir(active, "backup")?;
    fs::create_dir(&backup_dir)
        .with_context(|| format!("create ledger rewrite backup dir {}", backup_dir.display()))?;
    for source in sources {
        let backup_path = backup_dir.join(source.path.file_name().ok_or_else(|| {
            anyhow::anyhow!("ledger source has no file name: {}", source.path.display())
        })?);
        fs::rename(&source.path, &backup_path).with_context(|| {
            format!(
                "move existing daemon lifecycle {ledger_name} ledger segment {} to backup {}",
                source.path.display(),
                backup_path.display()
            )
        })?;
    }

    for (newest_offset, segment) in retained.iter().rev().enumerate() {
        let destination = if newest_offset == 0 {
            active.to_path_buf()
        } else {
            segment_path(active, newest_offset)
        };
        fs::rename(&segment.path, &destination).with_context(|| {
            format!(
                "install daemon lifecycle {ledger_name} ledger segment {} to {}",
                segment.path.display(),
                destination.display()
            )
        })?;
    }
    let active_bytes = retained.last().map_or(0, |segment| segment.bytes);

    match fs::remove_dir_all(&backup_dir) {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(
                code = "MCP_DAEMON_LIFECYCLE_LEDGER_BACKUP_CLEANUP_FAILED",
                ledger = ledger_name,
                backup_dir = %backup_dir.display(),
                error = %error,
                "daemon lifecycle ledger rewrite succeeded but backup cleanup failed"
            );
        }
    }
    Ok(active_bytes)
}

fn remove_dir_all_best_effort(path: &Path, role: &'static str, ledger_name: &'static str) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                code = "MCP_DAEMON_LIFECYCLE_LEDGER_TEMP_CLEANUP_FAILED",
                ledger = ledger_name,
                role,
                path = %path.display(),
                error = %error,
                "daemon lifecycle ledger temporary directory cleanup failed"
            );
        }
    }
}

fn unique_ledger_dir(active: &Path, role: &str) -> anyhow::Result<PathBuf> {
    let parent = ledger_parent(active)?;
    let file_name = active
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("ledger path has no file name: {}", active.display()))?
        .to_string_lossy();
    Ok(parent.join(format!(
        ".{file_name}.{role}.{}.{}",
        std::process::id(),
        uuid::Uuid::now_v7().simple()
    )))
}

fn ledger_parent(active: &Path) -> anyhow::Result<&Path> {
    active
        .parent()
        .ok_or_else(|| anyhow::anyhow!("ledger path has no parent: {}", active.display()))
}

/// Rotate an active lifecycle ledger using a fixed shift scheme.
///
/// `<ledger>.1` is always the most recently rotated segment and
/// `<ledger>.{MAX_LEDGER_SEGMENTS}` the oldest. The oldest slot is pruned before
/// shifting so the file count cannot exceed the retention cap. Every error is
/// propagated so callers never continue appending into an oversized active file.
fn rotate_ledger(active: &Path, ledger_name: &'static str) -> anyhow::Result<()> {
    let oldest = segment_path(active, MAX_LEDGER_SEGMENTS);
    match fs::metadata(&oldest) {
        Ok(metadata) => {
            fs::remove_file(&oldest).with_context(|| {
                format!(
                    "prune oldest daemon lifecycle {ledger_name} segment {}",
                    oldest.display()
                )
            })?;
            tracing::warn!(
                code = "MCP_DAEMON_LIFECYCLE_LEDGER_RETENTION_PRUNED",
                ledger = ledger_name,
                path = %oldest.display(),
                pruned_bytes = metadata.len(),
                max_segments = MAX_LEDGER_SEGMENTS,
                "daemon lifecycle ledger rotation pruned oldest retained segment"
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "stat oldest daemon lifecycle {ledger_name} segment {}",
                    oldest.display()
                )
            });
        }
    }
    for index in (1..MAX_LEDGER_SEGMENTS).rev() {
        let from = segment_path(active, index);
        let to = segment_path(active, index + 1);
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "shift daemon lifecycle {ledger_name} segment {} to {}",
                        from.display(),
                        to.display()
                    )
                });
            }
        }
    }
    let newest = segment_path(active, 1);
    fs::rename(active, &newest).with_context(|| {
        format!(
            "rotate active daemon lifecycle {ledger_name} ledger {} to {}",
            active.display(),
            newest.display()
        )
    })
}

fn discover_ledger_sources(active: &Path) -> anyhow::Result<Vec<LedgerSource>> {
    let parent = ledger_parent(active)?;
    let file_name = active
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("ledger path has no file name: {}", active.display()))?
        .to_string_lossy()
        .into_owned();
    let rotated_prefix = format!("{file_name}.");
    let mut sources = Vec::new();
    match fs::read_dir(parent) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.with_context(|| format!("read entry in {}", parent.display()))?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == file_name {
                    sources.push(LedgerSource {
                        path: entry.path(),
                        suffix: None,
                    });
                } else if let Some(suffix) = name.strip_prefix(&rotated_prefix)
                    && let Ok(index) = suffix.parse::<usize>()
                    && index > 0
                {
                    sources.push(LedgerSource {
                        path: entry.path(),
                        suffix: Some(index),
                    });
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", parent.display())),
    }
    sources.sort_by(|left, right| match (left.suffix, right.suffix) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    Ok(sources)
}

pub(crate) fn lifecycle_ledger_paths_oldest_first(active: &Path) -> anyhow::Result<Vec<PathBuf>> {
    discover_ledger_sources(active)
        .map(|sources| sources.into_iter().map(|source| source.path).collect())
}

fn ledger_diagnostic_value(
    active: &Path,
    max_segment_bytes: u64,
    ledger_name: &'static str,
) -> Value {
    match ledger_segment_values(active, max_segment_bytes) {
        Ok((segments, total_bytes, oversized_segment_count)) => json!({
            "status": "ok",
            "ledger": ledger_name,
            "active_path": active.display().to_string(),
            "max_segment_bytes": max_segment_bytes,
            "max_segments": MAX_LEDGER_SEGMENTS,
            "max_retained_files": MAX_RETAINED_LEDGER_FILES,
            "active_bytes": segments
                .iter()
                .find(|segment| segment.get("suffix").is_none_or(Value::is_null))
                .and_then(|segment| segment.get("bytes"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "rotated_segment_count": segments
                .iter()
                .filter(|segment| !segment.get("suffix").is_none_or(Value::is_null))
                .count(),
            "segment_count": segments.len(),
            "total_bytes": total_bytes,
            "oversized_segment_count": oversized_segment_count,
            "segments": segments,
        }),
        Err(error) => json!({
            "status": "error",
            "ledger": ledger_name,
            "active_path": active.display().to_string(),
            "max_segment_bytes": max_segment_bytes,
            "detail": format!("{error:#}"),
        }),
    }
}

fn ledger_summary_for_health(active: &Path, max_segment_bytes: u64) -> String {
    match ledger_segment_values(active, max_segment_bytes) {
        Ok((segments, total_bytes, oversized_segment_count)) => {
            let active_bytes = segments
                .iter()
                .find(|segment| segment.get("suffix").is_none_or(Value::is_null))
                .and_then(|segment| segment.get("bytes"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            format!(
                "active_bytes:{active_bytes},segments:{},total_bytes:{total_bytes},oversized_segments:{oversized_segment_count},max_segment_bytes:{max_segment_bytes}",
                segments.len()
            )
        }
        Err(error) => format!("error:{error:#}"),
    }
}

fn ledger_segment_values(
    active: &Path,
    max_segment_bytes: u64,
) -> anyhow::Result<(Vec<Value>, u64, usize)> {
    let mut sources = discover_ledger_sources(active)?;
    sources.sort_by(|left, right| match (left.suffix, right.suffix) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => left.cmp(&right),
    });
    let mut total_bytes = 0_u64;
    let mut oversized_segment_count = 0_usize;
    let mut values = Vec::with_capacity(sources.len());
    for source in sources {
        let bytes = fs::metadata(&source.path)
            .with_context(|| format!("stat lifecycle ledger segment {}", source.path.display()))?
            .len();
        total_bytes = total_bytes.saturating_add(bytes);
        let oversized = bytes > max_segment_bytes;
        if oversized {
            oversized_segment_count = oversized_segment_count.saturating_add(1);
        }
        values.push(json!({
            "path": source.path.display().to_string(),
            "role": if source.suffix.is_some() { "rotated" } else { "active" },
            "suffix": source.suffix,
            "bytes": bytes,
            "oversized": oversized,
        }));
    }
    Ok((values, total_bytes, oversized_segment_count))
}

/// Build the path of rotated segment `index` for `active` by appending
/// `.{index}` to the active file name (e.g. `daemon-tool-events.jsonl.1`).
fn segment_path(active: &Path, index: usize) -> PathBuf {
    let mut name = active
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".{index}"));
    active.with_file_name(name)
}

fn configured_max_segment_bytes() -> u64 {
    #[cfg(test)]
    {
        let override_bytes =
            TEST_MAX_LEDGER_SEGMENT_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        if override_bytes > 0 {
            return override_bytes;
        }
    }
    MAX_LEDGER_SEGMENT_BYTES
}

#[cfg(test)]
pub(crate) fn set_max_segment_bytes_for_test(bytes: u64) {
    TEST_MAX_LEDGER_SEGMENT_BYTES.store(bytes, std::sync::atomic::Ordering::Relaxed);
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = guard.as_mut() {
        state.max_segment_bytes = bytes;
    }
}

fn state_slot() -> &'static Mutex<Option<DaemonLifecycleState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    TEST_MAX_LEDGER_SEGMENT_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
    if let Some(slot) = STATE.get() {
        let mut guard = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
    }
}

fn health_detail_for_state(state: &DaemonLifecycleState) -> String {
    let last_error = state
        .last_error
        .as_deref()
        .map_or_else(|| "none".to_owned(), ToOwned::to_owned);
    let tool_ledger = ledger_summary_for_health(
        Path::new(&state.paths.tool_events_path),
        state.max_segment_bytes,
    );
    let exit_ledger = ledger_summary_for_health(
        Path::new(&state.paths.exit_events_path),
        state.max_segment_bytes,
    );
    format!(
        "run_id={} pid={} run_current_path={} tool_last_path={} tool_events_path={} exit_events_path={} in_flight_count={} tool_ledger={} exit_ledger={} last_error={}",
        state.run.run_id,
        state.run.pid,
        state.paths.run_current_path,
        state.paths.tool_last_path,
        state.paths.tool_events_path,
        state.paths.exit_events_path,
        state.in_flight.len(),
        tool_ledger,
        exit_ledger,
        last_error
    )
}

fn read_optional_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parse JSON {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp = path.with_extension("tmp");
    {
        let mut file = File::create(&temp).with_context(|| format!("create {}", temp.display()))?;
        serde_json::to_writer_pretty(&mut file, value)
            .with_context(|| format!("encode JSON {}", temp.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("write newline {}", temp.display()))?;
        file.flush()
            .with_context(|| format!("flush {}", temp.display()))?;
        file.sync_data()
            .with_context(|| format!("sync {}", temp.display()))?;
    }
    fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))
}

fn now_unix_ms() -> u64 {
    duration_millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    use serde_json::json;

    use super::*;

    #[test]
    fn consume_panic_payload_preserves_string_diagnostic() {
        let message = consume_panic_payload(Box::new("synthetic tool panic"));

        assert_eq!(message, "synthetic tool panic");
    }

    #[test]
    fn consume_panic_payload_drops_unknown_payload() {
        struct MarksDrop(Arc<AtomicBool>);

        impl Drop for MarksDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let message = consume_panic_payload(Box::new(MarksDrop(Arc::clone(&dropped))));

        assert!(dropped.load(Ordering::Acquire));
        assert!(message.starts_with("non-string panic payload"));
    }

    #[test]
    fn consume_panic_payload_contains_payload_destructor_panic() {
        struct PanicOnDrop;

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("synthetic panic-payload destructor failure");
            }
        }

        let result = std::panic::catch_unwind(|| consume_panic_payload(Box::new(PanicOnDrop)));

        let message = result.unwrap_or_else(|_panic| {
            panic!("panic payload consumer must contain destructor panics")
        });
        assert!(message.contains("payload Drop panicked"));
        assert!(message.contains("synthetic panic-payload destructor failure"));
    }

    #[test]
    fn records_tool_start_and_finish_to_physical_files() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap();

        let guard = begin_tool_call(ToolCallStart {
            tool: "health".to_owned(),
            operation: Some("compact".to_owned()),
            route_id: Some("health.compact".to_owned()),
            profile: Some("normal_agent".to_owned()),
            tool_surface_sha256: Some("synthetic-surface".to_owned()),
            tool_profile_read_error: None,
            mcp_session_id: Some("session-a".to_owned()),
            audit_context: Some(json!({"profile_id": "synthetic"})),
            audit_context_read_error: None,
            foreground: Some(json!({"hwnd": 1234, "window_title": "Synthetic"})),
            foreground_read_error: None,
            session_target: Some(json!({"kind": "window", "hwnd": 1234})),
            session_target_read_error: None,
        })
        .unwrap();
        guard.finish_ok_with_effective_target(None).unwrap();

        let last: ToolEvent = read_optional_json(Path::new(&paths.tool_last_path))
            .unwrap()
            .unwrap();
        assert_eq!(last.tool, "health");
        assert_eq!(last.status, "ok");
        assert_eq!(last.mcp_session_id.as_deref(), Some("session-a"));
        assert_eq!(last.effective_target, None);

        let events = fs::read_to_string(&paths.tool_events_path).unwrap();
        assert_eq!(events.lines().count(), 2);
        assert!(events.contains("\"status\":\"started\""));
        assert!(events.contains("\"status\":\"ok\""));
        let rows = events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            rows[0].get("status").and_then(Value::as_str),
            Some("started")
        );
        assert!(
            rows[0].get("audit_context").is_none(),
            "started rows must not duplicate full audit context"
        );
        assert!(
            rows[0].get("foreground").is_none(),
            "started rows must not duplicate foreground snapshots"
        );
        assert!(
            rows[0].get("session_target").is_none(),
            "started rows must not duplicate session target snapshots"
        );
        assert_eq!(rows[1].get("status").and_then(Value::as_str), Some("ok"));
        assert!(
            rows[1].get("audit_context").is_some(),
            "completion row keeps the full audit context once"
        );
        assert!(
            rows[1].get("foreground").is_some(),
            "completion row keeps the foreground snapshot once"
        );
        assert!(
            rows[1].get("session_target").is_some(),
            "completion row keeps the session target once"
        );
    }

    #[test]
    fn unfinished_tool_guard_drop_publishes_terminal_error() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap();

        let guard = begin_tool_call(ToolCallStart {
            tool: "observe".to_owned(),
            operation: None,
            route_id: Some("observe".to_owned()),
            profile: None,
            tool_surface_sha256: None,
            tool_profile_read_error: None,
            mcp_session_id: Some("session-cancelled".to_owned()),
            audit_context: None,
            audit_context_read_error: None,
            foreground: None,
            foreground_read_error: None,
            session_target: None,
            session_target_read_error: None,
        })
        .unwrap();
        drop(guard);

        let last: ToolEvent = read_optional_json(Path::new(&paths.tool_last_path))
            .unwrap()
            .unwrap();
        assert_eq!(last.tool, "observe");
        assert_eq!(last.status, "error");
        assert_eq!(
            last.error
                .as_ref()
                .and_then(|error| error.get("detail_code"))
                .and_then(Value::as_str),
            Some("MCP_TOOL_CALL_GUARD_DROPPED_UNFINISHED")
        );
        assert!(
            in_flight_tool_calls_for_session("session-cancelled")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn records_effective_target_on_tool_finish() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap();

        let guard = begin_tool_call(ToolCallStart {
            tool: "browser_dom".to_owned(),
            operation: Some("inspect".to_owned()),
            route_id: Some("browser_dom.inspect".to_owned()),
            profile: Some("browser_control".to_owned()),
            tool_surface_sha256: Some("synthetic-surface".to_owned()),
            tool_profile_read_error: None,
            mcp_session_id: Some("session-a".to_owned()),
            audit_context: None,
            audit_context_read_error: None,
            foreground: None,
            foreground_read_error: None,
            session_target: Some(json!({
                "kind": "cdp",
                "window_hwnd": 1,
                "cdp_target_id": "chrome-tab:session",
            })),
            session_target_read_error: None,
        })
        .unwrap();
        guard
            .finish_ok_with_effective_target(Some(json!({
                "kind": "cdp",
                "window_hwnd": 2,
                "cdp_target_id": "chrome-tab:explicit",
                "source": "structured_content.content",
            })))
            .unwrap();

        let last: ToolEvent = read_optional_json(Path::new(&paths.tool_last_path))
            .unwrap()
            .unwrap();
        assert_eq!(
            last.session_target
                .as_ref()
                .and_then(|target| target.get("cdp_target_id"))
                .and_then(Value::as_str),
            Some("chrome-tab:session")
        );
        assert_eq!(
            last.effective_target
                .as_ref()
                .and_then(|target| target.get("cdp_target_id"))
                .and_then(Value::as_str),
            Some("chrome-tab:explicit")
        );
    }

    #[test]
    fn records_context_event_to_physical_files() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap();

        let seq = record_context_event(ContextEvent {
            event_kind: "foreground_context_restore",
            tool: "act_press",
            status: "skipped_human_moved",
            mcp_session_id: Some("session-restore".to_owned()),
            foreground: Some(json!({"hwnd": 2222, "pid": 3333})),
            foreground_read_error: None,
            detail: json!({
                "code": "FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED",
                "reason_code": "foreground_restore_skipped_human_moved",
                "detail": {
                    "prior_hwnd": 1111,
                    "expected_pid": 4444,
                },
            }),
        })
        .unwrap();

        let last: ToolEvent = read_optional_json(Path::new(&paths.tool_last_path))
            .unwrap()
            .unwrap();
        assert_eq!(last.seq, seq);
        assert_eq!(last.event_kind, "foreground_context_restore");
        assert_eq!(last.tool, "act_press");
        assert_eq!(last.status, "skipped_human_moved");
        assert_eq!(last.mcp_session_id.as_deref(), Some("session-restore"));
        assert_eq!(
            last.detail
                .as_ref()
                .and_then(|detail| detail.get("code"))
                .and_then(Value::as_str),
            Some("FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED")
        );

        let events = fs::read_to_string(&paths.tool_events_path).unwrap();
        assert!(events.contains("\"event_kind\":\"foreground_context_restore\""));
        assert!(events.contains("\"status\":\"skipped_human_moved\""));
        assert!(events.contains("\"code\":\"FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED\""));
    }

    #[test]
    fn next_start_records_previous_unclean_run() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap();
        let guard = begin_tool_call(ToolCallStart {
            tool: "observe".to_owned(),
            operation: None,
            route_id: Some("observe".to_owned()),
            profile: None,
            tool_surface_sha256: None,
            tool_profile_read_error: None,
            mcp_session_id: Some("session-crash".to_owned()),
            audit_context: None,
            audit_context_read_error: None,
            foreground: Some(json!({"hwnd": 99})),
            foreground_read_error: None,
            session_target: None,
            session_target_read_error: None,
        })
        .unwrap();
        // Simulate process loss: a real process crash does not run Drop.
        std::mem::forget(guard);

        configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap();

        let exits = fs::read_to_string(&paths.exit_events_path).unwrap();
        assert!(exits.contains("\"event_kind\":\"previous_run_unclean\""));
        assert!(exits.contains("\"cause\":\"process_missing_on_startup\""));
        assert!(exits.contains("\"tool\":\"observe\""));
        assert!(exits.contains("\"status\":\"started\""));
    }

    #[test]
    fn superseded_daemon_exit_never_overwrites_successor_run_current() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap();
        let mut old_state = state_slot()
            .lock()
            .unwrap()
            .as_ref()
            .expect("old lifecycle state")
            .clone();
        let old_run_id = old_state.run.run_id.clone();

        let mut successor_before = old_state.run.clone();
        successor_before.run_id = "synthetic-successor-run".to_owned();
        successor_before.pid = old_state.run.pid.saturating_add(1);
        successor_before.bind_addr = Some("127.0.0.1:7701".to_owned());
        successor_before.started_at_unix_ms = old_state.run.started_at_unix_ms.saturating_add(1);
        with_lifecycle_ledger_lock(temp.path(), "publish synthetic successor", || {
            write_json_atomic(Path::new(&paths.run_current_path), &successor_before)
        })
        .unwrap();
        assert_ne!(successor_before.run_id, old_run_id);
        assert!(successor_before.ended_at_unix_ms.is_none());

        record_exit_for_state(
            &mut old_state,
            "daemon_exit",
            "graceful",
            json!({"source": "delayed_old_daemon"}),
        )
        .unwrap();

        let successor_after: RunRecord = read_optional_json(Path::new(&paths.run_current_path))
            .unwrap()
            .expect("successor current-run row after old exit");
        assert_eq!(successor_after.run_id, successor_before.run_id);
        assert!(successor_after.ended_at_unix_ms.is_none());
        let exits = fs::read_to_string(&paths.exit_events_path).unwrap();
        let old_graceful = exits
            .lines()
            .filter_map(|line| serde_json::from_str::<ExitEvent>(line).ok())
            .any(|event| {
                event.run_id == old_run_id
                    && event.cause == "graceful"
                    && event.detail.get("source").and_then(Value::as_str)
                        == Some("delayed_old_daemon")
            });
        assert!(old_graceful, "old daemon exit event must remain durable");
    }

    #[test]
    fn graceful_finalization_serializes_successor_read_after_exit_write() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap();
        let predecessor = state_slot()
            .lock()
            .unwrap()
            .as_ref()
            .expect("predecessor lifecycle state")
            .run
            .clone();
        let predecessor_run_id = predecessor.run_id.clone();
        let mut successor = predecessor.clone();
        successor.run_id = "synthetic-serialized-successor".to_owned();
        successor.pid = predecessor.pid.saturating_add(1);
        successor.started_at_unix_ms = predecessor.started_at_unix_ms.saturating_add(1);
        successor.ended_at_unix_ms = None;
        successor.ended_reason = None;

        let finalization = begin_graceful_exit_finalization().unwrap();
        let contender_db_path = temp.path().to_path_buf();
        let contender_run_path = PathBuf::from(&paths.run_current_path);
        let contender_successor = successor.clone();
        let (contention_tx, contention_rx) = mpsc::channel();
        let contender = std::thread::spawn(move || {
            let lock_path = contender_db_path.join(LIFECYCLE_LOCK_FILE);
            let lock_file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .unwrap();
            let contention = fs2::FileExt::try_lock_exclusive(&lock_file)
                .expect_err("predecessor finalization must physically own lifecycle lock")
                .to_string();
            contention_tx.send(contention).unwrap();

            fs2::FileExt::lock_exclusive(&lock_file).unwrap();
            let predecessor_seen = read_optional_json::<RunRecord>(&contender_run_path)
                .unwrap()
                .expect("predecessor row after serialized lifecycle acquisition");
            write_json_atomic(&contender_run_path, &contender_successor).unwrap();
            fs2::FileExt::unlock(&lock_file).unwrap();
            predecessor_seen
        });
        let contention = contention_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("successor must independently observe the held lifecycle transaction");
        assert!(!contention.is_empty());

        record_graceful_exit_after_lifetime_lock_close(finalization, "serialized_predecessor_test")
            .unwrap();
        let predecessor_seen = contender.join().expect("join successor lifecycle writer");

        assert_eq!(predecessor_seen.run_id, predecessor_run_id);
        assert!(predecessor_seen.ended_at_unix_ms.is_some());
        assert_eq!(predecessor_seen.ended_reason.as_deref(), Some("graceful"));
        let current: RunRecord = read_optional_json(Path::new(&paths.run_current_path))
            .unwrap()
            .expect("successor current-run row");
        assert_eq!(current.run_id, successor.run_id);
        assert!(current.ended_at_unix_ms.is_none());
        let exits = fs::read_to_string(&paths.exit_events_path).unwrap();
        assert!(exits.lines().any(|line| {
            serde_json::from_str::<ExitEvent>(line).is_ok_and(|event| {
                event.run_id == predecessor_run_id
                    && event.cause == "graceful"
                    && event.detail.get("source").and_then(Value::as_str)
                        == Some("serialized_predecessor_test")
            })
        }));
    }

    fn configure_temp(temp: &tempfile::TempDir) -> DaemonLifecyclePaths {
        configure(DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some("127.0.0.1:7700".to_owned()),
            db_path: temp.path().to_path_buf(),
        })
        .unwrap()
    }

    /// Write `count` synthetic single-line tool events, each carrying a unique
    /// `idx` in its detail so records can be counted and located exactly on disk.
    fn write_synthetic_events(count: usize) {
        for idx in 0..count {
            record_context_event(ContextEvent {
                event_kind: "synthetic_rotation_probe",
                tool: "rotation_test",
                status: "recorded",
                mcp_session_id: Some(format!("session-{idx}")),
                foreground: None,
                foreground_read_error: None,
                detail: json!({ "idx": idx, "code": "SYNTHETIC_ROTATION_PROBE" }),
            })
            .unwrap();
        }
    }

    fn segment_line_count(path: &Path) -> usize {
        match fs::read_to_string(path) {
            Ok(contents) => contents.lines().count(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => panic!("read {}: {error}", path.display()),
        }
    }

    /// Total records across the active file plus every contiguous rotated
    /// segment (`.1`, `.2`, ...). The shift scheme keeps segments gap-free.
    fn total_records(active: &Path) -> usize {
        let mut total = segment_line_count(active);
        let mut index = 1;
        while segment_path(active, index).exists() {
            total += segment_line_count(&segment_path(active, index));
            index += 1;
        }
        total
    }

    fn read_all_segments_concat(active: &Path) -> String {
        let mut all = fs::read_to_string(active).unwrap_or_default();
        let mut index = 1;
        while segment_path(active, index).exists() {
            all.push_str(&fs::read_to_string(segment_path(active, index)).unwrap());
            index += 1;
        }
        all
    }

    fn legacy_line(idx: usize) -> String {
        format!(
            "{{\"event_kind\":\"legacy_probe\",\"tool\":\"legacy_tool\",\"status\":\"ok\",\"idx\":{idx},\"pad\":\"{}\"}}\n",
            "x".repeat(48)
        )
    }

    #[test]
    fn rotates_active_ledger_when_size_cap_exceeded() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure_temp(&temp);
        // 1-byte cap forces a rotation on every append after the first, so a
        // few synthetic records exercise the whole rotate-before-write path.
        set_max_segment_bytes_for_test(1);
        let active = PathBuf::from(&paths.tool_events_path);
        let seg1 = segment_path(&active, 1);

        // Before: no rotated segment exists yet.
        assert!(!seg1.exists());

        write_synthetic_events(3);

        // After: a rotated `.1` segment exists and the active file was reset to
        // a fresh single-record segment (not the pre-rotation contents).
        assert!(
            seg1.exists(),
            "rotated segment .1 must exist after the cap is exceeded"
        );
        let active_lines = segment_line_count(&active);
        assert_eq!(
            active_lines, 1,
            "active file must be reset to a fresh segment"
        );
        let total = total_records(&active);
        assert_eq!(total, 3, "every record must survive rotation");
        println!(
            "readback=rotate seg1_exists={} active_lines={active_lines} total_records={total}",
            seg1.exists()
        );
    }

    #[test]
    fn retains_at_most_max_segments_and_prunes_oldest() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure_temp(&temp);
        set_max_segment_bytes_for_test(1);
        let active = PathBuf::from(&paths.tool_events_path);

        // 9 records => 8 rotations, more than the retention cap of 5.
        write_synthetic_events(9);

        let mut rotated = 0;
        for index in 1..=(MAX_LEDGER_SEGMENTS + 3) {
            if segment_path(&active, index).exists() {
                rotated += 1;
            }
        }
        assert_eq!(
            rotated, MAX_LEDGER_SEGMENTS,
            "retention cap must bound rotated segments"
        );
        assert!(
            !segment_path(&active, MAX_LEDGER_SEGMENTS + 1).exists(),
            "segments beyond the cap must be pruned"
        );

        let total = total_records(&active);
        assert_eq!(
            total,
            MAX_LEDGER_SEGMENTS + 1,
            "only the active file plus retained segments remain"
        );

        let all = read_all_segments_concat(&active);
        assert!(!all.contains("\"idx\":0"), "oldest record must be pruned");
        assert!(!all.contains("\"idx\":2"), "oldest records must be pruned");
        assert!(all.contains("\"idx\":8"), "newest record must be retained");
        println!("readback=retention rotated={rotated} total_records={total}");
    }

    #[test]
    fn preserves_all_records_across_rotation_within_cap() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure_temp(&temp);
        set_max_segment_bytes_for_test(1);
        let active = PathBuf::from(&paths.tool_events_path);

        // 5 records => 4 rotations, within the retention cap of 5: nothing pruned.
        write_synthetic_events(5);

        let total = total_records(&active);
        assert_eq!(
            total, 5,
            "no records lost when rotations stay within the cap"
        );
        let all = read_all_segments_concat(&active);
        assert!(
            all.contains("\"idx\":0"),
            "oldest record retained within the cap"
        );
        assert!(
            all.contains("\"idx\":4"),
            "newest record retained within the cap"
        );
        println!("readback=noloss total_records={total}");
    }

    #[test]
    fn startup_reconciles_sparse_legacy_tool_segments_to_size_and_retention_cap() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join(TOOL_EVENTS_FILE);
        fs::write(
            segment_path(&active, 3),
            (0..10).map(legacy_line).collect::<String>(),
        )
        .unwrap();
        fs::write(&active, (10..50).map(legacy_line).collect::<String>()).unwrap();
        set_max_segment_bytes_for_test(512);

        let paths = configure_temp(&temp);
        let active = PathBuf::from(&paths.tool_events_path);

        assert!(
            !segment_path(&active, MAX_LEDGER_SEGMENTS + 1).exists(),
            "startup reconciliation must remove suffixes beyond retention"
        );
        let mut retained_files = 0;
        for index in 0..=MAX_LEDGER_SEGMENTS {
            let path = if index == 0 {
                active.clone()
            } else {
                segment_path(&active, index)
            };
            if path.exists() {
                retained_files += 1;
                let bytes = fs::metadata(&path).unwrap().len();
                assert!(
                    bytes <= 512,
                    "retained split segment {} exceeded cap with {bytes} bytes",
                    path.display()
                );
            }
        }
        assert!(
            retained_files <= MAX_RETAINED_LEDGER_FILES,
            "retained file count must be bounded"
        );
        let all = read_all_segments_concat(&active);
        assert!(
            !all.contains("\"idx\":0"),
            "oldest legacy records outside retention must be pruned"
        );
        assert!(
            all.contains("\"idx\":49"),
            "newest legacy record must survive startup reconciliation"
        );
        let diagnostic = diagnostic_value();
        assert_eq!(
            diagnostic
                .pointer("/ledgers/tool_events/oversized_segment_count")
                .and_then(Value::as_u64),
            Some(0)
        );
        println!(
            "readback=startup_reconcile retained_files={retained_files} total_records={}",
            total_records(&active)
        );
    }

    #[test]
    fn rotates_exit_ledger_and_retains_newest_segments() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure_temp(&temp);
        set_max_segment_bytes_for_test(1);
        let active = PathBuf::from(&paths.exit_events_path);

        for idx in 0..9 {
            record_startup_exit(
                "synthetic_exit",
                json!({
                    "idx": idx,
                    "pad": "x".repeat(128),
                }),
            )
            .unwrap();
        }

        let mut rotated = 0;
        for index in 1..=(MAX_LEDGER_SEGMENTS + 3) {
            if segment_path(&active, index).exists() {
                rotated += 1;
            }
        }
        assert_eq!(
            rotated, MAX_LEDGER_SEGMENTS,
            "exit ledger rotation must enforce the segment cap"
        );
        assert!(
            !segment_path(&active, MAX_LEDGER_SEGMENTS + 1).exists(),
            "exit ledger suffixes beyond the cap must be pruned"
        );
        let all = read_all_segments_concat(&active);
        assert!(!all.contains("\"idx\":0"), "oldest exit event pruned");
        assert!(all.contains("\"idx\":8"), "newest exit event retained");
        let diagnostic = diagnostic_value();
        assert_eq!(
            diagnostic
                .pointer("/ledgers/exit_events/rotated_segment_count")
                .and_then(Value::as_u64),
            Some(MAX_LEDGER_SEGMENTS as u64)
        );
        println!(
            "readback=exit_rotation rotated={rotated} total_records={}",
            total_records(&active)
        );
    }

    #[test]
    fn rotation_failure_preserves_existing_active_bytes_and_reports_error() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        let temp = tempfile::tempdir().unwrap();
        let paths = configure_temp(&temp);
        set_max_segment_bytes_for_test(1);
        let active = PathBuf::from(&paths.tool_events_path);
        write_synthetic_events(1);
        let before = fs::read_to_string(&active).unwrap();
        fs::create_dir(segment_path(&active, MAX_LEDGER_SEGMENTS)).unwrap();

        let result = record_context_event(ContextEvent {
            event_kind: "synthetic_rotation_failure",
            tool: "rotation_test",
            status: "recorded",
            mcp_session_id: Some("session-failure".to_owned()),
            foreground: None,
            foreground_read_error: None,
            detail: json!({ "idx": 999, "code": "SYNTHETIC_ROTATION_FAILURE" }),
        });

        assert!(result.is_err(), "rotation failure must fail the append");
        let after = fs::read_to_string(&active).unwrap();
        assert_eq!(
            after, before,
            "failed rotation must not append into the oversized active file"
        );
        let diagnostic = diagnostic_value();
        assert_eq!(
            diagnostic.get("status").and_then(Value::as_str),
            Some("error")
        );
        assert!(
            diagnostic
                .get("last_error")
                .and_then(Value::as_str)
                .is_some_and(
                    |error| error.contains("prune oldest daemon lifecycle tool_events segment")
                ),
            "last_error must name the failed rotation step"
        );
        println!(
            "readback=rotation_failure preserved_active_bytes={}",
            after.len()
        );
    }
}

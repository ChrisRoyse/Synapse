use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use synapse_core::SubsystemHealth;

const SCHEMA_VERSION: u32 = 1;
const RUN_CURRENT_FILE: &str = "daemon-run-current.json";
const TOOL_LAST_FILE: &str = "daemon-tool-last.json";
const TOOL_EVENTS_FILE: &str = "daemon-tool-events.jsonl";
const EXIT_EVENTS_FILE: &str = "daemon-exit.jsonl";

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

static STATE: OnceLock<Mutex<Option<DaemonLifecycleState>>> = OnceLock::new();
static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ToolEvent {
    schema_version: u32,
    run_id: String,
    pid: u32,
    seq: u64,
    event_kind: String,
    tool: String,
    status: String,
    started_at_unix_ms: u64,
    finished_at_unix_ms: Option<u64>,
    duration_ms: Option<u64>,
    mcp_session_id: Option<String>,
    audit_context: Option<Value>,
    audit_context_read_error: Option<Value>,
    foreground: Option<Value>,
    foreground_read_error: Option<Value>,
    session_target: Option<Value>,
    session_target_read_error: Option<Value>,
    effective_target: Option<Value>,
    error: Option<Value>,
    panic: Option<Value>,
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
    /// Size cap the active tool-event segment may reach before rotation. Seeded
    /// from [`MAX_LEDGER_SEGMENT_BYTES`]; overridable only in tests via
    /// [`set_max_segment_bytes_for_test`] to force rotation without writing MiB.
    max_segment_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct ToolCallGuard {
    seq: u64,
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

    let previous_run = read_optional_json::<RunRecord>(Path::new(&paths.run_current_path))
        .with_context(|| {
            format!(
                "read daemon lifecycle current run {}",
                paths.run_current_path
            )
        })?;
    let previous_last_tool = read_optional_json::<ToolEvent>(Path::new(&paths.tool_last_path))
        .with_context(|| format!("read daemon lifecycle last tool {}", paths.tool_last_path))?;

    let run = RunRecord {
        schema_version: SCHEMA_VERSION,
        run_id: format!("{}-{}", now_unix_ms(), std::process::id()),
        pid: std::process::id(),
        mode: config.mode.to_owned(),
        bind_addr: config.bind_addr,
        db_path: paths.db_path.clone(),
        started_at_unix_ms: now_unix_ms(),
        ended_at_unix_ms: None,
        ended_reason: None,
    };

    if let Some(previous) = previous_run.as_ref()
        && previous.ended_at_unix_ms.is_none()
    {
        append_json_line(
            Path::new(&paths.exit_events_path),
            &ExitEvent {
                schema_version: SCHEMA_VERSION,
                run_id: previous.run_id.clone(),
                pid: previous.pid,
                event_kind: "previous_run_unclean".to_owned(),
                cause: "process_missing_on_startup".to_owned(),
                detail: json!({
                    "new_pid": std::process::id(),
                    "new_run_id": run.run_id,
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
    // Seed the in-memory ledger size from the file that survives across daemon
    // restarts, so the very first append after startup rotates when the active
    // segment was already at the cap. A missing file simply starts at zero.
    let tool_events_bytes = match fs::metadata(&paths.tool_events_path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("stat daemon tool events {}", paths.tool_events_path));
        }
    };
    let state = DaemonLifecycleState {
        run,
        paths: paths.clone(),
        in_flight: BTreeMap::new(),
        seq: 0,
        last_error: None,
        tool_events_bytes,
        max_segment_bytes: MAX_LEDGER_SEGMENT_BYTES,
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
    write_tool_event(state, &event)?;
    state.in_flight.insert(seq, event);
    Ok(ToolCallGuard { seq })
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
        self,
        effective_target: Option<Value>,
    ) -> anyhow::Result<()> {
        finish_tool_call(self.seq, "ok", None, None, effective_target)
    }

    pub(crate) fn finish_error(self, error: Value) -> anyhow::Result<()> {
        finish_tool_call(self.seq, "error", Some(error), None, None)
    }

    pub(crate) fn finish_error_with_effective_target(
        self,
        error: Value,
        effective_target: Option<Value>,
    ) -> anyhow::Result<()> {
        finish_tool_call(self.seq, "error", Some(error), None, effective_target)
    }

    pub(crate) fn finish_panic(self, panic: Value) -> anyhow::Result<()> {
        finish_tool_call(self.seq, "panic", None, Some(panic), None)
    }
}

pub(crate) fn record_graceful_exit(source: &'static str) -> anyhow::Result<()> {
    record_exit(
        "daemon_exit",
        "graceful",
        json!({
            "source": source,
        }),
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
        Some(state) => json!({
            "status": if state.last_error.is_some() { "error" } else { "ok" },
            "run_id": state.run.run_id,
            "pid": state.run.pid,
            "paths": state.paths,
            "last_error": state.last_error,
            "in_flight_count": state.in_flight.len(),
        }),
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

fn finish_tool_call(
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
    let Some(mut event) = state.in_flight.remove(&seq) else {
        bail!("daemon lifecycle in-flight tool event {seq} is missing");
    };
    let finished_at_unix_ms = now_unix_ms();
    status.clone_into(&mut event.status);
    event.finished_at_unix_ms = Some(finished_at_unix_ms);
    event.duration_ms = Some(finished_at_unix_ms.saturating_sub(event.started_at_unix_ms));
    event.effective_target = effective_target;
    event.error = error;
    event.panic = panic;
    write_tool_event(state, &event)
}

fn record_panic(info: &std::panic::PanicHookInfo<'_>) -> anyhow::Result<()> {
    let location = info.location().map(|location| {
        json!({
            "file": location.file(),
            "line": location.line(),
            "column": location.column(),
        })
    });
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned());
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
    let guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_ref() else {
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
    append_json_line(Path::new(&state.paths.exit_events_path), &event).with_context(|| {
        format!(
            "append daemon diagnostic event {}",
            state.paths.exit_events_path
        )
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
    append_json_line(Path::new(&state.paths.exit_events_path), &event)
        .with_context(|| format!("append daemon exit event {}", state.paths.exit_events_path))?;
    write_json_atomic(Path::new(&state.paths.run_current_path), &run).with_context(|| {
        format!(
            "write daemon ended current run {}",
            state.paths.run_current_path
        )
    })?;
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

/// Append one JSON line to the active `daemon-tool-events.jsonl`, rotating the
/// segment first when this record would push it past the size cap.
///
/// Size is tracked with the in-memory [`DaemonLifecycleState::tool_events_bytes`]
/// counter, so the hot path performs no per-write stat. The rotation runs before
/// any handle to the active file is opened; because appends reopen and close the
/// handle per call, the active file is guaranteed to have no open write handle at
/// rename time, which Windows requires. An empty active file is never rotated, so
/// a single record larger than the cap is still written (to an empty segment)
/// rather than lost.
fn append_tool_event(state: &mut DaemonLifecycleState, event: &ToolEvent) -> anyhow::Result<()> {
    let events_path = state.paths.tool_events_path.clone();
    let path = Path::new(&events_path);

    let mut line =
        serde_json::to_vec(event).with_context(|| format!("encode JSON line {events_path}"))?;
    line.push(b'\n');
    let line_len = u64::try_from(line.len()).unwrap_or(u64::MAX);

    if state.tool_events_bytes > 0
        && state.tool_events_bytes.saturating_add(line_len) > state.max_segment_bytes
    {
        if let Err(error) = rotate_tool_events(path) {
            let detail = format!("{error:#}");
            tracing::error!(
                code = "DAEMON_LEDGER_ROTATE_FAILED",
                tool_events_path = %events_path,
                active_bytes = state.tool_events_bytes,
                next_record_bytes = line_len,
                max_segment_bytes = state.max_segment_bytes,
                detail = %detail,
                "daemon lifecycle tool-event ledger rotation failed"
            );
            return Err(error);
        }
        state.tool_events_bytes = 0;
        tracing::info!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_ROTATED",
            tool_events_path = %events_path,
            max_segment_bytes = state.max_segment_bytes,
            max_segments = MAX_LEDGER_SEGMENTS,
            "daemon lifecycle tool-event ledger rotated"
        );
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open append {events_path}"))?;
    file.write_all(&line)
        .with_context(|| format!("write daemon tool event {events_path}"))?;
    file.flush()
        .with_context(|| format!("flush {events_path}"))?;
    file.sync_data()
        .with_context(|| format!("sync {events_path}"))?;
    state.tool_events_bytes = state.tool_events_bytes.saturating_add(line_len);
    Ok(())
}

/// Rotate the active daemon tool-event ledger using a fixed shift scheme.
///
/// `daemon-tool-events.jsonl.1` is always the most recently rotated segment and
/// `daemon-tool-events.jsonl.{MAX_LEDGER_SEGMENTS}` the oldest. On each rotation
/// the oldest slot is pruned first, remaining segments shift up by one
/// (`.1`->`.2`, ...), and the active file is renamed into `.1`. That bounds the
/// number of rotated files at [`MAX_LEDGER_SEGMENTS`]. Every step runs with no
/// open handle on the files involved (required on Windows). All failures are
/// propagated so the caller never keeps appending to an oversized ledger.
fn rotate_tool_events(active: &Path) -> anyhow::Result<()> {
    // Prune the oldest retained segment so the shift below cannot exceed the cap.
    let oldest = segment_path(active, MAX_LEDGER_SEGMENTS);
    match fs::remove_file(&oldest) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "prune oldest daemon tool-event segment {}",
                    oldest.display()
                )
            });
        }
    }
    // Shift existing segments up: .(MAX-1)->.MAX, ..., .1->.2.
    for index in (1..MAX_LEDGER_SEGMENTS).rev() {
        let from = segment_path(active, index);
        let to = segment_path(active, index + 1);
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "shift daemon tool-event segment {} to {}",
                        from.display(),
                        to.display()
                    )
                });
            }
        }
    }
    // Move the (closed) active file into the newest rotated slot.
    let newest = segment_path(active, 1);
    fs::rename(active, &newest).with_context(|| {
        format!(
            "rotate active daemon tool-event ledger {} to {}",
            active.display(),
            newest.display()
        )
    })
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

#[cfg(test)]
pub(crate) fn set_max_segment_bytes_for_test(bytes: u64) {
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
    format!(
        "run_id={} pid={} run_current_path={} tool_last_path={} tool_events_path={} exit_events_path={} in_flight_count={} last_error={}",
        state.run.run_id,
        state.run.pid,
        state.paths.run_current_path,
        state.paths.tool_last_path,
        state.paths.tool_events_path,
        state.paths.exit_events_path,
        state.in_flight.len(),
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

fn append_json_line<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open append {}", path.display()))?;
    serde_json::to_writer(&mut file, value)
        .with_context(|| format!("encode JSON line {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("write newline {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("sync {}", path.display()))
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
    use serde_json::json;

    use super::*;

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
        let _guard = begin_tool_call(ToolCallStart {
            tool: "observe".to_owned(),
            mcp_session_id: Some("session-crash".to_owned()),
            audit_context: None,
            audit_context_read_error: None,
            foreground: Some(json!({"hwnd": 99})),
            foreground_read_error: None,
            session_target: None,
            session_target_read_error: None,
        })
        .unwrap();

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
}

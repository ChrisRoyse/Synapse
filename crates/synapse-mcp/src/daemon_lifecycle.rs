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
    let state = DaemonLifecycleState {
        run,
        paths: paths.clone(),
        in_flight: BTreeMap::new(),
        seq: 0,
        last_error: None,
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
        error: None,
        panic: None,
        detail: Some(input.detail),
    };
    write_tool_event(state, &event)?;
    Ok(seq)
}

impl ToolCallGuard {
    pub(crate) fn finish_ok(self) -> anyhow::Result<()> {
        finish_tool_call(self.seq, "ok", None, None)
    }

    pub(crate) fn finish_error(self, error: Value) -> anyhow::Result<()> {
        finish_tool_call(self.seq, "error", Some(error), None)
    }

    pub(crate) fn finish_panic(self, panic: Value) -> anyhow::Result<()> {
        finish_tool_call(self.seq, "panic", None, Some(panic))
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
    match write_tool_event_inner(&state.paths, event) {
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

fn write_tool_event_inner(paths: &DaemonLifecyclePaths, event: &ToolEvent) -> anyhow::Result<()> {
    append_json_line(Path::new(&paths.tool_events_path), event)
        .with_context(|| format!("append daemon tool event {}", paths.tool_events_path))?;
    write_json_atomic(Path::new(&paths.tool_last_path), event)
        .with_context(|| format!("write daemon last tool {}", paths.tool_last_path))
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
        guard.finish_ok().unwrap();

        let last: ToolEvent = read_optional_json(Path::new(&paths.tool_last_path))
            .unwrap()
            .unwrap();
        assert_eq!(last.tool, "health");
        assert_eq!(last.status, "ok");
        assert_eq!(last.mcp_session_id.as_deref(), Some("session-a"));

        let events = fs::read_to_string(&paths.tool_events_path).unwrap();
        assert_eq!(events.lines().count(), 2);
        assert!(events.contains("\"status\":\"started\""));
        assert!(events.contains("\"status\":\"ok\""));
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
}

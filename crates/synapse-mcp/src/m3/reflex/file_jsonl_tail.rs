use std::{
    fmt,
    fs::File,
    io::{BufRead, BufReader},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use chrono::Utc;
use rmcp::ErrorData;
use serde_json::{Value, json};
use synapse_core::{Event, EventSource, error_codes};
use synapse_reflex::{EventBus, ReflexActivation};
use tokio_util::sync::CancellationToken;

use crate::{m1::mcp_error, m3::SharedM3State};

use super::common::{FILE_JSONL_TAIL_EVENT_KIND, ValidatedFileJsonlTailWhen};

static NEXT_FILE_JSONL_TAIL_EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

pub(crate) struct FileJsonlTailWatcher {
    cancel: CancellationToken,
    start: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl fmt::Debug for FileJsonlTailWatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileJsonlTailWatcher")
            .field("cancelled", &self.cancel.is_cancelled())
            .field("started", &self.start.is_cancelled())
            .field("task_finished", &self.task.is_finished())
            .finish()
    }
}

pub(crate) struct PreparedFileJsonlTailWatcher {
    reflex_id: String,
    cancel: CancellationToken,
    start: CancellationToken,
}

pub(crate) struct PreparedFileJsonlTailWatcherCancellation {
    reflex_id: String,
    watcher: Option<FileJsonlTailWatcher>,
}

impl PreparedFileJsonlTailWatcherCancellation {
    pub(crate) fn commit(mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.cancel();
        }
    }

    pub(crate) fn rollback(mut self, state: &SharedM3State) -> Result<(), ErrorData> {
        let Some(watcher) = self.watcher.take() else {
            return Ok(());
        };
        let mut state = state.lock().map_err(|_error| {
            watcher.cancel();
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "m3 state lock poisoned while rolling back prepared file_jsonl_tail cancellation",
            )
        })?;
        if state.file_jsonl_tail_watchers.contains_key(&self.reflex_id) {
            watcher.cancel();
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "FILE_JSONL_TAIL_WATCHER_ROLLBACK_IDENTITY_CONFLICT: reflex_id={}; remediation=inspect the exact watcher generation before retrying cancellation",
                    self.reflex_id
                ),
            ));
        }
        state
            .file_jsonl_tail_watchers
            .insert(self.reflex_id, watcher);
        Ok(())
    }
}

impl PreparedFileJsonlTailWatcher {
    pub(crate) fn activate(self) {
        self.start.cancel();
    }

    pub(crate) fn rollback(self, state: &SharedM3State) -> Result<(), ErrorData> {
        self.cancel.cancel();
        let mut state = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "m3 state lock poisoned while rolling back prepared file_jsonl_tail watcher",
            )
        })?;
        state.file_jsonl_tail_watchers.remove(&self.reflex_id);
        Ok(())
    }
}

impl FileJsonlTailWatcher {
    fn cancel(&self) {
        self.cancel.cancel();
    }
}

pub(crate) fn install_recovered_file_jsonl_tail_watcher(
    state: &SharedM3State,
    reflex_id: String,
    activation: ReflexActivation,
    event_bus: EventBus,
) -> Result<(), ErrorData> {
    let ReflexActivation::FileJsonlTail {
        host,
        path,
        json_path,
        json_pointer,
        event_data_pointer,
        equals,
        min_lines,
        poll_interval_ms,
        local_host,
        stop_after_first_match,
    } = activation;
    let spec = ValidatedFileJsonlTailWhen {
        host,
        path,
        json_path,
        json_pointer,
        event_data_pointer,
        equals,
        min_lines,
        poll_interval_ms,
        local_host,
    };
    install_validated_file_jsonl_tail_watcher(
        state,
        reflex_id,
        spec,
        stop_after_first_match,
        event_bus,
    )
}

fn install_validated_file_jsonl_tail_watcher(
    state: &SharedM3State,
    reflex_id: String,
    spec: ValidatedFileJsonlTailWhen,
    stop_after_first_match: bool,
    event_bus: EventBus,
) -> Result<(), ErrorData> {
    let start = CancellationToken::new();
    start.cancel();
    install_file_jsonl_tail_watcher_with_gate(
        state,
        reflex_id,
        spec,
        stop_after_first_match,
        event_bus,
        start,
    )?;
    Ok(())
}

pub(crate) fn prepare_file_jsonl_tail_watcher(
    state: &SharedM3State,
    reflex_id: String,
    activation: ReflexActivation,
    event_bus: EventBus,
) -> Result<PreparedFileJsonlTailWatcher, ErrorData> {
    let ReflexActivation::FileJsonlTail {
        host,
        path,
        json_path,
        json_pointer,
        event_data_pointer,
        equals,
        min_lines,
        poll_interval_ms,
        local_host,
        stop_after_first_match,
    } = activation;
    let spec = ValidatedFileJsonlTailWhen {
        host,
        path,
        json_path,
        json_pointer,
        event_data_pointer,
        equals,
        min_lines,
        poll_interval_ms,
        local_host,
    };
    let start = CancellationToken::new();
    let cancel = install_file_jsonl_tail_watcher_with_gate(
        state,
        reflex_id.clone(),
        spec,
        stop_after_first_match,
        event_bus,
        start.clone(),
    )?;
    Ok(PreparedFileJsonlTailWatcher {
        reflex_id,
        cancel,
        start,
    })
}

fn install_file_jsonl_tail_watcher_with_gate(
    state: &SharedM3State,
    reflex_id: String,
    spec: ValidatedFileJsonlTailWhen,
    stop_after_first_match: bool,
    event_bus: EventBus,
    start: CancellationToken,
) -> Result<CancellationToken, ErrorData> {
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_file_jsonl_tail_watcher(
        state.clone(),
        reflex_id.clone(),
        spec.clone(),
        stop_after_first_match,
        event_bus,
        cancel.clone(),
        start.clone(),
    ));
    let watcher = FileJsonlTailWatcher {
        cancel: cancel.clone(),
        start,
        task,
    };
    let mut state = state.lock().map_err(|_error| {
        watcher.cancel();
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "m3 state lock poisoned while installing file_jsonl_tail watcher",
        )
    })?;
    if state.file_jsonl_tail_watchers.contains_key(&reflex_id) {
        watcher.cancel();
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "FILE_JSONL_TAIL_WATCHER_IDENTITY_CONFLICT: reflex_id={reflex_id}; remediation=cancel the exact existing reflex before registering a replacement"
            ),
        ));
    }
    state
        .file_jsonl_tail_watchers
        .insert(reflex_id.clone(), watcher);
    tracing::info!(
        code = "FILE_JSONL_TAIL_WATCHER_INSTALLED",
        reflex_id = %reflex_id,
        host = %spec.host,
        path = %spec.path,
        json_path = %spec.json_path,
        min_lines = spec.min_lines,
        poll_interval_ms = spec.poll_interval_ms,
        stop_after_first_match,
        "installed file_jsonl_tail watcher"
    );
    Ok(cancel)
}

pub(crate) fn file_jsonl_tail_watcher_installed(
    state: &SharedM3State,
    reflex_id: &str,
) -> Result<bool, ErrorData> {
    state
        .lock()
        .map(|state| state.file_jsonl_tail_watchers.contains_key(reflex_id))
        .map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "m3 state lock poisoned while reading file_jsonl_tail watcher state",
            )
        })
}

pub(crate) fn cancel_file_jsonl_tail_watcher(
    state: &SharedM3State,
    reflex_id: &str,
) -> Result<bool, ErrorData> {
    let mut state = state.lock().map_err(|_error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "m3 state lock poisoned while cancelling file_jsonl_tail watcher",
        )
    })?;
    let Some(watcher) = state.file_jsonl_tail_watchers.remove(reflex_id) else {
        return Ok(false);
    };
    watcher.cancel();
    Ok(true)
}

pub(crate) fn prepare_file_jsonl_tail_watcher_cancellation(
    state: &SharedM3State,
    reflex_id: &str,
) -> Result<PreparedFileJsonlTailWatcherCancellation, ErrorData> {
    let watcher = state
        .lock()
        .map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "m3 state lock poisoned while preparing file_jsonl_tail cancellation",
            )
        })?
        .file_jsonl_tail_watchers
        .remove(reflex_id);
    Ok(PreparedFileJsonlTailWatcherCancellation {
        reflex_id: reflex_id.to_owned(),
        watcher,
    })
}

async fn run_file_jsonl_tail_watcher(
    state: SharedM3State,
    reflex_id: String,
    spec: ValidatedFileJsonlTailWhen,
    stop_after_first_match: bool,
    event_bus: EventBus,
    cancel: CancellationToken,
    start: CancellationToken,
) {
    tokio::select! {
        () = cancel.cancelled() => return,
        () = start.cancelled() => {}
    }
    let mut interval = tokio::time::interval(Duration::from_millis(spec.poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_published = None::<MatchSignature>;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = interval.tick() => {}
        }
        let spec_for_read = spec.clone();
        let read_result =
            tokio::task::spawn_blocking(move || read_file_jsonl_tail_snapshot(&spec_for_read))
                .await;
        let snapshot = match read_result {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => {
                tracing::warn!(
                    code = "FILE_JSONL_TAIL_READ_FAILED",
                    reflex_id = %reflex_id,
                    host = %spec.host,
                    path = %spec.path,
                    detail = %error,
                    "file_jsonl_tail watcher read failed"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    code = "FILE_JSONL_TAIL_READ_JOIN_FAILED",
                    reflex_id = %reflex_id,
                    host = %spec.host,
                    path = %spec.path,
                    detail = %error,
                    "file_jsonl_tail watcher read task failed"
                );
                continue;
            }
        };

        if let Some(parse_error) = &snapshot.parse_error {
            tracing::debug!(
                code = "FILE_JSONL_TAIL_LAST_LINE_INVALID_JSON",
                reflex_id = %reflex_id,
                host = %spec.host,
                path = %spec.path,
                line_count = snapshot.line_count,
                detail = %parse_error,
                "file_jsonl_tail watcher last line is not valid JSON"
            );
        }

        let Some(signature) = matching_signature(&spec, &snapshot) else {
            continue;
        };
        if last_published.as_ref() == Some(&signature) {
            continue;
        }
        last_published = Some(signature);
        publish_file_jsonl_tail_event(&event_bus, &reflex_id, &spec, &snapshot);
        if stop_after_first_match {
            break;
        }
    }
    remove_watcher_if_present(&state, &reflex_id);
    tracing::info!(
        code = "FILE_JSONL_TAIL_WATCHER_STOPPED",
        reflex_id = %reflex_id,
        host = %spec.host,
        path = %spec.path,
        "file_jsonl_tail watcher stopped"
    );
}

fn remove_watcher_if_present(state: &SharedM3State, reflex_id: &str) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    state.file_jsonl_tail_watchers.remove(reflex_id);
}

#[derive(Clone, Debug, PartialEq)]
struct MatchSignature {
    line_count: u64,
    last_raw: Option<String>,
}

fn matching_signature(
    spec: &ValidatedFileJsonlTailWhen,
    snapshot: &FileJsonlTailSnapshot,
) -> Option<MatchSignature> {
    if snapshot.line_count < spec.min_lines {
        return None;
    }
    let last_json = snapshot.last_json.as_ref()?;
    if last_json.pointer(&spec.json_pointer) != Some(&spec.equals) {
        return None;
    }
    Some(MatchSignature {
        line_count: snapshot.line_count,
        last_raw: snapshot.last_raw.clone(),
    })
}

fn publish_file_jsonl_tail_event(
    event_bus: &EventBus,
    reflex_id: &str,
    spec: &ValidatedFileJsonlTailWhen,
    snapshot: &FileJsonlTailSnapshot,
) {
    let seq = NEXT_FILE_JSONL_TAIL_EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    let event = Event {
        seq,
        at: Utc::now(),
        source: EventSource::Filesystem,
        kind: FILE_JSONL_TAIL_EVENT_KIND.to_owned(),
        data: json!({
            "reflex_id": reflex_id,
            "host": spec.host,
            "path": spec.path,
            "line_count": snapshot.line_count,
            "last_json": snapshot.last_json,
            "predicate": {
                "json_path": spec.json_path,
                "equals": spec.equals,
            },
            "min_lines": spec.min_lines,
            "read": {
                "transport": snapshot.transport,
                "exists": snapshot.exists,
                "matched": true,
            }
        }),
        correlations: Vec::new(),
    };
    let report = event_bus.publish(event);
    tracing::info!(
        code = "FILE_JSONL_TAIL_MATCH_PUBLISHED",
        reflex_id = %reflex_id,
        host = %spec.host,
        path = %spec.path,
        line_count = snapshot.line_count,
        json_path = %spec.json_path,
        matched_subscribers = report.matched,
        queued_subscribers = report.queued,
        dropped_subscriber_events = report.dropped,
        "file_jsonl_tail watcher published matching filesystem event"
    );
}

#[derive(Clone, Debug)]
struct FileJsonlTailSnapshot {
    transport: &'static str,
    exists: bool,
    line_count: u64,
    last_raw: Option<String>,
    last_json: Option<Value>,
    parse_error: Option<String>,
}

fn read_file_jsonl_tail_snapshot(
    spec: &ValidatedFileJsonlTailWhen,
) -> Result<FileJsonlTailSnapshot, String> {
    if spec.local_host {
        return read_local_file_jsonl_tail(&spec.path);
    }
    read_remote_file_jsonl_tail(spec)
}

fn read_local_file_jsonl_tail(path: &str) -> Result<FileJsonlTailSnapshot, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileJsonlTailSnapshot {
                transport: "local",
                exists: false,
                line_count: 0,
                last_raw: None,
                last_json: None,
                parse_error: None,
            });
        }
        Err(error) => return Err(format!("open local JSONL path failed: {error}")),
    };
    let reader = BufReader::new(file);
    let mut line_count = 0_u64;
    let mut last_raw = None::<String>;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("read local JSONL line failed: {error}"))?;
        line_count = line_count.saturating_add(1);
        last_raw = Some(line);
    }
    Ok(snapshot_from_tail("local", true, line_count, last_raw))
}

fn read_remote_file_jsonl_tail(
    spec: &ValidatedFileJsonlTailWhen,
) -> Result<FileJsonlTailSnapshot, String> {
    let quoted_path = shell_single_quote(&spec.path);
    let script = format!(
        "p={quoted_path}; if [ ! -f \"$p\" ]; then printf 'missing\\t0\\n'; exit 0; fi; c=$(wc -l < \"$p\") || exit 2; printf 'ok\\t%s\\n' \"$c\"; tail -n 1 -- \"$p\""
    );
    let remote_command = format!("sh -lc {}", shell_single_quote(&script));
    let output = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            &spec.host,
            &remote_command,
        ])
        .output()
        .map_err(|error| format!("launch ssh failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ssh tail command failed with status {:?}: {}",
            output.status.code(),
            stderr.trim()
        ));
    }
    parse_remote_tail_stdout(&output.stdout)
}

fn parse_remote_tail_stdout(stdout: &[u8]) -> Result<FileJsonlTailSnapshot, String> {
    let text = String::from_utf8_lossy(stdout);
    let mut parts = text.splitn(2, '\n');
    let header = parts.next().unwrap_or_default().trim_end_matches('\r');
    let rest = parts.next();
    let mut fields = header.split('\t');
    let status = fields.next().unwrap_or_default();
    let count = fields
        .next()
        .ok_or_else(|| format!("remote tail header missing count: {header:?}"))?
        .parse::<u64>()
        .map_err(|error| format!("remote tail line count invalid: {error}"))?;
    match status {
        "missing" => Ok(FileJsonlTailSnapshot {
            transport: "ssh",
            exists: false,
            line_count: count,
            last_raw: None,
            last_json: None,
            parse_error: None,
        }),
        "ok" => {
            let last_raw = rest
                .map(|value| value.trim_end_matches(['\r', '\n']).to_owned())
                .filter(|value| !value.is_empty());
            Ok(snapshot_from_tail("ssh", true, count, last_raw))
        }
        other => Err(format!("remote tail header status invalid: {other:?}")),
    }
}

fn snapshot_from_tail(
    transport: &'static str,
    exists: bool,
    line_count: u64,
    last_raw: Option<String>,
) -> FileJsonlTailSnapshot {
    let (last_json, parse_error) = match &last_raw {
        Some(line) => match serde_json::from_str::<Value>(line) {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error.to_string())),
        },
        None => (None, None),
    };
    FileJsonlTailSnapshot {
        transport,
        exists,
        line_count,
        last_raw,
        last_json,
        parse_error,
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

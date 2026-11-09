use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use rmcp::model::ErrorCode;
use serde::Deserialize;
use serde_json::{Value, json};
use synapse_core::error_codes;

use crate::{daemon_lifecycle, server::ErrorData};

use super::{
    AUDIT_SOT, AUDIT_TOOL,
    errors::{io_error, lifecycle_corrupt_error, lifecycle_oversized_error},
    types::{AuditLifecycleRowSummary, AuditLifecycleTailParams, AuditLifecycleTailResponse},
    util::{nested_string, prefixed_sha256, sha256_text, string_field},
    validation::validate_lifecycle_params,
};

#[derive(Debug, Deserialize)]
struct LifecycleFilterProbe {
    event_kind: Option<String>,
    status: Option<String>,
    tool: Option<String>,
}
pub(super) fn lifecycle_path(key: &str) -> Result<PathBuf, ErrorData> {
    let diagnostic = daemon_lifecycle::diagnostic_value();
    let path = diagnostic
        .get("paths")
        .and_then(|paths| paths.get(key))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ErrorData::new(
                ErrorCode(-32099),
                format!("audit operation needs daemon lifecycle {key}, but it is unavailable"),
                Some(json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "tool": AUDIT_TOOL,
                    "operation": key,
                    "source_id": "daemon_lifecycle::diagnostic_value",
                    "source_of_truth": AUDIT_SOT,
                    "remediation": "repair daemon lifecycle configuration and retry audit lifecycle query",
                })),
            )
        })?;
    Ok(PathBuf::from(path))
}

pub(super) fn read_lifecycle_tail(
    path: &Path,
    params: &AuditLifecycleTailParams,
) -> Result<AuditLifecycleTailResponse, ErrorData> {
    validate_lifecycle_params(params)?;
    let segment_paths =
        daemon_lifecycle::lifecycle_ledger_paths_oldest_first(path).map_err(|error| {
            ErrorData::new(
                ErrorCode(-32099),
                format!(
                    "audit operation=lifecycle_tail could not discover daemon lifecycle ledger segments for {}: {error:#}",
                    path.display()
                ),
                Some(json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "tool": AUDIT_TOOL,
                    "operation": "lifecycle_tail",
                    "source_id": path.display().to_string(),
                    "source_of_truth": AUDIT_SOT,
                    "remediation": "inspect daemon lifecycle segment paths and file permissions",
                })),
            )
        })?;
    if segment_paths.is_empty() {
        File::open(path).map_err(|error| {
            io_error(
                AUDIT_TOOL,
                "lifecycle_tail",
                &path.display().to_string(),
                AUDIT_SOT,
                error,
                "inspect daemon lifecycle paths and file permissions",
            )
        })?;
    }
    let mut state = LifecycleTailState::new(params);
    for segment_path in &segment_paths {
        read_lifecycle_segment(segment_path, params, &mut state)?;
    }
    let rows: Vec<_> = state.rows.into_iter().rev().collect();
    Ok(AuditLifecycleTailResponse {
        path: path.display().to_string(),
        segment_count: segment_paths.len(),
        limit: params.limit,
        max_line_bytes: params.max_line_bytes,
        total_lines_read: state.total_lines_read,
        matched_lines_seen: state.matched_lines_seen,
        oversized_lines_seen: state.oversized_lines_seen,
        oversized_lines_skipped: state.oversized_lines_skipped,
        returned_count: rows.len(),
        rows,
    })
}

struct LifecycleTailState {
    total_lines_read: u64,
    matched_lines_seen: u64,
    oversized_lines_seen: u64,
    oversized_lines_skipped: u64,
    rows: VecDeque<AuditLifecycleRowSummary>,
}

impl LifecycleTailState {
    fn new(params: &AuditLifecycleTailParams) -> Self {
        Self {
            total_lines_read: 0,
            matched_lines_seen: 0,
            oversized_lines_seen: 0,
            oversized_lines_skipped: 0,
            rows: VecDeque::with_capacity(params.limit),
        }
    }
}

fn read_lifecycle_segment(
    path: &Path,
    params: &AuditLifecycleTailParams,
    state: &mut LifecycleTailState,
) -> Result<(), ErrorData> {
    let file = File::open(path).map_err(|error| {
        io_error(
            AUDIT_TOOL,
            "lifecycle_tail",
            &path.display().to_string(),
            AUDIT_SOT,
            error,
            "inspect daemon lifecycle paths and file permissions",
        )
    })?;
    let reader = BufReader::new(file);
    for line in reader.split(b'\n') {
        let mut bytes = line.map_err(|error| {
            io_error(
                AUDIT_TOOL,
                "lifecycle_tail",
                &path.display().to_string(),
                AUDIT_SOT,
                error,
                "inspect daemon lifecycle ledger readability",
            )
        })?;
        state.total_lines_read = state.total_lines_read.saturating_add(1);
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        if bytes.is_empty() {
            return Err(lifecycle_corrupt_error(
                path,
                state.total_lines_read,
                "empty JSONL line",
            ));
        }
        if bytes.len() > params.max_line_bytes {
            state.oversized_lines_seen = state.oversized_lines_seen.saturating_add(1);
            let probe: LifecycleFilterProbe = serde_json::from_slice(&bytes).map_err(|error| {
                lifecycle_corrupt_error(
                    path,
                    state.total_lines_read,
                    format!("oversized_row JSON decode failed: {error}"),
                )
            })?;
            if !lifecycle_probe_matches(&probe, params) {
                state.oversized_lines_skipped = state.oversized_lines_skipped.saturating_add(1);
                continue;
            }
            return Err(lifecycle_oversized_error(
                path,
                state.total_lines_read,
                bytes.len(),
                params.max_line_bytes,
                &probe.tool,
                &probe.status,
                &probe.event_kind,
            ));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            lifecycle_corrupt_error(
                path,
                state.total_lines_read,
                format!("JSON decode failed: {error}"),
            )
        })?;
        if lifecycle_matches(&value, params) {
            state.matched_lines_seen = state.matched_lines_seen.saturating_add(1);
            if state.rows.len() == params.limit {
                state.rows.pop_front();
            }
            state.rows.push_back(summarize_lifecycle_row(
                state.total_lines_read,
                &bytes,
                &value,
            ));
        }
    }
    Ok(())
}

fn lifecycle_probe_matches(
    probe: &LifecycleFilterProbe,
    params: &AuditLifecycleTailParams,
) -> bool {
    params
        .tool
        .as_deref()
        .is_none_or(|tool| probe.tool.as_deref() == Some(tool))
        && params
            .status
            .as_deref()
            .is_none_or(|status| probe.status.as_deref() == Some(status))
        && params
            .event_kind
            .as_deref()
            .is_none_or(|event_kind| probe.event_kind.as_deref() == Some(event_kind))
}

fn lifecycle_matches(value: &Value, params: &AuditLifecycleTailParams) -> bool {
    params
        .tool
        .as_deref()
        .is_none_or(|tool| string_field(value, "tool").as_deref() == Some(tool))
        && params
            .status
            .as_deref()
            .is_none_or(|status| string_field(value, "status").as_deref() == Some(status))
        && params.event_kind.as_deref().is_none_or(|event_kind| {
            string_field(value, "event_kind").as_deref() == Some(event_kind)
        })
}

fn summarize_lifecycle_row(line_no: u64, bytes: &[u8], value: &Value) -> AuditLifecycleRowSummary {
    let last_tool_event = value.get("last_tool_event");
    let mcp_session_id = string_field(value, "mcp_session_id");
    AuditLifecycleRowSummary {
        line_no,
        raw_len_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        raw_sha256: prefixed_sha256(bytes),
        schema_version: value.get("schema_version").and_then(Value::as_u64),
        run_id: string_field(value, "run_id"),
        pid: value.get("pid").and_then(Value::as_u64),
        seq: value.get("seq").and_then(Value::as_u64),
        event_kind: string_field(value, "event_kind"),
        tool: string_field(value, "tool"),
        status: string_field(value, "status"),
        cause: string_field(value, "cause"),
        started_at_unix_ms: value.get("started_at_unix_ms").and_then(Value::as_u64),
        finished_at_unix_ms: value.get("finished_at_unix_ms").and_then(Value::as_u64),
        duration_ms: value.get("duration_ms").and_then(Value::as_u64),
        recorded_at_unix_ms: value.get("recorded_at_unix_ms").and_then(Value::as_u64),
        mcp_session_id_present: mcp_session_id.is_some(),
        mcp_session_id_sha256: mcp_session_id.as_deref().map(sha256_text),
        error_code: nested_string(value, &["error", "data", "code"])
            .or_else(|| nested_string(value, &["error", "code"])),
        panic_present: !value.get("panic").is_none_or(Value::is_null),
        detail_code: nested_string(value, &["detail", "code"]),
        in_flight_count: value
            .get("in_flight_tool_events")
            .and_then(Value::as_array)
            .map(|items| u64::try_from(items.len()).unwrap_or(u64::MAX)),
        last_tool: last_tool_event.and_then(|event| string_field(event, "tool")),
        last_tool_status: last_tool_event.and_then(|event| string_field(event, "status")),
    }
}

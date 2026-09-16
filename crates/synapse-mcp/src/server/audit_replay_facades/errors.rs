use std::path::Path;

use rmcp::model::ErrorCode;
use serde_json::{Value, json};
use synapse_core::error_codes;

use crate::server::ErrorData;

use super::{AUDIT_SOT, AUDIT_TOOL, REPLAY_SOT, REPLAY_TOOL};
pub(super) fn invalid_operation(
    tool: &'static str,
    operation: &str,
    allowed: &[&'static str],
    source_of_truth: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32602),
        format!("{tool} operation={operation} is invalid"),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_id": "operation",
            "source_of_truth": source_of_truth,
            "allowed_operations": allowed,
            "remediation": "set operation to one of the allowed values and pass exactly the matching payload object",
        })),
    )
}

pub(super) fn missing_spec(
    tool: &'static str,
    operation: &'static str,
    source_of_truth: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32602),
        format!("{tool} operation={operation} missing operation payload"),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_id": operation,
            "source_of_truth": source_of_truth,
            "remediation": "pass the payload object matching operation",
        })),
    )
}

pub(super) fn params_error(
    tool: &'static str,
    operation: &'static str,
    source_id: &'static str,
    source_of_truth: &'static str,
    message: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32602),
        format!(
            "{tool} operation={operation} invalid {source_id}: {}",
            message.into()
        ),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_id": source_id,
            "source_of_truth": source_of_truth,
            "remediation": "fix the parameter value and retry",
        })),
    )
}

pub(super) fn delegate_error(
    tool: &'static str,
    operation: &'static str,
    source_id: &str,
    source_of_truth: &'static str,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} operation={operation} failed for {source_id}: {}",
            error.message
        ),
        Some(json!({
            "code": error_code_from(&error),
            "tool": tool,
            "operation": operation,
            "source_id": source_id,
            "source_of_truth": source_of_truth,
            "remediation": remediation,
            "cause": {
                "message": error.message.to_string(),
                "data": error.data,
            },
        })),
    )
}

pub(super) fn io_error(
    tool: &'static str,
    operation: &'static str,
    source_id: &str,
    source_of_truth: &'static str,
    error: std::io::Error,
    remediation: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("{tool} operation={operation} could not read {source_id}: {error}"),
        Some(json!({
            "code": error_codes::STORAGE_READ_FAILED,
            "tool": tool,
            "operation": operation,
            "source_id": source_id,
            "source_of_truth": source_of_truth,
            "remediation": remediation,
            "io_error_kind": format!("{:?}", error.kind()),
        })),
    )
}

pub(super) fn lifecycle_corrupt_error(
    path: &Path,
    line_no: u64,
    reason: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "audit operation=lifecycle_tail found corrupt daemon lifecycle row {}:{}",
            path.display(),
            line_no
        ),
        Some(json!({
            "code": error_codes::STORAGE_READ_FAILED,
            "tool": AUDIT_TOOL,
            "operation": "lifecycle_tail",
            "source_id": path.display().to_string(),
            "source_of_truth": AUDIT_SOT,
            "line_no": line_no,
            "reason": reason.into(),
            "remediation": "inspect the daemon lifecycle JSONL file and repair or rotate the corrupt ledger before trusting audit output",
        })),
    )
}

pub(super) fn lifecycle_oversized_error(
    path: &Path,
    line_no: u64,
    line_bytes: usize,
    max_line_bytes: usize,
    tool: &Option<String>,
    status: &Option<String>,
    event_kind: &Option<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "audit operation=lifecycle_tail found oversized daemon lifecycle row {}:{}",
            path.display(),
            line_no
        ),
        Some(json!({
            "code": error_codes::STORAGE_READ_FAILED,
            "tool": AUDIT_TOOL,
            "operation": "lifecycle_tail",
            "source_id": path.display().to_string(),
            "source_of_truth": AUDIT_SOT,
            "line_no": line_no,
            "reason": "oversized_row",
            "line_bytes": line_bytes,
            "max_line_bytes": max_line_bytes,
            "row_tool": tool,
            "row_status": status,
            "row_event_kind": event_kind,
            "remediation": "raise max_line_bytes for this matching row, or rotate/repair the lifecycle ledger after preserving forensic evidence; filtered reads skip only valid oversized rows that do not match the requested filters",
        })),
    )
}

pub(super) fn replay_artifact_corrupt_error(
    path: &Path,
    line_no: u64,
    reason: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "replay operation=artifact_inspect found corrupt replay JSONL row {}:{}",
            path.display(),
            line_no
        ),
        Some(json!({
            "code": error_codes::STORAGE_READ_FAILED,
            "tool": REPLAY_TOOL,
            "operation": "artifact_inspect",
            "source_id": path.display().to_string(),
            "source_of_truth": REPLAY_SOT,
            "line_no": line_no,
            "reason": reason.into(),
            "remediation": "recreate the replay artifact from source rows or inspect the corrupt JSONL bytes",
        })),
    )
}

fn error_code_from(error: &ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned()
}

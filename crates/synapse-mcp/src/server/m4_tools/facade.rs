use super::*;
use super::types::*;

pub(super) fn shell_facade_error(
    operation: ShellOperation,
    source_id: impl Into<String>,
    message: impl Into<String>,
    remediation: &'static str,
) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": operation.as_str(),
            "source_of_truth": SHELL_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "remediation": remediation,
        })),
    )
}

pub(super) fn process_facade_error(
    operation: ProcessOperation,
    source_id: impl Into<String>,
    message: impl Into<String>,
    remediation: &'static str,
) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": operation.as_str(),
            "source_of_truth": PROCESS_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "remediation": remediation,
        })),
    )
}

pub(super) fn shell_facade_delegate_error(
    operation: ShellOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let source_id = source_id.into();
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause_data = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "operation": operation.as_str(),
            "source_of_truth": SHELL_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id,
            "remediation": remediation,
            "cause": cause_data,
        })),
    )
}

pub(super) fn process_facade_delegate_error(
    operation: ProcessOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let source_id = source_id.into();
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause_data = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "operation": operation.as_str(),
            "source_of_truth": PROCESS_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id,
            "remediation": remediation,
            "cause": cause_data,
        })),
    )
}

pub(super) fn require_shell_text(
    operation: ShellOperation,
    value: Option<String>,
    field: &'static str,
    source_id: &str,
) -> Result<String, ErrorData> {
    let Some(value) = value else {
        return Err(shell_facade_error(
            operation,
            source_id,
            format!("shell operation={} requires {field}", operation.as_str()),
            "provide the required field for this shell operation",
        ));
    };
    if value.trim().is_empty() {
        return Err(shell_facade_error(
            operation,
            source_id,
            format!(
                "shell operation={} requires non-empty {field}",
                operation.as_str()
            ),
            "provide a non-empty executable or job id",
        ));
    }
    Ok(value)
}

pub(super) fn require_process_text(
    operation: ProcessOperation,
    value: Option<String>,
    field: &'static str,
    source_id: &str,
) -> Result<String, ErrorData> {
    let Some(value) = value else {
        return Err(process_facade_error(
            operation,
            source_id,
            format!("process operation={} requires {field}", operation.as_str()),
            "provide the required field for this process operation",
        ));
    };
    if value.trim().is_empty() {
        return Err(process_facade_error(
            operation,
            source_id,
            format!(
                "process operation={} requires non-empty {field}",
                operation.as_str()
            ),
            "provide a non-empty executable path/name or source id",
        ));
    }
    Ok(value)
}

pub(super) fn shell_unexpected_fields(
    operation: ShellOperation,
    params: &ShellParams,
    fields: &[&'static str],
) -> Result<(), ErrorData> {
    if fields.is_empty() {
        return Ok(());
    }
    let source_id = params
        .job_id
        .as_deref()
        .or(params.command.as_deref())
        .unwrap_or_else(|| operation.as_str());
    Err(shell_facade_error(
        operation,
        source_id,
        format!(
            "shell operation={} does not accept field(s): {}",
            operation.as_str(),
            fields.join(", ")
        ),
        "remove fields that belong to a different shell operation",
    ))
}

pub(super) fn process_unexpected_fields(
    operation: ProcessOperation,
    params: &ProcessParams,
    fields: &[&'static str],
) -> Result<(), ErrorData> {
    if fields.is_empty() {
        return Ok(());
    }
    let source_id = params
        .target
        .as_deref()
        .or_else(|| params.process_name_contains.as_deref())
        .or_else(|| params.command_line_contains.as_deref())
        .map(str::to_owned)
        .or_else(|| params.pid.map(|pid| pid.to_string()))
        .unwrap_or_else(|| operation.as_str().to_owned());
    Err(process_facade_error(
        operation,
        source_id,
        format!(
            "process operation={} does not accept field(s): {}",
            operation.as_str(),
            fields.join(", ")
        ),
        "remove fields that belong to a different process operation",
    ))
}

pub(super) fn shell_run_params(params: ShellParams) -> Result<ActRunShellParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.job_id.is_some() {
        unexpected.push("job_id");
    }
    if params.tail_bytes.is_some() {
        unexpected.push("tail_bytes");
    }
    shell_unexpected_fields(ShellOperation::Run, &params, &unexpected)?;
    let command = require_shell_text(
        ShellOperation::Run,
        params.command,
        "command",
        ShellOperation::Run.as_str(),
    )?;
    Ok(ActRunShellParams {
        command,
        args: params.args.unwrap_or_default(),
        working_dir: params.working_dir,
        env: params.env.unwrap_or_default(),
        timeout_ms: params
            .timeout_ms
            .unwrap_or(crate::m4::DEFAULT_SHELL_TIMEOUT_MS),
        execution_mode: params
            .execution_mode
            .unwrap_or(ActRunShellExecutionMode::Auto),
        durable_timeout_ms: params.durable_timeout_ms,
        idempotency_key: params.idempotency_key,
    })
}

pub(super) fn shell_start_params(params: ShellParams) -> Result<ActRunShellStartParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.execution_mode.is_some() {
        unexpected.push("execution_mode");
    }
    if params.durable_timeout_ms.is_some() {
        unexpected.push("durable_timeout_ms");
    }
    if params.idempotency_key.is_some() {
        unexpected.push("idempotency_key");
    }
    if params.tail_bytes.is_some() {
        unexpected.push("tail_bytes");
    }
    shell_unexpected_fields(ShellOperation::Start, &params, &unexpected)?;
    let command = require_shell_text(
        ShellOperation::Start,
        params.command,
        "command",
        ShellOperation::Start.as_str(),
    )?;
    Ok(ActRunShellStartParams {
        command,
        args: params.args.unwrap_or_default(),
        working_dir: params.working_dir,
        env: params.env.unwrap_or_default(),
        timeout_ms: params.timeout_ms,
        job_id: params.job_id,
    })
}

pub(super) fn shell_status_params(params: ShellParams) -> Result<ActRunShellStatusParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.command.is_some() {
        unexpected.push("command");
    }
    if params.args.is_some() {
        unexpected.push("args");
    }
    if params.working_dir.is_some() {
        unexpected.push("working_dir");
    }
    if params.env.is_some() {
        unexpected.push("env");
    }
    if params.timeout_ms.is_some() {
        unexpected.push("timeout_ms");
    }
    if params.execution_mode.is_some() {
        unexpected.push("execution_mode");
    }
    if params.durable_timeout_ms.is_some() {
        unexpected.push("durable_timeout_ms");
    }
    if params.idempotency_key.is_some() {
        unexpected.push("idempotency_key");
    }
    shell_unexpected_fields(ShellOperation::Status, &params, &unexpected)?;
    let job_id = require_shell_text(
        ShellOperation::Status,
        params.job_id,
        "job_id",
        ShellOperation::Status.as_str(),
    )?;
    Ok(ActRunShellStatusParams {
        job_id,
        tail_bytes: params
            .tail_bytes
            .unwrap_or(crate::m4::SHELL_JOB_TAIL_DEFAULT_BYTES),
    })
}

pub(super) fn shell_cancel_params(params: ShellParams) -> Result<ActRunShellJobIdParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.command.is_some() {
        unexpected.push("command");
    }
    if params.args.is_some() {
        unexpected.push("args");
    }
    if params.working_dir.is_some() {
        unexpected.push("working_dir");
    }
    if params.env.is_some() {
        unexpected.push("env");
    }
    if params.timeout_ms.is_some() {
        unexpected.push("timeout_ms");
    }
    if params.execution_mode.is_some() {
        unexpected.push("execution_mode");
    }
    if params.durable_timeout_ms.is_some() {
        unexpected.push("durable_timeout_ms");
    }
    if params.idempotency_key.is_some() {
        unexpected.push("idempotency_key");
    }
    if params.tail_bytes.is_some() {
        unexpected.push("tail_bytes");
    }
    shell_unexpected_fields(ShellOperation::Cancel, &params, &unexpected)?;
    let job_id = require_shell_text(
        ShellOperation::Cancel,
        params.job_id,
        "job_id",
        ShellOperation::Cancel.as_str(),
    )?;
    Ok(ActRunShellJobIdParams { job_id })
}

pub(super) fn process_launch_params(params: ProcessParams) -> Result<ActLaunchParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.pid.is_some() {
        unexpected.push("pid");
    }
    if params.process_name_contains.is_some() {
        unexpected.push("process_name_contains");
    }
    if params.command_line_contains.is_some() {
        unexpected.push("command_line_contains");
    }
    if params.limit.is_some() {
        unexpected.push("limit");
    }
    if params.include_command_line.is_some() {
        unexpected.push("include_command_line");
    }
    process_unexpected_fields(ProcessOperation::Launch, &params, &unexpected)?;
    let target = require_process_text(
        ProcessOperation::Launch,
        params.target,
        "target",
        ProcessOperation::Launch.as_str(),
    )?;
    Ok(ActLaunchParams {
        target,
        args: params.args.unwrap_or_default(),
        working_dir: params.working_dir,
        env: params.env.unwrap_or_default(),
        wait_for_window_title_regex: params.wait_for_window_title_regex,
        timeout_ms: params
            .timeout_ms
            .unwrap_or(crate::m4::DEFAULT_LAUNCH_TIMEOUT_MS),
        idempotency_key: params.idempotency_key,
        cdp_debug: params.cdp_debug,
        force_renderer_accessibility: params.force_renderer_accessibility,
        windows_console_window_state: params.windows_console_window_state,
        desktop: params.desktop,
    })
}

pub(super) fn validate_process_query_params(
    operation: ProcessOperation,
    params: &ProcessParams,
) -> Result<usize, ErrorData> {
    let mut unexpected = Vec::new();
    if params.target.is_some() {
        unexpected.push("target");
    }
    if params.args.is_some() {
        unexpected.push("args");
    }
    if params.working_dir.is_some() {
        unexpected.push("working_dir");
    }
    if params.env.is_some() {
        unexpected.push("env");
    }
    if params.wait_for_window_title_regex.is_some() {
        unexpected.push("wait_for_window_title_regex");
    }
    if params.timeout_ms.is_some() {
        unexpected.push("timeout_ms");
    }
    if params.idempotency_key.is_some() {
        unexpected.push("idempotency_key");
    }
    if params.cdp_debug.is_some() {
        unexpected.push("cdp_debug");
    }
    if params.force_renderer_accessibility.is_some() {
        unexpected.push("force_renderer_accessibility");
    }
    if params.windows_console_window_state.is_some() {
        unexpected.push("windows_console_window_state");
    }
    if params.desktop.is_some() {
        unexpected.push("desktop");
    }
    process_unexpected_fields(operation, params, &unexpected)?;

    for (field, value) in [
        (
            "process_name_contains",
            params.process_name_contains.as_deref(),
        ),
        (
            "command_line_contains",
            params.command_line_contains.as_deref(),
        ),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(process_facade_error(
                operation,
                field,
                format!(
                    "process operation={} requires non-empty {field}",
                    operation.as_str()
                ),
                "remove the empty filter or provide a non-empty filter value",
            ));
        }
    }

    let limit = params.limit.unwrap_or(match operation {
        ProcessOperation::List => PROCESS_LIST_DEFAULT_LIMIT,
        ProcessOperation::History => PROCESS_HISTORY_DEFAULT_LIMIT,
        ProcessOperation::Launch => unreachable!("launch is not a query operation"),
    });
    let max_limit = match operation {
        ProcessOperation::List => PROCESS_LIST_MAX_LIMIT,
        ProcessOperation::History => PROCESS_HISTORY_MAX_LIMIT,
        ProcessOperation::Launch => unreachable!("launch is not a query operation"),
    };
    if limit == 0 || limit > max_limit {
        return Err(process_facade_error(
            operation,
            limit.to_string(),
            format!(
                "process operation={} limit must be between 1 and {max_limit}",
                operation.as_str()
            ),
            "use a bounded positive limit for the requested readback",
        ));
    }
    Ok(limit)
}

pub(super) fn process_filters(params: &ProcessParams) -> ProcessFilters {
    ProcessFilters {
        pid: params.pid,
        process_name_contains: params.process_name_contains.clone(),
        command_line_contains: params.command_line_contains.clone(),
    }
}

pub(super) fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

pub(super) fn process_row_matches(filters: &ProcessFilters, row: &ProcessRow) -> bool {
    if filters.pid.is_some_and(|pid| row.pid != pid) {
        return false;
    }
    if let Some(filter) = filters.process_name_contains.as_deref()
        && !contains_case_insensitive(&row.name, filter)
    {
        return false;
    }
    if let Some(filter) = filters.command_line_contains.as_deref() {
        let Some(command_line) = row.command_line.as_deref() else {
            return false;
        };
        if !contains_case_insensitive(command_line, filter) {
            return false;
        }
    }
    true
}

pub(super) fn process_history_row_matches(filters: &ProcessFilters, row: &ProcessHistoryRow) -> bool {
    if filters.pid.is_some_and(|pid| row.pid != Some(pid)) {
        return false;
    }
    if let Some(filter) = filters.process_name_contains.as_deref() {
        let Some(target) = row.target.as_deref() else {
            return false;
        };
        if !contains_case_insensitive(target, filter) {
            return false;
        }
    }
    if let Some(filter) = filters.command_line_contains.as_deref() {
        let Some(command_line) = row.command_line.as_deref() else {
            return false;
        };
        if !contains_case_insensitive(command_line, filter) {
            return false;
        }
    }
    true
}

pub(super) fn process_list_response(params: &ProcessParams) -> Result<ProcessListResponse, ErrorData> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let limit = validate_process_query_params(ProcessOperation::List, params)?;
    let filters = process_filters(params);
    let include_command_line =
        params.include_command_line.unwrap_or(false) || filters.command_line_contains.is_some();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );

    let mut rows = Vec::new();
    for (pid, process) in system.processes() {
        let command_line = process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let row = ProcessRow {
            pid: pid.as_u32(),
            parent_pid: process.parent().map(|parent| parent.as_u32()),
            name: process.name().to_string_lossy().into_owned(),
            exe: process.exe().map(|path| path.display().to_string()),
            cwd: process.cwd().map(|path| path.display().to_string()),
            status: format!("{:?}", process.status()),
            start_time_unix_ms: process.start_time().saturating_mul(1000),
            command_line: include_command_line.then_some(command_line),
        };
        if !process_row_matches(&filters, &row) {
            continue;
        }
        rows.push(row);
        if rows.len() >= limit {
            break;
        }
    }
    Ok(ProcessListResponse {
        source_of_truth: "live OS process table via sysinfo refresh_processes_specifics".to_owned(),
        returned_count: rows.len(),
        limit,
        filters,
        rows,
    })
}

pub(super) fn process_history_response(
    service: &SynapseService,
    params: &ProcessParams,
) -> Result<ProcessHistoryResponse, ErrorData> {
    let limit = validate_process_query_params(ProcessOperation::History, params)?;
    let filters = process_filters(params);
    let rows = {
        let runtime = service.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            process_facade_error(
                ProcessOperation::History,
                cf::CF_PROCESS_HISTORY,
                "reflex runtime lock poisoned while reading process history",
                "retry after the daemon lock recovers; inspect daemon logs if this repeats",
            )
        })?;
        runtime
            .storage_cf_tail_rows(cf::CF_PROCESS_HISTORY, limit)
            .map_err(|error| {
                process_facade_error(
                    ProcessOperation::History,
                    cf::CF_PROCESS_HISTORY,
                    format!("CF_PROCESS_HISTORY tail read failed: {error}"),
                    "inspect the RocksDB column family and daemon storage logs",
                )
            })?
    };
    let scanned_tail_rows = rows.len();
    let mut decoded_rows = Vec::new();
    for (key, value) in rows {
        let decoded = decode_json::<Value>(&value).map_err(|error| {
            process_facade_error(
                ProcessOperation::History,
                hex_lower(&key),
                format!("CF_PROCESS_HISTORY row decode failed: {error}"),
                "inspect the exact process history row bytes and fix the writer",
            )
        })?;
        let row_json = serde_json::to_string(&decoded).map_err(|error| {
            process_facade_error(
                ProcessOperation::History,
                hex_lower(&key),
                format!("CF_PROCESS_HISTORY row JSON render failed: {error}"),
                "inspect the decoded process history row",
            )
        })?;
        let row = ProcessHistoryRow {
            key: String::from_utf8_lossy(&key).into_owned(),
            key_hex: hex_lower(&key),
            value_len_bytes: u64::try_from(value.len()).unwrap_or(u64::MAX),
            row_json,
            pid: json_u32_field(&decoded, "pid"),
            target: json_string_field(&decoded, "target"),
            tool: json_string_field(&decoded, "tool"),
            status: json_string_field(&decoded, "status"),
            launched_at: json_string_field(&decoded, "launched_at"),
            command_line: json_string_field(&decoded, "command_line"),
        };
        if process_history_row_matches(&filters, &row) {
            decoded_rows.push(row);
        }
    }
    Ok(ProcessHistoryResponse {
        source_of_truth: PROCESS_FACADE_SOURCE_OF_TRUTH.to_owned(),
        cf_name: cf::CF_PROCESS_HISTORY.to_owned(),
        returned_count: decoded_rows.len(),
        scanned_tail_rows,
        limit,
        filters,
        rows: decoded_rows,
    })
}

pub(super) fn json_u32_field(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|raw| u32::try_from(raw).ok())
}

pub(super) fn json_string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub(super) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

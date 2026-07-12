// Split-out sibling of the m4_tools module; the glob mirrors the pre-split
// single-module layout and keeps the shared symbol set in one place.
#[allow(clippy::wildcard_imports)]
use super::*;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShellOperation {
    #[default]
    Run,
    Start,
    Status,
    Cancel,
}

impl ShellOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Start => "start",
            Self::Status => "status",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellParams {
    #[serde(default)]
    #[schemars(description = "Shell facade operation. Defaults to run.")]
    pub operation: ShellOperation,
    #[serde(default)]
    #[schemars(description = "Executable path/name only for run/start operations.")]
    pub command: Option<String>,
    #[serde(default)]
    #[schemars(default, description = "Literal executable arguments for run/start.")]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub execution_mode: Option<ActRunShellExecutionMode>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub durable_timeout_ms: Option<u64>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 128))]
    pub job_id: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 0, max = 1048576))]
    pub tail_bytes: Option<u64>,
}

pub(crate) fn shell_input_schema() -> Arc<Map<String, Value>> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "operation": {
                "type": "string",
                "enum": ["run", "start", "status", "cancel"],
                "default": "run",
                "description": "Shell facade operation. Omit only for the default run operation."
            },
            "command": {
                "type": ["string", "null"],
                "description": "Executable path/name only. Accepted by run/start."
            },
            "args": {
                "type": ["array", "null"],
                "items": { "type": "string" },
                "description": "Literal executable arguments. Accepted by run/start."
            },
            "working_dir": {
                "type": ["string", "null"],
                "description": "Working directory for run/start."
            },
            "env": {
                "type": ["object", "null"],
                "additionalProperties": { "type": "string" },
                "description": "Extra environment variables for run/start."
            },
            "timeout_ms": {
                "type": ["integer", "null"],
                "minimum": 1,
                "description": "run: caller inline wait budget. start: durable job lifetime cap. Omit start timeout for an unbounded durable job."
            },
            "execution_mode": {
                "type": ["string", "null"],
                "enum": ["auto", "inline", "durable", null],
                "description": "run only. Controls inline vs durable/background routing."
            },
            "durable_timeout_ms": {
                "type": ["integer", "null"],
                "minimum": 1,
                "description": "run only. Applies if run creates a durable/background job; start uses timeout_ms for its durable lifetime cap."
            },
            "idempotency_key": {
                "type": ["string", "null"],
                "description": "run only. Deduplicates/replays matching run requests."
            },
            "job_id": {
                "type": ["string", "null"],
                "minLength": 1,
                "maxLength": 128,
                "description": "start/status/cancel durable job id. Optional for start, required for status/cancel."
            },
            "tail_bytes": {
                "type": ["integer", "null"],
                "minimum": 0,
                "maximum": 1048576,
                "description": "status only. Number of stdout/stderr tail bytes to read."
            }
        },
        "oneOf": [
            {
                "title": "shell operation=run",
                "type": "object",
                "additionalProperties": false,
                "required": ["command"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "const": "run",
                        "default": "run",
                        "description": "Default shell operation."
                    },
                    "command": { "$ref": "#/properties/command" },
                    "args": { "$ref": "#/properties/args" },
                    "working_dir": { "$ref": "#/properties/working_dir" },
                    "env": { "$ref": "#/properties/env" },
                    "timeout_ms": { "$ref": "#/properties/timeout_ms" },
                    "execution_mode": { "$ref": "#/properties/execution_mode" },
                    "durable_timeout_ms": { "$ref": "#/properties/durable_timeout_ms" },
                    "idempotency_key": { "$ref": "#/properties/idempotency_key" }
                }
            },
            {
                "title": "shell operation=start",
                "type": "object",
                "additionalProperties": false,
                "required": ["operation", "command"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "const": "start",
                        "description": "Create a durable shell job immediately."
                    },
                    "command": { "$ref": "#/properties/command" },
                    "args": { "$ref": "#/properties/args" },
                    "working_dir": { "$ref": "#/properties/working_dir" },
                    "env": { "$ref": "#/properties/env" },
                    "timeout_ms": { "$ref": "#/properties/timeout_ms" },
                    "job_id": { "$ref": "#/properties/job_id" }
                }
            },
            {
                "title": "shell operation=status",
                "type": "object",
                "additionalProperties": false,
                "required": ["operation", "job_id"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "const": "status",
                        "description": "Read persisted durable shell job state."
                    },
                    "job_id": { "$ref": "#/properties/job_id" },
                    "tail_bytes": { "$ref": "#/properties/tail_bytes" }
                }
            },
            {
                "title": "shell operation=cancel",
                "type": "object",
                "additionalProperties": false,
                "required": ["operation", "job_id"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "const": "cancel",
                        "description": "Terminate an exact durable shell job."
                    },
                    "job_id": { "$ref": "#/properties/job_id" }
                }
            }
        ]
    });
    match schema {
        Value::Object(object) => Arc::new(object),
        _ => Arc::new(Map::new()),
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellFacadeResponse {
    pub operation: ShellOperation,
    pub source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<ActRunShellResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<ActRunShellStartResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ActRunShellStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<ActRunShellCancelResponse>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOperation {
    #[default]
    List,
    Launch,
    History,
}

impl ProcessOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Launch => "launch",
            Self::History => "history",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessParams {
    #[serde(default)]
    #[schemars(description = "Process facade operation. Defaults to list.")]
    pub operation: ProcessOperation,
    #[serde(default)]
    #[schemars(description = "Executable path/name for launch operations.")]
    pub target: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub wait_for_window_title_regex: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub cdp_debug: Option<bool>,
    #[serde(default)]
    pub force_renderer_accessibility: Option<bool>,
    #[serde(default)]
    pub windows_console_window_state: Option<LaunchWindowState>,
    #[serde(default)]
    pub desktop: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub process_name_contains: Option<String>,
    #[serde(default)]
    pub command_line_contains: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 1000))]
    pub limit: Option<usize>,
    #[serde(default)]
    #[schemars(default)]
    pub include_command_line: Option<bool>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessFacadeResponse {
    pub operation: ProcessOperation,
    pub source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<ActLaunchResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<ProcessListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<ProcessHistoryResponse>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessListResponse {
    pub source_of_truth: String,
    pub returned_count: usize,
    pub limit: usize,
    pub filters: ProcessFilters,
    pub rows: Vec<ProcessRow>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line_contains: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessRow {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_pid: Option<u32>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub status: String,
    pub start_time_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessHistoryResponse {
    pub source_of_truth: String,
    pub cf_name: String,
    pub returned_count: usize,
    pub scanned_tail_rows: usize,
    pub limit: usize,
    pub filters: ProcessFilters,
    pub rows: Vec<ProcessHistoryRow>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessHistoryRow {
    pub key: String,
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub row_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line: Option<String>,
}

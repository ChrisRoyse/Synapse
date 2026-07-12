use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use rmcp::{
    ErrorData,
    model::ErrorCode,
    schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::{
    Action, Backend, ComboInput, ComboStep, ForegroundContext, Key, Rect, error_codes,
    new_reflex_id,
};
use synapse_reflex::{ComboParams, ReflexRuntime, ScheduledReflex};
use synapse_storage::{decode_json, encode_json};
use tokio::{io::AsyncReadExt, process::Command as TokioCommand};

use crate::{
    m1::mcp_error,
    m2::{ActPressParams, action_from_press_params},
    m3::permissions::{RequiredPermissions, add_action_permissions},
};

const MAX_COMBO_STEPS: usize = 256;
pub(crate) const DEFAULT_SHELL_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS: u64 = 90_000;
pub const DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS: u64 = 110_000;
pub(crate) const DEFAULT_LAUNCH_TIMEOUT_MS: u64 = 10_000;
#[cfg(windows)]
const SW_HIDE: u16 = 0;
#[cfg(windows)]
const SW_SHOWNOACTIVATE: u16 = 4;
const DEFAULT_AGENT_SPAWN_WAIT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS: u64 = 1_800_000;
const DEFAULT_AGENT_SPAWN_HOLD_OPEN_MS: u64 = 60_000;
const MAX_AGENT_SPAWN_PROMPT_BYTES: usize = 128 * 1024;
/// Upper bound on a spawn `model` id. Generous for provider/version/ARN-style
/// ids (matches the cost table's `MODEL_PRICE_MAX_ID_CHARS`).
const MAX_AGENT_SPAWN_MODEL_BYTES: usize = 256;
const MAX_SHELL_IDEMPOTENCY_KEY_BYTES: usize = 256;
const ALLOW_SHELL_ENV: &str = "SYNAPSE_ALLOW_SHELL";
const ALLOW_LAUNCH_ENV: &str = "SYNAPSE_ALLOW_LAUNCH";
/// Unrestricted shell/launch. **On by default**: Synapse is general local
/// computer-control, so any command/target is permitted unless the operator
/// explicitly sets the env to a falsey value (`0`/`false`/`no`/`off`), which
/// restores the per-target allowlist. Every command/target is recorded in
/// `CF_ACTION_LOG` regardless, and the mode is logged loudly at startup.
const ALLOW_SHELL_ANY_ENV: &str = "SYNAPSE_ALLOW_SHELL_ANY";
const ALLOW_LAUNCH_ANY_ENV: &str = "SYNAPSE_ALLOW_LAUNCH_ANY";
const RUN_SHELL_INLINE_AWAIT_LIMIT_ENV: &str = "SYNAPSE_RUN_SHELL_INLINE_AWAIT_LIMIT_MS";
/// Sentinel recorded as the matched pattern when permissive mode authorizes a
/// command/target without an allowlist entry.
const ANY_PERMITTED_SENTINEL: &str = "__any_permitted__";
const SHELL_OUTPUT_CAP_BYTES: usize = 1024 * 1024;
pub(crate) const SHELL_JOB_TAIL_DEFAULT_BYTES: u64 = 64 * 1024;
const SHELL_JOB_TAIL_MAX_BYTES: u64 = 1024 * 1024;
const SHELL_JOB_DASHBOARD_TAIL_BYTES: u64 = 2 * 1024;
const SHELL_JOB_ID_MAX_BYTES: usize = 128;
const SHELL_COMMAND_METADATA_POLICY: &str = "safe_display_v1";
const SHELL_ARG_DISPLAY_MAX_BYTES: usize = 160;
const SHELL_ARGS_DISPLAY_MAX_ITEMS: usize = 16;
const SHELL_COMMAND_LINE_DISPLAY_MAX_BYTES: usize = 512;
const SHELL_SESSION_ID_ENV: &str = "SYNAPSE_MCP_SESSION_ID";
const SHELL_SESSION_DIR_ENV: &str = "SYNAPSE_SHELL_SESSION_DIR";
const SHELL_WORKING_DIR_ENV: &str = "SYNAPSE_SHELL_WORKING_DIR";
const SHELL_RESERVED_ENV_KEYS: [&str; 3] = [
    SHELL_SESSION_ID_ENV,
    SHELL_SESSION_DIR_ENV,
    SHELL_WORKING_DIR_ENV,
];
const ALLOW_PATTERN_SIZE_LIMIT_BYTES: usize = 256 * 1024;
const PROCESS_BASE_ENV_KEYS: [&str; 20] = [
    "PATH",
    "PATHEXT",
    "COMSPEC",
    "SystemDrive",
    "SystemRoot",
    "WINDIR",
    "TEMP",
    "TMP",
    "USERDOMAIN",
    "USERNAME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
];
#[cfg(windows)]
const WINDOWS_DEFAULT_PATHEXT: &str =
    ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC;.PY;.PYW";
#[cfg(windows)]
const WINDOWS_GIT_SSH_RELATIVE_DIR: &str = r"Git\usr\bin";
const LAUNCH_WINDOW_POLL_INTERVAL_MS: u64 = 20;
const RUN_SHELL_IDEMPOTENCY_PREFIX: &str = "m4/act_run_shell/idempotency/v1/";
const SHELL_JOB_FINALIZING_GRACE_MS: u64 = 30_000;
const SHELL_REMOTE_TRANSPORT_LOCAL: &str = "local";
const SHELL_REMOTE_TRANSPORT_SSH: &str = "ssh";
const SHELL_REMOTE_CLEANUP_NOT_APPLICABLE: &str = "not_applicable";
const SHELL_REMOTE_CLEANUP_NOT_TRACKED: &str = "remote_process_not_tracked";
const SHELL_REMOTE_CLEANUP_TRACKING_PENDING: &str = "remote_process_tracking_pending";
const SHELL_REMOTE_CLEANUP_TRACKED: &str = "remote_process_tracked";
const SHELL_REMOTE_CLEANUP_VERIFIED: &str = "remote_cleanup_verified";
const SHELL_REMOTE_CLEANUP_UNVERIFIED: &str = "remote_cleanup_unverified";
const SHELL_REMOTE_CLEANUP_FAILED: &str = "remote_cleanup_failed";
const SHELL_REMOTE_CLEANUP_PRE_MARKER_TERMINAL: &str =
    "remote_process_never_started_or_untracked_pre_marker";
const SHELL_JOB_STATUS_REMOTE_TRANSPORT_LOST: &str = "transport_lost_process_may_still_run";
const SHELL_JOB_STATUS_REMOTE_EXITED_LOCAL_STALE: &str =
    "remote_process_exited_local_transport_stale";
const SHELL_REMOTE_CLEANUP_TRANSPORT_LOST: &str = "transport_lost_process_may_still_run";
const SHELL_REMOTE_CLEANUP_ALREADY_GONE: &str = "remote_process_already_gone";
const SHELL_REMOTE_PROCESS_MARKER: &str = "SYNAPSE_REMOTE_PROCESS_V1";
const SHELL_REMOTE_EXIT_MARKER: &str = "SYNAPSE_REMOTE_EXIT_V1";
const SHELL_REMOTE_CLEANUP_MARKER: &str = "SYNAPSE_REMOTE_CLEANUP_V1";
const SHELL_REMOTE_LIVENESS_MARKER: &str = "SYNAPSE_REMOTE_LIVENESS_V1";
const SHELL_REMOTE_METADATA_PREFIX_BYTES: usize = 128 * 1024;
const SHELL_REMOTE_METADATA_WAIT_MS: u64 = 1_500;
const SHELL_REMOTE_CLEANUP_TIMEOUT_MS: u64 = 15_000;
const SHELL_REMOTE_LIVENESS_TIMEOUT_MS: u64 = 2_500;
pub const SHELL_PATTERN_TOO_BROAD: &str = "SHELL_PATTERN_TOO_BROAD";
pub const LAUNCH_PATTERN_TOO_BROAD: &str = "LAUNCH_PATTERN_TOO_BROAD";

// All fields are allowlist policy for the two gated tools; the shared `allow_`
// prefix is intentional and reads clearly at call sites.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug)]
pub struct M4ServiceConfig {
    allow_shell: Vec<AllowPattern>,
    allow_launch: Vec<AllowPattern>,
    allow_shell_any: bool,
    allow_launch_any: bool,
    run_shell_inline_await_limit_ms: u64,
}

impl Default for M4ServiceConfig {
    fn default() -> Self {
        Self {
            allow_shell: Vec::new(),
            allow_launch: Vec::new(),
            allow_shell_any: false,
            allow_launch_any: false,
            run_shell_inline_await_limit_ms: DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
        }
    }
}

#[derive(Clone, Debug)]
struct AllowPattern {
    raw: String,
    regex: regex::Regex,
}

#[derive(Debug)]
pub struct BroadAllowPatternError {
    source_name: &'static str,
    tool_name: &'static str,
    code: &'static str,
    raw: String,
    reason: &'static str,
}

impl BroadAllowPatternError {
    #[must_use]
    pub const fn source_name(&self) -> &'static str {
        self.source_name
    }

    #[must_use]
    pub const fn tool_name(&self) -> &'static str {
        self.tool_name
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    #[must_use]
    pub const fn reason(&self) -> &'static str {
        self.reason
    }
}

impl std::fmt::Display for BroadAllowPatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} pattern {:?} is too broad for {}: {}",
            self.source_name, self.raw, self.tool_name, self.reason
        )
    }
}

impl std::error::Error for BroadAllowPatternError {}

impl M4ServiceConfig {
    pub fn from_cli_parts(
        allow_shell: Vec<String>,
        allow_launch: Vec<String>,
        run_shell_inline_await_limit_ms: u64,
    ) -> anyhow::Result<Self> {
        let allow_shell_any = env_flag_default_true(ALLOW_SHELL_ANY_ENV);
        let allow_launch_any = env_flag_default_true(ALLOW_LAUNCH_ANY_ENV);
        if allow_shell_any {
            tracing::warn!(
                code = "M4_ALLOW_SHELL_ANY_ENABLED",
                env = ALLOW_SHELL_ANY_ENV,
                "act_run_shell permissive mode enabled: ALL shell commands are allowed (allowlist bypassed); every command is still recorded in CF_ACTION_LOG"
            );
        }
        if allow_launch_any {
            tracing::warn!(
                code = "M4_ALLOW_LAUNCH_ANY_ENABLED",
                env = ALLOW_LAUNCH_ANY_ENV,
                "act_launch permissive mode enabled: ALL launch targets are allowed (allowlist bypassed); every launch is still recorded in CF_ACTION_LOG"
            );
        }
        Ok(Self {
            allow_shell: compile_allow_patterns(
                ALLOW_SHELL_ENV,
                allow_shell,
                AllowPatternPolicy::Shell,
            )?,
            allow_launch: compile_allow_patterns(
                ALLOW_LAUNCH_ENV,
                allow_launch,
                AllowPatternPolicy::Launch,
            )?,
            allow_shell_any,
            allow_launch_any,
            run_shell_inline_await_limit_ms,
        })
    }

    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_cli_parts(
            parse_env_list(ALLOW_SHELL_ENV),
            parse_env_list(ALLOW_LAUNCH_ENV),
            parse_env_u64(
                RUN_SHELL_INLINE_AWAIT_LIMIT_ENV,
                DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
            )?,
        )
    }

    #[must_use]
    pub const fn allow_shell_count(&self) -> usize {
        self.allow_shell.len()
    }

    #[must_use]
    pub const fn allow_launch_count(&self) -> usize {
        self.allow_launch.len()
    }

    #[must_use]
    pub const fn allow_shell_any(&self) -> bool {
        self.allow_shell_any
    }

    #[must_use]
    pub const fn allow_launch_any(&self) -> bool {
        self.allow_launch_any
    }

    #[must_use]
    pub const fn run_shell_inline_await_limit_ms(&self) -> u64 {
        self.run_shell_inline_await_limit_ms
    }

    #[must_use]
    pub const fn run_shell_inline_client_call_budget_ms(&self) -> u64 {
        DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS
    }

    #[must_use]
    pub const fn run_shell_durable_default_timeout_ms(&self) -> Option<u64> {
        None
    }

    #[must_use]
    pub const fn run_shell_durable_max_timeout_ms(&self) -> Option<u64> {
        None
    }

    fn shell_match<'a>(&'a self, command_line: &str) -> Option<&'a str> {
        if self.allow_shell_any {
            return Some(ANY_PERMITTED_SENTINEL);
        }
        self.allow_shell
            .iter()
            .find(|pattern| pattern.regex.is_match(command_line))
            .map(|pattern| pattern.raw.as_str())
    }

    pub(crate) fn launch_match<'a>(&'a self, command_line: &str) -> Option<&'a str> {
        if self.allow_launch_any {
            return Some(ANY_PERMITTED_SENTINEL);
        }
        self.allow_launch
            .iter()
            .find(|pattern| pattern.regex.is_match(command_line))
            .map(|pattern| pattern.raw.as_str())
    }
}

/// Returns `true` unless the env var is explicitly set to a falsey value
/// (`0`/`false`/`no`/`off`). An unset variable means `true`, i.e.
/// permissive-by-default for shell/launch.
fn env_flag_default_true(name: &str) -> bool {
    std::env::var(name).map_or(true, |value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

fn parse_env_u64(name: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(name) {
        Ok(raw) => raw
            .trim()
            .parse::<u64>()
            .with_context(|| format!("{name} must be an unsigned integer number of milliseconds")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("read {name}")),
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActComboParams {
    #[schemars(length(min = 1, max = 256))]
    pub steps: Vec<ActComboStep>,
    #[serde(default = "default_backend")]
    #[schemars(default = "default_backend")]
    pub backend: Backend,
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActComboStep {
    pub at_ms: u32,
    pub action: ActComboAction,
    pub params: serde_json::Value,
    pub backend: Option<Backend>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActComboAction {
    ActPress,
    Retired(String),
}

impl<'de> Deserialize<'de> for ActComboAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "act_press" => Ok(Self::ActPress),
            _ => Ok(Self::Retired(value)),
        }
    }
}

impl JsonSchema for ActComboAction {
    fn schema_name() -> Cow<'static, str> {
        "ActComboAction".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "enum": ["act_press"],
            "description": "Only timed act_press key steps are supported. Use act_stroke for continuous mouse motion/path execution."
        })
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActComboResponse {
    pub combo_id: String,
    pub scheduled_steps: u32,
    pub backend: Backend,
    pub started_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellParams {
    #[schemars(
        description = "Executable path or program name only. Do not include arguments, quotes, pipes, redirection, or other shell syntax here; pass arguments in args. For shell syntax, set command to an explicit shell executable such as powershell.exe or cmd.exe and put the shell flags/snippet in args. Headed Playwright/Chromium automation launches are refused here because they can surface Chrome debugger/automation banners that shift browser layout; use existing authenticated Chrome via cdp_* / target_act / browser_* tools or act_launch with Synapse-injected isolated CDP flags."
    )]
    pub command: String,
    #[serde(default)]
    #[schemars(
        default,
        description = "Arguments passed literally to the executable. These are not parsed by a shell unless command itself is an explicit shell executable."
    )]
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub env: BTreeMap<String, String>,
    #[serde(
        default = "default_shell_timeout_ms",
        deserialize_with = "deserialize_nullable_shell_timeout_ms"
    )]
    #[schemars(
        default = "default_shell_timeout_ms",
        range(min = 1),
        description = "Caller-requested inline wait budget in milliseconds. In execution_mode=inline this is honored directly when it fits inside the MCP client-call budget; larger values return a durable job handle before the client-side tools/call cap can hide completion status. In execution_mode=auto, values above the inline await limit return a durable job handle."
    )]
    pub timeout_ms: u64,
    #[serde(default = "default_run_shell_execution_mode")]
    #[schemars(
        default = "default_run_shell_execution_mode",
        description = "Shell execution route: auto preserves compatibility and backgrounds when timeout_ms exceeds the inline await limit; inline waits up to timeout_ms only while that budget fits inside the MCP client-call budget and otherwise returns a durable job handle; durable immediately returns a durable job handle."
    )]
    pub execution_mode: ActRunShellExecutionMode,
    #[serde(default)]
    #[schemars(
        default,
        range(min = 1),
        description = "Optional explicit durable job lifetime cap in milliseconds. Applied only if this request creates a durable/background job; ignored when execution completes inline. Omit for an unbounded durable job."
    )]
    pub durable_timeout_ms: Option<u64>,
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActRunShellExecutionMode {
    Auto,
    Inline,
    Durable,
}

impl ActRunShellExecutionMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Inline => "inline",
            Self::Durable => "durable",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellResponse {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u32,
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub backgrounded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_await_limit_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_client_call_budget_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_execution_mode: Option<ActRunShellExecutionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_execution_mode: Option<ActRunShellExecutionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<ActRunShellJobStatus>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellStartParams {
    #[schemars(
        description = "Executable path or program name only. Do not include arguments, quotes, pipes, redirection, or other shell syntax here; pass arguments in args. For shell syntax, set command to an explicit shell executable such as powershell.exe or cmd.exe and put the shell flags/snippet in args. Headed Playwright/Chromium automation launches are refused here because they can surface Chrome debugger/automation banners that shift browser layout; use existing authenticated Chrome via cdp_* / target_act / browser_* tools or act_launch with Synapse-injected isolated CDP flags."
    )]
    pub command: String,
    #[serde(default)]
    #[schemars(
        default,
        description = "Arguments passed literally to the executable. These are not parsed by a shell unless command itself is an explicit shell executable."
    )]
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    #[schemars(
        range(min = 1),
        description = "Optional explicit durable job lifetime cap in milliseconds. Omit for an unbounded job that exits normally or is stopped only by act_run_shell_cancel/session cleanup."
    )]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    #[schemars(length(max = 128))]
    pub job_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellJobIdParams {
    #[schemars(length(min = 1, max = 128))]
    pub job_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellStatusParams {
    #[schemars(length(min = 1, max = 128))]
    pub job_id: String,
    #[serde(default = "default_shell_job_tail_bytes")]
    #[schemars(
        default = "default_shell_job_tail_bytes",
        range(min = 0, max = 1048576)
    )]
    pub tail_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellStartResponse {
    pub job: ActRunShellJobStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellStatusResponse {
    pub job: ActRunShellJobStatus,
    pub running: bool,
    pub stdout_len_bytes: u64,
    pub stderr_len_bytes: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellJobsDashboardSnapshot {
    pub source_of_truth: String,
    pub job_root: Option<String>,
    pub max_jobs: usize,
    pub job_count: usize,
    pub returned_count: usize,
    pub running_count: usize,
    pub terminal_count: usize,
    pub status_files_read: usize,
    pub skipped_invalid_job_dirs: usize,
    pub skipped_unreadable_status_files: usize,
    pub rows: Vec<ActRunShellStatusResponse>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellJobDiagnostics {
    pub checked_at: String,
    pub running: bool,
    pub elapsed_ms: Option<u64>,
    pub stdout_len_bytes: u64,
    pub stderr_len_bytes: u64,
    pub output_state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process_tree: Vec<ActRunShellProcessDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<ActRunShellTransferDiagnostics>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actionable_hints: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellProcessDiagnostic {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellTransferDiagnostics {
    pub family: String,
    pub client: String,
    pub protocol_hint: String,
    pub remote_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detection_evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_next_steps: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellCancelResponse {
    pub job_id: String,
    pub before_status: String,
    pub cancel_requested: bool,
    pub termination_attempted: bool,
    pub termination_status: String,
    pub remaining_process_ids: Vec<u32>,
    pub remote_process_scope: ActRunShellRemoteProcessScope,
    pub status: ActRunShellStatusResponse,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellRemoteProcessScope {
    pub transport: String,
    pub local_process_scope: String,
    pub remote_cleanup_required: bool,
    pub remote_cleanup_verified: bool,
    pub remote_cleanup_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_process_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_process_group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_cleanup_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_cleanup_message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detection_evidence: Vec<String>,
}

impl Default for ActRunShellRemoteProcessScope {
    fn default() -> Self {
        Self {
            transport: SHELL_REMOTE_TRANSPORT_LOCAL.to_owned(),
            local_process_scope: "job_owned_process_tree".to_owned(),
            remote_cleanup_required: false,
            remote_cleanup_verified: true,
            remote_cleanup_status: SHELL_REMOTE_CLEANUP_NOT_APPLICABLE.to_owned(),
            remote_identity: None,
            remote_process_id: None,
            remote_process_group_id: None,
            remote_cleanup_error_code: None,
            remote_cleanup_message: None,
            detection_evidence: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellJobStatus {
    pub schema_version: u32,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub status: String,
    pub pid: Option<u32>,
    pub command: String,
    #[serde(default = "default_shell_command_metadata_policy")]
    #[schemars(!default)]
    pub command_metadata_policy: String,
    pub args: Vec<String>,
    pub command_line: String,
    #[serde(default)]
    #[schemars(!default)]
    pub args_redacted: bool,
    #[serde(default)]
    #[schemars(!default)]
    pub command_line_redacted: bool,
    #[serde(default)]
    #[schemars(!default)]
    pub args_original_count: usize,
    #[serde(default)]
    #[schemars(!default)]
    pub args_original_bytes: usize,
    #[serde(default)]
    #[schemars(!default)]
    pub args_sha256: String,
    #[serde(default)]
    #[schemars(!default)]
    pub command_line_original_bytes: usize,
    #[serde(default)]
    #[schemars(!default)]
    pub command_line_sha256: String,
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_working_dir: Option<String>,
    pub env_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_env_keys: Vec<String>,
    pub timeout_ms: Option<u64>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancel_requested: bool,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub stdout_path: String,
    pub stderr_path: String,
    pub status_path: String,
    pub request_sha256: String,
    pub matched_pattern: String,
    #[serde(default)]
    #[schemars(!default)]
    pub remote_process_scope: ActRunShellRemoteProcessScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<ActRunShellJobDiagnostics>,
}

#[derive(Clone, Debug)]
struct ShellJobPaths {
    job_dir: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    status_path: PathBuf,
    request_path: PathBuf,
    remote_cleanup_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ShellSessionCleanupReadback {
    pub reason: String,
    pub session_id: String,
    pub job_root: Option<String>,
    pub status_files_read: usize,
    pub skipped_invalid_job_dirs: usize,
    pub skipped_unreadable_status_files: usize,
    pub skipped_foreign_jobs: usize,
    pub live_jobs_before: usize,
    pub retained_live_jobs: usize,
    /// Durable jobs whose status still claimed live ("running"/"cancel_requested")
    /// but whose backing process was already dead, reconciled to a terminal state
    /// on this cleanup pass instead of being retained as a phantom forever (#1334).
    #[serde(default)]
    #[schemars(!default)]
    pub reaped_phantom_jobs: usize,
    /// Job directories that vanished (or whose status file vanished) between
    /// enumeration and read because a concurrent session, the reaper, or a
    /// parallel test mutated the shared job root. This is an expected outcome of
    /// operating on a shared store and is tracked separately from `failed` so a
    /// benign race never inflates the error signal operators watch (#1509).
    #[serde(default)]
    #[schemars(!default)]
    pub skipped_concurrently_mutated: usize,
    pub termination_attempted: usize,
    pub termination_succeeded: usize,
    pub failed: usize,
    pub job_ids: Vec<String>,
    pub remaining_process_ids: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct RunShellAuthorization {
    command_line: String,
    matched_pattern: String,
}

#[derive(Clone, Debug)]
pub struct ShellExecutionContext {
    session_id: String,
    session_dir: PathBuf,
    default_working_dir: PathBuf,
}

impl ShellExecutionContext {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    #[must_use]
    pub fn default_working_dir(&self) -> &Path {
        &self.default_working_dir
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RunShellIdempotencyRow {
    schema_version: u32,
    tool: String,
    session_id: Option<String>,
    idempotency_key_sha256: String,
    request_sha256: String,
    status: String,
    command_line: String,
    #[serde(default)]
    command_line_sha256: String,
    #[serde(default)]
    command_line_redacted: bool,
    matched_pattern: String,
    started_at: String,
    completed_at: Option<String>,
    response: Option<ActRunShellResponse>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActLaunchParams {
    pub target: String,
    #[serde(default)]
    #[schemars(default)]
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub env: BTreeMap<String, String>,
    pub wait_for_window_title_regex: Option<String>,
    #[serde(default = "default_launch_timeout_ms")]
    #[schemars(default = "default_launch_timeout_ms", range(min = 1))]
    pub timeout_ms: u64,
    pub idempotency_key: Option<String>,
    /// Controls CDP debug-port injection for Chromium-family targets (#684).
    /// `None` (default) = auto: inject `--remote-debugging-port=0`, a dedicated
    /// `--user-data-dir`, `--silent-debugger-extension-api`, and
    /// `--disable-extensions` so `observe`/`find` can read the browser's DOM
    /// without loading user-profile extensions or surfacing debugger UI.
    /// `Some(false)` = opt out (launch the browser untouched). `Some(true)` =
    /// force injection even if heuristics would skip it. Ignored for
    /// non-Chromium targets.
    #[serde(default)]
    #[schemars(default)]
    pub cdp_debug: Option<bool>,
    /// Opt-in Chromium UIA renderer accessibility fallback (#689).
    /// `Some(true)` adds `--force-renderer-accessibility` for Chromium-family
    /// launches unless the caller already supplied that flag. `Some(false)`
    /// disables the env opt-in. `None` follows `SYNAPSE_FORCE_RENDERER_ACCESSIBILITY`.
    #[serde(default)]
    #[schemars(default)]
    pub force_renderer_accessibility: Option<bool>,
    /// Windows console window state for console targets launched through
    /// `CreateProcessW`. `None` and `hidden` use `CREATE_NO_WINDOW` so background
    /// helper shells do not flash or activate a visible blank console. `normal`
    /// is refused until a non-activating visible-console path can be proven.
    #[serde(default)]
    #[schemars(default)]
    pub windows_console_window_state: Option<LaunchWindowState>,
    /// Optional Windows desktop routing. Supported values are
    /// `agent:session`, `agent:<current Mcp-Session-Id>`, and
    /// `existing:<desktop-name>`.
    #[serde(default)]
    #[schemars(default)]
    pub desktop: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchWindowState {
    Normal,
    Hidden,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActSpawnAgentCli {
    Codex,
    Claude,
    LocalModel,
}

impl ActSpawnAgentCli {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::LocalModel => "local-model",
        }
    }

    #[must_use]
    pub const fn is_local_model(self) -> bool {
        matches!(self, Self::LocalModel)
    }

    #[must_use]
    pub const fn uses_approval_gate(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActSpawnAgentTarget {
    Window {
        window_hwnd: i64,
    },
    Cdp {
        window_hwnd: i64,
        cdp_target_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSpawnAgentParams {
    /// Back-compat agent selector for existing callers. New callers may use
    /// `kind`; if both are supplied they must name the same agent kind.
    #[serde(default)]
    #[schemars(default)]
    pub cli: Option<ActSpawnAgentCli>,
    /// Agent kind to spawn. `local_model` launches the registry-backed local
    /// model runner; `codex`/`claude` keep the existing CLI paths.
    #[serde(default)]
    #[schemars(default)]
    pub kind: Option<ActSpawnAgentCli>,
    /// Optional model id for the spawned agent (Claude `--model`, Codex
    /// `-m/--model`). When set it is also recorded in the spawn manifest so the
    /// transcript ingester can attribute the spawn's cost — the Codex
    /// `exec --json` stream carries no model id, so capturing it at spawn time
    /// is the only authoritative source (#949). Omit to use the CLI's own
    /// configured default; the spawn is then honestly reported as the model id
    /// the stream surfaces, or `unknown`/unpriced if none.
    #[serde(default)]
    #[schemars(default)]
    pub model: Option<String>,
    /// Local-model only: registry row name to launch through the #931 runner.
    /// `model` is accepted as a legacy alias when `kind/cli=local_model`, but
    /// `model_ref` is the explicit field used by templates and the dashboard.
    #[serde(default)]
    #[schemars(default)]
    pub model_ref: Option<String>,
    /// Work prompt for the spawned primary agent. Direct spawns require a
    /// non-empty prompt; template spawns render this from the durable template
    /// row. Synapse prepends a mandatory provisioning preflight that calls
    /// health/tools through the real client MCP surface and binds the requested
    /// target.
    #[serde(default)]
    #[schemars(default)]
    pub prompt: Option<String>,
    /// Optional per-session perception target that the spawned agent must bind
    /// with set_target before act_spawn_agent returns success.
    #[serde(default)]
    #[schemars(default)]
    pub target: Option<ActSpawnAgentTarget>,
    /// Working directory for the primary agent. Defaults to the daemon process
    /// current directory.
    #[serde(default)]
    #[schemars(default)]
    pub working_dir: Option<String>,
    /// Streamable HTTP MCP endpoint for the spawned agent. Defaults to the
    /// canonical local Synapse daemon endpoint.
    #[serde(default = "default_agent_spawn_mcp_url")]
    #[schemars(default = "default_agent_spawn_mcp_url")]
    pub mcp_url: String,
    /// Time to wait for distinct MCP session/target registry readback and
    /// task-start readiness artifact readback.
    #[serde(default = "default_agent_spawn_wait_timeout_ms")]
    #[schemars(
        default = "default_agent_spawn_wait_timeout_ms",
        range(min = 1, max = 1_800_000)
    )]
    pub wait_timeout_ms: u64,
    /// Provision-only agents hold the primary process open long enough for
    /// manual readback; task prompts may continue doing useful work during this
    /// interval.
    #[serde(default = "default_agent_spawn_hold_open_ms")]
    #[schemars(default = "default_agent_spawn_hold_open_ms", range(min = 0))]
    pub hold_open_ms: u64,
    /// Gate spawned Claude risky tool calls through the human Approvals inbox
    /// (#927). When true (the default) Claude uses the public
    /// `mcp__synapse__approval` facade, which delegates to the hidden gate;
    /// when false Claude uses `bypassPermissions` for trusted unattended automation. Local-model
    /// workers ignore this knob: they execute autonomously after prompt/exact
    /// contract prevalidation, with target/lease/tool-level invariants still
    /// failing closed.
    #[serde(default = "default_require_approval_gate")]
    #[schemars(default = "default_require_approval_gate")]
    pub require_approval_gate: bool,
    /// Spawn-template provenance (#909), stamped server-side when this spawn was
    /// rendered from an `agent_template_*` template. Never set by callers
    /// directly — `act_spawn_agent` resolves a template into these. Recorded in
    /// the spawn manifest, response, and session row so every run is reproducible
    /// and the fleet is auditable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_config_hash: Option<String>,
}

/// Caller-facing input for `act_spawn_agent` (#909). Either a direct spawn (set
/// `cli`/`kind` and the spawn fields) or a template-rendered spawn (set
/// `template_id` and `template_params`). The two are mutually exclusive: when a
/// template is used, the template fully defines
/// `cli`/`kind`/`model`/`model_ref`/`prompt`/`working_dir`/`target` and supplying
/// any of them is a loud error — an instance is rendered
/// atomically from its versioned template, never assembled piecemeal.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSpawnAgentRequest {
    /// Spawn from this durable template id (see `agent_template_put`). When set,
    /// `cli`/`model`/`prompt`/`working_dir`/`target` must be omitted.
    #[serde(default)]
    #[schemars(default)]
    pub template_id: Option<String>,
    /// Legacy field, retained for contract stability and ignored: templates are
    /// no longer versioned, so a spawn always renders from the current row.
    #[serde(default)]
    #[schemars(default)]
    pub template_version: Option<u32>,
    /// Legacy field, retained for contract stability and ignored: a template's
    /// prompt is used verbatim (no `${slot}` substitution), so no params apply.
    #[serde(default)]
    #[schemars(default)]
    pub template_params: BTreeMap<String, String>,
    /// Direct-spawn agent CLI. Required when `template_id` is omitted; must be
    /// omitted when `template_id` is set (the template supplies the kind).
    #[serde(default)]
    #[schemars(default)]
    pub cli: Option<ActSpawnAgentCli>,
    /// Direct-spawn agent kind. Back-compat alias for `cli`; if both are set
    /// they must match. Must be omitted when `template_id` is set.
    #[serde(default)]
    #[schemars(default)]
    pub kind: Option<ActSpawnAgentCli>,
    /// Direct-spawn model. Must be omitted when `template_id` is set.
    #[serde(default)]
    #[schemars(default)]
    pub model: Option<String>,
    /// Direct-spawn local-model registry row. Valid only with
    /// `kind=local_model`; must be omitted when `template_id` is set.
    #[serde(default)]
    #[schemars(default)]
    pub model_ref: Option<String>,
    /// Direct-spawn work prompt. Required and non-empty when `template_id` is
    /// omitted; must be omitted when `template_id` is set.
    #[serde(default)]
    #[schemars(default)]
    pub prompt: Option<String>,
    /// Direct-spawn perception target. Must be omitted when `template_id` is set.
    #[serde(default)]
    #[schemars(default)]
    pub target: Option<ActSpawnAgentTarget>,
    /// Direct-spawn working directory. Must be omitted when `template_id` is set.
    #[serde(default)]
    #[schemars(default)]
    pub working_dir: Option<String>,
    /// Streamable HTTP MCP endpoint for the spawned agent. Applies to both
    /// direct and template spawns (a runtime knob, not template config).
    #[serde(default = "default_agent_spawn_mcp_url")]
    #[schemars(default = "default_agent_spawn_mcp_url")]
    pub mcp_url: String,
    /// Readback wait budget. Runtime knob; applies to both spawn modes.
    #[serde(default = "default_agent_spawn_wait_timeout_ms")]
    #[schemars(
        default = "default_agent_spawn_wait_timeout_ms",
        range(min = 1, max = 1_800_000)
    )]
    pub wait_timeout_ms: u64,
    /// Provision-only hold-open interval. Runtime knob; applies to both modes.
    #[serde(default = "default_agent_spawn_hold_open_ms")]
    #[schemars(default = "default_agent_spawn_hold_open_ms", range(min = 0))]
    pub hold_open_ms: u64,
    /// Route the spawned agent's risky tool calls through the human Approvals
    /// inbox (#927). Defaults true; a runtime safety knob applied to both
    /// direct and template spawns (not template config). Only affects Claude.
    #[serde(default = "default_require_approval_gate")]
    #[schemars(default = "default_require_approval_gate")]
    pub require_approval_gate: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnTaskStartedParams {
    /// Spawn id issued by `act_spawn_agent`. The MCP request's real
    /// `Mcp-Session-Id` header supplies the session id; callers cannot provide
    /// or spoof it.
    pub spawn_id: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnTaskStartedResponse {
    pub ok: bool,
    pub spawn_id: String,
    pub session_id: String,
    pub cli: ActSpawnAgentCli,
    pub task_started_path: String,
    pub started_at_unix_ms: u64,
    pub readiness_source: String,
    pub artifact: Value,
}

fn default_task_readiness_source() -> String {
    "task_start_artifact".to_owned()
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSpawnAgentResponse {
    pub spawn_id: String,
    pub cli: ActSpawnAgentCli,
    pub kind: ActSpawnAgentCli,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    pub launcher_process_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_process_id: Option<u32>,
    pub session_id: String,
    pub mcp_url: String,
    pub working_dir: String,
    pub launch_target: String,
    pub launch_target_source: String,
    pub launched_at_unix_ms: u64,
    pub registered_at_unix_ms: u64,
    pub task_started_at_unix_ms: u64,
    /// How task-start readiness was proven: current spawns use
    /// `agent_spawn_task_started_tool` (the agent called the daemon MCP
    /// readiness tool, which wrote the artifact from the real request session).
    /// `task_start_artifact` is retained only for older spawn-dir readback.
    #[serde(default = "default_task_readiness_source")]
    pub task_readiness_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ActSpawnAgentTarget>,
    /// Spawn-template provenance (#909): present when this spawn was rendered
    /// from a template. The exact `(id, version, config_hash)` the run used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_config_hash: Option<String>,
    pub log_paths: ActSpawnAgentLogPaths,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSpawnAgentLogPaths {
    pub log_dir: String,
    pub prompt_path: String,
    pub stdout_path: String,
    pub stderr_path: String,
    pub final_message_path: String,
    pub completion_status_path: String,
    pub task_started_path: String,
    /// Legacy compatibility path. Current Claude/Codex spawned-agent readiness
    /// does not materialize this helper; readiness must go through
    /// `agent operation=task_started`.
    pub task_started_script_path: String,
    pub terminal_asciicast_path: String,
    pub terminal_capture_status_path: String,
    pub terminal_final_screen_path: String,
    pub terminal_input_audit_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_config_path: Option<String>,
    /// Claude only: generated `--settings` file that wires the CLI's HTTP
    /// hooks to the daemon's `/agent-events` push-telemetry ingress (#899).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_settings_path: Option<String>,
    /// Codex only: generated `notify` PowerShell script that POSTs
    /// turn-complete events to the same ingress (#899).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_script_path: Option<String>,
    /// Codex only: generated app-server runner script used when spawning Codex
    /// through the interruptible `turn/start` protocol (#958).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_app_server_runner_path: Option<String>,
    /// Codex only: control artifact containing endpoint/thread/turn ids for
    /// `agent_interrupt` to call real `turn/interrupt` (#958).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_app_server_control_path: Option<String>,
    /// Codex only: JSON-RPC event stream from the app-server connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_app_server_events_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_app_server_stdout_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_app_server_stderr_path: Option<String>,
    /// Local-model only: marker/config file written by the #931 runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_model_runner_path: Option<String>,
}

#[must_use]
pub fn default_agent_spawn_mcp_url() -> String {
    "http://127.0.0.1:7700/mcp".to_owned()
}

/// Builds the MCP URL a spawned agent should phone home to, anchored to the
/// daemon's *actual* HTTP bind address rather than the hardcoded default. A
/// daemon running on a non-default port (e.g. an isolated local verification
/// instance, or a future multi-daemon layout) must hand its children its own
/// endpoint, or they connect to the wrong daemon's tools. Loopback is preserved
/// verbatim.
#[must_use]
pub fn agent_spawn_mcp_url_for_bind(bind_addr: std::net::SocketAddr) -> String {
    format!("http://{bind_addr}/mcp")
}

#[must_use]
pub const fn default_agent_spawn_wait_timeout_ms() -> u64 {
    DEFAULT_AGENT_SPAWN_WAIT_TIMEOUT_MS
}

#[must_use]
pub const fn default_agent_spawn_hold_open_ms() -> u64 {
    DEFAULT_AGENT_SPAWN_HOLD_OPEN_MS
}

/// Approval gating is on by default for spawned agents with a supported bridge.
pub const fn default_require_approval_gate() -> bool {
    true
}

pub fn validate_agent_spawn_params(params: &ActSpawnAgentParams) -> Result<(), ErrorData> {
    let agent_kind = params.effective_cli()?;
    if params.mcp_url.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent mcp_url must not be empty",
        ));
    }
    if params.mcp_url.len() > 2_048 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent mcp_url must be <= 2048 bytes",
        ));
    }
    if params.mcp_url.chars().any(char::is_whitespace) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent mcp_url must not contain whitespace",
        ));
    }
    if !(params.mcp_url.starts_with("http://") || params.mcp_url.starts_with("https://")) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent mcp_url must be an http:// or https:// URL",
        ));
    }
    if params.wait_timeout_ms == 0 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent wait_timeout_ms must be >= 1",
        ));
    }
    if params.wait_timeout_ms > MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_spawn_agent wait_timeout_ms must be <= {MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS}"),
        ));
    }
    let prompt_missing_or_blank = params
        .prompt
        .as_deref()
        .is_none_or(|prompt| prompt.trim().is_empty());
    if agent_kind.is_local_model() && prompt_missing_or_blank {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent local_model prompt must not be empty",
        ));
    }
    if params.template_id.is_none() && prompt_missing_or_blank {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent direct spawn prompt must not be empty",
        ));
    }
    if let Some(prompt) = &params.prompt {
        if prompt.len() > MAX_AGENT_SPAWN_PROMPT_BYTES {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("act_spawn_agent prompt must be <= {MAX_AGENT_SPAWN_PROMPT_BYTES} bytes"),
            ));
        }
    }
    if let Some(working_dir) = &params.working_dir {
        if working_dir.trim().is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_spawn_agent working_dir must not be empty",
            ));
        }
    }
    if let Some(model) = &params.model {
        if model.trim().is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_spawn_agent model must not be empty when provided",
            ));
        }
        if model.len() > MAX_AGENT_SPAWN_MODEL_BYTES {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("act_spawn_agent model must be <= {MAX_AGENT_SPAWN_MODEL_BYTES} bytes"),
            ));
        }
        // Provider CLI model ids are passed as a single argv element; reject
        // whitespace/control characters so they cannot smuggle extra
        // arguments. Local-model registry refs may contain spaces and are
        // passed quoted through PowerShell, so they use model_ref validation.
        if !agent_kind.is_local_model()
            && model
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_spawn_agent model must not contain whitespace or control characters",
            ));
        }
    }
    if let Some(model_ref) = &params.model_ref {
        validate_local_model_ref(model_ref)?;
    }
    if agent_kind.is_local_model() {
        let model_ref = params.local_model_ref().ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_spawn_agent local_model requires model_ref",
            )
        })?;
        validate_local_model_ref(model_ref)?;
    } else if params.model_ref.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent model_ref is only valid when kind is local_model",
        ));
    }
    if let Some(ActSpawnAgentTarget::Cdp { cdp_target_id, .. }) = &params.target {
        if cdp_target_id.trim().is_empty()
            || !cdp_target_id.chars().all(|ch| ('!'..='~').contains(&ch))
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_spawn_agent cdp_target_id must contain only visible ASCII characters",
            ));
        }
    }
    Ok(())
}

impl ActSpawnAgentParams {
    pub fn effective_cli(&self) -> Result<ActSpawnAgentCli, ErrorData> {
        match (self.cli, self.kind) {
            (Some(cli), Some(kind)) if cli != kind => Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_spawn_agent cli and kind must match when both are supplied",
            )),
            (Some(cli), _) | (_, Some(cli)) => Ok(cli),
            (None, None) => Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_spawn_agent requires cli or kind",
            )),
        }
    }

    pub fn local_model_ref(&self) -> Option<&str> {
        self.model_ref
            .as_deref()
            .or_else(|| self.model.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    pub fn model_for_spawn_manifest(&self, agent_kind: ActSpawnAgentCli) -> Option<&str> {
        if agent_kind.is_local_model() {
            self.local_model_ref()
        } else {
            self.model.as_deref()
        }
    }
}

fn validate_local_model_ref(model_ref: &str) -> Result<(), ErrorData> {
    let trimmed = model_ref.trim();
    if trimmed.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent model_ref must not be empty",
        ));
    }
    if trimmed.chars().count() > 100 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent model_ref must be at most 100 characters",
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_spawn_agent model_ref must not contain control characters",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActLaunchResponse {
    /// PID of the process act_launch freshly spawned.
    pub pid: u32,
    pub hwnd: Option<i64>,
    /// PID that actually OWNS `hwnd`. Equals `pid` for a normal launch where the
    /// spawned process showed its own window. Differs from `pid` when act_launch
    /// matched a PRE-EXISTING same-app window via the existing-window fallback, or
    /// the target re-exec'd into another process — so `(hwnd, window_owner_pid)`
    /// always refer to the same process even when `pid` does not (#1358).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_owner_pid: Option<u32>,
    /// True when `hwnd` is a window act_launch did NOT freshly spawn: its owner
    /// (`window_owner_pid`) differs from the launched `pid`. The spawned `pid` may
    /// be a separate, still-running (orphaned) process — bind by `hwnd` /
    /// `window_owner_pid`, not `pid`, to drive the matched window (#1358).
    pub reused_existing_window: bool,
    pub matched_title: Option<String>,
    pub launched_at: String,
    pub reason: Option<String>,
    /// CDP debug port opened for a Synapse-launched Chromium browser (#684), if
    /// injection ran. `observe`/`find` use it to attach and read the DOM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_debug_port: Option<u16>,
    /// CDP HTTP endpoint (`http://127.0.0.1:<port>`) when a debug port opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_endpoint: Option<String>,
    /// Dedicated automation `--user-data-dir` the browser was launched with.
    /// NOT the user's primary profile — logins there do not carry over (Chrome
    /// 136+ refuses remote debugging on the default profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_user_data_dir: Option<String>,
    /// URL observed in the launched browser's CDP target list when Synapse
    /// opened the debug port and the caller supplied a URL argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_verified_url: Option<String>,
    /// Title observed for the verified CDP target URL, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_verified_title: Option<String>,
    /// Desktop routing readback when `desktop` was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<ActLaunchDesktopReadback>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActLaunchDesktopReadback {
    pub requested: String,
    pub scope: String,
    pub name: String,
    pub startup_desktop: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

pub fn launch_request_details(params: &ActLaunchParams) -> serde_json::Value {
    json!({
        "target": params.target,
        "args": params.args,
        "working_dir": params.working_dir,
        "env_keys": params.env.keys().cloned().collect::<Vec<_>>(),
        "wait_for_window_title_regex": params.wait_for_window_title_regex,
        "timeout_ms": params.timeout_ms,
        "idempotency_key_present": params.idempotency_key.is_some(),
        "cdp_debug": params.cdp_debug,
        "force_renderer_accessibility": params.force_renderer_accessibility,
        "windows_console_window_state": params.windows_console_window_state,
        "desktop": params.desktop,
        "windows_new_console": launch_target_needs_new_console(&params.target),
        "request_sha256": launch_request_sha256(params).ok(),
    })
}

pub fn launch_process_history_row_key(response: &ActLaunchResponse) -> Vec<u8> {
    format!(
        "process_history/v1/act_launch/{}/{}",
        response.launched_at.replace(':', "_"),
        response.pid
    )
    .into_bytes()
}

pub fn launch_process_history_row(
    params: &ActLaunchParams,
    response: &ActLaunchResponse,
) -> Result<Vec<u8>, ErrorData> {
    let row = json!({
        "schema_version": 1,
        "row_kind": "process_start",
        "tool": "act_launch",
        "status": "started",
        "target": params.target,
        "args": params.args,
        "working_dir": params.working_dir,
        "env_keys": params.env.keys().cloned().collect::<Vec<_>>(),
        "wait_for_window_title_regex": params.wait_for_window_title_regex,
        "timeout_ms": params.timeout_ms,
        "idempotency_key_present": params.idempotency_key.is_some(),
        "windows_new_console": launch_target_needs_new_console(&params.target),
        "request_sha256": launch_request_sha256(params).ok(),
        "command_line": launch_command_line(params).ok(),
        "pid": response.pid,
        "hwnd": response.hwnd,
        "matched_title": response.matched_title,
        "launched_at": response.launched_at,
        "reason": response.reason,
        "cdp_debug": params.cdp_debug,
        "force_renderer_accessibility": params.force_renderer_accessibility,
        "windows_console_window_state": params.windows_console_window_state,
        "cdp_debug_port": response.cdp_debug_port,
        "cdp_endpoint": response.cdp_endpoint,
        "cdp_user_data_dir": response.cdp_user_data_dir,
        "cdp_verified_url": response.cdp_verified_url,
        "cdp_verified_title": response.cdp_verified_title,
        "desktop": params.desktop,
        "desktop_readback": response.desktop,
    });
    encode_json(&row).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_launch process history row encode failed: {error}"),
        )
    })
}

pub async fn execute_combo(
    runtime: Arc<Mutex<ReflexRuntime>>,
    params: ActComboParams,
) -> Result<ActComboResponse, ErrorData> {
    validate_combo_params(&params)?;
    let idempotency_present = params.idempotency_key.is_some();
    let started = Instant::now();
    let steps = combo_steps_from_params(&params)?;
    let scheduled_steps = u32::try_from(steps.len()).map_err(|_err| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_combo expanded steps length exceeds u32::MAX",
        )
    })?;
    let combo_id = new_reflex_id();
    let reflex = ScheduledReflex::combo(combo_id.clone(), ComboParams::new(steps, params.backend));
    let state = runtime
        .lock()
        .map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "reflex runtime lock poisoned",
            )
        })?
        .register(&reflex)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    tracing::info!(
        code = "M4_ACT_COMBO_EXECUTED",
        combo_id = %combo_id,
        idempotency_present,
        scheduled_steps,
        backend = ?params.backend,
        state = ?state.state,
        "readback=act_combo after=reflex_runtime_register"
    );
    Ok(ActComboResponse {
        combo_id,
        scheduled_steps,
        backend: params.backend,
        started_at_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

pub fn required_combo_permissions(
    params: &ActComboParams,
) -> Result<RequiredPermissions, ErrorData> {
    validate_combo_params(params)?;
    let action = Action::Combo {
        steps: combo_steps_from_params(params)?,
        backend: params.backend,
    };
    let mut required = RequiredPermissions::new();
    add_action_permissions(&action, &mut required);
    Ok(required)
}

#[allow(
    dead_code,
    reason = "kept as the direct M4 execution helper for unit tests and non-server callers"
)]
pub async fn run_shell(
    config: &M4ServiceConfig,
    params: ActRunShellParams,
) -> Result<ActRunShellResponse, ErrorData> {
    let authorization = authorize_run_shell(config, &params)?;
    run_authorized_shell(
        params,
        &authorization,
        config.run_shell_inline_await_limit_ms(),
        None,
    )
    .await
}

pub fn authorize_run_shell(
    config: &M4ServiceConfig,
    params: &ActRunShellParams,
) -> Result<RunShellAuthorization, ErrorData> {
    validate_run_shell_params(params)?;
    let command_line = shell_command_line(params);
    let Some(matched_pattern) = config.shell_match(&command_line) else {
        let command_metadata = shell_command_metadata(&params.command, &params.args);
        let reason = if config.allow_shell_count() == 0 {
            "no_allow_shell_policy"
        } else {
            "shell_command_not_allowlisted"
        };
        return Err(policy_error(
            error_codes::SAFETY_SHELL_DENIED_BY_POLICY,
            "act_run_shell command is not permitted by --allow-shell policy",
            json!({
                "code": error_codes::SAFETY_SHELL_DENIED_BY_POLICY,
                "command": params.command,
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": command_metadata.args,
                "args_redacted": command_metadata.args_redacted,
                "args_original_count": command_metadata.args_original_count,
                "args_original_bytes": command_metadata.args_original_bytes,
                "args_sha256": command_metadata.args_sha256,
                "command_line": command_metadata.command_line,
                "command_line_redacted": command_metadata.command_line_redacted,
                "command_line_original_bytes": command_metadata.command_line_original_bytes,
                "command_line_sha256": command_metadata.command_line_sha256,
                "working_dir": params.working_dir,
                "env_keys": params.env.keys().cloned().collect::<Vec<_>>(),
                "timeout_ms": params.timeout_ms,
                "idempotency_key_present": params.idempotency_key.is_some(),
                "allow_shell_patterns": config.allow_shell_count(),
                "reason": reason,
            }),
        ));
    };
    Ok(RunShellAuthorization {
        command_line,
        matched_pattern: matched_pattern.to_owned(),
    })
}

pub async fn run_authorized_shell(
    params: ActRunShellParams,
    authorization: &RunShellAuthorization,
    inline_await_limit_ms: u64,
    context: Option<&ShellExecutionContext>,
) -> Result<ActRunShellResponse, ErrorData> {
    validate_run_shell_execution_plan(&params, inline_await_limit_ms)?;
    let started = Instant::now();
    let idempotency_present = params.idempotency_key.is_some();
    let trace_metadata = shell_command_metadata(&params.command, &params.args);
    let requested_execution_mode = params.execution_mode;
    let result = if let Some(background_reason) =
        direct_shell_background_reason(&params, inline_await_limit_ms)
    {
        let start_params = run_shell_params_to_start_params(params);
        let started_job = start_authorized_shell_job(start_params, authorization, context)?;
        act_run_shell_background_response(
            started_job.job,
            elapsed_ms_u32(started),
            background_reason,
            inline_await_limit_ms,
            requested_execution_mode,
        )
    } else {
        run_allowlisted_shell(params, inline_await_limit_ms, context).await?
    };
    let trace_command_line = if let Some(job) = &result.job {
        job.command_line.as_str()
    } else {
        trace_metadata.command_line.as_str()
    };
    let trace_command_line_sha256 = if let Some(job) = &result.job {
        job.command_line_sha256.as_str()
    } else {
        trace_metadata.command_line_sha256.as_str()
    };
    tracing::info!(
        code = "M4_ACT_RUN_SHELL_EXECUTED",
        command_line = %trace_command_line,
        command_line_sha256 = %trace_command_line_sha256,
        matched_pattern = %authorization.matched_pattern,
        exit_code = ?result.exit_code,
        duration_ms = result.duration_ms,
        timed_out = result.timed_out,
        backgrounded = result.backgrounded,
        job_id = ?result.job_id,
        inline_await_limit_ms = ?result.inline_await_limit_ms,
        stdout_truncated = result.stdout_truncated,
        stderr_truncated = result.stderr_truncated,
        session_id = ?result.session_id,
        effective_working_dir = ?result.effective_working_dir,
        idempotency_present,
        "readback=act_run_shell after=process_complete"
    );
    Ok(result)
}

fn direct_shell_background_reason(
    params: &ActRunShellParams,
    inline_await_limit_ms: u64,
) -> Option<&'static str> {
    match params.execution_mode {
        ActRunShellExecutionMode::Auto if params.timeout_ms > inline_await_limit_ms => {
            Some("timeout_exceeds_inline_await_budget")
        }
        ActRunShellExecutionMode::Auto
            if params.timeout_ms > DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS =>
        {
            Some("timeout_exceeds_mcp_client_call_budget")
        }
        ActRunShellExecutionMode::Inline
            if params.timeout_ms > DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS =>
        {
            Some("inline_timeout_exceeds_mcp_client_call_budget")
        }
        ActRunShellExecutionMode::Durable => Some("execution_mode_durable"),
        ActRunShellExecutionMode::Auto | ActRunShellExecutionMode::Inline => None,
    }
}

pub fn validate_run_shell_execution_plan(
    params: &ActRunShellParams,
    inline_await_limit_ms: u64,
) -> Result<(), ErrorData> {
    let _ = (params, inline_await_limit_ms);
    Ok(())
}

fn run_shell_params_to_start_params(params: ActRunShellParams) -> ActRunShellStartParams {
    ActRunShellStartParams {
        command: params.command,
        args: params.args,
        working_dir: params.working_dir,
        env: params.env,
        timeout_ms: params.durable_timeout_ms,
        job_id: None,
    }
}

fn act_run_shell_background_response(
    job: ActRunShellJobStatus,
    duration_ms: u32,
    reason: &'static str,
    inline_await_limit_ms: u64,
    requested_execution_mode: ActRunShellExecutionMode,
) -> ActRunShellResponse {
    let durable_timeout_ms = job.timeout_ms;
    ActRunShellResponse {
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        duration_ms,
        timed_out: false,
        error_code: None,
        error_message: None,
        stdout_truncated: false,
        stderr_truncated: false,
        session_id: job.session_id.clone(),
        effective_working_dir: job.effective_working_dir.clone(),
        backgrounded: true,
        background_reason: Some(reason.to_owned()),
        inline_await_limit_ms: Some(inline_await_limit_ms),
        inline_client_call_budget_ms: Some(DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS),
        requested_execution_mode: Some(requested_execution_mode),
        effective_execution_mode: Some(ActRunShellExecutionMode::Durable),
        durable_timeout_ms,
        job_id: Some(job.job_id.clone()),
        job: Some(job),
    }
}

pub fn run_shell_request_details(
    params: &ActRunShellParams,
    inline_await_limit_ms: u64,
) -> serde_json::Value {
    let background_reason = direct_shell_background_reason(params, inline_await_limit_ms);
    let will_background = background_reason.is_some();
    let command_metadata = shell_command_metadata(&params.command, &params.args);
    let durable_timeout_ms_if_backgrounded = if will_background {
        params.durable_timeout_ms
    } else {
        None
    };
    json!({
        "command": params.command,
        "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
        "args": command_metadata.args,
        "args_redacted": command_metadata.args_redacted,
        "args_original_count": command_metadata.args_original_count,
        "args_original_bytes": command_metadata.args_original_bytes,
        "args_sha256": command_metadata.args_sha256,
        "command_line": command_metadata.command_line,
        "command_line_redacted": command_metadata.command_line_redacted,
        "command_line_original_bytes": command_metadata.command_line_original_bytes,
        "command_line_sha256": command_metadata.command_line_sha256,
        "working_dir": params.working_dir,
        "env_keys": params.env.keys().cloned().collect::<Vec<_>>(),
        "timeout_ms": params.timeout_ms,
        "execution_mode": params.execution_mode.as_str(),
        "inline_await_limit_ms": inline_await_limit_ms,
        "inline_client_call_budget_ms": DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS,
        "background_reason": background_reason,
        "will_background": will_background,
        "durable_timeout_ms": params.durable_timeout_ms,
        "durable_timeout_ms_if_backgrounded": durable_timeout_ms_if_backgrounded,
        "durable_timeout_policy": if will_background && params.durable_timeout_ms.is_some() {
            "explicit_timeout_ms"
        } else if will_background {
            "unbounded_until_exit_or_cancel"
        } else if params.durable_timeout_ms.is_some() {
            "ignored_inline_execution"
        } else {
            "inline_timeout_only"
        },
        "idempotency_key_present": params.idempotency_key.is_some(),
        "request_sha256": run_shell_request_sha256(params).ok(),
    })
}

const fn is_false(value: &bool) -> bool {
    !*value
}

pub fn authorize_run_shell_start(
    config: &M4ServiceConfig,
    params: &ActRunShellStartParams,
) -> Result<RunShellAuthorization, ErrorData> {
    validate_run_shell_start_params(params)?;
    let shell_params = run_shell_params_for_start_validation(params);
    authorize_run_shell(config, &shell_params)
}

pub fn run_shell_start_request_details(params: &ActRunShellStartParams) -> serde_json::Value {
    let command_metadata = shell_command_metadata(&params.command, &params.args);
    json!({
        "command": params.command,
        "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
        "args": command_metadata.args,
        "args_redacted": command_metadata.args_redacted,
        "args_original_count": command_metadata.args_original_count,
        "args_original_bytes": command_metadata.args_original_bytes,
        "args_sha256": command_metadata.args_sha256,
        "command_line": command_metadata.command_line,
        "command_line_redacted": command_metadata.command_line_redacted,
        "command_line_original_bytes": command_metadata.command_line_original_bytes,
        "command_line_sha256": command_metadata.command_line_sha256,
        "working_dir": params.working_dir,
        "env_keys": params.env.keys().cloned().collect::<Vec<_>>(),
        "timeout_ms": params.timeout_ms,
        "durable_timeout_policy": if params.timeout_ms.is_some() {
            "explicit_timeout_ms"
        } else {
            "unbounded_until_exit_or_cancel"
        },
        "job_id": params.job_id,
        "request_sha256": run_shell_start_request_sha256(params).ok(),
    })
}

pub fn shell_execution_context_for_session(
    session_id: &str,
) -> Result<ShellExecutionContext, ErrorData> {
    validate_shell_session_id(session_id)?;
    let session_dir = shell_session_root_dir()?.join(shell_session_dir_name(session_id));
    let default_working_dir = session_dir.join("cwd");
    fs::create_dir_all(&default_working_dir).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell failed to create per-session working directory: {error}"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "session_id": session_id,
                "path": default_working_dir,
                "reason": "session_working_dir_create_failed",
            }),
        )
    })?;
    let session_dir = fs::canonicalize(&session_dir).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell failed to resolve per-session directory: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "session_id": session_id,
                "path": session_dir,
                "reason": "session_dir_canonicalize_failed",
            }),
        )
    })?;
    let default_working_dir = fs::canonicalize(&default_working_dir).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell failed to resolve per-session working directory: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "session_id": session_id,
                "path": default_working_dir,
                "reason": "session_working_dir_canonicalize_failed",
            }),
        )
    })?;
    Ok(ShellExecutionContext {
        session_id: session_id.to_owned(),
        session_dir,
        default_working_dir,
    })
}

pub fn prepare_run_shell_params_for_context(
    mut params: ActRunShellParams,
    context: &ShellExecutionContext,
) -> Result<ActRunShellParams, ErrorData> {
    let effective_working_dir = resolve_shell_working_dir(
        params.working_dir.as_deref(),
        Some(context),
        "act_run_shell",
    )?;
    params.working_dir = Some(path_string(&effective_working_dir));
    Ok(params)
}

pub fn prepare_run_shell_start_params_for_context(
    mut params: ActRunShellStartParams,
    context: &ShellExecutionContext,
) -> Result<ActRunShellStartParams, ErrorData> {
    let effective_working_dir = resolve_shell_working_dir(
        params.working_dir.as_deref(),
        Some(context),
        "act_run_shell_start",
    )?;
    params.working_dir = Some(path_string(&effective_working_dir));
    Ok(params)
}

pub fn start_authorized_shell_job(
    params: ActRunShellStartParams,
    authorization: &RunShellAuthorization,
    context: Option<&ShellExecutionContext>,
) -> Result<ActRunShellStartResponse, ErrorData> {
    let started = Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339();
    let request_sha256 = run_shell_start_request_sha256(&params)?;
    let (job_id, paths) = create_shell_job_paths(params.job_id.as_deref())?;
    write_shell_job_request(&paths, &params, &request_sha256, context)?;
    write_shell_remote_cleanup_invocation(&paths, &params)?;

    let stdout_file = open_shell_job_output(&paths.stdout_path, "stdout", &job_id)?;
    let stderr_file = open_shell_job_output(&paths.stderr_path, "stderr", &job_id)?;
    let spawned = match spawn_shell_job_child(&params, &job_id, stdout_file, stderr_file, context) {
        Ok(spawned) => spawned,
        Err(error) => {
            let mut status = shell_job_status_record(
                &job_id,
                "spawn_failed",
                &params,
                &paths,
                &request_sha256,
                authorization,
                started_at,
                None,
                context,
            );
            status.completed_at = Some(chrono::Utc::now().to_rfc3339());
            status.duration_ms =
                Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            status.error_code = Some(extract_error_code(&error));
            status.error_message = Some(error.message.to_string());
            if let Err(write_error) = write_shell_job_status(&paths.status_path, &status) {
                tracing::error!(
                    code = "M4_ACT_RUN_SHELL_JOB_STATUS_WRITE_FAILED_AFTER_SPAWN_FAILURE",
                    job_id = %job_id,
                    error = ?write_error,
                    "act_run_shell_start could not persist spawn failure status"
                );
            }
            return Err(error);
        }
    };
    let mut child = spawned.child;
    let process_job = spawned.process_job;

    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        let mut status = shell_job_status_record(
            &job_id,
            "pid_unavailable",
            &params,
            &paths,
            &request_sha256,
            authorization,
            started_at,
            None,
            context,
        );
        status.completed_at = Some(chrono::Utc::now().to_rfc3339());
        status.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        status.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
        status.error_message = Some("spawned process id was unavailable".to_owned());
        write_shell_job_status(&paths.status_path, &status)?;
        return Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell_start spawned a child process but could not read its pid",
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "job_id": job_id,
                "reason": "pid_unavailable",
                "status_path": paths.status_path,
            }),
        ));
    };

    let status = shell_job_status_record(
        &job_id,
        "running",
        &params,
        &paths,
        &request_sha256,
        authorization,
        started_at,
        Some(pid),
        context,
    );
    write_shell_job_status(&paths.status_path, &status)?;

    let monitor_paths = paths.clone();
    let monitor_status = status.clone();
    let monitor_original_args = params.args.clone();
    tokio::spawn(async move {
        monitor_shell_job(
            child,
            process_job,
            monitor_status,
            monitor_paths,
            started,
            monitor_original_args,
        )
        .await;
    });

    let command_metadata = shell_command_metadata(&params.command, &params.args);
    tracing::info!(
        code = "M4_ACT_RUN_SHELL_JOB_STARTED",
        job_id = %job_id,
        pid,
        command_line = %command_metadata.command_line,
        command_line_sha256 = %command_metadata.command_line_sha256,
        matched_pattern = %authorization.matched_pattern,
        timeout_ms = ?params.timeout_ms,
        session_id = ?status.session_id,
        effective_working_dir = ?status.effective_working_dir,
        status_path = %paths.status_path.display(),
        stdout_path = %paths.stdout_path.display(),
        stderr_path = %paths.stderr_path.display(),
        "readback=act_run_shell_start after=job_status_persisted"
    );
    Ok(ActRunShellStartResponse { job: status })
}

pub fn shell_job_status(
    params: &ActRunShellStatusParams,
    session_id: Option<&str>,
) -> Result<ActRunShellStatusResponse, ErrorData> {
    validate_shell_job_id(&params.job_id)?;
    if params.tail_bytes > SHELL_JOB_TAIL_MAX_BYTES {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_run_shell_status tail_bytes must be <= {SHELL_JOB_TAIL_MAX_BYTES}"),
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "job_id": params.job_id,
                "tail_bytes": params.tail_bytes,
                "max_tail_bytes": SHELL_JOB_TAIL_MAX_BYTES,
                "reason": "tail_bytes_too_large",
            }),
        ));
    }
    let paths = shell_job_paths_for_id(session_id, &params.job_id)?;
    let mut job = read_shell_job_status(&paths.status_path, &params.job_id)?;
    job = reconcile_shell_job_process_state(job, &paths)?;
    refresh_shell_job_remote_metadata_from_outputs(&mut job, &paths)?;
    let mut running = shell_job_process_still_running(&job);
    if shell_job_live_status(&job.status) && !running {
        job = reconcile_shell_job_process_state(job, &paths)?;
        refresh_shell_job_remote_metadata_from_outputs(&mut job, &paths)?;
        running = shell_job_process_still_running(&job);
    }
    if reconcile_shell_job_remote_exit_marker(
        &mut job,
        &paths,
        running,
        "act_run_shell_status_remote_exit_readback",
    )? {
        running = shell_job_process_still_running(&job);
    }
    if reconcile_shell_job_remote_already_gone_if_local_stale(
        &mut job,
        &paths,
        running,
        "act_run_shell_status_remote_liveness_readback",
    ) {
        running = shell_job_process_still_running(&job);
    }
    verify_shell_job_remote_cleanup_after_terminal(
        &mut job,
        &paths,
        "act_run_shell_status_terminal_readback",
        None,
    );
    let tail_bytes =
        usize::try_from(params.tail_bytes).unwrap_or(SHELL_JOB_TAIL_MAX_BYTES as usize);
    let stdout_len_bytes = file_len(&paths.stdout_path, &params.job_id, "stdout")?;
    let stderr_len_bytes = file_len(&paths.stderr_path, &params.job_id, "stderr")?;
    let diagnostics =
        shell_job_status_diagnostics(&job, running, stdout_len_bytes, stderr_len_bytes);
    job.diagnostics = Some(diagnostics);
    let (job, running) =
        write_shell_job_status_readback(&paths, job, running, stdout_len_bytes, stderr_len_bytes)?;
    let stdout_tail = tail_file_lossy(&paths.stdout_path, tail_bytes)?;
    let stderr_tail = tail_file_lossy(&paths.stderr_path, tail_bytes)?;
    tracing::info!(
        code = "M4_ACT_RUN_SHELL_JOB_STATUS_READ",
        job_id = %params.job_id,
        session_id = ?session_id,
        status = %job.status,
        running,
        stdout_len_bytes,
        stderr_len_bytes,
        output_state = ?job.diagnostics.as_ref().map(|diagnostics| diagnostics.output_state.as_str()),
        transfer_protocol_hint = ?job
            .diagnostics
            .as_ref()
            .and_then(|diagnostics| diagnostics.transfer.as_ref())
            .map(|transfer| transfer.protocol_hint.as_str()),
        "readback=act_run_shell_status after=status_file_and_process_table"
    );
    Ok(ActRunShellStatusResponse {
        job,
        running,
        stdout_len_bytes,
        stderr_len_bytes,
        stdout_tail,
        stderr_tail,
    })
}

fn write_shell_job_status_readback(
    paths: &ShellJobPaths,
    candidate: ActRunShellJobStatus,
    candidate_running: bool,
    stdout_len_bytes: u64,
    stderr_len_bytes: u64,
) -> Result<(ActRunShellJobStatus, bool), ErrorData> {
    let candidate_status = candidate.status.clone();
    let candidate_exit_code = candidate.exit_code;
    let candidate_completed_at = candidate.completed_at.clone();
    let mut persisted = write_shell_job_reconciliation_status(paths, candidate)?;
    let terminal_won = persisted.status != candidate_status
        || persisted.exit_code != candidate_exit_code
        || persisted.completed_at != candidate_completed_at;
    if !terminal_won {
        return Ok((persisted, candidate_running));
    }

    let persisted_running = shell_job_process_still_running(&persisted);
    persisted.diagnostics = Some(shell_job_status_diagnostics(
        &persisted,
        persisted_running,
        stdout_len_bytes,
        stderr_len_bytes,
    ));
    let persisted = write_shell_job_reconciliation_status(paths, persisted)?;
    let running = shell_job_process_still_running(&persisted);
    Ok((persisted, running))
}

fn shell_job_process_still_running(job: &ActRunShellJobStatus) -> bool {
    shell_job_live_status(&job.status)
        && job
            .pid
            .is_some_and(|pid| shell_job_live_process_ids(&[pid]).contains(&pid))
}

fn shell_job_status_diagnostics(
    job: &ActRunShellJobStatus,
    running: bool,
    stdout_len_bytes: u64,
    stderr_len_bytes: u64,
) -> ActRunShellJobDiagnostics {
    let process_tree = shell_job_process_diagnostics(job.pid.filter(|_| running));
    let output_state = shell_job_output_state(running, stdout_len_bytes, stderr_len_bytes);
    let transfer = shell_job_transfer_diagnostics(job, &process_tree);
    let mut actionable_hints = Vec::new();
    if running && stdout_len_bytes == 0 && stderr_len_bytes == 0 {
        actionable_hints.push(
            "child_process_running_no_stdout_or_stderr_yet_check_process_tree_and_protocol"
                .to_owned(),
        );
    }
    if let Some(transfer) = &transfer {
        actionable_hints.extend(transfer.suggested_next_steps.iter().cloned());
    }
    actionable_hints.extend(shell_job_remote_command_exit_status_hints(job));
    actionable_hints.sort();
    actionable_hints.dedup();
    ActRunShellJobDiagnostics {
        checked_at: chrono::Utc::now().to_rfc3339(),
        running,
        elapsed_ms: elapsed_ms_since_rfc3339(&job.started_at),
        stdout_len_bytes,
        stderr_len_bytes,
        output_state: output_state.to_owned(),
        process_tree,
        transfer,
        actionable_hints,
    }
}

fn shell_job_remote_command_exit_status_hints(job: &ActRunShellJobStatus) -> Vec<String> {
    if job.status != "exit_nonzero"
        || job.exit_code != Some(1)
        || job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_SSH
    {
        return Vec::new();
    }
    let Some(parts) = ssh_direct_command_parts(&job.args) else {
        return Vec::new();
    };
    let Some(remote_command) = parts.remote_command else {
        return Vec::new();
    };
    if ssh_remote_command_has_bash_login_errexit_hazard(&remote_command) {
        return vec![
            "bash_login_shell_errexit_can_override_inner_exit_status_use_bash_c_or_disable_errexit_before_exit"
                .to_owned(),
        ];
    }
    Vec::new()
}

fn ssh_remote_command_has_bash_login_errexit_hazard(remote_command: &str) -> bool {
    let lower = remote_command.to_ascii_lowercase();
    let invokes_login_bash = lower.contains("bash -lc")
        || lower.contains("bash -l -c")
        || lower.contains("bash --login -c");
    let enables_errexit = lower.contains("set -e") || lower.contains("set -o errexit");
    invokes_login_bash && enables_errexit
}

fn shell_job_output_state(
    running: bool,
    stdout_len_bytes: u64,
    stderr_len_bytes: u64,
) -> &'static str {
    match (running, stdout_len_bytes > 0, stderr_len_bytes > 0) {
        (true, false, false) => "running_no_output",
        (true, true, false) => "running_stdout_only",
        (true, false, true) => "running_stderr_only",
        (true, true, true) => "running_stdout_stderr",
        (false, false, false) => "terminal_no_output",
        (false, true, false) => "terminal_stdout_only",
        (false, false, true) => "terminal_stderr_only",
        (false, true, true) => "terminal_stdout_stderr",
    }
}

fn shell_job_process_diagnostics(root_pid: Option<u32>) -> Vec<ActRunShellProcessDiagnostic> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let Some(root_pid) = root_pid else {
        return Vec::new();
    };
    let process_ids = shell_job_process_tree_ids(root_pid);
    let pids = process_ids
        .iter()
        .copied()
        .map(Pid::from_u32)
        .collect::<Vec<_>>();
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&pids), true);
    process_ids
        .into_iter()
        .filter_map(|pid| {
            let process = system.process(Pid::from_u32(pid))?;
            Some(ActRunShellProcessDiagnostic {
                pid,
                parent_pid: process.parent().map(|parent| parent.as_u32()),
                name: process.name().to_string_lossy().into_owned(),
            })
        })
        .collect()
}

fn shell_job_transfer_diagnostics(
    job: &ActRunShellJobStatus,
    process_tree: &[ActRunShellProcessDiagnostic],
) -> Option<ActRunShellTransferDiagnostics> {
    let (client, evidence) = shell_job_transfer_client(job, process_tree)?;
    let protocol_hint = shell_transfer_protocol_hint(client, &job.args).to_owned();
    let suggested_next_steps = shell_transfer_suggested_next_steps(client, &protocol_hint);
    Some(ActRunShellTransferDiagnostics {
        family: "ssh_file_transfer".to_owned(),
        client: client.to_owned(),
        protocol_hint,
        remote_identity: shell_transfer_remote_identity(client, &job.args)
            .or_else(|| job.remote_process_scope.remote_identity.clone()),
        detection_evidence: evidence,
        suggested_next_steps,
    })
}

fn shell_job_transfer_client(
    job: &ActRunShellJobStatus,
    process_tree: &[ActRunShellProcessDiagnostic],
) -> Option<(&'static str, Vec<String>)> {
    if let Some(client) = ssh_family_client_for_executable(&job.command) {
        return Some((
            client,
            vec![format!(
                "direct_command_ssh_family:{client}:{}",
                executable_leaf(&job.command)
            )],
        ));
    }
    for process in process_tree {
        if let Some(client) = ssh_family_client_for_executable(&process.name) {
            return Some((
                client,
                vec![format!(
                    "process_tree_ssh_family:{client}:{}:{}",
                    process.pid, process.name
                )],
            ));
        }
    }
    None
}

fn shell_transfer_protocol_hint(client: &str, args: &[String]) -> &'static str {
    match client {
        "scp" if scp_legacy_protocol_forced(args) => "scp_legacy_protocol_forced_by_-O",
        "scp" => "scp_default_sftp_protocol",
        "sftp" => "sftp_protocol",
        "ssh" => "ssh_remote_command_or_transport",
        _ => "unknown_ssh_family_transport",
    }
}

fn shell_transfer_suggested_next_steps(client: &str, protocol_hint: &str) -> Vec<String> {
    let mut steps = Vec::new();
    match client {
        "scp" => {
            if protocol_hint == "scp_default_sftp_protocol" {
                steps.push("if_server_lacks_sftp_retry_scp_with_-O_legacy_protocol".to_owned());
            }
            steps.push(
                "rerun_with_-v_to_surface_ssh_auth_subsystem_and_protocol_progress".to_owned(),
            );
            steps.push("check_remote_sftp_subsystem_auth_and_path_expansion".to_owned());
        }
        "sftp" => {
            steps.push("rerun_with_-v_to_surface_sftp_subsystem_progress".to_owned());
            steps.push("check_remote_sftp_subsystem_auth_and_path_permissions".to_owned());
        }
        "ssh" => {
            steps.push("rerun_with_-v_or_batchmode_to_surface_ssh_auth_progress".to_owned());
            steps.push("check_remote_command_tty_stdin_and_auth_prompts".to_owned());
        }
        _ => {}
    }
    steps
}

fn shell_transfer_remote_identity(client: &str, args: &[String]) -> Option<String> {
    match client {
        "ssh" | "sftp" => ssh_remote_identity(args),
        "scp" => scp_remote_identity(args),
        _ => None,
    }
}

fn scp_legacy_protocol_forced(args: &[String]) -> bool {
    args.iter().any(|arg| trim_arg_quotes(arg) == "-O")
}

fn scp_remote_identity(args: &[String]) -> Option<String> {
    let mut index = 0;
    let mut options_done = false;
    while index < args.len() {
        let arg = trim_arg_quotes(&args[index]);
        if arg.is_empty() {
            index += 1;
            continue;
        }
        if !options_done && arg == "--" {
            options_done = true;
            index += 1;
            continue;
        }
        if !options_done && arg.starts_with('-') && arg != "-" {
            index += if scp_option_consumes_next(arg, args.get(index + 1)) {
                2
            } else {
                1
            };
            continue;
        }
        if let Some(remote) = scp_remote_endpoint(arg) {
            return Some(remote);
        }
        index += 1;
    }
    None
}

fn scp_option_consumes_next(arg: &str, next: Option<&String>) -> bool {
    if arg.contains('=') || next.is_none() {
        return false;
    }
    matches!(
        arg,
        "-c" | "-D" | "-F" | "-i" | "-J" | "-l" | "-o" | "-P" | "-S" | "-X"
    )
}

fn scp_remote_endpoint(arg: &str) -> Option<String> {
    if let Some(uri) = arg.strip_prefix("scp://") {
        return uri
            .split('/')
            .next()
            .filter(|endpoint| !endpoint.is_empty())
            .map(ToOwned::to_owned);
    }
    let colon = arg.find(':')?;
    if colon == 1 && arg.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    (colon > 0).then(|| arg[..colon].to_owned())
}

pub fn cancel_shell_job(
    params: &ActRunShellJobIdParams,
    session_id: Option<&str>,
) -> Result<ActRunShellCancelResponse, ErrorData> {
    validate_shell_job_id(&params.job_id)?;
    let paths = shell_job_paths_for_id(session_id, &params.job_id)?;
    let mut job = read_shell_job_status(&paths.status_path, &params.job_id)?;
    let before_status = job.status.clone();
    let mut cancel_requested = false;
    let mut termination_attempted = false;
    let mut termination_status = "already_terminal".to_owned();
    let mut remaining_process_ids = Vec::new();

    if shell_job_live_status(&job.status) {
        cancel_requested = true;
        ensure_shell_job_remote_scope_from_process_tree(&mut job);
        job.cancel_requested = true;
        job.status = "cancel_requested".to_owned();
        let _ = wait_for_shell_job_remote_metadata(
            &mut job,
            &paths,
            Duration::from_millis(SHELL_REMOTE_METADATA_WAIT_MS),
        )?;
        write_shell_job_status(&paths.status_path, &job)?;
        if let Some(pid) = job.pid {
            let termination = terminate_shell_job_process_tree(pid);
            termination_attempted = termination.attempted;
            termination_status = termination.status;
            remaining_process_ids = termination.remaining_process_ids;
        } else {
            termination_status = "pid_unavailable".to_owned();
        }
        refresh_shell_job_remote_metadata_from_outputs(&mut job, &paths)?;
        let _remote_cleanup_status =
            attempt_shell_job_remote_cleanup(&mut job, &paths, "act_run_shell_cancel", None);
        if job.remote_process_scope.remote_cleanup_required
            && !job.remote_process_scope.remote_cleanup_verified
            && job.remote_process_scope.remote_cleanup_status != SHELL_REMOTE_CLEANUP_FAILED
            && !mark_shell_job_remote_pre_marker_terminal_if_detected(
                &mut job,
                &paths,
                "act_run_shell_cancel",
            )?
        {
            mark_shell_job_remote_cleanup_unverified(
                &mut job,
                "act_run_shell_cancel",
                &termination_status,
            );
        }
        termination_status =
            remote_aware_termination_status(&termination_status, &job.remote_process_scope);
        write_shell_job_status(&paths.status_path, &job)?;
    } else if job.remote_process_scope.remote_cleanup_required
        && !job.remote_process_scope.remote_cleanup_verified
    {
        refresh_shell_job_remote_metadata_from_outputs(&mut job, &paths)?;
        let _remote_cleanup_status =
            attempt_shell_job_remote_cleanup(&mut job, &paths, "act_run_shell_cancel", None);
        if !job.remote_process_scope.remote_cleanup_verified
            && job.remote_process_scope.remote_cleanup_status != SHELL_REMOTE_CLEANUP_FAILED
            && !mark_shell_job_remote_pre_marker_terminal_if_detected(
                &mut job,
                &paths,
                "act_run_shell_cancel",
            )?
        {
            mark_shell_job_remote_cleanup_unverified(
                &mut job,
                "act_run_shell_cancel",
                &termination_status,
            );
        }
        termination_status =
            remote_aware_termination_status(&termination_status, &job.remote_process_scope);
        write_shell_job_status(&paths.status_path, &job)?;
    }

    let status = shell_job_status(
        &ActRunShellStatusParams {
            job_id: params.job_id.clone(),
            tail_bytes: default_shell_job_tail_bytes(),
        },
        session_id,
    )?;
    tracing::info!(
        code = "M4_ACT_RUN_SHELL_JOB_CANCEL_READBACK",
        job_id = %params.job_id,
        session_id = ?session_id,
        before_status = %before_status,
        after_status = %status.job.status,
        termination_status = %termination_status,
        remaining_process_ids = ?remaining_process_ids,
        remote_transport = %status.job.remote_process_scope.transport,
        remote_cleanup_status = %status.job.remote_process_scope.remote_cleanup_status,
        remote_cleanup_verified = status.job.remote_process_scope.remote_cleanup_verified,
        "readback=act_run_shell_cancel after=status_file_and_process_table"
    );
    if status.job.remote_process_scope.remote_cleanup_required
        && !status.job.remote_process_scope.remote_cleanup_verified
    {
        tracing::warn!(
            code = error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED,
            job_id = %params.job_id,
            session_id = ?session_id,
            remote_identity = ?status.job.remote_process_scope.remote_identity,
            remote_cleanup_status = %status.job.remote_process_scope.remote_cleanup_status,
            remote_cleanup_message = ?status.job.remote_process_scope.remote_cleanup_message,
            "act_run_shell_cancel verified local SSH client cleanup only; remote process cleanup is unverified"
        );
    }
    let remote_process_scope = status.job.remote_process_scope.clone();
    Ok(ActRunShellCancelResponse {
        job_id: params.job_id.clone(),
        before_status,
        cancel_requested,
        termination_attempted,
        termination_status,
        remaining_process_ids,
        remote_process_scope,
        status,
    })
}

pub fn cleanup_shell_jobs_for_session(
    session_id: &str,
    reason: &str,
) -> Result<ShellSessionCleanupReadback, ErrorData> {
    validate_shell_session_id(session_id)?;
    // #1510: opportunistically reap stale terminal jobs on every session teardown
    // so a long-lived daemon's durable store stays bounded even when the process
    // never restarts. This runs before the live-job scan below, so once the
    // backlog is drained every subsequent enumeration is cheap again. Best-effort:
    // a reaper error must never abort this session's own cleanup — it is logged and
    // the next teardown (or a daemon restart) retries.
    if let Err(error) = reap_stale_shell_jobs() {
        tracing::warn!(
            code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP_REAP_FAILED",
            session_id,
            reason,
            detail = %error.message,
            "act_run_shell session cleanup could not run the stale-job reaper; continuing with live-job cleanup"
        );
    }
    let root = shell_durable_job_root_dir()?;
    if !root.exists() {
        return Ok(ShellSessionCleanupReadback {
            reason: reason.to_owned(),
            session_id: session_id.to_owned(),
            job_root: Some(path_string(&root)),
            status_files_read: 0,
            skipped_invalid_job_dirs: 0,
            skipped_unreadable_status_files: 0,
            skipped_foreign_jobs: 0,
            live_jobs_before: 0,
            retained_live_jobs: 0,
            reaped_phantom_jobs: 0,
            skipped_concurrently_mutated: 0,
            termination_attempted: 0,
            termination_succeeded: 0,
            failed: 0,
            job_ids: Vec::new(),
            remaining_process_ids: Vec::new(),
        });
    }

    let mut readback = ShellSessionCleanupReadback {
        reason: reason.to_owned(),
        session_id: session_id.to_owned(),
        job_root: Some(path_string(&root)),
        status_files_read: 0,
        skipped_invalid_job_dirs: 0,
        skipped_unreadable_status_files: 0,
        skipped_foreign_jobs: 0,
        live_jobs_before: 0,
        retained_live_jobs: 0,
        reaped_phantom_jobs: 0,
        skipped_concurrently_mutated: 0,
        termination_attempted: 0,
        termination_succeeded: 0,
        failed: 0,
        job_ids: Vec::new(),
        remaining_process_ids: Vec::new(),
    };
    let entries = fs::read_dir(&root).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell session cleanup failed to read shell job root: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "session_id": session_id,
                "path": root,
                "reason": "session_job_root_read_failed",
            }),
        )
    })?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A sibling job directory was removed by a concurrent session,
                // the reaper, or a parallel test between opening the directory
                // stream and yielding this entry. That is an expected outcome of
                // scanning a shared store, not a failure of this cleanup pass.
                readback.skipped_concurrently_mutated =
                    readback.skipped_concurrently_mutated.saturating_add(1);
                tracing::debug!(
                    code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP_DIR_ENTRY_VANISHED",
                    session_id,
                    reason,
                    error = %error,
                    "act_run_shell session cleanup skipped a job directory entry that vanished mid-scan"
                );
                continue;
            }
            Err(error) => {
                readback.failed = readback.failed.saturating_add(1);
                tracing::error!(
                    code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP_DIR_ENTRY_FAILED",
                    session_id,
                    reason,
                    error = %error,
                    "act_run_shell session cleanup could not read one job directory entry"
                );
                continue;
            }
        };
        let job_dir = entry.path();
        if !job_dir.is_dir() {
            continue;
        }
        let Some(job_id) = job_dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            readback.skipped_invalid_job_dirs = readback.skipped_invalid_job_dirs.saturating_add(1);
            tracing::warn!(
                code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP_JOB_DIR_NAME_INVALID",
                session_id,
                reason,
                path = %path_string(&job_dir),
                "act_run_shell session cleanup skipped a job directory with a non-utf8 name"
            );
            continue;
        };
        if validate_shell_job_id(&job_id).is_err() {
            readback.skipped_invalid_job_dirs = readback.skipped_invalid_job_dirs.saturating_add(1);
            tracing::warn!(
                code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP_JOB_ID_INVALID",
                session_id,
                reason,
                job_id,
                path = %path_string(&job_dir),
                "act_run_shell session cleanup skipped a job directory with an invalid job id"
            );
            continue;
        }
        let paths = shell_job_paths_from_root(&root, &job_id);
        let job = match read_shell_job_status(&paths.status_path, &job_id) {
            Ok(job) => job,
            Err(error) => {
                readback.skipped_unreadable_status_files =
                    readback.skipped_unreadable_status_files.saturating_add(1);
                tracing::warn!(
                    code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP_STATUS_READ_SKIPPED",
                    session_id,
                    reason,
                    job_id,
                    path = %path_string(&paths.status_path),
                    detail = %error.message,
                    "act_run_shell session cleanup skipped a durable job whose status was not readable yet"
                );
                continue;
            }
        };
        readback.status_files_read = readback.status_files_read.saturating_add(1);
        if job.session_id.as_deref() != Some(session_id) {
            readback.skipped_foreign_jobs = readback.skipped_foreign_jobs.saturating_add(1);
            continue;
        }
        if !shell_job_live_status(&job.status) {
            continue;
        }
        // #1334: liveness must be PID-backed, not status-string-only. A durable
        // job whose status still claims live but whose backing process is dead is
        // a phantom — reconcile it to a terminal state (persisting the fix so it
        // is cleaned product-wide) instead of retaining it as "running" forever.
        let claimed_live_before = job.status.clone();
        let job = match reconcile_shell_job_process_state(job, &paths) {
            Ok(job) => job,
            Err(error) => {
                readback.failed = readback.failed.saturating_add(1);
                tracing::warn!(
                    code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP_RECONCILE_FAILED",
                    session_id,
                    reason,
                    job_id,
                    detail = %error.message,
                    "act_run_shell session cleanup could not reconcile a durable job's process state"
                );
                continue;
            }
        };
        if !shell_job_process_still_running(&job) {
            if shell_job_live_status(&claimed_live_before) {
                readback.reaped_phantom_jobs = readback.reaped_phantom_jobs.saturating_add(1);
                tracing::info!(
                    code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP_PHANTOM_REAPED",
                    session_id,
                    reason,
                    job_id,
                    claimed_status = %claimed_live_before,
                    reconciled_status = %job.status,
                    "act_run_shell session cleanup reconciled a phantom durable job (live status, dead process) to terminal"
                );
            }
            continue;
        }
        readback.live_jobs_before = readback.live_jobs_before.saturating_add(1);
        readback.retained_live_jobs = readback.retained_live_jobs.saturating_add(1);
        readback.job_ids.push(job_id.clone());
    }
    readback.remaining_process_ids.sort_unstable();
    readback.remaining_process_ids.dedup();
    tracing::info!(
        code = "M4_ACT_RUN_SHELL_SESSION_CLEANUP",
        session_id,
        reason,
        job_root = ?readback.job_root,
        status_files_read = readback.status_files_read,
        skipped_invalid_job_dirs = readback.skipped_invalid_job_dirs,
        skipped_unreadable_status_files = readback.skipped_unreadable_status_files,
        skipped_foreign_jobs = readback.skipped_foreign_jobs,
        skipped_concurrently_mutated = readback.skipped_concurrently_mutated,
        live_jobs_before = readback.live_jobs_before,
        retained_live_jobs = readback.retained_live_jobs,
        termination_attempted = readback.termination_attempted,
        termination_succeeded = readback.termination_succeeded,
        failed = readback.failed,
        remaining_process_ids = ?readback.remaining_process_ids,
        "readback=act_run_shell_session_cleanup after=status_files_and_process_table"
    );
    Ok(readback)
}

/// Default retention for settled durable shell-job directories: 7 days. A job
/// whose backing process is no longer live (any status other than `running` or
/// `cancel_requested`) and whose completion is older than this is eligible for
/// reaping. Live jobs and recently-settled jobs are always retained so an
/// operator can still read a job they just finished. Override with the
/// `SYNAPSE_SHELL_JOB_TTL_SECS` environment variable.
const DEFAULT_SHELL_JOB_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

/// Cap on how many reaped job ids are echoed in the reap readback so draining a
/// huge backlog (the 856-dir accumulation in #1510) cannot emit an unbounded log
/// line. The `reaped_stale_jobs` count is always exact; only the id sample is
/// capped.
const SHELL_JOB_REAP_ID_SAMPLE_CAP: usize = 64;

/// Structured evidence of one durable shell-job retention pass. Every scanned
/// directory lands in exactly one bucket so the numbers are auditable:
/// `scanned_job_dirs == reaped_stale_jobs + retained_live_jobs
///   + retained_recent_terminal_jobs + skipped_unreadable_status_files
///   + reap_failures (+ any concurrently-vanished dirs)`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ShellJobReapReadback {
    pub job_root: Option<String>,
    pub retention_secs: u64,
    pub scanned_job_dirs: usize,
    pub reaped_stale_jobs: usize,
    pub retained_live_jobs: usize,
    pub retained_recent_terminal_jobs: usize,
    pub skipped_invalid_job_dirs: usize,
    pub skipped_unreadable_status_files: usize,
    pub skipped_concurrently_mutated: usize,
    pub reap_failures: usize,
    pub bytes_reclaimed: u64,
    pub reaped_job_ids_sample: Vec<String>,
}

/// Resolve the terminal-job retention TTL. Unset uses the 7-day default; a set
/// value is parsed as a positive integer number of seconds. A set-but-invalid
/// value (non-UTF-8, unparseable, or zero) is a misconfiguration and fails loudly
/// rather than silently disabling retention — mirroring `SYNAPSE_SHELL_JOB_ROOT`.
fn shell_job_retention_ttl() -> Result<Duration, ErrorData> {
    let Some(raw) = std::env::var_os("SYNAPSE_SHELL_JOB_TTL_SECS") else {
        return Ok(Duration::from_secs(DEFAULT_SHELL_JOB_RETENTION_SECS));
    };
    let text = raw.to_str().ok_or_else(|| {
        shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "SYNAPSE_SHELL_JOB_TTL_SECS must be valid UTF-8 (a positive integer number of seconds)",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "shell_job_ttl_not_utf8",
            }),
        )
    })?;
    let secs: u64 = text.trim().parse().map_err(|error| {
        shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "SYNAPSE_SHELL_JOB_TTL_SECS must be a positive integer number of seconds: {error}"
            ),
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "value": text,
                "reason": "shell_job_ttl_unparseable",
            }),
        )
    })?;
    if secs == 0 {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "SYNAPSE_SHELL_JOB_TTL_SECS must be greater than zero; unset it for the 7-day default",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "shell_job_ttl_zero",
            }),
        ));
    }
    Ok(Duration::from_secs(secs))
}

/// Age in milliseconds of a settled job, measured from its completion timestamp
/// (falling back to `started_at` if a record lacks `completed_at`). An
/// unparseable or future timestamp yields `None`, which the reaper treats as
/// age 0 (retain), so a clock skew can never cause a premature deletion.
fn shell_job_terminal_age_ms(job: &ActRunShellJobStatus) -> Option<u64> {
    let stamp = job
        .completed_at
        .as_deref()
        .unwrap_or(job.started_at.as_str());
    elapsed_ms_since_rfc3339(stamp)
}

/// Best-effort byte accounting for a job directory's known artifacts, summed
/// before removal so the readback can report reclaimed disk. Missing files
/// contribute zero rather than failing the pass.
fn shell_job_dir_bytes(paths: &ShellJobPaths) -> u64 {
    [
        &paths.status_path,
        &paths.stdout_path,
        &paths.stderr_path,
        &paths.request_path,
        &paths.remote_cleanup_path,
    ]
    .into_iter()
    .filter_map(|path| fs::metadata(path).ok())
    .map(|metadata| metadata.len())
    .fold(0u64, |acc, len| acc.saturating_add(len))
}

/// Reap stale settled durable shell-job directories using the configured TTL
/// (#1510). Returns structured evidence of every scanned directory's disposition.
/// Only jobs whose backing process is no longer live AND older than the TTL are
/// removed; anything whose status cannot be read, still claims a live process
/// (`running`/`cancel_requested`), or settled recently is retained. This is the
/// source-of-truth mutation for the retention policy and is invoked at daemon
/// startup and opportunistically during session cleanup.
pub fn reap_stale_shell_jobs() -> Result<ShellJobReapReadback, ErrorData> {
    let ttl = shell_job_retention_ttl()?;
    reap_stale_shell_jobs_with_ttl(ttl)
}

fn reap_stale_shell_jobs_with_ttl(ttl: Duration) -> Result<ShellJobReapReadback, ErrorData> {
    let root = shell_durable_job_root_dir()?;
    let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
    let mut readback = ShellJobReapReadback {
        job_root: Some(path_string(&root)),
        retention_secs: ttl.as_secs(),
        scanned_job_dirs: 0,
        reaped_stale_jobs: 0,
        retained_live_jobs: 0,
        retained_recent_terminal_jobs: 0,
        skipped_invalid_job_dirs: 0,
        skipped_unreadable_status_files: 0,
        skipped_concurrently_mutated: 0,
        reap_failures: 0,
        bytes_reclaimed: 0,
        reaped_job_ids_sample: Vec::new(),
    };
    if !root.exists() {
        return Ok(readback);
    }
    let entries = fs::read_dir(&root).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("shell job reaper failed to read shell job root: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": root,
                "reason": "reap_job_root_read_failed",
            }),
        )
    })?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A sibling job directory vanished mid-scan (concurrent session,
                // parallel test, or a prior reap). Expected on a shared store.
                readback.skipped_concurrently_mutated =
                    readback.skipped_concurrently_mutated.saturating_add(1);
                continue;
            }
            Err(error) => {
                readback.reap_failures = readback.reap_failures.saturating_add(1);
                tracing::error!(
                    code = "M4_SHELL_JOB_REAP_DIR_ENTRY_FAILED",
                    error = %error,
                    "shell job reaper could not read one job directory entry"
                );
                continue;
            }
        };
        let job_dir = entry.path();
        if !job_dir.is_dir() {
            continue;
        }
        let Some(job_id) = job_dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            readback.skipped_invalid_job_dirs = readback.skipped_invalid_job_dirs.saturating_add(1);
            continue;
        };
        if validate_shell_job_id(&job_id).is_err() {
            readback.skipped_invalid_job_dirs = readback.skipped_invalid_job_dirs.saturating_add(1);
            continue;
        }
        readback.scanned_job_dirs = readback.scanned_job_dirs.saturating_add(1);
        let paths = shell_job_paths_from_root(&root, &job_id);
        let job = match read_shell_job_status(&paths.status_path, &job_id) {
            Ok(job) => job,
            Err(_error) => {
                // Unreadable status = possibly a job mid-write or a corrupt record.
                // NEVER reap a job we cannot prove is terminal; a live job whose
                // status file is momentarily unreadable must survive.
                readback.skipped_unreadable_status_files =
                    readback.skipped_unreadable_status_files.saturating_add(1);
                continue;
            }
        };
        // Safety invariant: a job whose status still claims a live backing
        // process (`running`/`cancel_requested`) is retained unconditionally,
        // regardless of age — reaping it could orphan a running child's on-disk
        // record. Dead-PID phantoms in these states are reconciled to a terminal
        // status by session cleanup (#1334); a later reap then removes them once
        // aged. Everything else — terminal statuses AND a `finalizing` job that
        // has been stuck far past the millisecond-scale finalize window (observed
        // in the real store, #1510) — is a settled job eligible for age-based
        // reaping.
        if shell_job_live_status(&job.status) {
            readback.retained_live_jobs = readback.retained_live_jobs.saturating_add(1);
            continue;
        }
        let age_ms = shell_job_terminal_age_ms(&job).unwrap_or(0);
        if age_ms < ttl_ms {
            readback.retained_recent_terminal_jobs =
                readback.retained_recent_terminal_jobs.saturating_add(1);
            continue;
        }
        let bytes = shell_job_dir_bytes(&paths);
        match fs::remove_dir_all(&job_dir) {
            Ok(()) => {
                readback.reaped_stale_jobs = readback.reaped_stale_jobs.saturating_add(1);
                readback.bytes_reclaimed = readback.bytes_reclaimed.saturating_add(bytes);
                if readback.reaped_job_ids_sample.len() < SHELL_JOB_REAP_ID_SAMPLE_CAP {
                    readback.reaped_job_ids_sample.push(job_id.clone());
                }
                tracing::debug!(
                    code = "M4_SHELL_JOB_REAPED",
                    job_id,
                    status = %job.status,
                    age_ms,
                    bytes,
                    "reaped stale terminal durable shell job"
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                readback.skipped_concurrently_mutated =
                    readback.skipped_concurrently_mutated.saturating_add(1);
            }
            Err(error) => {
                readback.reap_failures = readback.reap_failures.saturating_add(1);
                tracing::error!(
                    code = "M4_SHELL_JOB_REAP_REMOVE_FAILED",
                    job_id,
                    path = %path_string(&job_dir),
                    error = %error,
                    "shell job reaper could not remove a stale terminal job directory"
                );
            }
        }
    }
    tracing::info!(
        code = "M4_SHELL_JOB_REAP",
        job_root = ?readback.job_root,
        retention_secs = readback.retention_secs,
        scanned_job_dirs = readback.scanned_job_dirs,
        reaped_stale_jobs = readback.reaped_stale_jobs,
        retained_live_jobs = readback.retained_live_jobs,
        retained_recent_terminal_jobs = readback.retained_recent_terminal_jobs,
        skipped_invalid_job_dirs = readback.skipped_invalid_job_dirs,
        skipped_unreadable_status_files = readback.skipped_unreadable_status_files,
        skipped_concurrently_mutated = readback.skipped_concurrently_mutated,
        reap_failures = readback.reap_failures,
        bytes_reclaimed = readback.bytes_reclaimed,
        "readback=shell_job_reap after=durable_job_root_scan"
    );
    Ok(readback)
}

/// Best-effort durable shell-job retention pass for daemon startup. Housekeeping
/// must never block or fail daemon boot, so a reaper error is logged and
/// swallowed; the next session teardown reaper retries. Called once from each
/// daemon entry point (stdio and HTTP) after the single-instance lock is held.
pub fn reap_stale_shell_jobs_on_startup() {
    match reap_stale_shell_jobs() {
        Ok(readback) => tracing::info!(
            code = "M4_SHELL_JOB_REAP_STARTUP",
            reaped_stale_jobs = readback.reaped_stale_jobs,
            scanned_job_dirs = readback.scanned_job_dirs,
            retained_live_jobs = readback.retained_live_jobs,
            retained_recent_terminal_jobs = readback.retained_recent_terminal_jobs,
            bytes_reclaimed = readback.bytes_reclaimed,
            "daemon startup reaped stale durable shell jobs"
        ),
        Err(error) => tracing::error!(
            code = "M4_SHELL_JOB_REAP_STARTUP_FAILED",
            detail = %error.message,
            "daemon startup shell-job reaper failed; will retry on next session cleanup"
        ),
    }
}

pub fn shell_jobs_dashboard_snapshot(
    max_jobs: usize,
) -> Result<ShellJobsDashboardSnapshot, ErrorData> {
    let root = shell_durable_job_root_dir()?;
    let source_of_truth = format!("durable shell status files under {}", path_string(&root));
    if !root.exists() {
        return Ok(ShellJobsDashboardSnapshot {
            source_of_truth,
            job_root: Some(path_string(&root)),
            max_jobs,
            job_count: 0,
            returned_count: 0,
            running_count: 0,
            terminal_count: 0,
            status_files_read: 0,
            skipped_invalid_job_dirs: 0,
            skipped_unreadable_status_files: 0,
            rows: Vec::new(),
        });
    }

    let entries = fs::read_dir(&root).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("dashboard shell job snapshot failed to read shell job root: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": root,
                "reason": "dashboard_shell_job_root_read_failed",
            }),
        )
    })?;

    let mut job_ids = Vec::new();
    let mut skipped_invalid_job_dirs = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_error) => {
                skipped_invalid_job_dirs = skipped_invalid_job_dirs.saturating_add(1);
                continue;
            }
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(job_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            skipped_invalid_job_dirs = skipped_invalid_job_dirs.saturating_add(1);
            continue;
        };
        if validate_shell_job_id(&job_id).is_err() {
            skipped_invalid_job_dirs = skipped_invalid_job_dirs.saturating_add(1);
            continue;
        }
        job_ids.push(job_id);
    }
    let job_count = job_ids.len();
    let mut candidates = Vec::new();
    let mut status_files_read = 0usize;
    let mut skipped_unreadable_status_files = 0usize;
    for job_id in job_ids {
        let paths = shell_job_paths_from_root(&root, &job_id);
        match read_shell_job_status(&paths.status_path, &job_id) {
            Ok(job) => {
                status_files_read = status_files_read.saturating_add(1);
                candidates.push((job_id, shell_job_dashboard_sort_key(&job)));
            }
            Err(_error) => {
                skipped_unreadable_status_files = skipped_unreadable_status_files.saturating_add(1);
            }
        }
    }
    candidates.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));

    let mut rows = Vec::new();
    let mut running_count = 0usize;
    let mut terminal_count = 0usize;
    for (job_id, _sort_key) in candidates.into_iter().take(max_jobs) {
        match shell_job_status(
            &ActRunShellStatusParams {
                job_id,
                tail_bytes: SHELL_JOB_DASHBOARD_TAIL_BYTES,
            },
            None,
        ) {
            Ok(status) => {
                if status.running {
                    running_count = running_count.saturating_add(1);
                }
                if shell_job_terminal_status(&status.job.status) {
                    terminal_count = terminal_count.saturating_add(1);
                }
                rows.push(status);
            }
            Err(_error) => {
                skipped_unreadable_status_files = skipped_unreadable_status_files.saturating_add(1);
            }
        }
    }

    Ok(ShellJobsDashboardSnapshot {
        source_of_truth,
        job_root: Some(path_string(&root)),
        max_jobs,
        job_count,
        returned_count: rows.len(),
        running_count,
        terminal_count,
        status_files_read,
        skipped_invalid_job_dirs,
        skipped_unreadable_status_files,
        rows,
    })
}

fn shell_job_dashboard_sort_key(job: &ActRunShellJobStatus) -> String {
    job.completed_at
        .as_deref()
        .unwrap_or(job.started_at.as_str())
        .to_owned()
}

pub fn run_shell_idempotency_row_key(
    params: &ActRunShellParams,
    session_id: Option<&str>,
) -> Result<Option<Vec<u8>>, ErrorData> {
    let Some(key) = &params.idempotency_key else {
        return Ok(None);
    };
    validate_run_shell_idempotency_key(key)?;
    if let Some(session_id) = session_id {
        validate_shell_session_id(session_id)?;
    }
    let owner = session_id
        .map(|session_id| format!("session/{}", sha256_hex(session_id.as_bytes())))
        .unwrap_or_else(|| "sessionless".to_owned());
    Ok(Some(
        format!(
            "{RUN_SHELL_IDEMPOTENCY_PREFIX}{owner}/{}",
            sha256_hex(key.as_bytes())
        )
        .into_bytes(),
    ))
}

pub fn run_shell_idempotency_reservation_row(
    params: &ActRunShellParams,
    authorization: &RunShellAuthorization,
    session_id: Option<&str>,
) -> Result<Vec<u8>, ErrorData> {
    let key_sha256 = params
        .idempotency_key
        .as_deref()
        .map(|key| sha256_hex(key.as_bytes()))
        .unwrap_or_default();
    let command_metadata = shell_command_metadata(&params.command, &params.args);
    let row = RunShellIdempotencyRow {
        schema_version: 2,
        tool: "act_run_shell".to_owned(),
        session_id: session_id.map(ToOwned::to_owned),
        idempotency_key_sha256: key_sha256,
        request_sha256: run_shell_request_sha256(params)?,
        status: "in_progress".to_owned(),
        command_line: command_metadata.command_line,
        command_line_sha256: command_metadata.command_line_sha256,
        command_line_redacted: command_metadata.command_line_redacted,
        matched_pattern: authorization.matched_pattern.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
        response: None,
    };
    encode_json(&row).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_run_shell idempotency reservation encode failed: {error}"),
        )
    })
}

pub fn run_shell_idempotency_completed_row(
    params: &ActRunShellParams,
    authorization: &RunShellAuthorization,
    response: &ActRunShellResponse,
    session_id: Option<&str>,
) -> Result<Vec<u8>, ErrorData> {
    let key_sha256 = params
        .idempotency_key
        .as_deref()
        .map(|key| sha256_hex(key.as_bytes()))
        .unwrap_or_default();
    let command_metadata = shell_command_metadata(&params.command, &params.args);
    let now = chrono::Utc::now().to_rfc3339();
    let row = RunShellIdempotencyRow {
        schema_version: 2,
        tool: "act_run_shell".to_owned(),
        session_id: session_id.map(ToOwned::to_owned),
        idempotency_key_sha256: key_sha256,
        request_sha256: run_shell_request_sha256(params)?,
        status: "ok".to_owned(),
        command_line: command_metadata.command_line,
        command_line_sha256: command_metadata.command_line_sha256,
        command_line_redacted: command_metadata.command_line_redacted,
        matched_pattern: authorization.matched_pattern.clone(),
        started_at: now.clone(),
        completed_at: Some(now),
        response: Some(response.clone()),
    };
    encode_json(&row).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_run_shell idempotency completion encode failed: {error}"),
        )
    })
}

pub fn run_shell_idempotency_replay(
    params: &ActRunShellParams,
    row_bytes: &[u8],
    session_id: Option<&str>,
) -> Result<ActRunShellResponse, ErrorData> {
    let row = decode_json::<RunShellIdempotencyRow>(row_bytes).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_run_shell idempotency row decode failed: {error}"),
        )
    })?;
    if row.schema_version != 2 || row.tool != "act_run_shell" {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell idempotency row has unexpected schema/tool",
        ));
    }
    if row.session_id.as_deref() != session_id {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell idempotency row session owner mismatch",
        ));
    }
    let expected_key_sha256 = params
        .idempotency_key
        .as_deref()
        .map(|key| sha256_hex(key.as_bytes()))
        .unwrap_or_default();
    if row.idempotency_key_sha256 != expected_key_sha256 {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell idempotency row key hash mismatch",
        ));
    }
    let request_sha256 = run_shell_request_sha256(params)?;
    if row.request_sha256 != request_sha256 {
        return Err(idempotency_error(
            "act_run_shell idempotency_key was already used for different parameters",
            "idempotency_key_conflict",
            &json!({
                "stored_request_sha256": row.request_sha256,
                "incoming_request_sha256": request_sha256,
            }),
        ));
    }
    match (row.status.as_str(), row.response) {
        ("ok", Some(response)) => {
            tracing::info!(
                code = "M4_ACT_RUN_SHELL_IDEMPOTENT_REPLAY",
                request_sha256 = %request_sha256,
                "readback=act_run_shell after=idempotent_replay"
            );
            Ok(response)
        }
        ("in_progress", _) => Err(idempotency_error(
            "act_run_shell idempotency_key is already in progress",
            "idempotency_in_progress",
            &json!({ "request_sha256": request_sha256 }),
        )),
        (status, _) => Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_run_shell idempotency row has unsupported status {status:?}"),
        )),
    }
}

fn run_shell_request_sha256(params: &ActRunShellParams) -> Result<String, ErrorData> {
    let payload = json!({
        "command": params.command,
        "args": params.args,
        "working_dir": params.working_dir,
        "env": params.env,
        "timeout_ms": params.timeout_ms,
    });
    let bytes = serde_json::to_vec(&payload).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_run_shell request fingerprint encode failed: {error}"),
        )
    })?;
    Ok(sha256_hex(&bytes))
}

fn launch_request_sha256(params: &ActLaunchParams) -> Result<String, ErrorData> {
    let payload = json!({
        "target": params.target,
        "args": params.args,
        "working_dir": params.working_dir,
        "env": params.env,
        "wait_for_window_title_regex": params.wait_for_window_title_regex,
        "timeout_ms": params.timeout_ms,
        "cdp_debug": params.cdp_debug,
        "force_renderer_accessibility": params.force_renderer_accessibility,
        "windows_console_window_state": params.windows_console_window_state,
        "desktop": params.desktop,
    });
    let bytes = serde_json::to_vec(&payload).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_launch request fingerprint encode failed: {error}"),
        )
    })?;
    Ok(sha256_hex(&bytes))
}

#[allow(
    dead_code,
    reason = "kept as the direct M4 launch helper for unit tests and non-server callers"
)]
pub async fn launch(
    config: &M4ServiceConfig,
    params: ActLaunchParams,
) -> Result<ActLaunchResponse, ErrorData> {
    Ok(launch_for_session(config, params, None).await?.response)
}

pub(crate) fn validate_launch_authorized(
    config: &M4ServiceConfig,
    params: &ActLaunchParams,
) -> Result<String, ErrorData> {
    validate_launch_params(params)?;
    let command_line = launch_command_line(params)?;
    if let Some(matched_pattern) = config.launch_match(&command_line) {
        return Ok(matched_pattern.to_owned());
    }
    let reason = if config.allow_launch_count() == 0 {
        "no_allow_launch_policy"
    } else {
        "launch_command_not_allowlisted"
    };
    Err(policy_error(
        error_codes::SAFETY_LAUNCH_DENIED_BY_POLICY,
        "act_launch target is not permitted by --allow-launch policy",
        json!({
            "code": error_codes::SAFETY_LAUNCH_DENIED_BY_POLICY,
            "target": params.target,
            "args": params.args,
            "command_line": command_line,
            "working_dir": params.working_dir,
            "env_keys": params.env.keys().cloned().collect::<Vec<_>>(),
            "timeout_ms": params.timeout_ms,
            "idempotency_key_present": params.idempotency_key.is_some(),
            "allow_launch_patterns": config.allow_launch_count(),
            "reason": reason,
        }),
    ))
}

pub(crate) async fn launch_for_session(
    config: &M4ServiceConfig,
    params: ActLaunchParams,
    session_id: Option<&str>,
) -> Result<ActLaunchOutcome, ErrorData> {
    let matched_pattern = validate_launch_authorized(config, &params)?;
    let command_line = launch_command_line(&params)?;
    let wait_regex = params
        .wait_for_window_title_regex
        .as_ref()
        .map(|pattern| {
            regex::Regex::new(pattern).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("act_launch wait_for_window_title_regex is invalid: {error}"),
                )
            })
        })
        .transpose()?;
    // #684: make a CDP debug port reachable for Synapse-launched Chromium so
    // observe/find can read the page DOM without manual flags. Augment the spawn
    // command only (policy already matched the original command above).
    let cdp_launch = chromium_cdp_launch(&params);
    let force_renderer_accessibility = chromium_renderer_accessibility_arg(&params);
    let spawn_params = params_with_chromium_launch_args(
        &params,
        cdp_launch.as_ref(),
        force_renderer_accessibility,
    );
    let launch_target_name = launch_target_effective_file_name(&params.target);
    let launch_desktop = prepare_launch_desktop(params.desktop.as_deref(), session_id)?;
    refuse_shared_tabbed_app_visible_reuse(&params, &launch_target_name, launch_desktop.as_ref())?;
    let excluded_hwnds = excluded_launch_wait_hwnds(wait_regex.as_ref(), launch_desktop.as_ref())?;
    let desktop_readback = launch_desktop
        .as_ref()
        .map(PreparedLaunchDesktop::to_response);
    let spawned = spawn_launch_child(&spawn_params, launch_desktop)?;
    let pid = spawned.pid;
    let cdp = if let Some(launch) = &cdp_launch {
        resolve_launched_cdp_port(pid, launch).await
    } else {
        LaunchedCdp::default()
    };
    let cdp_target =
        verify_launched_chromium_url(&params, cdp_launch.as_ref(), &cdp, params.timeout_ms).await?;
    let window = if let Some(regex) = wait_regex {
        if let Some(desktop_lease) = spawned.desktop_lease.as_ref() {
            wait_for_launch_desktop_window(
                pid,
                &regex,
                params.timeout_ms,
                &excluded_hwnds,
                &launch_target_name,
                &params.args,
                desktop_lease.name().to_owned(),
                desktop_lease.raw_handle_value(),
            )
            .await?
        } else {
            wait_for_launch_window(
                pid,
                &regex,
                params.timeout_ms,
                &excluded_hwnds,
                &launch_target_name,
                &params.args,
            )
            .await?
        }
    } else {
        WindowWaitResult::not_requested()
    };
    let launched_at = chrono::Utc::now().to_rfc3339();
    tracing::info!(
        code = "M4_ACT_LAUNCH_EXECUTED",
        command_line = %command_line,
        matched_pattern = %matched_pattern,
        pid,
        hwnd = ?window.hwnd,
        matched_title = ?window.matched_title,
        reason = ?window.reason,
        wait_requested = params.wait_for_window_title_regex.is_some(),
        idempotency_present = params.idempotency_key.is_some(),
        cdp_debug_port = ?cdp.port,
        desktop = ?desktop_readback,
        cdp_verified_url = ?cdp_target.as_ref().map(|target| target.url.as_str()),
        "readback=act_launch after=process_spawn"
    );
    // #1358: keep (hwnd, window_owner_pid) consistent and flag when act_launch
    // matched a window it did not freshly spawn (existing-window fallback / a
    // re-exec'd pid). `pid` stays the spawned process for back-compat.
    let window_owner_pid = window.matched_pid;
    let reused_existing_window = window.matched_pid.is_some_and(|owner| owner != pid);
    if reused_existing_window {
        tracing::warn!(
            code = "M4_ACT_LAUNCH_REUSED_EXISTING_WINDOW",
            launched_pid = pid,
            window_owner_pid = ?window_owner_pid,
            hwnd = ?window.hwnd,
            "act_launch matched a pre-existing/foreign window not owned by the spawned pid (#1358)"
        );
    }
    Ok(ActLaunchOutcome {
        response: ActLaunchResponse {
            pid,
            window_owner_pid,
            reused_existing_window,
            hwnd: window.hwnd,
            matched_title: window.matched_title,
            launched_at,
            reason: window.reason,
            cdp_debug_port: cdp.port,
            cdp_endpoint: cdp.endpoint,
            cdp_user_data_dir: cdp.user_data_dir,
            cdp_verified_url: cdp_target.as_ref().map(|target| target.url.clone()),
            cdp_verified_title: cdp_target.and_then(|target| target.title),
            desktop: desktop_readback,
        },
        desktop_lease: spawned.desktop_lease,
    })
}

#[derive(Debug)]
pub(crate) struct ActLaunchOutcome {
    pub response: ActLaunchResponse,
    pub desktop_lease: Option<LaunchDesktopLease>,
}

/// Planned CDP-debug augmentation for a Chromium-family launch (#684).
#[derive(Clone, Debug)]
struct ChromiumCdpLaunch {
    /// Dedicated automation profile dir (must be non-default for Chrome 136+).
    user_data_dir: std::path::PathBuf,
    /// Args injected ahead of the caller's args.
    injected_args: Vec<String>,
}

/// Optional Chromium renderer accessibility launch flag (#689).
///
/// Kept independent from CDP injection: callers may opt into the UIA renderer
/// tree even when they opt out of CDP, and CDP users may opt in to improve the
/// non-CDP fallback path.
fn chromium_renderer_accessibility_arg(params: &ActLaunchParams) -> Option<String> {
    if !force_renderer_accessibility_enabled(params) {
        return None;
    }
    if !synapse_a11y::is_chromium_family(&launch_target_effective_file_name(&params.target)) {
        return None;
    }
    let already_configured = params
        .args
        .iter()
        .any(|arg| is_force_renderer_accessibility_arg(arg));
    if already_configured {
        tracing::info!(
            code = "M4_ACT_LAUNCH_RENDERER_A11Y_SKIPPED",
            reason = "caller_supplied_force_renderer_accessibility",
            "act_launch leaving caller-specified renderer accessibility flag untouched"
        );
        return None;
    }
    Some("--force-renderer-accessibility".to_owned())
}

fn is_force_renderer_accessibility_arg(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("--force-renderer-accessibility") else {
        return false;
    };
    rest.is_empty() || rest.starts_with('=')
}

fn force_renderer_accessibility_enabled(params: &ActLaunchParams) -> bool {
    match params.force_renderer_accessibility {
        Some(value) => value,
        None => truthy_env("SYNAPSE_FORCE_RENDERER_ACCESSIBILITY"),
    }
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| truthy_value(&value))
        .unwrap_or(false)
}

fn truthy_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Outcome of opening + discovering a launched browser's CDP port.
#[derive(Clone, Debug, Default)]
struct LaunchedCdp {
    port: Option<u16>,
    endpoint: Option<String>,
    user_data_dir: Option<String>,
}

/// Decides whether to inject CDP debug flags for this launch. Returns `None` for
/// non-Chromium targets, when the caller opted out (`cdp_debug = Some(false)`),
/// or when the caller already specified a debug port / user-data-dir (respect
/// their intent). Otherwise plans an ephemeral port + dedicated profile.
fn chromium_cdp_launch(params: &ActLaunchParams) -> Option<ChromiumCdpLaunch> {
    if params.cdp_debug == Some(false) {
        return None;
    }
    let is_chromium =
        synapse_a11y::is_chromium_family(&launch_target_effective_file_name(&params.target));
    if !is_chromium && params.cdp_debug != Some(true) {
        return None;
    }
    let already_configured = params.args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        lower.starts_with("--remote-debugging-port") || lower.starts_with("--user-data-dir")
    });
    if already_configured {
        tracing::info!(
            code = "M4_ACT_LAUNCH_CDP_SKIPPED",
            reason = "caller_supplied_debug_or_profile_flags",
            "act_launch leaving caller-specified CDP/profile flags untouched"
        );
        return None;
    }
    let user_data_dir = cdp_automation_profile_dir();
    let injected_args = vec![
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", user_data_dir.display()),
        "--silent-debugger-extension-api".to_owned(),
        "--disable-extensions".to_owned(),
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
    ];
    Some(ChromiumCdpLaunch {
        user_data_dir,
        injected_args,
    })
}

/// A fresh ActLaunchParams whose injected browser args precede the caller's
/// args (so a positional URL still parses).
fn params_with_chromium_launch_args(
    params: &ActLaunchParams,
    cdp_launch: Option<&ChromiumCdpLaunch>,
    force_renderer_accessibility: Option<String>,
) -> ActLaunchParams {
    let mut spawn_params = params.clone();
    let mut args = cdp_launch
        .map(|launch| launch.injected_args.clone())
        .unwrap_or_default();
    if let Some(arg) = force_renderer_accessibility {
        args.push(arg);
    }
    args.extend(params.args.iter().cloned());
    spawn_params.args = args;
    spawn_params
}

/// Dedicated, non-default automation profile dir. Honors
/// `SYNAPSE_CDP_USER_DATA_DIR` for a stable (login-persisting) profile; otherwise
/// a unique per-launch dir under the OS temp so concurrent browsers never share
/// a profile (Chrome refuses a second debug port on the same profile).
fn cdp_automation_profile_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("SYNAPSE_CDP_USER_DATA_DIR") {
        let dir = std::path::PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let token = format!("{}-{seq}-{nanos:x}", std::process::id());
    std::env::temp_dir()
        .join("synapse-cdp-profiles")
        .join(token)
}

/// Polls the launched browser's `DevToolsActivePort` file (Chrome writes the
/// chosen ephemeral port there) and registers it so observe/find can attach.
/// Fail-loud: logs an error if the port never appears, but does not orphan the
/// already-spawned browser.
async fn resolve_launched_cdp_port(pid: u32, launch: &ChromiumCdpLaunch) -> LaunchedCdp {
    let port_file = launch.user_data_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_secs(15);
    let user_data_dir = Some(launch.user_data_dir.display().to_string());
    loop {
        if let Some(port) = read_devtools_active_port(&port_file) {
            synapse_a11y::register_launched_port(pid, port);
            tracing::info!(
                code = "M4_ACT_LAUNCH_CDP_PORT_OPENED",
                pid,
                port,
                user_data_dir = ?launch.user_data_dir,
                "act_launch opened a CDP debug port for the launched browser"
            );
            return LaunchedCdp {
                port: Some(port),
                endpoint: Some(format!("http://127.0.0.1:{port}")),
                user_data_dir,
            };
        }
        if Instant::now() >= deadline {
            tracing::error!(
                code = error_codes::A11Y_CDP_ATTACH_FAILED,
                pid,
                user_data_dir = ?launch.user_data_dir,
                "act_launch injected CDP flags but DevToolsActivePort never appeared; \
                 the browser launched but its DOM will not be observable via CDP"
            );
            return LaunchedCdp {
                port: None,
                endpoint: None,
                user_data_dir,
            };
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Reads the first line of a `DevToolsActivePort` file as a port number.
fn read_devtools_active_port(path: &Path) -> Option<u16> {
    let contents = std::fs::read_to_string(path).ok()?;
    contents.lines().next()?.trim().parse::<u16>().ok()
}

#[derive(Clone, Debug)]
struct VerifiedCdpTarget {
    url: String,
    title: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct CdpTargetListEntry {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "type")]
    target_type: Option<String>,
}

async fn verify_launched_chromium_url(
    params: &ActLaunchParams,
    cdp_launch: Option<&ChromiumCdpLaunch>,
    cdp: &LaunchedCdp,
    timeout_ms: u64,
) -> Result<Option<VerifiedCdpTarget>, ErrorData> {
    let Some(expected_url) = launch_requested_url(&params.args) else {
        return Ok(None);
    };
    if cdp_launch.is_none() {
        return Ok(None);
    }
    let Some(endpoint) = cdp.endpoint.as_deref() else {
        return Err(launch_url_verification_error(
            "cdp_endpoint_missing",
            &expected_url,
            None,
            timeout_ms,
            None,
            &[],
        ));
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(750))
        .build()
        .map_err(|error| {
            launch_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("act_launch failed to build CDP verification client: {error}"),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "reason": "cdp_verification_client_build_failed",
                    "expected_url": expected_url,
                    "endpoint": endpoint,
                }),
            )
        })?;
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let mut last_error: Option<String>;
    let mut last_targets = Vec::new();
    loop {
        match fetch_cdp_target_list(&client, endpoint).await {
            Ok(targets) => {
                last_targets = target_summaries(&targets);
                if let Some(target) = targets.iter().find(|target| {
                    target
                        .target_type
                        .as_deref()
                        .is_none_or(|kind| kind == "page")
                        && url_matches(&expected_url, &target.url)
                }) {
                    tracing::info!(
                        code = "M4_ACT_LAUNCH_CDP_URL_VERIFIED",
                        endpoint,
                        expected_url,
                        actual_url = %target.url,
                        title = ?target.title,
                        "act_launch verified requested browser URL in CDP target list"
                    );
                    return Ok(Some(VerifiedCdpTarget {
                        url: target.url.clone(),
                        title: target.title.clone().filter(|title| !title.is_empty()),
                    }));
                }
                last_error = None;
            }
            Err(error) => {
                last_error = Some(error);
            }
        }

        if started.elapsed() >= timeout {
            return Err(launch_url_verification_error(
                "url_not_observed_within_timeout",
                &expected_url,
                Some(endpoint),
                timeout_ms,
                last_error,
                &last_targets,
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn fetch_cdp_target_list(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<Vec<CdpTargetListEntry>, String> {
    let url = format!("{}/json/list", endpoint.trim_end_matches('/'));
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("GET {url}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("GET {url}: HTTP {status}"));
    }
    response
        .json::<Vec<CdpTargetListEntry>>()
        .await
        .map_err(|error| format!("decode {url}: {error}"))
}

fn launch_requested_url(args: &[String]) -> Option<String> {
    args.iter().find_map(|arg| {
        if let Some(value) = arg.strip_prefix("--app=")
            && supported_launch_url(value)
        {
            return Some(value.to_owned());
        }
        if arg.starts_with("--") {
            return None;
        }
        supported_launch_url(arg).then(|| arg.to_owned())
    })
}

fn supported_launch_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "file"))
}

fn url_matches(expected: &str, actual: &str) -> bool {
    canonical_launch_url(expected)
        .zip(canonical_launch_url(actual))
        .is_some_and(|(expected, actual)| expected == actual)
}

fn canonical_launch_url(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "file") {
        return None;
    }
    let mut canonical = url.to_string();
    if url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        canonical = canonical.trim_end_matches('/').to_owned();
    }
    Some(canonical)
}

fn target_summaries(targets: &[CdpTargetListEntry]) -> Vec<serde_json::Value> {
    targets
        .iter()
        .take(5)
        .map(|target| {
            json!({
                "type": target.target_type.as_deref(),
                "title": target.title.as_deref(),
                "url": target.url.as_str(),
            })
        })
        .collect()
}

fn launch_url_verification_error(
    reason: &'static str,
    expected_url: &str,
    endpoint: Option<&str>,
    timeout_ms: u64,
    last_error: Option<String>,
    observed_targets: &[serde_json::Value],
) -> ErrorData {
    launch_tool_error(
        error_codes::ACTION_LAUNCH_URL_NOT_REACHED,
        format!("act_launch did not verify requested browser URL: {reason}"),
        json!({
            "code": error_codes::ACTION_LAUNCH_URL_NOT_REACHED,
            "reason": reason,
            "expected_url": expected_url,
            "endpoint": endpoint,
            "timeout_ms": timeout_ms,
            "last_error": last_error,
            "observed_targets": observed_targets,
        }),
    )
}

fn validate_combo_params(params: &ActComboParams) -> Result<(), ErrorData> {
    if params.steps.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_combo steps must contain at least one step",
        ));
    }
    if params.steps.len() > MAX_COMBO_STEPS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_combo steps length {} exceeds max {MAX_COMBO_STEPS}",
                params.steps.len()
            ),
        ));
    }
    let mut previous = 0;
    for (index, step) in params.steps.iter().enumerate() {
        if index > 0 && step.at_ms < previous {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("act_combo steps[{index}].at_ms must be monotonic"),
            ));
        }
        if let Some(step_backend) = step.backend {
            ensure_combo_step_backend_matches(index, "backend", step_backend, params.backend)?;
        }
        previous = step.at_ms;
    }
    Ok(())
}

fn combo_steps_from_params(params: &ActComboParams) -> Result<Vec<ComboStep>, ErrorData> {
    let mut out = Vec::new();
    for (index, step) in params.steps.iter().enumerate() {
        match step.action {
            ActComboAction::ActPress => {
                let press: ActPressParams =
                    serde_json::from_value(step.params.clone()).map_err(|error| {
                        mcp_error(
                            error_codes::TOOL_PARAMS_INVALID,
                            format!("act_combo steps[{index}].act_press params invalid: {error}"),
                        )
                    })?;
                push_press_combo_steps(&mut out, index, step.at_ms, &press, params.backend)?;
            }
            ActComboAction::Retired(ref action) => {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    retired_combo_action_message(index, action),
                ));
            }
        }
    }
    Ok(out)
}

fn retired_combo_action_message(index: usize, action: &str) -> String {
    match action {
        "act_aim" | "act_drag" | "act_stroke" | "mouse_move" | "MouseMove" => format!(
            "act_combo steps[{index}].action {action:?} is not combo-lowerable; act_combo is intentionally limited to timed act_press key steps. Use act_stroke for continuous mouse motion/path execution."
        ),
        _ => format!(
            "act_combo steps[{index}].action {action:?} is not combo-lowerable; supported action: act_press"
        ),
    }
}

fn push_press_combo_steps(
    out: &mut Vec<ComboStep>,
    index: usize,
    at_ms: u32,
    params: &ActPressParams,
    combo_backend: Backend,
) -> Result<(), ErrorData> {
    match action_from_press_params(params)? {
        Action::KeyPress {
            key,
            hold_ms,
            backend,
        } => {
            ensure_combo_step_backend_matches(
                index,
                "act_press params.backend",
                backend,
                combo_backend,
            )?;
            let hold_ms = u16::try_from(hold_ms).map_err(|_err| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("act_combo steps[{index}].act_press hold_ms exceeds u16::MAX"),
                )
            })?;
            out.push(ComboStep {
                at_ms,
                input: ComboInput::KeyPress { key, hold_ms },
            });
        }
        Action::KeyChord {
            keys,
            hold_ms,
            backend,
        } => {
            ensure_combo_step_backend_matches(
                index,
                "act_press params.backend",
                backend,
                combo_backend,
            )?;
            push_key_chord_combo_steps(out, at_ms, keys, hold_ms);
        }
        other => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("act_combo steps[{index}].act_press lowered to unsupported {other:?}"),
            ));
        }
    }
    Ok(())
}

fn ensure_combo_step_backend_matches(
    index: usize,
    field: &'static str,
    requested: Backend,
    combo_backend: Backend,
) -> Result<(), ErrorData> {
    if requested == Backend::Auto || requested == combo_backend {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!(
            "act_combo steps[{index}].{field} differs from top-level backend; per-step backend routing is not lowerable yet"
        ),
    ))
}

fn push_key_chord_combo_steps(out: &mut Vec<ComboStep>, at_ms: u32, keys: Vec<Key>, hold_ms: u32) {
    for key in &keys {
        out.push(ComboStep {
            at_ms,
            input: ComboInput::KeyDown { key: key.clone() },
        });
    }
    let release_at_ms = at_ms.saturating_add(hold_ms);
    for key in keys.into_iter().rev() {
        out.push(ComboStep {
            at_ms: release_at_ms,
            input: ComboInput::KeyUp { key },
        });
    }
}

fn validate_run_shell_params(params: &ActRunShellParams) -> Result<(), ErrorData> {
    let command = params.command.trim();
    if command.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell command must not be empty",
        ));
    }
    validate_run_shell_environment(&params.env)?;
    validate_run_shell_command_shape(params, command)?;
    validate_run_shell_chromium_debug_policy(params)?;
    let command_line = shell_command_line(params);
    if let Some(marker) = detect_shell_global_input(&command_line) {
        return Err(shell_tool_error(
            error_codes::SAFETY_SHELL_GLOBAL_INPUT_DENIED,
            "act_run_shell command performs global OS keyboard/mouse/foreground input, which bypasses the foreground input lease and lands on the human operator's foreground window; use Synapse's lease-gated action primitives or a background target-specific tool instead of injecting input through a shell",
            json!({
                "code": error_codes::SAFETY_SHELL_GLOBAL_INPUT_DENIED,
                "matched_marker": marker,
                "reason": "global_input_via_shell_denied",
                "command": params.command,
                "remediation": "use lease-gated act_press/act_type/act_stroke for input or a target-specific background browser tool for tab/window selection",
            }),
        ));
    }
    if let Some(variable) = detect_shell_reserved_variable_assignment(&command_line) {
        return Err(shell_tool_error(
            error_codes::SAFETY_SHELL_RESERVED_VARIABLE_COLLISION,
            &format!(
                "act_run_shell assigns to the PowerShell automatic/read-only variable ${variable}; PowerShell variable names are case-insensitive, so this collides with the built-in ${} and the assignment silently fails while later uses keep the built-in value (this is how #1507 targeted the operator home directory). Choose a non-reserved variable name.",
                variable.to_ascii_uppercase()
            ),
            json!({
                "code": error_codes::SAFETY_SHELL_RESERVED_VARIABLE_COLLISION,
                "reserved_variable": variable,
                "reason": "reserved_powershell_variable_assignment",
                "reserved_variables": SHELL_RESERVED_PS_VARIABLES,
                "remediation": format!(
                    "rename the variable (e.g. $calyx_home instead of ${variable}); do not assign to PowerShell automatic variables"
                ),
            }),
        ));
    }
    if let Some(reference) = detect_uncontained_recursive_delete(&command_line) {
        let resolved = resolve_uncontained_path_reference(reference);
        return Err(shell_tool_error(
            error_codes::SAFETY_SHELL_RECURSIVE_DELETE_UNCONTAINED,
            &format!(
                "act_run_shell performs a recursive delete/move whose target references {reference}, which resolves to {} — a path outside the shell job working directory that Synapse cannot prove is contained. Refusing rather than run an unbounded recursive delete against an operator/home/tooling path (#1507). Target an explicit absolute path inside the workspace instead.",
                resolved
                    .as_deref()
                    .unwrap_or("an operator/home/system path")
            ),
            json!({
                "code": error_codes::SAFETY_SHELL_RECURSIVE_DELETE_UNCONTAINED,
                "path_reference": reference,
                "resolved_target": resolved,
                "working_dir": params.working_dir,
                "reason": "recursive_delete_target_not_workspace_contained",
                "remediation": "pass an explicit absolute path under the working_dir; do not delete paths derived from $HOME/$env:USERPROFILE/$PROFILE/system roots",
            }),
        ));
    }
    if params.timeout_ms == 0 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell timeout_ms must be >= 1",
        ));
    }
    if matches!(params.durable_timeout_ms, Some(0)) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell durable_timeout_ms must be >= 1 when provided",
        ));
    }
    if let Some(key) = &params.idempotency_key {
        validate_run_shell_idempotency_key(key)?;
    }
    Ok(())
}

fn validate_run_shell_environment(env: &BTreeMap<String, String>) -> Result<(), ErrorData> {
    for (key, value) in env {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(shell_tool_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_run_shell env entries must have non-empty keys without '=' or NUL and values without NUL",
                json!({
                    "code": error_codes::TOOL_PARAMS_INVALID,
                    "env_key": key,
                    "reason": "env_entry_invalid",
                }),
            ));
        }
        if SHELL_RESERVED_ENV_KEYS
            .iter()
            .any(|reserved| key.eq_ignore_ascii_case(reserved))
        {
            return Err(shell_tool_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_run_shell env cannot override Synapse session isolation variables",
                json!({
                    "code": error_codes::TOOL_PARAMS_INVALID,
                    "env_key": key,
                    "reserved_env_keys": SHELL_RESERVED_ENV_KEYS,
                    "reason": "reserved_session_env_key",
                }),
            ));
        }
    }
    Ok(())
}

fn validate_run_shell_command_shape(
    params: &ActRunShellParams,
    command: &str,
) -> Result<(), ErrorData> {
    let command_metadata = shell_command_metadata(&params.command, &params.args);
    if command != params.command {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell command must not include leading or trailing whitespace",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "command": params.command,
                "trimmed_command": command,
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": command_metadata.args,
                "args_redacted": command_metadata.args_redacted,
                "args_sha256": command_metadata.args_sha256,
                "working_dir": params.working_dir,
                "reason": "command_has_outer_whitespace",
            }),
        ));
    }

    if is_wrapped_in_quotes(command) {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell command must be an unquoted executable path/name; pass arguments separately in args",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "command": params.command,
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": command_metadata.args,
                "args_redacted": command_metadata.args_redacted,
                "args_sha256": command_metadata.args_sha256,
                "working_dir": params.working_dir,
                "reason": "command_must_not_be_quoted",
                "expected_shape_windows": {
                    "command": r"C:\Program Files\PowerShell\7\pwsh.exe",
                    "args": ["-NoProfile", "-Command", "Write-Output ok"],
                },
            }),
        ));
    }
    if starts_with_unclosed_quote(command) {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell command has an opening quote without a closing quote; pass the unquoted executable path/name in command",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "command": params.command,
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": command_metadata.args,
                "args_redacted": command_metadata.args_redacted,
                "args_sha256": command_metadata.args_sha256,
                "working_dir": params.working_dir,
                "reason": "command_has_unbalanced_quote",
            }),
        ));
    }

    let Some(first_token) = first_command_token(command) else {
        return Ok(());
    };
    if first_token == command || command_exists_verbatim(command, params.working_dir.as_deref()) {
        return Ok(());
    }

    Err(shell_tool_error(
        error_codes::TOOL_PARAMS_INVALID,
        "act_run_shell command must be an executable path/name only; pass flags and shell snippets in args",
        json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "command": params.command,
            "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
            "args": command_metadata.args,
            "args_redacted": command_metadata.args_redacted,
            "args_sha256": command_metadata.args_sha256,
            "working_dir": params.working_dir,
            "reason": "command_contains_arguments",
            "detected_executable_token": first_token,
            "expected_shape_windows_powershell": {
                "command": "powershell.exe",
                "args": ["-NoProfile", "-Command", "Write-Output ok"],
            },
            "expected_shape_windows_cmd": {
                "command": "cmd.exe",
                "args": ["/d", "/c", "echo ok"],
            },
        }),
    ))
}

fn is_wrapped_in_quotes(command: &str) -> bool {
    command.len() >= 2 && command.starts_with('"') && command.ends_with('"')
}

fn starts_with_unclosed_quote(command: &str) -> bool {
    command.starts_with('"') && !command[1..].contains('"')
}

fn command_exists_verbatim(command: &str, working_dir: Option<&str>) -> bool {
    let path = Path::new(command);
    if path.is_file() {
        return true;
    }
    if path.is_relative() {
        if let Some(working_dir) = working_dir {
            return Path::new(working_dir).join(path).is_file();
        }
    }
    false
}

fn first_command_token(command: &str) -> Option<&str> {
    if command.starts_with('"') {
        let closing_quote = command[1..].find('"')? + 1;
        return Some(&command[..=closing_quote]);
    }
    command
        .char_indices()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(&command[..index]))
}

fn validate_run_shell_idempotency_key(key: &str) -> Result<(), ErrorData> {
    if key.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell idempotency_key must not be empty",
        ));
    }
    if key.len() > MAX_SHELL_IDEMPOTENCY_KEY_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_run_shell idempotency_key must be <= {MAX_SHELL_IDEMPOTENCY_KEY_BYTES} bytes"
            ),
        ));
    }
    Ok(())
}

fn validate_launch_params(params: &ActLaunchParams) -> Result<(), ErrorData> {
    if params.target.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_launch target must not be empty",
        ));
    }
    if params.timeout_ms == 0 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_launch timeout_ms must be >= 1",
        ));
    }
    if let Some(pattern) = &params.wait_for_window_title_regex {
        regex::Regex::new(pattern).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("act_launch wait_for_window_title_regex is invalid: {error}"),
            )
        })?;
    }
    validate_console_launch_visibility(params)?;
    validate_launch_desktop_option(params)?;
    validate_shared_tabbed_desktop_launch_target(params)?;
    validate_chromium_debug_launch_policy(params)?;
    Ok(())
}

fn validate_launch_desktop_option(params: &ActLaunchParams) -> Result<(), ErrorData> {
    let Some(desktop) = params.desktop.as_deref() else {
        return Ok(());
    };
    validate_launch_desktop_request(desktop)
}

fn validate_shared_tabbed_desktop_launch_target(params: &ActLaunchParams) -> Result<(), ErrorData> {
    let Some(desktop) = params.desktop.as_deref() else {
        return Ok(());
    };
    let launch_target_name = launch_target_effective_file_name(&params.target);
    let Some(risk_family) = shared_tabbed_app_family(&launch_target_name) else {
        return Ok(());
    };

    #[cfg(not(windows))]
    {
        let _ = desktop;
        let _ = risk_family;
        return Ok(());
    }

    #[cfg(windows)]
    {
        if launch_target_is_absolute_windows_path(&params.target) {
            return Ok(());
        }

        Err(launch_tool_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "act_launch refused {launch_target_name} on desktop route because non-absolute shared-tabbed app targets can resolve through Windows process search, relative paths, or aliases after policy decisions"
            ),
            json!({
                "code": error_codes::ACTION_TARGET_INVALID,
                "reason": "shared_tabbed_app_desktop_requires_explicit_path",
                "target": params.target,
                "args": params.args,
                "desktop": desktop,
                "launch_target_name": launch_target_name,
                "risk_family": risk_family,
                "required_invariant": "desktop-routed shared-tabbed app launches must use an absolute executable path so Synapse policy and Windows process creation agree on the executable identity before spawn",
                "resolution": "use an absolute executable path such as C:\\Windows\\notepad.exe for hidden desktop launches; aliases or relative paths like notepad, notepad.exe, or .\\notepad.exe are not isolation-safe",
            }),
        ))
    }
}

fn validate_launch_desktop_request(desktop: &str) -> Result<(), ErrorData> {
    let trimmed = desktop.trim();
    if trimmed.is_empty() || trimmed != desktop {
        return Err(launch_desktop_params_error(
            "act_launch desktop must not be empty or padded with whitespace",
            desktop,
            "desktop_empty_or_padded",
        ));
    }
    if desktop.len() > 512 {
        return Err(launch_desktop_params_error(
            "act_launch desktop must be <= 512 bytes",
            desktop,
            "desktop_too_long",
        ));
    }
    if let Some(rest) = desktop.strip_prefix("agent:") {
        if rest.is_empty() {
            return Err(launch_desktop_params_error(
                "act_launch desktop agent scope must be agent:session or agent:<current-session-id>",
                desktop,
                "desktop_agent_scope_empty",
            ));
        }
        validate_desktop_leaf_name(rest, desktop, "desktop_agent_scope_invalid")?;
        return Ok(());
    }
    if let Some(rest) = desktop.strip_prefix("existing:") {
        validate_desktop_leaf_name(rest, desktop, "desktop_existing_name_invalid")?;
        return Ok(());
    }
    Err(launch_desktop_params_error(
        "act_launch desktop must be agent:session, agent:<current-session-id>, or existing:<desktop-name>",
        desktop,
        "desktop_scope_unsupported",
    ))
}

fn validate_desktop_leaf_name(
    name: &str,
    requested: &str,
    reason: &'static str,
) -> Result<(), ErrorData> {
    if name.is_empty() || name.len() > 128 {
        return Err(launch_desktop_params_error(
            "act_launch desktop leaf name must be 1..=128 bytes",
            requested,
            reason,
        ));
    }
    if name
        .chars()
        .any(|ch| ch == '\\' || ch == '\0' || ch.is_control())
    {
        return Err(launch_desktop_params_error(
            "act_launch desktop leaf name must not contain backslash, NUL, or control characters",
            requested,
            reason,
        ));
    }
    Ok(())
}

fn launch_desktop_params_error(
    message: &'static str,
    requested: &str,
    reason: &'static str,
) -> ErrorData {
    launch_tool_error(
        error_codes::TOOL_PARAMS_INVALID,
        message,
        json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "reason": reason,
            "desktop": requested,
        }),
    )
}

fn validate_chromium_debug_launch_policy(params: &ActLaunchParams) -> Result<(), ErrorData> {
    let is_chromium =
        synapse_a11y::is_chromium_family(&launch_target_effective_file_name(&params.target));
    if !is_chromium && params.cdp_debug != Some(true) {
        return Ok(());
    }
    let Some(violation) = chromium_debug_args_policy_violation(
        &params.args,
        "chromium_remote_debugging_not_popup_safe",
    ) else {
        return Ok(());
    };

    Err(launch_tool_error(
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
        "act_launch refused a Chromium remote-debugging launch that could surface Chrome debugger/native-host UI on an end-user profile",
        json!({
            "code": error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
            "reason": violation.reason,
            "target": params.target,
            "args": params.args,
            "user_data_dir": violation.user_data_dir,
            "user_data_dir_state": violation.user_data_dir_state.as_str(),
            "has_silent_debugger_extension_api": violation.silent_debugger,
            "has_disable_extensions": violation.disable_extensions,
            "has_extension_loading_flags": violation.loads_extensions,
            "has_layout_shifting_infobar_flags": !violation.layout_infobar_flags.is_empty(),
            "layout_shifting_infobar_flags": violation.layout_infobar_flags,
            "required_invariant": CHROMIUM_DEBUG_LAUNCH_REQUIRED_INVARIANT,
            "remediation": "omit caller-supplied remote-debugging/profile flags so Synapse injects its isolated automation profile, or pass the required flags against a non-default automation profile; never debug the user's normal Chrome profile",
        }),
    ))
}

const CHROMIUM_DEBUG_LAUNCH_REQUIRED_INVARIANT: &str = "remote-debugging Chromium launches must use a non-default dedicated user-data-dir, --silent-debugger-extension-api, --disable-extensions, no extension-loading flags, and no known layout-shifting Chrome warning flags such as --disable-blink-features=AutomationControlled";

#[derive(Debug)]
struct ChromiumDebugPolicyViolation {
    reason: &'static str,
    user_data_dir: Option<String>,
    user_data_dir_state: ChromiumUserDataDirSafety,
    silent_debugger: bool,
    disable_extensions: bool,
    loads_extensions: bool,
    layout_infobar_flags: Vec<String>,
}

fn chromium_debug_args_policy_violation(
    args: &[String],
    reason: &'static str,
) -> Option<ChromiumDebugPolicyViolation> {
    if !has_remote_debugging_arg(args) {
        return None;
    }

    let user_data_dir = user_data_dir_arg(args);
    let user_data_dir_state = user_data_dir
        .as_deref()
        .map(chromium_user_data_dir_safety)
        .unwrap_or(ChromiumUserDataDirSafety::Missing);
    let silent_debugger = args
        .iter()
        .any(|arg| is_switch_arg(arg, "--silent-debugger-extension-api"));
    let disable_extensions = args
        .iter()
        .any(|arg| is_switch_arg(arg, "--disable-extensions"));
    let loads_extensions = args.iter().any(|arg| {
        is_switch_arg(arg, "--load-extension") || is_switch_arg(arg, "--disable-extensions-except")
    });
    let layout_infobar_flags = chromium_layout_infobar_flags(args);

    if silent_debugger
        && disable_extensions
        && !loads_extensions
        && layout_infobar_flags.is_empty()
        && matches!(user_data_dir_state, ChromiumUserDataDirSafety::Dedicated)
    {
        return None;
    }

    Some(ChromiumDebugPolicyViolation {
        reason,
        user_data_dir,
        user_data_dir_state,
        silent_debugger,
        disable_extensions,
        loads_extensions,
        layout_infobar_flags,
    })
}

fn validate_run_shell_chromium_debug_policy(params: &ActRunShellParams) -> Result<(), ErrorData> {
    let command_name = launch_target_file_name(&params.command);
    if synapse_a11y::is_chromium_family(&command_name)
        && let Some(violation) = chromium_debug_args_policy_violation(
            &params.args,
            "chromium_remote_debugging_not_popup_safe",
        )
    {
        return Err(run_shell_chromium_debug_error(
            params,
            "act_run_shell refused a direct Chromium remote-debugging launch that could surface Chrome debugger/native-host UI or a layout-shifting automation banner",
            violation.reason,
            Some(violation),
            Vec::new(),
        ));
    }

    let command_line = shell_command_line(params);
    if let Some(violation) =
        shell_wrapped_chromium_debug_policy_violation(&command_name, &command_line)
    {
        return Err(run_shell_chromium_debug_error(
            params,
            "act_run_shell refused a shell-wrapped Chromium/Playwright launch that could surface Chrome debugger/native-host UI or a layout-shifting automation banner",
            violation.reason,
            None,
            violation.markers,
        ));
    }

    Ok(())
}

#[derive(Debug)]
struct ShellChromiumDebugPolicyViolation {
    reason: &'static str,
    markers: Vec<&'static str>,
}

fn shell_wrapped_chromium_debug_policy_violation(
    command_name: &str,
    command_line: &str,
) -> Option<ShellChromiumDebugPolicyViolation> {
    let lower = command_line.to_ascii_lowercase();
    let launcher_line = lower.replace(['"', '\''], "").replace('\\', "/");
    let known_playwright_mcp_launcher = launcher_line.contains("npx @playwright/mcp")
        || launcher_line.contains("npx.cmd @playwright/mcp")
        || launcher_line.contains("npx.exe @playwright/mcp")
        || launcher_line.contains("npm exec @playwright/mcp")
        || launcher_line.contains("pnpm dlx @playwright/mcp")
        || launcher_line.contains("yarn dlx @playwright/mcp")
        || (matches!(
            command_name,
            "npx" | "npx.cmd" | "npx.exe" | "npm" | "npm.cmd" | "npm.exe"
        ) && launcher_line.contains("@playwright/mcp"));
    if known_playwright_mcp_launcher && shell_command_can_launch_browser_helper(command_name) {
        return Some(ShellChromiumDebugPolicyViolation {
            reason: "known_playwright_mcp_browser_launcher_denied",
            markers: vec!["playwright_mcp"],
        });
    }

    if !shell_command_can_launch_browser_helper(command_name) {
        return None;
    }
    if shell_command_is_read_only_process_inspection(command_name, &lower) {
        return None;
    }

    let mentions_chromium = lower.contains("chrome.exe")
        || lower.contains("chrome ")
        || lower.contains("msedge.exe")
        || lower.contains("msedge ")
        || lower.contains("chromium.exe")
        || lower.contains("chromium ");
    let remote_debugging =
        lower.contains("--remote-debugging-pipe") || lower.contains("--remote-debugging-port");
    if !mentions_chromium || !remote_debugging {
        return None;
    }

    let has_silent = lower.contains("--silent-debugger-extension-api");
    let has_disable_extensions =
        lower.contains("--disable-extensions") && !lower.contains("--disable-extensions-except");
    let loads_extensions =
        lower.contains("--load-extension") || lower.contains("--disable-extensions-except");
    let has_user_data_dir = lower.contains("--user-data-dir");
    let default_profile =
        lower.contains("\\google\\chrome\\user data") || lower.contains("/google/chrome/user data");
    let dedicated_profile = has_user_data_dir && !default_profile;
    let has_layout_flag =
        lower.contains("--disable-blink-features") && lower.contains("automationcontrolled");

    if has_silent
        && has_disable_extensions
        && !loads_extensions
        && dedicated_profile
        && !has_layout_flag
    {
        return None;
    }

    let mut markers = vec!["remote_debugging_chromium_shell"];
    if !has_silent {
        markers.push("missing_silent_debugger_extension_api");
    }
    if !has_disable_extensions {
        markers.push("missing_disable_extensions");
    }
    if loads_extensions {
        markers.push("extension_loading_flags");
    }
    if !dedicated_profile {
        markers.push("missing_dedicated_user_data_dir");
    }
    if has_layout_flag {
        markers.push("layout_flag_automationcontrolled");
    }

    Some(ShellChromiumDebugPolicyViolation {
        reason: "shell_wrapped_chromium_remote_debugging_not_popup_safe",
        markers,
    })
}

fn shell_command_can_launch_browser_helper(command_name: &str) -> bool {
    matches!(
        command_name,
        "cmd"
            | "cmd.exe"
            | "powershell"
            | "powershell.exe"
            | "pwsh"
            | "pwsh.exe"
            | "node.exe"
            | "node"
            | "npm.cmd"
            | "npm.exe"
            | "npm"
            | "npx.cmd"
            | "npx.exe"
            | "npx"
            | "pnpm.cmd"
            | "pnpm.exe"
            | "pnpm"
            | "yarn.cmd"
            | "yarn.exe"
            | "yarn"
    )
}

fn shell_command_is_read_only_process_inspection(
    command_name: &str,
    lower_command_line: &str,
) -> bool {
    if !matches!(
        command_name,
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    ) {
        return false;
    }
    let reads_process_state = lower_command_line.contains("get-ciminstance win32_process")
        || lower_command_line.contains("get-wmiobject win32_process")
        || lower_command_line.contains("get-process");
    if !reads_process_state {
        return false;
    }

    let mutating_tokens = [
        "start-process",
        "stop-process",
        "invoke-expression",
        "iex ",
        "new-object",
        "start-job",
        "remove-item",
        "move-item",
        "set-item",
        "set-content",
        "out-file",
        "chrome.exe --",
        "msedge.exe --",
        "chromium.exe --",
        "npx @playwright/mcp",
        "npm exec @playwright/mcp",
        "pnpm dlx @playwright/mcp",
        "yarn dlx @playwright/mcp",
    ];
    !mutating_tokens
        .iter()
        .any(|token| lower_command_line.contains(token))
}

fn run_shell_chromium_debug_error(
    params: &ActRunShellParams,
    message: &'static str,
    reason: &'static str,
    direct_violation: Option<ChromiumDebugPolicyViolation>,
    shell_markers: Vec<&'static str>,
) -> ErrorData {
    let command_metadata = shell_command_metadata(&params.command, &params.args);
    let (
        user_data_dir,
        user_data_dir_state,
        silent_debugger,
        disable_extensions,
        loads_extensions,
        layout_infobar_flags,
    ) = if let Some(violation) = direct_violation {
        (
            violation.user_data_dir,
            violation.user_data_dir_state.as_str().to_owned(),
            Some(violation.silent_debugger),
            Some(violation.disable_extensions),
            Some(violation.loads_extensions),
            violation.layout_infobar_flags,
        )
    } else {
        (
            None,
            "unknown_shell_wrapped".to_owned(),
            None,
            None,
            None,
            Vec::new(),
        )
    };

    shell_tool_error(
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
        message,
        json!({
            "code": error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
            "reason": reason,
            "command": params.command,
            "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
            "args": command_metadata.args,
            "args_redacted": command_metadata.args_redacted,
            "args_original_count": command_metadata.args_original_count,
            "args_original_bytes": command_metadata.args_original_bytes,
            "args_sha256": command_metadata.args_sha256,
            "command_line": command_metadata.command_line,
            "command_line_redacted": command_metadata.command_line_redacted,
            "command_line_original_bytes": command_metadata.command_line_original_bytes,
            "command_line_sha256": command_metadata.command_line_sha256,
            "working_dir": params.working_dir,
            "shell_markers": shell_markers,
            "user_data_dir": user_data_dir,
            "user_data_dir_state": user_data_dir_state,
            "has_silent_debugger_extension_api": silent_debugger,
            "has_disable_extensions": disable_extensions,
            "has_extension_loading_flags": loads_extensions,
            "has_layout_shifting_infobar_flags": !layout_infobar_flags.is_empty(),
            "layout_shifting_infobar_flags": layout_infobar_flags,
            "required_invariant": CHROMIUM_DEBUG_LAUNCH_REQUIRED_INVARIANT,
            "remediation": "use the existing authenticated Chrome through Synapse cdp_* / target_act / browser_* tools, or use act_launch with Synapse-injected isolated CDP flags; do not start headed Playwright/Chromium automation from act_run_shell",
        }),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChromiumUserDataDirSafety {
    Missing,
    DefaultProfile,
    Dedicated,
}

impl ChromiumUserDataDirSafety {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::DefaultProfile => "default_profile",
            Self::Dedicated => "dedicated",
        }
    }
}

fn chromium_user_data_dir_safety(path: &str) -> ChromiumUserDataDirSafety {
    if path.trim().is_empty() {
        return ChromiumUserDataDirSafety::Missing;
    }
    if is_default_chrome_user_data_dir(path) {
        ChromiumUserDataDirSafety::DefaultProfile
    } else {
        ChromiumUserDataDirSafety::Dedicated
    }
}

fn has_remote_debugging_arg(args: &[String]) -> bool {
    args.iter().any(|arg| {
        is_switch_arg(arg, "--remote-debugging-port")
            || is_switch_arg(arg, "--remote-debugging-pipe")
    })
}

fn chromium_layout_infobar_flags(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|arg| {
            is_switch_arg(arg, "--disable-blink-features") && arg.contains("AutomationControlled")
        })
        .cloned()
        .collect()
}

fn user_data_dir_arg(args: &[String]) -> Option<String> {
    switch_arg_value(args, "--user-data-dir")
}

fn switch_arg_value(args: &[String], switch: &str) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        if is_switch_arg(arg, switch) {
            if let Some((_head, value)) = arg.split_once('=') {
                return Some(trim_arg_quotes(value).to_owned());
            }
            if let Some(value) = args.get(index + 1) {
                return Some(trim_arg_quotes(value).to_owned());
            }
        }
    }
    None
}

fn is_switch_arg(arg: &str, switch: &str) -> bool {
    let lower = trim_arg_quotes(arg).to_ascii_lowercase();
    let switch = switch.to_ascii_lowercase();
    lower == switch || lower.starts_with(&format!("{switch}="))
}

fn trim_arg_quotes(value: &str) -> &str {
    value.trim().trim_matches('"')
}

fn is_default_chrome_user_data_dir(path: &str) -> bool {
    let Some(default_dir) = default_chrome_user_data_dir() else {
        return false;
    };
    let candidate = normalize_path_for_policy(path);
    let default_dir = normalize_path_for_policy(default_dir.to_string_lossy().as_ref());
    candidate == default_dir || candidate.starts_with(&format!("{default_dir}\\"))
}

fn default_chrome_user_data_dir() -> Option<std::path::PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::PathBuf::from(local_app_data)
            .join("Google")
            .join("Chrome")
            .join("User Data"),
    )
}

fn normalize_path_for_policy(path: &str) -> String {
    let path = trim_arg_quotes(path);
    let path = std::path::Path::new(path);
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn validate_console_launch_visibility(params: &ActLaunchParams) -> Result<(), ErrorData> {
    if !launch_target_needs_new_console(&params.target) {
        return Ok(());
    }
    if matches!(
        params.windows_console_window_state,
        Some(LaunchWindowState::Normal)
    ) {
        return Err(launch_tool_error(
            error_codes::FOREGROUND_ACTIVATION_REFUSED,
            "act_launch refused a visible console window because Windows may activate the console host/terminal; use hidden console state for background helpers",
            json!({
                "code": error_codes::FOREGROUND_ACTIVATION_REFUSED,
                "reason": "visible_console_activation_not_proven",
                "target": params.target,
                "windows_console_window_state": params.windows_console_window_state,
            }),
        ));
    }
    if params.wait_for_window_title_regex.is_some() {
        return Err(launch_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_launch cannot wait for a console window title when console launch is hidden/no-window",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "hidden_console_has_no_window_to_wait_for",
                "target": params.target,
                "wait_for_window_title_regex": params.wait_for_window_title_regex,
                "windows_console_window_state": params.windows_console_window_state,
            }),
        ));
    }
    Ok(())
}

struct SpawnedLaunchChild {
    pid: u32,
    desktop_lease: Option<LaunchDesktopLease>,
}

fn spawn_launch_child(
    params: &ActLaunchParams,
    desktop: Option<PreparedLaunchDesktop>,
) -> Result<SpawnedLaunchChild, ErrorData> {
    #[cfg(windows)]
    {
        return spawn_windows_child(params, desktop);
    }

    #[cfg(not(windows))]
    {
        if desktop.is_some() {
            return Err(launch_tool_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_launch desktop routing is only supported on Windows",
                json!({
                    "code": error_codes::TOOL_PARAMS_INVALID,
                    "reason": "desktop_option_windows_only",
                    "target": params.target,
                }),
            ));
        }
        let needs_new_console = launch_target_needs_new_console(&params.target);

        let mut command = StdCommand::new(&params.target);
        command.args(&params.args);
        if let Some(working_dir) = &params.working_dir {
            command.current_dir(working_dir);
        }
        apply_launch_environment(&mut command, params)?;
        if needs_new_console {
            apply_new_console_creation_flags(&mut command);
        } else {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        }

        let child = command.spawn().map_err(|error| {
            launch_tool_error(
                error_codes::ACTION_TARGET_INVALID,
                format!("act_launch failed to spawn target: {error}"),
                json!({
                    "code": error_codes::ACTION_TARGET_INVALID,
                    "target": params.target,
                    "args": params.args,
                    "working_dir": params.working_dir,
                    "reason": "spawn_failed",
                }),
            )
        })?;
        Ok(SpawnedLaunchChild {
            pid: child.id(),
            desktop_lease: None,
        })
    }
}

#[cfg(not(windows))]
fn apply_launch_environment(
    command: &mut StdCommand,
    params: &ActLaunchParams,
) -> Result<(), ErrorData> {
    command.env_clear();
    let mut env = child_base_environment();
    ensure_child_temp_environment(&mut env);
    validate_child_base_environment(&env, "act_launch")?;
    for (_sort_key, (key, value)) in env {
        command.env(key, value);
    }
    command.envs(&params.env);
    Ok(())
}

fn launch_target_needs_new_console(target: &str) -> bool {
    let name = launch_target_effective_file_name(target);
    matches!(name.as_str(), "cmd.exe" | "powershell.exe" | "pwsh.exe")
}

fn launch_target_file_name(target: &str) -> String {
    Path::new(target)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(target)
        .to_ascii_lowercase()
}

fn launch_target_effective_file_name(target: &str) -> String {
    let file_name = launch_target_file_name(target);
    #[cfg(windows)]
    {
        if !is_path_like_launch_target(target)
            && Path::new(&file_name).extension().is_none()
            && !file_name.ends_with('.')
        {
            return format!("{file_name}.exe");
        }
    }
    file_name
}

#[cfg(windows)]
fn launch_target_is_absolute_windows_path(target: &str) -> bool {
    !target.contains("://") && Path::new(target).is_absolute()
}

#[cfg(windows)]
fn spawn_windows_child(
    params: &ActLaunchParams,
    desktop: Option<PreparedLaunchDesktop>,
) -> Result<SpawnedLaunchChild, ErrorData> {
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                CreateProcessW, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
            },
        },
        core::{PCWSTR, PWSTR},
    };

    let command_line = launch_command_line(params)?;
    let mut command_line_wide = wide_null(&command_line);
    let current_dir_wide = params.working_dir.as_ref().map(|dir| wide_null(dir));
    let desktop_wide = desktop
        .as_ref()
        .map(|desktop| wide_null(desktop.startup_desktop()));
    let environment = launch_environment_block(params)?;
    let startup_info_cb = u32::try_from(std::mem::size_of::<STARTUPINFOW>()).map_err(|error| {
        launch_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_launch failed to prepare console startup info: {error}"),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "reason": "launch_startup_info_size_overflow",
                "target": params.target,
            }),
        )
    })?;

    let startup_info = STARTUPINFOW {
        cb: startup_info_cb,
        lpDesktop: desktop_wide
            .as_ref()
            .map_or(PWSTR::null(), |desktop| PWSTR(desktop.as_ptr().cast_mut())),
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: windows_launch_show_window(params),
        ..Default::default()
    };

    let mut process_info = PROCESS_INFORMATION::default();
    let current_dir = current_dir_wide
        .as_ref()
        .map_or(PCWSTR::null(), |dir| PCWSTR(dir.as_ptr()));

    let result = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command_line_wide.as_mut_ptr())),
            None,
            None,
            false,
            windows_launch_creation_flags(params),
            Some(environment.as_ptr().cast()),
            current_dir,
            &raw const startup_info,
            &raw mut process_info,
        )
    };

    match result {
        Ok(()) => {
            let pid = process_info.dwProcessId;
            let _ = unsafe { CloseHandle(process_info.hThread) };
            let _ = unsafe { CloseHandle(process_info.hProcess) };
            Ok(SpawnedLaunchChild {
                pid,
                desktop_lease: desktop.map(|desktop| desktop.lease),
            })
        }
        Err(error) => Err(launch_tool_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("act_launch failed to spawn target: {error}"),
            json!({
                "code": error_codes::ACTION_TARGET_INVALID,
                "target": params.target,
                "args": params.args,
                "working_dir": params.working_dir,
                "reason": "spawn_failed",
                "desktop": params.desktop,
                "source_error": error.to_string(),
            }),
        )),
    }
}

#[derive(Debug)]
struct PreparedLaunchDesktop {
    requested: String,
    scope: &'static str,
    name: String,
    startup_desktop: String,
    session_id: Option<String>,
    lease: LaunchDesktopLease,
}

impl PreparedLaunchDesktop {
    fn startup_desktop(&self) -> &str {
        &self.startup_desktop
    }

    fn is_agent_session(&self) -> bool {
        self.scope == "agent_session"
    }

    fn launch_wait_excluded_hwnds(&self) -> Result<HashSet<i64>, ErrorData> {
        self.lease.window_hwnds().map_err(|error| {
            launch_tool_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "act_launch could not read hidden desktop '{}' before launch: {error}",
                    self.name
                ),
                json!({
                    "code": error_codes::ACTION_TARGET_INVALID,
                    "reason": "desktop_window_readback_unavailable",
                    "desktop": self.requested,
                    "desktop_name": self.name,
                    "source_error": error,
                }),
            )
        })
    }

    fn to_response(&self) -> ActLaunchDesktopReadback {
        ActLaunchDesktopReadback {
            requested: self.requested.clone(),
            scope: self.scope.to_owned(),
            name: self.name.clone(),
            startup_desktop: self.startup_desktop.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct LaunchDesktopLease {
    name: String,
    terminate_windows_on_close: bool,
    handle: Option<windows::Win32::System::StationsAndDesktops::HDESK>,
}

#[cfg(not(windows))]
#[derive(Debug)]
pub(crate) struct LaunchDesktopLease {
    name: String,
    terminate_windows_on_close: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct LaunchDesktopCloseReadback {
    pub name: String,
    pub attempted: bool,
    pub succeeded: bool,
    pub error_message: Option<String>,
    pub window_process_ids_before: Vec<u32>,
    pub window_termination_attempted: bool,
    pub window_termination_status: Option<String>,
    pub window_process_ids_after: Vec<u32>,
}

impl LaunchDesktopLease {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn is_session_owned(&self) -> bool {
        self.terminate_windows_on_close
    }

    #[cfg(windows)]
    fn window_hwnds(&self) -> Result<HashSet<i64>, String> {
        let Some(handle) = self.handle else {
            return Ok(HashSet::new());
        };
        desktop_window_hwnds(handle).map(|hwnds| hwnds.into_iter().collect::<HashSet<i64>>())
    }

    #[cfg(not(windows))]
    fn window_hwnds(&self) -> Result<HashSet<i64>, String> {
        let _ = self;
        Ok(HashSet::new())
    }

    #[cfg(windows)]
    fn raw_handle_value(&self) -> Option<isize> {
        self.handle.map(|handle| handle.0 as isize)
    }

    #[cfg(not(windows))]
    const fn raw_handle_value(&self) -> Option<isize> {
        let _ = self;
        None
    }
}

#[cfg(windows)]
impl LaunchDesktopLease {
    pub(crate) fn close(mut self) -> LaunchDesktopCloseReadback {
        let name = std::mem::take(&mut self.name);
        let terminate_windows_on_close = self.terminate_windows_on_close;
        let Some(handle) = self.handle.take() else {
            return LaunchDesktopCloseReadback {
                name,
                attempted: false,
                succeeded: true,
                error_message: None,
                window_process_ids_before: Vec::new(),
                window_termination_attempted: false,
                window_termination_status: None,
                window_process_ids_after: Vec::new(),
            };
        };

        let mut errors = Vec::new();
        let mut window_process_ids_before = Vec::new();
        let mut window_process_ids_after = Vec::new();
        let mut window_termination_attempted = false;
        let mut window_termination_status = None;

        if terminate_windows_on_close {
            match desktop_window_process_ids(handle) {
                Ok(process_ids) => {
                    window_process_ids_before = process_ids;
                    if !window_process_ids_before.is_empty() {
                        window_termination_attempted = true;
                        let termination = terminate_owned_process_ids(&window_process_ids_before);
                        window_termination_status = Some(termination.status.clone());
                        window_process_ids_after = termination.remaining_process_ids;
                    }
                }
                Err(error) => errors.push(error),
            }

            if window_termination_attempted && window_process_ids_after.is_empty() {
                match desktop_window_process_ids(handle) {
                    Ok(after) => {
                        window_process_ids_after = after;
                    }
                    Err(error) => errors.push(error),
                }
            }

            if !window_process_ids_after.is_empty() {
                errors.push(format!(
                    "desktop {name:?} still has live window process ids after termination: {window_process_ids_after:?}"
                ));
            }
        }

        if let Err(error) =
            unsafe { windows::Win32::System::StationsAndDesktops::CloseDesktop(handle) }
        {
            errors.push(error.to_string());
        }

        LaunchDesktopCloseReadback {
            name,
            attempted: true,
            succeeded: errors.is_empty(),
            error_message: (!errors.is_empty()).then(|| errors.join("; ")),
            window_process_ids_before,
            window_termination_attempted,
            window_termination_status,
            window_process_ids_after,
        }
    }
}

#[cfg(not(windows))]
impl LaunchDesktopLease {
    pub(crate) fn close(self) -> LaunchDesktopCloseReadback {
        LaunchDesktopCloseReadback {
            name: self.name,
            attempted: false,
            succeeded: true,
            error_message: None,
            window_process_ids_before: Vec::new(),
            window_termination_attempted: false,
            window_termination_status: None,
            window_process_ids_after: Vec::new(),
        }
    }
}

#[cfg(windows)]
impl Drop for LaunchDesktopLease {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            if self.terminate_windows_on_close {
                match desktop_window_process_ids(handle) {
                    Ok(process_ids) if !process_ids.is_empty() => {
                        let termination = terminate_owned_process_ids(&process_ids);
                        if !termination.remaining_process_ids.is_empty() {
                            tracing::warn!(
                                code = "ACT_LAUNCH_DESKTOP_DROP_REMAINING_WINDOWS",
                                desktop = %self.name,
                                process_ids_before = ?process_ids,
                                remaining_process_ids = ?termination.remaining_process_ids,
                                "readback=act_launch_desktop_drop after=window_process_cleanup_failed"
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            code = "ACT_LAUNCH_DESKTOP_DROP_ENUM_FAILED",
                            desktop = %self.name,
                            error = %error,
                            "readback=act_launch_desktop_drop after=window_process_enum_failed"
                        );
                    }
                }
            }
            if let Err(error) =
                unsafe { windows::Win32::System::StationsAndDesktops::CloseDesktop(handle) }
            {
                tracing::warn!(
                    code = "ACT_LAUNCH_DESKTOP_DROP_CLOSE_FAILED",
                    desktop = %self.name,
                    error = %error,
                    "readback=act_launch_desktop_drop after=close_failed"
                );
            }
        }
    }
}

#[cfg(windows)]
unsafe impl Send for LaunchDesktopLease {}

fn prepare_launch_desktop(
    requested: Option<&str>,
    session_id: Option<&str>,
) -> Result<Option<PreparedLaunchDesktop>, ErrorData> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let Some(session_id) = session_id else {
        return Err(launch_tool_error(
            error_codes::HTTP_SESSION_INVALID,
            "act_launch desktop routing requires an MCP session id so teardown can reclaim the desktop handle",
            json!({
                "code": error_codes::HTTP_SESSION_INVALID,
                "reason": "desktop_requires_mcp_session",
                "desktop": requested,
            }),
        ));
    };
    #[cfg(not(windows))]
    {
        let _ = session_id;
        return Err(launch_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_launch desktop routing is only supported on Windows",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "desktop_option_windows_only",
                "desktop": requested,
            }),
        ));
    }
    #[cfg(windows)]
    {
        let (scope, name) = if let Some(rest) = requested.strip_prefix("agent:") {
            if rest != "session" && rest != session_id {
                return Err(launch_tool_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "act_launch desktop agent scope must target the current MCP session",
                    json!({
                        "code": error_codes::TOOL_PARAMS_INVALID,
                        "reason": "desktop_agent_session_mismatch",
                        "desktop": requested,
                        "current_session_id": session_id,
                    }),
                ));
            }
            ("agent_session", hidden_desktop_name_for_session(session_id))
        } else if let Some(rest) = requested.strip_prefix("existing:") {
            ("existing", rest.to_owned())
        } else {
            return Err(launch_desktop_params_error(
                "act_launch desktop must be agent:session, agent:<current-session-id>, or existing:<desktop-name>",
                requested,
                "desktop_scope_unsupported",
            ));
        };
        let lease = open_launch_desktop(requested, scope, &name)?;
        Ok(Some(PreparedLaunchDesktop {
            requested: requested.to_owned(),
            scope,
            startup_desktop: name.clone(),
            name,
            session_id: (scope == "agent_session").then(|| session_id.to_owned()),
            lease,
        }))
    }
}

fn hidden_desktop_name_for_session(session_id: &str) -> String {
    let digest = sha256_hex(session_id.as_bytes());
    format!("SynapseAgent_{}", &digest[..24])
}

#[cfg(windows)]
fn open_launch_desktop(
    requested: &str,
    scope: &str,
    name: &str,
) -> Result<LaunchDesktopLease, ErrorData> {
    use windows::{
        Win32::System::StationsAndDesktops::{
            CreateDesktopW, DESKTOP_CONTROL_FLAGS, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW,
            DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL, DESKTOP_READ_CONTROL, DESKTOP_READOBJECTS,
            DESKTOP_WRITEOBJECTS, OpenDesktopW,
        },
        core::PCWSTR,
    };

    let access = DESKTOP_CREATEMENU.0
        | DESKTOP_CREATEWINDOW.0
        | DESKTOP_ENUMERATE.0
        | DESKTOP_HOOKCONTROL.0
        | DESKTOP_READOBJECTS.0
        | DESKTOP_READ_CONTROL.0
        | DESKTOP_WRITEOBJECTS.0;
    let name_wide = wide_null(name);
    let handle = if scope == "agent_session" {
        unsafe {
            CreateDesktopW(
                PCWSTR(name_wide.as_ptr()),
                PCWSTR::null(),
                None,
                DESKTOP_CONTROL_FLAGS::default(),
                access,
                None,
            )
        }
        .map_err(|error| {
            launch_tool_error(
                error_codes::ACTION_TARGET_INVALID,
                format!("act_launch failed to create or reuse hidden desktop '{name}': {error}"),
                json!({
                    "code": error_codes::ACTION_TARGET_INVALID,
                    "reason": "desktop_create_failed",
                    "desktop": requested,
                    "desktop_name": name,
                    "source_error": error.to_string(),
                }),
            )
        })?
    } else {
        unsafe {
            OpenDesktopW(
                PCWSTR(name_wide.as_ptr()),
                DESKTOP_CONTROL_FLAGS::default(),
                false,
                access,
            )
        }
        .map_err(|error| {
            launch_tool_error(
                error_codes::ACTION_TARGET_INVALID,
                format!("act_launch failed to open existing desktop '{name}': {error}"),
                json!({
                    "code": error_codes::ACTION_TARGET_INVALID,
                    "reason": "desktop_open_failed",
                    "desktop": requested,
                    "desktop_name": name,
                    "source_error": error.to_string(),
                }),
            )
        })?
    };
    Ok(LaunchDesktopLease {
        name: name.to_owned(),
        terminate_windows_on_close: scope == "agent_session",
        handle: Some(handle),
    })
}

#[cfg(windows)]
fn desktop_window_process_ids(
    handle: windows::Win32::System::StationsAndDesktops::HDESK,
) -> Result<Vec<u32>, String> {
    use windows::Win32::{
        Foundation::LPARAM, System::StationsAndDesktops::EnumDesktopWindows,
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    };
    use windows::core::BOOL;

    struct Search {
        process_ids: Vec<u32>,
    }

    unsafe extern "system" fn enum_window(
        hwnd: windows::Win32::Foundation::HWND,
        lparam: LPARAM,
    ) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut process_id = 0_u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&raw mut process_id));
        }
        if process_id != 0 && process_id != std::process::id() {
            search.process_ids.push(process_id);
        }
        BOOL(1)
    }

    let mut search = Search {
        process_ids: Vec::new(),
    };
    let result = unsafe {
        EnumDesktopWindows(
            Some(handle),
            Some(enum_window),
            LPARAM((&raw mut search).cast::<core::ffi::c_void>() as isize),
        )
    };
    if let Err(error) = result {
        if desktop_window_enum_error_means_empty(&error) {
            return Ok(Vec::new());
        }
        return Err(format!(
            "EnumDesktopWindows failed for hidden desktop: {error}"
        ));
    }
    search.process_ids.sort_unstable();
    search.process_ids.dedup();
    Ok(search.process_ids)
}

#[cfg(windows)]
fn desktop_window_hwnds(
    handle: windows::Win32::System::StationsAndDesktops::HDESK,
) -> Result<Vec<i64>, String> {
    use windows::Win32::{
        Foundation::{LPARAM, SetLastError, WIN32_ERROR},
        System::StationsAndDesktops::EnumDesktopWindows,
    };
    use windows::core::BOOL;

    struct Search {
        hwnds: Vec<i64>,
    }

    unsafe extern "system" fn enum_window(
        hwnd: windows::Win32::Foundation::HWND,
        lparam: LPARAM,
    ) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        search.hwnds.push(hwnd.0 as isize as i64);
        BOOL(1)
    }

    let mut search = Search { hwnds: Vec::new() };
    unsafe {
        SetLastError(WIN32_ERROR(0));
    }
    let result = unsafe {
        EnumDesktopWindows(
            Some(handle),
            Some(enum_window),
            LPARAM((&raw mut search).cast::<core::ffi::c_void>() as isize),
        )
    };
    if let Err(error) = result {
        if desktop_window_enum_error_means_empty(&error) {
            return Ok(Vec::new());
        }
        return Err(format!(
            "EnumDesktopWindows failed for hidden desktop: {error}"
        ));
    }
    search.hwnds.sort_unstable();
    search.hwnds.dedup();
    Ok(search.hwnds)
}

#[cfg(windows)]
fn desktop_window_contexts(
    handle: windows::Win32::System::StationsAndDesktops::HDESK,
) -> Result<Vec<ForegroundContext>, String> {
    Ok(desktop_window_hwnds(handle)?
        .into_iter()
        .filter_map(|hwnd| hidden_desktop_window_context(hwnd))
        .filter(|context| !context.window_title.is_empty())
        .collect())
}

#[cfg(windows)]
fn desktop_window_contexts_from_handle_value(
    handle: Option<isize>,
) -> Result<Vec<ForegroundContext>, String> {
    use windows::Win32::System::StationsAndDesktops::HDESK;

    let Some(handle) = handle else {
        return Ok(Vec::new());
    };
    desktop_window_contexts(HDESK(handle as *mut core::ffi::c_void))
}

#[cfg(not(windows))]
fn desktop_window_contexts_from_handle_value(
    _handle: Option<isize>,
) -> Result<Vec<ForegroundContext>, String> {
    Ok(Vec::new())
}

#[cfg(windows)]
fn hidden_desktop_window_context(hwnd: i64) -> Option<ForegroundContext> {
    use windows::Win32::{
        Foundation::{HWND, RECT},
        UI::WindowsAndMessaging::{GetWindowRect, GetWindowTextW, GetWindowThreadProcessId},
    };

    let hwnd = HWND(hwnd as isize as *mut core::ffi::c_void);
    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
    }
    if pid == 0 {
        return None;
    }
    let mut title_buffer = vec![0_u16; 512];
    let title_len = unsafe { GetWindowTextW(hwnd, &mut title_buffer) };
    let window_title =
        String::from_utf16_lossy(&title_buffer[..usize::try_from(title_len).unwrap_or(0)]);
    let process_path = hidden_desktop_process_path(pid).unwrap_or_default();
    let process_name = Path::new(&process_path).file_name().map_or_else(
        || format!("pid-{pid}"),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut rect = RECT::default();
    let window_bounds = unsafe { GetWindowRect(hwnd, &raw mut rect) }.map_or(
        Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        |()| Rect {
            x: rect.left,
            y: rect.top,
            w: rect.right.saturating_sub(rect.left),
            h: rect.bottom.saturating_sub(rect.top),
        },
    );

    Some(ForegroundContext {
        hwnd: hwnd.0 as isize as i64,
        pid,
        process_name,
        process_path,
        window_title,
        window_bounds,
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: None,
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: true,
    })
}

#[cfg(windows)]
fn hidden_desktop_process_path(pid: u32) -> Option<String> {
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
        core::PWSTR,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = vec![0_u16; 32_768];
    let mut len = u32::try_from(buffer.len()).ok()?;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    result.ok()?;
    Some(String::from_utf16_lossy(
        &buffer[..usize::try_from(len).ok()?],
    ))
}

#[cfg(windows)]
fn desktop_window_enum_error_means_empty(error: &windows::core::Error) -> bool {
    use windows::Win32::Foundation::{
        ERROR_ENVVAR_NOT_FOUND, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_NO_MORE_FILES,
    };

    let code = error.code();
    code.0 == 0
        || code == ERROR_FILE_NOT_FOUND.to_hresult()
        || code == ERROR_NO_MORE_FILES.to_hresult()
        || code == ERROR_INVALID_HANDLE.to_hresult()
        || code == ERROR_ENVVAR_NOT_FOUND.to_hresult()
}

#[cfg(windows)]
fn windows_launch_show_window(params: &ActLaunchParams) -> u16 {
    if launch_target_needs_new_console(&params.target) {
        SW_HIDE
    } else {
        SW_SHOWNOACTIVATE
    }
}

#[cfg(windows)]
fn windows_launch_creation_flags(
    params: &ActLaunchParams,
) -> windows::Win32::System::Threading::PROCESS_CREATION_FLAGS {
    use windows::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    };

    if launch_target_needs_new_console(&params.target) {
        CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT
    } else {
        CREATE_UNICODE_ENVIRONMENT
    }
}

#[cfg(windows)]
fn launch_environment_block(params: &ActLaunchParams) -> Result<Vec<u16>, ErrorData> {
    let mut env = child_base_environment();
    ensure_child_temp_environment(&mut env);
    validate_child_base_environment(&env, "act_launch")?;
    for (key, value) in &params.env {
        validate_launch_environment_entry(key, value)?;
        env.insert(key.to_ascii_uppercase(), (key.clone(), value.clone()));
    }

    let mut block = Vec::new();
    for (_sort_key, (key, value)) in env {
        block.extend(format!("{key}={value}").encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

pub(crate) fn launch_child_environment(
    params: &ActLaunchParams,
    surface: &'static str,
) -> Result<BTreeMap<String, String>, ErrorData> {
    let mut env = child_base_environment();
    ensure_child_temp_environment(&mut env);
    validate_child_base_environment(&env, surface)?;
    for (key, value) in &params.env {
        #[cfg(windows)]
        validate_launch_environment_entry(key, value)?;
        env.insert(key.to_ascii_uppercase(), (key.clone(), value.clone()));
    }
    Ok(env.into_values().collect())
}

fn child_base_environment() -> BTreeMap<String, (String, String)> {
    let mut env: BTreeMap<String, (String, String)> = BTreeMap::new();
    for key in PROCESS_BASE_ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            env.insert(
                key.to_ascii_uppercase(),
                (key.to_owned(), value.to_string_lossy().into_owned()),
            );
        }
    }
    add_windows_registry_environment(&mut env);
    add_windows_standard_environment(&mut env);
    add_windows_profile_environment(&mut env);
    env
}

/// Resolves a bare executable name (`rg`, `findstr`, …) against a semicolon
/// PATH plus PATHEXT, returning the first matching file. Mirrors how Windows
/// resolves a bare command name so the readback matches what a shell job's own
/// executable resolution would find.
#[cfg(windows)]
fn resolve_program_on_path(program: &str, path: &str, pathext: &str) -> Option<String> {
    let exts: Vec<&str> = pathext
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .collect();
    for dir in path.split(';').map(str::trim).filter(|dir| !dir.is_empty()) {
        let base = Path::new(dir.trim_matches('"'));
        // Honor an already-qualified name (e.g. "rg.exe") before appending exts.
        let direct = base.join(program);
        if direct.is_file() {
            return Some(direct.to_string_lossy().into_owned());
        }
        for ext in &exts {
            let candidate = base.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Reports which bounded-search tools resolve inside the exact child-process
/// environment Synapse shell jobs receive — not the daemon's own PATH.
///
/// Agents are told to prefer `rg` for fast bounded FSV scans, but `rg` may be
/// absent from the machine entirely (it is not a Windows built-in, and it lived
/// in `~/.cargo/bin` which is easy to wipe). Without a deterministic
/// availability signal an agent only learns `rg` is missing *after* a shell job
/// fails with `is not recognized`, and a harness that does not fail closed on
/// stderr can mistake that for a completed scan (#1505, #1506). This readback
/// lets an agent pick a resolvable primitive up front. `findstr` (a Windows
/// built-in) and PowerShell `Select-String` are the documented always-available
/// fallbacks when `rg` is absent.
#[must_use]
pub fn shell_search_tool_readback() -> String {
    #[cfg(windows)]
    {
        let env = child_base_environment();
        let path = env_value(&env, "PATH").unwrap_or_default();
        let pathext = env_value(&env, "PATHEXT").unwrap_or(WINDOWS_DEFAULT_PATHEXT);
        let rg = resolve_program_on_path("rg", path, pathext);
        let findstr = resolve_program_on_path("findstr", path, pathext);
        let git = resolve_program_on_path("git", path, pathext);
        let powershell = resolve_program_on_path("powershell", path, pathext)
            .or_else(|| resolve_program_on_path("pwsh", path, pathext));
        let primary = if rg.is_some() {
            "rg"
        } else if findstr.is_some() {
            "findstr"
        } else {
            "powershell_select_string"
        };
        format!(
            "shell_search_tools rg={} findstr={} git={} powershell={} primary={primary} documented_fallback=powershell_select_string",
            rg.as_deref().unwrap_or("absent"),
            findstr.as_deref().unwrap_or("absent"),
            git.as_deref().unwrap_or("absent"),
            powershell.as_deref().unwrap_or("absent"),
        )
    }
    #[cfg(not(windows))]
    {
        "shell_search_tools platform=non_windows primary=which_rg_or_grep documented_fallback=grep"
            .to_owned()
    }
}

fn env_value<'a>(env: &'a BTreeMap<String, (String, String)>, key: &str) -> Option<&'a str> {
    env.get(&key.to_ascii_uppercase())
        .map(|(_key, value)| value.as_str())
        .filter(|value| !value.trim().is_empty())
}

fn set_env_value(env: &mut BTreeMap<String, (String, String)>, key: &str, value: String) {
    if value.trim().is_empty() || value.contains('\0') {
        tracing::warn!(
            code = "M4_CHILD_ENV_DERIVE_INVALID",
            key,
            "child process environment derivation produced an invalid value"
        );
        return;
    }
    env.insert(key.to_ascii_uppercase(), (key.to_owned(), value));
}

fn insert_env_if_absent(env: &mut BTreeMap<String, (String, String)>, key: &str, value: String) {
    if env_value(env, key).is_none() {
        set_env_value(env, key, value);
    }
}

fn merge_semicolon_env_value(
    env: &mut BTreeMap<String, (String, String)>,
    key: &str,
    incoming: &str,
) {
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for raw in env_value(env, key)
        .into_iter()
        .chain(std::iter::once(incoming))
    {
        for part in raw
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let normalized = part.trim_matches('"').to_ascii_uppercase();
            if seen.insert(normalized) {
                parts.push(part.to_owned());
            }
        }
    }
    if !parts.is_empty() {
        set_env_value(env, key, parts.join(";"));
    }
}

fn ensure_child_temp_environment(env: &mut BTreeMap<String, (String, String)>) {
    if env.contains_key("TEMP") && env.contains_key("TMP") {
        return;
    }
    let Some(local_appdata) = env.get("LOCALAPPDATA").map(|(_key, value)| value.clone()) else {
        tracing::warn!(
            code = "M4_CHILD_ENV_TEMP_UNAVAILABLE",
            "child process environment is missing TEMP/TMP and LOCALAPPDATA"
        );
        return;
    };
    let candidate = Path::new(&local_appdata).join("Temp");
    let temp = candidate.to_string_lossy().into_owned();
    insert_env_if_absent(env, "TEMP", temp.clone());
    insert_env_if_absent(env, "TMP", temp);
}

#[cfg(windows)]
fn add_windows_registry_environment(env: &mut BTreeMap<String, (String, String)>) {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    const MACHINE_ENVIRONMENT: &str =
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    const USER_ENVIRONMENT: &str = "Environment";

    for key in PROCESS_BASE_ENV_KEYS {
        if let Some(value) =
            read_windows_registry_environment_value(HKEY_LOCAL_MACHINE, MACHINE_ENVIRONMENT, key)
        {
            apply_windows_registry_environment_value(env, key, value, "machine");
        }
    }
    for key in PROCESS_BASE_ENV_KEYS {
        if let Some(value) =
            read_windows_registry_environment_value(HKEY_CURRENT_USER, USER_ENVIRONMENT, key)
        {
            apply_windows_registry_environment_value(env, key, value, "user");
        }
    }
}

#[cfg(not(windows))]
fn add_windows_registry_environment(_env: &mut BTreeMap<String, (String, String)>) {}

#[cfg(windows)]
fn read_windows_registry_environment_value(
    root: windows::Win32::System::Registry::HKEY,
    subkey: &str,
    value_name: &str,
) -> Option<String> {
    use windows::{
        Win32::{
            Foundation::ERROR_SUCCESS,
            System::Registry::{REG_VALUE_TYPE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW},
        },
        core::PCWSTR,
    };

    let subkey_wide = wide_null(subkey);
    let value_wide = wide_null(value_name);
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let mut value_type = REG_VALUE_TYPE::default();
    let mut byte_len = 0_u32;
    let status = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey_wide.as_ptr()),
            PCWSTR(value_wide.as_ptr()),
            flags,
            Some(&raw mut value_type),
            None,
            Some(&raw mut byte_len),
        )
    };
    if status != ERROR_SUCCESS || byte_len == 0 {
        return None;
    }

    let mut buffer = vec![0_u16; (byte_len as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey_wide.as_ptr()),
            PCWSTR(value_wide.as_ptr()),
            flags,
            Some(&raw mut value_type),
            Some(buffer.as_mut_ptr().cast()),
            Some(&raw mut byte_len),
        )
    };
    if status != ERROR_SUCCESS {
        tracing::warn!(
            code = "M4_CHILD_ENV_REGISTRY_READ_FAILED",
            key = value_name,
            status = status.0,
            "child process environment registry read failed after size query"
        );
        return None;
    }

    let units = (byte_len as usize).div_ceil(2).min(buffer.len());
    buffer.truncate(units);
    let nul = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    let raw = String::from_utf16_lossy(&buffer[..nul]);
    expand_windows_environment_string(&raw).or(Some(raw))
}

#[cfg(windows)]
fn expand_windows_environment_string(raw: &str) -> Option<String> {
    use windows::{Win32::System::Environment::ExpandEnvironmentStringsW, core::PCWSTR};

    let source = wide_null(raw);
    let required = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), None) };
    if required == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; required as usize];
    let written = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), Some(&mut buffer)) };
    if written == 0 || written as usize > buffer.len() {
        return None;
    }
    let len = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(written as usize);
    Some(String::from_utf16_lossy(&buffer[..len]))
}

#[cfg(windows)]
fn apply_windows_registry_environment_value(
    env: &mut BTreeMap<String, (String, String)>,
    key: &str,
    value: String,
    source: &'static str,
) {
    if key.eq_ignore_ascii_case("PATH") || key.eq_ignore_ascii_case("PATHEXT") {
        let before = env_value(env, key).map(ToOwned::to_owned);
        merge_semicolon_env_value(env, key, &value);
        if before.as_deref() != env_value(env, key) {
            tracing::info!(
                code = "M4_CHILD_ENV_REGISTRY_MERGED",
                key,
                source,
                "merged persisted Windows environment value into child process environment"
            );
        }
        return;
    }

    if env_value(env, key).is_none() {
        set_env_value(env, key, value);
    }
}

#[cfg(windows)]
fn add_windows_standard_environment(env: &mut BTreeMap<String, (String, String)>) {
    let system_root = env_value(env, "SystemRoot")
        .or_else(|| env_value(env, "windir"))
        .map(ToOwned::to_owned)
        .or_else(windows_directory)
        .unwrap_or_else(|| r"C:\Windows".to_owned());
    let system_drive = env_value(env, "SystemDrive")
        .map(ToOwned::to_owned)
        .or_else(|| windows_drive_from_path(&system_root))
        .unwrap_or_else(|| "C:".to_owned());

    insert_env_if_absent(env, "SystemDrive", system_drive.clone());
    insert_env_if_absent(env, "SystemRoot", system_root.clone());
    insert_env_if_absent(env, "windir", system_root.clone());
    insert_env_if_absent(
        env,
        "ComSpec",
        Path::new(&system_root)
            .join("System32")
            .join("cmd.exe")
            .to_string_lossy()
            .into_owned(),
    );
    insert_env_if_absent(
        env,
        "ProgramData",
        Path::new(&system_drive)
            .join("ProgramData")
            .to_string_lossy()
            .into_owned(),
    );
    insert_env_if_absent(
        env,
        "ProgramFiles",
        Path::new(&system_drive)
            .join("Program Files")
            .to_string_lossy()
            .into_owned(),
    );
    let program_files_x86 = Path::new(&system_drive).join("Program Files (x86)");
    if program_files_x86.is_dir() {
        let value = program_files_x86.to_string_lossy().into_owned();
        insert_env_if_absent(env, "ProgramFiles(x86)", value.clone());
        insert_env_if_absent(
            env,
            "CommonProgramFiles(x86)",
            format!("{value}\\Common Files"),
        );
    }
    let program_files = env_value(env, "ProgramFiles").map(ToOwned::to_owned);
    if let Some(program_files) = program_files {
        insert_env_if_absent(env, "ProgramW6432", program_files.clone());
        insert_env_if_absent(
            env,
            "CommonProgramFiles",
            format!("{program_files}\\Common Files"),
        );
        insert_env_if_absent(
            env,
            "CommonProgramW6432",
            format!("{program_files}\\Common Files"),
        );
    }

    ensure_windows_pathext(env);
    ensure_windows_path_entries(env, &system_root);
}

#[cfg(not(windows))]
fn add_windows_standard_environment(_env: &mut BTreeMap<String, (String, String)>) {}

#[cfg(windows)]
fn windows_directory() -> Option<String> {
    use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;

    let mut buffer = vec![0_u16; 260];
    let written = unsafe { GetWindowsDirectoryW(Some(&mut buffer)) };
    if written == 0 || written as usize >= buffer.len() {
        tracing::warn!(
            code = "M4_CHILD_ENV_WINDOWS_DIR_UNAVAILABLE",
            written,
            "GetWindowsDirectoryW did not return a usable Windows directory"
        );
        return None;
    }
    buffer.truncate(written as usize);
    Some(String::from_utf16_lossy(&buffer))
}

#[cfg(windows)]
fn windows_drive_from_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.get(1).is_some_and(|value| *value == b':') {
        return Some(path[..2].to_owned());
    }
    None
}

#[cfg(windows)]
fn ensure_windows_pathext(env: &mut BTreeMap<String, (String, String)>) {
    let before = env_value(env, "PATHEXT").map(ToOwned::to_owned);
    merge_semicolon_env_value(env, "PATHEXT", WINDOWS_DEFAULT_PATHEXT);
    let after = env_value(env, "PATHEXT").map(ToOwned::to_owned);
    if before != after {
        tracing::warn!(
            code = "M4_CHILD_ENV_PATHEXT_NORMALIZED",
            before = before.as_deref().unwrap_or("<missing>"),
            after = after.as_deref().unwrap_or("<missing>"),
            "normalized child process PATHEXT so Windows executable resolution works"
        );
    }
}

#[cfg(windows)]
fn ensure_windows_path_entries(env: &mut BTreeMap<String, (String, String)>, system_root: &str) {
    let candidates = [
        Path::new(system_root).join("System32"),
        Path::new(system_root).to_path_buf(),
        Path::new(system_root).join("System32").join("Wbem"),
        Path::new(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0"),
        Path::new(system_root).join("System32").join("OpenSSH"),
    ];
    let required = candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if required.is_empty() {
        return;
    }
    let before = env_value(env, "PATH").map(ToOwned::to_owned);
    merge_semicolon_env_value(env, "PATH", &required.join(";"));
    if before != env_value(env, "PATH").map(ToOwned::to_owned) {
        tracing::info!(
            code = "M4_CHILD_ENV_PATH_NORMALIZED",
            "merged required Windows system directories into child process PATH"
        );
    }
    prefer_windows_git_ssh_directory_on_path(env, system_root);
}

#[cfg(windows)]
fn prefer_windows_git_ssh_directory_on_path(
    env: &mut BTreeMap<String, (String, String)>,
    system_root: &str,
) {
    let Some(git_ssh_dir) = windows_git_ssh_directory() else {
        return;
    };
    let git_ssh_dir = git_ssh_dir.to_string_lossy().into_owned();
    let openssh_dir = Path::new(system_root)
        .join("System32")
        .join("OpenSSH")
        .to_string_lossy()
        .into_owned();
    let before = env_value(env, "PATH").map(ToOwned::to_owned);
    let after = reorder_semicolon_path_entry_before_targets(
        before.as_deref().unwrap_or_default(),
        &git_ssh_dir,
        &[openssh_dir],
    );
    if before.as_deref() != Some(after.as_str()) {
        set_env_value(env, "PATH", after);
        tracing::info!(
            code = "M4_CHILD_ENV_GIT_SSH_PATH_PREFERRED",
            git_ssh_dir,
            "preferred Git-bundled SSH client directory before Windows OpenSSH in child PATH"
        );
    }
}

#[cfg(windows)]
fn reorder_semicolon_path_entry_before_targets(
    current: &str,
    preferred: &str,
    targets: &[String],
) -> String {
    let preferred = preferred.trim();
    if preferred.is_empty() {
        return current.to_owned();
    }
    let preferred_norm = normalize_semicolon_path_part(preferred);
    let target_norms = targets
        .iter()
        .map(|target| normalize_semicolon_path_part(target))
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for part in current
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let normalized = normalize_semicolon_path_part(part);
        if normalized == preferred_norm {
            continue;
        }
        if seen.insert(normalized) {
            parts.push(part.to_owned());
        }
    }
    let insert_at = parts
        .iter()
        .position(|part| target_norms.contains(&normalize_semicolon_path_part(part)))
        .unwrap_or(parts.len());
    parts.insert(insert_at, preferred.to_owned());
    parts.join(";")
}

#[cfg(windows)]
fn normalize_semicolon_path_part(part: &str) -> String {
    part.trim()
        .trim_matches('"')
        .trim_end_matches(['\\', '/'])
        .to_ascii_uppercase()
}

#[cfg(windows)]
fn validate_child_base_environment(
    env: &BTreeMap<String, (String, String)>,
    surface: &'static str,
) -> Result<(), ErrorData> {
    let required = [
        "PATH",
        "PATHEXT",
        "ComSpec",
        "SystemRoot",
        "windir",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "ProgramData",
        "ProgramFiles",
    ];
    let missing: Vec<&str> = required
        .into_iter()
        .filter(|key| env_value(env, key).is_none())
        .collect();
    let mut invalid = Vec::new();
    if let Some(pathext) = env_value(env, "PATHEXT") {
        let normalized = pathext.to_ascii_uppercase();
        if !normalized.split(';').any(|part| part.trim() == ".EXE")
            || !normalized.split(';').any(|part| part.trim() == ".CMD")
        {
            invalid.push("PATHEXT_missing_EXE_or_CMD");
        }
    }
    if let Some(comspec) = env_value(env, "ComSpec")
        && !Path::new(comspec).is_file()
    {
        invalid.push("ComSpec_not_a_file");
    }
    if missing.is_empty() && invalid.is_empty() {
        return Ok(());
    }
    tracing::error!(
        code = "M4_CHILD_ENV_INCOMPLETE",
        surface,
        missing = ?missing,
        invalid = ?invalid,
        "child process environment is missing required Windows variables"
    );
    let message = format!(
        "{surface} cannot spawn a reliable Windows child process because Synapse could not construct required environment variables: missing=[{}] invalid=[{}]",
        missing.join(", "),
        invalid.join(", ")
    );
    let data = json!({
        "code": error_codes::ACTION_TARGET_INVALID,
        "reason": "child_environment_incomplete",
        "surface": surface,
        "missing": missing,
        "invalid": invalid,
        "required": required,
    });
    if surface == "act_run_shell" {
        return Err(shell_tool_error(
            error_codes::ACTION_TARGET_INVALID,
            message,
            data,
        ));
    }
    Err(launch_tool_error(
        error_codes::ACTION_TARGET_INVALID,
        message,
        data,
    ))
}

#[cfg(not(windows))]
fn validate_child_base_environment(
    _env: &BTreeMap<String, (String, String)>,
    _surface: &'static str,
) -> Result<(), ErrorData> {
    Ok(())
}

#[cfg(windows)]
fn add_windows_profile_environment(env: &mut BTreeMap<String, (String, String)>) {
    let Some(userprofile) = env
        .get("USERPROFILE")
        .map(|(_key, value)| value.clone())
        .filter(|value| !value.trim().is_empty())
    else {
        tracing::warn!(
            code = "M4_CHILD_ENV_PROFILE_UNAVAILABLE",
            "child process environment is missing USERPROFILE; APPDATA and LOCALAPPDATA cannot be derived"
        );
        return;
    };
    let profile = Path::new(&userprofile);
    insert_env_if_absent(
        env,
        "APPDATA",
        profile
            .join("AppData")
            .join("Roaming")
            .to_string_lossy()
            .into_owned(),
    );
    insert_env_if_absent(
        env,
        "LOCALAPPDATA",
        profile
            .join("AppData")
            .join("Local")
            .to_string_lossy()
            .into_owned(),
    );
    let system_drive = env
        .get("SYSTEMDRIVE")
        .map(|(_key, value)| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("C:");
    insert_env_if_absent(env, "ProgramData", format!("{system_drive}\\ProgramData"));
}

#[cfg(not(windows))]
fn add_windows_profile_environment(_env: &mut BTreeMap<String, (String, String)>) {}

#[cfg(windows)]
fn validate_launch_environment_entry(key: &str, value: &str) -> Result<(), ErrorData> {
    if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_launch env entries must have non-empty keys without '=' or NUL and values without NUL",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(not(windows))]
const fn apply_new_console_creation_flags(_command: &mut StdCommand) {}

#[derive(Debug)]
struct WindowWaitResult {
    hwnd: Option<i64>,
    /// PID owning the matched window (#1358) — may differ from the launched pid
    /// when the existing-window fallback matches a pre-existing instance.
    matched_pid: Option<u32>,
    matched_title: Option<String>,
    reason: Option<String>,
}

impl WindowWaitResult {
    const fn not_requested() -> Self {
        Self {
            hwnd: None,
            matched_pid: None,
            matched_title: None,
            reason: None,
        }
    }

    fn matched(context: ForegroundContext) -> Self {
        Self {
            hwnd: Some(context.hwnd),
            matched_pid: Some(context.pid),
            matched_title: Some(context.window_title),
            reason: None,
        }
    }
}

fn excluded_launch_wait_hwnds(
    wait_regex: Option<&regex::Regex>,
    launch_desktop: Option<&PreparedLaunchDesktop>,
) -> Result<HashSet<i64>, ErrorData> {
    if wait_regex.is_none() {
        return Ok(HashSet::new());
    }
    if let Some(desktop) = launch_desktop {
        return desktop.launch_wait_excluded_hwnds();
    }
    Ok(snapshot_visible_window_hwnds())
}

async fn wait_for_launch_window(
    pid: u32,
    title_regex: &regex::Regex,
    timeout_ms: u64,
    excluded_hwnds: &HashSet<i64>,
    launch_target_name: &str,
    launch_args: &[String],
) -> Result<WindowWaitResult, ErrorData> {
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let mut last_error: Option<String>;
    let mut last_windows = Vec::new();
    let mut last_title_mismatch: Vec<serde_json::Value> = Vec::new();
    loop {
        match synapse_a11y::visible_top_level_window_contexts() {
            Ok(contexts) => {
                last_windows = window_context_summaries(&contexts);
                last_title_mismatch = title_matching_other_pid_windows(&contexts, title_regex, pid);
                if let Some(context) = select_launch_window(
                    &contexts,
                    pid,
                    title_regex,
                    excluded_hwnds,
                    launch_target_name,
                    launch_args,
                ) {
                    tracing::info!(
                        code = "M4_ACT_LAUNCH_WINDOW_MATCHED",
                        hwnd = context.hwnd,
                        pid = context.pid,
                        title = %context.window_title,
                        "act_launch matched the requested launched window without activating it"
                    );
                    return Ok(WindowWaitResult::matched(context.clone()));
                }
                last_error = None;
            }
            Err(error) if error.code() == error_codes::A11Y_NOT_AVAILABLE => {
                tracing::error!(
                    code = error.code(),
                    error = %error,
                    "act_launch window readback unavailable"
                );
                return Err(launch_window_error(
                    "window_readback_unavailable",
                    pid,
                    title_regex.as_str(),
                    timeout_ms,
                    Some(error.to_string()),
                    &last_windows,
                    &last_title_mismatch,
                ));
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }

        if started.elapsed() >= timeout {
            tracing::warn!(
                code = "M4_ACT_LAUNCH_WINDOW_WAIT_TIMEOUT",
                pid,
                title_regex = title_regex.as_str(),
                ?excluded_hwnds,
                last_error,
                "act_launch window title wait timed out"
            );
            return Err(launch_window_error(
                "no_match_within_timeout",
                pid,
                title_regex.as_str(),
                timeout_ms,
                last_error,
                &last_windows,
                &last_title_mismatch,
            ));
        }
        tokio::time::sleep(Duration::from_millis(LAUNCH_WINDOW_POLL_INTERVAL_MS)).await;
    }
}

async fn wait_for_launch_desktop_window(
    pid: u32,
    title_regex: &regex::Regex,
    timeout_ms: u64,
    excluded_hwnds: &HashSet<i64>,
    launch_target_name: &str,
    launch_args: &[String],
    desktop_name: String,
    desktop_handle: Option<isize>,
) -> Result<WindowWaitResult, ErrorData> {
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let mut last_error: Option<String>;
    let mut last_windows = Vec::new();
    let mut last_title_mismatch: Vec<serde_json::Value> = Vec::new();
    loop {
        match desktop_window_contexts_from_handle_value(desktop_handle) {
            Ok(contexts) => {
                last_windows = window_context_summaries(&contexts);
                last_title_mismatch = title_matching_other_pid_windows(&contexts, title_regex, pid);
                if let Some(context) = select_launch_desktop_window(
                    &contexts,
                    pid,
                    title_regex,
                    excluded_hwnds,
                    launch_target_name,
                    launch_args,
                ) {
                    tracing::info!(
                        code = "M4_ACT_LAUNCH_DESKTOP_WINDOW_MATCHED",
                        hwnd = context.hwnd,
                        pid = context.pid,
                        title = %context.window_title,
                        desktop = %desktop_name,
                        "act_launch matched the requested launched hidden-desktop window without activating the human foreground"
                    );
                    return Ok(WindowWaitResult::matched(context.clone()));
                }
                last_error = None;
            }
            Err(error) => {
                last_error = Some(error);
            }
        }

        if started.elapsed() >= timeout {
            tracing::warn!(
                code = "M4_ACT_LAUNCH_DESKTOP_WINDOW_WAIT_TIMEOUT",
                pid,
                desktop = %desktop_name,
                title_regex = title_regex.as_str(),
                ?excluded_hwnds,
                last_error,
                "act_launch hidden-desktop window title wait timed out"
            );
            return Err(launch_window_error(
                "desktop_no_match_within_timeout",
                pid,
                title_regex.as_str(),
                timeout_ms,
                last_error,
                &last_windows,
                &last_title_mismatch,
            ));
        }
        tokio::time::sleep(Duration::from_millis(LAUNCH_WINDOW_POLL_INTERVAL_MS)).await;
    }
}

fn window_context_summaries(contexts: &[ForegroundContext]) -> Vec<serde_json::Value> {
    contexts
        .iter()
        .take(12)
        .map(|context| {
            json!({
                "hwnd": context.hwnd,
                "pid": context.pid,
                "process_name": context.process_name,
                "title": context.window_title,
            })
        })
        .collect()
}

/// Windows whose title matches the wait regex but are NOT owned by the launched
/// pid — the actionable "why wasn't the already-visible matching window
/// accepted?" diagnostic for an act_launch window-wait timeout (#1357). A
/// non-empty list means a same-titled window exists under a different process: a
/// stale/leftover instance from an earlier launch, or the launched process
/// re-exec'd into a pid act_launch is not tracking. act_launch only accepts a
/// title match owned by the launched pid, so these are rejected — and now say so.
fn title_matching_other_pid_windows(
    contexts: &[ForegroundContext],
    title_regex: &regex::Regex,
    launch_pid: u32,
) -> Vec<serde_json::Value> {
    contexts
        .iter()
        .filter(|context| context.pid != launch_pid && title_regex.is_match(&context.window_title))
        .take(12)
        .map(|context| {
            json!({
                "hwnd": context.hwnd,
                "pid": context.pid,
                "process_name": context.process_name,
                "title": context.window_title,
            })
        })
        .collect()
}

fn refuse_shared_tabbed_app_visible_reuse(
    params: &ActLaunchParams,
    launch_target_name: &str,
    launch_desktop: Option<&PreparedLaunchDesktop>,
) -> Result<(), ErrorData> {
    let Some(risk_family) = shared_tabbed_app_family(launch_target_name) else {
        return Ok(());
    };

    #[cfg(not(windows))]
    {
        let _ = params;
        let _ = launch_desktop;
        let _ = risk_family;
        return Ok(());
    }

    #[cfg(windows)]
    {
        let contexts = synapse_a11y::visible_top_level_window_contexts().map_err(|error| {
            launch_tool_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "act_launch could not prove no existing shared tabbed app window is open for {launch_target_name}; refusing before spawn so the launch cannot hijack a user tabbed host"
                ),
                json!({
                    "code": error_codes::ACTION_TARGET_INVALID,
                    "reason": "shared_tabbed_app_preflight_unavailable",
                    "target": params.target,
                    "args": params.args,
                    "desktop": params.desktop,
                    "launch_target_name": launch_target_name,
                    "risk_family": risk_family,
                    "source_error_code": error.code(),
                    "source_error": error.to_string(),
                    "resolution": "retry after the window Source of Truth is readable, or use a provably owned native target instead of launching into a shared tabbed app host",
                }),
            )
        })?;
        let risky_windows = contexts
            .into_iter()
            .filter(|context| shared_tabbed_app_window_matches(launch_target_name, context))
            .collect::<Vec<_>>();
        if risky_windows.is_empty() {
            return Ok(());
        }
        if let Some(desktop) = launch_desktop {
            if desktop.is_agent_session() && params.wait_for_window_title_regex.is_some() {
                tracing::info!(
                    code = "M4_ACT_LAUNCH_SHARED_TABBED_DESKTOP_ROUTE_ALLOWED",
                    target = %params.target,
                    launch_target_name,
                    desktop = %desktop.name,
                    existing_window_count = risky_windows.len(),
                    "act_launch allowing shared-tabbed app launch because session-owned hidden desktop plus window wait will prove the owned target"
                );
                return Ok(());
            }
            return Err(launch_tool_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "act_launch refused {launch_target_name} on desktop route because an existing visible shared tabbed app window is already open and this desktop route cannot prove a new session-owned target"
                ),
                json!({
                    "code": error_codes::ACTION_TARGET_INVALID,
                    "reason": "shared_tabbed_app_desktop_wait_required",
                    "target": params.target,
                    "args": params.args,
                    "desktop": params.desktop,
                    "launch_target_name": launch_target_name,
                    "risk_family": risk_family,
                    "existing_window_count": risky_windows.len(),
                    "observed_windows": window_context_summaries(&risky_windows),
                    "resolution": "use desktop=agent:session with wait_for_window_title_regex so hidden-desktop enumeration can prove the owned target",
                }),
            ));
        }
        Err(launch_tool_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "act_launch refused {launch_target_name} because an existing visible shared tabbed app window is already open; spawning could reuse that user-owned tab host"
            ),
            json!({
                "code": error_codes::ACTION_TARGET_INVALID,
                "reason": "shared_tabbed_app_existing_window_risk",
                "target": params.target,
                "args": params.args,
                "desktop": params.desktop,
                "launch_target_name": launch_target_name,
                "risk_family": risk_family,
                "existing_window_count": risky_windows.len(),
                "observed_windows": window_context_summaries(&risky_windows),
                "resolution": "create or select a provably owned native tab/document target first; Synapse refuses to launch into an existing user-owned tabbed host",
            }),
        ))
    }
}

fn shared_tabbed_app_family(target_name: &str) -> Option<&'static str> {
    match target_name.to_ascii_lowercase().as_str() {
        "notepad.exe" => Some("windows_notepad_tabbed_host"),
        _ => None,
    }
}

fn shared_tabbed_app_window_matches(target_name: &str, context: &ForegroundContext) -> bool {
    shared_tabbed_app_family(target_name).is_some()
        && context.process_name.eq_ignore_ascii_case(target_name)
}

fn launch_window_error(
    reason: &'static str,
    pid: u32,
    title_regex: &str,
    timeout_ms: u64,
    last_error: Option<String>,
    observed_windows: &[serde_json::Value],
    title_matching_other_pid: &[serde_json::Value],
) -> ErrorData {
    // #1357: when a title-matching window exists but under a different pid,
    // explain WHY it was not accepted instead of leaving the caller to compare
    // pids in observed_windows by hand.
    let rejection_explanation = (!title_matching_other_pid.is_empty()).then(|| {
        format!(
            "{} visible window(s) match the title regex but are owned by a different pid than the launched process (pid {pid}). act_launch only accepts a title match owned by the launched pid, so these were rejected as stale/foreign instances. Close the stale window(s) and retry, wait for the launched pid's own window, or — if the target intentionally re-execs into another process — match on that pid instead.",
            title_matching_other_pid.len()
        )
    });
    launch_tool_error(
        error_codes::ACTION_LAUNCH_WINDOW_NOT_FOUND,
        format!("act_launch did not verify requested launch window: {reason}"),
        json!({
            "code": error_codes::ACTION_LAUNCH_WINDOW_NOT_FOUND,
            "reason": reason,
            "pid": pid,
            "title_regex": title_regex,
            "timeout_ms": timeout_ms,
            "last_error": last_error,
            "observed_windows": observed_windows,
            "title_matching_windows_with_other_pid": title_matching_other_pid,
            "rejection_explanation": rejection_explanation,
        }),
    )
}

fn select_launch_window<'a>(
    contexts: &'a [ForegroundContext],
    pid: u32,
    title_regex: &regex::Regex,
    excluded_hwnds: &HashSet<i64>,
    launch_target_name: &str,
    launch_args: &[String],
) -> Option<&'a ForegroundContext> {
    contexts
        .iter()
        .find(|context| {
            context.pid == pid
                && !excluded_hwnds.contains(&context.hwnd)
                && title_regex.is_match(&context.window_title)
        })
        .or_else(|| {
            contexts.iter().find(|context| {
                !excluded_hwnds.contains(&context.hwnd)
                    && launch_target_matches_brokered_window(
                        launch_target_name,
                        launch_args,
                        context,
                    )
                    && title_regex.is_match(&context.window_title)
            })
        })
        .or_else(|| {
            contexts.iter().find(|context| {
                excluded_hwnds.contains(&context.hwnd)
                    && launch_target_matches_existing_window(
                        launch_target_name,
                        launch_args,
                        context,
                    )
                    && title_regex.is_match(&context.window_title)
            })
        })
}

fn select_launch_desktop_window<'a>(
    contexts: &'a [ForegroundContext],
    pid: u32,
    title_regex: &regex::Regex,
    excluded_hwnds: &HashSet<i64>,
    launch_target_name: &str,
    launch_args: &[String],
) -> Option<&'a ForegroundContext> {
    select_launch_window(
        contexts,
        pid,
        title_regex,
        excluded_hwnds,
        launch_target_name,
        launch_args,
    )
    .or_else(|| {
        contexts.iter().find(|context| {
            !excluded_hwnds.contains(&context.hwnd)
                && launch_target_matches_hidden_desktop_spawn_window(launch_target_name, context)
                && title_regex.is_match(&context.window_title)
        })
    })
}

fn launch_target_matches_hidden_desktop_spawn_window(
    target_name: &str,
    context: &ForegroundContext,
) -> bool {
    let target_name = target_name.to_ascii_lowercase();
    let process_name = context.process_name.to_ascii_lowercase();
    shared_tabbed_app_family(&target_name).is_some() && target_name == process_name
}

fn launch_target_matches_brokered_window(
    target_name: &str,
    launch_args: &[String],
    context: &ForegroundContext,
) -> bool {
    let target_name = target_name.to_ascii_lowercase();
    let process_name = context.process_name.to_ascii_lowercase();
    if target_name == process_name {
        return false;
    }
    launch_target_matches_shell_activation(&target_name, launch_args, &process_name)
        || matches!(
            (target_name.as_str(), process_name.as_str()),
            ("wt.exe", "windowsterminal.exe")
                | (
                    "calc.exe",
                    "calculatorapp.exe" | "calculator.exe" | "applicationframehost.exe",
                )
                | (
                    "cmd.exe" | "powershell.exe" | "pwsh.exe",
                    "windowsterminal.exe" | "openconsole.exe" | "conhost.exe",
                )
        )
}

fn launch_target_matches_existing_window(
    target_name: &str,
    launch_args: &[String],
    context: &ForegroundContext,
) -> bool {
    let target_name = target_name.to_ascii_lowercase();
    let process_name = context.process_name.to_ascii_lowercase();
    if shared_tabbed_app_family(&target_name).is_some() && target_name == process_name {
        return false;
    }
    target_name == process_name
        || launch_target_matches_shell_activation(&target_name, launch_args, &process_name)
        || matches!(
            (target_name.as_str(), process_name.as_str()),
            ("wt.exe", "windowsterminal.exe")
                | (
                    "calc.exe",
                    "calculatorapp.exe" | "calculator.exe" | "applicationframehost.exe",
                )
                | (
                    "cmd.exe" | "powershell.exe" | "pwsh.exe",
                    "windowsterminal.exe" | "openconsole.exe" | "conhost.exe",
                )
        )
}

fn launch_target_matches_shell_activation(
    target_name: &str,
    launch_args: &[String],
    process_name: &str,
) -> bool {
    if target_name != "explorer.exe" {
        return false;
    }
    let args = launch_args
        .iter()
        .map(|arg| arg.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "ms-settings:") {
        return matches!(
            process_name,
            "systemsettings.exe" | "applicationframehost.exe"
        );
    }
    if args
        .iter()
        .any(|arg| arg.contains("microsoft.windows.photos"))
    {
        return matches!(
            process_name,
            "photos.exe" | "microsoft.photos.exe" | "applicationframehost.exe"
        );
    }
    false
}

fn snapshot_visible_window_hwnds() -> HashSet<i64> {
    match synapse_a11y::visible_top_level_window_contexts() {
        Ok(contexts) => contexts.into_iter().map(|context| context.hwnd).collect(),
        Err(error) => {
            tracing::warn!(
                code = error.code(),
                error = %error,
                "act_launch could not snapshot pre-existing windows"
            );
            HashSet::new()
        }
    }
}

fn validate_run_shell_start_params(params: &ActRunShellStartParams) -> Result<(), ErrorData> {
    if matches!(params.timeout_ms, Some(0)) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell_start timeout_ms must be >= 1 when provided",
        ));
    }
    if let Some(job_id) = &params.job_id {
        validate_shell_job_id(job_id)?;
    }
    let shell_params = run_shell_params_for_start_validation(params);
    validate_run_shell_params(&shell_params)
}

fn run_shell_params_for_start_validation(params: &ActRunShellStartParams) -> ActRunShellParams {
    ActRunShellParams {
        command: params.command.clone(),
        args: params.args.clone(),
        working_dir: params.working_dir.clone(),
        env: params.env.clone(),
        timeout_ms: params.timeout_ms.unwrap_or(1),
        execution_mode: ActRunShellExecutionMode::Durable,
        durable_timeout_ms: params.timeout_ms,
        idempotency_key: None,
    }
}

fn validate_shell_job_id(job_id: &str) -> Result<(), ErrorData> {
    if job_id.is_empty() {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell job_id must not be empty",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "job_id_empty",
            }),
        ));
    }
    if job_id.len() > SHELL_JOB_ID_MAX_BYTES {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_run_shell job_id must be <= {SHELL_JOB_ID_MAX_BYTES} bytes"),
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "job_id": job_id,
                "max_bytes": SHELL_JOB_ID_MAX_BYTES,
                "reason": "job_id_too_long",
            }),
        ));
    }
    if !job_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell job_id may contain only ASCII letters, digits, hyphen, and underscore",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "job_id": job_id,
                "reason": "job_id_invalid_characters",
            }),
        ));
    }
    Ok(())
}

fn create_shell_job_paths(
    requested_job_id: Option<&str>,
) -> Result<(String, ShellJobPaths), ErrorData> {
    let root = shell_durable_job_root_dir()?;
    fs::create_dir_all(&root).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell_start failed to create shell job root: {error}"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "path": root,
                "reason": "job_root_create_failed",
            }),
        )
    })?;

    if let Some(job_id) = requested_job_id {
        validate_shell_job_id(job_id)?;
        let paths = shell_job_paths_from_root(&root, job_id);
        match fs::create_dir(&paths.job_dir) {
            Ok(()) => return Ok((job_id.to_owned(), paths)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(shell_tool_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "act_run_shell_start job_id already exists",
                    json!({
                        "code": error_codes::TOOL_PARAMS_INVALID,
                        "job_id": job_id,
                        "path": paths.job_dir,
                        "reason": "job_id_already_exists",
                    }),
                ));
            }
            Err(error) => {
                return Err(shell_tool_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!("act_run_shell_start failed to create shell job directory: {error}"),
                    json!({
                        "code": error_codes::STORAGE_WRITE_FAILED,
                        "job_id": job_id,
                        "path": paths.job_dir,
                        "reason": "job_dir_create_failed",
                    }),
                ));
            }
        }
    }

    for _attempt in 0..8 {
        let job_id = new_reflex_id();
        let paths = shell_job_paths_from_root(&root, &job_id);
        match fs::create_dir(&paths.job_dir) {
            Ok(()) => return Ok((job_id, paths)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(shell_tool_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "act_run_shell_start failed to create generated shell job directory: {error}"
                    ),
                    json!({
                        "code": error_codes::STORAGE_WRITE_FAILED,
                        "job_id": job_id,
                        "path": paths.job_dir,
                        "reason": "job_dir_create_failed",
                    }),
                ));
            }
        }
    }
    Err(shell_tool_error(
        error_codes::TOOL_INTERNAL_ERROR,
        "act_run_shell_start could not allocate a unique shell job id",
        json!({
            "code": error_codes::TOOL_INTERNAL_ERROR,
            "root": root,
            "reason": "job_id_allocation_failed",
        }),
    ))
}

fn shell_job_paths_for_id(
    session_id: Option<&str>,
    job_id: &str,
) -> Result<ShellJobPaths, ErrorData> {
    validate_shell_job_id(job_id)?;
    let durable_paths = shell_job_paths_from_root(&shell_durable_job_root_dir()?, job_id);
    if durable_paths.status_path.exists() {
        return Ok(durable_paths);
    }
    if let Some(session_id) = session_id {
        let legacy_paths =
            shell_job_paths_from_root(&shell_job_root_dir_for_session(Some(session_id))?, job_id);
        if legacy_paths.status_path.exists() {
            return Ok(legacy_paths);
        }
    }
    Ok(durable_paths)
}

fn shell_job_paths_from_root(root: &Path, job_id: &str) -> ShellJobPaths {
    let job_dir = root.join(job_id);
    ShellJobPaths {
        stdout_path: job_dir.join("stdout.log"),
        stderr_path: job_dir.join("stderr.log"),
        status_path: job_dir.join("status.json"),
        request_path: job_dir.join("request.json"),
        remote_cleanup_path: job_dir.join("remote-cleanup.json"),
        job_dir,
    }
}

// Per-thread override of the durable shell-job store root. Set only by
// `ShellJobRootGuard` in tests so each test gets a hermetic root instead of
// sharing the process-wide `%LOCALAPPDATA%\Synapse\shell-jobs` directory
// (#1509). All durable-job path resolution funnels through
// `shell_job_root_dir`, and every root read happens synchronously on the
// caller's thread (the background monitor uses the absolute `ShellJobPaths`
// resolved at start time), so a thread-local override fully isolates a test
// without touching the production code path.
#[cfg(test)]
thread_local! {
    static SHELL_JOB_ROOT_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn shell_job_root_override() -> Option<PathBuf> {
    SHELL_JOB_ROOT_OVERRIDE.with(|cell| cell.borrow().clone())
}

#[cfg(not(test))]
#[inline]
fn shell_job_root_override() -> Option<PathBuf> {
    None
}

/// RAII guard that redirects the durable shell-job store to a unique temporary
/// directory for the lifetime of a single test, then restores the previous
/// override and drops the temp dir. Install it at the top of any test that
/// starts durable jobs, scans the job root, or asserts cleanup counts so the
/// test never observes — or is perturbed by — jobs written by other tests
/// running in parallel (#1509).
#[cfg(test)]
struct ShellJobRootGuard {
    previous: Option<PathBuf>,
    _temp: tempfile::TempDir,
}

#[cfg(test)]
impl ShellJobRootGuard {
    fn new() -> Self {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create hermetic shell-jobs root: {error}"));
        let previous = SHELL_JOB_ROOT_OVERRIDE
            .with(|cell| cell.borrow_mut().replace(temp.path().to_path_buf()));
        Self {
            previous,
            _temp: temp,
        }
    }
}

#[cfg(test)]
impl Drop for ShellJobRootGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        SHELL_JOB_ROOT_OVERRIDE.with(|cell| *cell.borrow_mut() = previous);
    }
}

fn shell_job_root_dir() -> Result<PathBuf, ErrorData> {
    if let Some(override_root) = shell_job_root_override() {
        return Ok(override_root);
    }
    // Operator/deployment seam: relocate the durable shell-job store off the
    // default per-user path (e.g. onto a faster or per-instance volume). A set
    // but empty value is a misconfiguration and must fail loudly rather than
    // silently falling back to the default root.
    if let Some(env_root) = std::env::var_os("SYNAPSE_SHELL_JOB_ROOT") {
        if env_root.is_empty() {
            return Err(shell_tool_error(
                error_codes::STORAGE_OPEN_FAILED,
                "act_run_shell SYNAPSE_SHELL_JOB_ROOT is set but empty; unset it or point it at a directory",
                json!({
                    "code": error_codes::STORAGE_OPEN_FAILED,
                    "reason": "shell_job_root_env_empty",
                }),
            ));
        }
        return Ok(PathBuf::from(env_root));
    }

    #[cfg(windows)]
    {
        let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
            return Err(shell_tool_error(
                error_codes::STORAGE_OPEN_FAILED,
                "act_run_shell_start cannot locate LOCALAPPDATA for durable shell job logs",
                json!({
                    "code": error_codes::STORAGE_OPEN_FAILED,
                    "reason": "localappdata_missing",
                }),
            ));
        };
        return Ok(PathBuf::from(local_app_data)
            .join("Synapse")
            .join("shell-jobs"));
    }

    #[cfg(not(windows))]
    {
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(state_home).join("synapse").join("shell-jobs"));
        }
        let Some(home) = std::env::var_os("HOME") else {
            return Err(shell_tool_error(
                error_codes::STORAGE_OPEN_FAILED,
                "act_run_shell_start cannot locate HOME or XDG_STATE_HOME for durable shell job logs",
                json!({
                    "code": error_codes::STORAGE_OPEN_FAILED,
                    "reason": "state_home_missing",
                }),
            ));
        };
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("synapse")
            .join("shell-jobs"))
    }
}

fn shell_job_root_dir_for_session(session_id: Option<&str>) -> Result<PathBuf, ErrorData> {
    let root = shell_job_root_dir()?;
    let Some(session_id) = session_id else {
        return Ok(root);
    };
    validate_shell_session_id(session_id)?;
    Ok(root.join(shell_session_dir_name(session_id)))
}

fn shell_durable_job_root_dir() -> Result<PathBuf, ErrorData> {
    Ok(shell_job_root_dir()?.join("jobs"))
}

fn shell_session_root_dir() -> Result<PathBuf, ErrorData> {
    #[cfg(windows)]
    {
        let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
            return Err(shell_tool_error(
                error_codes::STORAGE_OPEN_FAILED,
                "act_run_shell cannot locate LOCALAPPDATA for per-session shell directories",
                json!({
                    "code": error_codes::STORAGE_OPEN_FAILED,
                    "reason": "localappdata_missing",
                }),
            ));
        };
        return Ok(PathBuf::from(local_app_data)
            .join("Synapse")
            .join("shell-sessions"));
    }

    #[cfg(not(windows))]
    {
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(state_home)
                .join("synapse")
                .join("shell-sessions"));
        }
        let Some(home) = std::env::var_os("HOME") else {
            return Err(shell_tool_error(
                error_codes::STORAGE_OPEN_FAILED,
                "act_run_shell cannot locate HOME or XDG_STATE_HOME for per-session shell directories",
                json!({
                    "code": error_codes::STORAGE_OPEN_FAILED,
                    "reason": "state_home_missing",
                }),
            ));
        };
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("synapse")
            .join("shell-sessions"))
    }
}

fn shell_session_dir_name(session_id: &str) -> String {
    let hash = sha256_hex(session_id.as_bytes());
    format!("session-{}", &hash[..32])
}

fn validate_shell_session_id(session_id: &str) -> Result<(), ErrorData> {
    if session_id.trim().is_empty() {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell requires a non-empty MCP session id",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "session_id_empty",
            }),
        ));
    }
    if session_id.chars().count() > 512 {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell MCP session id must be at most 512 Unicode scalar values",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "session_id_too_long",
            }),
        ));
    }
    if !session_id.chars().all(|ch| ('!'..='~').contains(&ch)) {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell MCP session id must contain only visible ASCII characters",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "session_id_invalid_characters",
            }),
        ));
    }
    Ok(())
}

fn resolve_shell_working_dir(
    requested_working_dir: Option<&str>,
    context: Option<&ShellExecutionContext>,
    tool_name: &'static str,
) -> Result<PathBuf, ErrorData> {
    let path = match requested_working_dir {
        Some(path) => {
            if path.trim().is_empty() {
                return Err(shell_tool_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{tool_name} working_dir must not be empty when provided"),
                    json!({
                        "code": error_codes::TOOL_PARAMS_INVALID,
                        "reason": "working_dir_empty",
                    }),
                ));
            }
            PathBuf::from(path)
        }
        None => match context {
            Some(context) => context.default_working_dir().to_path_buf(),
            None => {
                return Ok(std::env::current_dir().map_err(|error| {
                    shell_tool_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("{tool_name} failed to read daemon current directory: {error}"),
                        json!({
                            "code": error_codes::TOOL_INTERNAL_ERROR,
                            "reason": "current_dir_read_failed",
                        }),
                    )
                })?);
            }
        },
    };
    let canonical = fs::canonicalize(&path).map_err(|error| {
        shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool_name} working_dir could not be resolved: {error}"),
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "path": path,
                "reason": "working_dir_resolve_failed",
            }),
        )
    })?;
    if !canonical.is_dir() {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool_name} working_dir is not a directory"),
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "path": canonical,
                "reason": "working_dir_not_directory",
            }),
        ));
    }
    Ok(canonical)
}

fn write_shell_job_request(
    paths: &ShellJobPaths,
    params: &ActRunShellStartParams,
    request_sha256: &str,
    context: Option<&ShellExecutionContext>,
) -> Result<(), ErrorData> {
    let command_metadata = shell_command_metadata(&params.command, &params.args);
    let request = json!({
        "schema_version": 3,
        "session_id": context.map(|context| context.session_id()),
        "session_dir": context.map(|context| path_string(context.session_dir())),
        "command": params.command,
        "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
        "args": command_metadata.args,
        "args_redacted": command_metadata.args_redacted,
        "args_original_count": command_metadata.args_original_count,
        "args_original_bytes": command_metadata.args_original_bytes,
        "args_sha256": command_metadata.args_sha256,
        "command_line": command_metadata.command_line,
        "command_line_redacted": command_metadata.command_line_redacted,
        "command_line_original_bytes": command_metadata.command_line_original_bytes,
        "command_line_sha256": command_metadata.command_line_sha256,
        "working_dir": params.working_dir,
        "effective_working_dir": params.working_dir,
        "env_keys": params.env.keys().cloned().collect::<Vec<_>>(),
        "session_env_keys": context.map_or_else(Vec::new, shell_session_env_keys),
        "timeout_ms": params.timeout_ms,
        "requested_job_id": params.job_id,
        "request_sha256": request_sha256,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    write_pretty_json_file(&paths.request_path, &request, "request")
}

fn shell_remote_cleanup_invocation_from_start_params(
    params: &ActRunShellStartParams,
) -> Option<ShellRemoteCleanupInvocation> {
    let invocation = shell_job_ssh_command_invocation(&params.command, &params.args)?;
    if ssh_family_client_for_executable(&invocation.command) != Some("ssh") {
        return None;
    }
    let parts = ssh_direct_command_parts(&invocation.args)?;
    parts.remote_command.as_ref()?;
    if parts.tracking_unsupported_reason.is_some() {
        return None;
    }
    Some(ShellRemoteCleanupInvocation {
        schema_version: 1,
        transport: SHELL_REMOTE_TRANSPORT_SSH.to_owned(),
        command: invocation.command,
        control_args: parts.control_args,
        remote_identity: parts.remote_identity,
        source_evidence: invocation.evidence.to_owned(),
        args_sha256: shell_args_sha256(&invocation.args),
        created_at: chrono::Utc::now().to_rfc3339(),
    })
}

fn write_shell_remote_cleanup_invocation(
    paths: &ShellJobPaths,
    params: &ActRunShellStartParams,
) -> Result<(), ErrorData> {
    let Some(invocation) = shell_remote_cleanup_invocation_from_start_params(params) else {
        return Ok(());
    };
    write_pretty_json_file(&paths.remote_cleanup_path, &invocation, "remote cleanup")
}

fn read_shell_remote_cleanup_invocation(
    paths: &ShellJobPaths,
    job_id: &str,
) -> Result<Option<ShellRemoteCleanupInvocation>, String> {
    if !paths.remote_cleanup_path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&paths.remote_cleanup_path)
        .map_err(|error| format!("failed to read remote cleanup sidecar for {job_id}: {error}"))?;
    let invocation: ShellRemoteCleanupInvocation =
        serde_json::from_slice(&bytes).map_err(|error| {
            format!("failed to decode remote cleanup sidecar for {job_id}: {error}")
        })?;
    if invocation.schema_version != 1 {
        return Err(format!(
            "unsupported remote cleanup sidecar schema_version={} for {job_id}",
            invocation.schema_version
        ));
    }
    if invocation.transport != SHELL_REMOTE_TRANSPORT_SSH {
        return Err(format!(
            "unsupported remote cleanup sidecar transport={} for {job_id}",
            invocation.transport
        ));
    }
    if ssh_family_client_for_executable(&invocation.command) != Some("ssh") {
        return Err(format!(
            "remote cleanup sidecar command is not ssh-family for {job_id}: {}",
            invocation.command
        ));
    }
    if ssh_direct_command_parts(&invocation.control_args).is_none() {
        return Err(format!(
            "remote cleanup sidecar control_args do not contain an ssh destination for {job_id}"
        ));
    }
    Ok(Some(invocation))
}

fn write_pretty_json_file<T: Serialize>(
    path: &Path,
    value: &T,
    role: &'static str,
) -> Result<(), ErrorData> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_run_shell failed to encode shell job {role}: {error}"),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "path": path,
                "reason": "job_json_encode_failed",
                "role": role,
            }),
        )
    })?;
    fs::write(path, bytes).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell failed to write shell job {role}: {error}"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "path": path,
                "reason": "job_json_write_failed",
                "role": role,
            }),
        )
    })
}

fn write_shell_job_status(path: &Path, status: &ActRunShellJobStatus) -> Result<(), ErrorData> {
    let safe_status = shell_job_status_with_safe_command_metadata(status);
    let bytes = serde_json::to_vec_pretty(&safe_status).map_err(|error| {
        shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_run_shell failed to encode shell job status: {error}"),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "job_id": safe_status.job_id,
                "path": path,
                "reason": "job_status_encode_failed",
            }),
        )
    })?;
    // Stage to a PER-WRITE UNIQUE sibling temp file, never a shared fixed name.
    // Multiple threads (the background monitor, `act_run_shell_status`
    // reconciliation, and terminal-status persistence) rewrite the same
    // `status.json` concurrently. A shared `status.json.tmp` let two `write_all`
    // calls interleave into the same staging blob — a shorter payload's tail
    // left the previous longer payload's bytes behind, producing the
    // `trailing characters at line N` corruption that was then renamed into
    // place (#1568). A unique name means each writer stages, fsyncs, and renames
    // its OWN complete blob; the rename is atomic, so readers observe either the
    // old or the new whole file — never a half-merged one. (Canonical
    // write→fsync→rename atomic-replace pattern.)
    //
    // Serialize concurrent writers to the SAME destination for the whole
    // stage+commit. Every writer of a given `status.json` lives in this one
    // daemon process, so an in-process per-path lock guarantees only one
    // `MoveFileExW(REPLACE_EXISTING)` targets a destination at a time. Without
    // it, many writers racing the same rename can starve one another past the
    // bounded retry window and fail a status write outright under load (#1568).
    let write_lock = shell_status_write_lock(path);
    let _write_guard = write_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tmp_path = shell_status_temp_path(path);
    if let Err(error) = write_shell_job_status_staging(&tmp_path, &bytes) {
        // Never leak the partial staging file on the write/fsync failure path.
        let _ = fs::remove_file(&tmp_path);
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell failed to write shell job status temp file: {error}"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": tmp_path,
                "reason": "job_status_temp_write_failed",
            }),
        ));
    }
    if let Err(error) = commit_shell_job_status_file(&tmp_path, path, &safe_status.job_id) {
        // The rename never happened, so the staging file is orphaned — remove it.
        let _ = fs::remove_file(&tmp_path);
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell failed to commit shell job status file: {error}"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": path,
                "tmp_path": tmp_path,
                "reason": "job_status_rename_failed",
            }),
        ));
    }
    Ok(())
}

/// Write the fully-serialized status blob to `tmp_path` and flush it to stable
/// storage before the caller renames it over the live status file. The
/// `sync_all` (fsync) is what makes the subsequent atomic rename crash-safe: a
/// power loss after the rename can only expose the fully-durable new blob or the
/// prior one, never a zero-length or partially-flushed file.
fn write_shell_job_status_staging(tmp_path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(tmp_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Deterministic-prefix, per-(process, write) UNIQUE staging path for a status
/// file. The `<name>.tmp.` prefix is what [`shell_status_replace_in_flight`]
/// scans for to tell an in-flight atomic replace apart from a genuinely-missing
/// job; the `<pid>.<seq>` suffix guarantees two concurrent writers — even across
/// daemon processes — never collide on the same staging file.
fn shell_status_temp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "status.json".to_owned());
    path.with_file_name(format!("{base}.tmp.{}.{seq}", std::process::id()))
}

/// In-process serialization lock for status writes to a given destination.
///
/// Uses a fixed pool of stripes hashed by path, so memory is bounded no matter
/// how many distinct jobs a long-running daemon creates (a per-path registry
/// would grow without bound). Writes to the same `status.json` always hash to
/// the same stripe and are therefore serialized; two unrelated paths only share
/// a stripe ~1/N of the time, and the resulting brief extra serialization is
/// harmless because a stage+commit is sub-millisecond.
fn shell_status_write_lock(path: &Path) -> &'static Mutex<()> {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
        sync::OnceLock,
    };
    const STRIPES: usize = 64;
    static LOCKS: OnceLock<Vec<Mutex<()>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| (0..STRIPES).map(|_| Mutex::new(())).collect());
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let index = (hasher.finish() as usize) % STRIPES;
    &locks[index]
}

#[cfg(windows)]
fn commit_shell_job_status_file(tmp_path: &Path, path: &Path, _job_id: &str) -> io::Result<()> {
    use windows::{
        Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION},
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };

    // Transient, retryable Windows lock codes: a background scanner (Windows
    // Defender, the search indexer) or another handle can briefly hold the
    // destination WITHOUT share-delete right after it is created/renamed,
    // bouncing MoveFileExW with ACCESS_DENIED (5) / SHARING_VIOLATION (32). This
    // is the documented AV/indexer transient-lock failure mode, not a real
    // permission error.
    const RETRYABLE_CODES: [u32; 2] = [ERROR_ACCESS_DENIED.0, ERROR_SHARING_VIOLATION.0];
    // Retry by ATTEMPT COUNT, not wall-clock. Under heavy CPU contention a
    // wall-clock budget is spent while this thread is descheduled, so a 500 ms
    // window can yield only ~1 real MoveFileEx call before "expiring" and fail
    // spuriously (observed: a full-suite run starved the retry and a durable
    // status rewrite failed with os error 5). A fixed attempt count guarantees
    // that many real calls regardless of scheduling, with exponential backoff so
    // the transient holder has escalating time to release.
    // A transient AV/indexer lock is typically released within tens of ms, so
    // prefer many short retries over few long ones: fast recovery in the common
    // case, and a ~1s worst-case budget that still survives a starved scheduler.
    const MAX_ATTEMPTS: u32 = 24;
    const BACKOFF_START_MS: u64 = 1;
    const BACKOFF_CAP_MS: u64 = 50;

    let tmp_wide = path_to_nul_terminated_wide(tmp_path);
    let path_wide = path_to_nul_terminated_wide(path);
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let mut backoff_ms = BACKOFF_START_MS;
    for attempt in 1..=MAX_ATTEMPTS {
        // SAFETY: both vectors are NUL-terminated and live for the duration of the call.
        match unsafe { MoveFileExW(PCWSTR(tmp_wide.as_ptr()), PCWSTR(path_wide.as_ptr()), flags) } {
            Ok(()) => return Ok(()),
            Err(error) => {
                let low_code = win32_error_low_code(&error);
                if attempt < MAX_ATTEMPTS && RETRYABLE_CODES.contains(&low_code) {
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = backoff_ms.saturating_mul(2).min(BACKOFF_CAP_MS);
                    continue;
                }
                // Terminal: a non-retryable code, or the retry budget is spent.
                // The caller wraps this with STORAGE_WRITE_FAILED + path/job_id so
                // the exact failing rename is diagnosable.
                return Err(io::Error::from_raw_os_error(low_code as i32));
            }
        }
    }
    Err(io::Error::other(
        "commit_shell_job_status_file exhausted MoveFileEx retries without a terminal result",
    ))
}

#[cfg(windows)]
fn win32_error_low_code(error: &windows::core::Error) -> u32 {
    (error.code().0 as u32) & 0xFFFF
}

#[cfg(windows)]
fn path_to_nul_terminated_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(not(windows))]
fn commit_shell_job_status_file(tmp_path: &Path, path: &Path, _job_id: &str) -> io::Result<()> {
    fs::rename(tmp_path, path)
}

/// Read a durable status file, tolerating the brief window in which a
/// concurrent [`write_shell_job_status`] is swapping the file in.
///
/// On Windows `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` is not observably atomic
/// when the destination is being read concurrently: a reader can transiently
/// see `ERROR_FILE_NOT_FOUND` (2) — the destination momentarily does not exist
/// mid-replace — as well as `ERROR_SHARING_VIOLATION` (32) or
/// `ERROR_ACCESS_DENIED` (5). A status poll, cleanup scan, or dashboard read
/// racing the monitor's frequent status updates must not fail spuriously, so we
/// retry within a bounded window (mirroring the writer's own move retry).
///
/// `NOT_FOUND` is overloaded: it is also the legitimate signal that a job never
/// existed. We disambiguate without penalising the genuine-missing path by
/// checking for any of the writer's `<name>.tmp.*` staging siblings: if one is
/// present a replace is in flight and we retry; if neither the target nor any
/// staging file exists the job is truly absent and we return immediately.
///
/// Open the status file for reading with **`FILE_SHARE_DELETE`** in addition to
/// share-read/write. This is the load-bearing half of the atomic-replace
/// contract on Windows: `std::fs::read` omits `FILE_SHARE_DELETE`, so a reader
/// holding the file open blocks the monitor's `MoveFileExW(REPLACE_EXISTING)`
/// with `ERROR_SHARING_VIOLATION`/`ERROR_ACCESS_DENIED`. Under sustained polling
/// the destination is almost never idle, so the writer's bounded rename retry
/// eventually exhausts and a status rewrite fails outright (#1568, the
/// "surfaced under heavy load" failure). With share-delete the replace proceeds
/// while the reader keeps reading the complete old inode, which Windows keeps
/// alive until the handle closes — so the reader still observes a whole file,
/// never a partial one.
#[cfg(windows)]
fn read_status_file_share_delete(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ (0x1) | FILE_SHARE_WRITE (0x2) | FILE_SHARE_DELETE (0x4).
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x1 | 0x2 | 0x4;
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_WRITE_DELETE)
        .open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(windows)]
fn read_shell_status_bytes(path: &Path) -> io::Result<Vec<u8>> {
    // ERROR_ACCESS_DENIED = 5, ERROR_SHARING_VIOLATION = 32.
    const TRANSIENT_OPEN_CODES: [i32; 2] = [5, 32];
    let started = Instant::now();
    loop {
        match read_status_file_share_delete(path) {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                let within_window = started.elapsed() < Duration::from_millis(500);
                let transient_open = error
                    .raw_os_error()
                    .is_some_and(|code| TRANSIENT_OPEN_CODES.contains(&code));
                // A NOT_FOUND only counts as a transient replace window while a
                // writer's unique staging file is still on disk; otherwise the
                // job is genuinely absent and the error is returned as-is.
                let mid_replace =
                    error.kind() == io::ErrorKind::NotFound && shell_status_replace_in_flight(path);
                if within_window && (transient_open || mid_replace) {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                return Err(error);
            }
        }
    }
}

#[cfg(not(windows))]
fn read_shell_status_bytes(path: &Path) -> io::Result<Vec<u8>> {
    // POSIX `rename(2)` is atomic: a reader always sees either the old or the
    // new inode, never a sharing violation or a missing-file window, so no
    // retry is required.
    fs::read(path)
}

/// Whether any writer currently has a `<name>.tmp.*` staging sibling for `path`
/// on disk — i.e. an atomic replace is mid-flight. Scans the status file's own
/// job directory (which holds only a handful of sidecar files) for the
/// [`shell_status_temp_path`] prefix. Used by [`read_shell_status_bytes`] to
/// keep the NOT_FOUND retry window from ever firing for a genuinely-absent job.
#[cfg(windows)]
fn shell_status_replace_in_flight(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    let base = match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => return false,
    };
    let prefix = format!("{base}.tmp.");
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
}

fn read_shell_job_status(path: &Path, job_id: &str) -> Result<ActRunShellJobStatus, ErrorData> {
    let bytes = read_shell_status_bytes(path).map_err(|error| {
        let code = if error.kind() == io::ErrorKind::NotFound {
            error_codes::TOOL_PARAMS_INVALID
        } else {
            error_codes::STORAGE_READ_FAILED
        };
        let reason = if error.kind() == io::ErrorKind::NotFound {
            "job_not_found"
        } else {
            "job_status_read_failed"
        };
        shell_tool_error(
            code,
            format!("act_run_shell job status could not be read: {error}"),
            json!({
                "code": code,
                "job_id": job_id,
                "path": path,
                "reason": reason,
            }),
        )
    })?;
    let mut job: ActRunShellJobStatus = serde_json::from_slice(&bytes).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell job status JSON is invalid: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "job_id": job_id,
                "path": path,
                "reason": "job_status_decode_failed",
            }),
        )
    })?;
    normalize_shell_job_remote_process_scope(&mut job);
    Ok(shell_job_status_with_safe_command_metadata(&job))
}

fn normalize_shell_job_remote_process_scope(job: &mut ActRunShellJobStatus) {
    if job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_LOCAL {
        return;
    }
    if let Some(client) = ssh_family_client_for_executable(&job.command) {
        let evidence = if client == "ssh" {
            "direct_command_ssh".to_owned()
        } else {
            format!("direct_command_ssh_family:{client}")
        };
        job.remote_process_scope = ssh_remote_process_scope(&job.command, &job.args, evidence);
    }
}

fn shell_job_remote_process_scope_from_start_params(
    params: &ActRunShellStartParams,
) -> ActRunShellRemoteProcessScope {
    if let Some(invocation) = shell_job_remote_scope_invocation(&params.command, &params.args) {
        ssh_remote_process_scope(
            &invocation.command,
            &invocation.args,
            invocation.evidence.to_owned(),
        )
    } else if let Some(client) = ssh_family_client_for_executable(&params.command) {
        ssh_remote_process_scope(
            &params.command,
            &params.args,
            format!("direct_command_ssh_family:{client}"),
        )
    } else {
        ActRunShellRemoteProcessScope::default()
    }
}

fn ssh_remote_process_scope(
    command: &str,
    args: &[String],
    evidence: impl Into<String>,
) -> ActRunShellRemoteProcessScope {
    let client = ssh_family_client_for_executable(command).unwrap_or("ssh");
    let mut cleanup_status = SHELL_REMOTE_CLEANUP_NOT_TRACKED.to_owned();
    let mut detection_evidence = vec![format!("{}:{}", evidence.into(), executable_leaf(command))];
    if client == "ssh" {
        if let Some(parts) = ssh_direct_command_parts(args) {
            if parts.remote_command.is_some() {
                if let Some(reason) = parts.tracking_unsupported_reason {
                    detection_evidence.push(format!("remote_tracking_unsupported:{reason}"));
                } else {
                    cleanup_status = SHELL_REMOTE_CLEANUP_TRACKING_PENDING.to_owned();
                    detection_evidence.push(format!(
                        "remote_tracking_pending:setsid_stderr_marker:{SHELL_REMOTE_PROCESS_MARKER}"
                    ));
                }
            }
        }
    }
    ActRunShellRemoteProcessScope {
        transport: SHELL_REMOTE_TRANSPORT_SSH.to_owned(),
        local_process_scope: "local_ssh_client_process_tree".to_owned(),
        remote_cleanup_required: true,
        remote_cleanup_verified: false,
        remote_cleanup_status: cleanup_status,
        remote_identity: shell_transfer_remote_identity(client, args),
        remote_process_id: None,
        remote_process_group_id: None,
        remote_cleanup_error_code: None,
        remote_cleanup_message: None,
        detection_evidence,
    }
}

fn ssh_family_client_for_executable(command: &str) -> Option<&'static str> {
    let leaf = executable_leaf(command).to_ascii_lowercase();
    match leaf.as_str() {
        "ssh" | "ssh.exe" => Some("ssh"),
        "scp" | "scp.exe" => Some("scp"),
        "sftp" | "sftp.exe" => Some("sftp"),
        _ => None,
    }
}

fn shell_spawn_command(command: &str) -> Cow<'_, str> {
    #[cfg(windows)]
    if let Some(resolved) = resolve_windows_ssh_family_spawn_command(command) {
        tracing::info!(
            code = "M4_ACT_RUN_SHELL_SSH_CLIENT_RESOLVED",
            requested_command = command,
            resolved_command = %resolved,
            "resolved bare Windows SSH-family command to Git-bundled executable"
        );
        return Cow::Owned(resolved);
    }
    Cow::Borrowed(command)
}

#[cfg(windows)]
fn resolve_windows_ssh_family_spawn_command(command: &str) -> Option<String> {
    resolve_windows_ssh_family_spawn_command_with_dirs(command, &windows_git_ssh_dir_candidates())
}

#[cfg(windows)]
fn resolve_windows_ssh_family_spawn_command_with_dirs(
    command: &str,
    candidate_dirs: &[PathBuf],
) -> Option<String> {
    if !is_bare_windows_executable_name(command) {
        return None;
    }
    let client = ssh_family_client_for_executable(command)?;
    for dir in candidate_dirs {
        if !is_known_good_git_ssh_directory(dir) {
            continue;
        }
        let candidate = dir.join(windows_ssh_family_executable_leaf(command, client));
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(windows)]
fn is_bare_windows_executable_name(command: &str) -> bool {
    let command = trim_arg_quotes(command).trim();
    !command.is_empty()
        && !command.contains(['\\', '/'])
        && command.as_bytes().get(1).is_none_or(|value| *value != b':')
        && !Path::new(command).is_absolute()
}

#[cfg(windows)]
fn windows_ssh_family_executable_leaf(command: &str, client: &str) -> String {
    let leaf = executable_leaf(command);
    if Path::new(leaf).extension().is_some() {
        leaf.to_owned()
    } else {
        format!("{client}.exe")
    }
}

#[cfg(windows)]
fn windows_git_ssh_directory() -> Option<PathBuf> {
    windows_git_ssh_dir_candidates()
        .into_iter()
        .find(|dir| is_known_good_git_ssh_directory(dir))
}

#[cfg(windows)]
fn windows_git_ssh_dir_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for key in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(value) = std::env::var_os(key) {
            dirs.push(Path::new(&value).join(WINDOWS_GIT_SSH_RELATIVE_DIR));
        }
    }
    dirs.push(PathBuf::from(r"C:\Program Files\Git\usr\bin"));
    dirs.push(PathBuf::from(r"C:\Program Files (x86)\Git\usr\bin"));

    let mut seen = HashSet::new();
    dirs.into_iter()
        .filter(|dir| seen.insert(normalize_semicolon_path_part(&dir.to_string_lossy())))
        .collect()
}

#[cfg(windows)]
fn is_known_good_git_ssh_directory(dir: &Path) -> bool {
    dir.join("ssh.exe").is_file() && dir.join("scp.exe").is_file()
}

fn executable_leaf(command: &str) -> &str {
    trim_arg_quotes(command)
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(command)
}

fn ssh_remote_identity(args: &[String]) -> Option<String> {
    if let Some(parts) = ssh_direct_command_parts(args) {
        return Some(parts.remote_identity);
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SshCommandParts {
    control_args: Vec<String>,
    remote_identity: String,
    remote_command: Option<String>,
    tracking_unsupported_reason: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SshCommandInvocation {
    command: String,
    args: Vec<String>,
    evidence: &'static str,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ShellRemoteCleanupInvocation {
    schema_version: u32,
    transport: String,
    command: String,
    control_args: Vec<String>,
    remote_identity: String,
    source_evidence: String,
    args_sha256: String,
    created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SshRemoteTrackingPlan {
    spawn_args: Vec<String>,
    remote_identity: String,
    remote_command: String,
    marker: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShellJobSpawnPlan {
    command: String,
    args: Vec<String>,
}

fn shell_job_spawn_plan(params: &ActRunShellStartParams, job_id: &str) -> ShellJobSpawnPlan {
    if let Some(invocation) = shell_job_ssh_command_invocation(&params.command, &params.args) {
        if let Some(plan) = ssh_remote_tracking_plan(&invocation.command, &invocation.args, job_id)
        {
            tracing::info!(
                code = "M4_ACT_RUN_SHELL_SSH_REMOTE_TRACKING_ENABLED",
                job_id,
                remote_identity = %plan.remote_identity,
                marker = %plan.marker,
                source = invocation.evidence,
                remote_command_sha256 = %sha256_hex(plan.remote_command.as_bytes()),
                "act_run_shell_start will capture SSH remote pid/process-group metadata"
            );
            return ShellJobSpawnPlan {
                command: invocation.command,
                args: plan.spawn_args,
            };
        }
    }
    ShellJobSpawnPlan {
        command: params.command.clone(),
        args: params.args.clone(),
    }
}

fn shell_job_ssh_command_invocation(
    command: &str,
    args: &[String],
) -> Option<SshCommandInvocation> {
    if ssh_family_client_for_executable(command) == Some("ssh") {
        return Some(SshCommandInvocation {
            command: command.to_owned(),
            args: args.to_vec(),
            evidence: "direct_command_ssh",
        });
    }
    shell_wrapped_ssh_command_invocation(command, args)
}

fn shell_wrapped_ssh_command_invocation(
    command: &str,
    args: &[String],
) -> Option<SshCommandInvocation> {
    let shell = executable_leaf(command).to_ascii_lowercase();
    let (script, evidence) = match shell.as_str() {
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => (
            powershell_command_script_arg(args)?,
            "shell_wrapped_ssh:powershell",
        ),
        "cmd" | "cmd.exe" => (cmd_command_script_arg(args)?, "shell_wrapped_ssh:cmd"),
        _ => return None,
    };
    // This is an argv splitter, not a shell parser. Preserve wrappers whose
    // escaped quote syntax can change grouping if we strip and rewrite it.
    if shell_wrapped_script_has_unsupported_quote_escaping(script) {
        return None;
    }
    let words = split_single_shell_command_words(script)?;
    let (ssh_command, ssh_args) = words.split_first()?;
    if ssh_family_client_for_executable(ssh_command) != Some("ssh") {
        return None;
    }
    Some(SshCommandInvocation {
        command: ssh_command.clone(),
        args: ssh_args.to_vec(),
        evidence,
    })
}

fn shell_wrapped_script_has_unsupported_quote_escaping(script: &str) -> bool {
    script.contains("\\\"")
        || script.contains("\\'")
        || script.contains("`\"")
        || script.contains("`'")
        || script.contains("^\"")
}

fn powershell_command_script_arg(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let arg = trim_arg_quotes(&args[index]).to_ascii_lowercase();
        match arg.as_str() {
            "-encodedcommand" | "-enc" | "-file" | "-f" => return None,
            "-command" | "-c" => {
                return (index + 2 == args.len()).then(|| args[index + 1].as_str());
            }
            _ => index += 1,
        }
    }
    None
}

fn cmd_command_script_arg(args: &[String]) -> Option<&str> {
    for (index, arg) in args.iter().enumerate() {
        let normalized = trim_arg_quotes(arg).to_ascii_lowercase();
        if matches!(normalized.as_str(), "/c" | "/k") && index + 2 == args.len() {
            return Some(args[index + 1].as_str());
        }
    }
    None
}

fn split_single_shell_command_words(script: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in script.chars() {
        match quote {
            Some(quote_ch) if ch == quote_ch => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            None if matches!(ch, ';' | '|' | '&' | '<' | '>' | '\r' | '\n') => return None,
            None => current.push(ch),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn shell_job_remote_scope_invocation(
    command: &str,
    args: &[String],
) -> Option<SshCommandInvocation> {
    shell_job_ssh_command_invocation(command, args)
        .filter(|invocation| ssh_family_client_for_executable(&invocation.command) == Some("ssh"))
}

fn shell_job_cleanup_invocation(
    job: &ActRunShellJobStatus,
    original_args: Option<&[String]>,
    remote_cleanup: Option<&ShellRemoteCleanupInvocation>,
) -> Option<SshCommandInvocation> {
    if let Some(args) = original_args {
        if let Some(invocation) = shell_job_ssh_command_invocation(&job.command, args) {
            return Some(invocation);
        }
    }
    if let Some(remote_cleanup) = remote_cleanup {
        return Some(SshCommandInvocation {
            command: remote_cleanup.command.clone(),
            args: remote_cleanup.control_args.clone(),
            evidence: "remote_cleanup_sidecar",
        });
    }
    shell_job_ssh_command_invocation(&job.command, &job.args)
}

fn ssh_remote_tracking_plan(
    command: &str,
    args: &[String],
    job_id: &str,
) -> Option<SshRemoteTrackingPlan> {
    if ssh_family_client_for_executable(command) != Some("ssh") {
        return None;
    }
    let parts = ssh_direct_command_parts(args)?;
    if parts.tracking_unsupported_reason.is_some() {
        return None;
    }
    let remote_command = parts.remote_command?;
    if remote_command.trim().is_empty() {
        return None;
    }

    let marker = format!("{SHELL_REMOTE_PROCESS_MARKER} job_id={job_id}");
    let exit_marker = format!("{SHELL_REMOTE_EXIT_MARKER} job_id={job_id}");
    let remote_wrapper = ssh_remote_tracking_command(&marker, &exit_marker, &remote_command);
    let mut spawn_args = parts.control_args.clone();
    spawn_args.push(remote_wrapper);
    Some(SshRemoteTrackingPlan {
        spawn_args,
        remote_identity: parts.remote_identity,
        remote_command,
        marker,
    })
}

fn ssh_remote_tracking_command(marker: &str, exit_marker: &str, remote_command: &str) -> String {
    const SCRIPT: &str = r#"marker=$1
exit_marker=$2
cmd=$3
if ! command -v setsid >/dev/null 2>&1; then
  printf '%s error=setsid_unavailable\n' "$marker" >&2
  exit 127
fi
setsid sh -c "$cmd" &
child=$!
pgid=$child
sid=$(ps -o sid= -p "$child" 2>/dev/null | tr -d '[:space:]' || true)
printf '%s pid=%s pgid=%s sid=%s\n' "$marker" "$child" "$pgid" "$sid" >&2
wait "$child"
rc=$?
printf '%s pid=%s pgid=%s exit_code=%s\n' "$exit_marker" "$child" "$pgid" "$rc" >&2
exit "$rc"
"#;
    format!(
        "sh -c {} synapse-remote-tracker {} {} {}",
        posix_single_quote(SCRIPT),
        posix_single_quote(marker),
        posix_single_quote(exit_marker),
        posix_single_quote(remote_command)
    )
}

fn ssh_direct_command_parts(args: &[String]) -> Option<SshCommandParts> {
    let mut index = 0;
    let mut options_done = false;
    let mut tracking_unsupported_reason = None;
    while index < args.len() {
        let arg = trim_arg_quotes(&args[index]);
        if arg.is_empty() {
            index += 1;
            continue;
        }
        if !options_done && arg == "--" {
            options_done = true;
            index += 1;
            continue;
        }
        if !options_done && arg.starts_with('-') && arg != "-" {
            if tracking_unsupported_reason.is_none() {
                tracking_unsupported_reason = ssh_option_remote_tracking_unsupported_reason(arg);
            }
            index += if ssh_option_consumes_next(arg, args.get(index + 1)) {
                2
            } else {
                1
            };
            continue;
        }
        let remote_command = if index + 1 < args.len() {
            Some(args[index + 1..].join(" "))
        } else {
            None
        };
        return Some(SshCommandParts {
            control_args: args[..=index].to_vec(),
            remote_identity: arg.to_owned(),
            remote_command,
            tracking_unsupported_reason,
        });
    }
    None
}

fn ssh_option_remote_tracking_unsupported_reason(arg: &str) -> Option<&'static str> {
    if ssh_short_option_has_flag(arg, 'N') {
        return Some("ssh_no_remote_command_flag");
    }
    if ssh_short_option_has_flag(arg, 'f') {
        return Some("ssh_backgrounds_before_command");
    }
    if ssh_short_option_has_flag(arg, 's') {
        return Some("ssh_subsystem_requested");
    }
    if ssh_short_option_has_flag(arg, 'W') {
        return Some("ssh_stdio_forwarding_requested");
    }
    if ssh_short_option_has_flag(arg, 'O') {
        return Some("ssh_multiplex_control_command");
    }
    if ssh_short_option_has_flag(arg, 'Q') {
        return Some("ssh_query_command");
    }
    None
}

fn ssh_short_option_has_flag(arg: &str, flag: char) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    !rest.starts_with('-') && rest.chars().any(|ch| ch == flag)
}

fn ssh_option_consumes_next(arg: &str, next: Option<&String>) -> bool {
    if arg.contains('=') || next.is_none() {
        return false;
    }
    matches!(
        arg,
        "-B" | "-b"
            | "-c"
            | "-D"
            | "-E"
            | "-e"
            | "-F"
            | "-I"
            | "-i"
            | "-J"
            | "-L"
            | "-l"
            | "-m"
            | "-O"
            | "-o"
            | "-p"
            | "-Q"
            | "-R"
            | "-S"
            | "-W"
            | "-w"
    )
}

fn ensure_shell_job_remote_scope_from_process_tree(job: &mut ActRunShellJobStatus) {
    if job.remote_process_scope.transport == SHELL_REMOTE_TRANSPORT_SSH {
        return;
    }
    let Some(pid) = job.pid else {
        return;
    };
    let process_ids = shell_job_process_tree_ids(pid);
    let evidence = shell_job_ssh_process_evidence(&process_ids);
    if evidence.is_empty() {
        return;
    }
    job.remote_process_scope = ActRunShellRemoteProcessScope {
        transport: SHELL_REMOTE_TRANSPORT_SSH.to_owned(),
        local_process_scope: "job_owned_process_tree_contains_ssh".to_owned(),
        remote_cleanup_required: true,
        remote_cleanup_verified: false,
        remote_cleanup_status: SHELL_REMOTE_CLEANUP_NOT_TRACKED.to_owned(),
        remote_identity: None,
        remote_process_id: None,
        remote_process_group_id: None,
        remote_cleanup_error_code: None,
        remote_cleanup_message: None,
        detection_evidence: evidence,
    };
}

fn shell_job_ssh_process_evidence(process_ids: &[u32]) -> Vec<String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let pids = process_ids
        .iter()
        .copied()
        .map(Pid::from_u32)
        .collect::<Vec<_>>();
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&pids), true);
    process_ids
        .iter()
        .copied()
        .filter_map(|pid| {
            let process = system.process(Pid::from_u32(pid))?;
            let name = process.name().to_string_lossy();
            let client = ssh_family_client_for_executable(&name)?;
            Some(format!("process_tree_ssh_family:{client}:{pid}:{name}"))
        })
        .collect()
}

fn mark_shell_job_remote_cleanup_unverified(
    job: &mut ActRunShellJobStatus,
    trigger: &'static str,
    local_termination_status: &str,
) {
    if !job.remote_process_scope.remote_cleanup_required {
        return;
    }
    let remote_identity = job
        .remote_process_scope
        .remote_identity
        .as_deref()
        .unwrap_or("unknown_remote");
    let message = format!(
        "{trigger} verified only the local process scope '{}' with local termination status '{local_termination_status}'; SSH remote cleanup for '{remote_identity}' is not tracked or verified because no remote pid/process-group metadata exists in the job status",
        job.remote_process_scope.local_process_scope
    );
    job.remote_process_scope.remote_cleanup_verified = false;
    job.remote_process_scope.remote_cleanup_status = SHELL_REMOTE_CLEANUP_UNVERIFIED.to_owned();
    job.remote_process_scope.remote_cleanup_error_code =
        Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED.to_owned());
    job.remote_process_scope.remote_cleanup_message = Some(message.clone());
    if job.error_code.is_none() {
        job.error_code = Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED.to_owned());
        job.error_message = Some(message);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemotePreMarkerTerminalEvidence {
    reason: &'static str,
    pattern: &'static str,
}

fn mark_shell_job_remote_pre_marker_terminal(
    job: &mut ActRunShellJobStatus,
    trigger: &'static str,
    terminal_status: &str,
    evidence: RemotePreMarkerTerminalEvidence,
) {
    if !job.remote_process_scope.remote_cleanup_required
        || job.remote_process_scope.remote_cleanup_status != SHELL_REMOTE_CLEANUP_TRACKING_PENDING
        || job.remote_process_scope.remote_process_id.is_some()
        || job.remote_process_scope.remote_process_group_id.is_some()
    {
        return;
    }
    let remote_identity = job
        .remote_process_scope
        .remote_identity
        .as_deref()
        .unwrap_or("unknown_remote");
    let suggested_readback = shell_remote_pre_marker_readback_hint(job);
    let message = format!(
        "{trigger} classified SSH remote tracking as pre-marker terminal failure for '{remote_identity}'; terminal_status='{terminal_status}'; reason={}; no {SHELL_REMOTE_PROCESS_MARKER} pid/process-group marker was found, so Synapse did not acquire a remote cleanup handle and will not report remote cleanup as unresolved. suggested_safe_readback={suggested_readback}",
        evidence.reason
    );
    job.remote_process_scope.remote_cleanup_required = false;
    job.remote_process_scope.remote_cleanup_verified = false;
    job.remote_process_scope.remote_cleanup_status =
        SHELL_REMOTE_CLEANUP_PRE_MARKER_TERMINAL.to_owned();
    job.remote_process_scope.remote_cleanup_error_code = None;
    job.remote_process_scope.remote_cleanup_message = Some(message);
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!("remote_tracking_pre_marker_terminal:{}", evidence.reason),
    );
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!("remote_tracking_pre_marker_pattern:{}", evidence.pattern),
    );
    if job.error_code.as_deref() == Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED) {
        job.error_code = None;
        job.error_message = None;
    }
}

fn mark_shell_job_remote_pre_marker_terminal_if_detected(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
    trigger: &'static str,
) -> Result<bool, ErrorData> {
    if job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_SSH
        || job.remote_process_scope.remote_cleanup_status != SHELL_REMOTE_CLEANUP_TRACKING_PENDING
        || job.remote_process_scope.remote_process_id.is_some()
        || job.remote_process_scope.remote_process_group_id.is_some()
    {
        return Ok(false);
    }
    let stderr_prefix =
        read_file_prefix_lossy(&paths.stderr_path, SHELL_REMOTE_METADATA_PREFIX_BYTES)?;
    let stderr_tail = tail_file_lossy(&paths.stderr_path, SHELL_JOB_TAIL_DEFAULT_BYTES as usize)?;
    let evidence = remote_pre_marker_terminal_evidence(&stderr_prefix)
        .or_else(|| remote_pre_marker_terminal_evidence(&stderr_tail));
    let Some(evidence) = evidence else {
        return Ok(false);
    };
    let terminal_status = job.status.clone();
    mark_shell_job_remote_pre_marker_terminal(job, trigger, &terminal_status, evidence);
    Ok(true)
}

fn remote_pre_marker_terminal_evidence(stderr: &str) -> Option<RemotePreMarkerTerminalEvidence> {
    let lower = stderr.to_ascii_lowercase();
    let patterns = [
        (
            "remote_shell_unmatched_quote",
            "unexpected eof while looking for matching",
        ),
        (
            "remote_shell_unexpected_end",
            "syntax error: unexpected end of file",
        ),
        (
            "remote_shell_unterminated_quote",
            "syntax error: unterminated quoted string",
        ),
        (
            "remote_shell_unterminated_quote",
            "unterminated quoted string",
        ),
        ("remote_shell_parse_error", "parse error near"),
    ];
    patterns
        .iter()
        .find(|(_, pattern)| lower.contains(pattern))
        .map(|(reason, pattern)| RemotePreMarkerTerminalEvidence { reason, pattern })
}

fn shell_remote_pre_marker_readback_hint(job: &ActRunShellJobStatus) -> String {
    let remote_identity = job
        .remote_process_scope
        .remote_identity
        .as_deref()
        .unwrap_or("<remote>");
    let remote_command = format!(
        "ps -eo pid,pgid,stat,args | grep -F {} | grep -F {} | grep -v grep",
        shell_word_for_double_quoted_grep(SHELL_REMOTE_PROCESS_MARKER),
        shell_word_for_double_quoted_grep(&job.job_id)
    );
    if let Some(invocation) = shell_job_ssh_command_invocation(&job.command, &job.args) {
        if let Some(parts) = ssh_direct_command_parts(&invocation.args) {
            let mut args = parts.control_args;
            args.push(remote_command);
            return shell_command_line_from_parts(&invocation.command, &args);
        }
    }
    format!(
        "ssh {remote_identity} {}",
        posix_single_quote(&remote_command)
    )
}

fn shell_word_for_double_quoted_grep(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteProcessMetadata {
    job_id: String,
    pid: String,
    pgid: String,
    sid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteExitMetadata {
    job_id: String,
    pid: String,
    pgid: String,
    exit_code: i32,
}

fn refresh_shell_job_remote_metadata_from_outputs(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
) -> Result<bool, ErrorData> {
    if job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_SSH {
        return Ok(false);
    }
    if job.remote_process_scope.remote_process_id.is_some()
        && job.remote_process_scope.remote_process_group_id.is_some()
    {
        return Ok(false);
    }
    let stderr_prefix =
        read_file_prefix_lossy(&paths.stderr_path, SHELL_REMOTE_METADATA_PREFIX_BYTES)?;
    let stderr_tail = tail_file_lossy(&paths.stderr_path, SHELL_JOB_TAIL_DEFAULT_BYTES as usize)?;
    let metadata = parse_remote_process_metadata(&stderr_prefix, &job.job_id)
        .or_else(|| parse_remote_process_metadata(&stderr_tail, &job.job_id));
    let Some(metadata) = metadata else {
        return Ok(false);
    };
    apply_remote_process_metadata(job, metadata);
    Ok(true)
}

fn reconcile_shell_job_remote_exit_marker(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
    running: bool,
    trigger: &'static str,
) -> Result<bool, ErrorData> {
    if job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_SSH
        || job.cancel_requested
        || job.timed_out
    {
        return Ok(false);
    }
    let stderr_prefix =
        read_file_prefix_lossy(&paths.stderr_path, SHELL_REMOTE_METADATA_PREFIX_BYTES)?;
    let stderr_tail = tail_file_lossy(&paths.stderr_path, SHELL_JOB_TAIL_DEFAULT_BYTES as usize)?;
    let metadata = parse_remote_exit_metadata(&stderr_prefix, &job.job_id)
        .or_else(|| parse_remote_exit_metadata(&stderr_tail, &job.job_id));
    let Some(metadata) = metadata else {
        return Ok(false);
    };
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!(
            "remote_exit_marker:{SHELL_REMOTE_EXIT_MARKER}:pid={}:pgid={}:exit_code={}",
            metadata.pid, metadata.pgid, metadata.exit_code
        ),
    );
    if job
        .remote_process_scope
        .remote_process_id
        .as_deref()
        .is_some_and(|pid| pid != metadata.pid)
        || job
            .remote_process_scope
            .remote_process_group_id
            .as_deref()
            .is_some_and(|pgid| pgid != metadata.pgid)
    {
        push_unique_evidence(
            &mut job.remote_process_scope.detection_evidence,
            "remote_exit_marker_ignored:metadata_mismatch".to_owned(),
        );
        return Ok(false);
    }
    if metadata.exit_code != 0 {
        return Ok(false);
    }
    if !running && job.status == "ok" && job.exit_code == Some(0) {
        return Ok(false);
    }
    let termination = if running {
        job.pid.map(terminate_shell_job_process_tree)
    } else {
        None
    };
    let local_termination_status = termination
        .as_ref()
        .map(|readback| readback.status.as_str())
        .unwrap_or("already_exited");
    let remaining_process_ids = termination
        .as_ref()
        .map(|readback| readback.remaining_process_ids.clone())
        .unwrap_or_default();
    mark_shell_job_remote_already_gone_local_stale(
        job,
        trigger,
        local_termination_status,
        &remaining_process_ids,
        Some(metadata.exit_code),
    );
    Ok(true)
}

fn wait_for_shell_job_remote_metadata(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
    timeout: Duration,
) -> Result<bool, ErrorData> {
    let started = Instant::now();
    loop {
        if refresh_shell_job_remote_metadata_from_outputs(job, paths)? {
            return Ok(true);
        }
        if started.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn verify_shell_job_remote_cleanup_after_terminal(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
    trigger: &'static str,
    original_args: Option<&[String]>,
) {
    if !shell_job_terminal_status(&job.status)
        || job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_SSH
        || !job.remote_process_scope.remote_cleanup_required
        || job.remote_process_scope.remote_cleanup_verified
        || job.remote_process_scope.remote_cleanup_status == SHELL_REMOTE_CLEANUP_FAILED
        || job.remote_process_scope.remote_cleanup_status == SHELL_REMOTE_CLEANUP_NOT_TRACKED
    {
        return;
    }

    if matches!(
        job.remote_process_scope.remote_cleanup_status.as_str(),
        SHELL_REMOTE_CLEANUP_TRANSPORT_LOST
    ) {
        return;
    }

    if let Err(error) = refresh_shell_job_remote_metadata_from_outputs(job, paths) {
        mark_shell_job_remote_cleanup_failed(
            job,
            trigger,
            "remote_metadata_read_failed",
            &format!("{error:?}"),
        );
        return;
    }

    if job.remote_process_scope.remote_process_id.is_some()
        && job.remote_process_scope.remote_process_group_id.is_some()
    {
        match mark_shell_job_remote_transport_lost_if_detected(job, paths, trigger) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                mark_shell_job_remote_cleanup_failed(
                    job,
                    trigger,
                    "remote_transport_loss_read_failed",
                    &format!("{error:?}"),
                );
                return;
            }
        }
        let _ = attempt_shell_job_remote_cleanup(job, paths, trigger, original_args);
        return;
    }

    if job.remote_process_scope.remote_cleanup_status == SHELL_REMOTE_CLEANUP_TRACKING_PENDING {
        match mark_shell_job_remote_pre_marker_terminal_if_detected(job, paths, trigger) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                mark_shell_job_remote_cleanup_failed(
                    job,
                    trigger,
                    "pre_marker_stderr_read_failed",
                    &format!("{error:?}"),
                );
                return;
            }
        }
        let terminal_status = job.status.clone();
        mark_shell_job_remote_cleanup_unverified(job, trigger, &terminal_status);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteTransportLostEvidence {
    reason: &'static str,
    pattern: &'static str,
}

fn mark_shell_job_remote_transport_lost_if_detected(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
    trigger: &'static str,
) -> Result<bool, ErrorData> {
    if job.cancel_requested
        || job.timed_out
        || job.status != "exit_nonzero"
        || job.exit_code != Some(255)
    {
        return Ok(false);
    }
    let stderr_prefix =
        read_file_prefix_lossy(&paths.stderr_path, SHELL_REMOTE_METADATA_PREFIX_BYTES)?;
    let stderr_tail = tail_file_lossy(&paths.stderr_path, SHELL_JOB_TAIL_DEFAULT_BYTES as usize)?;
    let evidence = remote_transport_lost_evidence(&stderr_prefix)
        .or_else(|| remote_transport_lost_evidence(&stderr_tail));
    let Some(evidence) = evidence else {
        return Ok(false);
    };
    mark_shell_job_remote_transport_lost(job, trigger, evidence);
    Ok(true)
}

fn remote_transport_lost_evidence(stderr: &str) -> Option<RemoteTransportLostEvidence> {
    let lower = stderr.to_ascii_lowercase();
    let patterns = [
        ("ssh_connection_reset", "connection reset by peer"),
        ("ssh_client_loop_disconnect", "client_loop: send disconnect"),
        ("ssh_broken_pipe", "broken pipe"),
        ("ssh_connection_timed_out", "connection timed out"),
        ("ssh_operation_timed_out", "operation timed out"),
        ("ssh_network_unreachable", "network is unreachable"),
        ("ssh_connection_closed", "connection closed by remote host"),
        ("ssh_closed_by_remote_host", "closed by remote host"),
    ];
    patterns
        .iter()
        .find(|(_, pattern)| lower.contains(pattern))
        .map(|(reason, pattern)| RemoteTransportLostEvidence { reason, pattern })
}

fn mark_shell_job_remote_transport_lost(
    job: &mut ActRunShellJobStatus,
    trigger: &'static str,
    evidence: RemoteTransportLostEvidence,
) {
    let remote_identity = job
        .remote_process_scope
        .remote_identity
        .as_deref()
        .unwrap_or("unknown_remote");
    let pid = job
        .remote_process_scope
        .remote_process_id
        .as_deref()
        .unwrap_or("unknown_pid");
    let pgid = job
        .remote_process_scope
        .remote_process_group_id
        .as_deref()
        .unwrap_or("unknown_pgid");
    let message = format!(
        "{trigger} classified SSH transport loss after the remote process marker for '{remote_identity}'; local ssh exit_code=255 matched {}; Synapse did not run remote cleanup because cancel_requested=false and timed_out=false. Remote pid {pid}, process group {pgid} may still be running; call act_run_shell_cancel for explicit remote cleanup.",
        evidence.reason
    );
    job.status = SHELL_JOB_STATUS_REMOTE_TRANSPORT_LOST.to_owned();
    job.remote_process_scope.remote_cleanup_required = true;
    job.remote_process_scope.remote_cleanup_verified = false;
    job.remote_process_scope.remote_cleanup_status = SHELL_REMOTE_CLEANUP_TRANSPORT_LOST.to_owned();
    job.remote_process_scope.remote_cleanup_error_code = None;
    job.remote_process_scope.remote_cleanup_message = Some(message.clone());
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!("remote_transport_lost:{}", evidence.reason),
    );
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!("remote_transport_lost_pattern:{}", evidence.pattern),
    );
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        "remote_cleanup_deferred_until_explicit_cancel".to_owned(),
    );
    if job.error_code.as_deref() == Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED) {
        job.error_code = None;
    }
    job.error_message = Some(message);
}

fn reconcile_shell_job_remote_already_gone_if_local_stale(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
    running: bool,
    trigger: &'static str,
) -> bool {
    if !running
        || !shell_job_live_status(&job.status)
        || job.cancel_requested
        || job.timed_out
        || job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_SSH
        || !job.remote_process_scope.remote_cleanup_required
        || job.remote_process_scope.remote_cleanup_verified
        || matches!(
            job.remote_process_scope.remote_cleanup_status.as_str(),
            SHELL_REMOTE_CLEANUP_TRANSPORT_LOST
                | SHELL_REMOTE_CLEANUP_FAILED
                | SHELL_REMOTE_CLEANUP_NOT_TRACKED
        )
    {
        return false;
    }
    let Some(pid) = job.remote_process_scope.remote_process_id.clone() else {
        return false;
    };
    let Some(pgid) = job.remote_process_scope.remote_process_group_id.clone() else {
        return false;
    };
    if !valid_remote_process_number(&pid) || !valid_remote_process_number(&pgid) {
        push_unique_evidence(
            &mut job.remote_process_scope.detection_evidence,
            "remote_liveness_probe_skipped:invalid_metadata".to_owned(),
        );
        return false;
    }
    let Some(remote_status) = probe_shell_job_remote_liveness(job, paths, &pid, &pgid) else {
        return false;
    };
    if remote_status != "already_gone" {
        return false;
    }
    let termination = job.pid.map(terminate_shell_job_process_tree);
    let local_termination_status = termination
        .as_ref()
        .map(|readback| readback.status.as_str())
        .unwrap_or("pid_unavailable");
    let remaining_process_ids = termination
        .as_ref()
        .map(|readback| readback.remaining_process_ids.clone())
        .unwrap_or_default();
    mark_shell_job_remote_already_gone_local_stale(
        job,
        trigger,
        local_termination_status,
        &remaining_process_ids,
        None,
    );
    true
}

fn probe_shell_job_remote_liveness(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
    pid: &str,
    pgid: &str,
) -> Option<String> {
    let remote_cleanup = match read_shell_remote_cleanup_invocation(paths, &job.job_id) {
        Ok(remote_cleanup) => remote_cleanup,
        Err(_) => {
            push_unique_evidence(
                &mut job.remote_process_scope.detection_evidence,
                "remote_liveness_probe_failed:sidecar_unreadable".to_owned(),
            );
            return None;
        }
    };
    let Some(invocation) = shell_job_cleanup_invocation(job, None, remote_cleanup.as_ref()) else {
        push_unique_evidence(
            &mut job.remote_process_scope.detection_evidence,
            "remote_liveness_probe_failed:ssh_destination_unavailable".to_owned(),
        );
        return None;
    };
    let Some(parts) = ssh_direct_command_parts(&invocation.args) else {
        push_unique_evidence(
            &mut job.remote_process_scope.detection_evidence,
            "remote_liveness_probe_failed:ssh_destination_unavailable".to_owned(),
        );
        return None;
    };
    let mut liveness_args = parts.control_args;
    liveness_args.push(ssh_remote_liveness_command(pid, pgid));
    let readback = match run_shell_cleanup_command_with_timeout(
        &invocation.command,
        &liveness_args,
        Duration::from_millis(SHELL_REMOTE_LIVENESS_TIMEOUT_MS),
    ) {
        Ok(readback) => readback,
        Err(_) => {
            push_unique_evidence(
                &mut job.remote_process_scope.detection_evidence,
                "remote_liveness_probe_failed:command_failed".to_owned(),
            );
            return None;
        }
    };
    let Some(status) = parse_remote_liveness_status(&readback.stdout, pid, pgid) else {
        push_unique_evidence(
            &mut job.remote_process_scope.detection_evidence,
            "remote_liveness_probe_failed:marker_unrecognized".to_owned(),
        );
        return None;
    };
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!(
            "remote_liveness_marker:{SHELL_REMOTE_LIVENESS_MARKER}:pgid={pgid}:status={status}"
        ),
    );
    Some(status)
}

fn mark_shell_job_remote_already_gone_local_stale(
    job: &mut ActRunShellJobStatus,
    trigger: &'static str,
    local_termination_status: &str,
    remaining_process_ids: &[u32],
    remote_exit_code: Option<i32>,
) {
    let remote_identity = job
        .remote_process_scope
        .remote_identity
        .as_deref()
        .unwrap_or("unknown_remote");
    let pid = job
        .remote_process_scope
        .remote_process_id
        .as_deref()
        .unwrap_or("unknown_pid");
    let pgid = job
        .remote_process_scope
        .remote_process_group_id
        .as_deref()
        .unwrap_or("unknown_pgid");
    let exit_message = remote_exit_code
        .map(|exit_code| format!(" Remote exit code from {SHELL_REMOTE_EXIT_MARKER}={exit_code}."))
        .unwrap_or_else(|| " Remote exit code is unavailable from the stale transport.".to_owned());
    let message = format!(
        "{trigger} verified remote pid {pid}, process group {pgid} on '{remote_identity}' is already gone while the local SSH transport was still live or reported a mismatched terminal state; local process-tree termination status={local_termination_status}.{exit_message}"
    );
    job.status = SHELL_JOB_STATUS_REMOTE_EXITED_LOCAL_STALE.to_owned();
    job.completed_at
        .get_or_insert_with(|| chrono::Utc::now().to_rfc3339());
    job.duration_ms
        .get_or_insert_with(|| elapsed_ms_since_rfc3339(&job.started_at).unwrap_or_default());
    job.exit_code = remote_exit_code;
    job.remote_process_scope.remote_cleanup_required = false;
    job.remote_process_scope.remote_cleanup_verified = true;
    job.remote_process_scope.remote_cleanup_status = SHELL_REMOTE_CLEANUP_ALREADY_GONE.to_owned();
    job.remote_process_scope.remote_cleanup_error_code = None;
    job.remote_process_scope.remote_cleanup_message = Some(message.clone());
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        "remote_process_already_gone_before_cancel".to_owned(),
    );
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!("local_transport_stale_termination:{local_termination_status}"),
    );
    if remaining_process_ids.is_empty() {
        if job.error_code.as_deref() == Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
        {
            job.error_code = None;
        }
        job.error_message = Some(message);
    } else {
        let remaining = remaining_process_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        job.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
        job.error_message = Some(format!(
            "{message} Local stale transport still has remaining process ids: {remaining}"
        ));
    }
}

fn parse_remote_process_metadata(
    stderr: &str,
    expected_job_id: &str,
) -> Option<RemoteProcessMetadata> {
    for line in stderr.lines() {
        let Some(marker_index) = line.find(SHELL_REMOTE_PROCESS_MARKER) else {
            continue;
        };
        let rest = &line[marker_index + SHELL_REMOTE_PROCESS_MARKER.len()..];
        let fields = parse_marker_fields(rest);
        let job_id = fields.get("job_id")?;
        if job_id != expected_job_id {
            continue;
        }
        let pid = fields.get("pid")?;
        let pgid = fields.get("pgid")?;
        if !valid_remote_process_number(pid) || !valid_remote_process_number(pgid) {
            continue;
        }
        let sid = fields
            .get("sid")
            .filter(|value| valid_remote_process_number(value))
            .cloned();
        return Some(RemoteProcessMetadata {
            job_id: job_id.clone(),
            pid: pid.clone(),
            pgid: pgid.clone(),
            sid,
        });
    }
    None
}

fn parse_remote_exit_metadata(stderr: &str, expected_job_id: &str) -> Option<RemoteExitMetadata> {
    let mut found = None;
    for line in stderr.lines() {
        let Some(marker_index) = line.find(SHELL_REMOTE_EXIT_MARKER) else {
            continue;
        };
        let rest = &line[marker_index + SHELL_REMOTE_EXIT_MARKER.len()..];
        let fields = parse_marker_fields(rest);
        let job_id = fields.get("job_id")?;
        if job_id != expected_job_id {
            continue;
        }
        let pid = fields.get("pid")?;
        let pgid = fields.get("pgid")?;
        let exit_code = fields.get("exit_code")?.parse::<i32>().ok()?;
        if !valid_remote_process_number(pid) || !valid_remote_process_number(pgid) {
            continue;
        }
        found = Some(RemoteExitMetadata {
            job_id: job_id.clone(),
            pid: pid.clone(),
            pgid: pgid.clone(),
            exit_code,
        });
    }
    found
}

fn apply_remote_process_metadata(job: &mut ActRunShellJobStatus, metadata: RemoteProcessMetadata) {
    job.remote_process_scope.remote_process_id = Some(metadata.pid.clone());
    job.remote_process_scope.remote_process_group_id = Some(metadata.pgid.clone());
    job.remote_process_scope.remote_cleanup_verified = false;
    job.remote_process_scope.remote_cleanup_status = SHELL_REMOTE_CLEANUP_TRACKED.to_owned();
    job.remote_process_scope.remote_cleanup_error_code = None;
    job.remote_process_scope.remote_cleanup_message = Some(format!(
        "SSH remote process group tracked for cleanup: job_id={} remote_pid={} remote_pgid={}",
        metadata.job_id, metadata.pid, metadata.pgid
    ));
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!(
            "remote_process_marker:{SHELL_REMOTE_PROCESS_MARKER}:pid={}:pgid={}",
            metadata.pid, metadata.pgid
        ),
    );
    if let Some(sid) = metadata.sid {
        push_unique_evidence(
            &mut job.remote_process_scope.detection_evidence,
            format!("remote_session_id:{sid}"),
        );
    }
}

fn push_unique_evidence(evidence: &mut Vec<String>, value: String) {
    if !evidence.iter().any(|existing| existing == &value) {
        evidence.push(value);
    }
}

fn parse_marker_fields(rest: &str) -> BTreeMap<String, String> {
    rest.split_whitespace()
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn valid_remote_process_number(value: &str) -> bool {
    value
        .parse::<u32>()
        .is_ok_and(|parsed| parsed > 1 && value.chars().all(|ch| ch.is_ascii_digit()))
}

fn attempt_shell_job_remote_cleanup(
    job: &mut ActRunShellJobStatus,
    paths: &ShellJobPaths,
    trigger: &'static str,
    original_args: Option<&[String]>,
) -> Option<String> {
    if job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_SSH
        || !job.remote_process_scope.remote_cleanup_required
        || job.remote_process_scope.remote_cleanup_verified
    {
        return None;
    }
    let Some(pid) = job.remote_process_scope.remote_process_id.clone() else {
        return None;
    };
    let Some(pgid) = job.remote_process_scope.remote_process_group_id.clone() else {
        return None;
    };
    if !valid_remote_process_number(&pid) || !valid_remote_process_number(&pgid) {
        mark_shell_job_remote_cleanup_failed(
            job,
            trigger,
            "remote_process_metadata_invalid",
            "remote pid/process-group metadata was present but failed validation",
        );
        return Some("remote_cleanup_metadata_invalid".to_owned());
    }
    let remote_cleanup = match read_shell_remote_cleanup_invocation(paths, &job.job_id) {
        Ok(remote_cleanup) => remote_cleanup,
        Err(message) => {
            mark_shell_job_remote_cleanup_failed(
                job,
                trigger,
                "remote_cleanup_sidecar_unreadable",
                &message,
            );
            return Some("remote_cleanup_sidecar_unreadable".to_owned());
        }
    };
    let Some(invocation) =
        shell_job_cleanup_invocation(job, original_args, remote_cleanup.as_ref())
    else {
        mark_shell_job_remote_cleanup_failed(
            job,
            trigger,
            "ssh_destination_unavailable",
            "remote process metadata exists but the original SSH destination could not be parsed",
        );
        return Some("remote_cleanup_destination_unavailable".to_owned());
    };
    let Some(parts) = ssh_direct_command_parts(&invocation.args) else {
        mark_shell_job_remote_cleanup_failed(
            job,
            trigger,
            "ssh_destination_unavailable",
            "remote process metadata exists but the original SSH destination could not be parsed",
        );
        return Some("remote_cleanup_destination_unavailable".to_owned());
    };
    let cleanup_command = ssh_remote_cleanup_command(&pid, &pgid);
    let mut cleanup_args = parts.control_args;
    cleanup_args.push(cleanup_command);
    let output = run_shell_cleanup_command_with_timeout(
        &invocation.command,
        &cleanup_args,
        Duration::from_millis(SHELL_REMOTE_CLEANUP_TIMEOUT_MS),
    );
    let readback = match output {
        Ok(readback) => readback,
        Err(message) => {
            mark_shell_job_remote_cleanup_failed(job, trigger, "cleanup_command_failed", &message);
            return Some("remote_cleanup_command_failed".to_owned());
        }
    };
    let cleanup_status = parse_remote_cleanup_status(&readback.stdout, &pid, &pgid);
    match cleanup_status.as_deref() {
        Some(status @ ("already_gone" | "terminated" | "killed")) => {
            job.remote_process_scope.remote_cleanup_verified = true;
            job.remote_process_scope.remote_cleanup_status =
                SHELL_REMOTE_CLEANUP_VERIFIED.to_owned();
            job.remote_process_scope.remote_cleanup_error_code = None;
            job.remote_process_scope.remote_cleanup_message = Some(format!(
                "{trigger} verified SSH remote cleanup for remote pid {pid}, process group {pgid}; cleanup command status={status}"
            ));
            push_unique_evidence(
                &mut job.remote_process_scope.detection_evidence,
                format!("remote_cleanup_marker:{SHELL_REMOTE_CLEANUP_MARKER}:pgid={pgid}"),
            );
            if job.error_code.as_deref()
                == Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
            {
                job.error_code = None;
                job.error_message = None;
            }
            Some("remote_cleanup_verified".to_owned())
        }
        Some("still_running") => {
            mark_shell_job_remote_cleanup_failed(
                job,
                trigger,
                "remote_process_still_running",
                &format!(
                    "SSH remote cleanup command returned still_running for pid {pid}, pgid {pgid}"
                ),
            );
            Some("remote_cleanup_still_running".to_owned())
        }
        _ => {
            mark_shell_job_remote_cleanup_failed(
                job,
                trigger,
                "cleanup_readback_unrecognized",
                &format!(
                    "SSH remote cleanup command did not produce a verified cleanup marker; exit={:?}; stdout_sha256={}; stderr_sha256={}; stdout_excerpt={:?}; stderr_excerpt={:?}",
                    readback.exit_code,
                    sha256_hex(readback.stdout.as_bytes()),
                    sha256_hex(readback.stderr.as_bytes()),
                    shell_cleanup_output_excerpt(&readback.stdout),
                    shell_cleanup_output_excerpt(&readback.stderr)
                ),
            );
            Some("remote_cleanup_readback_unrecognized".to_owned())
        }
    }
}

fn mark_shell_job_remote_cleanup_failed(
    job: &mut ActRunShellJobStatus,
    trigger: &'static str,
    reason: &'static str,
    detail: &str,
) {
    let remote_identity = job
        .remote_process_scope
        .remote_identity
        .as_deref()
        .unwrap_or("unknown_remote");
    let pid = job
        .remote_process_scope
        .remote_process_id
        .as_deref()
        .unwrap_or("unknown_pid");
    let pgid = job
        .remote_process_scope
        .remote_process_group_id
        .as_deref()
        .unwrap_or("unknown_pgid");
    let message = format!(
        "{trigger} could not verify SSH remote cleanup for {remote_identity}; remote_pid={pid}; remote_pgid={pgid}; reason={reason}; detail={detail}"
    );
    job.remote_process_scope.remote_cleanup_verified = false;
    job.remote_process_scope.remote_cleanup_status = SHELL_REMOTE_CLEANUP_FAILED.to_owned();
    job.remote_process_scope.remote_cleanup_error_code =
        Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED.to_owned());
    job.remote_process_scope.remote_cleanup_message = Some(message.clone());
    job.error_code = Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED.to_owned());
    job.error_message = Some(message);
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CleanupCommandReadback {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn shell_cleanup_output_excerpt(value: &str) -> String {
    const MAX_CHARS: usize = 512;
    let normalized = value.replace('\r', "\\r").replace('\n', "\\n");
    let mut chars = normalized.chars();
    let excerpt: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{excerpt}...[truncated]")
    } else {
        excerpt
    }
}

fn run_shell_cleanup_command_with_timeout(
    command: &str,
    args: &[String],
    timeout: Duration,
) -> Result<CleanupCommandReadback, String> {
    let spawn_command = shell_spawn_command(command);
    let mut child = StdCommand::new(spawn_command.as_ref());
    child
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_no_window_std(&mut child);
    let mut child = child
        .spawn()
        .map_err(|error| format!("spawn cleanup ssh failed: {error}"))?;
    let started = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|error| format!("poll cleanup ssh failed: {error}"))?
        {
            Some(_status) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("read cleanup ssh output failed: {error}"))?;
                return Ok(CleanupCommandReadback {
                    exit_code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "cleanup ssh timed out after {} ms",
                    timeout.as_millis()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn ssh_remote_cleanup_command(pid: &str, pgid: &str) -> String {
    const SCRIPT: &str = r#"pid=$1
pgid=$2
case "$pid:$pgid" in
  *[!0123456789:]*|:*|*:)
    printf '%s pid=%s pgid=%s status=invalid_metadata\n' SYNAPSE_REMOTE_CLEANUP_V1 "$pid" "$pgid"
    exit 2
    ;;
esac
if ! kill -0 "$pid" 2>/dev/null; then
  printf '%s pid=%s pgid=%s status=already_gone\n' SYNAPSE_REMOTE_CLEANUP_V1 "$pid" "$pgid"
  exit 0
fi
kill -TERM -"$pgid" 2>/dev/null || true
i=0
while [ "$i" -lt 25 ]; do
  if ! kill -0 "$pid" 2>/dev/null; then
    printf '%s pid=%s pgid=%s status=terminated\n' SYNAPSE_REMOTE_CLEANUP_V1 "$pid" "$pgid"
    exit 0
  fi
  i=$((i + 1))
  sleep 0.2
done
kill -KILL -"$pgid" 2>/dev/null || true
i=0
while [ "$i" -lt 25 ]; do
  if ! kill -0 "$pid" 2>/dev/null; then
    printf '%s pid=%s pgid=%s status=killed\n' SYNAPSE_REMOTE_CLEANUP_V1 "$pid" "$pgid"
    exit 0
  fi
  i=$((i + 1))
  sleep 0.2
done
printf '%s pid=%s pgid=%s status=still_running\n' SYNAPSE_REMOTE_CLEANUP_V1 "$pid" "$pgid"
exit 1
"#;
    format!(
        "sh -c {} synapse-remote-cleanup {} {}",
        posix_single_quote(SCRIPT),
        posix_single_quote(pid),
        posix_single_quote(pgid)
    )
}

fn ssh_remote_liveness_command(pid: &str, pgid: &str) -> String {
    const SCRIPT: &str = r#"pid=$1
pgid=$2
case "$pid:$pgid" in
  *[!0123456789:]*|:*|*:)
    printf '%s pid=%s pgid=%s status=invalid_metadata\n' SYNAPSE_REMOTE_LIVENESS_V1 "$pid" "$pgid"
    exit 2
    ;;
esac
actual_pgid=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d '[:space:]' || true)
if [ "$actual_pgid" = "$pgid" ]; then
  printf '%s pid=%s pgid=%s status=alive\n' SYNAPSE_REMOTE_LIVENESS_V1 "$pid" "$pgid"
else
  printf '%s pid=%s pgid=%s status=already_gone\n' SYNAPSE_REMOTE_LIVENESS_V1 "$pid" "$pgid"
fi
"#;
    format!(
        "sh -c {} synapse-remote-liveness {} {}",
        posix_single_quote(SCRIPT),
        posix_single_quote(pid),
        posix_single_quote(pgid)
    )
}

fn parse_remote_cleanup_status(
    stdout: &str,
    expected_pid: &str,
    expected_pgid: &str,
) -> Option<String> {
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix(SHELL_REMOTE_CLEANUP_MARKER) else {
            continue;
        };
        let fields = parse_marker_fields(rest);
        if fields.get("pid").map(String::as_str) != Some(expected_pid) {
            continue;
        }
        if fields.get("pgid").map(String::as_str) != Some(expected_pgid) {
            continue;
        }
        return fields.get("status").cloned();
    }
    None
}

fn parse_remote_liveness_status(
    stdout: &str,
    expected_pid: &str,
    expected_pgid: &str,
) -> Option<String> {
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix(SHELL_REMOTE_LIVENESS_MARKER) else {
            continue;
        };
        let fields = parse_marker_fields(rest);
        if fields.get("pid").map(String::as_str) != Some(expected_pid) {
            continue;
        }
        if fields.get("pgid").map(String::as_str) != Some(expected_pgid) {
            continue;
        }
        return fields.get("status").cloned();
    }
    None
}

fn remote_aware_termination_status(
    local_termination_status: &str,
    remote_process_scope: &ActRunShellRemoteProcessScope,
) -> String {
    if !remote_process_scope.remote_cleanup_required {
        return local_termination_status.to_owned();
    }
    if remote_process_scope.remote_cleanup_verified {
        return match local_termination_status {
            "terminated" => "local_ssh_client_terminated_remote_cleanup_verified".to_owned(),
            "already_exited" => {
                "local_ssh_client_already_exited_remote_cleanup_verified".to_owned()
            }
            "pid_unavailable" => {
                "local_ssh_client_pid_unavailable_remote_cleanup_verified".to_owned()
            }
            other => format!("{other}:remote_cleanup_verified"),
        };
    }
    match local_termination_status {
        "terminated" => "local_ssh_client_terminated_remote_cleanup_unverified".to_owned(),
        "already_exited" => "local_ssh_client_already_exited_remote_cleanup_unverified".to_owned(),
        "pid_unavailable" => {
            "local_ssh_client_pid_unavailable_remote_cleanup_unverified".to_owned()
        }
        other => format!("{other}:remote_cleanup_unverified"),
    }
}

fn shell_job_status_record(
    job_id: &str,
    status: &str,
    params: &ActRunShellStartParams,
    paths: &ShellJobPaths,
    request_sha256: &str,
    authorization: &RunShellAuthorization,
    started_at: String,
    pid: Option<u32>,
    context: Option<&ShellExecutionContext>,
) -> ActRunShellJobStatus {
    let status = ActRunShellJobStatus {
        schema_version: 4,
        job_id: job_id.to_owned(),
        session_id: context.map(|context| context.session_id().to_owned()),
        status: status.to_owned(),
        pid,
        command: params.command.clone(),
        command_metadata_policy: "legacy_raw".to_owned(),
        args: params.args.clone(),
        command_line: authorization.command_line.clone(),
        args_redacted: false,
        command_line_redacted: false,
        args_original_count: 0,
        args_original_bytes: 0,
        args_sha256: String::new(),
        command_line_original_bytes: 0,
        command_line_sha256: String::new(),
        working_dir: params.working_dir.clone(),
        session_dir: context.map(|context| path_string(context.session_dir())),
        effective_working_dir: params.working_dir.clone(),
        env_keys: params.env.keys().cloned().collect(),
        session_env_keys: context.map_or_else(Vec::new, shell_session_env_keys),
        timeout_ms: params.timeout_ms,
        started_at,
        completed_at: None,
        duration_ms: None,
        exit_code: None,
        timed_out: false,
        cancel_requested: false,
        error_code: None,
        error_message: None,
        stdout_path: path_string(&paths.stdout_path),
        stderr_path: path_string(&paths.stderr_path),
        status_path: path_string(&paths.status_path),
        request_sha256: request_sha256.to_owned(),
        matched_pattern: authorization.matched_pattern.clone(),
        remote_process_scope: shell_job_remote_process_scope_from_start_params(params),
        diagnostics: None,
    };
    shell_job_status_with_safe_command_metadata(&status)
}

fn open_shell_job_output(
    path: &Path,
    stream: &'static str,
    job_id: &str,
) -> Result<fs::File, ErrorData> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            shell_tool_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!("act_run_shell_start failed to open shell job {stream} log: {error}"),
                json!({
                    "code": error_codes::STORAGE_WRITE_FAILED,
                    "job_id": job_id,
                    "path": path,
                    "stream": stream,
                    "reason": "job_output_open_failed",
                }),
            )
        })
}

fn spawn_shell_job_child(
    params: &ActRunShellStartParams,
    job_id: &str,
    stdout_file: fs::File,
    stderr_file: fs::File,
    context: Option<&ShellExecutionContext>,
) -> Result<SpawnedShellChild, ErrorData> {
    let spawn_plan = shell_job_spawn_plan(params, job_id);
    let spawn_command = shell_spawn_command(&spawn_plan.command);
    let mut command = TokioCommand::new(spawn_command.as_ref());
    command.args(&spawn_plan.args);
    if let Some(working_dir) = &params.working_dir {
        command.current_dir(working_dir);
    }
    command.env_clear();
    let mut env = child_base_environment();
    ensure_child_temp_environment(&mut env);
    validate_child_base_environment(&env, "act_run_shell")?;
    for (_sort_key, (key, value)) in env {
        command.env(key, value);
    }
    command.envs(&params.env);
    apply_shell_session_environment(&mut command, params.working_dir.as_deref(), context);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(false);
    apply_no_window_tokio(&mut command);

    let mut child = command.spawn().map_err(|error| {
        let command_metadata = shell_command_metadata(&params.command, &params.args);
        shell_tool_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("act_run_shell_start failed to spawn command: {error}"),
            json!({
                "code": error_codes::ACTION_TARGET_INVALID,
                "command": params.command,
                "spawn_command": spawn_command.as_ref(),
                "spawn_command_resolved": spawn_command.as_ref() != params.command.as_str(),
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": command_metadata.args,
                "args_redacted": command_metadata.args_redacted,
                "args_original_count": command_metadata.args_original_count,
                "args_original_bytes": command_metadata.args_original_bytes,
                "args_sha256": command_metadata.args_sha256,
                "command_line": command_metadata.command_line,
                "command_line_redacted": command_metadata.command_line_redacted,
                "command_line_original_bytes": command_metadata.command_line_original_bytes,
                "command_line_sha256": command_metadata.command_line_sha256,
                "working_dir": params.working_dir,
                "reason": "spawn_failed",
            }),
        )
    })?;
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        return Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell_start spawned a child process but could not read its pid",
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "command": params.command,
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": shell_command_metadata(&params.command, &params.args).args,
                "args_sha256": shell_args_sha256(&params.args),
                "working_dir": params.working_dir,
                "reason": "pid_unavailable",
            }),
        ));
    };
    let process_job =
        assign_owned_process_job(pid, "act_run_shell_start", params.job_id.as_deref())?;
    Ok(SpawnedShellChild { child, process_job })
}

fn apply_shell_session_environment(
    command: &mut TokioCommand,
    effective_working_dir: Option<&str>,
    context: Option<&ShellExecutionContext>,
) {
    let Some(context) = context else {
        return;
    };
    command.env(SHELL_SESSION_ID_ENV, context.session_id());
    command.env(SHELL_SESSION_DIR_ENV, context.session_dir());
    if let Some(working_dir) = effective_working_dir {
        command.env(SHELL_WORKING_DIR_ENV, working_dir);
    }
}

fn shell_session_env_keys(_context: &ShellExecutionContext) -> Vec<String> {
    SHELL_RESERVED_ENV_KEYS
        .iter()
        .map(|key| (*key).to_owned())
        .collect()
}

async fn monitor_shell_job(
    mut child: tokio::process::Child,
    _process_job: OwnedProcessJob,
    mut status: ActRunShellJobStatus,
    paths: ShellJobPaths,
    started: Instant,
    original_args: Vec<String>,
) {
    let (exit_code, timed_out, wait_error) =
        wait_shell_job_child(&mut child, status.timeout_ms).await;
    if let Ok(latest) = read_shell_job_status(&paths.status_path, &status.job_id) {
        status.cancel_requested |= latest.cancel_requested;
        if latest.status == "cancel_requested" {
            status.status = latest.status;
        }
        if latest.remote_process_scope.remote_cleanup_required {
            status.remote_process_scope = latest.remote_process_scope;
        }
        if latest.error_code.as_deref()
            == Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
            && status.error_code.is_none()
        {
            status.error_code = latest.error_code;
            status.error_message = latest.error_message;
        }
    }
    status.exit_code = exit_code;
    status.timed_out = timed_out;
    status.completed_at = Some(chrono::Utc::now().to_rfc3339());
    status.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    if let Some(error) = wait_error {
        status.status = "wait_failed".to_owned();
        status.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
        status.error_message = Some(error);
    } else if status.timed_out {
        status.status =
            terminal_shell_job_status(status.exit_code, status.timed_out, status.cancel_requested)
                .to_owned();
        let timeout_ms = status.timeout_ms.unwrap_or_default();
        status.error_code = Some(error_codes::ACTION_BUDGET_EXPIRED.to_owned());
        status.error_message = Some(format!(
            "durable job timeout_ms cap expired after {timeout_ms} ms; the process tree was terminated. \
             Durable jobs are unbounded by default — omit durable_timeout_ms (or raise it) to let the job \
             run until it exits or is cancelled with act_run_shell_cancel."
        ));
        mark_shell_job_remote_cleanup_unverified(
            &mut status,
            "act_run_shell_start_timeout",
            "timeout_local_process_tree_termination_requested",
        );
    } else {
        status.status =
            terminal_shell_job_status(status.exit_code, status.timed_out, status.cancel_requested)
                .to_owned();
    }
    if let Err(error) = refresh_shell_job_remote_metadata_from_outputs(&mut status, &paths) {
        mark_shell_job_remote_cleanup_failed(
            &mut status,
            "act_run_shell_start_remote_metadata_readback",
            "remote_metadata_read_failed",
            &format!("{error:?}"),
        );
    } else if let Err(error) = reconcile_shell_job_remote_exit_marker(
        &mut status,
        &paths,
        false,
        "act_run_shell_start_remote_exit_readback",
    ) {
        mark_shell_job_remote_cleanup_failed(
            &mut status,
            "act_run_shell_start_remote_exit_readback",
            "remote_exit_marker_read_failed",
            &format!("{error:?}"),
        );
    }
    persist_shell_job_local_terminal_status(&paths, &status);
    verify_shell_job_remote_cleanup_after_terminal(
        &mut status,
        &paths,
        "act_run_shell_start_process_exit",
        Some(&original_args),
    );
    if let Err(error) = write_shell_job_status(&paths.status_path, &status) {
        tracing::error!(
            code = "M4_ACT_RUN_SHELL_JOB_FINAL_STATUS_WRITE_FAILED",
            job_id = %status.job_id,
            error = ?error,
            "act_run_shell_start monitor could not persist final job status"
        );
    } else {
        tracing::info!(
            code = "M4_ACT_RUN_SHELL_JOB_COMPLETED",
            job_id = %status.job_id,
            pid = ?status.pid,
            status = %status.status,
            exit_code = ?status.exit_code,
            timed_out = status.timed_out,
            cancel_requested = status.cancel_requested,
            "readback=act_run_shell_start after=process_complete_status_persisted"
        );
    }
}

fn persist_shell_job_local_terminal_status(paths: &ShellJobPaths, status: &ActRunShellJobStatus) {
    if let Err(error) = write_shell_job_reconciliation_status(paths, status.clone()) {
        tracing::error!(
            code = "M4_ACT_RUN_SHELL_JOB_LOCAL_TERMINAL_STATUS_WRITE_FAILED",
            job_id = %status.job_id,
            status = %status.status,
            exit_code = ?status.exit_code,
            error = ?error,
            "act_run_shell_start monitor could not persist local terminal status before remote cleanup"
        );
    } else {
        tracing::info!(
            code = "M4_ACT_RUN_SHELL_JOB_LOCAL_TERMINAL_STATUS_PERSISTED",
            job_id = %status.job_id,
            pid = ?status.pid,
            status = %status.status,
            exit_code = ?status.exit_code,
            timed_out = status.timed_out,
            cancel_requested = status.cancel_requested,
            remote_cleanup_status = %status.remote_process_scope.remote_cleanup_status,
            "readback=act_run_shell_start after=local_process_complete_status_persisted_before_remote_cleanup"
        );
    }
}

async fn wait_shell_job_child(
    child: &mut tokio::process::Child,
    timeout_ms: Option<u64>,
) -> (Option<i32>, bool, Option<String>) {
    match timeout_ms {
        Some(timeout_ms) => {
            let budget = Duration::from_millis(timeout_ms);
            let waited = Instant::now();
            match tokio::time::timeout(budget, child.wait()).await {
                Ok(Ok(status)) => {
                    // Deterministic budget enforcement from the SOURCE OF TRUTH
                    // (measured wait vs cap), not the wait-vs-timer race. Under
                    // scheduler starvation `tokio::time::timeout` polls the inner
                    // future first, so a child that self-exited AFTER the deadline
                    // is still delivered as `Ok(exit)` with the fired deadline
                    // ignored — the job silently reports "ok" despite outrunning
                    // its cap (#1580). If the measured wait met or exceeded the
                    // budget the job timed out, regardless of the exit code.
                    let timed_out = waited.elapsed() >= budget;
                    (status.code(), timed_out, None)
                }
                Ok(Err(error)) => (None, false, Some(format!("wait_failed:{error}"))),
                Err(_elapsed) => {
                    if let Some(pid) = child.id() {
                        let termination = terminate_shell_job_process_tree(pid);
                        tracing::warn!(
                            code = "M4_ACT_RUN_SHELL_JOB_TIMEOUT_TREE_TERMINATED",
                            pid,
                            attempted = termination.attempted,
                            status = %termination.status,
                            remaining_process_ids = ?termination.remaining_process_ids,
                            "act_run_shell_start timeout requested process-tree termination"
                        );
                    } else if let Err(error) = child.start_kill() {
                        tracing::warn!(
                            code = "M4_ACT_RUN_SHELL_JOB_TIMEOUT_KILL_FAILED",
                            error = %error,
                            "act_run_shell_start timeout kill request failed because pid was unavailable"
                        );
                    }
                    match child.wait().await {
                        Ok(_status) => (None, true, None),
                        Err(error) => (
                            None,
                            true,
                            Some(format!("wait_after_timeout_failed:{error}")),
                        ),
                    }
                }
            }
        }
        None => match child.wait().await {
            Ok(status) => (status.code(), false, None),
            Err(error) => (None, false, Some(format!("wait_failed:{error}"))),
        },
    }
}

fn terminal_shell_job_status(
    exit_code: Option<i32>,
    timed_out: bool,
    cancel_requested: bool,
) -> &'static str {
    if timed_out {
        "timed_out"
    } else if cancel_requested {
        "cancelled"
    } else if exit_code == Some(0) {
        "ok"
    } else {
        "exit_nonzero"
    }
}

fn reconcile_shell_job_process_state(
    mut job: ActRunShellJobStatus,
    paths: &ShellJobPaths,
) -> Result<ActRunShellJobStatus, ErrorData> {
    if job.status == "finalizing" {
        if let Some(terminal) =
            wait_for_shell_job_terminal_status(paths, &job.job_id, Duration::from_millis(500))?
        {
            return Ok(terminal);
        }
        if job
            .completed_at
            .as_deref()
            .and_then(elapsed_ms_since_rfc3339)
            .is_some_and(|elapsed_ms| elapsed_ms >= SHELL_JOB_FINALIZING_GRACE_MS)
        {
            job.status = "exited_unobserved".to_owned();
            job.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
            job.error_message =
                Some("job process exited before the monitor persisted final status".to_owned());
            job = write_shell_job_reconciliation_status(paths, job)?;
        }
        return Ok(job);
    }
    if !shell_job_live_status(&job.status) {
        return Ok(job);
    }
    let Some(pid) = job.pid else {
        job.status = "pid_unavailable".to_owned();
        job.completed_at
            .get_or_insert_with(|| chrono::Utc::now().to_rfc3339());
        job.duration_ms
            .get_or_insert_with(|| elapsed_ms_since_rfc3339(&job.started_at).unwrap_or_default());
        job.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
        job.error_message = Some("job status had no pid while marked live".to_owned());
        return write_shell_job_reconciliation_status(paths, job);
    };
    if shell_job_live_process_ids(&[pid]).contains(&pid) {
        return Ok(job);
    }
    if let Some(terminal) =
        wait_for_shell_job_terminal_status(paths, &job.job_id, Duration::from_millis(500))?
    {
        return Ok(terminal);
    }
    if job.cancel_requested {
        job.status = "cancelled".to_owned();
        job.completed_at
            .get_or_insert_with(|| chrono::Utc::now().to_rfc3339());
        job.duration_ms
            .get_or_insert_with(|| elapsed_ms_since_rfc3339(&job.started_at).unwrap_or_default());
        return write_shell_job_reconciliation_status(paths, job);
    }
    job.status = "finalizing".to_owned();
    job.completed_at
        .get_or_insert_with(|| chrono::Utc::now().to_rfc3339());
    job.duration_ms
        .get_or_insert_with(|| elapsed_ms_since_rfc3339(&job.started_at).unwrap_or_default());
    write_shell_job_reconciliation_status(paths, job)
}

fn write_shell_job_reconciliation_status(
    paths: &ShellJobPaths,
    candidate: ActRunShellJobStatus,
) -> Result<ActRunShellJobStatus, ErrorData> {
    let latest = read_shell_job_status(&paths.status_path, &candidate.job_id)?;
    if shell_job_latest_terminal_should_win(&latest, &candidate) {
        tracing::info!(
            code = "M4_ACT_RUN_SHELL_RECONCILE_PRESERVED_TERMINAL_STATUS",
            job_id = %candidate.job_id,
            candidate_status = %candidate.status,
            latest_status = %latest.status,
            latest_exit_code = ?latest.exit_code,
            "act_run_shell_status preserved monitor-written terminal status"
        );
        return Ok(latest);
    }
    write_shell_job_status(&paths.status_path, &candidate)?;
    Ok(candidate)
}

fn shell_job_latest_terminal_should_win(
    latest: &ActRunShellJobStatus,
    candidate: &ActRunShellJobStatus,
) -> bool {
    if !shell_job_terminal_status(&latest.status) {
        return false;
    }
    if !shell_job_terminal_status(&candidate.status) {
        return true;
    }
    if matches!(
        candidate.status.as_str(),
        "exited_unobserved" | "pid_unavailable"
    ) {
        return true;
    }
    if latest.status == "ok" && latest.exit_code == Some(0) && candidate.exit_code != Some(0) {
        return true;
    }
    candidate.exit_code.is_none() && latest.exit_code.is_some()
}

fn wait_for_shell_job_terminal_status(
    paths: &ShellJobPaths,
    job_id: &str,
    max_wait: Duration,
) -> Result<Option<ActRunShellJobStatus>, ErrorData> {
    let started = Instant::now();
    loop {
        let latest = read_shell_job_status(&paths.status_path, job_id)?;
        if shell_job_terminal_status(&latest.status) {
            return Ok(Some(latest));
        }
        if started.elapsed() >= max_wait {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn shell_job_live_status(status: &str) -> bool {
    matches!(status, "running" | "cancel_requested")
}

fn shell_job_terminal_status(status: &str) -> bool {
    !matches!(status, "running" | "cancel_requested" | "finalizing")
}

fn elapsed_ms_since_rfc3339(started_at: &str) -> Option<u64> {
    let started_at = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let elapsed = chrono::Utc::now().signed_duration_since(started_at);
    u64::try_from(elapsed.num_milliseconds().max(0)).ok()
}

fn file_len(path: &Path, job_id: &str, stream: &'static str) -> Result<u64, ErrorData> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| {
            shell_tool_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "act_run_shell_status failed to read shell job {stream} log metadata: {error}"
                ),
                json!({
                    "code": error_codes::STORAGE_READ_FAILED,
                    "job_id": job_id,
                    "path": path,
                    "stream": stream,
                    "reason": "job_output_metadata_read_failed",
                }),
            )
        })
}

fn tail_file_lossy(path: &Path, limit_bytes: usize) -> Result<String, ErrorData> {
    let mut file = fs::File::open(path).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell_status failed to open shell job output: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": path,
                "reason": "job_output_open_read_failed",
            }),
        )
    })?;
    let len = file
        .metadata()
        .map_err(|error| {
            shell_tool_error(
                error_codes::STORAGE_READ_FAILED,
                format!("act_run_shell_status failed to read shell job output metadata: {error}"),
                json!({
                    "code": error_codes::STORAGE_READ_FAILED,
                    "path": path,
                    "reason": "job_output_metadata_read_failed",
                }),
            )
        })?
        .len();
    let start = len.saturating_sub(u64::try_from(limit_bytes).unwrap_or(u64::MAX));
    file.seek(SeekFrom::Start(start)).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell_status failed to seek shell job output: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": path,
                "reason": "job_output_seek_failed",
            }),
        )
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell_status failed to read shell job output: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": path,
                "reason": "job_output_read_failed",
            }),
        )
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_file_prefix_lossy(path: &Path, limit_bytes: usize) -> Result<String, ErrorData> {
    let mut file = fs::File::open(path).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell_status failed to open shell job output: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": path,
                "reason": "job_output_open_read_failed",
            }),
        )
    })?;
    let mut bytes = Vec::new();
    // Disambiguate `by_ref`: both `io::Read` and `io::Write` are in scope for
    // `File`, so name the Read impl explicitly for this bounded output read.
    io::Read::by_ref(&mut file)
        .take(u64::try_from(limit_bytes).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            shell_tool_error(
                error_codes::STORAGE_READ_FAILED,
                format!("act_run_shell_status failed to read shell job output prefix: {error}"),
                json!({
                    "code": error_codes::STORAGE_READ_FAILED,
                    "path": path,
                    "reason": "job_output_prefix_read_failed",
                }),
            )
        })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn run_shell_start_request_sha256(params: &ActRunShellStartParams) -> Result<String, ErrorData> {
    let payload = json!({
        "command": params.command,
        "args": params.args,
        "working_dir": params.working_dir,
        "env": params.env,
        "timeout_ms": params.timeout_ms,
        "job_id": params.job_id,
    });
    let bytes = serde_json::to_vec(&payload).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_run_shell_start request fingerprint encode failed: {error}"),
        )
    })?;
    Ok(sha256_hex(&bytes))
}

fn extract_error_code(error: &ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(windows)]
fn apply_no_window_tokio(command: &mut TokioCommand) {
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn apply_no_window_tokio(_command: &mut TokioCommand) {}

#[cfg(windows)]
fn apply_no_window_std(command: &mut StdCommand) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn apply_no_window_std(_command: &mut StdCommand) {}

struct ShellJobTerminationReadback {
    attempted: bool,
    status: String,
    remaining_process_ids: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnedProcessTerminationReadback {
    pub pid: u32,
    pub process_ids: Vec<u32>,
    pub live_process_ids_before: Vec<u32>,
    pub attempted: bool,
    pub status: String,
    pub remaining_process_ids: Vec<u32>,
}

pub(crate) fn owned_process_tree_ids(pid: u32) -> Vec<u32> {
    shell_job_process_tree_ids(pid)
}

pub(crate) fn owned_live_process_ids(process_ids: &[u32]) -> Vec<u32> {
    shell_job_live_process_ids(process_ids)
}

pub(crate) fn wait_for_owned_process_tree_exit(
    process_ids: &[u32],
    timeout: Duration,
) -> (Vec<u32>, u64) {
    wait_for_shell_job_process_tree_exit(process_ids, timeout)
}

pub(crate) fn process_exists(pid: u32) -> bool {
    owned_live_process_ids(&[pid]).contains(&pid)
}

pub fn terminate_owned_process_tree(pid: u32) -> OwnedProcessTerminationReadback {
    let process_ids = shell_job_process_tree_ids(pid);
    let live_process_ids_before = shell_job_live_process_ids(&process_ids);
    if live_process_ids_before.is_empty() {
        return OwnedProcessTerminationReadback {
            pid,
            process_ids,
            live_process_ids_before,
            attempted: false,
            status: "already_exited".to_owned(),
            remaining_process_ids: Vec::new(),
        };
    }

    let termination = terminate_shell_job_process_tree_platform(pid, &process_ids);
    OwnedProcessTerminationReadback {
        pid,
        process_ids,
        live_process_ids_before,
        attempted: termination.attempted,
        status: termination.status,
        remaining_process_ids: termination.remaining_process_ids,
    }
}

pub(crate) fn terminate_owned_process_ids(process_ids: &[u32]) -> OwnedProcessTerminationReadback {
    let mut process_ids = process_ids.to_vec();
    process_ids.sort_unstable();
    process_ids.dedup();
    let live_process_ids_before = shell_job_live_process_ids(&process_ids);
    if live_process_ids_before.is_empty() {
        return OwnedProcessTerminationReadback {
            pid: 0,
            process_ids,
            live_process_ids_before,
            attempted: false,
            status: "already_exited".to_owned(),
            remaining_process_ids: Vec::new(),
        };
    }
    let termination = terminate_shell_job_process_tree_platform(0, &process_ids);
    OwnedProcessTerminationReadback {
        pid: 0,
        process_ids,
        live_process_ids_before,
        attempted: termination.attempted,
        status: termination.status,
        remaining_process_ids: termination.remaining_process_ids,
    }
}

// ----------------------------------------------------------------------------
// Process suspend / resume (#906 agent_pause / agent_resume)
// ----------------------------------------------------------------------------
//
// Suspending a process is done with the undocumented ntdll `NtSuspendProcess` /
// `NtResumeProcess` rather than the documented "enumerate threads then
// SuspendThread each" route: the documented route has a thread-creation race
// (a thread spawned between the snapshot and the per-thread suspend escapes),
// whereas the kernel walks the live thread list atomically. This is the same
// approach PsSuspend and py-spy use. Each suspend increments a per-thread
// suspend count, so N suspends require N resumes — `agent_pause` therefore
// reads the physical suspend state back and refuses to stack suspends.

/// Physical suspend-state of one process, read from the OS thread table — the
/// source of truth for "is it actually frozen". A fully-suspended process has
/// `total_threads > 0` and `suspended_threads == total_threads`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessSuspendState {
    pub pid: u32,
    pub total_threads: u32,
    pub suspended_threads: u32,
    pub fully_suspended: bool,
}

/// One pid that an Nt(Suspend|Resume)Process call could not be applied to.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessControlFailure {
    pub pid: u32,
    pub reason: String,
}

/// Readback for a suspend/resume sweep over an owned process tree. `states_after`
/// is the physical thread-table readback taken AFTER the operation.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnedProcessSuspendReadback {
    pub process_ids: Vec<u32>,
    pub live_process_ids: Vec<u32>,
    /// Pids the Nt(Suspend|Resume)Process call returned success for.
    pub applied_process_ids: Vec<u32>,
    /// Pids the call could not be applied to, each with its reason.
    pub failed: Vec<ProcessControlFailure>,
    /// Physical per-process thread suspend-state after the operation (the SoT).
    pub states_after: Vec<ProcessSuspendState>,
    /// True iff every live process in the tree is fully suspended.
    pub all_suspended: bool,
    /// True iff every live process in the tree has zero suspended threads.
    pub all_running: bool,
}

fn summarize_suspend_readback(
    process_ids: Vec<u32>,
    live_process_ids: Vec<u32>,
    applied_process_ids: Vec<u32>,
    failed: Vec<ProcessControlFailure>,
) -> OwnedProcessSuspendReadback {
    let states_after = process_tree_suspend_state(&live_process_ids);
    let all_suspended =
        !states_after.is_empty() && states_after.iter().all(|state| state.fully_suspended);
    let all_running = states_after
        .iter()
        .all(|state| state.suspended_threads == 0);
    OwnedProcessSuspendReadback {
        process_ids,
        live_process_ids,
        applied_process_ids,
        failed,
        states_after,
        all_suspended,
        all_running,
    }
}

/// Suspends the given owned process ids (the caller's already-resolved tree).
pub(crate) fn suspend_owned_process_ids(process_ids: &[u32]) -> OwnedProcessSuspendReadback {
    let live_process_ids = shell_job_live_process_ids(process_ids);
    let (applied, failed) = set_process_tree_suspended_platform(&live_process_ids, true);
    summarize_suspend_readback(process_ids.to_vec(), live_process_ids, applied, failed)
}

/// Resumes the given owned process ids (the caller's already-resolved tree).
pub(crate) fn resume_owned_process_ids(process_ids: &[u32]) -> OwnedProcessSuspendReadback {
    let live_process_ids = shell_job_live_process_ids(process_ids);
    let (applied, failed) = set_process_tree_suspended_platform(&live_process_ids, false);
    summarize_suspend_readback(process_ids.to_vec(), live_process_ids, applied, failed)
}

/// Reads the physical suspend state of each pid from the OS thread table.
pub(crate) fn process_tree_suspend_state(process_ids: &[u32]) -> Vec<ProcessSuspendState> {
    process_tree_suspend_state_platform(process_ids)
}

#[cfg(windows)]
fn set_process_tree_suspended_platform(
    live_process_ids: &[u32],
    suspend: bool,
) -> (Vec<u32>, Vec<ProcessControlFailure>) {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SUSPEND_RESUME};

    // Undocumented but stable ntdll process-wide suspend/resume. Exported from
    // ntdll on every supported Windows; linked directly the way py-spy does.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSuspendProcess(handle: HANDLE) -> i32;
        fn NtResumeProcess(handle: HANDLE) -> i32;
    }

    let mut applied = Vec::new();
    let mut failed = Vec::new();
    for &pid in live_process_ids {
        let handle = match unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, false, pid) } {
            Ok(handle) => handle,
            Err(error) => {
                failed.push(ProcessControlFailure {
                    pid,
                    reason: format!("OpenProcess(PROCESS_SUSPEND_RESUME) failed: {error}"),
                });
                continue;
            }
        };
        // NTSTATUS: negative values are errors; 0 (STATUS_SUCCESS) is success.
        let status = if suspend {
            unsafe { NtSuspendProcess(handle) }
        } else {
            unsafe { NtResumeProcess(handle) }
        };
        let _ = unsafe { CloseHandle(handle) };
        if status < 0 {
            failed.push(ProcessControlFailure {
                pid,
                reason: format!(
                    "{} returned NTSTATUS 0x{status:08x}",
                    if suspend {
                        "NtSuspendProcess"
                    } else {
                        "NtResumeProcess"
                    }
                ),
            });
        } else {
            applied.push(pid);
        }
    }
    (applied, failed)
}

/// Reads each pid's thread suspend state by a NET-ZERO probe: `SuspendThread`
/// returns the suspend count *before* it increments, so we suspend then
/// immediately resume each thread, leaving the count unchanged while learning
/// whether it was already suspended (returned count >= 1). Lower UB risk than
/// hand-walking `NtQuerySystemInformation` buffers, and still a real read of OS
/// state. Safe because we only ever touch pids inside an owned agent tree.
#[cfg(windows)]
fn process_tree_suspend_state_platform(process_ids: &[u32]) -> Vec<ProcessSuspendState> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
    };

    let mut states: Vec<ProcessSuspendState> = process_ids
        .iter()
        .map(|&pid| ProcessSuspendState {
            pid,
            total_threads: 0,
            suspended_threads: 0,
            fully_suspended: false,
        })
        .collect();
    if states.is_empty() {
        return states;
    }

    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) } {
        Ok(snapshot) => snapshot,
        Err(_) => return states,
    };
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).unwrap_or(0),
        ..Default::default()
    };
    if unsafe { Thread32First(snapshot, &mut entry) }.is_ok() {
        loop {
            let owner = entry.th32OwnerProcessID;
            if let Some(state) = states.iter_mut().find(|state| state.pid == owner) {
                state.total_threads = state.total_threads.saturating_add(1);
                if let Ok(thread) =
                    unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
                {
                    // Net-zero probe: previous count = value returned by SuspendThread.
                    let previous = unsafe { SuspendThread(thread) };
                    if previous != u32::MAX {
                        let _ = unsafe { ResumeThread(thread) };
                        if previous >= 1 {
                            state.suspended_threads = state.suspended_threads.saturating_add(1);
                        }
                    }
                    let _ = unsafe { CloseHandle(thread) };
                }
            }
            entry = THREADENTRY32 {
                dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).unwrap_or(0),
                ..Default::default()
            };
            if unsafe { Thread32Next(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(snapshot) };

    for state in &mut states {
        state.fully_suspended =
            state.total_threads > 0 && state.suspended_threads == state.total_threads;
    }
    states
}

#[cfg(not(windows))]
fn set_process_tree_suspended_platform(
    live_process_ids: &[u32],
    suspend: bool,
) -> (Vec<u32>, Vec<ProcessControlFailure>) {
    let signal = if suspend { "-STOP" } else { "-CONT" };
    let mut applied = Vec::new();
    let mut failed = Vec::new();
    for &pid in live_process_ids {
        match StdCommand::new("kill")
            .args([signal, &pid.to_string()])
            .output()
        {
            Ok(output) if output.status.success() => applied.push(pid),
            Ok(output) => failed.push(ProcessControlFailure {
                pid,
                reason: format!("kill {signal} exited {:?}", output.status.code()),
            }),
            Err(error) => failed.push(ProcessControlFailure {
                pid,
                reason: format!("kill {signal} spawn failed: {error}"),
            }),
        }
    }
    (applied, failed)
}

#[cfg(not(windows))]
fn process_tree_suspend_state_platform(process_ids: &[u32]) -> Vec<ProcessSuspendState> {
    // Non-Windows builds exist only for unit-testable host portability; the
    // owned-PTY/agent fleet runs on Windows. Report unknown thread counts
    // rather than fabricate a suspend state.
    process_ids
        .iter()
        .map(|&pid| ProcessSuspendState {
            pid,
            total_threads: 0,
            suspended_threads: 0,
            fully_suspended: false,
        })
        .collect()
}

fn terminate_shell_job_process_tree(pid: u32) -> ShellJobTerminationReadback {
    let process_ids = shell_job_process_tree_ids(pid);
    let initial_live_process_ids = shell_job_live_process_ids(&process_ids);
    if initial_live_process_ids.is_empty() {
        return ShellJobTerminationReadback {
            attempted: false,
            status: "already_exited".to_owned(),
            remaining_process_ids: Vec::new(),
        };
    }

    terminate_shell_job_process_tree_platform(pid, &process_ids)
}

#[cfg(windows)]
fn terminate_shell_job_process_tree_platform(
    _pid: u32,
    process_ids: &[u32],
) -> ShellJobTerminationReadback {
    let mut spawn_error = None;
    for target_pid in process_ids.iter().rev() {
        let mut command = StdCommand::new("taskkill.exe");
        command.args(["/PID", &target_pid.to_string(), "/F"]);
        apply_no_window_std(&mut command);
        if let Err(error) = command.output() {
            spawn_error = Some(error.to_string());
            break;
        }
    }
    let (remaining_process_ids, _waited_ms) =
        wait_for_shell_job_process_tree_exit(process_ids, Duration::from_secs(5));
    ShellJobTerminationReadback {
        attempted: true,
        status: if remaining_process_ids.is_empty() {
            "terminated".to_owned()
        } else if let Some(error) = spawn_error {
            format!("taskkill_spawn_failed:{error}")
        } else {
            "termination_incomplete".to_owned()
        },
        remaining_process_ids,
    }
}

#[cfg(not(windows))]
fn terminate_shell_job_process_tree_platform(
    _pid: u32,
    process_ids: &[u32],
) -> ShellJobTerminationReadback {
    let mut status = "terminated".to_owned();
    for pid in process_ids.iter().rev() {
        let output = StdCommand::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output();
        if let Err(error) = output {
            status = format!("kill_spawn_failed:{error}");
        }
    }
    let (mut remaining_process_ids, _waited_ms) =
        wait_for_shell_job_process_tree_exit(process_ids, Duration::from_secs(5));
    if !remaining_process_ids.is_empty() {
        for pid in &remaining_process_ids {
            let _ = StdCommand::new("kill")
                .args(["-KILL", &pid.to_string()])
                .output();
        }
        let (remaining_after_kill, _waited_ms) =
            wait_for_shell_job_process_tree_exit(process_ids, Duration::from_secs(5));
        remaining_process_ids = remaining_after_kill;
        if !remaining_process_ids.is_empty() {
            status = "termination_failed".to_owned();
        }
    }
    ShellJobTerminationReadback {
        attempted: true,
        status,
        remaining_process_ids,
    }
}

fn shell_job_process_tree_ids(root_pid: u32) -> Vec<u32> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut system = System::new_all();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut ids = vec![root_pid];
    let Some(root_process) = system.process(Pid::from_u32(root_pid)) else {
        return ids;
    };
    ids.extend(shell_job_descendant_process_ids(
        &system,
        root_pid,
        root_process.start_time(),
    ));
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn shell_job_descendant_process_ids(
    system: &sysinfo::System,
    root_pid: u32,
    root_start_time: u64,
) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut stack = vec![root_pid];
    let mut visited = HashSet::from([root_pid]);
    while let Some(parent) = stack.pop() {
        for (pid, process) in system.processes() {
            if process.parent().map(|value| value.as_u32()) == Some(parent) {
                let child = pid.as_u32();
                if process.start_time() < root_start_time || !visited.insert(child) {
                    continue;
                }
                descendants.push(child);
                stack.push(child);
            }
        }
    }
    descendants
}

fn shell_job_live_process_ids(process_ids: &[u32]) -> Vec<u32> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let pids = process_ids
        .iter()
        .copied()
        .map(Pid::from_u32)
        .collect::<Vec<_>>();
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&pids), false);
    process_ids
        .iter()
        .copied()
        .filter(|pid| system.process(Pid::from_u32(*pid)).is_some())
        .collect()
}

fn wait_for_shell_job_process_tree_exit(process_ids: &[u32], timeout: Duration) -> (Vec<u32>, u64) {
    let started = Instant::now();
    loop {
        let remaining = shell_job_live_process_ids(process_ids);
        if remaining.is_empty() || started.elapsed() >= timeout {
            return (remaining, started.elapsed().as_millis() as u64);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

async fn run_allowlisted_shell(
    params: ActRunShellParams,
    inline_await_limit_ms: u64,
    context: Option<&ShellExecutionContext>,
) -> Result<ActRunShellResponse, ErrorData> {
    let started = Instant::now();
    let requested_execution_mode = params.execution_mode;
    let mut spawned = spawn_shell_child(&params, context)?;
    let (stdout_task, stderr_task) = spawn_capped_readers(&mut spawned.child)?;
    let (exit_code, timed_out) = wait_shell_child(&mut spawned.child, params.timeout_ms).await?;
    let stdout = join_capped_stream(stdout_task, "stdout").await?;
    let stderr = join_capped_stream(stderr_task, "stderr").await?;
    let (error_code, error_message) = shell_budget_error(
        timed_out,
        params.timeout_ms,
        requested_execution_mode,
        inline_await_limit_ms,
    );
    Ok(ActRunShellResponse {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        duration_ms: elapsed_ms_u32(started),
        timed_out,
        error_code,
        error_message,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        session_id: context.map(|context| context.session_id().to_owned()),
        effective_working_dir: params.working_dir.clone().or_else(|| {
            Some(path_string(
                &resolve_shell_working_dir(None, context, "act_run_shell").ok()?,
            ))
        }),
        backgrounded: false,
        background_reason: None,
        inline_await_limit_ms: None,
        inline_client_call_budget_ms: None,
        requested_execution_mode: Some(requested_execution_mode),
        effective_execution_mode: Some(ActRunShellExecutionMode::Inline),
        durable_timeout_ms: None,
        job_id: None,
        job: None,
    })
}

fn shell_budget_error(
    timed_out: bool,
    timeout_ms: u64,
    execution_mode: ActRunShellExecutionMode,
    inline_await_limit_ms: u64,
) -> (Option<String>, Option<String>) {
    if !timed_out {
        return (None, None);
    }
    // The caller's own timeout_ms budget expired while running inline. The message must name the
    // failure, the cause, and a concrete remediation the caller can act on without consulting docs
    // (Google AIP-193 / "Fail Fast with Actionable Errors"): how to get more time in a single call.
    let remediation = match execution_mode {
        ActRunShellExecutionMode::Inline => {
            "raise timeout_ms only when the total wait still fits inside the MCP client-call budget, \
             or switch to execution_mode=\"durable\" (or act_run_shell_start) for an unbounded \
             background job polled with act_run_shell_status"
                .to_owned()
        }
        // Durable execution backgrounds before reaching the inline path, so this arm is defensive.
        ActRunShellExecutionMode::Auto | ActRunShellExecutionMode::Durable => format!(
            "raise timeout_ms above the {inline_await_limit_ms} ms inline await limit to auto-background \
             into a durable job polled with act_run_shell_status, set execution_mode=\"durable\", \
             or set execution_mode=\"inline\" only for a single-call wait that fits inside the MCP \
             client-call budget"
        ),
    };
    (
        Some(error_codes::ACTION_BUDGET_EXPIRED.to_owned()),
        Some(format!(
            "caller timeout_ms budget expired after {timeout_ms} ms; the process tree was terminated. \
             Inline MCP calls are guarded by a {DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS} ms client-call budget so long-running commands keep \
             a durable status handle instead of disappearing behind a client timeout. To allow more \
             time: {remediation}."
        )),
    )
}

struct SpawnedShellChild {
    child: tokio::process::Child,
    process_job: OwnedProcessJob,
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct OwnedProcessJob {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(not(windows))]
#[derive(Debug)]
pub(crate) struct OwnedProcessJob;

#[cfg(windows)]
impl Drop for OwnedProcessJob {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(windows)]
unsafe impl Send for OwnedProcessJob {}

#[cfg(windows)]
impl OwnedProcessJob {
    pub(crate) fn disarm_kill_on_close(
        &mut self,
        tool_name: &'static str,
        pid: u32,
        resource_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        use windows::Win32::System::JobObjects::{
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = Default::default();
        let limit_size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(|error| {
                shell_tool_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{tool_name} failed to size Windows job object limits: {error}"),
                    json!({
                        "code": error_codes::TOOL_INTERNAL_ERROR,
                        "pid": pid,
                        "resource_id": resource_id,
                        "reason": "job_object_limit_size_failed",
                    }),
                )
            })?;
        unsafe {
            SetInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                limit_size,
            )
        }
        .map_err(|error| {
            shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{tool_name} failed to disarm Windows job object kill-on-close: {error}"),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "resource_id": resource_id,
                    "reason": "job_object_kill_on_close_disarm_failed",
                }),
            )
        })
    }
}

#[cfg(not(windows))]
impl OwnedProcessJob {
    pub(crate) fn disarm_kill_on_close(
        &mut self,
        _tool_name: &'static str,
        _pid: u32,
        _resource_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) fn assign_owned_process_job(
    pid: u32,
    tool_name: &'static str,
    resource_id: Option<&str>,
) -> Result<OwnedProcessJob, ErrorData> {
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::{
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                    SetInformationJobObject,
                },
                Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
            },
        },
        core::PCWSTR,
    };

    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(|error| {
        shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("{tool_name} failed to create a Windows job object: {error}"),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "resource_id": resource_id,
                "reason": "job_object_create_failed",
            }),
        )
    })?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let limit_size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
        .map_err(|error| {
            let _ = unsafe { CloseHandle(job) };
            shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{tool_name} failed to size Windows job object limits: {error}"),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "resource_id": resource_id,
                    "reason": "job_object_limit_size_failed",
                }),
            )
        })?;
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            limit_size,
        )
    }
    .map_err(|error| {
        let _ = unsafe { CloseHandle(job) };
        shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("{tool_name} failed to set Windows job object kill-on-close: {error}"),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "resource_id": resource_id,
                "reason": "job_object_limit_failed",
            }),
        )
    })?;
    let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) }
        .map_err(|error| {
            let _ = unsafe { CloseHandle(job) };
            shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{tool_name} failed to open child process for job assignment: {error}"),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "resource_id": resource_id,
                    "reason": "job_object_process_open_failed",
                }),
            )
        })?;
    let assign_result = unsafe { AssignProcessToJobObject(job, process) };
    let _ = unsafe { CloseHandle(process) };
    assign_result.map_err(|error| {
        let _ = unsafe { CloseHandle(job) };
        shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("{tool_name} failed to assign child process to Windows job object: {error}"),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "resource_id": resource_id,
                "reason": "job_object_assign_failed",
            }),
        )
    })?;
    Ok(OwnedProcessJob { handle: job })
}

#[cfg(not(windows))]
pub(crate) fn assign_owned_process_job(
    _pid: u32,
    _tool_name: &'static str,
    _resource_id: Option<&str>,
) -> Result<OwnedProcessJob, ErrorData> {
    Ok(OwnedProcessJob)
}

fn spawn_shell_child(
    params: &ActRunShellParams,
    context: Option<&ShellExecutionContext>,
) -> Result<SpawnedShellChild, ErrorData> {
    let spawn_command = shell_spawn_command(&params.command);
    let mut command = TokioCommand::new(spawn_command.as_ref());
    command.args(&params.args);
    if let Some(working_dir) = &params.working_dir {
        command.current_dir(working_dir);
    }
    command.env_clear();
    let mut env = child_base_environment();
    ensure_child_temp_environment(&mut env);
    validate_child_base_environment(&env, "act_run_shell")?;
    for (_sort_key, (key, value)) in env {
        command.env(key, value);
    }
    command.envs(&params.env);
    apply_shell_session_environment(&mut command, params.working_dir.as_deref(), context);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_no_window_tokio(&mut command);

    let mut child = command.spawn().map_err(|error| {
        let command_metadata = shell_command_metadata(&params.command, &params.args);
        shell_tool_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("act_run_shell failed to spawn command: {error}"),
            json!({
                "code": error_codes::ACTION_TARGET_INVALID,
                "command": params.command,
                "spawn_command": spawn_command.as_ref(),
                "spawn_command_resolved": spawn_command.as_ref() != params.command.as_str(),
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": command_metadata.args,
                "args_redacted": command_metadata.args_redacted,
                "args_original_count": command_metadata.args_original_count,
                "args_original_bytes": command_metadata.args_original_bytes,
                "args_sha256": command_metadata.args_sha256,
                "command_line": command_metadata.command_line,
                "command_line_redacted": command_metadata.command_line_redacted,
                "command_line_original_bytes": command_metadata.command_line_original_bytes,
                "command_line_sha256": command_metadata.command_line_sha256,
                "working_dir": params.working_dir,
                "reason": "spawn_failed",
            }),
        )
    })?;
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        return Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell spawned a child process but could not read its pid",
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "command": params.command,
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": shell_command_metadata(&params.command, &params.args).args,
                "args_sha256": shell_args_sha256(&params.args),
                "working_dir": params.working_dir,
                "reason": "pid_unavailable",
            }),
        ));
    };
    let process_job = assign_owned_process_job(pid, "act_run_shell", None)?;
    Ok(SpawnedShellChild { child, process_job })
}

type CappedStreamTask = tokio::task::JoinHandle<io::Result<CappedOutput>>;

fn spawn_capped_readers(
    child: &mut tokio::process::Child,
) -> Result<(CappedStreamTask, CappedStreamTask), ErrorData> {
    let stdout = child.stdout.take().ok_or_else(|| {
        shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell stdout pipe missing after spawn",
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "reason": "stdout_pipe_missing",
            }),
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell stderr pipe missing after spawn",
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "reason": "stderr_pipe_missing",
            }),
        )
    })?;
    let stdout_task = tokio::spawn(read_capped_stream(stdout));
    let stderr_task = tokio::spawn(read_capped_stream(stderr));
    Ok((stdout_task, stderr_task))
}

async fn wait_shell_child(
    child: &mut tokio::process::Child,
    timeout_ms: u64,
) -> Result<(Option<i32>, bool), ErrorData> {
    let wait_result = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await;
    let result = match wait_result {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(error)) => {
            return Err(shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("act_run_shell failed while waiting for command: {error}"),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "reason": "wait_failed",
                }),
            ));
        }
        Err(_elapsed) => {
            if let Some(pid) = child.id() {
                let termination = terminate_shell_job_process_tree(pid);
                tracing::warn!(
                    code = "M4_ACT_RUN_SHELL_TIMEOUT_TREE_TERMINATED",
                    pid,
                    attempted = termination.attempted,
                    status = %termination.status,
                    remaining_process_ids = ?termination.remaining_process_ids,
                    "act_run_shell timeout requested process-tree termination"
                );
            } else if let Err(error) = child.start_kill() {
                tracing::warn!(
                    code = "M4_ACT_RUN_SHELL_KILL_FAILED",
                    error = %error,
                    "act_run_shell timeout kill request failed because pid was unavailable"
                );
            }
            let _status = child.wait().await.map_err(|error| {
                shell_tool_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("act_run_shell failed while waiting after timeout kill: {error}"),
                    json!({
                        "code": error_codes::TOOL_INTERNAL_ERROR,
                        "reason": "wait_after_timeout_failed",
                    }),
                )
            })?;
            (None, true)
        }
    };
    Ok(result)
}

#[derive(Debug)]
struct CappedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_capped_stream<R>(mut reader: R) -> io::Result<CappedOutput>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = SHELL_OUTPUT_CAP_BYTES.saturating_sub(bytes.len());
        let keep = read.min(remaining);
        if keep > 0 {
            bytes.extend_from_slice(&buffer[..keep]);
        }
        if keep < read {
            truncated = true;
        }
    }
    Ok(CappedOutput { bytes, truncated })
}

/// Upper bound on how long we will keep draining a child's stdout/stderr pipe
/// after the child itself has been waited on. A normally-exited (or freshly
/// killed) process closes its write handle, so the reader reaches EOF
/// immediately and this cap is never approached. The cap exists only to defend
/// against an *escaped* descendant (e.g. a lingering `conhost.exe` that
/// inherited the pipe and survived the process-tree kill): without it that
/// orphan keeps the read end open and a 500 ms-timeout call can block for
/// minutes waiting for an EOF that never comes.
const SHELL_READER_DRAIN_CAP: Duration = Duration::from_secs(5);

async fn join_capped_stream(
    task: CappedStreamTask,
    stream_name: &'static str,
) -> Result<CappedOutput, ErrorData> {
    let abort_handle = task.abort_handle();
    let join_result = match tokio::time::timeout(SHELL_READER_DRAIN_CAP, task).await {
        Ok(join_result) => join_result,
        Err(_elapsed) => {
            // The child was already waited on but the pipe never reached EOF —
            // an escaped descendant is holding the write end open. Stop the
            // reader and return what we have rather than hang the whole call.
            abort_handle.abort();
            tracing::warn!(
                code = "M4_ACT_RUN_SHELL_READER_DRAIN_CAPPED",
                stream = stream_name,
                cap_ms = SHELL_READER_DRAIN_CAP.as_millis() as u64,
                "act_run_shell {stream_name} reader did not reach EOF within the drain cap after \
                 the process was waited on; an inherited pipe handle likely outlived the killed \
                 process tree. Returning partial output."
            );
            return Ok(CappedOutput {
                bytes: Vec::new(),
                truncated: true,
            });
        }
    };
    join_result
        .map_err(|error| {
            shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("act_run_shell {stream_name} reader task failed: {error}"),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "stream": stream_name,
                    "reason": "stream_join_failed",
                }),
            )
        })?
        .map_err(|error| {
            shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("act_run_shell {stream_name} read failed: {error}"),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "stream": stream_name,
                    "reason": "stream_read_failed",
                }),
            )
        })
}

fn shell_command_line(params: &ActRunShellParams) -> String {
    shell_command_line_from_parts(&params.command, &params.args)
}

/// Distinctive substrings of global OS input / foreground-seizing APIs that a
/// shell command must not invoke. These calls bypass Synapse's lease and act on
/// the human operator's foreground window (#717/#1204).
const SHELL_GLOBAL_INPUT_MARKERS: &[&str] = &[
    "sendkeys",
    "sendwait",
    "sendinput",
    "keybd_event",
    "mouse_event",
    "setcursorpos",
    "setforegroundwindow",
    "bringwindowtotop",
    "autohotkey",
];

/// Returns the first global-input marker found in a composed shell command line
/// (case-insensitive), or `None` if the command does not invoke global OS input.
fn detect_shell_global_input(command_line: &str) -> Option<&'static str> {
    let haystack = command_line.to_ascii_lowercase();
    SHELL_GLOBAL_INPUT_MARKERS
        .iter()
        .copied()
        .find(|marker| haystack.contains(marker))
}

/// PowerShell automatic/read-only variables (case-insensitive names). Assigning
/// to any of these silently fails or throws, and the name then keeps its
/// built-in value — the `$home`/`$HOME` collision behind #1507. Assignment to
/// one of these is almost never intended and is refused fail-closed.
const SHELL_RESERVED_PS_VARIABLES: &[&str] = &[
    "home",
    "pwd",
    "pid",
    "profile",
    "pshome",
    "psscriptroot",
    "pscommandpath",
    "psversiontable",
    "host",
    "true",
    "false",
    "null",
    "input",
    "matches",
    "myinvocation",
    "executioncontext",
    "shellid",
    "lastexitcode",
    "consolefilename",
];

/// PowerShell/cmd path variables that can resolve outside a shell job's
/// workspace (user home, profile, tooling roots). A recursive delete/move that
/// targets one of these cannot be proven contained. Lowercase for matching.
const SHELL_UNCONTAINED_PATH_REFERENCES: &[&str] = &[
    "$home",
    "${home}",
    "$env:userprofile",
    "$env:homepath",
    "$env:homedrive",
    "$env:appdata",
    "$env:localappdata",
    "$env:systemroot",
    "$env:windir",
    "$env:programfiles",
    "$env:programdata",
    "$profile",
    "$pshome",
];

/// Recursive/whole-tree destructive verb+flag pairs. Each entry is
/// `(verb_markers, recursive_flag_markers)`; a hazard requires one marker from
/// each set in the same command. Lowercase for matching.
const SHELL_RECURSIVE_DELETE_VERBS: &[&str] = &[
    "remove-item",
    "remove-itemproperty",
    "[system.io.directory]::delete",
    " ri ",
    " rm ",
    " rmdir",
    " rd ",
    " del ",
    " erase ",
    "move-item",
    " mv ",
    " move ",
];

const SHELL_RECURSIVE_FLAGS: &[&str] = &[
    "-recurse",
    "-r ",
    "-r\"",
    " /s",
    "-force -recurse",
    ", $true",
];

/// Detects assignment to a PowerShell automatic/read-only variable (the
/// `$home = ...` collision from #1507). Returns the offending variable name.
///
/// Only assignment is a hazard: read-only *use* (`Join-Path $HOME x`) and
/// `$env:` namespace variables are left alone. `==`, `-eq`, and comparison
/// contexts are not assignments and do not match.
fn detect_shell_reserved_variable_assignment(command_line: &str) -> Option<&'static str> {
    let haystack = command_line.to_ascii_lowercase();
    for reserved in SHELL_RESERVED_PS_VARIABLES {
        let needle = format!("${reserved}");
        let mut search_from = 0;
        while let Some(rel) = haystack[search_from..].find(&needle) {
            let start = search_from + rel;
            let after = start + needle.len();
            // The next non-name byte must not continue the identifier — otherwise
            // `$home` matched inside `$homedir`. PowerShell identifiers allow
            // ASCII alphanumerics and `_`.
            let boundary_ok = haystack.as_bytes().get(after).is_none_or(|byte| {
                !(byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b':')
            });
            if boundary_ok {
                // Skip whitespace, then require a single `=` that is not `==`,
                // `-eq`, `+=` is still a write to a read-only var so it counts.
                let rest = haystack[after..].trim_start();
                let is_assignment = rest
                    .strip_prefix("+=")
                    .or_else(|| rest.strip_prefix('='))
                    .is_some_and(|tail| !tail.starts_with('='));
                if is_assignment {
                    return Some(reserved);
                }
            }
            search_from = after;
        }
    }
    None
}

/// Detects a recursive delete/move whose target references a path variable that
/// can resolve outside the workspace (the `Remove-Item $home -Recurse` shape).
/// Returns the offending path reference marker.
fn detect_uncontained_recursive_delete(command_line: &str) -> Option<&'static str> {
    // Pad with spaces so ` rm `/` del ` style word-boundary markers match at the
    // command-line edges too.
    let haystack = format!(" {} ", command_line.to_ascii_lowercase());
    let has_recursive_verb = SHELL_RECURSIVE_DELETE_VERBS
        .iter()
        .any(|verb| haystack.contains(verb));
    if !has_recursive_verb {
        return None;
    }
    let has_recursive_flag = SHELL_RECURSIVE_FLAGS
        .iter()
        .any(|flag| haystack.contains(flag));
    if !has_recursive_flag {
        return None;
    }
    SHELL_UNCONTAINED_PATH_REFERENCES
        .iter()
        .copied()
        .find(|reference| haystack.contains(reference))
}

/// Resolves an uncontained path reference marker to the absolute path it would
/// evaluate to on this host, so the refusal can surface the real target.
#[cfg(windows)]
fn resolve_uncontained_path_reference(reference: &str) -> Option<String> {
    let key = match reference {
        "$home" | "${home}" | "$env:userprofile" => "USERPROFILE",
        "$env:homepath" => "HOMEPATH",
        "$env:homedrive" => "HOMEDRIVE",
        "$env:appdata" => "APPDATA",
        "$env:localappdata" => "LOCALAPPDATA",
        "$env:systemroot" | "$env:windir" => "SystemRoot",
        "$env:programfiles" => "ProgramFiles",
        "$env:programdata" => "ProgramData",
        "$profile" | "$pshome" => "USERPROFILE",
        _ => return None,
    };
    std::env::var(key).ok()
}

#[cfg(not(windows))]
fn resolve_uncontained_path_reference(_reference: &str) -> Option<String> {
    None
}

fn shell_command_line_from_parts(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(|part| quote_command_part(part))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Debug)]
struct ShellCommandMetadata {
    args: Vec<String>,
    command_line: String,
    args_redacted: bool,
    command_line_redacted: bool,
    args_original_count: usize,
    args_original_bytes: usize,
    args_sha256: String,
    command_line_original_bytes: usize,
    command_line_sha256: String,
}

fn default_shell_command_metadata_policy() -> String {
    "legacy_raw".to_owned()
}

fn shell_command_metadata(command: &str, args: &[String]) -> ShellCommandMetadata {
    let raw_command_line = shell_command_line_from_parts(command, args);
    let args_sha256 = shell_args_sha256(args);
    let command_line_sha256 = sha256_hex(raw_command_line.as_bytes());
    let args_original_bytes = args.iter().map(|arg| arg.len()).sum();
    let mut display_args = Vec::new();
    let mut args_redacted = false;

    for (index, arg) in args.iter().enumerate() {
        if index >= SHELL_ARGS_DISPLAY_MAX_ITEMS {
            args_redacted = true;
            display_args.push(format!(
                "[redacted:{}-additional-args:sha256={args_sha256}]",
                args.len() - index
            ));
            break;
        }
        let display = shell_arg_metadata_display(arg);
        if display != *arg {
            args_redacted = true;
        }
        display_args.push(display);
    }

    let mut display_command_line = shell_command_line_from_parts(command, &display_args);
    let mut command_line_redacted =
        args_redacted || raw_command_line.len() > SHELL_COMMAND_LINE_DISPLAY_MAX_BYTES;
    if display_command_line.len() > SHELL_COMMAND_LINE_DISPLAY_MAX_BYTES {
        command_line_redacted = true;
        display_command_line = format!(
            "{} [redacted-command-line:sha256={command_line_sha256}:bytes={}:args={}]",
            quote_command_part(command),
            raw_command_line.len(),
            args.len()
        );
    }

    ShellCommandMetadata {
        args: display_args,
        command_line: display_command_line,
        args_redacted,
        command_line_redacted,
        args_original_count: args.len(),
        args_original_bytes,
        args_sha256,
        command_line_original_bytes: raw_command_line.len(),
        command_line_sha256,
    }
}

fn shell_args_sha256(args: &[String]) -> String {
    let bytes = serde_json::to_vec(args).unwrap_or_else(|_error| args.join("\0").into_bytes());
    sha256_hex(&bytes)
}

fn shell_arg_metadata_display(arg: &str) -> String {
    if shell_arg_needs_metadata_redaction(arg) {
        return format!(
            "[redacted-arg:sha256={}:bytes={}]",
            sha256_hex(arg.as_bytes()),
            arg.len()
        );
    }
    arg.to_owned()
}

fn shell_arg_needs_metadata_redaction(arg: &str) -> bool {
    if arg.len() > SHELL_ARG_DISPLAY_MAX_BYTES || arg.contains(['\r', '\n']) {
        return true;
    }
    let lower = arg.to_ascii_lowercase();
    if [
        "authorization:",
        "bearer ",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "password",
        "passwd",
        "secret",
        "recovery_code",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return true;
    }
    shell_arg_looks_like_opaque_token(arg)
}

fn shell_arg_looks_like_opaque_token(arg: &str) -> bool {
    let trimmed = arg.trim_matches(['"', '\'']);
    if trimmed.len() < 32 || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let mut has_alpha = false;
    let mut has_digit = false;
    let mut token_chars = 0usize;
    for ch in trimmed.chars() {
        if ch.is_ascii_alphabetic() {
            has_alpha = true;
            token_chars += 1;
        } else if ch.is_ascii_digit() {
            has_digit = true;
            token_chars += 1;
        } else if matches!(ch, '-' | '_' | '.' | '=' | '+' | '/') {
            token_chars += 1;
        }
    }
    has_alpha && has_digit && token_chars == trimmed.chars().count()
}

fn shell_job_status_with_safe_command_metadata(
    status: &ActRunShellJobStatus,
) -> ActRunShellJobStatus {
    if status.command_metadata_policy == SHELL_COMMAND_METADATA_POLICY
        && !status.args_sha256.is_empty()
        && !status.command_line_sha256.is_empty()
    {
        return status.clone();
    }
    let metadata = shell_command_metadata(&status.command, &status.args);
    let mut safe = status.clone();
    safe.command_metadata_policy = SHELL_COMMAND_METADATA_POLICY.to_owned();
    safe.args = metadata.args;
    safe.command_line = metadata.command_line;
    safe.args_redacted = metadata.args_redacted;
    safe.command_line_redacted = metadata.command_line_redacted;
    safe.args_original_count = metadata.args_original_count;
    safe.args_original_bytes = metadata.args_original_bytes;
    safe.args_sha256 = metadata.args_sha256;
    safe.command_line_original_bytes = metadata.command_line_original_bytes;
    safe.command_line_sha256 = metadata.command_line_sha256;
    safe
}

fn launch_command_line(params: &ActLaunchParams) -> Result<String, ErrorData> {
    let target = resolve_launch_target_for_policy(&params.target)?;
    Ok(std::iter::once(&target)
        .chain(params.args.iter())
        .map(|part| quote_command_part(part))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(windows)]
fn resolve_launch_target_for_policy(target: &str) -> Result<String, ErrorData> {
    if !is_path_like_launch_target(target) {
        return Ok(target.to_owned());
    }

    win32_long_path_name(target).map_err(|error| {
        launch_tool_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("act_launch target path could not be resolved with GetLongPathNameW: {error}"),
            json!({
                "code": error_codes::ACTION_TARGET_INVALID,
                "target": target,
                "reason": "launch_target_path_resolution_failed",
            }),
        )
    })
}

#[cfg(not(windows))]
fn resolve_launch_target_for_policy(target: &str) -> Result<String, ErrorData> {
    Ok(target.to_owned())
}

#[cfg(windows)]
fn is_path_like_launch_target(target: &str) -> bool {
    if target.contains("://") {
        return false;
    }
    target.contains('\\')
        || target.contains('/')
        || target
            .as_bytes()
            .get(1)
            .is_some_and(|second| *second == b':')
}

#[cfg(windows)]
fn win32_long_path_name(target: &str) -> anyhow::Result<String> {
    use std::{
        ffi::{OsStr, OsString},
        os::windows::ffi::{OsStrExt, OsStringExt},
    };
    use windows::{Win32::Storage::FileSystem::GetLongPathNameW, core::PCWSTR};

    let wide_target = OsStr::new(target)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    // SAFETY: `wide_target` is explicitly NUL-terminated and remains alive for the call.
    let required = unsafe { GetLongPathNameW(PCWSTR(wide_target.as_ptr()), None) };
    if required == 0 {
        return Err(anyhow::Error::new(windows::core::Error::from_thread()))
            .with_context(|| format!("resolve launch target {target:?}"));
    }

    let mut buffer = vec![0; required as usize + 1];
    // SAFETY: the buffer is writable for its full length and the input pointer is valid.
    let written = unsafe { GetLongPathNameW(PCWSTR(wide_target.as_ptr()), Some(&mut buffer)) };
    if written == 0 {
        return Err(anyhow::Error::new(windows::core::Error::from_thread()))
            .with_context(|| format!("resolve launch target {target:?}"));
    }
    if written as usize >= buffer.len() {
        anyhow::bail!(
            "GetLongPathNameW returned {} characters for a {} character buffer",
            written,
            buffer.len()
        );
    }

    buffer.truncate(written as usize);
    Ok(OsString::from_wide(&buffer).to_string_lossy().into_owned())
}

fn quote_command_part(part: &str) -> String {
    if part.is_empty() {
        return "\"\"".to_owned();
    }
    if !part.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        return part.to_owned();
    }
    let escaped = part.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn posix_single_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn elapsed_ms_u32(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}

fn policy_error(code: &'static str, message: &'static str, data: serde_json::Value) -> ErrorData {
    tracing::warn!(code, "M4 policy gate denied tool invocation: {message}");
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

fn shell_tool_error(
    code: &'static str,
    message: impl Into<String>,
    data: serde_json::Value,
) -> ErrorData {
    let message = message.into();
    tracing::warn!(code, "M4 shell tool error: {message}");
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

fn idempotency_error(
    message: &'static str,
    reason: &'static str,
    details: &serde_json::Value,
) -> ErrorData {
    let mut data = json!({
        "code": error_codes::TOOL_PARAMS_INVALID,
        "reason": reason,
    });
    if let (Some(target), Some(source)) = (data.as_object_mut(), details.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

fn launch_tool_error(
    code: &'static str,
    message: impl Into<String>,
    data: serde_json::Value,
) -> ErrorData {
    let message = message.into();
    tracing::warn!(code, "M4 launch tool error: {message}");
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

const fn default_backend() -> Backend {
    Backend::Auto
}

const fn default_shell_timeout_ms() -> u64 {
    DEFAULT_SHELL_TIMEOUT_MS
}

fn deserialize_nullable_shell_timeout_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or_else(default_shell_timeout_ms))
}

const fn default_run_shell_execution_mode() -> ActRunShellExecutionMode {
    ActRunShellExecutionMode::Auto
}

const fn default_shell_job_tail_bytes() -> u64 {
    SHELL_JOB_TAIL_DEFAULT_BYTES
}

const fn default_launch_timeout_ms() -> u64 {
    DEFAULT_LAUNCH_TIMEOUT_MS
}

fn parse_env_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .map(|raw| raw.split(',').map(ToOwned::to_owned).collect())
        .unwrap_or_default()
}

#[derive(Copy, Clone)]
enum AllowPatternPolicy {
    Shell,
    Launch,
}

impl AllowPatternPolicy {
    const fn tool_name(self) -> &'static str {
        match self {
            Self::Shell => "act_run_shell",
            Self::Launch => "act_launch",
        }
    }

    const fn broad_code(self) -> &'static str {
        match self {
            Self::Shell => SHELL_PATTERN_TOO_BROAD,
            Self::Launch => LAUNCH_PATTERN_TOO_BROAD,
        }
    }

    const fn unanchored_reason(self) -> &'static str {
        match self {
            Self::Shell => "shell_pattern_must_match_full_command_line",
            Self::Launch => "launch_pattern_must_match_full_command_line",
        }
    }
}

fn compile_allow_patterns(
    source_name: &'static str,
    patterns: Vec<String>,
    policy: AllowPatternPolicy,
) -> anyhow::Result<Vec<AllowPattern>> {
    patterns
        .into_iter()
        .map(|raw| {
            validate_allow_pattern_source(source_name, &raw, policy)?;
            let regex = regex::RegexBuilder::new(&raw)
                .size_limit(ALLOW_PATTERN_SIZE_LIMIT_BYTES)
                .build()
                .with_context(|| {
                    format!("{source_name} pattern {raw:?} is not valid regex or exceeds the compiled-size limit")
                })?;
            validate_compiled_allow_pattern(source_name, &raw, &regex, policy)?;
            Ok(AllowPattern { raw, regex })
        })
        .collect()
}

fn validate_allow_pattern_source(
    source_name: &'static str,
    raw: &str,
    policy: AllowPatternPolicy,
) -> Result<(), BroadAllowPatternError> {
    if raw.trim().is_empty() {
        return Err(broad_allow_pattern(
            source_name,
            raw,
            "empty_pattern",
            policy,
        ));
    }
    if contains_unbounded_dot_repetition(raw) || contains_any_character_class_repetition(raw) {
        return Err(broad_allow_pattern(
            source_name,
            raw,
            "unbounded_any_character_repetition",
            policy,
        ));
    }
    if !has_full_command_anchors(raw) {
        return Err(broad_allow_pattern(
            source_name,
            raw,
            policy.unanchored_reason(),
            policy,
        ));
    }
    Ok(())
}

fn validate_compiled_allow_pattern(
    source_name: &'static str,
    raw: &str,
    regex: &regex::Regex,
    policy: AllowPatternPolicy,
) -> Result<(), BroadAllowPatternError> {
    if regex.is_match("") {
        return Err(broad_allow_pattern(
            source_name,
            raw,
            "matches_empty",
            policy,
        ));
    }
    if BROAD_COMMAND_PROBES
        .iter()
        .all(|probe| regex.is_match(probe))
    {
        return Err(broad_allow_pattern(
            source_name,
            raw,
            "matches_arbitrary_command_lines",
            policy,
        ));
    }
    Ok(())
}

const BROAD_COMMAND_PROBES: [&str; 4] = [
    "cmd.exe /c \"echo synapse-broad-probe\"",
    "powershell.exe -NoProfile -Command Get-Process",
    "notepad.exe",
    "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -EncodedCommand AAAA",
];

fn broad_allow_pattern(
    source_name: &'static str,
    raw: &str,
    reason: &'static str,
    policy: AllowPatternPolicy,
) -> BroadAllowPatternError {
    BroadAllowPatternError {
        source_name,
        tool_name: policy.tool_name(),
        code: policy.broad_code(),
        raw: raw.to_owned(),
        reason,
    }
}

fn has_full_command_anchors(raw: &str) -> bool {
    let pattern = strip_leading_global_flags(raw.trim());
    pattern.starts_with('^') && (pattern.ends_with('$') || pattern.ends_with("\\z"))
}

fn strip_leading_global_flags(pattern: &str) -> &str {
    let Some(rest) = pattern.strip_prefix("(?") else {
        return pattern;
    };
    let Some(close_index) = rest.find(')') else {
        return pattern;
    };
    let flags = &rest[..close_index];
    if flags.is_empty()
        || flags
            .chars()
            .any(|ch| !matches!(ch, 'i' | 'm' | 's' | 'R' | 'U' | 'u' | 'x' | '-'))
    {
        return pattern;
    }
    &rest[(close_index + 1)..]
}

fn contains_unbounded_dot_repetition(pattern: &str) -> bool {
    let mut escaped = false;
    let mut in_class = false;
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '.' if !in_class => {
                let Some(next) = chars.peek().copied() else {
                    continue;
                };
                if matches!(next, '*' | '+') {
                    return true;
                }
                if next == '{' && following_counted_repetition_is_unbounded(chars.clone()) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn following_counted_repetition_is_unbounded<I>(mut chars: I) -> bool
where
    I: Iterator<Item = char>,
{
    if chars.next() != Some('{') {
        return false;
    }
    let mut body = String::new();
    for ch in chars {
        if ch == '}' {
            return body.trim_end().ends_with(',');
        }
        body.push(ch);
    }
    false
}

fn contains_any_character_class_repetition(pattern: &str) -> bool {
    let compact = pattern
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>();
    [
        r"[\s\S]*", r"[\S\s]*", r"[\d\D]*", r"[\D\d]*", r"[\w\W]*", r"[\W\w]*", r"[\s\S]+",
        r"[\S\s]+", r"[\d\D]+", r"[\D\d]+", r"[\w\W]+", r"[\W\w]+",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn shell_search_tool_readback_resolves_windows_builtins() {
        // The readback resolves against the same child-process environment shell
        // jobs receive. `findstr` and `powershell` are Windows built-ins that
        // `ensure_windows_path_entries` always merges into the child PATH, so
        // they must resolve to real files — otherwise the readback (and the
        // shell env it describes) is broken. This is the FSV anchor: a missing
        // findstr here means the child PATH is not actually usable.
        let readback = shell_search_tool_readback();
        assert!(
            readback.starts_with("shell_search_tools "),
            "unexpected readback shape: {readback}"
        );
        assert!(
            readback.contains("documented_fallback=powershell_select_string"),
            "readback must name the documented fallback primitive: {readback}"
        );
        assert!(
            !readback.contains("findstr=absent"),
            "findstr is a Windows built-in on the child PATH and must resolve: {readback}"
        );
        assert!(
            !readback.contains("powershell=absent"),
            "powershell must resolve on the child PATH: {readback}"
        );
        // `primary` must never claim a tool the readback reports as absent.
        let primary = readback
            .split_whitespace()
            .find_map(|token| token.strip_prefix("primary="))
            .expect("readback must include a primary= token");
        assert!(
            matches!(primary, "rg" | "findstr" | "powershell_select_string"),
            "unexpected primary tool {primary}: {readback}"
        );
        if primary == "rg" {
            assert!(
                !readback.contains("rg=absent"),
                "primary=rg contradicts rg=absent: {readback}"
            );
        }
    }

    #[test]
    fn reserved_variable_assignment_detection() {
        // Collisions that must be refused.
        assert_eq!(
            detect_shell_reserved_variable_assignment("$home = \"C:\\temp\\calyx\""),
            Some("home")
        );
        assert_eq!(
            detect_shell_reserved_variable_assignment("$HOME=$scratch"),
            Some("home")
        );
        assert_eq!(
            detect_shell_reserved_variable_assignment("$Profile = 'x'"),
            Some("profile")
        );
        assert_eq!(
            detect_shell_reserved_variable_assignment("$PWD += 'x'"),
            Some("pwd")
        );
        // Safe: read-only use, env namespace, different identifier, RHS use.
        assert_eq!(
            detect_shell_reserved_variable_assignment("Join-Path $HOME 'sub'"),
            None
        );
        assert_eq!(
            detect_shell_reserved_variable_assignment("$env:HOME = 'x'"),
            None
        );
        assert_eq!(
            detect_shell_reserved_variable_assignment("$homedir = 'x'"),
            None
        );
        assert_eq!(
            detect_shell_reserved_variable_assignment("$scratch = $HOME"),
            None
        );
        assert_eq!(
            detect_shell_reserved_variable_assignment("if ($host -eq 'x') { 1 }"),
            None
        );
    }

    #[test]
    fn uncontained_recursive_delete_detection() {
        // The exact #1507 shape and variants must be refused.
        assert_eq!(
            detect_uncontained_recursive_delete("Remove-Item $home -Recurse -Force"),
            Some("$home")
        );
        assert_eq!(
            detect_uncontained_recursive_delete("rm -r $env:USERPROFILE\\scratch"),
            Some("$env:userprofile")
        );
        assert_eq!(
            detect_uncontained_recursive_delete("[System.IO.Directory]::Delete($profile, $true)"),
            Some("$profile")
        );
        // Safe: recursive delete of an explicit workspace path (no home ref).
        assert_eq!(
            detect_uncontained_recursive_delete(
                "Remove-Item C:\\code\\Synapse\\target\\fsv -Recurse -Force"
            ),
            None
        );
        // Safe: reference present but NOT recursive.
        assert_eq!(detect_uncontained_recursive_delete("Get-Item $home"), None);
        assert_eq!(
            detect_uncontained_recursive_delete("Remove-Item $home"),
            None
        );
    }

    #[test]
    fn validate_run_shell_params_refuses_reserved_variable_and_recursive_home_delete() {
        let collision = shell_params(
            "powershell.exe",
            vec!["-NoProfile", "-Command", "$home = 'C:\\temp\\x'"],
            1000,
        );
        let err = validate_run_shell_params(&collision)
            .expect_err("reserved variable assignment must be refused");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(serde_json::Value::as_str),
            Some(error_codes::SAFETY_SHELL_RESERVED_VARIABLE_COLLISION)
        );

        let uncontained = shell_params(
            "powershell.exe",
            vec![
                "-NoProfile",
                "-Command",
                "Remove-Item $home -Recurse -Force",
            ],
            1000,
        );
        let err = validate_run_shell_params(&uncontained)
            .expect_err("recursive home delete must be refused");
        let data = err.data.as_ref().expect("structured error data");
        assert_eq!(
            data.get("code").and_then(serde_json::Value::as_str),
            Some(error_codes::SAFETY_SHELL_RECURSIVE_DELETE_UNCONTAINED)
        );
        // The refusal must surface the resolved absolute target, not just the ref.
        #[cfg(windows)]
        assert!(
            data.get("resolved_target")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|target| !target.is_empty()),
            "refusal must surface the resolved home path"
        );
    }

    #[test]
    fn validate_run_shell_params_allows_workspace_recursive_delete() {
        let ok = shell_params(
            "powershell.exe",
            vec![
                "-NoProfile",
                "-Command",
                "Remove-Item C:\\code\\Synapse\\target\\fsv -Recurse -Force",
            ],
            1000,
        );
        validate_run_shell_params(&ok)
            .expect("recursive delete of an explicit workspace path must be allowed");
    }

    #[cfg(windows)]
    #[test]
    fn resolve_program_on_path_finds_and_misses() {
        let system32 = format!(
            "{}\\System32",
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned())
        );
        let pathext = WINDOWS_DEFAULT_PATHEXT;
        assert!(
            resolve_program_on_path("findstr", &system32, pathext).is_some(),
            "findstr.exe must resolve under System32"
        );
        assert!(
            resolve_program_on_path("synapse_definitely_not_a_real_tool_xyz", &system32, pathext)
                .is_none(),
            "a nonexistent tool must resolve to None, not a false positive"
        );
    }

    fn shell_config_for(params: &ActRunShellParams) -> M4ServiceConfig {
        match M4ServiceConfig::from_cli_parts(
            vec![format!("^{}$", regex::escape(&shell_command_line(params)))],
            Vec::new(),
            DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
        ) {
            Ok(config) => config,
            Err(error) => panic!("synthetic shell allowlist should compile: {error:#}"),
        }
    }

    fn shell_params(command: &str, args: Vec<&str>, timeout_ms: u64) -> ActRunShellParams {
        ActRunShellParams {
            command: command.to_owned(),
            args: args.into_iter().map(str::to_owned).collect(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms,
            execution_mode: ActRunShellExecutionMode::Auto,
            durable_timeout_ms: None,
            idempotency_key: None,
        }
    }

    fn temp_shell_job_paths(temp: &tempfile::TempDir) -> ShellJobPaths {
        ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        }
    }

    fn issue1277_ssh_status(
        job_id: &str,
        status: &str,
        paths: &ShellJobPaths,
    ) -> ActRunShellJobStatus {
        let params = ActRunShellStartParams {
            command: "ssh.exe".to_owned(),
            args: vec![
                "aiwonder".to_owned(),
                "bash -lc 'exec -a issue1277 sleep 600'".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some(job_id.to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "ssh.exe aiwonder \"bash -lc 'exec -a issue1277 sleep 600'\"".to_owned(),
            matched_pattern: "^ssh".to_owned(),
        };
        shell_job_status_record(
            job_id,
            status,
            &params,
            paths,
            "request-sha",
            &authorization,
            "2026-06-20T00:00:00Z".to_owned(),
            Some(1234),
            None,
        )
    }

    fn local_model_spawn_params(prompt: Option<&str>) -> ActSpawnAgentParams {
        spawn_params(ActSpawnAgentCli::LocalModel, prompt)
    }

    fn spawn_params(cli: ActSpawnAgentCli, prompt: Option<&str>) -> ActSpawnAgentParams {
        ActSpawnAgentParams {
            cli: Some(cli),
            kind: Some(cli),
            model: None,
            model_ref: cli.is_local_model().then(|| "qwen8v2-tool-live".to_owned()),
            prompt: prompt.map(str::to_owned),
            target: None,
            working_dir: Some(r"C:\code\Synapse".to_owned()),
            mcp_url: default_agent_spawn_mcp_url(),
            wait_timeout_ms: default_agent_spawn_wait_timeout_ms(),
            hold_open_ms: default_agent_spawn_hold_open_ms(),
            require_approval_gate: default_require_approval_gate(),
            template_id: None,
            template_version: None,
            template_config_hash: None,
        }
    }

    #[test]
    fn local_model_spawn_empty_prompt_errors_before_launch() {
        for prompt in [None, Some(""), Some("   \n\t   ")] {
            let params = local_model_spawn_params(prompt);
            let error = validate_agent_spawn_params(&params)
                .expect_err("blank local-model prompts must fail before launch");
            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("code")),
                Some(&json!(error_codes::TOOL_PARAMS_INVALID))
            );
            assert!(
                error
                    .message
                    .contains("local_model prompt must not be empty"),
                "{}",
                error.message
            );
        }

        validate_agent_spawn_params(&local_model_spawn_params(Some("call health once")))
            .expect("nonblank local-model prompt remains valid");
    }

    #[test]
    fn direct_primary_spawn_empty_prompt_errors_before_launch() {
        for cli in [ActSpawnAgentCli::Codex, ActSpawnAgentCli::Claude] {
            for prompt in [None, Some(""), Some("  \r\n\t  ")] {
                let params = spawn_params(cli, prompt);
                let error = validate_agent_spawn_params(&params)
                    .expect_err("blank direct primary-agent prompts must fail before launch");
                assert_eq!(
                    error.data.as_ref().and_then(|data| data.get("code")),
                    Some(&json!(error_codes::TOOL_PARAMS_INVALID))
                );
                assert!(
                    error
                        .message
                        .contains("direct spawn prompt must not be empty"),
                    "{}",
                    error.message
                );
            }

            validate_agent_spawn_params(&spawn_params(cli, Some("call health once")))
                .expect("nonblank direct primary-agent prompt remains valid");
        }
    }

    #[test]
    fn template_rendered_primary_spawn_prompt_remains_template_governed() {
        let mut params = spawn_params(ActSpawnAgentCli::Codex, Some("template task"));
        params.template_id = Some("issue1245-template".to_owned());
        params.template_version = Some(1);
        params.template_config_hash = Some("sha256:test".to_owned());

        validate_agent_spawn_params(&params)
            .expect("template-rendered nonblank prompt remains valid");
    }

    fn foreground_for_launch_selection(
        hwnd: i64,
        pid: u32,
        process_name: &str,
        window_title: &str,
    ) -> ForegroundContext {
        ForegroundContext {
            hwnd,
            pid,
            process_name: process_name.to_owned(),
            process_path: format!(r"C:\Synthetic\{process_name}"),
            window_title: window_title.to_owned(),
            window_bounds: synapse_core::Rect {
                x: 0,
                y: 0,
                w: 640,
                h: 480,
            },
            monitor_index: 0,
            dpi_scale: 1.0,
            profile_id: None,
            steam_appid: None,
            is_fullscreen: false,
            is_dwm_composed: true,
        }
    }

    #[cfg(windows)]
    #[test]
    fn child_environment_derives_windows_profile_paths_from_slim_daemon_env() {
        let mut env = BTreeMap::new();
        env.insert(
            "USERPROFILE".to_owned(),
            ("USERPROFILE".to_owned(), r"C:\Users\hotra".to_owned()),
        );
        env.insert(
            "SYSTEMDRIVE".to_owned(),
            ("SystemDrive".to_owned(), "D:".to_owned()),
        );

        add_windows_profile_environment(&mut env);

        assert_eq!(
            env.get("APPDATA").map(|(_key, value)| value.as_str()),
            Some(r"C:\Users\hotra\AppData\Roaming")
        );
        assert_eq!(
            env.get("LOCALAPPDATA").map(|(_key, value)| value.as_str()),
            Some(r"C:\Users\hotra\AppData\Local")
        );
        assert_eq!(
            env.get("PROGRAMDATA").map(|(_key, value)| value.as_str()),
            Some(r"D:\ProgramData")
        );
        println!(
            "readback=child_env edge=slim_daemon after_appdata={} after_localappdata={} after_programdata={}",
            env["APPDATA"].1, env["LOCALAPPDATA"].1, env["PROGRAMDATA"].1
        );
    }

    #[cfg(windows)]
    #[test]
    fn shell_spawn_command_prefers_git_ssh_for_bare_windows_ssh_family() {
        let dir = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp git ssh dir: {error}"));
        for leaf in ["ssh.exe", "scp.exe", "sftp.exe"] {
            std::fs::write(dir.path().join(leaf), b"synthetic git ssh binary")
                .unwrap_or_else(|error| panic!("write {leaf}: {error}"));
        }
        let dirs = vec![dir.path().to_path_buf()];

        let ssh = resolve_windows_ssh_family_spawn_command_with_dirs("ssh", &dirs)
            .unwrap_or_else(|| panic!("bare ssh should resolve"));
        let scp = resolve_windows_ssh_family_spawn_command_with_dirs("scp.exe", &dirs)
            .unwrap_or_else(|| panic!("bare scp.exe should resolve"));
        let sftp = resolve_windows_ssh_family_spawn_command_with_dirs("sftp", &dirs)
            .unwrap_or_else(|| panic!("bare sftp should resolve"));

        println!(
            "readback=act_run_shell_spawn_resolution edge=bare_ssh before=ssh/scp/sftp after_ssh={ssh} after_scp={scp} after_sftp={sftp}"
        );
        assert_eq!(ssh, dir.path().join("ssh.exe").to_string_lossy());
        assert_eq!(scp, dir.path().join("scp.exe").to_string_lossy());
        assert_eq!(sftp, dir.path().join("sftp.exe").to_string_lossy());

        assert_eq!(
            resolve_windows_ssh_family_spawn_command_with_dirs(
                r"C:\Windows\System32\OpenSSH\ssh.exe",
                &dirs
            ),
            None
        );
        assert_eq!(
            resolve_windows_ssh_family_spawn_command_with_dirs(r".\ssh.exe", &dirs),
            None
        );
        assert_eq!(
            resolve_windows_ssh_family_spawn_command_with_dirs("powershell.exe", &dirs),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn shell_spawn_command_does_not_use_incomplete_git_ssh_directory() {
        let dir = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create incomplete git ssh dir: {error}"));
        std::fs::write(dir.path().join("ssh.exe"), b"synthetic git ssh binary")
            .unwrap_or_else(|error| panic!("write ssh.exe: {error}"));
        let dirs = vec![dir.path().to_path_buf()];

        let resolved = resolve_windows_ssh_family_spawn_command_with_dirs("ssh", &dirs);

        println!(
            "readback=act_run_shell_spawn_resolution edge=incomplete_git_dir before=ssh_only after={resolved:?}"
        );
        assert_eq!(resolved, None);
    }

    #[cfg(windows)]
    #[test]
    fn child_path_prefers_git_ssh_before_windows_openssh() {
        let git_dir = r"C:\Program Files\Git\usr\bin";
        let openssh_dir = r"C:\Windows\System32\OpenSSH";
        let before = r"C:\Windows\System32;C:\Windows\System32\OpenSSH;C:\Program Files\Git\usr\bin;C:\Tools;C:\Windows\System32\OpenSSH";

        let after =
            reorder_semicolon_path_entry_before_targets(before, git_dir, &[openssh_dir.to_owned()]);
        let parts = after.split(';').collect::<Vec<_>>();
        let git_index = parts
            .iter()
            .position(|part| {
                normalize_semicolon_path_part(part) == normalize_semicolon_path_part(git_dir)
            })
            .unwrap_or_else(|| panic!("git ssh dir should be present"));
        let openssh_index = parts
            .iter()
            .position(|part| {
                normalize_semicolon_path_part(part) == normalize_semicolon_path_part(openssh_dir)
            })
            .unwrap_or_else(|| panic!("windows openssh dir should be present"));
        let git_count = parts
            .iter()
            .filter(|part| {
                normalize_semicolon_path_part(part) == normalize_semicolon_path_part(git_dir)
            })
            .count();

        println!("readback=child_env_path edge=git_before_openssh before={before} after={after}");
        assert!(git_index < openssh_index);
        assert_eq!(git_count, 1);
    }

    #[cfg(windows)]
    #[test]
    fn child_environment_preserves_explicit_appdata_from_daemon_env() {
        let mut env = BTreeMap::new();
        env.insert(
            "USERPROFILE".to_owned(),
            ("USERPROFILE".to_owned(), r"C:\Users\hotra".to_owned()),
        );
        env.insert(
            "APPDATA".to_owned(),
            ("APPDATA".to_owned(), r"E:\Roaming".to_owned()),
        );

        add_windows_profile_environment(&mut env);

        assert_eq!(
            env.get("APPDATA").map(|(_key, value)| value.as_str()),
            Some(r"E:\Roaming")
        );
        println!(
            "readback=child_env edge=explicit_appdata after_appdata={}",
            env["APPDATA"].1
        );
    }

    fn launch_config_for(params: &ActLaunchParams) -> M4ServiceConfig {
        let command_line = launch_command_line(params)
            .unwrap_or_else(|error| panic!("synthetic launch command line should build: {error}"));
        match M4ServiceConfig::from_cli_parts(
            Vec::new(),
            vec![format!("^{}$", regex::escape(&command_line))],
            DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
        ) {
            Ok(config) => config,
            Err(error) => panic!("synthetic launch allowlist should compile: {error:#}"),
        }
    }

    fn launch_params(target: &str, args: Vec<&str>, timeout_ms: u64) -> ActLaunchParams {
        ActLaunchParams {
            target: target.to_owned(),
            args: args.into_iter().map(str::to_owned).collect(),
            working_dir: None,
            env: BTreeMap::new(),
            wait_for_window_title_regex: None,
            timeout_ms,
            idempotency_key: None,
            cdp_debug: None,
            force_renderer_accessibility: None,
            windows_console_window_state: None,
            desktop: None,
        }
    }

    #[test]
    fn launch_desktop_option_rejects_invalid_shapes() {
        let cases = [
            ("", "desktop_empty_or_padded"),
            (" agent:session", "desktop_empty_or_padded"),
            ("agent:", "desktop_agent_scope_empty"),
            ("existing:", "desktop_existing_name_invalid"),
            ("existing:bad\\name", "desktop_existing_name_invalid"),
            ("default", "desktop_scope_unsupported"),
        ];

        for (desktop, reason) in cases {
            let mut params = launch_params("notepad.exe", Vec::new(), 10_000);
            params.desktop = Some(desktop.to_owned());
            let error = validate_launch_params(&params)
                .expect_err("invalid desktop shape should fail closed");
            println!(
                "readback=act_launch_desktop_validation edge={reason} before={desktop:?} after={:?}",
                error.data
            );
            assert_eq!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("reason"))
                    .and_then(|reason| reason.as_str()),
                Some(reason)
            );
        }
    }

    #[test]
    fn launch_desktop_option_accepts_window_wait_for_hidden_desktop_readback() {
        let mut params = launch_params(r"C:\Windows\notepad.exe", Vec::new(), 10_000);
        params.desktop = Some("agent:session".to_owned());
        params.wait_for_window_title_regex = Some("^owned-hidden-notepad$".to_owned());

        validate_launch_params(&params)
            .expect("desktop launch waits are supported through hidden-desktop enumeration");
    }

    #[test]
    fn launch_desktop_agent_scope_is_session_bound() {
        let error = prepare_launch_desktop(Some("agent:other-session"), Some("current-session"))
            .expect_err("agent desktop scope must match current session");

        println!(
            "readback=act_launch_desktop_scope edge=session_mismatch before=request:agent:other-session,current:current-session after={:?}",
            error.data
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("desktop_agent_session_mismatch")
        );
        assert_eq!(
            hidden_desktop_name_for_session("current-session"),
            hidden_desktop_name_for_session("current-session")
        );
        assert_ne!(
            hidden_desktop_name_for_session("current-session"),
            hidden_desktop_name_for_session("other-session")
        );
        assert!(hidden_desktop_name_for_session("current-session").len() <= 128);
    }

    #[test]
    fn chromium_cdp_launch_injects_ephemeral_port_and_dedicated_profile() {
        let params = launch_params("chrome.exe", vec!["https://example.com"], 10_000);
        let launch = chromium_cdp_launch(&params).expect("chrome.exe should get CDP injection");
        println!(
            "readback=cdp_launch edge=chrome before=args:{:?} after=injected:{:?} udd:{:?}",
            params.args, launch.injected_args, launch.user_data_dir
        );
        assert!(
            launch
                .injected_args
                .iter()
                .any(|arg| arg == "--remote-debugging-port=0")
        );
        assert!(
            launch
                .injected_args
                .iter()
                .any(|arg| arg.starts_with("--user-data-dir="))
        );
        assert!(
            launch
                .injected_args
                .iter()
                .any(|arg| arg == "--silent-debugger-extension-api")
        );
        assert!(
            launch
                .injected_args
                .iter()
                .any(|arg| arg == "--disable-extensions")
        );
        // The dedicated profile must be non-default (Chrome 136+ requirement).
        assert!(
            launch
                .user_data_dir
                .to_string_lossy()
                .contains("synapse-cdp-profiles")
        );

        let spawn_params = params_with_chromium_launch_args(&params, Some(&launch), None);
        // Injected flags precede the caller's URL so the positional arg parses.
        assert_eq!(
            spawn_params.args.first().map(String::as_str),
            Some("--remote-debugging-port=0")
        );
        assert_eq!(
            spawn_params.args.last().map(String::as_str),
            Some("https://example.com")
        );
    }

    #[test]
    fn chromium_cdp_launch_respects_opt_out_and_non_chromium() {
        let mut opted_out = launch_params("chrome.exe", vec![], 10_000);
        opted_out.cdp_debug = Some(false);
        println!("readback=cdp_launch edge=opt_out before=cdp_debug:Some(false)");
        assert!(chromium_cdp_launch(&opted_out).is_none());

        let notepad = launch_params("notepad.exe", vec![], 10_000);
        println!("readback=cdp_launch edge=non_chromium before=target:notepad.exe");
        assert!(chromium_cdp_launch(&notepad).is_none());
    }

    #[test]
    fn chromium_cdp_launch_defers_to_popup_safe_caller_supplied_flags() {
        let with_port = launch_params("msedge.exe", vec!["--remote-debugging-port=9222"], 10_000);
        println!(
            "readback=cdp_launch edge=caller_port before=args:{:?}",
            with_port.args
        );
        assert!(chromium_cdp_launch(&with_port).is_none());
        let error = validate_launch_params(&with_port).expect_err("unsafe debug launch must fail");
        assert_eq!(
            extract_error_code(&error),
            error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        );
        assert!(
            error
                .message
                .contains("refused a Chromium remote-debugging launch")
        );

        let with_profile = launch_params("chrome.exe", vec!["--user-data-dir=C:\\my"], 10_000);
        assert!(chromium_cdp_launch(&with_profile).is_none());

        let safe_profile = cdp_automation_profile_dir();
        let safe_profile_arg = format!("--user-data-dir={}", safe_profile.display());
        let safe_remote_debug = launch_params(
            "chrome.exe",
            vec![
                "--remote-debugging-port=0",
                safe_profile_arg.as_str(),
                "--silent-debugger-extension-api",
                "--disable-extensions",
                "about:blank",
            ],
            10_000,
        );
        println!(
            "readback=cdp_launch edge=caller_popup_safe before=args:{:?}",
            safe_remote_debug.args
        );
        validate_launch_params(&safe_remote_debug).expect("popup-safe caller CDP launch");

        let banner_profile = cdp_automation_profile_dir();
        let banner_profile_arg = format!("--user-data-dir={}", banner_profile.display());
        let banner_remote_debug = launch_params(
            "chrome.exe",
            vec![
                "--remote-debugging-pipe",
                banner_profile_arg.as_str(),
                "--silent-debugger-extension-api",
                "--disable-extensions",
                "--disable-blink-features=AutomationControlled",
                "about:blank",
            ],
            10_000,
        );
        let error = validate_launch_params(&banner_remote_debug)
            .expect_err("layout-shifting Chrome warning flags must fail closed");
        assert_eq!(
            extract_error_code(&error),
            error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        );
        assert!(error.message.contains("remote-debugging launch"));
        let error_text = format!("{error:?}");
        assert!(error_text.contains("has_layout_shifting_infobar_flags"));
        assert!(error_text.contains("AutomationControlled"));
    }

    #[test]
    fn chromium_renderer_accessibility_is_opt_in_and_chromium_only() {
        let mut params = launch_params("chrome.exe", vec!["https://example.com"], 10_000);
        println!(
            "readback=renderer_a11y edge=default before=force_renderer_accessibility:{:?}",
            params.force_renderer_accessibility
        );
        assert!(chromium_renderer_accessibility_arg(&params).is_none());

        params.force_renderer_accessibility = Some(true);
        let arg = chromium_renderer_accessibility_arg(&params);
        println!(
            "readback=renderer_a11y edge=opt_in before=args:{:?} after=arg:{arg:?}",
            params.args
        );
        assert_eq!(arg.as_deref(), Some("--force-renderer-accessibility"));

        let launch = chromium_cdp_launch(&params).expect("chrome should still get CDP launch");
        let spawn_params = params_with_chromium_launch_args(&params, Some(&launch), arg);
        assert!(
            spawn_params
                .args
                .iter()
                .any(|arg| arg == "--force-renderer-accessibility")
        );
        assert_eq!(
            spawn_params.args.last().map(String::as_str),
            Some("https://example.com")
        );

        let mut notepad = launch_params("notepad.exe", vec![], 10_000);
        notepad.force_renderer_accessibility = Some(true);
        assert!(chromium_renderer_accessibility_arg(&notepad).is_none());
    }

    #[test]
    fn chromium_renderer_accessibility_respects_caller_flag_and_truthy_env_values() {
        let mut caller = launch_params(
            "msedge.exe",
            vec!["--force-renderer-accessibility", "https://example.com"],
            10_000,
        );
        caller.force_renderer_accessibility = Some(true);
        assert!(
            chromium_renderer_accessibility_arg(&caller).is_none(),
            "caller-supplied flag must not be duplicated"
        );

        caller.args[0] = "--force-renderer-accessibility=complete".to_owned();
        assert!(
            chromium_renderer_accessibility_arg(&caller).is_none(),
            "caller-supplied valued flag must not be duplicated"
        );

        for value in ["1", "true", "yes", "on", " TRUE "] {
            assert!(truthy_value(value), "{value:?} should enable env opt-in");
        }
        for value in ["", "0", "false", "off", "no", "maybe"] {
            assert!(
                !truthy_value(value),
                "{value:?} should not enable env opt-in"
            );
        }
    }

    #[test]
    fn read_devtools_active_port_parses_first_line() {
        let dir = std::env::temp_dir().join(format!(
            "synapse-cdp-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let port_file = dir.join("DevToolsActivePort");
        std::fs::write(&port_file, "51234\n/devtools/browser/abc-123\n").expect("write port file");
        let port = read_devtools_active_port(&port_file);
        println!("readback=devtools_active_port before=file:{port_file:?} after=port:{port:?}");
        assert_eq!(port, Some(51234));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_requested_url_detects_browser_page_args() {
        let args = vec![
            "--new-window".to_owned(),
            "http://localhost:5173/polis".to_owned(),
        ];
        let url = launch_requested_url(&args);
        println!(
            "readback=act_launch_url edge=wsl_localhost_arg before=args:{args:?} after={url:?}"
        );
        assert_eq!(url.as_deref(), Some("http://localhost:5173/polis"));

        let app_args = vec!["--app=https://example.test/app".to_owned()];
        assert_eq!(
            launch_requested_url(&app_args).as_deref(),
            Some("https://example.test/app")
        );

        let non_url_args = vec!["--new-window".to_owned(), "not-a-url".to_owned()];
        assert!(launch_requested_url(&non_url_args).is_none());
    }

    #[tokio::test]
    async fn launch_url_verification_skips_when_synapse_did_not_open_cdp() {
        let mut opted_out = launch_params("chrome.exe", vec!["http://localhost:5173"], 10);
        opted_out.cdp_debug = Some(false);
        let result =
            verify_launched_chromium_url(&opted_out, None, &LaunchedCdp::default(), 10).await;
        println!(
            "readback=act_launch_url edge=cdp_opt_out before=cdp_launch:None after={result:?}"
        );
        assert!(matches!(result, Ok(None)));

        let non_chromium = launch_params("notepad.exe", vec!["http://localhost:5173"], 10);
        let result =
            verify_launched_chromium_url(&non_chromium, None, &LaunchedCdp::default(), 10).await;
        println!(
            "readback=act_launch_url edge=non_chromium before=cdp_launch:None after={result:?}"
        );
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn launch_url_matching_normalizes_root_trailing_slash() {
        assert!(url_matches(
            "http://localhost:5173",
            "http://localhost:5173/"
        ));
        assert!(url_matches(
            "https://example.test/path?q=1",
            "https://example.test/path?q=1"
        ));
        assert!(!url_matches(
            "http://localhost:5173/expected",
            "http://localhost:5173/other"
        ));
    }

    fn combo_press_step(at_ms: u32, key: &str) -> ActComboStep {
        ActComboStep {
            at_ms,
            action: ActComboAction::ActPress,
            params: json!({
                "keys": [key],
                "hold_ms": 1,
                "backend": "software",
            }),
            backend: None,
        }
    }

    fn combo_params(steps: Vec<ActComboStep>) -> ActComboParams {
        ActComboParams {
            steps,
            backend: Backend::Software,
            idempotency_key: None,
        }
    }

    fn assert_tool_params_invalid(error: &ErrorData) {
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn combo_rejects_empty_steps() {
        let error = match validate_combo_params(&combo_params(Vec::new())) {
            Ok(()) => panic!("empty combo should reject"),
            Err(error) => error,
        };

        assert_tool_params_invalid(&error);
        assert!(
            error
                .message
                .contains("steps must contain at least one step")
        );
    }

    #[test]
    fn combo_rejects_more_than_256_steps() {
        let steps = (0..=MAX_COMBO_STEPS)
            .map(|index| combo_press_step(u32::try_from(index).unwrap_or(u32::MAX), "f13"))
            .collect::<Vec<_>>();
        let error = match validate_combo_params(&combo_params(steps)) {
            Ok(()) => panic!("257-step combo should reject"),
            Err(error) => error,
        };

        assert_tool_params_invalid(&error);
        assert!(error.message.contains("exceeds max 256"));
    }

    #[test]
    fn combo_rejects_non_monotonic_steps() {
        let error = match validate_combo_params(&combo_params(vec![
            combo_press_step(10, "f14"),
            combo_press_step(9, "f15"),
        ])) {
            Ok(()) => panic!("non-monotonic combo should reject"),
            Err(error) => error,
        };

        assert_tool_params_invalid(&error);
        assert!(error.message.contains("at_ms must be monotonic"));
    }

    #[test]
    fn combo_rejects_motion_action_with_act_stroke_pointer() {
        let params = combo_params(vec![ActComboStep {
            at_ms: 0,
            action: ActComboAction::Retired("act_drag".to_owned()),
            params: json!({"path": [{"x": 0, "y": 0}, {"x": 10, "y": 0}]}),
            backend: None,
        }]);
        let error = match combo_steps_from_params(&params) {
            Ok(steps) => panic!("motion combo action should reject, got {steps:?}"),
            Err(error) => error,
        };

        assert_tool_params_invalid(&error);
        assert!(error.message.contains("act_drag"));
        assert!(error.message.contains("not combo-lowerable"));
        assert!(error.message.contains("Use act_stroke"));
    }

    #[test]
    fn combo_rejects_nested_press_backend_mismatch() {
        let params = ActComboParams {
            steps: vec![ActComboStep {
                at_ms: 0,
                action: ActComboAction::ActPress,
                params: json!({
                    "keys": ["f17"],
                    "hold_ms": 5,
                    "backend": "hardware"
                }),
                backend: None,
            }],
            backend: Backend::Software,
            idempotency_key: None,
        };

        let error = match combo_steps_from_params(&params) {
            Ok(steps) => panic!("mismatched backend should reject, got {steps:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert!(
            error
                .message
                .contains("act_press params.backend differs from top-level backend")
        );
    }

    #[test]
    fn combo_allows_nested_press_auto_backend_to_use_top_level_backend() {
        let params = ActComboParams {
            steps: vec![ActComboStep {
                at_ms: 0,
                action: ActComboAction::ActPress,
                params: json!({
                    "keys": ["f18"],
                    "hold_ms": 5,
                    "backend": "auto"
                }),
                backend: None,
            }],
            backend: Backend::Software,
            idempotency_key: None,
        };

        let steps = match combo_steps_from_params(&params) {
            Ok(steps) => steps,
            Err(error) => panic!("auto backend should lower: {error}"),
        };

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].at_ms, 0);
    }

    #[test]
    fn shell_command_line_quotes_empty_and_space_args() {
        let params = shell_params("cmd.exe", vec!["/c", "echo hello", ""], 30_000);

        assert_eq!(
            shell_command_line(&params),
            "cmd.exe /c \"echo hello\" \"\""
        );
    }

    // Regression for #1204: the witnessed SendKeys command used shell-based
    // global input to select a background Chrome tab, which landed on the human
    // foreground window. It must be denied before the allowlist check can permit
    // the exact command line.
    #[test]
    fn run_shell_rejects_global_sendkeys_input() {
        let params = shell_params(
            "powershell",
            vec![
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('^9'); Start-Sleep -Milliseconds 750",
            ],
            5_000,
        );
        let error = authorize_run_shell(&shell_config_for(&params), &params)
            .expect_err("a SendKeys global-input command must be denied");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::SAFETY_SHELL_GLOBAL_INPUT_DENIED))
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("matched_marker"))
                .and_then(|marker| marker.as_str()),
            Some("sendkeys")
        );
    }

    #[test]
    fn run_shell_rejects_each_global_input_marker() {
        for snippet in [
            "[System.Windows.Forms.SendKeys]::Send('a')",
            "$wsh.SendKeys('{ENTER}')",
            "[Win32]::SendInput($n, $inputs, $size)",
            "keybd_event(0x41, 0, 0, 0)",
            "mouse_event(2, 0, 0, 0, 0)",
            "[Win32]::SetCursorPos(10, 10)",
            "[Win32]::SetForegroundWindow($h)",
            "[Win32]::BringWindowToTop($h)",
            "AutoHotkey.exe send.ahk",
        ] {
            let params = shell_params("powershell", vec!["-Command", snippet], 5_000);
            let error = authorize_run_shell(&shell_config_for(&params), &params)
                .expect_err("global-input snippet must be denied");

            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("code")),
                Some(&json!(error_codes::SAFETY_SHELL_GLOBAL_INPUT_DENIED)),
                "global-input snippet must be denied: {snippet}"
            );
        }
    }

    #[test]
    fn run_shell_allows_ordinary_commands_without_global_input() {
        for params in [
            shell_params("cmd.exe", vec!["/c", "echo hello"], 5_000),
            shell_params("powershell", vec!["-Command", "Get-Process chrome"], 5_000),
            shell_params("git", vec!["status", "--short"], 5_000),
            shell_params("cargo", vec!["check", "-p", "synapse-core"], 5_000),
        ] {
            assert!(
                detect_shell_global_input(&shell_command_line(&params)).is_none(),
                "benign command must not be flagged as global input: {}",
                shell_command_line(&params)
            );
            authorize_run_shell(&shell_config_for(&params), &params)
                .unwrap_or_else(|error| panic!("benign command must authorize: {error}"));
        }
    }

    #[test]
    fn shell_command_metadata_redacts_large_and_secret_args() {
        let large_body = format!(
            "$body = @'\n{}\n'@; $body | gh issue comment 1 --body-file -",
            "SYN877-LARGE-BODY-DO-NOT-ECHO ".repeat(12)
        );
        let secret_arg = "synapse_token_0123456789abcdef0123456789abcdef";
        let args = vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            large_body.clone(),
            secret_arg.to_owned(),
        ];

        let metadata = shell_command_metadata("powershell.exe", &args);

        println!(
            "readback=act_run_shell_metadata edge=large_secret before=large_bytes:{} token_bytes:{} after={metadata:?}",
            large_body.len(),
            secret_arg.len()
        );
        assert!(metadata.args_redacted);
        assert!(metadata.command_line_redacted);
        assert_eq!(metadata.args_original_count, 4);
        assert!(metadata.args_sha256.len() == 64);
        assert!(metadata.command_line_sha256.len() == 64);
        assert!(
            !metadata
                .args
                .iter()
                .any(|arg| arg.contains("SYN877-LARGE-BODY"))
        );
        assert!(!metadata.args.iter().any(|arg| arg.contains(secret_arg)));
        assert!(!metadata.command_line.contains("SYN877-LARGE-BODY"));
        assert!(!metadata.command_line.contains(secret_arg));
    }

    #[test]
    fn shell_job_status_and_request_store_safe_command_metadata() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let raw_body = format!(
            "Write-Output '{}'",
            "SYN877-REQUEST-BODY-DO-NOT-PERSIST ".repeat(10)
        );
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                raw_body.clone(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue877-safe-metadata".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: shell_command_line_from_parts(&params.command, &params.args),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("start request should hash: {error}"));

        write_shell_job_request(&paths, &params, &request_sha, None)
            .unwrap_or_else(|error| panic!("request should write: {error}"));
        let status = shell_job_status_record(
            "issue877-safe-metadata",
            "running",
            &params,
            &paths,
            &request_sha,
            &authorization,
            "2026-06-11T00:00:00Z".to_owned(),
            Some(1234),
            None,
        );
        write_shell_job_status(&paths.status_path, &status)
            .unwrap_or_else(|error| panic!("status should write: {error}"));
        let request_json = std::fs::read_to_string(&paths.request_path)
            .unwrap_or_else(|error| panic!("request should read: {error}"));
        let status_json = std::fs::read_to_string(&paths.status_path)
            .unwrap_or_else(|error| panic!("status should read: {error}"));
        let read_status = read_shell_job_status(&paths.status_path, "issue877-safe-metadata")
            .unwrap_or_else(|error| panic!("status should decode: {error}"));

        println!(
            "readback=act_run_shell_metadata edge=status_request before=raw_bytes:{} after=request:{} status:{}",
            raw_body.len(),
            request_json,
            status_json
        );
        assert!(!request_json.contains("SYN877-REQUEST-BODY-DO-NOT-PERSIST"));
        assert!(!status_json.contains("SYN877-REQUEST-BODY-DO-NOT-PERSIST"));
        assert!(
            !read_status
                .command_line
                .contains("SYN877-REQUEST-BODY-DO-NOT-PERSIST")
        );
        assert!(read_status.args_redacted);
        assert!(read_status.command_line_redacted);
        assert_eq!(
            read_status.command_metadata_policy,
            SHELL_COMMAND_METADATA_POLICY
        );
        assert_eq!(read_status.args_original_count, 3);
        assert_eq!(read_status.request_sha256, request_sha);
        assert!(read_status.args_sha256.len() == 64);
        assert!(read_status.command_line_sha256.len() == 64);
    }

    #[test]
    fn shell_job_status_rewrite_has_no_missing_poll_window() {
        use std::{
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
            thread,
        };

        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Write-Output issue1012-status-race".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1012-status-race".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: shell_command_line_from_parts(&params.command, &params.args),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("start request should hash: {error}"));
        let mut status = shell_job_status_record(
            "issue1012-status-race",
            "running",
            &params,
            &paths,
            &request_sha,
            &authorization,
            "2026-06-15T00:00:00Z".to_owned(),
            Some(4242),
            None,
        );
        write_shell_job_status(&paths.status_path, &status)
            .unwrap_or_else(|error| panic!("initial status should write: {error}"));

        let stop = Arc::new(AtomicBool::new(false));
        let read_errors = Arc::new(AtomicUsize::new(0));
        let status_path = paths.status_path.clone();
        let reader_stop = Arc::clone(&stop);
        let reader_errors = Arc::clone(&read_errors);
        let reader = thread::spawn(move || {
            while !reader_stop.load(Ordering::Relaxed) {
                if read_shell_job_status(&status_path, "issue1012-status-race").is_err() {
                    reader_errors.fetch_add(1, Ordering::Relaxed);
                }
                thread::yield_now();
            }
        });

        for iteration in 0..1_000 {
            status.duration_ms = Some(iteration);
            status.status = if iteration % 2 == 0 {
                "running".to_owned()
            } else {
                "finalizing".to_owned()
            };
            write_shell_job_status(&paths.status_path, &status)
                .unwrap_or_else(|error| panic!("status rewrite should commit: {error}"));
        }

        stop.store(true, Ordering::Relaxed);
        reader
            .join()
            .unwrap_or_else(|error| panic!("reader thread should join: {error:?}"));
        let final_readback = read_shell_job_status(&paths.status_path, "issue1012-status-race")
            .unwrap_or_else(|error| panic!("final status should read: {error}"));

        println!(
            "readback=act_run_shell_status edge=status_replace_race before=1000_rewrites after=read_errors:{} final_status:{}",
            read_errors.load(Ordering::Relaxed),
            final_readback.status
        );
        assert_eq!(read_errors.load(Ordering::Relaxed), 0);
        assert_eq!(final_readback.job_id, "issue1012-status-race");
    }

    // #1509/#1568: the status reader tolerates the Windows atomic-replace window
    // (destination transiently reports NOT_FOUND while a writer's unique staging
    // sibling is renamed in) WITHOUT slowing down the genuinely-missing path.
    // Both arms are asserted against real filesystem state so a future change
    // that either drops the mid-replace tolerance or blanket-retries every
    // NOT_FOUND is caught. The mid-replace arm lands the file via the REAL
    // atomic write path (unique temp -> fsync -> rename), so the reader can only
    // ever observe a whole file — never the empty/partial slice a truncate-in-
    // place writer used to expose (the flaky failure surfaced in #1568).
    #[cfg(windows)]
    #[test]
    fn shell_status_read_notfound_gate_distinguishes_replace_from_missing() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp status dir: {error}"));
        let status_path = temp.path().join("status.json");

        // Arm 1 — genuinely missing: neither the target nor any staging sibling
        // exists, so the read must fail immediately rather than burning the
        // 500 ms replace-tolerance window.
        assert!(!status_path.exists());
        assert!(!shell_status_replace_in_flight(&status_path));
        let started = Instant::now();
        let missing = read_shell_status_bytes(&status_path);
        let missing_elapsed = started.elapsed();
        println!(
            "readback=read_shell_status_bytes edge=genuine_missing after=err:{} elapsed_ms:{}",
            missing.is_err(),
            missing_elapsed.as_millis()
        );
        assert!(missing.is_err(), "absent status file must error");
        assert_eq!(
            missing.err().and_then(|error| error.raw_os_error()),
            Some(2),
            "missing file must surface ERROR_FILE_NOT_FOUND"
        );
        assert!(
            missing_elapsed < Duration::from_millis(200),
            "genuine missing read must return promptly, took {missing_elapsed:?}"
        );

        // Arm 2 — mid-replace window: target absent but a writer's unique staging
        // sibling is present, so the reader retries. A concurrent thread
        // atomically lands the real file (unique temp -> fsync -> rename, exactly
        // as `write_shell_job_status` does); the read must then succeed with the
        // delivered WHOLE-file bytes, never an empty/partial slice. No timing
        // lower bound is asserted — correctness must not depend on the reader
        // happening to catch the window mid-flight.
        let delivered = br#"{"delivered":true}"#.to_vec();
        let seed_staging = shell_status_temp_path(&status_path);
        std::fs::write(&seed_staging, b"pending-replace")
            .unwrap_or_else(|error| panic!("seed staging sibling: {error}"));
        assert!(
            shell_status_replace_in_flight(&status_path),
            "seeded staging sibling must register as an in-flight replace"
        );
        let writer_status_path = status_path.clone();
        let writer_bytes = delivered.clone();
        let seed_for_writer = seed_staging.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let landing = shell_status_temp_path(&writer_status_path);
            write_shell_job_status_staging(&landing, &writer_bytes)
                .unwrap_or_else(|error| panic!("stage replacement status: {error}"));
            commit_shell_job_status_file(&landing, &writer_status_path, "issue1568-mid-replace")
                .unwrap_or_else(|error| panic!("commit replacement status: {error}"));
            let _ = std::fs::remove_file(&seed_for_writer);
        });
        let recovered = read_shell_status_bytes(&status_path);
        writer
            .join()
            .unwrap_or_else(|error| panic!("writer thread should join: {error:?}"));
        println!(
            "readback=read_shell_status_bytes edge=mid_replace after=ok:{}",
            recovered.is_ok()
        );
        assert_eq!(
            recovered.expect("mid-replace read must recover once the file lands"),
            delivered,
            "reader must return the freshly delivered whole-file bytes, never empty/partial"
        );
    }

    // #1568 root-cause reproduction: many threads rewriting the SAME durable
    // status file concurrently must never let a reader observe a corrupt,
    // partial, empty, or trailing-garbage state, and must not leak staging temp
    // files. Writers alternate between a SHORT and a LONG payload (via a large
    // `error_message`) so that the pre-fix shared `status.json.tmp` staging file
    // would interleave two `write_all`s — a shorter blob's tail left the longer
    // blob's bytes behind, exactly the `trailing characters at line N` corruption
    // the daemon logged. With per-write unique staging names this is impossible:
    // each writer renames its own complete, fsynced blob into place. The source
    // of truth checked here is the ACTUAL on-disk bytes (parsed back), not any
    // return value.
    #[test]
    fn shell_status_concurrent_multiwriter_never_corrupts() {
        use std::{
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
            thread,
        };

        const WRITERS: usize = 6;
        const WRITES_PER_WRITER: usize = 300;
        const READERS: usize = 4;

        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Write-Output issue1568-mw".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1568-mw".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: shell_command_line_from_parts(&params.command, &params.args),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("start request should hash: {error}"));
        let base = shell_job_status_record(
            "issue1568-mw",
            "running",
            &params,
            &paths,
            &request_sha,
            &authorization,
            "2026-07-12T00:00:00Z".to_owned(),
            Some(4242),
            None,
        );
        // Seed the initial complete status so readers always have a whole file.
        write_shell_job_status(&paths.status_path, &base)
            .unwrap_or_else(|error| panic!("initial status should write: {error}"));

        let stop = Arc::new(AtomicBool::new(false));
        let read_errors = Arc::new(AtomicUsize::new(0));
        let reads_ok = Arc::new(AtomicUsize::new(0));

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let status_path = paths.status_path.clone();
                let stop = Arc::clone(&stop);
                let read_errors = Arc::clone(&read_errors);
                let reads_ok = Arc::clone(&reads_ok);
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match read_shell_job_status(&status_path, "issue1568-mw") {
                            Ok(status) => {
                                assert_eq!(status.job_id, "issue1568-mw");
                                reads_ok.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => {
                                read_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        thread::yield_now();
                    }
                })
            })
            .collect();

        let writes_ok = Arc::new(AtomicUsize::new(0));
        let writes_failed_loud = Arc::new(AtomicUsize::new(0));
        let writes_failed_other = Arc::new(AtomicUsize::new(0));
        let writers: Vec<_> = (0..WRITERS)
            .map(|writer_index| {
                let paths = paths.clone();
                let mut status = base.clone();
                let writes_ok = Arc::clone(&writes_ok);
                let writes_failed_loud = Arc::clone(&writes_failed_loud);
                let writes_failed_other = Arc::clone(&writes_failed_other);
                thread::spawn(move || {
                    for iteration in 0..WRITES_PER_WRITER {
                        // Alternate a large vs empty `error_message` so successive
                        // serialized blobs differ sharply in length — the exact
                        // condition that turned a shared staging file into
                        // trailing-garbage before the fix.
                        if (writer_index + iteration) % 2 == 0 {
                            status.status = "running".to_owned();
                            status.error_message = Some("y".repeat(8192));
                        } else {
                            status.status = "finalizing".to_owned();
                            status.error_message = None;
                        }
                        status.duration_ms = Some(iteration as u64);
                        // A rewrite either fully commits (atomic rename) or fails
                        // LOUDLY. Under extreme OS/AV rename contention a commit can
                        // still fail with STORAGE_WRITE_FAILED after its bounded
                        // retries — that is correct fail-loud behavior, NOT
                        // corruption: the atomic rename leaves the on-disk file
                        // untouched, so a concurrent reader still sees the prior whole
                        // status. The #1568 invariant under test is "no reader ever
                        // observes a corrupt/partial file" (asserted below); a loud
                        // write failure is tolerated, a silent/other failure is not.
                        match write_shell_job_status(&paths.status_path, &status) {
                            Ok(()) => {
                                writes_ok.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(error) if error.to_string().contains("STORAGE_WRITE_FAILED") => {
                                writes_failed_loud.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => {
                                writes_failed_other.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                })
            })
            .collect();

        for writer in writers {
            writer
                .join()
                .unwrap_or_else(|error| panic!("writer thread should join: {error:?}"));
        }
        stop.store(true, Ordering::Relaxed);
        for reader in readers {
            reader
                .join()
                .unwrap_or_else(|error| panic!("reader thread should join: {error:?}"));
        }

        // Source of truth #1: the final on-disk bytes parse as a whole status.
        let final_status = read_shell_job_status(&paths.status_path, "issue1568-mw")
            .unwrap_or_else(|error| panic!("final status must parse: {error}"));
        // Source of truth #2: not a single reader observed a corrupt/partial read
        // across the whole concurrent run.
        let observed_errors = read_errors.load(Ordering::Relaxed);
        let observed_ok = reads_ok.load(Ordering::Relaxed);
        let ok = writes_ok.load(Ordering::Relaxed);
        let failed_loud = writes_failed_loud.load(Ordering::Relaxed);
        let failed_other = writes_failed_other.load(Ordering::Relaxed);
        // Source of truth #3: no staging temp file leaked in the job dir beyond
        // the rare loud write failures whose best-effort cleanup could also lose
        // the AV race (a clean run leaks none).
        let leaked: Vec<String> = fs::read_dir(temp.path())
            .unwrap_or_else(|error| panic!("scan job dir: {error}"))
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp."))
            .collect();

        println!(
            "readback=act_run_shell_status edge=concurrent_multiwriter before={WRITERS}x{WRITES_PER_WRITER}_rewrites after=read_errors:{observed_errors} reads_ok:{observed_ok} writes_ok:{ok} writes_failed_loud:{failed_loud} writes_failed_other:{failed_other} final_status:{} leaked_temps:{}",
            final_status.status,
            leaked.len()
        );
        // THE #1568 invariant: atomicity — no reader ever observed a
        // corrupt/partial/empty status across the whole concurrent storm.
        assert_eq!(
            observed_errors, 0,
            "no reader may observe a corrupt/partial/empty status ({observed_errors} did)"
        );
        assert!(observed_ok > 0, "readers must have completed real reads");
        // Every write failure, if any, is a LOUD STORAGE_WRITE_FAILED (old file
        // intact) — never a silent or unexpected failure.
        assert_eq!(
            failed_other, 0,
            "a write failure must surface as a loud STORAGE_WRITE_FAILED, never silent/other ({failed_other} were)"
        );
        assert!(ok > 0, "at least some concurrent writes must commit");
        assert_eq!(final_status.job_id, "issue1568-mw");
        assert!(
            leaked.len() <= failed_loud,
            "staging temp leaks ({}) must not exceed loud write failures ({failed_loud}): {leaked:?}",
            leaked.len()
        );
    }

    #[test]
    fn shell_job_reconciliation_preserves_monitor_terminal_status() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Write-Output issue970-ok".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue970-reconcile".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: shell_command_line_from_parts(&params.command, &params.args),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("start request should hash: {error}"));
        let mut terminal = shell_job_status_record(
            "issue970-reconcile",
            "ok",
            &params,
            &paths,
            &request_sha,
            &authorization,
            "2026-06-14T00:00:00Z".to_owned(),
            Some(4242),
            None,
        );
        terminal.exit_code = Some(0);
        terminal.completed_at = Some("2026-06-14T00:00:01Z".to_owned());
        terminal.duration_ms = Some(1000);
        write_shell_job_status(&paths.status_path, &terminal)
            .unwrap_or_else(|error| panic!("terminal status should write: {error}"));

        let mut finalizing = terminal.clone();
        finalizing.status = "finalizing".to_owned();
        finalizing.exit_code = None;
        finalizing.completed_at = Some("2026-06-14T00:00:02Z".to_owned());
        finalizing.duration_ms = Some(2000);

        let preserved = write_shell_job_reconciliation_status(&paths, finalizing)
            .unwrap_or_else(|error| panic!("reconcile write should preserve terminal: {error}"));
        let readback = read_shell_job_status(&paths.status_path, "issue970-reconcile")
            .unwrap_or_else(|error| panic!("status should read: {error}"));

        println!(
            "readback=act_run_shell_status edge=terminal_preservation before=candidate:finalizing after=file_status:{} exit_code:{:?}",
            readback.status, readback.exit_code
        );
        assert_eq!(preserved.status, "ok");
        assert_eq!(preserved.exit_code, Some(0));
        assert_eq!(readback.status, "ok");
        assert_eq!(readback.exit_code, Some(0));

        let mut exited_unobserved = terminal.clone();
        exited_unobserved.status = "exited_unobserved".to_owned();
        exited_unobserved.exit_code = None;
        exited_unobserved.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
        exited_unobserved.error_message =
            Some("job process exited before the monitor persisted final status".to_owned());
        let preserved_after_unobserved =
            write_shell_job_reconciliation_status(&paths, exited_unobserved).unwrap_or_else(
                |error| panic!("reconcile write should not downgrade terminal: {error}"),
            );
        let readback_after_unobserved =
            read_shell_job_status(&paths.status_path, "issue970-reconcile")
                .unwrap_or_else(|error| panic!("status should read after unobserved: {error}"));

        println!(
            "readback=act_run_shell_status edge=terminal_preservation before=candidate:exited_unobserved after=file_status:{} exit_code:{:?}",
            readback_after_unobserved.status, readback_after_unobserved.exit_code
        );
        assert_eq!(preserved_after_unobserved.status, "ok");
        assert_eq!(preserved_after_unobserved.exit_code, Some(0));
        assert_eq!(readback_after_unobserved.status, "ok");
        assert_eq!(readback_after_unobserved.exit_code, Some(0));
    }

    // #1334: a durable job whose status still claims "running" but whose backing
    // process is dead must be reconciled off the live set, not retained forever.
    // Source of truth = the persisted status file on disk after reconcile.
    #[test]
    fn reconcile_demotes_running_job_with_dead_pid_off_live_set() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Start-Sleep -Seconds 600".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1334-phantom".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: shell_command_line_from_parts(&params.command, &params.args),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("start request should hash: {error}"));
        // A PID that cannot be alive (max u32, never a real Windows PID).
        let dead_pid = u32::MAX - 1;
        let phantom = shell_job_status_record(
            "issue1334-phantom",
            "running",
            &params,
            &paths,
            &request_sha,
            &authorization,
            "2026-06-14T00:00:00Z".to_owned(),
            Some(dead_pid),
            None,
        );
        write_shell_job_status(&paths.status_path, &phantom)
            .unwrap_or_else(|error| panic!("plant phantom status: {error}"));

        // Precondition: status string alone classifies it live (the old bug).
        assert!(shell_job_live_status(&phantom.status));
        // But PID-backed liveness already knows it is dead.
        assert!(!shell_job_process_still_running(&phantom));

        let reconciled = reconcile_shell_job_process_state(phantom, &paths)
            .unwrap_or_else(|error| panic!("reconcile should succeed: {error}"));
        println!(
            "readback=reconcile edge=running_dead_pid before=running after=status:{}",
            reconciled.status
        );
        assert_ne!(reconciled.status, "running");
        assert!(!shell_job_process_still_running(&reconciled));

        // Source of truth: re-read the persisted file — the phantom is no longer
        // a live "running" job on disk.
        let on_disk = read_shell_job_status(&paths.status_path, "issue1334-phantom")
            .unwrap_or_else(|error| panic!("status should read after reconcile: {error}"));
        assert_ne!(on_disk.status, "running");
        assert!(!shell_job_live_status(&on_disk.status));
    }

    #[test]
    fn shell_job_status_readback_preserves_terminal_monitor_status() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        std::fs::write(&paths.stdout_path, b"issue989-ok\r\n")
            .unwrap_or_else(|error| panic!("stdout should write: {error}"));
        std::fs::write(&paths.stderr_path, b"")
            .unwrap_or_else(|error| panic!("stderr should write: {error}"));
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Write-Output issue989-ok".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue989-status-readback".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: shell_command_line_from_parts(&params.command, &params.args),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("start request should hash: {error}"));
        let mut terminal = shell_job_status_record(
            "issue989-status-readback",
            "ok",
            &params,
            &paths,
            &request_sha,
            &authorization,
            "2026-06-14T00:00:00Z".to_owned(),
            Some(4242),
            None,
        );
        terminal.exit_code = Some(0);
        terminal.completed_at = Some("2026-06-14T00:00:01Z".to_owned());
        terminal.duration_ms = Some(1000);
        write_shell_job_status(&paths.status_path, &terminal)
            .unwrap_or_else(|error| panic!("terminal status should write: {error}"));

        let mut stale_finalizing = terminal.clone();
        stale_finalizing.status = "finalizing".to_owned();
        stale_finalizing.exit_code = None;
        stale_finalizing.completed_at = Some("2026-06-14T00:00:02Z".to_owned());
        stale_finalizing.duration_ms = Some(2000);
        stale_finalizing.diagnostics = Some(shell_job_status_diagnostics(
            &stale_finalizing,
            false,
            13,
            0,
        ));

        let (persisted, running) =
            write_shell_job_status_readback(&paths, stale_finalizing, false, 13, 0).unwrap_or_else(
                |error| panic!("status readback should preserve terminal: {error}"),
            );
        let readback = read_shell_job_status(&paths.status_path, "issue989-status-readback")
            .unwrap_or_else(|error| panic!("status should read after readback write: {error}"));

        println!(
            "readback=act_run_shell_status edge=diagnostic_write_race before=candidate:finalizing/null-exit after=file_status:{} exit_code:{:?} diagnostics:{}",
            readback.status,
            readback.exit_code,
            readback.diagnostics.is_some()
        );
        assert!(!running);
        assert_eq!(persisted.status, "ok");
        assert_eq!(persisted.exit_code, Some(0));
        assert!(persisted.diagnostics.is_some());
        assert_eq!(readback.status, "ok");
        assert_eq!(readback.exit_code, Some(0));
        assert!(readback.diagnostics.is_some());
    }

    #[test]
    fn shell_monitor_persists_terminal_status_before_remote_cleanup() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "ssh.exe".to_owned(),
            args: vec!["aiwonder".to_owned(), "printf issue1244-ok".to_owned()],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1244-local-terminal-before-cleanup".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "ssh.exe aiwonder 'printf issue1244-ok'".to_owned(),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("start request should hash: {error}"));
        let mut terminal = shell_job_status_record(
            "issue1244-local-terminal-before-cleanup",
            "ok",
            &params,
            &paths,
            &request_sha,
            &authorization,
            "2026-06-18T00:00:00Z".to_owned(),
            Some(4242),
            None,
        );
        let metadata =
            parse_remote_process_metadata(
                "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1244-local-terminal-before-cleanup pid=12345 pgid=12345 sid=12345\n",
                "issue1244-local-terminal-before-cleanup",
            )
            .unwrap_or_else(|| panic!("remote marker should parse"));
        apply_remote_process_metadata(&mut terminal, metadata);
        terminal.exit_code = Some(0);
        terminal.completed_at = Some("2026-06-18T00:00:01Z".to_owned());
        terminal.duration_ms = Some(1000);

        let mut stale_finalizing = terminal.clone();
        stale_finalizing.status = "finalizing".to_owned();
        stale_finalizing.exit_code = None;
        stale_finalizing.completed_at = Some("2026-06-18T00:00:02Z".to_owned());
        stale_finalizing.duration_ms = Some(2000);
        write_shell_job_status(&paths.status_path, &stale_finalizing)
            .unwrap_or_else(|error| panic!("stale finalizing status should write: {error}"));

        persist_shell_job_local_terminal_status(&paths, &terminal);
        let readback = read_shell_job_status(
            &paths.status_path,
            "issue1244-local-terminal-before-cleanup",
        )
        .unwrap_or_else(|error| {
            panic!("status should read after local terminal prewrite: {error}")
        });

        println!(
            "readback=act_run_shell_start edge=local_terminal_pre_remote_cleanup before=status:finalizing exit_code:None remote:{} after=status:{} exit_code:{:?} remote:{}",
            stale_finalizing.remote_process_scope.remote_cleanup_status,
            readback.status,
            readback.exit_code,
            readback.remote_process_scope.remote_cleanup_status
        );
        assert_eq!(readback.status, "ok");
        assert_eq!(readback.exit_code, Some(0));
        assert_eq!(
            readback.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_TRACKED
        );
        assert!(!readback.remote_process_scope.remote_cleanup_verified);
    }

    #[test]
    fn shell_reconciliation_preserves_successful_terminal_status_against_late_nonzero_candidate() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "ssh.exe".to_owned(),
            args: vec!["aiwonder".to_owned(), "printf issue1251-ok".to_owned()],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1251-preserve-ok".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "ssh.exe aiwonder 'printf issue1251-ok'".to_owned(),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("start request should hash: {error}"));
        let mut latest_ok = shell_job_status_record(
            "issue1251-preserve-ok",
            "ok",
            &params,
            &paths,
            &request_sha,
            &authorization,
            "2026-06-18T00:00:00Z".to_owned(),
            Some(4242),
            None,
        );
        let metadata =
            parse_remote_process_metadata(
                "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1251-preserve-ok pid=12345 pgid=12345 sid=12345\n",
                "issue1251-preserve-ok",
            )
            .unwrap_or_else(|| panic!("remote marker should parse"));
        apply_remote_process_metadata(&mut latest_ok, metadata);
        latest_ok.remote_process_scope.remote_cleanup_verified = true;
        latest_ok.remote_process_scope.remote_cleanup_status =
            SHELL_REMOTE_CLEANUP_VERIFIED.to_owned();
        latest_ok.exit_code = Some(0);
        latest_ok.completed_at = Some("2026-06-18T00:00:01Z".to_owned());
        latest_ok.duration_ms = Some(1000);
        write_shell_job_status(&paths.status_path, &latest_ok)
            .unwrap_or_else(|error| panic!("latest ok status should write: {error}"));

        let mut late_nonzero = latest_ok.clone();
        late_nonzero.status = "exit_nonzero".to_owned();
        late_nonzero.exit_code = Some(1);
        late_nonzero.completed_at = Some("2026-06-18T00:00:02Z".to_owned());
        late_nonzero.duration_ms = Some(2000);
        late_nonzero.remote_process_scope.remote_cleanup_message = Some(
            "late cleanup/readback candidate must not downgrade the remote command verdict"
                .to_owned(),
        );

        let persisted = write_shell_job_reconciliation_status(&paths, late_nonzero)
            .unwrap_or_else(|error| panic!("status readback should preserve success: {error}"));
        let readback = read_shell_job_status(&paths.status_path, "issue1251-preserve-ok")
            .unwrap_or_else(|error| panic!("status should read after reconciliation: {error}"));

        println!(
            "readback=act_run_shell_status edge=preserve_successful_terminal before=file_status:ok exit_code:0 candidate:exit_nonzero/1 after=file_status:{} exit_code:{:?} remote:{}",
            readback.status,
            readback.exit_code,
            readback.remote_process_scope.remote_cleanup_status
        );
        assert_eq!(persisted.status, "ok");
        assert_eq!(persisted.exit_code, Some(0));
        assert_eq!(readback.status, "ok");
        assert_eq!(readback.exit_code, Some(0));
        assert_eq!(
            readback.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_VERIFIED
        );
    }

    #[test]
    fn shell_remote_scope_classifies_direct_ssh_with_destination() {
        let args = vec![
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-p".to_owned(),
            "22".to_owned(),
            "aiwonder".to_owned(),
            "sleep".to_owned(),
            "60".to_owned(),
        ];

        let scope = ssh_remote_process_scope(
            r"C:\Windows\System32\OpenSSH\ssh.exe",
            &args,
            "regression_direct",
        );

        println!(
            "readback=act_run_shell_remote_scope edge=direct_ssh before=args:{args:?} after={scope:?}"
        );
        assert_eq!(scope.transport, SHELL_REMOTE_TRANSPORT_SSH);
        assert_eq!(scope.remote_identity.as_deref(), Some("aiwonder"));
        assert!(scope.remote_cleanup_required);
        assert!(!scope.remote_cleanup_verified);
        assert_eq!(
            scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_TRACKING_PENDING
        );
        assert!(
            scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence.contains(SHELL_REMOTE_PROCESS_MARKER))
        );
    }

    #[test]
    fn shell_remote_scope_ssh_option_parser_is_case_sensitive() {
        let background_args = vec!["-f".to_owned(), "aiwonder".to_owned()];
        let config_args = vec![
            "-F".to_owned(),
            r"C:\tmp\ssh_config".to_owned(),
            "aiwonder".to_owned(),
        ];

        let background_identity = ssh_remote_identity(&background_args);
        let config_identity = ssh_remote_identity(&config_args);

        println!(
            "readback=act_run_shell_remote_scope edge=ssh_option_case before=-f:{background_args:?},-F:{config_args:?} after=-f:{background_identity:?},-F:{config_identity:?}"
        );
        assert_eq!(background_identity.as_deref(), Some("aiwonder"));
        assert_eq!(config_identity.as_deref(), Some("aiwonder"));
    }

    #[test]
    fn shell_remote_tracking_plan_wraps_direct_ssh_remote_command() {
        let args = vec![
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "aiwonder".to_owned(),
            "bash -lc 'exec -a synapse940 sleep 60'".to_owned(),
        ];

        let plan = ssh_remote_tracking_plan("ssh.exe", &args, "issue940-track")
            .expect("direct ssh remote command should be tracking-capable");

        println!(
            "readback=act_run_shell_remote_tracking edge=wrap before=args:{args:?} after={plan:?}"
        );
        assert_eq!(plan.remote_identity, "aiwonder");
        assert_eq!(
            plan.remote_command,
            "bash -lc 'exec -a synapse940 sleep 60'"
        );
        assert_eq!(plan.spawn_args[0], "-o");
        assert_eq!(plan.spawn_args[1], "BatchMode=yes");
        assert_eq!(plan.spawn_args[2], "aiwonder");
        let remote_wrapper = plan
            .spawn_args
            .last()
            .expect("wrapper command should be appended after destination");
        assert!(remote_wrapper.contains("setsid sh -c"));
        assert!(remote_wrapper.contains("SYNAPSE_REMOTE_PROCESS_V1 job_id=issue940-track"));
        assert!(remote_wrapper.contains("bash -lc"));
    }

    #[test]
    fn shell_wrapped_powershell_ssh_remote_command_is_tracked() {
        let args = vec![
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "ssh -o BatchMode=yes aiwonder \"cd /repo/calyx && cargo test -p calyx-aster --test soak_ph58 -- --nocapture --test-threads=1\""
                .to_owned(),
        ];
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: args.clone(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1019-powershell-ssh".to_owned()),
        };

        let invocation = shell_job_ssh_command_invocation(&params.command, &params.args)
            .expect("single PowerShell SSH command should be parseable");
        let scope = shell_job_remote_process_scope_from_start_params(&params);
        let spawn_plan = shell_job_spawn_plan(&params, "issue1019-powershell-ssh");

        println!(
            "readback=act_run_shell_remote_tracking edge=powershell_ssh before=command:{} args:{args:?} after=invocation:{invocation:?} scope:{scope:?} spawn:{spawn_plan:?}",
            params.command
        );
        assert_eq!(invocation.command, "ssh");
        assert_eq!(invocation.evidence, "shell_wrapped_ssh:powershell");
        assert_eq!(scope.transport, SHELL_REMOTE_TRANSPORT_SSH);
        assert_eq!(scope.remote_identity.as_deref(), Some("aiwonder"));
        assert_eq!(
            scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_TRACKING_PENDING
        );
        assert!(
            scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence.contains("shell_wrapped_ssh:powershell"))
        );
        assert_eq!(spawn_plan.command, "ssh");
        assert!(spawn_plan.args.last().is_some_and(|arg| {
            arg.contains("SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1019-powershell-ssh")
        }));
    }

    #[test]
    fn shell_wrapped_complex_powershell_script_is_not_guessed_as_trackable_ssh() {
        let args = vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Write-Output before; ssh aiwonder sleep 60".to_owned(),
        ];
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: args.clone(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1019-complex-powershell".to_owned()),
        };

        let invocation = shell_job_ssh_command_invocation(&params.command, &params.args);
        let scope = shell_job_remote_process_scope_from_start_params(&params);
        let spawn_plan = shell_job_spawn_plan(&params, "issue1019-complex-powershell");

        println!(
            "readback=act_run_shell_remote_tracking edge=complex_powershell before=command:{} args:{args:?} after=invocation:{invocation:?} scope:{scope:?} spawn:{spawn_plan:?}",
            params.command
        );
        assert!(invocation.is_none());
        assert_eq!(scope.transport, SHELL_REMOTE_TRANSPORT_LOCAL);
        assert_eq!(
            scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_NOT_APPLICABLE
        );
        assert_eq!(spawn_plan.command, "powershell.exe");
        assert_eq!(spawn_plan.args, args);
    }

    #[test]
    fn shell_wrapped_powershell_ssh_with_escaped_remote_quotes_is_not_rewritten() {
        let script = "ssh -o BatchMode=yes -i //wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519 -l croyse aiwonder \"sh -lc 'd=$HOME/synapse_issue1259; mkdir -p \\\"$d\\\"; printf 0 > \\\"$d/remote.rc\\\"; exit 0'\"";
        let args = vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ];
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: args.clone(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1259-escaped-powershell".to_owned()),
        };

        let invocation = shell_job_ssh_command_invocation(&params.command, &params.args);
        let scope = shell_job_remote_process_scope_from_start_params(&params);
        let spawn_plan = shell_job_spawn_plan(&params, "issue1259-escaped-powershell");

        println!(
            "readback=act_run_shell_remote_tracking edge=escaped_powershell_quotes before=script:{script:?} after=invocation:{invocation:?} scope:{scope:?} spawn:{spawn_plan:?}"
        );
        assert!(invocation.is_none());
        assert_eq!(scope.transport, SHELL_REMOTE_TRANSPORT_LOCAL);
        assert_eq!(
            scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_NOT_APPLICABLE
        );
        assert_eq!(spawn_plan.command, "powershell.exe");
        assert_eq!(spawn_plan.args, args);
    }

    #[test]
    fn shell_remote_tracking_plan_refuses_ssh_modes_without_cleanup_handle() {
        let forwarding = vec![
            "-N".to_owned(),
            "-L".to_owned(),
            "127.0.0.1:1:127.0.0.1:1".to_owned(),
            "aiwonder".to_owned(),
        ];
        let subsystem = vec!["-s".to_owned(), "aiwonder".to_owned(), "sftp".to_owned()];

        let forwarding_plan = ssh_remote_tracking_plan("ssh.exe", &forwarding, "issue940-forward");
        let subsystem_plan = ssh_remote_tracking_plan("ssh.exe", &subsystem, "issue940-subsystem");
        let subsystem_scope =
            ssh_remote_process_scope("ssh.exe", &subsystem, "regression_subsystem");

        println!(
            "readback=act_run_shell_remote_tracking edge=unsupported before=-N:{forwarding:?},-s:{subsystem:?} after=-N:{forwarding_plan:?},-s:{subsystem_plan:?},scope:{subsystem_scope:?}"
        );
        assert!(forwarding_plan.is_none());
        assert!(subsystem_plan.is_none());
        assert_eq!(
            subsystem_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_NOT_TRACKED
        );
        assert!(
            subsystem_scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence.contains("remote_tracking_unsupported"))
        );
    }

    #[test]
    fn shell_terminal_tracking_pending_without_marker_is_loudly_unverified() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        std::fs::write(&paths.stdout_path, b"")
            .unwrap_or_else(|error| panic!("write stdout log: {error}"));
        std::fs::write(&paths.stderr_path, b"")
            .unwrap_or_else(|error| panic!("write stderr log: {error}"));
        let params = ActRunShellStartParams {
            command: "ssh.exe".to_owned(),
            args: vec!["aiwonder".to_owned(), "true".to_owned()],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue972-no-marker".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "ssh.exe aiwonder true".to_owned(),
            matched_pattern: "^ssh".to_owned(),
        };
        let mut status = shell_job_status_record(
            "issue972-no-marker",
            "ok",
            &params,
            &paths,
            "request-sha",
            &authorization,
            "2026-06-14T00:00:00Z".to_owned(),
            Some(1234),
            None,
        );

        verify_shell_job_remote_cleanup_after_terminal(
            &mut status,
            &paths,
            "regression_terminal_readback",
            None,
        );

        println!(
            "readback=act_run_shell_remote_cleanup edge=terminal_no_marker before=tracking_pending after={:?}",
            status.remote_process_scope
        );
        assert_eq!(
            status.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_UNVERIFIED
        );
        assert_eq!(
            status.error_code.as_deref(),
            Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
        );
        assert!(!status.remote_process_scope.remote_cleanup_verified);
    }

    #[test]
    fn shell_terminal_pre_marker_parse_failure_is_classified_without_cleanup_unverified() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let stderr = "bash: -c: line 1: unexpected EOF while looking for matching `''\n";
        std::fs::write(&paths.stdout_path, b"")
            .unwrap_or_else(|error| panic!("write stdout log: {error}"));
        std::fs::write(&paths.stderr_path, stderr)
            .unwrap_or_else(|error| panic!("write stderr log: {error}"));
        let params = ActRunShellStartParams {
            command: "ssh.exe".to_owned(),
            args: vec![
                "-p".to_owned(),
                "22231".to_owned(),
                "-i".to_owned(),
                "issue1231_key".to_owned(),
                "aiwonder".to_owned(),
                "bash -lc 'echo issue1231".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1231-pre-marker-parse".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "ssh.exe -p 22231 -i issue1231_key aiwonder \"bash -lc 'echo issue1231\""
                .to_owned(),
            matched_pattern: "^ssh".to_owned(),
        };
        let mut status = shell_job_status_record(
            "issue1231-pre-marker-parse",
            "exit_nonzero",
            &params,
            &paths,
            "request-sha",
            &authorization,
            "2026-06-17T00:00:00Z".to_owned(),
            Some(1234),
            None,
        );
        status.exit_code = Some(2);

        verify_shell_job_remote_cleanup_after_terminal(
            &mut status,
            &paths,
            "regression_terminal_readback",
            None,
        );

        println!(
            "readback=act_run_shell_remote_cleanup edge=pre_marker_parse before=tracking_pending stderr={stderr:?} after={:?}",
            status.remote_process_scope
        );
        assert_eq!(
            status.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_PRE_MARKER_TERMINAL
        );
        assert!(!status.remote_process_scope.remote_cleanup_required);
        assert!(!status.remote_process_scope.remote_cleanup_verified);
        assert_eq!(status.remote_process_scope.remote_cleanup_error_code, None);
        assert_eq!(status.error_code, None);
        assert!(
            status
                .remote_process_scope
                .remote_cleanup_message
                .as_deref()
                .is_some_and(|message| message.contains(
                    "suggested_safe_readback=ssh.exe -p 22231 -i issue1231_key aiwonder"
                ))
        );
        assert!(
            status
                .remote_process_scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence
                    == "remote_tracking_pre_marker_terminal:remote_shell_unmatched_quote")
        );
    }

    #[test]
    fn shell_terminal_not_tracked_ssh_status_is_preserved() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        std::fs::write(&paths.stdout_path, b"")
            .unwrap_or_else(|error| panic!("write stdout log: {error}"));
        std::fs::write(&paths.stderr_path, b"")
            .unwrap_or_else(|error| panic!("write stderr log: {error}"));
        let params = ActRunShellStartParams {
            command: "ssh.exe".to_owned(),
            args: vec!["-N".to_owned(), "aiwonder".to_owned()],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue972-not-tracked".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "ssh.exe -N aiwonder".to_owned(),
            matched_pattern: "^ssh".to_owned(),
        };
        let mut status = shell_job_status_record(
            "issue972-not-tracked",
            "ok",
            &params,
            &paths,
            "request-sha",
            &authorization,
            "2026-06-14T00:00:00Z".to_owned(),
            Some(1234),
            None,
        );

        verify_shell_job_remote_cleanup_after_terminal(
            &mut status,
            &paths,
            "regression_terminal_readback",
            None,
        );

        println!(
            "readback=act_run_shell_remote_cleanup edge=terminal_not_tracked before=not_tracked after={:?}",
            status.remote_process_scope
        );
        assert_eq!(
            status.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_NOT_TRACKED
        );
        assert!(status.error_code.is_none());
    }

    #[test]
    fn issue1277_shell_terminal_tracked_ssh_transport_reset_defers_remote_cleanup() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = temp_shell_job_paths(&temp);
        let stderr = "noise before marker\n\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1277-reset pid=3487519 pgid=3487519 sid=3487519\n\
Read from remote host aiwonder.mst.com: Connection reset by peer\r\n\
client_loop: send disconnect: Connection reset by peer\r\n";
        std::fs::write(&paths.stdout_path, b"")
            .unwrap_or_else(|error| panic!("write stdout log: {error}"));
        std::fs::write(&paths.stderr_path, stderr)
            .unwrap_or_else(|error| panic!("write stderr log: {error}"));
        let mut status = issue1277_ssh_status("issue1277-reset", "exit_nonzero", &paths);
        status.exit_code = Some(255);

        verify_shell_job_remote_cleanup_after_terminal(
            &mut status,
            &paths,
            "regression_terminal_readback",
            None,
        );

        println!(
            "readback=act_run_shell_remote_cleanup issue=1277 edge=transport_reset before=status:exit_nonzero exit_code:255 stderr={stderr:?} after=status:{} remote:{:?}",
            status.status, status.remote_process_scope
        );
        assert_eq!(status.status, SHELL_JOB_STATUS_REMOTE_TRANSPORT_LOST);
        assert_eq!(
            status.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_TRANSPORT_LOST
        );
        assert!(status.remote_process_scope.remote_cleanup_required);
        assert!(!status.remote_process_scope.remote_cleanup_verified);
        assert_eq!(
            status.remote_process_scope.remote_process_id.as_deref(),
            Some("3487519")
        );
        assert_eq!(
            status
                .remote_process_scope
                .remote_process_group_id
                .as_deref(),
            Some("3487519")
        );
        assert_eq!(status.remote_process_scope.remote_cleanup_error_code, None);
        assert_eq!(status.error_code, None);
        assert!(
            status
                .remote_process_scope
                .remote_cleanup_message
                .as_deref()
                .is_some_and(|message| message.contains("act_run_shell_cancel"))
        );
        assert!(
            status
                .remote_process_scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence == "remote_transport_lost:ssh_connection_reset")
        );
        assert!(
            status
                .remote_process_scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence == "remote_cleanup_deferred_until_explicit_cancel")
        );

        let after_first_readback =
            serde_json::to_value(&status.remote_process_scope).expect("remote scope serializes");
        verify_shell_job_remote_cleanup_after_terminal(
            &mut status,
            &paths,
            "regression_terminal_readback",
            None,
        );
        assert_eq!(
            serde_json::to_value(&status.remote_process_scope).expect("remote scope serializes"),
            after_first_readback
        );
    }

    #[test]
    fn issue1277_shell_transport_loss_detection_skips_cancel_timeout_and_unrelated_exit_255() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = temp_shell_job_paths(&temp);
        let stderr = "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1277-guard pid=3487519 pgid=3487519 sid=3487519\n\
client_loop: send disconnect: Connection reset by peer\r\n";
        std::fs::write(&paths.stdout_path, b"")
            .unwrap_or_else(|error| panic!("write stdout log: {error}"));
        std::fs::write(&paths.stderr_path, stderr)
            .unwrap_or_else(|error| panic!("write stderr log: {error}"));
        let mut base = issue1277_ssh_status("issue1277-guard", "exit_nonzero", &paths);
        base.exit_code = Some(255);
        let metadata = parse_remote_process_metadata(stderr, "issue1277-guard")
            .expect("remote marker should parse");
        apply_remote_process_metadata(&mut base, metadata);

        let closed_by_remote = remote_transport_lost_evidence(
            "Connection to aiwonder.mst.com closed by remote host.\r\n",
        )
        .expect("OpenSSH closed-by-remote-host stderr must classify as transport loss");
        assert_eq!(closed_by_remote.reason, "ssh_closed_by_remote_host");

        let mut cancel_requested = base.clone();
        cancel_requested.cancel_requested = true;
        let mut timed_out = base.clone();
        timed_out.timed_out = true;
        let mut non_255_exit = base.clone();
        non_255_exit.exit_code = Some(1);
        let mut successful_status = base.clone();
        successful_status.status = "ok".to_owned();
        successful_status.exit_code = Some(0);

        for (label, mut edge) in [
            ("cancel_requested", cancel_requested),
            ("timed_out", timed_out),
            ("non_255_exit", non_255_exit),
            ("successful_status", successful_status),
        ] {
            let before_status = edge.status.clone();
            let before_cleanup_status = edge.remote_process_scope.remote_cleanup_status.clone();
            let detected = mark_shell_job_remote_transport_lost_if_detected(
                &mut edge,
                &paths,
                "regression_terminal_readback",
            )
            .unwrap_or_else(|error| {
                panic!("{label} transport detection should read stderr: {error}")
            });
            println!(
                "readback=act_run_shell_remote_cleanup issue=1277 edge={label} before=status:{before_status} cleanup:{before_cleanup_status} after=status:{} cleanup:{} detected:{detected}",
                edge.status, edge.remote_process_scope.remote_cleanup_status
            );
            assert!(
                !detected,
                "{label} must not be classified as transport loss"
            );
            assert_eq!(edge.status, before_status);
            assert_eq!(
                edge.remote_process_scope.remote_cleanup_status,
                before_cleanup_status
            );
        }

        let no_transport_stderr = "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1277-guard pid=3487519 pgid=3487519 sid=3487519\nexit 255 without an SSH transport-loss string\n";
        std::fs::write(&paths.stderr_path, no_transport_stderr)
            .unwrap_or_else(|error| panic!("write non-transport stderr log: {error}"));
        let mut no_transport = base.clone();
        let detected = mark_shell_job_remote_transport_lost_if_detected(
            &mut no_transport,
            &paths,
            "regression_terminal_readback",
        )
        .unwrap_or_else(|error| panic!("non-transport detection should read stderr: {error}"));
        println!(
            "readback=act_run_shell_remote_cleanup issue=1277 edge=no_transport_pattern before=exit_nonzero/255 after=status:{} cleanup:{} detected:{detected}",
            no_transport.status, no_transport.remote_process_scope.remote_cleanup_status
        );
        assert!(!detected);
        assert_eq!(no_transport.status, "exit_nonzero");
        assert_eq!(
            no_transport.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_TRACKED
        );
    }

    #[test]
    fn issue1274_shell_status_marks_remote_already_gone_local_transport_stale() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = temp_shell_job_paths(&temp);
        let mut status = issue1277_ssh_status("issue1274-stale", "running", &paths);
        status.pid = Some(4242);
        let metadata =
            parse_remote_process_metadata(
                "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1274-stale pid=2266815 pgid=2266815 sid=2266815\n",
                "issue1274-stale",
            )
            .unwrap_or_else(|| panic!("remote marker should parse"));
        apply_remote_process_metadata(&mut status, metadata);

        mark_shell_job_remote_already_gone_local_stale(
            &mut status,
            "regression_status_remote_liveness",
            "terminated",
            &[],
            None,
        );

        println!(
            "readback=act_run_shell_status issue=1274 edge=remote_already_gone before=status:running remote:tracked after=status:{} cleanup:{} verified:{} required:{}",
            status.status,
            status.remote_process_scope.remote_cleanup_status,
            status.remote_process_scope.remote_cleanup_verified,
            status.remote_process_scope.remote_cleanup_required
        );
        assert_eq!(status.status, SHELL_JOB_STATUS_REMOTE_EXITED_LOCAL_STALE);
        assert!(shell_job_terminal_status(&status.status));
        assert_eq!(status.exit_code, None);
        assert!(status.completed_at.is_some());
        assert!(status.duration_ms.is_some());
        assert!(!status.remote_process_scope.remote_cleanup_required);
        assert!(status.remote_process_scope.remote_cleanup_verified);
        assert_eq!(
            status.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_ALREADY_GONE
        );
        assert_eq!(status.remote_process_scope.remote_cleanup_error_code, None);
        assert_eq!(status.error_code, None);
        assert!(
            status
                .remote_process_scope
                .remote_cleanup_message
                .as_deref()
                .is_some_and(|message| message.contains("already gone"))
        );
        assert!(
            status
                .remote_process_scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence == "remote_process_already_gone_before_cancel")
        );
        assert!(
            status
                .remote_process_scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence == "local_transport_stale_termination:terminated")
        );
    }

    #[test]
    fn issue1274_remote_liveness_marker_parser_distinguishes_alive_and_gone() {
        let alive =
            format!("{SHELL_REMOTE_LIVENESS_MARKER} pid=2266815 pgid=2266815 status=alive\n");
        let gone = format!(
            "{SHELL_REMOTE_LIVENESS_MARKER} pid=2266815 pgid=2266815 status=already_gone\n"
        );
        let wrong_pid =
            format!("{SHELL_REMOTE_LIVENESS_MARKER} pid=1 pgid=2266815 status=already_gone\n");
        let command = ssh_remote_liveness_command("2266815", "2266815");

        println!(
            "readback=act_run_shell_status issue=1274 edge=liveness_parse alive={:?} gone={:?} command={command:?}",
            parse_remote_liveness_status(&alive, "2266815", "2266815"),
            parse_remote_liveness_status(&gone, "2266815", "2266815")
        );
        assert_eq!(
            parse_remote_liveness_status(&alive, "2266815", "2266815").as_deref(),
            Some("alive")
        );
        assert_eq!(
            parse_remote_liveness_status(&gone, "2266815", "2266815").as_deref(),
            Some("already_gone")
        );
        assert_eq!(
            parse_remote_liveness_status(&wrong_pid, "2266815", "2266815"),
            None
        );
        assert!(command.contains(SHELL_REMOTE_LIVENESS_MARKER));
        assert!(command.contains("ps -o pgid="));
        assert!(!command.contains("kill -TERM"));
        assert!(!command.contains("kill -KILL"));
    }

    #[test]
    fn issue1274_remote_exit_marker_zero_marks_stale_transport_success() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = temp_shell_job_paths(&temp);
        let mut status = issue1277_ssh_status("issue1274-exit-zero", "exit_nonzero", &paths);
        status.exit_code = Some(1);
        let stderr = "\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1274-exit-zero pid=2266815 pgid=2266815 sid=2266815
SYNAPSE_REMOTE_EXIT_V1 job_id=issue1274-exit-zero pid=2266815 pgid=2266815 exit_code=0
";
        std::fs::write(&paths.stderr_path, stderr)
            .unwrap_or_else(|error| panic!("write remote exit stderr: {error}"));
        refresh_shell_job_remote_metadata_from_outputs(&mut status, &paths)
            .unwrap_or_else(|error| panic!("remote process marker should read: {error}"));

        let reconciled = reconcile_shell_job_remote_exit_marker(
            &mut status,
            &paths,
            false,
            "regression_remote_exit_marker",
        )
        .unwrap_or_else(|error| panic!("remote exit marker should read: {error}"));

        println!(
            "readback=act_run_shell_status issue=1274 edge=remote_exit_zero before=local_exit_nonzero after=status:{} exit_code:{:?} cleanup:{} reconciled:{reconciled}",
            status.status, status.exit_code, status.remote_process_scope.remote_cleanup_status
        );
        assert!(reconciled);
        assert_eq!(status.status, SHELL_JOB_STATUS_REMOTE_EXITED_LOCAL_STALE);
        assert_eq!(status.exit_code, Some(0));
        assert!(status.remote_process_scope.remote_cleanup_verified);
        assert_eq!(
            status.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_ALREADY_GONE
        );
        assert!(
            status
                .remote_process_scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence
                    == "remote_exit_marker:SYNAPSE_REMOTE_EXIT_V1:pid=2266815:pgid=2266815:exit_code=0")
        );
    }

    #[test]
    fn issue1274_remote_exit_marker_nonzero_does_not_hide_remote_failure() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = temp_shell_job_paths(&temp);
        let mut status = issue1277_ssh_status("issue1274-exit-nonzero", "exit_nonzero", &paths);
        status.exit_code = Some(7);
        let stderr = "\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1274-exit-nonzero pid=2266815 pgid=2266815 sid=2266815
SYNAPSE_REMOTE_EXIT_V1 job_id=issue1274-exit-nonzero pid=2266815 pgid=2266815 exit_code=7
";
        std::fs::write(&paths.stderr_path, stderr)
            .unwrap_or_else(|error| panic!("write remote exit stderr: {error}"));
        refresh_shell_job_remote_metadata_from_outputs(&mut status, &paths)
            .unwrap_or_else(|error| panic!("remote process marker should read: {error}"));

        let reconciled = reconcile_shell_job_remote_exit_marker(
            &mut status,
            &paths,
            false,
            "regression_remote_exit_marker",
        )
        .unwrap_or_else(|error| panic!("remote exit marker should read: {error}"));

        println!(
            "readback=act_run_shell_status issue=1274 edge=remote_exit_nonzero before=exit_nonzero/7 after=status:{} exit_code:{:?} cleanup:{} reconciled:{reconciled}",
            status.status, status.exit_code, status.remote_process_scope.remote_cleanup_status
        );
        assert!(!reconciled);
        assert_eq!(status.status, "exit_nonzero");
        assert_eq!(status.exit_code, Some(7));
        assert_eq!(
            status.remote_process_scope.remote_cleanup_status,
            SHELL_REMOTE_CLEANUP_TRACKED
        );
        assert!(
            status
                .remote_process_scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence
                    == "remote_exit_marker:SYNAPSE_REMOTE_EXIT_V1:pid=2266815:pgid=2266815:exit_code=7")
        );
    }

    #[test]
    fn issue1283_bash_login_errexit_exit_one_surfaces_specific_hint() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = temp_shell_job_paths(&temp);
        let mut status = issue1277_ssh_status("issue1283-bash-login", "exit_nonzero", &paths);
        status.exit_code = Some(1);
        status.args = vec![
            "-l".to_owned(),
            "croyse".to_owned(),
            "aiwonder.mst.com".to_owned(),
            "bash -lc 'set +e; true; EC=$?; set -e; printf \"inner_exit=%s\\n\" \"$EC\"; exit \"$EC\"'"
                .to_owned(),
        ];
        status.remote_process_scope =
            ssh_remote_process_scope("ssh.exe", &status.args, "regression_issue1283");

        let diagnostics = shell_job_status_diagnostics(&status, false, 23, 211);

        println!(
            "readback=act_run_shell_status issue=1283 edge=bash_login_errexit hints={:?}",
            diagnostics.actionable_hints
        );
        assert!(diagnostics.actionable_hints.iter().any(|hint| hint
            == "bash_login_shell_errexit_can_override_inner_exit_status_use_bash_c_or_disable_errexit_before_exit"));
    }

    #[test]
    fn issue1283_non_login_bash_errexit_exit_one_keeps_generic_ssh_hints() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = temp_shell_job_paths(&temp);
        let mut status = issue1277_ssh_status("issue1283-bash-non-login", "exit_nonzero", &paths);
        status.exit_code = Some(1);
        status.args = vec![
            "-l".to_owned(),
            "croyse".to_owned(),
            "aiwonder.mst.com".to_owned(),
            "bash -c 'set +e; true; EC=$?; set -e; printf \"inner_exit=%s\\n\" \"$EC\"; exit \"$EC\"'"
                .to_owned(),
        ];
        status.remote_process_scope =
            ssh_remote_process_scope("ssh.exe", &status.args, "regression_issue1283");

        let diagnostics = shell_job_status_diagnostics(&status, false, 23, 211);

        println!(
            "readback=act_run_shell_status issue=1283 edge=non_login_bash hints={:?}",
            diagnostics.actionable_hints
        );
        assert!(!diagnostics.actionable_hints.iter().any(|hint| hint
            == "bash_login_shell_errexit_can_override_inner_exit_status_use_bash_c_or_disable_errexit_before_exit"));
        assert!(
            diagnostics
                .actionable_hints
                .iter()
                .any(|hint| hint == "check_remote_command_tty_stdin_and_auth_prompts")
        );
    }

    #[test]
    fn ssh_cleanup_command_parts_prefers_live_original_args_over_safe_status_args() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let safe_args = vec![
            "-i".to_owned(),
            "[redacted-arg:sha256=deadbeef:bytes=48]".to_owned(),
            "-l".to_owned(),
            "croyse".to_owned(),
            "aiwonder.mst.com".to_owned(),
            "true".to_owned(),
        ];
        let original_args = vec![
            "-i".to_owned(),
            "//wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519".to_owned(),
            "-l".to_owned(),
            "croyse".to_owned(),
            "aiwonder.mst.com".to_owned(),
            "true".to_owned(),
        ];
        let params = ActRunShellStartParams {
            command: "ssh".to_owned(),
            args: safe_args,
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue972-redacted-status".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line:
                "ssh -i [redacted-arg:sha256=deadbeef:bytes=48] -l croyse aiwonder.mst.com true"
                    .to_owned(),
            matched_pattern: "^ssh".to_owned(),
        };
        let status = shell_job_status_record(
            "issue972-redacted-status",
            "ok",
            &params,
            &paths,
            "request-sha",
            &authorization,
            "2026-06-14T00:00:00Z".to_owned(),
            Some(1234),
            None,
        );

        let live_invocation = shell_job_cleanup_invocation(&status, Some(&original_args), None)
            .unwrap_or_else(|| panic!("parse live original cleanup invocation"));
        let live_parts = ssh_direct_command_parts(&live_invocation.args)
            .unwrap_or_else(|| panic!("parse live original cleanup args"));
        assert!(
            live_parts
                .control_args
                .iter()
                .any(|arg| arg == "//wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519")
        );
        assert!(
            !live_parts
                .control_args
                .iter()
                .any(|arg| arg.contains("[redacted-arg:"))
        );

        let persisted_invocation = shell_job_cleanup_invocation(&status, None, None)
            .unwrap_or_else(|| panic!("parse persisted cleanup invocation"));
        let persisted_parts = ssh_direct_command_parts(&persisted_invocation.args)
            .unwrap_or_else(|| panic!("parse persisted cleanup args"));
        assert!(
            persisted_parts
                .control_args
                .iter()
                .any(|arg| arg.contains("[redacted-arg:"))
        );
    }

    #[test]
    fn shell_wrapped_ssh_cleanup_sidecar_survives_redacted_status_args() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let remote_body = format!("bash -lc 'exec -a issue1019 {}'", "sleep 600 ".repeat(80));
        let script = format!(
            "ssh -o BatchMode=yes -i //wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519 -l croyse aiwonder.mst.com \"{remote_body}\""
        );
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                script,
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue1019-sidecar".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: shell_command_line_from_parts(&params.command, &params.args),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let status = shell_job_status_record(
            "issue1019-sidecar",
            "running",
            &params,
            &paths,
            "request-sha",
            &authorization,
            "2026-06-15T00:00:00Z".to_owned(),
            Some(1234),
            None,
        );

        write_shell_remote_cleanup_invocation(&paths, &params)
            .unwrap_or_else(|error| panic!("cleanup sidecar should write: {error}"));
        let cleanup = read_shell_remote_cleanup_invocation(&paths, "issue1019-sidecar")
            .unwrap_or_else(|error| panic!("cleanup sidecar should read: {error}"))
            .unwrap_or_else(|| panic!("cleanup sidecar should exist"));
        let persisted_invocation = shell_job_cleanup_invocation(&status, None, Some(&cleanup))
            .unwrap_or_else(|| panic!("parse persisted cleanup sidecar invocation"));
        let persisted_parts = ssh_direct_command_parts(&persisted_invocation.args)
            .unwrap_or_else(|| panic!("parse persisted sidecar cleanup args"));

        println!(
            "readback=act_run_shell_remote_cleanup edge=shell_wrapped_redacted_status before=args_redacted:{} after=cleanup:{cleanup:?} invocation:{persisted_invocation:?}",
            status.args_redacted
        );
        assert_eq!(persisted_invocation.command, "ssh");
        assert!(status.args_redacted);
        assert!(
            persisted_parts
                .control_args
                .iter()
                .any(|arg| arg == "//wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519")
        );
        assert!(
            !persisted_parts
                .control_args
                .iter()
                .any(|arg| arg.contains("[redacted-arg:"))
        );
        assert!(
            !cleanup
                .control_args
                .iter()
                .any(|arg| arg.contains("exec -a issue1019"))
        );
        assert_eq!(cleanup.remote_identity, "aiwonder.mst.com");
        assert_eq!(cleanup.source_evidence, "shell_wrapped_ssh:powershell");
    }

    #[test]
    fn shell_cleanup_output_excerpt_is_bounded_and_one_line() {
        let input = format!("line1\r\n{}", "x".repeat(600));
        let excerpt = shell_cleanup_output_excerpt(&input);

        assert!(excerpt.contains("\\r\\n"));
        assert!(!excerpt.contains('\r'));
        assert!(!excerpt.contains('\n'));
        assert!(excerpt.ends_with("...[truncated]"));
        assert!(excerpt.len() <= 530);
    }

    #[test]
    fn shell_remote_process_marker_updates_cleanup_handle() {
        let mut scope = ssh_remote_process_scope(
            "ssh.exe",
            &["aiwonder".to_owned(), "sleep 60".to_owned()],
            "regression_marker",
        );
        let mut status = ActRunShellJobStatus {
            schema_version: 4,
            job_id: "issue940-marker".to_owned(),
            session_id: None,
            status: "running".to_owned(),
            pid: Some(4242),
            command: "ssh.exe".to_owned(),
            command_metadata_policy: SHELL_COMMAND_METADATA_POLICY.to_owned(),
            args: vec!["aiwonder".to_owned(), "sleep 60".to_owned()],
            command_line: "ssh.exe aiwonder \"sleep 60\"".to_owned(),
            args_redacted: false,
            command_line_redacted: false,
            args_original_count: 2,
            args_original_bytes: 17,
            args_sha256: "args-sha".to_owned(),
            command_line_original_bytes: 27,
            command_line_sha256: "command-sha".to_owned(),
            working_dir: None,
            session_dir: None,
            effective_working_dir: None,
            env_keys: Vec::new(),
            session_env_keys: Vec::new(),
            timeout_ms: None,
            started_at: "2026-06-13T00:00:00Z".to_owned(),
            completed_at: None,
            duration_ms: None,
            exit_code: None,
            timed_out: false,
            cancel_requested: false,
            error_code: None,
            error_message: None,
            stdout_path: "stdout.log".to_owned(),
            stderr_path: "stderr.log".to_owned(),
            status_path: "status.json".to_owned(),
            request_sha256: "request-sha".to_owned(),
            matched_pattern: "^ssh".to_owned(),
            remote_process_scope: scope.clone(),
            diagnostics: None,
        };
        let stderr = "noise\nSYNAPSE_REMOTE_PROCESS_V1 job_id=issue940-marker pid=12345 pgid=12345 sid=12345\n";
        let metadata = parse_remote_process_metadata(stderr, "issue940-marker")
            .expect("marker line should parse");

        apply_remote_process_metadata(&mut status, metadata);
        scope = status.remote_process_scope.clone();

        println!(
            "readback=act_run_shell_remote_tracking edge=marker_parse before={stderr:?} after={scope:?}"
        );
        assert_eq!(scope.remote_cleanup_status, SHELL_REMOTE_CLEANUP_TRACKED);
        assert_eq!(scope.remote_process_id.as_deref(), Some("12345"));
        assert_eq!(scope.remote_process_group_id.as_deref(), Some("12345"));
        assert!(!scope.remote_cleanup_verified);
        assert!(
            scope
                .detection_evidence
                .iter()
                .any(|evidence| evidence.contains("remote_session_id:12345"))
        );

        let concatenated_stderr = "user-stderr-without-newlineSYNAPSE_REMOTE_PROCESS_V1 job_id=issue940-marker pid=54321 pgid=54321 sid=54321\n";
        let concatenated_metadata =
            parse_remote_process_metadata(concatenated_stderr, "issue940-marker")
                .expect("marker concatenated after user stderr should parse");
        println!(
            "readback=act_run_shell_remote_tracking edge=marker_after_user_stderr before={concatenated_stderr:?} after={concatenated_metadata:?}"
        );
        assert_eq!(concatenated_metadata.pid, "54321");
        assert_eq!(concatenated_metadata.pgid, "54321");
    }

    #[test]
    fn shell_remote_cleanup_command_uses_dash_compatible_negative_pgid() {
        let command = ssh_remote_cleanup_command("12345", "12345");

        println!("readback=act_run_shell_remote_cleanup edge=dash_kill_syntax after={command:?}");
        assert!(command.contains("kill -TERM -\"$pgid\""));
        assert!(command.contains("kill -KILL -\"$pgid\""));
        assert!(!command.contains("kill -TERM --"));
        assert!(!command.contains("kill -KILL --"));
    }

    #[test]
    fn shell_status_diagnostics_classifies_scp_default_sftp_no_output() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "scp.exe".to_owned(),
            args: vec![
                "local.txt".to_owned(),
                "aiwonder:/tmp/synapse885-local.txt".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue885-scp-diagnostics".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "scp.exe local.txt aiwonder:/tmp/synapse885-local.txt".to_owned(),
            matched_pattern: "^scp".to_owned(),
        };
        let status = shell_job_status_record(
            "issue885-scp-diagnostics",
            "running",
            &params,
            &paths,
            "request-sha",
            &authorization,
            "2026-06-12T00:00:00Z".to_owned(),
            Some(1234),
            None,
        );

        let diagnostics = shell_job_status_diagnostics(&status, true, 0, 0);
        let transfer = diagnostics
            .transfer
            .as_ref()
            .expect("scp diagnostics should identify transfer family");

        println!(
            "readback=act_run_shell_status edge=scp_default_sftp_no_output before=stdout:0,stderr:0,args:{:?} after={diagnostics:?}",
            params.args
        );
        assert_eq!(diagnostics.output_state, "running_no_output");
        assert_eq!(transfer.client, "scp");
        assert_eq!(transfer.protocol_hint, "scp_default_sftp_protocol");
        assert_eq!(transfer.remote_identity.as_deref(), Some("aiwonder"));
        assert!(
            diagnostics
                .actionable_hints
                .iter()
                .any(|hint| hint.contains("retry_scp_with_-O"))
        );
        assert!(
            diagnostics
                .actionable_hints
                .iter()
                .any(|hint| hint.contains("rerun_with_-v"))
        );
    }

    #[test]
    fn shell_status_diagnostics_classifies_scp_legacy_o_flag() {
        let args = vec![
            "-O".to_owned(),
            "local.txt".to_owned(),
            "aiwonder:/tmp/synapse885-local.txt".to_owned(),
        ];

        let protocol_hint = shell_transfer_protocol_hint("scp", &args);
        let remote_identity = scp_remote_identity(&args);

        println!(
            "readback=act_run_shell_status edge=scp_legacy_flag before=args:{args:?} after=protocol:{protocol_hint} remote:{remote_identity:?}"
        );
        assert_eq!(protocol_hint, "scp_legacy_protocol_forced_by_-O");
        assert_eq!(remote_identity.as_deref(), Some("aiwonder"));
    }

    #[test]
    fn shell_remote_scope_normalizes_legacy_direct_ssh_status_file() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "ssh.exe".to_owned(),
            args: vec!["aiwonder".to_owned(), "sleep".to_owned(), "60".to_owned()],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue827-legacy".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "ssh.exe aiwonder sleep 60".to_owned(),
            matched_pattern: "^ssh".to_owned(),
        };
        let mut status = serde_json::to_value(shell_job_status_record(
            "issue827-legacy",
            "running",
            &params,
            &paths,
            "request-sha",
            &authorization,
            "2026-06-10T00:00:00Z".to_owned(),
            Some(1234),
            None,
        ))
        .unwrap_or_else(|error| panic!("status should encode to JSON: {error}"));
        status["schema_version"] = json!(2);
        status
            .as_object_mut()
            .expect("status is an object")
            .remove("remote_process_scope");
        std::fs::write(
            &paths.status_path,
            serde_json::to_vec_pretty(&status).unwrap(),
        )
        .unwrap_or_else(|error| panic!("write legacy status file: {error}"));

        let read = read_shell_job_status(&paths.status_path, "issue827-legacy")
            .unwrap_or_else(|error| panic!("legacy status should read: {error}"));

        println!(
            "readback=act_run_shell_remote_scope edge=legacy_status before={status} after={:?}",
            read.remote_process_scope
        );
        assert_eq!(
            read.remote_process_scope.transport,
            SHELL_REMOTE_TRANSPORT_SSH
        );
        assert_eq!(
            read.remote_process_scope.remote_identity.as_deref(),
            Some("aiwonder")
        );
    }

    #[test]
    fn shell_remote_scope_marks_cancelled_ssh_cleanup_unverified() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
            remote_cleanup_path: temp.path().join("remote-cleanup.json"),
        };
        let params = ActRunShellStartParams {
            command: "ssh.exe".to_owned(),
            args: vec!["aiwonder".to_owned(), "sleep".to_owned(), "60".to_owned()],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            job_id: Some("issue827-cancel".to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: "ssh.exe aiwonder sleep 60".to_owned(),
            matched_pattern: "^ssh".to_owned(),
        };
        let mut status = shell_job_status_record(
            "issue827-cancel",
            "cancel_requested",
            &params,
            &paths,
            "request-sha",
            &authorization,
            "2026-06-10T00:00:00Z".to_owned(),
            Some(1234),
            None,
        );

        mark_shell_job_remote_cleanup_unverified(&mut status, "act_run_shell_cancel", "terminated");
        let termination_status =
            remote_aware_termination_status("terminated", &status.remote_process_scope);

        println!(
            "readback=act_run_shell_remote_scope edge=cancel_unverified before=terminated after=status:{termination_status} scope:{:?}",
            status.remote_process_scope
        );
        assert_eq!(
            termination_status,
            "local_ssh_client_terminated_remote_cleanup_unverified"
        );
        assert_eq!(
            status.error_code.as_deref(),
            Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
        );
        assert_eq!(
            status
                .remote_process_scope
                .remote_cleanup_error_code
                .as_deref(),
            Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
        );
        assert!(!status.remote_process_scope.remote_cleanup_verified);
    }

    #[test]
    fn shell_rejects_direct_chromium_remote_debugging_without_popup_safe_flags() {
        let params = shell_params(
            "chrome.exe",
            vec!["--remote-debugging-port=9222", "about:blank"],
            30_000,
        );

        let error = validate_run_shell_params(&params)
            .expect_err("direct unsafe Chrome remote-debugging shell launch must fail closed");

        println!(
            "readback=act_run_shell_chromium_policy edge=direct_unsafe before=args:{:?} after={:?}",
            params.args, error.data
        );
        assert_eq!(
            extract_error_code(&error),
            error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("chromium_remote_debugging_not_popup_safe")
        );
        assert!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("required_invariant"))
                .and_then(|value| value.as_str())
                .is_some_and(|value| value.contains("--silent-debugger-extension-api"))
        );
    }

    #[test]
    fn shell_allows_direct_chromium_remote_debugging_with_popup_safe_flags() {
        let safe_profile = cdp_automation_profile_dir();
        let safe_profile_arg = format!("--user-data-dir={}", safe_profile.display());
        let params = shell_params(
            "chrome.exe",
            vec![
                "--remote-debugging-port=0",
                safe_profile_arg.as_str(),
                "--silent-debugger-extension-api",
                "--disable-extensions",
                "about:blank",
            ],
            30_000,
        );

        println!(
            "readback=act_run_shell_chromium_policy edge=direct_safe before=args:{:?}",
            params.args
        );
        validate_run_shell_params(&params).unwrap_or_else(|error| {
            panic!("popup-safe direct Chrome shell launch rejected: {error}")
        });
    }

    #[test]
    fn shell_rejects_direct_chromium_layout_infobar_flag_even_when_silent() {
        let safe_profile = cdp_automation_profile_dir();
        let safe_profile_arg = format!("--user-data-dir={}", safe_profile.display());
        let params = shell_params(
            "chrome.exe",
            vec![
                "--remote-debugging-pipe",
                safe_profile_arg.as_str(),
                "--silent-debugger-extension-api",
                "--disable-extensions",
                "--disable-blink-features=AutomationControlled",
                "about:blank",
            ],
            30_000,
        );

        let error = validate_run_shell_params(&params)
            .expect_err("layout-shifting Chrome flags must fail closed even with silent debugger");

        println!(
            "readback=act_run_shell_chromium_policy edge=direct_layout_flag before=args:{:?} after={:?}",
            params.args, error.data
        );
        assert_eq!(
            extract_error_code(&error),
            error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        );
        let error_text = format!("{error:?}");
        assert!(error_text.contains("has_layout_shifting_infobar_flags"));
        assert!(error_text.contains("AutomationControlled"));
    }

    #[test]
    fn shell_rejects_wrapped_chromium_layout_infobar_launch() {
        let snippet = r#"& "C:\Program Files\Google\Chrome\Application\chrome.exe" --remote-debugging-pipe --user-data-dir="$env:LOCALAPPDATA\ms-playwright-mcp\mcp-chrome-issue1260" --disable-blink-features=AutomationControlled about:blank"#;
        let params = shell_params(
            "powershell.exe",
            vec!["-NoProfile", "-Command", snippet],
            30_000,
        );

        let error = validate_run_shell_params(&params)
            .expect_err("shell-wrapped layout-shifting Chrome launch must fail closed");

        println!(
            "readback=act_run_shell_chromium_policy edge=wrapped_layout_flag before=args:{:?} after={:?}",
            params.args, error.data
        );
        assert_eq!(
            extract_error_code(&error),
            error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("shell_wrapped_chromium_remote_debugging_not_popup_safe")
        );
        assert!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("shell_markers"))
                .and_then(|markers| markers.as_array())
                .is_some_and(|markers| markers
                    .iter()
                    .any(|marker| marker == "layout_flag_automationcontrolled"))
        );
    }

    #[test]
    fn shell_rejects_known_playwright_mcp_launcher_but_allows_text_search() {
        let launcher = shell_params(
            "cmd.exe",
            vec!["/c", "npx @playwright/mcp --browser chrome"],
            30_000,
        );

        let error = validate_run_shell_params(&launcher)
            .expect_err("known Playwright MCP browser launcher must fail closed");
        println!(
            "readback=act_run_shell_chromium_policy edge=playwright_mcp before=args:{:?} after={:?}",
            launcher.args, error.data
        );
        assert_eq!(
            extract_error_code(&error),
            error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        );

        let search = shell_params("rg", vec!["@playwright/mcp"], 30_000);
        validate_run_shell_params(&search).unwrap_or_else(|error| {
            panic!("text search mentioning Playwright MCP rejected: {error}")
        });

        let search_remote_debug = shell_params(
            "rg",
            vec!["chrome.exe --remote-debugging-pipe @playwright/mcp"],
            30_000,
        );
        validate_run_shell_params(&search_remote_debug).unwrap_or_else(|error| {
            panic!("text search mentioning remote debugging rejected: {error}")
        });
    }

    #[test]
    fn shell_rejects_known_playwright_mcp_launcher_from_bare_shell_names() {
        for command in ["cmd", "powershell", "pwsh"] {
            let params = shell_params(
                command,
                vec!["/c", "npx @playwright/mcp --browser chrome"],
                30_000,
            );

            let error = validate_run_shell_params(&params)
                .expect_err("bare shell names must not bypass Playwright MCP launch guard");
            println!(
                "readback=act_run_shell_chromium_policy edge=bare_shell command={command} before=args:{:?} after={:?}",
                params.args, error.data
            );
            assert_eq!(
                extract_error_code(&error),
                error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
            );
            assert_eq!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("reason"))
                    .and_then(|reason| reason.as_str()),
                Some("known_playwright_mcp_browser_launcher_denied")
            );
        }
    }

    #[test]
    fn shell_allows_read_only_process_query_mentioning_playwright_mcp() {
        let snippet = "$rows = Get-CimInstance Win32_Process | Where-Object { $_.Name -match '^(chrome|node|cmd)\\.exe$' -and (($_.CommandLine -like '*ms-playwright-mcp*') -or ($_.CommandLine -like '*@playwright/mcp*')) }; $rows | ConvertTo-Json";
        let params = shell_params(
            "powershell.exe",
            vec!["-NoProfile", "-Command", snippet],
            30_000,
        );

        println!(
            "readback=act_run_shell_chromium_policy edge=readonly_process_query before=args:{:?}",
            params.args
        );
        validate_run_shell_params(&params).unwrap_or_else(|error| {
            panic!("read-only process query mentioning Playwright MCP rejected: {error}")
        });
    }

    #[test]
    fn shell_allows_read_only_process_query_mentioning_remote_debugging() {
        let snippet = "$rows = Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'chrome.exe' -and $_.CommandLine -like '*--remote-debugging-port=9222*' } | Select-Object ProcessId,CommandLine; $rows | ConvertTo-Json";
        let params = shell_params(
            "powershell.exe",
            vec!["-NoProfile", "-Command", snippet],
            30_000,
        );

        println!(
            "readback=act_run_shell_chromium_policy edge=readonly_remote_debugging_query before=args:{:?}",
            params.args
        );
        validate_run_shell_params(&params).unwrap_or_else(|error| {
            panic!("read-only process query mentioning remote debugging rejected: {error}")
        });
    }

    #[test]
    fn shell_params_reject_command_string_with_embedded_args() {
        let params = shell_params(
            "powershell -NoProfile -Command Write-Output SYN769",
            Vec::new(),
            30_000,
        );

        let error = match validate_run_shell_params(&params) {
            Ok(()) => panic!("command string with embedded args should be rejected"),
            Err(error) => error,
        };

        let reason = error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str());
        println!(
            "readback=act_run_shell_command_shape edge=embedded_args before={:?} after_reason={reason:?}",
            params.command
        );
        assert_eq!(reason, Some("command_contains_arguments"));
    }

    #[test]
    fn shell_params_reject_quoted_command_path() {
        let params = shell_params(
            r#""C:\Program Files\PowerShell\7\pwsh.exe""#,
            Vec::new(),
            30_000,
        );

        let error = match validate_run_shell_params(&params) {
            Ok(()) => panic!("quoted command path should be rejected"),
            Err(error) => error,
        };

        let reason = error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str());
        println!(
            "readback=act_run_shell_command_shape edge=quoted_path before={:?} after_reason={reason:?}",
            params.command
        );
        assert_eq!(reason, Some("command_must_not_be_quoted"));
    }

    #[test]
    fn shell_params_reject_unbalanced_command_quote() {
        let params = shell_params(
            r#""C:\Program Files\PowerShell\7\pwsh.exe"#,
            Vec::new(),
            30_000,
        );

        let error = match validate_run_shell_params(&params) {
            Ok(()) => panic!("unbalanced command quote should be rejected"),
            Err(error) => error,
        };

        let reason = error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str());
        println!(
            "readback=act_run_shell_command_shape edge=unbalanced_quote before={:?} after_reason={reason:?}",
            params.command
        );
        assert_eq!(reason, Some("command_has_unbalanced_quote"));
    }

    #[test]
    fn shell_params_allow_existing_command_path_with_spaces() {
        let dir = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell path dir: {error}"));
        let nested = dir.path().join("dir with spaces");
        std::fs::create_dir_all(&nested)
            .unwrap_or_else(|error| panic!("create nested temp path: {error}"));
        let command_path = nested.join("tool with spaces.exe");
        std::fs::write(&command_path, b"synthetic executable path marker")
            .unwrap_or_else(|error| panic!("write temp command path: {error}"));
        let params = shell_params(
            &command_path.display().to_string(),
            vec!["--version"],
            30_000,
        );

        println!(
            "readback=act_run_shell_command_shape edge=existing_path_with_spaces before={:?}",
            params.command
        );
        validate_run_shell_params(&params).unwrap_or_else(|error| {
            panic!("existing executable path with spaces rejected: {error}")
        });
    }

    #[test]
    fn shell_params_allow_working_dir_relative_command_path_with_spaces() {
        let dir = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell working dir: {error}"));
        let command_name = "tool with spaces.exe";
        let command_path = dir.path().join(command_name);
        std::fs::write(&command_path, b"synthetic executable path marker")
            .unwrap_or_else(|error| panic!("write temp command path: {error}"));
        let mut params = shell_params(command_name, vec!["--version"], 30_000);
        params.working_dir = Some(dir.path().display().to_string());

        println!(
            "readback=act_run_shell_command_shape edge=working_dir_relative_path before={:?} working_dir={:?}",
            params.command, params.working_dir
        );
        validate_run_shell_params(&params).unwrap_or_else(|error| {
            panic!("working_dir-relative executable path with spaces rejected: {error}")
        });
    }

    #[test]
    fn launch_command_line_quotes_empty_and_space_args() {
        let params = launch_params("notepad.exe", vec!["C:\\tmp\\hello world.txt", ""], 10_000);

        assert_eq!(
            launch_command_line(&params).unwrap_or_else(|error| {
                panic!("synthetic launch command line should build: {error}")
            }),
            "notepad.exe \"C:\\tmp\\hello world.txt\" \"\""
        );
    }

    #[cfg(windows)]
    #[test]
    fn launch_command_line_uses_win32_long_path_for_existing_path_target() {
        let dir = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp launch path dir: {error}"));
        let target_path = dir.path().join("synapse launch target.exe");
        std::fs::write(&target_path, b"synthetic")
            .unwrap_or_else(|error| panic!("write temp target: {error}"));
        let params = launch_params(&target_path.display().to_string(), vec!["--flag"], 10_000);

        let command_line = launch_command_line(&params).unwrap_or_else(|error| {
            panic!("existing path-like launch target should resolve: {error}")
        });

        assert!(
            command_line.contains("synapse launch target.exe\" --flag"),
            "{command_line}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn launch_command_line_rejects_unresolvable_path_target() {
        let dir = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp launch path dir: {error}"));
        let target_path = dir.path().join("missing-launch-target.exe");
        let params = launch_params(&target_path.display().to_string(), Vec::new(), 10_000);

        let error = match launch_command_line(&params) {
            Ok(command_line) => panic!("missing path should not resolve, got {command_line}"),
            Err(error) => error,
        };

        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("launch_target_path_resolution_failed")
        );
    }

    #[cfg(windows)]
    #[test]
    fn launch_desktop_shared_tabbed_targets_require_absolute_path() {
        for target in ["notepad", "notepad.exe"] {
            let mut params = launch_params(target, vec![r"C:\tmp\issue1319.txt"], 10_000);
            params.desktop = Some("agent:session".to_owned());
            params.wait_for_window_title_regex = Some("issue1319".to_owned());

            let error = validate_launch_params(&params)
                .expect_err("pathless shared-tabbed desktop targets must fail closed");

            println!(
                "readback=act_launch_shared_tabbed_desktop_target edge=pathless before=target:{target:?} after={:?}",
                error.data
            );
            assert_eq!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("reason"))
                    .and_then(|reason| reason.as_str()),
                Some("shared_tabbed_app_desktop_requires_explicit_path")
            );
            assert_eq!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("launch_target_name"))
                    .and_then(|name| name.as_str()),
                Some("notepad.exe")
            );
        }

        let mut explicit = launch_params(
            r"C:\Windows\notepad.exe",
            vec![r"C:\tmp\issue1319.txt"],
            10_000,
        );
        explicit.desktop = Some("agent:session".to_owned());
        explicit.wait_for_window_title_regex = Some("issue1319".to_owned());

        println!(
            "readback=act_launch_shared_tabbed_desktop_target happy=explicit_path before=target:{:?}",
            explicit.target
        );
        validate_launch_params(&explicit).expect(
            "absolute shared-tabbed desktop target remains eligible for hidden-desktop wait",
        );
    }

    #[cfg(windows)]
    #[test]
    fn launch_target_effective_file_name_matches_windows_createprocess_rules() {
        let cases = [
            ("notepad", "notepad.exe"),
            ("notepad.exe", "notepad.exe"),
            ("notepad.", "notepad."),
            (r"C:\Windows\notepad", "notepad"),
            (r"C:\Windows\notepad.exe", "notepad.exe"),
        ];

        for (target, expected) in cases {
            let actual = launch_target_effective_file_name(target);
            println!(
                "readback=act_launch_effective_target_name before=target:{target:?} after={actual:?}"
            );
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn launch_window_selection_prefers_new_matching_window() {
        let contexts = vec![
            foreground_for_launch_selection(10, 100, "chrome.exe", "Google Chrome"),
            foreground_for_launch_selection(11, 999, "chrome.exe", "Google Chrome"),
        ];
        let excluded = HashSet::from([10]);
        let title_regex = regex::Regex::new("Chrome|Chromium").expect("synthetic regex compiles");

        let selected =
            select_launch_window(&contexts, 999, &title_regex, &excluded, "chrome.exe", &[])
                .expect("new matching window should be selected");

        assert_eq!(selected.hwnd, 11);
    }

    #[test]
    fn launch_window_selection_rejects_unowned_new_matching_window() {
        let contexts = vec![foreground_for_launch_selection(
            11,
            200,
            "chrome.exe",
            "Google Chrome",
        )];
        let excluded = HashSet::new();
        let title_regex = regex::Regex::new("Chrome|Chromium").expect("synthetic regex compiles");

        let selected =
            select_launch_window(&contexts, 999, &title_regex, &excluded, "chrome.exe", &[]);

        assert!(
            selected.is_none(),
            "a matching title from an unrelated PID must not satisfy launch wait"
        );
    }

    #[test]
    fn launch_desktop_window_selection_accepts_new_tabbed_notepad_with_broker_pid() {
        let contexts = vec![foreground_for_launch_selection(
            11,
            39016,
            "Notepad.exe",
            "Untitled - Notepad",
        )];
        let excluded = HashSet::new();
        let title_regex =
            regex::Regex::new("^Untitled - Notepad$").expect("synthetic regex compiles");

        let selected = select_launch_desktop_window(
            &contexts,
            51028,
            &title_regex,
            &excluded,
            "notepad.exe",
            &[],
        )
        .expect("new hidden-desktop Notepad window should satisfy launch wait despite broker PID");

        assert_eq!(selected.hwnd, 11);
    }

    #[cfg(windows)]
    #[test]
    fn launch_desktop_window_selection_accepts_extensionless_notepad_effective_name() {
        let contexts = vec![foreground_for_launch_selection(
            11,
            39016,
            "Notepad.exe",
            "issue1319.txt - Notepad",
        )];
        let excluded = HashSet::new();
        let title_regex =
            regex::Regex::new("^issue1319\\.txt - Notepad$").expect("synthetic regex compiles");
        let launch_target_name = launch_target_effective_file_name("notepad");

        let selected = select_launch_desktop_window(
            &contexts,
            51028,
            &title_regex,
            &excluded,
            &launch_target_name,
            &[r"C:\tmp\issue1319.txt".to_owned()],
        )
        .expect("effective notepad.exe name should match brokered hidden-desktop Notepad window");

        assert_eq!(launch_target_name, "notepad.exe");
        assert_eq!(selected.hwnd, 11);
    }

    #[test]
    fn launch_desktop_window_selection_rejects_excluded_tabbed_notepad() {
        let contexts = vec![foreground_for_launch_selection(
            10,
            39016,
            "Notepad.exe",
            "Untitled - Notepad",
        )];
        let excluded = HashSet::from([10]);
        let title_regex =
            regex::Regex::new("^Untitled - Notepad$").expect("synthetic regex compiles");

        let selected = select_launch_desktop_window(
            &contexts,
            51028,
            &title_regex,
            &excluded,
            "notepad.exe",
            &[],
        );

        assert!(
            selected.is_none(),
            "pre-existing hidden-desktop Notepad windows must not become owned launch targets"
        );
    }

    #[test]
    fn launch_window_selection_accepts_new_brokered_windows_terminal_window() {
        let contexts = vec![foreground_for_launch_selection(
            11,
            200,
            "WindowsTerminal.exe",
            "Calyx #559 regression",
        )];
        let excluded = HashSet::new();
        let title_regex =
            regex::Regex::new("^Calyx #559 regression$").expect("synthetic regex compiles");

        let selected = select_launch_window(&contexts, 999, &title_regex, &excluded, "wt.exe", &[])
            .expect("new brokered Windows Terminal window should satisfy launch wait");

        assert_eq!(selected.hwnd, 11);
    }

    #[test]
    fn launch_window_selection_accepts_existing_single_instance_window() {
        let contexts = vec![foreground_for_launch_selection(
            10,
            100,
            "chrome.exe",
            "Google Chrome",
        )];
        let excluded = HashSet::from([10]);
        let title_regex = regex::Regex::new("Chrome|Chromium").expect("synthetic regex compiles");

        let selected =
            select_launch_window(&contexts, 999, &title_regex, &excluded, "chrome.exe", &[])
                .expect("existing single-instance matching window should be selected");

        assert_eq!(selected.hwnd, 10);
    }

    #[test]
    fn launch_window_selection_rejects_existing_tabbed_notepad_window() {
        let contexts = vec![foreground_for_launch_selection(
            10,
            100,
            "Notepad.exe",
            "issue1034-qwen8v2-notepad.txt - Notepad",
        )];
        let excluded = HashSet::from([10]);
        let title_regex = regex::Regex::new("Notepad").expect("synthetic regex compiles");

        let selected = select_launch_window(
            &contexts,
            999,
            &title_regex,
            &excluded,
            "notepad.exe",
            &["C:\\tmp\\issue1271.txt".to_owned()],
        );

        assert!(
            selected.is_none(),
            "existing Notepad tab hosts must not become agent-owned launch targets"
        );
    }

    #[test]
    fn launch_window_selection_accepts_new_tabbed_notepad_window_from_spawn_pid() {
        let contexts = vec![
            foreground_for_launch_selection(10, 100, "Notepad.exe", "User Notes - Notepad"),
            foreground_for_launch_selection(11, 999, "Notepad.exe", "issue1271.txt - Notepad"),
        ];
        let excluded = HashSet::from([10]);
        let title_regex =
            regex::Regex::new("issue1271\\.txt - Notepad").expect("synthetic regex compiles");

        let selected = select_launch_window(
            &contexts,
            999,
            &title_regex,
            &excluded,
            "notepad.exe",
            &["C:\\tmp\\issue1271.txt".to_owned()],
        )
        .expect("new Notepad window owned by the spawned PID should be selected");

        assert_eq!(selected.hwnd, 11);
    }

    #[test]
    fn launch_window_selection_rejects_unrelated_existing_window() {
        let contexts = vec![foreground_for_launch_selection(
            10,
            100,
            "WindowsTerminal.exe",
            "Synapse - Windows Terminal",
        )];
        let excluded = HashSet::from([10]);
        let title_regex = regex::Regex::new("Synapse|Explorer").expect("synthetic regex compiles");

        let selected =
            select_launch_window(&contexts, 999, &title_regex, &excluded, "explorer.exe", &[]);

        assert!(
            selected.is_none(),
            "unrelated existing windows must not satisfy broad launch title regexes"
        );
    }

    #[test]
    fn launch_window_selection_accepts_known_shell_activation_window() {
        let contexts = vec![foreground_for_launch_selection(
            10,
            100,
            "ApplicationFrameHost.exe",
            "Settings",
        )];
        let excluded = HashSet::from([10]);
        let title_regex =
            regex::Regex::new("^(Settings|Control Panel)$").expect("synthetic regex compiles");
        let launch_args = vec!["ms-settings:".to_owned()];

        let selected = select_launch_window(
            &contexts,
            999,
            &title_regex,
            &excluded,
            "explorer.exe",
            &launch_args,
        )
        .expect("known shell-activated app window should be accepted");

        assert_eq!(selected.hwnd, 10);
    }

    #[test]
    fn shell_allowlist_accepts_narrow_startup_patterns() {
        let config = M4ServiceConfig::from_cli_parts(
            vec![
                r"^git \w+$".to_owned(),
                r"^echo .{0,100}$".to_owned(),
                r"^cargo (build|test)( --[\w-]+)*$".to_owned(),
            ],
            Vec::new(),
            DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
        );

        assert!(
            config.is_ok(),
            "narrow allow-shell examples should compile: {config:?}"
        );
    }

    #[test]
    fn shell_allowlist_rejects_broad_startup_patterns() {
        let cases = [
            ("", "empty_pattern"),
            (".*", "unbounded_any_character_repetition"),
            ("^.+$", "unbounded_any_character_repetition"),
            ("^$", "matches_empty"),
            ("git status", "shell_pattern_must_match_full_command_line"),
            (r"^[\s\S]*$", "unbounded_any_character_repetition"),
        ];

        for (pattern, reason) in cases {
            let error = match M4ServiceConfig::from_cli_parts(
                vec![pattern.to_owned()],
                Vec::new(),
                DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
            ) {
                Ok(config) => panic!("pattern {pattern:?} should reject, got {config:?}"),
                Err(error) => error,
            };
            let Some(broad) = error.downcast_ref::<BroadAllowPatternError>() else {
                panic!("pattern {pattern:?} returned unexpected error: {error:#}");
            };
            assert_eq!(broad.reason(), reason);
        }
    }

    #[tokio::test]
    async fn shell_denies_without_allowlist() {
        let params = shell_params("synthetic-shell-denied", Vec::new(), 30_000);

        let error = match run_shell(&M4ServiceConfig::default(), params).await {
            Ok(response) => panic!("unallowlisted shell should deny, got {response:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some(error_codes::SAFETY_SHELL_DENIED_BY_POLICY)
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("no_allow_shell_policy")
        );
    }

    #[tokio::test]
    async fn launch_denies_without_allowlist() {
        let params = launch_params("synthetic-launch-denied", Vec::new(), 10_000);

        let error = match launch(&M4ServiceConfig::default(), params).await {
            Ok(response) => panic!("unallowlisted launch should deny, got {response:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some(error_codes::SAFETY_LAUNCH_DENIED_BY_POLICY)
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("no_allow_launch_policy")
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn launch_applies_working_dir_and_env() {
        let dir = match tempfile::TempDir::new() {
            Ok(dir) => dir,
            Err(error) => panic!("create temp launch dir: {error}"),
        };
        let output_path = dir.path().join("launch-env.txt");
        let mut params = launch_params(
            "cmd.exe",
            vec!["/c", "echo %SYNAPSE_LAUNCH_ENV%>launch-env.txt"],
            10_000,
        );
        params.working_dir = Some(dir.path().display().to_string());
        params.env.insert(
            "SYNAPSE_LAUNCH_ENV".to_owned(),
            "synapse-launch-ok".to_owned(),
        );
        let config = launch_config_for(&params);

        let response = match launch(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("allowlisted cmd launch should spawn: {error}"),
        };

        assert!(response.pid > 0);
        assert_eq!(response.hwnd, None);
        assert_eq!(response.matched_title, None);
        assert_eq!(response.reason, None);
        let text = read_text_file_with_retry(&output_path).await;
        assert_eq!(text.trim(), "synapse-launch-ok");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn launch_wait_refuses_console_window_title_wait() {
        // Console launches are hidden/no-window by policy, so a window-title
        // wait on a console target is unsatisfiable and must fail closed
        // before spawning anything (validate_console_launch_visibility).
        let mut params = launch_params("cmd.exe", vec!["/c", "exit 0"], 50);
        params.wait_for_window_title_regex = Some("^SynapseLaunchNoSuchWindow$".to_owned());
        let config = launch_config_for(&params);

        let error = match launch(&config, params).await {
            Ok(response) => panic!("console window wait should fail closed: {response:?}"),
            Err(error) => error,
        };

        println!(
            "readback=act_launch_window_wait edge=console_no_window before=regex:^SynapseLaunchNoSuchWindow$ after=error:{error}"
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("hidden_console_has_no_window_to_wait_for")
        );
    }

    #[cfg(windows)]
    async fn read_text_file_with_retry(path: &std::path::Path) -> String {
        for _ in 0..100 {
            match std::fs::read_to_string(path) {
                Ok(text) => return text,
                Err(_error) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        panic!(
            "file {} was not created by launched process",
            path.display()
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_allows_cmd_echo_and_captures_stdout() {
        let params = shell_params("cmd.exe", vec!["/c", "echo synapse-m4-shell-ok"], 30_000);
        let config = shell_config_for(&params);

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("allowlisted cmd echo should run: {error}"),
        };

        assert_eq!(response.exit_code, Some(0));
        assert_eq!(response.stdout, "synapse-m4-shell-ok\r\n");
        assert_eq!(response.stderr, "");
        assert!(!response.timed_out);
        assert!(!response.stdout_truncated);
        assert!(!response.stderr_truncated);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_caps_stdout_and_marks_truncated() {
        let params = shell_params(
            "powershell.exe",
            vec![
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write(('x'*1048580))",
            ],
            30_000,
        );
        let config = shell_config_for(&params);

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("allowlisted large stdout command should run: {error}"),
        };

        assert_eq!(response.exit_code, Some(0));
        assert_eq!(response.stdout.len(), SHELL_OUTPUT_CAP_BYTES);
        assert!(response.stdout.chars().all(|ch| ch == 'x'));
        assert!(response.stdout_truncated);
        assert_eq!(response.stderr, "");
        assert!(!response.stderr_truncated);
        assert!(!response.timed_out);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "real-process FSV: spawns + tree-kills a real powershell.exe; the spawn/kill wall-clock balloons on a saturated host (run explicitly with `cargo test -p synapse-mcp -- --ignored`). See M4_ACT_RUN_SHELL timeout-path perf follow-up."]
    async fn shell_timeout_kills_process_and_marks_timed_out() {
        let params = shell_params(
            "powershell.exe",
            vec!["-NoProfile", "-Command", "Start-Sleep -Milliseconds 5000"],
            500,
        );
        let config = shell_config_for(&params);

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => {
                panic!("allowlisted sleeping command should return timeout response: {error}")
            }
        };

        assert_eq!(response.exit_code, None);
        assert!(response.timed_out);
        assert_eq!(
            response.error_code.as_deref(),
            Some(error_codes::ACTION_BUDGET_EXPIRED)
        );
        assert!(
            response
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("500 ms")),
            "{response:?}"
        );
        // Correctness here is the timeout *firing* and the call returning at all —
        // proven by `timed_out`, the ACTION_BUDGET_EXPIRED code, and the "500 ms"
        // message above, all of which are load-independent. We deliberately do NOT
        // assert a wall-clock bound on `duration_ms`: spawning and tree-killing a
        // real powershell.exe is an OS-scheduling cost that balloons on a saturated
        // host, so any fixed bound is flaky as a gate. Protection against the call
        // *hanging indefinitely* on a wedged inherited pipe lives in the production
        // path (SHELL_READER_DRAIN_CAP), not in a timing assertion here.
    }

    #[test]
    fn shell_accepts_timeout_above_legacy_cap() {
        let params = shell_params("cmd.exe", vec!["/c", "echo long-timeout-ok"], 1_200_000);

        let authorization = authorize_run_shell(&shell_config_for(&params), &params)
            .unwrap_or_else(|error| panic!("legacy-cap timeout should authorize: {error}"));

        assert_eq!(
            authorization.command_line,
            "cmd.exe /c \"echo long-timeout-ok\""
        );
    }

    #[test]
    fn act_run_shell_deserializes_null_timeout_fields_as_omitted() {
        let omitted: ActRunShellParams = serde_json::from_value(json!({
            "command": "powershell.exe",
            "args": ["-NoProfile", "-Command", "Write-Output omitted"],
            "execution_mode": "durable"
        }))
        .unwrap_or_else(|error| panic!("omitted timeout fields should deserialize: {error}"));
        let explicit_null: ActRunShellParams = serde_json::from_value(json!({
            "command": "powershell.exe",
            "args": ["-NoProfile", "-Command", "Write-Output null"],
            "execution_mode": "durable",
            "timeout_ms": null,
            "durable_timeout_ms": null
        }))
        .unwrap_or_else(|error| panic!("null timeout fields should deserialize: {error}"));

        println!(
            "readback=act_run_shell_params edge=null_timeouts before=omitted after=timeout_ms:{} durable_timeout_ms:{:?}",
            explicit_null.timeout_ms, explicit_null.durable_timeout_ms
        );
        assert_eq!(omitted.timeout_ms, default_shell_timeout_ms());
        assert_eq!(explicit_null.timeout_ms, default_shell_timeout_ms());
        assert_eq!(explicit_null.durable_timeout_ms, None);
    }

    #[test]
    fn act_run_shell_invalid_timeout_type_still_fails_deserialization() {
        let error = serde_json::from_value::<ActRunShellParams>(json!({
            "command": "powershell.exe",
            "args": ["-NoProfile", "-Command", "Write-Output invalid"],
            "execution_mode": "durable",
            "timeout_ms": "not-a-number"
        }))
        .expect_err("invalid concrete timeout type must fail");

        println!("readback=act_run_shell_params edge=invalid_timeout_type after=error:{error}");
        assert!(error.to_string().contains("invalid type"));
    }

    #[test]
    fn act_run_shell_zero_timeout_still_fails_validation() {
        let params: ActRunShellParams = serde_json::from_value(json!({
            "command": "powershell.exe",
            "args": ["-NoProfile", "-Command", "Write-Output zero"],
            "execution_mode": "durable",
            "timeout_ms": 0
        }))
        .unwrap_or_else(|error| panic!("zero timeout should deserialize for validation: {error}"));
        let error = validate_run_shell_params(&params)
            .expect_err("zero timeout must fail closed in validation");

        println!(
            "readback=act_run_shell_params edge=zero_timeout after=error:{}",
            error.message
        );
        assert_eq!(extract_error_code(&error), error_codes::TOOL_PARAMS_INVALID);
        assert!(error.message.contains("timeout_ms must be >= 1"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_long_timeout_returns_durable_job_handle() {
        let inline_await_limit_ms = 1;
        let timeout_ms = DEFAULT_SHELL_TIMEOUT_MS;
        let params = shell_params(
            "cmd.exe",
            vec!["/c", "echo background-handoff-ok"],
            timeout_ms,
        );
        let mut config = shell_config_for(&params);
        config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("long direct shell call should return job handle: {error}"),
        };

        println!("readback=act_run_shell edge=long_timeout_handoff after=response:{response:?}");
        assert!(response.backgrounded);
        assert_eq!(
            response.background_reason.as_deref(),
            Some("timeout_exceeds_inline_await_budget")
        );
        assert_eq!(response.inline_await_limit_ms, Some(inline_await_limit_ms));
        assert_eq!(response.exit_code, None);
        assert_eq!(response.stdout, "");
        assert_eq!(response.stderr, "");
        let job_id = response
            .job_id
            .clone()
            .expect("background response should include job id");
        let job = response
            .job
            .expect("background response should include job");
        assert_eq!(job.job_id, job_id);
        assert_eq!(job.status, "running");
        assert_eq!(job.timeout_ms, None);

        for _ in 0..100 {
            let status = shell_job_status(
                &ActRunShellStatusParams {
                    job_id: job_id.clone(),
                    tail_bytes: 4096,
                },
                None,
            )
            .unwrap_or_else(|error| panic!("status should read durable job state: {error}"));
            println!("readback=act_run_shell edge=long_timeout_handoff after=status:{status:?}");
            if status.job.status == "finalizing" {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            if !status.running {
                assert_eq!(status.job.status, "ok");
                assert_eq!(status.job.exit_code, Some(0));
                assert!(status.stdout_tail.contains("background-handoff-ok"));
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        panic!("background job {job_id} did not complete within the regression readback window");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_inline_mode_honors_timeout_above_auto_background_limit() {
        let inline_await_limit_ms = 1;
        let mut params = shell_params(
            "cmd.exe",
            vec!["/c", "echo inline-override-ok"],
            DEFAULT_SHELL_TIMEOUT_MS,
        );
        params.execution_mode = ActRunShellExecutionMode::Inline;
        let mut config = shell_config_for(&params);
        config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("inline execution mode should not auto-background: {error}"),
        };

        println!("readback=act_run_shell edge=inline_mode_above_limit after=response:{response:?}");
        assert_eq!(response.exit_code, Some(0));
        assert_eq!(response.stdout, "inline-override-ok\r\n");
        assert!(!response.backgrounded);
        assert_eq!(
            response.requested_execution_mode,
            Some(ActRunShellExecutionMode::Inline)
        );
        assert_eq!(
            response.effective_execution_mode,
            Some(ActRunShellExecutionMode::Inline)
        );
        assert_eq!(response.job_id, None);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_inline_mode_waits_past_inline_await_limit() {
        // Regression for #954: a command that runs LONGER than the daemon inline await limit must
        // still complete inline when execution_mode="inline" and the requested wait fits inside
        // the MCP client-call budget. The inline await limit only governs the auto→durable
        // background decision.
        let inline_await_limit_ms = 200;
        let mut params = shell_params(
            "powershell.exe",
            vec![
                "-NoProfile",
                "-Command",
                "Start-Sleep -Milliseconds 600; Write-Output inline-slow-ok",
            ],
            10_000,
        );
        params.execution_mode = ActRunShellExecutionMode::Inline;
        let mut config = shell_config_for(&params);
        config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

        println!(
            "readback=act_run_shell edge=inline_waits_past_limit before=inline_await_limit_ms:{inline_await_limit_ms} command_runtime_ms:~600"
        );
        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("inline execution past the await limit should complete: {error}"),
        };

        println!("readback=act_run_shell edge=inline_waits_past_limit after=response:{response:?}");
        assert!(
            !response.timed_out,
            "command shorter than timeout_ms must not be killed by the inline await limit: {response:?}"
        );
        assert_eq!(response.exit_code, Some(0), "{response:?}");
        assert!(!response.backgrounded, "{response:?}");
        assert!(
            response.stdout.contains("inline-slow-ok"),
            "command must run to completion inline: {response:?}"
        );
        assert!(
            response.duration_ms >= inline_await_limit_ms as u32,
            "execution must have outlasted the {inline_await_limit_ms} ms inline await limit: {response:?}"
        );
        assert!(response.error_code.is_none(), "{response:?}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_inline_timeout_above_client_budget_returns_durable_job_handle() {
        let mut params = shell_params(
            "cmd.exe",
            vec!["/c", "echo inline-client-budget-handoff-ok"],
            DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS + 1,
        );
        params.execution_mode = ActRunShellExecutionMode::Inline;
        let config = shell_config_for(&params);

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("oversized inline request should return durable handle: {error}"),
        };

        println!(
            "readback=act_run_shell edge=inline_client_budget_handoff after=response:{response:?}"
        );
        assert!(response.backgrounded, "{response:?}");
        assert_eq!(
            response.background_reason.as_deref(),
            Some("inline_timeout_exceeds_mcp_client_call_budget")
        );
        assert_eq!(
            response.inline_client_call_budget_ms,
            Some(DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS)
        );
        assert_eq!(
            response.requested_execution_mode,
            Some(ActRunShellExecutionMode::Inline)
        );
        assert_eq!(
            response.effective_execution_mode,
            Some(ActRunShellExecutionMode::Durable)
        );
        let job_id = response.job_id.clone().expect("job id should be returned");
        assert_durable_job_finishes_ok(&job_id, "inline-client-budget-handoff-ok").await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_budget_expiry_message_is_actionable() {
        // When the caller's own timeout_ms budget expires, the error must point at the concrete
        // escape hatch (durable execution / the inline await limit) instead of dead-ending.
        let mut params = shell_params(
            "powershell.exe",
            vec!["-NoProfile", "-Command", "Start-Sleep -Milliseconds 5000"],
            400,
        );
        params.execution_mode = ActRunShellExecutionMode::Auto;
        let config = shell_config_for(&params);

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => {
                panic!("expired-budget command should return a timeout response: {error}")
            }
        };

        println!("readback=act_run_shell edge=actionable_budget_error after=response:{response:?}");
        assert!(response.timed_out, "{response:?}");
        assert_eq!(
            response.error_code.as_deref(),
            Some(error_codes::ACTION_BUDGET_EXPIRED),
            "{response:?}"
        );
        let message = response
            .error_message
            .as_deref()
            .expect("expired budget must carry an error message");
        assert!(
            message.contains("400 ms"),
            "names the expired budget: {message}"
        );
        assert!(
            message.contains("execution_mode=\"durable\""),
            "names the durable escape hatch: {message}"
        );
        assert!(
            message.contains("inline await limit"),
            "names the configurable inline await limit: {message}"
        );
        assert!(
            message.contains("MCP client-call budget"),
            "names the client-call guard: {message}"
        );
    }

    /// Seed a synthetic durable shell job on disk with a fully-formed status
    /// record so the reaper's real `read_shell_job_status` path exercises it.
    /// `completed_at` is the retention clock; `None` models a still-live job.
    #[cfg(test)]
    fn seed_synthetic_shell_job(
        root: &Path,
        job_id: &str,
        status: &str,
        started_at: &str,
        completed_at: Option<&str>,
    ) -> ShellJobPaths {
        let paths = shell_job_paths_from_root(root, job_id);
        fs::create_dir_all(&paths.job_dir)
            .unwrap_or_else(|error| panic!("seed job dir {job_id} should create: {error}"));
        let params = ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Write-Output reap-seed".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: Some(30_000),
            job_id: Some(job_id.to_owned()),
        };
        let authorization = RunShellAuthorization {
            command_line: shell_command_line_from_parts(&params.command, &params.args),
            matched_pattern: "__any_permitted__".to_owned(),
        };
        let request_sha = run_shell_start_request_sha256(&params)
            .unwrap_or_else(|error| panic!("seed request should hash: {error}"));
        let mut record = shell_job_status_record(
            job_id,
            status,
            &params,
            &paths,
            &request_sha,
            &authorization,
            started_at.to_owned(),
            Some(4242),
            None,
        );
        record.completed_at = completed_at.map(ToOwned::to_owned);
        if completed_at.is_some() {
            record.exit_code = Some(0);
            record.duration_ms = Some(10);
        }
        // Give each job a little stdout so bytes_reclaimed is provably non-zero.
        fs::write(&paths.stdout_path, b"reap-seed-stdout\n")
            .unwrap_or_else(|error| panic!("seed stdout {job_id} should write: {error}"));
        write_shell_job_status(&paths.status_path, &record)
            .unwrap_or_else(|error| panic!("seed status {job_id} should write: {error}"));
        paths
    }

    // #1510 full-state verification: seed synthetic jobs with backdated
    // completion timestamps and prove — by reading the filesystem, the source of
    // truth — that only aged *terminal* jobs are removed while live, finalizing,
    // recently-terminal, and unreadable jobs survive.
    #[test]
    fn reap_stale_shell_jobs_removes_only_aged_terminal_jobs() {
        let _root_guard = ShellJobRootGuard::new();
        let root = shell_durable_job_root_dir()
            .unwrap_or_else(|error| panic!("durable root should resolve: {error}"));
        fs::create_dir_all(&root)
            .unwrap_or_else(|error| panic!("durable root should create: {error}"));

        let old = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        let recent = chrono::Utc::now().to_rfc3339();

        // Reap candidates: two distinct terminal statuses, both aged out.
        let old_ok = seed_synthetic_shell_job(&root, "reap-old-ok", "ok", &old, Some(&old));
        let old_exited = seed_synthetic_shell_job(
            &root,
            "reap-old-exited",
            "exited_unobserved",
            &old,
            Some(&old),
        );
        // Reap candidate: a `finalizing` job stuck far past the millisecond-scale
        // finalize window is abandoned and must be reaped like any settled job
        // (the exact leak observed in the real store, #1510).
        let old_finalizing =
            seed_synthetic_shell_job(&root, "reap-old-finalizing", "finalizing", &old, Some(&old));
        // Retain: recently completed terminal job (age below TTL).
        let recent_ok =
            seed_synthetic_shell_job(&root, "reap-recent-ok", "ok", &recent, Some(&recent));
        // Retain: a genuinely in-flight `finalizing` job (just now) — the age
        // guard protects the millisecond window while the monitor writes final
        // output.
        let recent_finalizing = seed_synthetic_shell_job(
            &root,
            "reap-recent-finalizing",
            "finalizing",
            &recent,
            Some(&recent),
        );
        // Retain: live job (running) — never reaped regardless of age.
        let running = seed_synthetic_shell_job(&root, "reap-running", "running", &old, None);
        // Retain: unreadable status file — cannot prove it is settled, never reaped.
        let corrupt_paths = shell_job_paths_from_root(&root, "reap-corrupt");
        fs::create_dir_all(&corrupt_paths.job_dir)
            .unwrap_or_else(|error| panic!("corrupt job dir should create: {error}"));
        fs::write(&corrupt_paths.status_path, b"{ this is not valid json")
            .unwrap_or_else(|error| panic!("corrupt status should write: {error}"));

        // Precondition (source of truth = filesystem): all seven dirs exist.
        for dir in [
            &old_ok.job_dir,
            &old_exited.job_dir,
            &old_finalizing.job_dir,
            &recent_ok.job_dir,
            &recent_finalizing.job_dir,
            &running.job_dir,
            &corrupt_paths.job_dir,
        ] {
            assert!(dir.exists(), "precondition: {} should exist", dir.display());
        }

        // Exercise the real public entry point (default 7-day TTL); the aged
        // jobs are 30 days old so they exceed it, the recent one does not.
        let readback =
            reap_stale_shell_jobs().unwrap_or_else(|error| panic!("reap should succeed: {error}"));
        println!("readback=shell_job_reap edge=mixed_store after={readback:?}");

        // Full-state verification against the filesystem itself.
        assert!(
            !old_ok.job_dir.exists(),
            "aged terminal ok job must be gone from disk"
        );
        assert!(
            !old_exited.job_dir.exists(),
            "aged terminal exited job must be gone from disk"
        );
        assert!(
            !old_finalizing.job_dir.exists(),
            "aged abandoned finalizing job must be gone from disk"
        );
        assert!(
            recent_ok.job_dir.exists(),
            "recently-terminal job must be retained on disk"
        );
        assert!(
            recent_finalizing.job_dir.exists(),
            "in-flight finalizing job must be retained on disk"
        );
        assert!(
            running.job_dir.exists(),
            "live running job must be retained on disk"
        );
        assert!(
            corrupt_paths.job_dir.exists(),
            "unreadable job must be retained on disk"
        );

        // Readback accounting must exactly partition the seven scanned dirs.
        assert_eq!(readback.scanned_job_dirs, 7, "readback: {readback:?}");
        assert_eq!(readback.reaped_stale_jobs, 3, "readback: {readback:?}");
        assert_eq!(readback.retained_live_jobs, 1, "readback: {readback:?}");
        assert_eq!(
            readback.retained_recent_terminal_jobs, 2,
            "readback: {readback:?}"
        );
        assert_eq!(
            readback.skipped_unreadable_status_files, 1,
            "readback: {readback:?}"
        );
        assert_eq!(readback.reap_failures, 0, "readback: {readback:?}");
        assert!(
            readback.bytes_reclaimed > 0,
            "reaped jobs had on-disk artifacts: {readback:?}"
        );
        let mut sample = readback.reaped_job_ids_sample.clone();
        sample.sort();
        assert_eq!(
            sample,
            vec!["reap-old-exited", "reap-old-finalizing", "reap-old-ok"]
        );

        // Idempotence: a second pass finds nothing new to reap.
        let second = reap_stale_shell_jobs()
            .unwrap_or_else(|error| panic!("second reap should succeed: {error}"));
        assert_eq!(second.reaped_stale_jobs, 0, "second pass: {second:?}");
        assert_eq!(second.scanned_job_dirs, 4, "second pass: {second:?}");
    }

    // #1510 boundary audit: the TTL is honored, not hardcoded. The same
    // ~2-hour-old terminal job is reaped under a 1-hour TTL and retained under a
    // 1-day TTL.
    #[test]
    fn reap_stale_shell_jobs_honors_ttl_boundary() {
        let _root_guard = ShellJobRootGuard::new();
        let root = shell_durable_job_root_dir()
            .unwrap_or_else(|error| panic!("durable root should resolve: {error}"));
        fs::create_dir_all(&root)
            .unwrap_or_else(|error| panic!("durable root should create: {error}"));
        let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();

        let under_short_ttl = seed_synthetic_shell_job(
            &root,
            "reap-ttl-short",
            "ok",
            &two_hours_ago,
            Some(&two_hours_ago),
        );
        let short = reap_stale_shell_jobs_with_ttl(Duration::from_secs(60 * 60))
            .unwrap_or_else(|error| panic!("short-ttl reap should succeed: {error}"));
        assert!(
            !under_short_ttl.job_dir.exists(),
            "2h-old job must be reaped under a 1h TTL: {short:?}"
        );
        assert_eq!(short.reaped_stale_jobs, 1, "short-ttl readback: {short:?}");

        let under_long_ttl = seed_synthetic_shell_job(
            &root,
            "reap-ttl-long",
            "ok",
            &two_hours_ago,
            Some(&two_hours_ago),
        );
        let long = reap_stale_shell_jobs_with_ttl(Duration::from_secs(24 * 60 * 60))
            .unwrap_or_else(|error| panic!("long-ttl reap should succeed: {error}"));
        assert!(
            under_long_ttl.job_dir.exists(),
            "2h-old job must be retained under a 1d TTL: {long:?}"
        );
        assert_eq!(long.reaped_stale_jobs, 0, "long-ttl readback: {long:?}");
        assert_eq!(
            long.retained_recent_terminal_jobs, 1,
            "long-ttl readback: {long:?}"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_auto_background_uses_explicit_durable_timeout() {
        let inline_await_limit_ms = 1;
        let mut params = shell_params(
            "cmd.exe",
            vec!["/c", "echo auto-durable-timeout-ok"],
            DEFAULT_SHELL_TIMEOUT_MS,
        );
        params.durable_timeout_ms = Some(5_000);
        let mut config = shell_config_for(&params);
        config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("auto background with durable timeout should run: {error}"),
        };

        println!("readback=act_run_shell edge=auto_background_timeout after=response:{response:?}");
        assert!(response.backgrounded);
        assert_eq!(
            response.background_reason.as_deref(),
            Some("timeout_exceeds_inline_await_budget")
        );
        assert_eq!(response.durable_timeout_ms, Some(5_000));
        let job_id = response.job_id.clone().expect("job id should be returned");
        let job = response.job.expect("job should be returned");
        assert_eq!(job.timeout_ms, Some(5_000));
        assert_durable_job_finishes_ok(&job_id, "auto-durable-timeout-ok").await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_durable_mode_returns_job_without_inline_limit() {
        let inline_await_limit_ms = DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS;
        let mut params = shell_params(
            "cmd.exe",
            vec!["/c", "echo explicit-durable-mode-ok"],
            DEFAULT_SHELL_TIMEOUT_MS,
        );
        params.execution_mode = ActRunShellExecutionMode::Durable;
        params.durable_timeout_ms = Some(5_000);
        let mut config = shell_config_for(&params);
        config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("durable execution mode should return a job handle: {error}"),
        };

        println!("readback=act_run_shell edge=durable_mode after=response:{response:?}");
        assert!(response.backgrounded);
        assert_eq!(
            response.background_reason.as_deref(),
            Some("execution_mode_durable")
        );
        assert_eq!(response.inline_await_limit_ms, Some(inline_await_limit_ms));
        assert_eq!(
            response.requested_execution_mode,
            Some(ActRunShellExecutionMode::Durable)
        );
        assert_eq!(
            response.effective_execution_mode,
            Some(ActRunShellExecutionMode::Durable)
        );
        let job_id = response.job_id.clone().expect("job id should be returned");
        let job = response.job.expect("job should be returned");
        assert_eq!(job.timeout_ms, Some(5_000));
        assert_durable_job_finishes_ok(&job_id, "explicit-durable-mode-ok").await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_inline_ignores_durable_timeout_when_execution_stays_inline() {
        let mut params = shell_params(
            "cmd.exe",
            vec!["/c", "echo inline-durable-timeout-ignored"],
            30_000,
        );
        params.execution_mode = ActRunShellExecutionMode::Inline;
        params.durable_timeout_ms = Some(5_000);
        let config = shell_config_for(&params);
        let details = run_shell_request_details(&params, config.run_shell_inline_await_limit_ms());

        println!("readback=act_run_shell edge=inline_plus_durable_timeout before={details}");
        assert_eq!(details["will_background"], false);
        assert_eq!(
            details["durable_timeout_policy"],
            "ignored_inline_execution"
        );
        assert_eq!(details["durable_timeout_ms"], 5_000);
        assert!(details["durable_timeout_ms_if_backgrounded"].is_null());

        let response = match run_shell(&config, params).await {
            Ok(response) => response,
            Err(error) => panic!("inline durable timeout should be ignored inline: {error}"),
        };

        println!(
            "readback=act_run_shell edge=inline_plus_durable_timeout after_response={response:?}"
        );
        assert!(!response.backgrounded);
        assert_eq!(response.exit_code, Some(0));
        assert!(response.stdout.contains("inline-durable-timeout-ignored"));
        assert_eq!(response.durable_timeout_ms, None);
        assert_eq!(response.job_id, None);
        assert!(response.job.is_none());
    }

    #[cfg(windows)]
    async fn assert_durable_job_finishes_ok(job_id: &str, expected_stdout: &str) {
        for _ in 0..100 {
            let status = shell_job_status(
                &ActRunShellStatusParams {
                    job_id: job_id.to_owned(),
                    tail_bytes: 4096,
                },
                None,
            )
            .unwrap_or_else(|error| panic!("status should read durable job state: {error}"));
            println!("readback=act_run_shell edge=durable_completion after=status:{status:?}");
            if status.job.status == "finalizing" {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            if !status.running {
                assert_eq!(status.job.status, "ok");
                assert_eq!(status.job.exit_code, Some(0));
                assert!(status.stdout_tail.contains(expected_stdout), "{status:?}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        panic!("background job {job_id} did not complete within the regression readback window");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_durable_timeout_persists_budget_expired_code() {
        // Hermetic durable-job root so this test neither scans nor is perturbed
        // by jobs written by other tests running in parallel (#1509).
        let _root_guard = ShellJobRootGuard::new();
        let timeout_ms = 200;
        let args = vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Start-Sleep -Milliseconds 2000".to_owned(),
        ];
        let auth_params = shell_params(
            "powershell.exe",
            args.iter().map(String::as_str).collect(),
            timeout_ms,
        );
        let authorization = authorize_run_shell(&shell_config_for(&auth_params), &auth_params)
            .unwrap_or_else(|error| panic!("durable timeout shell should authorize: {error}"));
        let started = start_authorized_shell_job(
            ActRunShellStartParams {
                command: "powershell.exe".to_owned(),
                args,
                working_dir: None,
                env: BTreeMap::new(),
                timeout_ms: Some(timeout_ms),
                job_id: None,
            },
            &authorization,
            None,
        )
        .unwrap_or_else(|error| panic!("durable timeout shell should start: {error}"));
        let job_id = started.job.job_id.clone();

        for _ in 0..100 {
            let status = shell_job_status(
                &ActRunShellStatusParams {
                    job_id: job_id.clone(),
                    tail_bytes: 4096,
                },
                None,
            )
            .unwrap_or_else(|error| panic!("durable timeout status should read: {error}"));
            println!("readback=act_run_shell_start edge=timeout_budget after=status:{status:?}");
            if status.job.status == "finalizing" || status.running {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            assert_eq!(status.job.status, "timed_out");
            assert!(status.job.timed_out);
            assert_eq!(
                status.job.error_code.as_deref(),
                Some(error_codes::ACTION_BUDGET_EXPIRED)
            );
            assert!(
                status
                    .job
                    .error_message
                    .as_deref()
                    .is_some_and(|message| message.contains("200 ms")),
                "{status:?}"
            );
            return;
        }

        panic!("durable timeout job {job_id} did not finish within the regression readback window");
    }

    // #1580 deterministic reproduction: under CPU starvation `tokio::time::timeout`
    // delivers a child's own exit even when the deadline already fired (it polls
    // the inner future before the timer), so a job that outran its cap was
    // silently reported as a clean exit instead of timed_out. We reproduce the
    // starvation exactly — a busy-loop task hogs the single runtime worker so the
    // deadline timer cannot fire before the real child self-exits — and assert
    // the budget is enforced from the measured wait, not the wait-vs-timer race.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn wait_shell_job_child_enforces_budget_when_starved_timer_misses_self_exit() {
        // Real child that self-exits well past the 80 ms cap (~200 ms sleep plus
        // interpreter startup). No mock: a genuine process on a genuine timer.
        let mut child = TokioCommand::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Milliseconds 200"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn starvation child: {error}"));

        // Hog the single worker with a non-yielding busy loop so the 80 ms
        // deadline timer is starved past the child's self-exit; when the wait is
        // finally polled both the child exit and the blown deadline are ready and
        // `tokio::time::timeout` returns the child's `Ok(exit)`.
        let hog = tokio::spawn(async {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(800) {
                std::hint::spin_loop();
            }
        });

        let (exit_code, timed_out, wait_error) = wait_shell_job_child(&mut child, Some(80)).await;
        hog.await
            .unwrap_or_else(|error| panic!("hog task join: {error}"));

        println!(
            "readback=wait_shell_job_child edge=starved_self_exit after=exit_code:{exit_code:?} timed_out:{timed_out} wait_error:{wait_error:?}"
        );
        assert!(
            wait_error.is_none(),
            "no wait error expected: {wait_error:?}"
        );
        assert!(
            timed_out,
            "a job that ran ~200 ms under an 80 ms cap must be timed_out even when the starved timer let it self-exit (exit_code={exit_code:?})"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_session_cleanup_retains_live_durable_jobs() {
        // Hermetic durable-job root: the cleanup scan now enumerates only this
        // test's own jobs instead of every job in the process-wide shared root,
        // so a parallel test mutating that root can never flip this test's
        // exact-count assertions (#1509).
        let _root_guard = ShellJobRootGuard::new();
        let args = vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Start-Sleep -Milliseconds 5000".to_owned(),
        ];
        let auth_params = shell_params(
            "powershell.exe",
            args.iter().map(String::as_str).collect(),
            30_000,
        );
        let authorization = authorize_run_shell(&shell_config_for(&auth_params), &auth_params)
            .unwrap_or_else(|error| panic!("durable cleanup shell should authorize: {error}"));
        // #1334: a unique session id per run so this test can never count a prior
        // run's (now dead-PID) durable job as live — combined with the PID-liveness
        // reconcile in cleanup_shell_jobs_for_session, the live count is exact.
        let session_label = format!("issue1334-cleanup-retain-{}", uuid::Uuid::new_v4());
        let context = shell_execution_context_for_session(&session_label)
            .unwrap_or_else(|error| panic!("shell context should build: {error}"));
        let started = start_authorized_shell_job(
            ActRunShellStartParams {
                command: "powershell.exe".to_owned(),
                args,
                working_dir: None,
                env: BTreeMap::new(),
                timeout_ms: Some(30_000),
                job_id: None,
            },
            &authorization,
            Some(&context),
        )
        .unwrap_or_else(|error| panic!("durable cleanup shell should start: {error}"));
        let job_id = started.job.job_id.clone();

        let foreign_session_label = format!("issue1461-cleanup-foreign-{}", uuid::Uuid::new_v4());
        let foreign_context = shell_execution_context_for_session(&foreign_session_label)
            .unwrap_or_else(|error| panic!("foreign shell context should build: {error}"));
        let foreign_started = start_authorized_shell_job(
            ActRunShellStartParams {
                command: "powershell.exe".to_owned(),
                args: vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Milliseconds 5000".to_owned(),
                ],
                working_dir: None,
                env: BTreeMap::new(),
                timeout_ms: Some(30_000),
                job_id: None,
            },
            &authorization,
            Some(&foreign_context),
        )
        .unwrap_or_else(|error| panic!("foreign durable cleanup shell should start: {error}"));
        let foreign_job_id = foreign_started.job.job_id.clone();

        let cleanup = cleanup_shell_jobs_for_session(context.session_id(), "regression_stale")
            .unwrap_or_else(|error| panic!("session cleanup readback should succeed: {error}"));
        println!("readback=act_run_shell_session_cleanup edge=retain after={cleanup:?}");
        assert_eq!(cleanup.live_jobs_before, 1);
        assert_eq!(cleanup.retained_live_jobs, 1);
        // The hermetic root (#1509) means the scan sees exactly this test's two
        // jobs — the retained live one and the synthetic foreign one — and never
        // the process-wide pile of jobs left by other tests or prior runs.
        assert_eq!(
            cleanup.status_files_read, 2,
            "hermetic scan must read only this test's two jobs: {cleanup:?}"
        );
        assert_eq!(
            cleanup.skipped_foreign_jobs, 1,
            "cleanup should count exactly the synthetic foreign job: {cleanup:?}"
        );
        // No sibling directory should be mutating this test's private root, so
        // the concurrent-mutation reconciliation counter must stay at zero.
        assert_eq!(cleanup.skipped_concurrently_mutated, 0);
        // The only durable job under this unique session is genuinely alive, so
        // nothing should be reaped as a phantom (#1334).
        assert_eq!(cleanup.reaped_phantom_jobs, 0);
        assert_eq!(cleanup.termination_attempted, 0);
        assert_eq!(cleanup.failed, 0);
        assert!(cleanup.job_ids.contains(&job_id));
        assert!(!cleanup.job_ids.contains(&foreign_job_id));

        let retained = shell_job_status(
            &ActRunShellStatusParams {
                job_id: job_id.clone(),
                tail_bytes: 4096,
            },
            Some("fresh-session-after-cleanup"),
        )
        .unwrap_or_else(|error| panic!("fresh session should read retained durable job: {error}"));
        println!("readback=act_run_shell_status edge=retained after={retained:?}");
        assert!(retained.running);
        assert_eq!(retained.job.status, "running");
        assert!(!retained.job.cancel_requested);

        let cancelled = cancel_shell_job(
            &ActRunShellJobIdParams {
                job_id: job_id.clone(),
            },
            Some("fresh-session-after-cleanup"),
        )
        .unwrap_or_else(|error| {
            panic!("fresh session should cancel retained durable job: {error}")
        });
        println!("readback=act_run_shell_cancel edge=retained_cleanup after={cancelled:?}");
        assert!(matches!(
            cancelled.status.job.status.as_str(),
            "cancelled" | "timed_out" | "exited_unobserved"
        ));

        let foreign_cancelled = cancel_shell_job(
            &ActRunShellJobIdParams {
                job_id: foreign_job_id.clone(),
            },
            Some(foreign_context.session_id()),
        )
        .unwrap_or_else(|error| panic!("foreign owner should cancel retained job: {error}"));
        println!("readback=act_run_shell_cancel edge=foreign_cleanup after={foreign_cancelled:?}");
        assert!(matches!(
            foreign_cancelled.status.job.status.as_str(),
            "cancelled" | "timed_out" | "exited_unobserved"
        ));
    }

    #[test]
    fn launch_rejects_zero_timeout_and_accepts_large_caller_budget() {
        let zero = launch_params("notepad.exe", Vec::new(), 0);
        let error = match validate_launch_params(&zero) {
            Ok(()) => panic!("zero timeout should reject"),
            Err(error) => error,
        };

        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert!(error.message.contains("timeout_ms must be >= 1"));

        let large = launch_params("notepad.exe", Vec::new(), 1_200_000);
        validate_launch_params(&large)
            .unwrap_or_else(|error| panic!("large explicit caller timeout should accept: {error}"));
    }

    #[test]
    fn launch_process_history_row_records_spawn_without_env_values() {
        let mut params = launch_params("notepad.exe", vec!["C:\\tmp\\launch.txt"], 10_000);
        params.env.insert(
            "SYNAPSE_LAUNCH_SECRET".to_owned(),
            "do-not-store".to_owned(),
        );
        let response = ActLaunchResponse {
            pid: 1234,
            hwnd: Some(5678),
            window_owner_pid: Some(1234),
            reused_existing_window: false,
            matched_title: Some("launch.txt - Notepad".to_owned()),
            launched_at: "2026-05-31T20:00:00Z".to_owned(),
            reason: None,
            cdp_debug_port: None,
            cdp_endpoint: None,
            cdp_user_data_dir: None,
            cdp_verified_url: None,
            cdp_verified_title: None,
            desktop: None,
        };

        let row = launch_process_history_row(&params, &response)
            .unwrap_or_else(|error| panic!("process history row should encode: {error}"));
        let value: serde_json::Value = serde_json::from_slice(&row)
            .unwrap_or_else(|error| panic!("process history row should decode: {error}"));

        assert_eq!(value["tool"], "act_launch");
        assert_eq!(value["pid"], 1234);
        assert_eq!(value["hwnd"], 5678);
        assert_eq!(value["matched_title"], "launch.txt - Notepad");
        assert_eq!(value["env_keys"], json!(["SYNAPSE_LAUNCH_SECRET"]));
        assert_eq!(value["cdp_debug"], serde_json::Value::Null);
        assert_eq!(value["cdp_debug_port"], serde_json::Value::Null);
        assert_eq!(value["cdp_endpoint"], serde_json::Value::Null);
        assert_eq!(value["cdp_user_data_dir"], serde_json::Value::Null);
        assert_eq!(value["cdp_verified_url"], serde_json::Value::Null);
        assert_eq!(value["cdp_verified_title"], serde_json::Value::Null);
        assert!(!String::from_utf8_lossy(&row).contains("do-not-store"));
        assert!(
            String::from_utf8_lossy(&launch_process_history_row_key(&response)).contains("1234")
        );
    }

    #[test]
    fn launch_process_history_row_records_cdp_launch_fields() {
        let mut params = launch_params("chrome.exe", vec!["https://example.test"], 10_000);
        params.cdp_debug = Some(true);
        let response = ActLaunchResponse {
            pid: 2222,
            hwnd: Some(3333),
            window_owner_pid: Some(2222),
            reused_existing_window: false,
            matched_title: Some("Synthetic CDP Page - Google Chrome".to_owned()),
            launched_at: "2026-06-03T23:00:00Z".to_owned(),
            reason: None,
            cdp_debug_port: Some(45678),
            cdp_endpoint: Some("http://127.0.0.1:45678".to_owned()),
            cdp_user_data_dir: Some("C:\\Temp\\synapse-cdp-profiles\\synthetic".to_owned()),
            cdp_verified_url: Some("https://example.test/".to_owned()),
            cdp_verified_title: Some("Synthetic CDP Page".to_owned()),
            desktop: None,
        };

        let row = launch_process_history_row(&params, &response)
            .unwrap_or_else(|error| panic!("process history row should encode: {error}"));
        let value: serde_json::Value = serde_json::from_slice(&row)
            .unwrap_or_else(|error| panic!("process history row should decode: {error}"));

        println!(
            "readback=act_launch_history_cdp before=port:{:?} after=row_port:{} endpoint:{}",
            response.cdp_debug_port, value["cdp_debug_port"], value["cdp_endpoint"]
        );
        assert_eq!(value["cdp_debug"], true);
        assert_eq!(value["cdp_debug_port"], 45678);
        assert_eq!(value["cdp_endpoint"], "http://127.0.0.1:45678");
        assert_eq!(
            value["cdp_user_data_dir"],
            "C:\\Temp\\synapse-cdp-profiles\\synthetic"
        );
        assert_eq!(value["cdp_verified_url"], "https://example.test/");
        assert_eq!(value["cdp_verified_title"], "Synthetic CDP Page");
    }

    #[test]
    fn launch_console_targets_request_real_console_windows() {
        for target in [
            "cmd",
            "cmd.exe",
            "C:\\Windows\\System32\\cmd.exe",
            "powershell",
            "powershell.exe",
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
        ] {
            assert!(
                launch_target_needs_new_console(target),
                "{target} should request CREATE_NEW_CONSOLE on Windows"
            );
        }

        for target in ["notepad.exe", "wt.exe", "WindowsTerminal.exe"] {
            assert!(
                !launch_target_needs_new_console(target),
                "{target} should use normal GUI launch stdio handling"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_launch_startup_state_is_non_activating_for_gui_and_hidden_for_console() {
        let gui = launch_params("notepad.exe", Vec::new(), 10_000);
        let console = launch_params("cmd.exe", vec!["/c", "exit"], 10_000);

        let gui_show = windows_launch_show_window(&gui);
        let console_show = windows_launch_show_window(&console);

        println!(
            "readback=act_launch_startup_show_state before=gui:notepad.exe,console:cmd.exe after=gui:{gui_show} console:{console_show}"
        );
        assert_eq!(gui_show, SW_SHOWNOACTIVATE);
        assert_eq!(console_show, SW_HIDE);
    }

    #[cfg(windows)]
    #[test]
    fn windows_launch_creation_flags_do_not_hide_gui_targets() {
        use windows::Win32::System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
        };

        let gui = launch_params("notepad.exe", Vec::new(), 10_000);
        let console = launch_params("cmd.exe", vec!["/c", "exit"], 10_000);

        let gui_flags = windows_launch_creation_flags(&gui);
        let console_flags = windows_launch_creation_flags(&console);

        println!(
            "readback=act_launch_creation_flags before=gui:notepad.exe,console:cmd.exe after=gui:0x{:x} console:0x{:x}",
            gui_flags.0, console_flags.0
        );
        assert_ne!(gui_flags.0 & CREATE_UNICODE_ENVIRONMENT.0, 0);
        assert_eq!(gui_flags.0 & CREATE_NO_WINDOW.0, 0);
        assert_eq!(gui_flags.0 & CREATE_NEW_PROCESS_GROUP.0, 0);

        assert_ne!(console_flags.0 & CREATE_UNICODE_ENVIRONMENT.0, 0);
        assert_ne!(console_flags.0 & CREATE_NO_WINDOW.0, 0);
        assert_ne!(console_flags.0 & CREATE_NEW_PROCESS_GROUP.0, 0);
    }

    #[cfg(windows)]
    #[test]
    fn hidden_desktop_enum_missing_or_exhausted_is_empty_readback() {
        use windows::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES,
        };
        use windows::core::Error;

        let file_not_found = Error::from_hresult(ERROR_FILE_NOT_FOUND.to_hresult());
        let no_more_files = Error::from_hresult(ERROR_NO_MORE_FILES.to_hresult());
        let access_denied = Error::from_hresult(ERROR_ACCESS_DENIED.to_hresult());

        println!(
            "readback=act_launch_desktop_enum_error before=file_not_found,no_more_files,access_denied after=empty:{},{} fail:{}",
            desktop_window_enum_error_means_empty(&file_not_found),
            desktop_window_enum_error_means_empty(&no_more_files),
            desktop_window_enum_error_means_empty(&access_denied)
        );
        assert!(desktop_window_enum_error_means_empty(&file_not_found));
        assert!(desktop_window_enum_error_means_empty(&no_more_files));
        assert!(!desktop_window_enum_error_means_empty(&access_denied));
    }

    #[test]
    fn shell_idempotency_replays_matching_completed_row() {
        let mut params = shell_params("cmd.exe", vec!["/c", "echo replay"], 30_000);
        params.idempotency_key = Some("issue-606-replay".to_owned());
        let authorization = authorize_run_shell(&shell_config_for(&params), &params)
            .unwrap_or_else(|error| panic!("authorized shell params: {error}"));
        let response = ActRunShellResponse {
            exit_code: Some(0),
            stdout: "replay\r\n".to_owned(),
            stderr: String::new(),
            duration_ms: 12,
            timed_out: false,
            error_code: None,
            error_message: None,
            stdout_truncated: false,
            stderr_truncated: false,
            session_id: Some("session-a".to_owned()),
            effective_working_dir: Some("C:\\code\\Synapse".to_owned()),
            backgrounded: false,
            background_reason: None,
            inline_await_limit_ms: None,
            inline_client_call_budget_ms: None,
            requested_execution_mode: Some(ActRunShellExecutionMode::Auto),
            effective_execution_mode: Some(ActRunShellExecutionMode::Inline),
            durable_timeout_ms: None,
            job_id: None,
            job: None,
        };
        let row = run_shell_idempotency_completed_row(
            &params,
            &authorization,
            &response,
            Some("session-a"),
        )
        .unwrap_or_else(|error| panic!("completed idempotency row should encode: {error}"));

        let replay = run_shell_idempotency_replay(&params, &row, Some("session-a"))
            .unwrap_or_else(|error| panic!("matching idempotency row should replay: {error}"));

        assert_eq!(replay.stdout, "replay\r\n");
        assert_eq!(replay.exit_code, Some(0));
    }

    #[test]
    fn shell_idempotency_rejects_conflicting_request_reuse() {
        let mut first = shell_params("cmd.exe", vec!["/c", "echo first"], 30_000);
        first.idempotency_key = Some("issue-606-conflict".to_owned());
        let authorization = authorize_run_shell(&shell_config_for(&first), &first)
            .unwrap_or_else(|error| panic!("first shell params should authorize: {error}"));
        let response = ActRunShellResponse {
            exit_code: Some(0),
            stdout: "first\r\n".to_owned(),
            stderr: String::new(),
            duration_ms: 10,
            timed_out: false,
            error_code: None,
            error_message: None,
            stdout_truncated: false,
            stderr_truncated: false,
            session_id: Some("session-a".to_owned()),
            effective_working_dir: Some("C:\\code\\Synapse".to_owned()),
            backgrounded: false,
            background_reason: None,
            inline_await_limit_ms: None,
            inline_client_call_budget_ms: None,
            requested_execution_mode: Some(ActRunShellExecutionMode::Auto),
            effective_execution_mode: Some(ActRunShellExecutionMode::Inline),
            durable_timeout_ms: None,
            job_id: None,
            job: None,
        };
        let row = run_shell_idempotency_completed_row(
            &first,
            &authorization,
            &response,
            Some("session-a"),
        )
        .unwrap_or_else(|error| panic!("completed idempotency row should encode: {error}"));
        let mut second = shell_params("cmd.exe", vec!["/c", "echo second"], 30_000);
        second.idempotency_key = first.idempotency_key.clone();

        let error = match run_shell_idempotency_replay(&second, &row, Some("session-a")) {
            Ok(replay) => panic!("conflicting idempotency reuse should reject, got {replay:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("idempotency_key_conflict")
        );
    }

    #[test]
    fn shell_idempotency_key_is_partitioned_by_session() {
        let mut params = shell_params("cmd.exe", vec!["/c", "echo owner"], 30_000);
        params.idempotency_key = Some("issue-802-owner".to_owned());

        let session_a = run_shell_idempotency_row_key(&params, Some("session-a"))
            .unwrap_or_else(|error| panic!("session-a key should encode: {error}"));
        let session_b = run_shell_idempotency_row_key(&params, Some("session-b"))
            .unwrap_or_else(|error| panic!("session-b key should encode: {error}"));

        assert_ne!(session_a, session_b);
    }
}

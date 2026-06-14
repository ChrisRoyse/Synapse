use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
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
use serde_json::json;
use sha2::{Digest, Sha256};
use synapse_core::{
    Action, Backend, ComboInput, ComboStep, ForegroundContext, Key, error_codes, new_reflex_id,
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
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS: u64 = 90_000;
const DEFAULT_LAUNCH_TIMEOUT_MS: u64 = 10_000;
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
const SHELL_JOB_TAIL_DEFAULT_BYTES: u64 = 64 * 1024;
const SHELL_JOB_TAIL_MAX_BYTES: u64 = 1024 * 1024;
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
const SHELL_REMOTE_PROCESS_MARKER: &str = "SYNAPSE_REMOTE_PROCESS_V1";
const SHELL_REMOTE_CLEANUP_MARKER: &str = "SYNAPSE_REMOTE_CLEANUP_V1";
const SHELL_REMOTE_METADATA_PREFIX_BYTES: usize = 128 * 1024;
const SHELL_REMOTE_METADATA_WAIT_MS: u64 = 1_500;
const SHELL_REMOTE_CLEANUP_TIMEOUT_MS: u64 = 15_000;
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

    fn launch_match<'a>(&'a self, command_line: &str) -> Option<&'a str> {
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
        description = "Executable path or program name only. Do not include arguments, quotes, pipes, redirection, or other shell syntax here; pass arguments in args. For shell syntax, set command to an explicit shell executable such as powershell.exe or cmd.exe and put the shell flags/snippet in args."
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
        description = "Caller-requested inline wait budget in milliseconds. In execution_mode=inline this is honored directly, even above the daemon inline await limit. In execution_mode=auto, values above the inline await limit return a durable job handle."
    )]
    pub timeout_ms: u64,
    #[serde(default = "default_run_shell_execution_mode")]
    #[schemars(
        default = "default_run_shell_execution_mode",
        description = "Shell execution route: auto preserves compatibility and backgrounds only when timeout_ms exceeds the inline await limit; inline never backgrounds and waits up to timeout_ms; durable immediately returns a durable job handle."
    )]
    pub execution_mode: ActRunShellExecutionMode,
    #[serde(default)]
    #[schemars(
        default,
        range(min = 1),
        description = "Optional explicit durable job lifetime cap in milliseconds. Valid only when execution_mode=durable or when execution_mode=auto will background because timeout_ms exceeds the inline await limit; omit for an unbounded durable job."
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
        description = "Executable path or program name only. Do not include arguments, quotes, pipes, redirection, or other shell syntax here; pass arguments in args. For shell syntax, set command to an explicit shell executable such as powershell.exe or cmd.exe and put the shell flags/snippet in args."
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
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ShellSessionCleanupReadback {
    pub reason: String,
    pub session_id: String,
    pub job_root: Option<String>,
    pub status_files_read: usize,
    pub skipped_invalid_job_dirs: usize,
    pub skipped_unreadable_status_files: usize,
    pub live_jobs_before: usize,
    pub retained_live_jobs: usize,
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
    /// Optional work prompt for the spawned primary agent. Synapse prepends a
    /// mandatory provisioning preflight that calls health/tools through the real
    /// client MCP surface and binds the requested target.
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
    /// Pin a specific template version snapshot. Omit to render from the current
    /// version (whichever the pointer holds at request time); the version
    /// actually rendered is recorded on the spawn.
    #[serde(default)]
    #[schemars(default)]
    pub template_version: Option<u32>,
    /// Values for the template's declared `required_params`. The key set must
    /// equal the template's contract exactly — missing or unknown keys are
    /// rejected. Empty for a template with no parameters or a direct spawn.
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
    /// Direct-spawn work prompt. Must be omitted when `template_id` is set.
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
    pub task_started_script_path: String,
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
/// daemon running on a non-default port (e.g. an isolated FSV instance, or a
/// future multi-daemon layout) must hand its children its own endpoint, or they
/// connect to the wrong daemon's tools. Loopback is preserved verbatim.
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
    pub pid: u32,
    pub hwnd: Option<i64>,
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
        ActRunShellExecutionMode::Durable => Some("execution_mode_durable"),
        ActRunShellExecutionMode::Auto | ActRunShellExecutionMode::Inline => None,
    }
}

pub fn validate_run_shell_execution_plan(
    params: &ActRunShellParams,
    inline_await_limit_ms: u64,
) -> Result<(), ErrorData> {
    if params.durable_timeout_ms.is_some()
        && direct_shell_background_reason(params, inline_await_limit_ms).is_none()
    {
        return Err(shell_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_run_shell durable_timeout_ms applies only to durable/background execution; set execution_mode=\"durable\" or use execution_mode=\"auto\" with timeout_ms above inline_await_limit_ms",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "durable_timeout_requires_background",
                "execution_mode": params.execution_mode.as_str(),
                "timeout_ms": params.timeout_ms,
                "inline_await_limit_ms": inline_await_limit_ms,
                "durable_timeout_ms": params.durable_timeout_ms,
            }),
        ));
    }
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
        "background_reason": background_reason,
        "will_background": will_background,
        "durable_timeout_ms": params.durable_timeout_ms,
        "durable_timeout_ms_if_backgrounded": durable_timeout_ms_if_backgrounded,
        "durable_timeout_policy": if will_background && params.durable_timeout_ms.is_some() {
            "explicit_timeout_ms"
        } else if will_background {
            "unbounded_until_exit_or_cancel"
        } else if params.durable_timeout_ms.is_some() {
            "invalid_without_background"
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
    write_shell_job_status(&paths.status_path, &job)?;
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
            attempt_shell_job_remote_cleanup(&mut job, "act_run_shell_cancel", None);
        if job.remote_process_scope.remote_cleanup_required
            && !job.remote_process_scope.remote_cleanup_verified
            && job.remote_process_scope.remote_cleanup_status != SHELL_REMOTE_CLEANUP_FAILED
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
            attempt_shell_job_remote_cleanup(&mut job, "act_run_shell_cancel", None);
        if !job.remote_process_scope.remote_cleanup_verified
            && job.remote_process_scope.remote_cleanup_status != SHELL_REMOTE_CLEANUP_FAILED
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
    let root = shell_durable_job_root_dir()?;
    if !root.exists() {
        return Ok(ShellSessionCleanupReadback {
            reason: reason.to_owned(),
            session_id: session_id.to_owned(),
            job_root: Some(path_string(&root)),
            status_files_read: 0,
            skipped_invalid_job_dirs: 0,
            skipped_unreadable_status_files: 0,
            live_jobs_before: 0,
            retained_live_jobs: 0,
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
        live_jobs_before: 0,
        retained_live_jobs: 0,
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
        if ensure_shell_job_session_owner(&job, Some(session_id)).is_err() {
            continue;
        }
        if !shell_job_live_status(&job.status) {
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

pub async fn launch(
    config: &M4ServiceConfig,
    params: ActLaunchParams,
) -> Result<ActLaunchResponse, ErrorData> {
    Ok(launch_for_session(config, params, None).await?.response)
}

pub(crate) async fn launch_for_session(
    config: &M4ServiceConfig,
    params: ActLaunchParams,
    session_id: Option<&str>,
) -> Result<ActLaunchOutcome, ErrorData> {
    validate_launch_params(&params)?;
    let command_line = launch_command_line(&params)?;
    let Some(matched_pattern) = config.launch_match(&command_line) else {
        let reason = if config.allow_launch_count() == 0 {
            "no_allow_launch_policy"
        } else {
            "launch_command_not_allowlisted"
        };
        return Err(policy_error(
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
        ));
    };
    let matched_pattern = matched_pattern.to_owned();
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
    let excluded_hwnds = if wait_regex.is_some() {
        snapshot_visible_window_hwnds()
    } else {
        HashSet::new()
    };
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
    let launch_desktop = prepare_launch_desktop(params.desktop.as_deref(), session_id)?;
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
    let launch_target_name = launch_target_file_name(&params.target);

    let cdp_target =
        verify_launched_chromium_url(&params, cdp_launch.as_ref(), &cdp, params.timeout_ms).await?;
    let window = if let Some(regex) = wait_regex {
        wait_for_launch_window(
            pid,
            &regex,
            params.timeout_ms,
            &excluded_hwnds,
            &launch_target_name,
            &params.args,
        )
        .await?
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
    Ok(ActLaunchOutcome {
        response: ActLaunchResponse {
            pid,
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
    if !synapse_a11y::is_chromium_family(&launch_target_file_name(&params.target)) {
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
    let is_chromium = synapse_a11y::is_chromium_family(&launch_target_file_name(&params.target));
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
    validate_chromium_debug_launch_policy(params)?;
    Ok(())
}

fn validate_launch_desktop_option(params: &ActLaunchParams) -> Result<(), ErrorData> {
    let Some(desktop) = params.desktop.as_deref() else {
        return Ok(());
    };
    if params.wait_for_window_title_regex.is_some() {
        return Err(launch_tool_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_launch desktop routing cannot be combined with wait_for_window_title_regex until hidden-desktop window enumeration is the verified wait source",
            json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "reason": "desktop_window_wait_not_supported",
                "desktop": desktop,
                "wait_for_window_title_regex": params.wait_for_window_title_regex,
            }),
        ));
    }
    validate_launch_desktop_request(desktop)
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
    let is_chromium = synapse_a11y::is_chromium_family(&launch_target_file_name(&params.target));
    if !is_chromium && params.cdp_debug != Some(true) {
        return Ok(());
    }
    if !has_remote_debugging_arg(&params.args) {
        return Ok(());
    }

    let user_data_dir = user_data_dir_arg(&params.args);
    let user_data_dir_state = user_data_dir
        .as_deref()
        .map(chromium_user_data_dir_safety)
        .unwrap_or(ChromiumUserDataDirSafety::Missing);
    let silent_debugger = params
        .args
        .iter()
        .any(|arg| is_switch_arg(arg, "--silent-debugger-extension-api"));
    let disable_extensions = params
        .args
        .iter()
        .any(|arg| is_switch_arg(arg, "--disable-extensions"));
    let loads_extensions = params.args.iter().any(|arg| {
        is_switch_arg(arg, "--load-extension") || is_switch_arg(arg, "--disable-extensions-except")
    });

    if silent_debugger
        && disable_extensions
        && !loads_extensions
        && matches!(user_data_dir_state, ChromiumUserDataDirSafety::Dedicated)
    {
        return Ok(());
    }

    Err(launch_tool_error(
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
        "act_launch refused a Chromium remote-debugging launch that could surface Chrome debugger/native-host UI on an end-user profile",
        json!({
            "code": error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
            "reason": "chromium_remote_debugging_not_popup_safe",
            "target": params.target,
            "args": params.args,
            "user_data_dir": user_data_dir,
            "user_data_dir_state": user_data_dir_state.as_str(),
            "has_silent_debugger_extension_api": silent_debugger,
            "has_disable_extensions": disable_extensions,
            "has_extension_loading_flags": loads_extensions,
            "required_invariant": "remote-debugging Chromium launches must use a non-default dedicated user-data-dir, --silent-debugger-extension-api, --disable-extensions, and no extension-loading flags",
            "remediation": "omit caller-supplied remote-debugging/profile flags so Synapse injects its isolated automation profile, or pass the required flags against a non-default automation profile; never debug the user's normal Chrome profile",
        }),
    ))
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
    let name = launch_target_file_name(target);
    matches!(name.as_str(), "cmd.exe" | "powershell.exe" | "pwsh.exe")
}

fn launch_target_file_name(target: &str) -> String {
    Path::new(target)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(target)
        .to_ascii_lowercase()
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
fn desktop_window_enum_error_means_empty(error: &windows::core::Error) -> bool {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES};

    let code = error.code();
    code == ERROR_FILE_NOT_FOUND.to_hresult() || code == ERROR_NO_MORE_FILES.to_hresult()
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
    matched_title: Option<String>,
    reason: Option<String>,
}

impl WindowWaitResult {
    const fn not_requested() -> Self {
        Self {
            hwnd: None,
            matched_title: None,
            reason: None,
        }
    }

    fn matched(context: ForegroundContext) -> Self {
        Self {
            hwnd: Some(context.hwnd),
            matched_title: Some(context.window_title),
            reason: None,
        }
    }
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
    loop {
        match synapse_a11y::visible_top_level_window_contexts() {
            Ok(contexts) => {
                last_windows = window_context_summaries(&contexts);
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

fn launch_window_error(
    reason: &'static str,
    pid: u32,
    title_regex: &str,
    timeout_ms: u64,
    last_error: Option<String>,
    observed_windows: &[serde_json::Value],
) -> ErrorData {
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
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
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
        job_dir,
    }
}

fn shell_job_root_dir() -> Result<PathBuf, ErrorData> {
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
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, bytes).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell failed to write shell job status temp file: {error}"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": tmp_path,
                "reason": "job_status_temp_write_failed",
            }),
        )
    })?;
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell failed to replace shell job status file: {error}"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": path,
                "reason": "job_status_replace_failed",
            }),
        ));
    }
    fs::rename(&tmp_path, path).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell failed to commit shell job status file: {error}"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": path,
                "tmp_path": tmp_path,
                "reason": "job_status_rename_failed",
            }),
        )
    })
}

fn read_shell_job_status(path: &Path, job_id: &str) -> Result<ActRunShellJobStatus, ErrorData> {
    let bytes = fs::read(path).map_err(|error| {
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
    if let Some(client) = ssh_family_client_for_executable(&params.command) {
        let evidence = if client == "ssh" {
            "direct_command_ssh".to_owned()
        } else {
            format!("direct_command_ssh_family:{client}")
        };
        ssh_remote_process_scope(&params.command, &params.args, evidence)
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

fn shell_spawn_command<'a>(command: &'a str) -> Cow<'a, str> {
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
struct SshRemoteTrackingPlan {
    spawn_args: Vec<String>,
    remote_identity: String,
    remote_command: String,
    marker: String,
}

fn shell_job_spawn_args(params: &ActRunShellStartParams, job_id: &str) -> Vec<String> {
    if let Some(plan) = ssh_remote_tracking_plan(&params.command, &params.args, job_id) {
        tracing::info!(
            code = "M4_ACT_RUN_SHELL_SSH_REMOTE_TRACKING_ENABLED",
            job_id,
            remote_identity = %plan.remote_identity,
            marker = %plan.marker,
            remote_command_sha256 = %sha256_hex(plan.remote_command.as_bytes()),
            "act_run_shell_start will capture SSH remote pid/process-group metadata"
        );
        return plan.spawn_args;
    }
    params.args.clone()
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
    let remote_wrapper = ssh_remote_tracking_command(&marker, &remote_command);
    let mut spawn_args = parts.control_args.clone();
    spawn_args.push(remote_wrapper);
    Some(SshRemoteTrackingPlan {
        spawn_args,
        remote_identity: parts.remote_identity,
        remote_command,
        marker,
    })
}

fn ssh_remote_tracking_command(marker: &str, remote_command: &str) -> String {
    const SCRIPT: &str = r#"marker=$1
cmd=$2
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
"#;
    format!(
        "sh -c {} synapse-remote-tracker {} {}",
        posix_single_quote(SCRIPT),
        posix_single_quote(marker),
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
struct RemoteProcessMetadata {
    job_id: String,
    pid: String,
    pgid: String,
    sid: Option<String>,
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
        let _ = attempt_shell_job_remote_cleanup(job, trigger, original_args);
        return;
    }

    if job.remote_process_scope.remote_cleanup_status == SHELL_REMOTE_CLEANUP_TRACKING_PENDING {
        let terminal_status = job.status.clone();
        mark_shell_job_remote_cleanup_unverified(job, trigger, &terminal_status);
    }
}

fn parse_remote_process_metadata(
    stderr: &str,
    expected_job_id: &str,
) -> Option<RemoteProcessMetadata> {
    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix(SHELL_REMOTE_PROCESS_MARKER) else {
            continue;
        };
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
    let Some(parts) = ssh_cleanup_command_parts(job, original_args) else {
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
        &job.command,
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

fn ssh_cleanup_command_parts(
    job: &ActRunShellJobStatus,
    original_args: Option<&[String]>,
) -> Option<SshCommandParts> {
    ssh_direct_command_parts(original_args.unwrap_or(&job.args))
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

fn ensure_shell_job_session_owner(
    job: &ActRunShellJobStatus,
    requesting_session_id: Option<&str>,
) -> Result<(), ErrorData> {
    let Some(requesting_session_id) = requesting_session_id else {
        return Ok(());
    };
    if job.session_id.as_deref() == Some(requesting_session_id) {
        return Ok(());
    }
    Err(shell_tool_error(
        error_codes::TOOL_PARAMS_INVALID,
        "act_run_shell job is not owned by this MCP session",
        json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "job_id": job.job_id,
            "requesting_session_id": requesting_session_id,
            "owner_session_id": job.session_id.clone(),
            "reason": "job_session_owner_mismatch",
        }),
    ))
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
    let spawn_command = shell_spawn_command(&params.command);
    let mut command = TokioCommand::new(spawn_command.as_ref());
    let spawn_args = shell_job_spawn_args(params, job_id);
    command.args(&spawn_args);
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

async fn wait_shell_job_child(
    child: &mut tokio::process::Child,
    timeout_ms: Option<u64>,
) -> (Option<i32>, bool, Option<String>) {
    match timeout_ms {
        Some(timeout_ms) => {
            match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
                Ok(Ok(status)) => (status.code(), false, None),
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
    file.by_ref()
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
            "execution_mode=\"inline\" honors any timeout_ms with no daemon cap, so raise timeout_ms \
             to allow more time, or switch to execution_mode=\"durable\" (or act_run_shell_start) for \
             an unbounded background job polled with act_run_shell_status"
                .to_owned()
        }
        // Durable execution backgrounds before reaching the inline path, so this arm is defensive.
        ActRunShellExecutionMode::Auto | ActRunShellExecutionMode::Durable => format!(
            "raise timeout_ms above the {inline_await_limit_ms} ms inline await limit to auto-background \
             into a durable job polled with act_run_shell_status, or set execution_mode=\"inline\" to \
             wait for any timeout_ms inline with no daemon cap"
        ),
    };
    (
        Some(error_codes::ACTION_BUDGET_EXPIRED.to_owned()),
        Some(format!(
            "caller timeout_ms budget expired after {timeout_ms} ms; the process tree was terminated. \
             To allow more time in a single call: {remediation}."
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
            &raw const limits as *const _,
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

async fn join_capped_stream(
    task: CappedStreamTask,
    stream_name: &'static str,
) -> Result<CappedOutput, ErrorData> {
    task.await
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
    fn launch_desktop_option_refuses_visible_window_wait() {
        let mut params = launch_params("notepad.exe", Vec::new(), 10_000);
        params.desktop = Some("agent:session".to_owned());
        params.wait_for_window_title_regex = Some(".*".to_owned());

        let error = validate_launch_params(&params)
            .expect_err("desktop launch should not use visible-desktop window wait");

        println!(
            "readback=act_launch_desktop_validation edge=wait_refused before=desktop:{:?},wait:{:?} after={:?}",
            params.desktop, params.wait_for_window_title_regex, error.data
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("desktop_window_wait_not_supported")
        );
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
    fn shell_job_reconciliation_preserves_monitor_terminal_status() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
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
    fn shell_terminal_not_tracked_ssh_status_is_preserved() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
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
    fn ssh_cleanup_command_parts_prefers_live_original_args_over_safe_status_args() {
        let temp = tempfile::TempDir::new()
            .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
        let paths = ShellJobPaths {
            job_dir: temp.path().to_path_buf(),
            stdout_path: temp.path().join("stdout.log"),
            stderr_path: temp.path().join("stderr.log"),
            status_path: temp.path().join("status.json"),
            request_path: temp.path().join("request.json"),
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

        let live_parts = ssh_cleanup_command_parts(&status, Some(&original_args))
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

        let persisted_parts = ssh_cleanup_command_parts(&status, None)
            .unwrap_or_else(|| panic!("parse persisted cleanup args"));
        assert!(
            persisted_parts
                .control_args
                .iter()
                .any(|arg| arg.contains("[redacted-arg:"))
        );
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
        assert!(response.duration_ms < 2_000, "{response:?}");
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

        println!(
            "readback=act_run_shell_params edge=invalid_timeout_type after=error:{}",
            error
        );
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
        // still complete inline (no hard cap) when execution_mode="inline". The inline await limit
        // only governs the auto→durable background decision, never inline execution itself.
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
    async fn shell_budget_expiry_message_is_actionable() {
        // When the caller's own timeout_ms budget expires, the error must point at the concrete
        // escape hatch (execution_mode="inline" / the inline await limit) instead of dead-ending.
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
            message.contains("execution_mode=\"inline\""),
            "names the inline escape hatch: {message}"
        );
        assert!(
            message.contains("inline await limit"),
            "names the configurable inline await limit: {message}"
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
    async fn shell_rejects_durable_timeout_when_execution_stays_inline() {
        let mut params = shell_params("cmd.exe", vec!["/c", "echo should-not-run"], 30_000);
        params.execution_mode = ActRunShellExecutionMode::Inline;
        params.durable_timeout_ms = Some(5_000);
        let config = shell_config_for(&params);

        let error = match run_shell(&config, params).await {
            Ok(response) => panic!("inline durable timeout should fail closed: {response:?}"),
            Err(error) => error,
        };

        println!("readback=act_run_shell edge=inline_plus_durable_timeout after_error={error}");
        assert!(error.message.contains("durable_timeout_ms applies only"));
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

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_session_cleanup_retains_live_durable_jobs() {
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
        let context = shell_execution_context_for_session("issue812-cleanup-retain-session")
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

        let cleanup = cleanup_shell_jobs_for_session(context.session_id(), "regression_stale")
            .unwrap_or_else(|error| panic!("session cleanup readback should succeed: {error}"));
        println!("readback=act_run_shell_session_cleanup edge=retain after={cleanup:?}");
        assert_eq!(cleanup.live_jobs_before, 1);
        assert_eq!(cleanup.retained_live_jobs, 1);
        assert_eq!(cleanup.termination_attempted, 0);
        assert_eq!(cleanup.failed, 0);
        assert!(cleanup.job_ids.contains(&job_id));

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
            "cmd.exe",
            "C:\\Windows\\System32\\cmd.exe",
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

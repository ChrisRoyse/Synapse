use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    future::Future,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::{Arc, Mutex, OnceLock},
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
#[cfg(windows)]
use synapse_core::win32_hwnd::{hwnd_from_wire, hwnd_to_wire};
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
const PHYSICAL_MUTATION_BOUNDARY_POLL_INTERVAL: Duration = Duration::from_millis(25);
pub(crate) type PhysicalMutationBoundary<'a> =
    dyn Fn(&'static str) -> Result<(), ErrorData> + Send + Sync + 'a;
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
const SHELL_CLEANUP_CAPTURE_CAP_BYTES: u64 = 1024 * 1024;
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
const SHELL_JOB_STATUS_SPAWN_CLEANUP_UNVERIFIED: &str = "spawn_cleanup_unverified";
const SHELL_REMOTE_CLEANUP_TRANSPORT_LOST: &str = "transport_lost_process_may_still_run";
const SHELL_REMOTE_CLEANUP_ALREADY_GONE: &str = "remote_process_already_gone";
const SHELL_REMOTE_PROCESS_MARKER: &str = "SYNAPSE_REMOTE_PROCESS_V1";
const SHELL_REMOTE_EXIT_MARKER: &str = "SYNAPSE_REMOTE_EXIT_V1";
const SHELL_REMOTE_CLEANUP_MARKER: &str = "SYNAPSE_REMOTE_CLEANUP_V1";
const SHELL_REMOTE_LIVENESS_MARKER: &str = "SYNAPSE_REMOTE_LIVENESS_V1";
const SHELL_REMOTE_METADATA_PREFIX_BYTES: usize = 128 * 1024;
const SHELL_REMOTE_METADATA_WAIT_MS: u64 = 1_500;
const SHELL_REMOTE_CLEANUP_PIDFD_WAIT_MS: u64 = 12_000;
const SHELL_REMOTE_GROUP_ABSENCE_PROBE_ATTEMPTS: u64 = 25;
const SHELL_REMOTE_GROUP_ABSENCE_PROBE_INTERVAL_MS: u64 = 200;
const SHELL_REMOTE_CLEANUP_TRANSPORT_MARGIN_MS: u64 = 8_000;
// The caller must outlive every bounded step in the remote proof protocol. The
// margin covers SSH connection/process startup and evidence flush after the
// 12-second pidfd wait plus the five-second kernel PGID-absence proof.
const SHELL_REMOTE_CLEANUP_TIMEOUT_MS: u64 = SHELL_REMOTE_CLEANUP_PIDFD_WAIT_MS
    + SHELL_REMOTE_GROUP_ABSENCE_PROBE_ATTEMPTS * SHELL_REMOTE_GROUP_ABSENCE_PROBE_INTERVAL_MS
    + SHELL_REMOTE_CLEANUP_TRANSPORT_MARGIN_MS;
const SHELL_REMOTE_LIVENESS_TIMEOUT_MS: u64 = 2_500;
const SHELL_SSH_CONFIG_PREFLIGHT_TIMEOUT_MS: u64 = 2_500;
/// A hard safety backstop for reaping an exact child handle after termination
/// has already been requested. This is not a runtime-performance assertion: the
/// caller reports timeout as an explicit cleanup failure with the last process
/// state it could read, rather than blocking a daemon worker forever.
const SHELL_CHILD_REAP_BACKSTOP_MS: u64 = 5_000;
const SHELL_CHILD_REAP_POLL_INTERVAL_MS: u64 = 10;
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

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
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

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
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
    /// Kernel boot UUID captured by the owned remote guardian before it starts
    /// the requested command. Together with `remote_process_start_time`, this
    /// prevents a recycled numeric PID from authorizing cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_boot_id: Option<String>,
    /// `/proc/<pid>/stat` field 22, in clock ticks after boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_process_start_time: Option<String>,
    /// Per-job nonce inherited by the owned remote guardian. This is an
    /// ownership correlation token, not a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_ownership_token: Option<String>,
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
            remote_boot_id: None,
            remote_process_start_time: None,
            remote_ownership_token: None,
            remote_cleanup_error_code: None,
            remote_cleanup_message: None,
            detection_evidence: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellJobStatus {
    pub schema_version: u32,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub status: String,
    pub pid: Option<u32>,
    /// Immutable kernel process identity captured from the exact spawned child.
    /// A numeric PID alone never authorizes destructive cleanup because the OS
    /// may recycle it after the original process exits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_process_identity: Option<ActRunShellLocalProcessIdentity>,
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
    /// Evidence for a child that failed after `CreateProcess`/`spawn` returned.
    /// A post-spawn failure is terminal only when the exact child handle (and,
    /// when acquired, its kill-on-close job authority) reached a verified
    /// terminal state. Otherwise the durable record remains explicitly live
    /// while the daemon retains the exact owner for bounded retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_failure: Option<ActRunShellSpawnFailureReadback>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellLocalProcessIdentity {
    pub pid: u32,
    /// Windows: `GetProcessTimes` creation FILETIME in 100 ns ticks. Other
    /// platforms: the kernel/process-table start-time value exposed by sysinfo.
    pub start_time: u64,
    pub start_time_source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActRunShellSpawnFailureReadback {
    pub stage: String,
    pub child_created: bool,
    pub cleanup_verified: bool,
    pub exact_child_reaped: bool,
    pub exact_child_kill_error: Option<String>,
    pub exact_child_reap_timed_out: bool,
    pub exact_child_reap_poll_attempts: u64,
    pub exact_child_reap_poll_error_count: u64,
    pub exact_child_reap_last_poll_error: Option<String>,
    pub exact_child_reap_elapsed_ms: u64,
    pub process_job_acquired: bool,
    pub process_job_close: Option<String>,
    pub tree_cleanup_verified: bool,
    pub final_identity_state: Option<String>,
    pub exact_owner_retained: bool,
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

#[derive(Clone, Debug, Serialize)]
struct ShellJobStatusPersistenceFailure {
    error_code: &'static str,
    reason: &'static str,
    detail: String,
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
        #[schemars(range(min = 1, max = 4_294_967_295_u64))]
        window_hwnd: i64,
    },
    Cdp {
        #[schemars(range(min = 1, max = 4_294_967_295_u64))]
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
    if let Some(target) = &params.target {
        let window_hwnd = match target {
            ActSpawnAgentTarget::Window { window_hwnd }
            | ActSpawnAgentTarget::Cdp { window_hwnd, .. } => *window_hwnd,
        };
        crate::m1::validate_window_hwnd_shape("act_spawn_agent", window_hwnd)?;
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

fn allow_physical_mutation(_stage: &'static str) -> Result<(), ErrorData> {
    Ok(())
}

fn physical_mutation_boundary_error(
    error: ErrorData,
    stage: &'static str,
    cleanup: Value,
) -> ErrorData {
    let mut data = match error.data {
        Some(Value::Object(data)) => data,
        Some(original_data) => {
            let mut data = serde_json::Map::new();
            data.insert("original_data".to_owned(), original_data);
            data
        }
        None => serde_json::Map::new(),
    };
    data.insert("physical_mutation_boundary_stage".to_owned(), json!(stage));
    data.insert("physical_mutation_cleanup".to_owned(), cleanup);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(Value::Object(data)),
    )
}

async fn await_physical_mutation_boundary<T>(
    boundary: &PhysicalMutationBoundary<'_>,
    stage: &'static str,
    future: impl Future<Output = T>,
) -> Result<T, ErrorData> {
    boundary(stage)?;
    crate::server::operator_panic_boundary::ensure_mcp_request_not_cancelled(stage)?;
    let mut future = Box::pin(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(result),
            _ = tokio::time::sleep(PHYSICAL_MUTATION_BOUNDARY_POLL_INTERVAL) => {
                boundary(stage)?;
                crate::server::operator_panic_boundary::ensure_mcp_request_not_cancelled(stage)?;
            }
        }
    }
}

#[allow(
    dead_code,
    reason = "kept as the direct M4 combo helper for unit tests and non-server callers"
)]
pub async fn execute_combo(
    runtime: Arc<Mutex<ReflexRuntime>>,
    params: ActComboParams,
) -> Result<ActComboResponse, ErrorData> {
    execute_combo_with_boundary(runtime, params, &allow_physical_mutation).await
}

pub(crate) async fn execute_combo_with_boundary(
    runtime: Arc<Mutex<ReflexRuntime>>,
    params: ActComboParams,
    boundary: &PhysicalMutationBoundary<'_>,
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
    boundary("act_combo_immediately_before_reflex_schedule")?;
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
    run_authorized_shell_with_boundary(
        params,
        authorization,
        inline_await_limit_ms,
        context,
        &allow_physical_mutation,
    )
    .await
}

pub(crate) async fn run_authorized_shell_with_boundary(
    params: ActRunShellParams,
    authorization: &RunShellAuthorization,
    inline_await_limit_ms: u64,
    context: Option<&ShellExecutionContext>,
    boundary: &PhysicalMutationBoundary<'_>,
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
        let started_job = start_authorized_shell_job_with_boundary(
            start_params,
            authorization,
            context,
            boundary,
        )?;
        act_run_shell_background_response(
            started_job.job,
            elapsed_ms_u32(started),
            background_reason,
            inline_await_limit_ms,
            requested_execution_mode,
        )
    } else {
        run_allowlisted_shell_with_boundary(params, inline_await_limit_ms, context, boundary)
            .await?
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
    if let Some(background_reason) = direct_shell_background_reason(params, inline_await_limit_ms) {
        reject_new_durable_ssh_promotion(
            &params.command,
            &params.args,
            Some(background_reason),
            None,
        )?;
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

#[allow(
    dead_code,
    reason = "kept as the direct M4 durable-shell helper for unit tests and non-server callers"
)]
pub fn start_authorized_shell_job(
    params: ActRunShellStartParams,
    authorization: &RunShellAuthorization,
    context: Option<&ShellExecutionContext>,
) -> Result<ActRunShellStartResponse, ErrorData> {
    start_authorized_shell_job_with_boundary(
        params,
        authorization,
        context,
        &allow_physical_mutation,
    )
}

pub(crate) fn start_authorized_shell_job_with_boundary(
    params: ActRunShellStartParams,
    authorization: &RunShellAuthorization,
    context: Option<&ShellExecutionContext>,
    boundary: &PhysicalMutationBoundary<'_>,
) -> Result<ActRunShellStartResponse, ErrorData> {
    let _ = unresolved_shell_child_owner_report();
    // This gate deliberately precedes request hashing and job-directory
    // creation. A refused SSH durable promotion must not spawn a local/remote
    // process or leave a synthetic tracked-job artifact that recovery could
    // later mistake for acquired ownership.
    reject_new_durable_ssh_promotion(
        &params.command,
        &params.args,
        Some("explicit_act_run_shell_start"),
        params.job_id.as_deref(),
    )?;
    let started = Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339();
    let request_sha256 = run_shell_start_request_sha256(&params)?;
    let (job_id, paths) = create_shell_job_paths(params.job_id.as_deref())?;
    write_shell_job_request(&paths, &params, &request_sha256, context)?;
    // Prepare the tracked argv and its recovery sidecar once. They are one
    // safety object: spawning a guardian without the exact replayable sidecar
    // would create a remote process that cancel/startup recovery cannot own.
    let spawn_plan = match shell_job_spawn_plan(&params, &job_id) {
        Ok(plan) => plan,
        Err(error) => {
            let mut status = shell_job_status_record(
                &job_id,
                "spawn_refused",
                &params,
                &paths,
                &request_sha256,
                authorization,
                started_at,
                None,
                context,
            );
            mark_shell_job_remote_pre_marker_terminal(
                &mut status,
                "act_run_shell_start_preflight",
                "spawn_refused",
                RemotePreMarkerTerminalEvidence {
                    reason: "tracking_preflight_refused_before_spawn",
                    pattern: "no_child_process_created",
                },
            );
            // Preflight runs before output files or a child exist, so this is
            // stronger than stderr-based pre-marker inference: no remote
            // process could have started even when the original argv was too
            // unsafe for the ordinary tracking-pending classifier.
            status.remote_process_scope.remote_cleanup_required = false;
            status.remote_process_scope.remote_cleanup_verified = false;
            status.remote_process_scope.remote_cleanup_status =
                SHELL_REMOTE_CLEANUP_PRE_MARKER_TERMINAL.to_owned();
            status.remote_process_scope.remote_cleanup_error_code = None;
            status.remote_process_scope.remote_cleanup_message = Some(
                "SSH tracking preflight refused the prepared plan before any child process was created"
                    .to_owned(),
            );
            push_unique_evidence(
                &mut status.remote_process_scope.detection_evidence,
                "remote_tracking_pre_marker_terminal:tracking_preflight_refused_before_spawn"
                    .to_owned(),
            );
            status.completed_at = Some(chrono::Utc::now().to_rfc3339());
            status.duration_ms =
                Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            status.error_code = Some(extract_error_code(&error));
            status.error_message = Some(error.message.to_string());
            let durable_state_failure = match write_shell_job_status(&paths.status_path, &status) {
                Ok(()) => match read_shell_job_status(&paths.status_path, &job_id) {
                    Ok(readback)
                        if readback.status == "spawn_refused"
                            && readback.pid.is_none()
                            && readback.error_code == status.error_code =>
                    {
                        tracing::info!(
                            code = "M4_ACT_RUN_SHELL_PREFLIGHT_REFUSAL_PERSISTED",
                            job_id,
                            status_path = %paths.status_path.display(),
                            "persisted and independently read back SSH tracking preflight refusal"
                        );
                        None
                    }
                    Ok(readback) => Some((
                        error_codes::STORAGE_READ_FAILED,
                        "spawn_refused_readback_mismatch",
                        format!(
                            "expected status=spawn_refused pid=None error_code={:?}; actual status={} pid={:?} error_code={:?}",
                            status.error_code, readback.status, readback.pid, readback.error_code
                        ),
                    )),
                    Err(read_error) => Some((
                        error_codes::STORAGE_READ_FAILED,
                        "spawn_refused_readback_failed",
                        format!("{read_error:?}"),
                    )),
                },
                Err(write_error) => Some((
                    error_codes::STORAGE_WRITE_FAILED,
                    "spawn_refused_write_failed",
                    format!("{write_error:?}"),
                )),
            };
            if let Some((durable_error_code, reason, durable_detail)) = durable_state_failure {
                tracing::error!(
                    code = "M4_ACT_RUN_SHELL_PREFLIGHT_REFUSAL_DURABLE_STATE_FAILED",
                    job_id,
                    reason,
                    durable_detail,
                    policy_error = %error.message,
                    status_path = %paths.status_path.display(),
                    "SSH tracking preflight refusal durable state could not be verified"
                );
                return Err(shell_tool_error(
                    durable_error_code,
                    format!(
                        "act_run_shell_start refused unsafe SSH tracking ({}) but could not verify its durable spawn_refused status: {durable_detail}",
                        error.message
                    ),
                    json!({
                        "code": durable_error_code,
                        "job_id": job_id,
                        "status_path": paths.status_path,
                        "reason": reason,
                        "durable_detail": durable_detail,
                        "policy_error_code": extract_error_code(&error),
                        "policy_error_message": error.message,
                    }),
                ));
            }
            return Err(error);
        }
    };
    write_shell_remote_cleanup_invocation(&paths, spawn_plan.remote_cleanup_invocation.as_ref())?;

    let stdout_file = open_shell_job_output(&paths.stdout_path, "stdout", &job_id)?;
    let stderr_file = open_shell_job_output(&paths.stderr_path, "stderr", &job_id)?;
    boundary("act_run_shell_start_immediately_before_create_process")?;
    let spawned = match spawn_shell_job_child(
        &params,
        &spawn_plan,
        stdout_file,
        stderr_file,
        context,
    ) {
        Ok(spawned) => spawned,
        Err(SpawnShellJobChildFailure::BeforeSpawn(error)) => {
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
            if let Err(durable_failure) =
                persist_and_verify_shell_job_status(&paths.status_path, &status)
            {
                let spawn_error_code = extract_error_code(&error);
                let spawn_error_message = error.message.to_string();
                tracing::error!(
                    code = "M4_ACT_RUN_SHELL_JOB_STATUS_UNVERIFIED_AFTER_SPAWN_FAILURE",
                    job_id = %job_id,
                    status_path = %paths.status_path.display(),
                    durable_error_code = durable_failure.error_code,
                    durable_reason = durable_failure.reason,
                    durable_detail = durable_failure.detail,
                    spawn_error_code,
                    spawn_error_message,
                    "act_run_shell_start could not verify the durable spawn failure status"
                );
                return Err(shell_tool_error(
                    durable_failure.error_code,
                    format!(
                        "act_run_shell_start failed to spawn ({spawn_error_message}) and could not verify its durable spawn_failed status: {}",
                        durable_failure.detail
                    ),
                    json!({
                        "code": durable_failure.error_code,
                        "job_id": job_id,
                        "status_path": paths.status_path,
                        "reason": durable_failure.reason,
                        "durable_detail": durable_failure.detail,
                        "spawn_error_code": spawn_error_code,
                        "spawn_error_message": spawn_error_message,
                    }),
                ));
            }
            tracing::info!(
                code = "M4_ACT_RUN_SHELL_JOB_SPAWN_FAILURE_PERSISTED",
                job_id = %job_id,
                status_path = %paths.status_path.display(),
                "persisted and independently read back the complete spawn_failed status"
            );
            return Err(error);
        }
        Err(SpawnShellJobChildFailure::AfterSpawn(failure)) => {
            let PostSpawnShellJobChildFailure {
                error,
                child,
                process_job,
                pid,
                local_process_identity,
                mut readback,
            } = *failure;
            let cleanup_verified = readback.cleanup_verified;
            let spawn_error_code = extract_error_code(&error);
            let spawn_error_message = error.message.to_string();
            let original_error_data = error.data;
            let mut status = shell_job_status_record(
                &job_id,
                if cleanup_verified {
                    "spawn_failed_reaped"
                } else {
                    SHELL_JOB_STATUS_SPAWN_CLEANUP_UNVERIFIED
                },
                &params,
                &paths,
                &request_sha256,
                authorization,
                started_at,
                pid,
                context,
            );
            status.local_process_identity = local_process_identity.clone();
            status.error_code = Some(spawn_error_code.clone());
            status.error_message = Some(format!(
                "post-spawn failure at stage={}; cleanup_verified={cleanup_verified}; {spawn_error_message}",
                readback.stage
            ));
            if cleanup_verified {
                status.completed_at = Some(chrono::Utc::now().to_rfc3339());
                status.duration_ms =
                    Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            } else {
                readback.exact_owner_retained = true;
            }
            status.spawn_failure = Some(readback.clone());

            let pending_owner = if cleanup_verified {
                None
            } else {
                let process_job_acquired = readback.process_job_acquired;
                let process_job_close_verified = !process_job_acquired
                    || readback.process_job_close.as_deref() == Some("Ok(())");
                let owner = RetainedShellChildOwner {
                    owner_id: new_reflex_id(),
                    pid,
                    stage: readback.stage.clone(),
                    child: RetainedExactShellChild::Tokio(Box::new(child)),
                    process_job,
                    process_job_acquired,
                    process_job_close_verified,
                    tree_cleanup_verified: readback.tree_cleanup_verified,
                    local_process_identity,
                    durable_spawn_failure: Some(RetainedDurableSpawnFailure {
                        status_path: paths.status_path.clone(),
                        status: status.clone(),
                    }),
                };
                Some(owner)
            };
            let durable_failure =
                persist_and_verify_shell_job_status(&paths.status_path, &status).err();
            // Publish the owner to the process-lifetime registry only after the
            // unresolved durable record has completed its first commit attempt.
            // This prevents a concurrent retry from writing the terminal state
            // and then being overwritten by this caller's older unresolved one.
            let retained_owner = pending_owner.map(retain_unresolved_shell_child_owner);
            tracing::error!(
                code = "M4_ACT_RUN_SHELL_POST_SPAWN_FAILURE_PERSISTED",
                job_id = %job_id,
                pid = ?pid,
                stage = %readback.stage,
                cleanup_verified,
                exact_owner_retained = readback.exact_owner_retained,
                retained_owner = ?retained_owner,
                durable_failure = ?durable_failure,
                status_path = %paths.status_path.display(),
                "post-spawn failure preserved exact cleanup ownership and durable truth"
            );
            let response_code = durable_failure
                .as_ref()
                .map_or(error_codes::TOOL_INTERNAL_ERROR, |failure| {
                    failure.error_code
                });
            return Err(shell_tool_error(
                response_code,
                if let Some(failure) = durable_failure.as_ref() {
                    format!(
                        "{spawn_error_message}; post-spawn cleanup_verified={cleanup_verified}, but durable status verification also failed: {}",
                        failure.detail
                    )
                } else {
                    format!(
                        "{spawn_error_message}; post-spawn cleanup_verified={cleanup_verified}; exact_owner_retained={}",
                        readback.exact_owner_retained
                    )
                },
                json!({
                    "code": response_code,
                    "job_id": job_id,
                    "pid": pid,
                    "status_path": paths.status_path,
                    "reason": "post_spawn_failure",
                    "spawn_error_code": spawn_error_code,
                    "spawn_error_message": spawn_error_message,
                    "spawn_error_data": original_error_data,
                    "spawn_failure": readback,
                    "retained_owner": retained_owner,
                    "durable_status_failure": durable_failure,
                }),
            ));
        }
    };
    let mut child = spawned.child;
    let mut process_job = spawned.process_job;
    let local_process_identity = spawned.local_process_identity;

    let Some(pid) = child.id() else {
        // Keep the kill-on-close job authority alive while first terminating
        // and polling the exact child handle. If that direct path cannot prove
        // reaping, close the job object (which terminates its owned tree on
        // Windows) and perform one final bounded exact-handle readback.
        let initial_cleanup = terminate_and_reap_tokio_child_bounded(
            &mut child,
            Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
        );
        let job_close = process_job.close_checked();
        let state_after_job_close = local_process_identity_state(&local_process_identity);
        let post_job_close_cleanup = if initial_cleanup.reaped
            && !matches!(
                state_after_job_close,
                LocalProcessIdentityState::Match | LocalProcessIdentityState::Unreadable(_)
            ) {
            None
        } else {
            Some(terminate_and_reap_tokio_child_bounded(
                &mut child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            ))
        };
        let final_identity_state = local_process_identity_state(&local_process_identity);
        let cleanup_verified = (initial_cleanup.reaped
            || post_job_close_cleanup
                .as_ref()
                .is_some_and(|readback| readback.reaped))
            && job_close.is_ok()
            && matches!(
                final_identity_state,
                LocalProcessIdentityState::Exited
                    | LocalProcessIdentityState::Absent
                    | LocalProcessIdentityState::Mismatch(_)
            );
        let mut status = shell_job_status_record(
            &job_id,
            if cleanup_verified {
                "pid_unavailable_reaped"
            } else {
                "pid_unavailable_cleanup_unverified"
            },
            &params,
            &paths,
            &request_sha256,
            authorization,
            started_at,
            None,
            context,
        );
        seed_shell_job_remote_ownership(&mut status, spawn_plan.remote_cleanup_invocation.as_ref());
        status.completed_at = Some(chrono::Utc::now().to_rfc3339());
        status.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        status.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
        status.error_message = Some(format!(
            "spawned process id was unavailable; exact child cleanup verified={cleanup_verified}; initial_cleanup={initial_cleanup:?}; job_close={job_close:?}; state_after_job_close={state_after_job_close:?}; post_job_close_cleanup={post_job_close_cleanup:?}; final_identity_state={final_identity_state:?}"
        ));
        let durable_failure =
            persist_and_verify_shell_job_status(&paths.status_path, &status).err();
        if let Some(failure) = durable_failure.as_ref() {
            tracing::error!(
                code = "M4_ACT_RUN_SHELL_JOB_PID_UNAVAILABLE_DURABLE_STATE_FAILED",
                job_id = %job_id,
                status_path = %paths.status_path.display(),
                cleanup_verified,
                initial_cleanup = ?initial_cleanup,
                job_close = ?job_close,
                state_after_job_close = ?state_after_job_close,
                post_job_close_cleanup = ?post_job_close_cleanup,
                final_identity_state = ?final_identity_state,
                durable_error_code = failure.error_code,
                durable_reason = failure.reason,
                durable_detail = failure.detail,
                "pid-unavailable child cleanup status could not be durably verified"
            );
        }
        let response_code = durable_failure
            .as_ref()
            .map_or(error_codes::TOOL_INTERNAL_ERROR, |failure| {
                failure.error_code
            });
        return Err(shell_tool_error(
            response_code,
            if cleanup_verified {
                "act_run_shell_start spawned a child process without an observable pid; the exact child handle was terminated and reaped"
            } else {
                "act_run_shell_start spawned a child process without an observable pid and could not verify exact-child reaping before the cleanup backstop"
            },
            json!({
                "code": response_code,
                "job_id": job_id,
                "reason": "pid_unavailable",
                "status_path": paths.status_path,
                "cleanup_verified": cleanup_verified,
                "initial_cleanup": initial_cleanup,
                "job_close": format!("{job_close:?}"),
                "state_after_job_close": state_after_job_close,
                "post_job_close_cleanup": post_job_close_cleanup,
                "final_identity_state": final_identity_state,
                "durable_status_failure": durable_failure,
            }),
        ));
    };

    let mut status = shell_job_status_record(
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
    seed_shell_job_remote_ownership(&mut status, spawn_plan.remote_cleanup_invocation.as_ref());
    status.local_process_identity = Some(local_process_identity.clone());
    persist_running_shell_job_status_or_cleanup(
        &paths,
        &mut status,
        &mut child,
        &local_process_identity,
        &mut process_job,
        started,
    )?;

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
    let _ = unresolved_shell_child_owner_report();
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
    if !shell_job_live_status(&job.status) {
        return false;
    }
    let Some(pid) = job.pid else {
        return false;
    };
    let Some(identity) = job.local_process_identity.as_ref() else {
        // Legacy status records predate immutable creation identity.
        return shell_job_live_process_ids(&[pid]).contains(&pid);
    };
    if identity.pid != pid {
        // Malformed identity is uncertainty, never proof of absence.
        return true;
    }
    matches!(
        local_process_identity_state(identity),
        LocalProcessIdentityState::Match | LocalProcessIdentityState::Unreadable(_)
    )
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
    let _ = unresolved_shell_child_owner_report();
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
        if job.status != SHELL_JOB_STATUS_SPAWN_CLEANUP_UNVERIFIED {
            job.status = "cancel_requested".to_owned();
        }
        let _ = wait_for_shell_job_remote_metadata(
            &mut job,
            &paths,
            Duration::from_millis(SHELL_REMOTE_METADATA_WAIT_MS),
        )?;
        write_shell_job_status(&paths.status_path, &job)?;
        if job.pid.is_some() {
            let termination = terminate_shell_job_from_status(&job);
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

const SHELL_JOB_QUARANTINE_MANIFEST_SCHEMA_VERSION: u32 = 2;
const SHELL_JOB_RECOVERY_ID_SAMPLE_CAP: usize = 64;

/// Startup-only evidence-preserving disposition for a durable shell-job
/// directory whose `status.json` cannot be decoded. The ordinary reaper and
/// live session cleanup deliberately never invoke this path: only daemon
/// startup, after the canonical shell-job-store lifetime lock proves exclusive
/// ownership of this exact frozen root, has the boundary required to classify
/// an unreadable row as abandoned.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ShellJobCorruptRecoveryReadback {
    pub job_root: Option<String>,
    pub quarantine_root: Option<String>,
    pub scanned_job_dirs: usize,
    pub retained_valid_status_jobs: usize,
    pub corrupt_status_jobs: usize,
    pub quarantined_jobs: usize,
    pub remote_state_verified_jobs: usize,
    pub retained_unverifiable_remote_jobs: usize,
    pub unexpected_job_root_entries: usize,
    pub skipped_concurrently_mutated: usize,
    pub recovery_failures: usize,
    pub bytes_quarantined: u64,
    pub quarantined_job_ids_sample: Vec<String>,
    pub retained_job_ids_sample: Vec<String>,
    pub unexpected_job_root_entries_sample: Vec<String>,
    pub quarantine_paths_sample: Vec<String>,
    pub manifest_paths_sample: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ShellJobQuarantineArtifact {
    relative_path: String,
    byte_len: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ShellJobRemoteCommandEvidence {
    operation: String,
    exit_code: Option<i32>,
    stdout_byte_len: u64,
    stdout_sha256: String,
    stderr_byte_len: u64,
    stderr_sha256: String,
    parsed_status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ShellJobQuarantineRemoteVerification {
    sidecar_present: bool,
    process_marker_present: bool,
    remote_identity_sha256: Option<String>,
    remote_pid: Option<String>,
    remote_pgid: Option<String>,
    liveness_before: Option<ShellJobRemoteCommandEvidence>,
    cleanup: Option<ShellJobRemoteCommandEvidence>,
    liveness_after: Option<ShellJobRemoteCommandEvidence>,
    #[serde(default)]
    recovery_intent_sha256: Option<String>,
    #[serde(default)]
    recovery_outcome_sha256: Option<String>,
    verdict: String,
}

/// Immutable authorization record committed before corrupt-job recovery is
/// allowed to signal a live remote guardian. A restart reuses this exact
/// `recovery_id`; it never creates a second ambiguous authorization record.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ShellJobRemoteRecoveryIntent {
    schema_version: u32,
    recovery_id: String,
    job_id: String,
    created_at: String,
    quarantine_job_dir: String,
    remote_identity_sha256: String,
    remote_pid: String,
    remote_pgid: String,
    remote_boot_id: String,
    remote_process_start_time: String,
    remote_ownership_token_sha256: String,
    cleanup_sidecar_sha256: String,
    cleanup_sidecar_schema_version: u32,
    reason: String,
}

/// Immutable terminal evidence linked to the pre-signal intent by digest.
/// Keeping intent and outcome separate makes their ordering independently
/// auditable after a crash.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ShellJobRemoteRecoveryOutcome {
    schema_version: u32,
    recovery_id: String,
    job_id: String,
    completed_at: String,
    intent_sha256: String,
    cleanup: Option<ShellJobRemoteCommandEvidence>,
    liveness_after: ShellJobRemoteCommandEvidence,
    verdict: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ShellJobQuarantineManifest {
    schema_version: u32,
    recovery_id: String,
    job_id: String,
    quarantined_at: String,
    reason: String,
    startup_safety_boundary: String,
    source_job_dir: String,
    quarantine_job_dir: String,
    status_read_error: String,
    original_artifact_count: usize,
    original_artifact_bytes: u64,
    /// Operator/job-produced artifacts, excluding recovery intent/outcome
    /// records generated by this state machine.
    #[serde(default)]
    pre_recovery_artifact_count: usize,
    #[serde(default)]
    pre_recovery_artifact_bytes: u64,
    #[serde(default)]
    recovery_generated_artifact_count: usize,
    #[serde(default)]
    recovery_generated_artifact_bytes: u64,
    artifacts: Vec<ShellJobQuarantineArtifact>,
    remote_verification: ShellJobQuarantineRemoteVerification,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ShellJobQuarantineCompletion {
    schema_version: u32,
    recovery_id: String,
    job_id: String,
    completed_at: String,
    quarantine_job_dir: String,
    manifest_file_name: String,
    manifest_sha256: String,
    original_artifact_count: usize,
    original_artifact_bytes: u64,
    #[serde(default)]
    pre_recovery_artifact_count: usize,
    #[serde(default)]
    pre_recovery_artifact_bytes: u64,
    #[serde(default)]
    recovery_generated_artifact_count: usize,
    #[serde(default)]
    recovery_generated_artifact_bytes: u64,
    remote_verdict: String,
}

fn shell_job_quarantine_root_dir() -> Result<PathBuf, ErrorData> {
    Ok(shell_job_root_dir()?.join("quarantine"))
}

fn shell_job_quarantine_artifacts(
    job_dir: &Path,
) -> Result<Vec<ShellJobQuarantineArtifact>, String> {
    let entries = fs::read_dir(job_dir).map_err(|error| {
        format!(
            "failed to enumerate corrupt job artifacts under {}: {error}",
            job_dir.display()
        )
    })?;
    let mut artifacts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read a corrupt job artifact entry under {}: {error}",
                job_dir.display()
            )
        })?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to classify corrupt job artifact {}: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            return Err(format!(
                "corrupt job artifact {} is not a regular file; refusing an unverifiable quarantine move",
                entry.path().display()
            ));
        }
        let relative_path = entry.file_name().into_string().map_err(|name| {
            format!(
                "corrupt job artifact name is not UTF-8: {:?}",
                name.to_string_lossy()
            )
        })?;
        let bytes = fs::read(entry.path()).map_err(|error| {
            format!(
                "failed to read corrupt job artifact {} before quarantine: {error}",
                entry.path().display()
            )
        })?;
        let byte_len = u64::try_from(bytes.len()).map_err(|error| {
            format!(
                "corrupt job artifact length cannot be represented at {}: {error}",
                entry.path().display()
            )
        })?;
        artifacts.push(ShellJobQuarantineArtifact {
            relative_path,
            byte_len,
            sha256: sha256_hex(&bytes),
        });
    }
    artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(artifacts)
}

fn shell_job_quarantine_artifact_accounting(
    artifacts: &[ShellJobQuarantineArtifact],
) -> Result<(usize, u64, usize, u64), String> {
    let mut pre_count = 0usize;
    let mut pre_bytes = 0u64;
    let mut generated_count = 0usize;
    let mut generated_bytes = 0u64;
    for artifact in artifacts {
        let recovery_generated = artifact
            .relative_path
            .starts_with("remote-recovery-intent-")
            || artifact
                .relative_path
                .starts_with("remote-recovery-outcome-");
        if recovery_generated {
            generated_count = generated_count.checked_add(1).ok_or_else(|| {
                format!(
                    "recovery-generated artifact count overflow while accounting {:?}",
                    artifact.relative_path
                )
            })?;
            generated_bytes = generated_bytes
                .checked_add(artifact.byte_len)
                .ok_or_else(|| {
                    format!(
                        "recovery-generated artifact byte total overflow while accounting {:?}: before={generated_bytes} add={}",
                        artifact.relative_path, artifact.byte_len
                    )
                })?;
        } else {
            pre_count = pre_count.checked_add(1).ok_or_else(|| {
                format!(
                    "pre-recovery artifact count overflow while accounting {:?}",
                    artifact.relative_path
                )
            })?;
            pre_bytes = pre_bytes.checked_add(artifact.byte_len).ok_or_else(|| {
                format!(
                    "pre-recovery artifact byte total overflow while accounting {:?}: before={pre_bytes} add={}",
                    artifact.relative_path, artifact.byte_len
                )
            })?;
        }
    }
    Ok((pre_count, pre_bytes, generated_count, generated_bytes))
}

fn shell_job_remote_command_evidence(
    operation: &str,
    readback: &CleanupCommandReadback,
    parsed_status: &str,
) -> ShellJobRemoteCommandEvidence {
    ShellJobRemoteCommandEvidence {
        operation: operation.to_owned(),
        exit_code: readback.exit_code,
        stdout_byte_len: readback.stdout_byte_len,
        stdout_sha256: readback.stdout_sha256.clone(),
        stderr_byte_len: readback.stderr_byte_len,
        stderr_sha256: readback.stderr_sha256.clone(),
        parsed_status: parsed_status.to_owned(),
    }
}

fn run_corrupt_job_remote_liveness_probe(
    invocation: &ShellRemoteCleanupInvocation,
    pid: &str,
    pgid: &str,
    operation: &str,
) -> Result<(CleanupCommandReadback, String), String> {
    run_remote_liveness_probe(
        &invocation.command,
        &invocation.control_args,
        pid,
        pgid,
        operation,
    )
}

fn run_remote_liveness_probe(
    command: &str,
    invocation_args: &[String],
    pid: &str,
    pgid: &str,
    operation: &str,
) -> Result<(CleanupCommandReadback, String), String> {
    let mut args = hardened_ssh_automatic_replay_args(invocation_args)?;
    args.push(ssh_remote_liveness_command(pid, pgid));
    let readback = run_shell_cleanup_command_with_timeout(
        command,
        &args,
        Duration::from_millis(SHELL_REMOTE_LIVENESS_TIMEOUT_MS),
    )?;
    let status = parse_remote_liveness_status(&readback.stdout, pid, pgid).ok_or_else(|| {
        format!(
            "{operation} returned no valid {SHELL_REMOTE_LIVENESS_MARKER}; exit={:?}; stdout_sha256={}; stderr_sha256={}",
            readback.exit_code,
            readback.stdout_sha256,
            readback.stderr_sha256
        )
    })?;
    if readback.exit_code != Some(0) {
        return Err(format!(
            "{operation} returned status={status} with nonzero/unknown exit={:?}; stdout_sha256={}; stderr_sha256={}",
            readback.exit_code, readback.stdout_sha256, readback.stderr_sha256
        ));
    }
    Ok((readback, status))
}

fn shell_job_recovery_record_paths(job_dir: &Path, prefix: &str) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(job_dir).map_err(|error| {
        format!(
            "failed to enumerate recovery records under {}: {error}",
            job_dir.display()
        )
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read recovery record entry under {}: {error}",
                job_dir.display()
            )
        })?;
        let name = entry.file_name().into_string().map_err(|name| {
            format!(
                "recovery record name is not UTF-8 under {}: {}",
                job_dir.display(),
                name.to_string_lossy()
            )
        })?;
        if name.starts_with(prefix)
            && std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            let file_type = entry.file_type().map_err(|error| {
                format!(
                    "failed to classify recovery record {}: {error}",
                    entry.path().display()
                )
            })?;
            if !file_type.is_file() {
                return Err(format!(
                    "recovery record path is not a regular file: {}",
                    entry.path().display()
                ));
            }
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn validate_recovery_sha256(value: &str, field: &str, job_id: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .chars()
            .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
    {
        Ok(())
    } else {
        Err(format!(
            "remote recovery {field} is not a lowercase SHA-256 digest for {job_id}"
        ))
    }
}

fn validate_remote_command_evidence_digests(
    evidence: &ShellJobRemoteCommandEvidence,
    role: &str,
    job_id: &str,
) -> Result<(), String> {
    validate_recovery_sha256(
        &evidence.stdout_sha256,
        &format!("{role}_stdout_sha256"),
        job_id,
    )?;
    validate_recovery_sha256(
        &evidence.stderr_sha256,
        &format!("{role}_stderr_sha256"),
        job_id,
    )?;
    for (stream, byte_len, digest) in [
        ("stdout", evidence.stdout_byte_len, &evidence.stdout_sha256),
        ("stderr", evidence.stderr_byte_len, &evidence.stderr_sha256),
    ] {
        if byte_len > SHELL_CLEANUP_CAPTURE_CAP_BYTES {
            return Err(format!(
                "remote recovery {role}_{stream}_byte_len exceeds the physical capture cap for {job_id}: recorded={byte_len} cap={SHELL_CLEANUP_CAPTURE_CAP_BYTES}"
            ));
        }
        let digest_is_empty = digest == &sha256_hex(b"");
        if (byte_len == 0) != digest_is_empty {
            return Err(format!(
                "remote recovery {role}_{stream} length/digest emptiness differs for {job_id}: byte_len={byte_len} sha256={digest}"
            ));
        }
    }
    Ok(())
}

fn read_existing_remote_recovery_intent(
    paths: &ShellJobPaths,
    job_id: &str,
) -> Result<Option<(ShellJobRemoteRecoveryIntent, String)>, String> {
    let records = shell_job_recovery_record_paths(&paths.job_dir, "remote-recovery-intent-")?;
    if records.len() > 1 {
        return Err(format!(
            "corrupt job {job_id} has {} remote recovery intents; exactly zero or one is allowed",
            records.len()
        ));
    }
    let Some(path) = records.first() else {
        return Ok(None);
    };
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read remote recovery intent for {job_id}: {error}"))?;
    let intent: ShellJobRemoteRecoveryIntent = serde_json::from_slice(&bytes).map_err(|error| {
        format!("failed to decode remote recovery intent for {job_id}: {error}")
    })?;
    if intent.schema_version != 1 || intent.job_id != job_id {
        return Err(format!(
            "remote recovery intent schema/job identity differs for {job_id}"
        ));
    }
    let expected_name = format!("remote-recovery-intent-{}.json", intent.recovery_id);
    if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return Err(format!(
            "remote recovery intent filename differs from recovery_id for {job_id}"
        ));
    }
    if !valid_remote_process_number(&intent.remote_pid)
        || !valid_remote_process_number(&intent.remote_pgid)
        || !valid_remote_boot_id(&intent.remote_boot_id)
        || !valid_remote_process_start_time(&intent.remote_process_start_time)
    {
        return Err(format!(
            "remote recovery intent contains malformed process identity for {job_id}"
        ));
    }
    validate_recovery_sha256(
        &intent.remote_identity_sha256,
        "remote_identity_sha256",
        job_id,
    )?;
    validate_recovery_sha256(
        &intent.remote_ownership_token_sha256,
        "remote_ownership_token_sha256",
        job_id,
    )?;
    validate_recovery_sha256(
        &intent.cleanup_sidecar_sha256,
        "cleanup_sidecar_sha256",
        job_id,
    )?;
    Ok(Some((intent, sha256_hex(&bytes))))
}

fn read_existing_remote_recovery_outcome(
    paths: &ShellJobPaths,
    job_id: &str,
) -> Result<Option<(ShellJobRemoteRecoveryOutcome, String)>, String> {
    let records = shell_job_recovery_record_paths(&paths.job_dir, "remote-recovery-outcome-")?;
    if records.len() > 1 {
        return Err(format!(
            "corrupt job {job_id} has {} remote recovery outcomes; exactly zero or one is allowed",
            records.len()
        ));
    }
    let Some(path) = records.first() else {
        return Ok(None);
    };
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read remote recovery outcome for {job_id}: {error}"))?;
    let outcome: ShellJobRemoteRecoveryOutcome =
        serde_json::from_slice(&bytes).map_err(|error| {
            format!("failed to decode remote recovery outcome for {job_id}: {error}")
        })?;
    if outcome.schema_version != 1 || outcome.job_id != job_id {
        return Err(format!(
            "remote recovery outcome schema/job identity differs for {job_id}"
        ));
    }
    let expected_name = format!("remote-recovery-outcome-{}.json", outcome.recovery_id);
    if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return Err(format!(
            "remote recovery outcome filename differs from recovery_id for {job_id}"
        ));
    }
    validate_recovery_sha256(&outcome.intent_sha256, "intent_sha256", job_id)?;
    validate_remote_recovery_outcome_semantics(&outcome, job_id)?;
    Ok(Some((outcome, sha256_hex(&bytes))))
}

fn validate_remote_recovery_outcome_semantics(
    outcome: &ShellJobRemoteRecoveryOutcome,
    job_id: &str,
) -> Result<(), String> {
    validate_remote_command_evidence_digests(
        &outcome.liveness_after,
        "outcome_liveness_after",
        job_id,
    )?;
    if let Some(cleanup) = outcome.cleanup.as_ref() {
        validate_remote_command_evidence_digests(cleanup, "outcome_cleanup", job_id)?;
    }
    let terminal_after = outcome.liveness_after.exit_code == Some(0)
        && outcome.liveness_after.parsed_status == "already_gone";
    let cleanup_valid = outcome.cleanup.as_ref().is_some_and(|cleanup| {
        cleanup.operation == "identity_bound_cleanup"
            && cleanup.exit_code == Some(0)
            && matches!(
                cleanup.parsed_status.as_str(),
                "terminated" | "already_gone"
            )
    });
    let valid = match outcome.verdict.as_str() {
        "remote_identity_bound_cleanup_verified" => {
            cleanup_valid && terminal_after && outcome.liveness_after.operation == "liveness_after"
        }
        "remote_already_gone_after_durable_cleanup_intent" => {
            outcome.cleanup.is_none()
                && terminal_after
                && outcome.liveness_after.operation == "resume_liveness_after"
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(format!(
            "remote recovery outcome has an impossible verdict/evidence shape for {job_id}: verdict={}",
            outcome.verdict
        ))
    }
}

fn persist_immutable_shell_recovery_record<T>(
    path: &Path,
    job_id: &str,
    role: &str,
    value: &T,
) -> Result<String, String>
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq,
{
    if path.try_exists().map_err(|error| {
        format!(
            "failed to inspect {role} existence at {}: {error}",
            path.display()
        )
    })? {
        return Err(format!(
            "refusing to overwrite immutable {role} at {}",
            path.display()
        ));
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("failed to encode {role} for {job_id}: {error}"))?;
    let tmp_path = shell_status_temp_path(path);
    if let Err(error) = write_shell_job_status_staging(&tmp_path, &bytes) {
        let staging_cleanup = cleanup_failed_atomic_staging_file(&tmp_path);
        return Err(format!(
            "failed to durably stage {role} at {}: {error}; {staging_cleanup}",
            path.display(),
        ));
    }
    if let Err(error) = commit_shell_job_status_file(&tmp_path, path, job_id) {
        let staging_cleanup = cleanup_failed_atomic_staging_file(&tmp_path);
        return Err(format!(
            "failed to atomically commit {role} at {}: {error}; {staging_cleanup}",
            path.display(),
        ));
    }
    let persisted = fs::read(path)
        .map_err(|error| format!("failed to read back {role} at {}: {error}", path.display()))?;
    if persisted != bytes {
        return Err(format!(
            "{role} byte readback differs at {}: expected_sha256={} actual_sha256={}",
            path.display(),
            sha256_hex(&bytes),
            sha256_hex(&persisted)
        ));
    }
    let decoded: T = serde_json::from_slice(&persisted)
        .map_err(|error| format!("failed to decode {role} readback for {job_id}: {error}"))?;
    if &decoded != value {
        return Err(format!(
            "{role} structured readback differs at {}",
            path.display()
        ));
    }
    Ok(sha256_hex(&persisted))
}

fn persist_remote_recovery_intent(
    paths: &ShellJobPaths,
    intent: &ShellJobRemoteRecoveryIntent,
) -> Result<String, String> {
    let path = paths.job_dir.join(format!(
        "remote-recovery-intent-{}.json",
        intent.recovery_id
    ));
    persist_immutable_shell_recovery_record(&path, &intent.job_id, "remote recovery intent", intent)
}

fn persist_remote_recovery_outcome(
    paths: &ShellJobPaths,
    outcome: &ShellJobRemoteRecoveryOutcome,
) -> Result<String, String> {
    let path = paths.job_dir.join(format!(
        "remote-recovery-outcome-{}.json",
        outcome.recovery_id
    ));
    persist_immutable_shell_recovery_record(
        &path,
        &outcome.job_id,
        "remote recovery outcome",
        outcome,
    )
}

fn run_corrupt_job_identity_bound_remote_cleanup(
    invocation: &ShellRemoteCleanupInvocation,
    pid: &str,
    pgid: &str,
    identity: &RemoteProcessOwnershipIdentity,
) -> Result<(CleanupCommandReadback, String), String> {
    let mut args = hardened_ssh_automatic_replay_args(&invocation.control_args)?;
    args.push(ssh_remote_cleanup_command(pid, pgid, identity));
    let readback = run_shell_cleanup_command_with_timeout(
        &invocation.command,
        &args,
        Duration::from_millis(SHELL_REMOTE_CLEANUP_TIMEOUT_MS),
    )?;
    let status = parse_remote_cleanup_status(&readback.stdout, pid, pgid, Some(identity))
        .ok_or_else(|| {
            format!(
                "startup identity-bound cleanup returned no valid {SHELL_REMOTE_CLEANUP_MARKER}; exit={:?}; stdout_sha256={}; stderr_sha256={}",
                readback.exit_code,
                readback.stdout_sha256,
                readback.stderr_sha256
            )
        })?;
    if readback.exit_code != Some(0) || !matches!(status.as_str(), "already_gone" | "terminated") {
        return Err(format!(
            "startup identity-bound cleanup did not reach a verified terminal status; status={status}; exit={:?}; stdout_sha256={}; stderr_sha256={}",
            readback.exit_code, readback.stdout_sha256, readback.stderr_sha256
        ));
    }
    Ok((readback, status))
}

/// Inspect the durable request only when corrupt-status recovery otherwise has
/// no remote ownership evidence. A typed local request can positively exclude
/// SSH intent; missing, malformed, or redacted shell metadata is uncertainty
/// and therefore cannot authorize quarantine.
fn corrupt_shell_job_request_ssh_intent(
    paths: &ShellJobPaths,
    job_id: &str,
) -> Result<Option<String>, String> {
    let request_bytes = match fs::read(&paths.request_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "request.json is absent for {job_id}; SSH intent cannot be excluded without a sidecar or process marker"
            ));
        }
        Err(error) => {
            return Err(format!(
                "could not read request.json while excluding SSH intent for {job_id}: {error}"
            ));
        }
    };
    let request: Value = serde_json::from_slice(&request_bytes).map_err(|error| {
        format!("could not decode request.json while excluding SSH intent for {job_id}: {error}")
    })?;
    let command = request
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "request.json has no typed command metadata for {job_id}; SSH intent is unverifiable"
            )
        })?;
    if let Some(client) = ssh_family_client_for_executable(command) {
        return Ok(Some(format!(
            "request_direct_ssh_family:{client}:{}",
            executable_leaf(command)
        )));
    }

    let args_redacted = match request.get("args_redacted") {
        None => false,
        Some(Value::Bool(redacted)) => *redacted,
        Some(_) => {
            return Err(format!(
                "request.json args_redacted metadata is not boolean for {job_id}; SSH intent is unverifiable"
            ));
        }
    };
    let args = match request.get("args") {
        None => {
            return Err(format!(
                "request.json has no typed args metadata for {job_id}; shell-wrapped SSH intent is unverifiable"
            ));
        }
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    format!(
                        "request.json args metadata contains a non-string value for {job_id}; SSH intent is unverifiable"
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(format!(
                "request.json args metadata is not an array for {job_id}; SSH intent is unverifiable"
            ));
        }
    };
    if let Some(evidence) = durable_ssh_promotion_evidence(command, &args) {
        return Ok(Some(format!("request_{evidence}")));
    }
    if args_redacted
        && matches!(
            executable_leaf(command).to_ascii_lowercase().as_str(),
            "powershell"
                | "powershell.exe"
                | "pwsh"
                | "pwsh.exe"
                | "cmd"
                | "cmd.exe"
                | "sh"
                | "bash"
                | "dash"
                | "zsh"
                | "ksh"
        )
    {
        return Err(format!(
            "request.json shell arguments are redacted for {job_id}; SSH intent cannot be excluded without a sidecar or process marker"
        ));
    }
    Ok(None)
}

fn verify_corrupt_shell_job_remote_state(
    paths: &ShellJobPaths,
    job_id: &str,
    recovery_id: &str,
    quarantine_job_dir: &Path,
) -> Result<ShellJobQuarantineRemoteVerification, String> {
    let sidecar_present = paths.remote_cleanup_path.try_exists().map_err(|error| {
        format!(
            "could not inspect remote cleanup sidecar {}: {error}",
            paths.remote_cleanup_path.display()
        )
    })?;
    let stderr = match fs::read(&paths.stderr_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(format!(
                "could not read stderr while excluding an SSH remote process marker for {job_id}: {error}"
            ));
        }
    };
    let stderr_text = String::from_utf8_lossy(&stderr);
    // A truncated/future-version marker is evidence that remote tracking may
    // have started even when the strict V1 parser cannot recover its fields.
    // Never reinterpret that uncertainty as "not remote".
    let raw_process_marker_present = stderr_text.contains("SYNAPSE_REMOTE_PROCESS_");
    let invocation = if sidecar_present {
        Some(
            read_shell_remote_cleanup_invocation(paths, job_id)?.ok_or_else(|| {
                "remote cleanup sidecar disappeared during startup recovery".to_owned()
            })?,
        )
    } else {
        None
    };
    let expected_ownership_token = invocation
        .as_ref()
        .and_then(|invocation| invocation.ownership_token.as_deref());
    let metadata = parse_remote_process_metadata_with_ownership(
        &stderr_text,
        job_id,
        expected_ownership_token,
    );
    if !sidecar_present && metadata.is_none() {
        if raw_process_marker_present {
            return Err(format!(
                "stderr contains a raw SYNAPSE_REMOTE_PROCESS_ marker for {job_id}, but no valid process identity and no remote-cleanup.json sidecar; remote state is unverifiable"
            ));
        }
        if let Some(request_evidence) = corrupt_shell_job_request_ssh_intent(paths, job_id)? {
            return Err(format!(
                "request.json records SSH intent for {job_id} ({request_evidence}), but no remote-cleanup.json sidecar or process marker proves remote ownership/state"
            ));
        }
        return Ok(ShellJobQuarantineRemoteVerification {
            sidecar_present: false,
            process_marker_present: false,
            remote_identity_sha256: None,
            remote_pid: None,
            remote_pgid: None,
            liveness_before: None,
            cleanup: None,
            liveness_after: None,
            recovery_intent_sha256: None,
            recovery_outcome_sha256: None,
            verdict: "not_remote_or_remote_process_never_tracked".to_owned(),
        });
    }
    if !sidecar_present {
        return Err(format!(
            "{SHELL_REMOTE_PROCESS_MARKER} exists for {job_id}, but remote-cleanup.json is absent; remote state is unverifiable"
        ));
    }
    let invocation = invocation
        .ok_or_else(|| "remote cleanup sidecar disappeared during startup recovery".to_owned())?;
    let sidecar_bytes = fs::read(&paths.remote_cleanup_path).map_err(|error| {
        format!(
            "could not read remote cleanup sidecar bytes for recovery binding at {}: {error}",
            paths.remote_cleanup_path.display()
        )
    })?;
    let cleanup_sidecar_sha256 = sha256_hex(&sidecar_bytes);
    let metadata = metadata.ok_or_else(|| {
        format!(
            "remote-cleanup.json exists for {job_id}, but stderr has no valid {SHELL_REMOTE_PROCESS_MARKER}; remote state is unverifiable"
        )
    })?;
    let pid = metadata.pid;
    let pgid = metadata.pgid;
    let remote_identity_digest = sha256_hex(invocation.remote_identity.as_bytes());
    let remote_identity_sha256 = Some(remote_identity_digest.clone());
    let existing_intent = read_existing_remote_recovery_intent(paths, job_id)?;
    let existing_outcome = read_existing_remote_recovery_outcome(paths, job_id)?;
    if existing_outcome.is_some() && existing_intent.is_none() {
        return Err(format!(
            "remote recovery outcome exists without its immutable intent for {job_id}"
        ));
    }
    if let Some((intent, intent_sha256)) = existing_intent.as_ref() {
        let ownership_token_sha256 = metadata
            .ownership_token
            .as_deref()
            .map(|token| sha256_hex(token.as_bytes()));
        if intent.recovery_id != recovery_id
            || intent.quarantine_job_dir != path_string(quarantine_job_dir)
            || intent.remote_identity_sha256 != remote_identity_digest
            || intent.remote_pid != pid
            || intent.remote_pgid != pgid
            || metadata.boot_id.as_deref() != Some(intent.remote_boot_id.as_str())
            || metadata.start_time.as_deref() != Some(intent.remote_process_start_time.as_str())
            || ownership_token_sha256.as_deref()
                != Some(intent.remote_ownership_token_sha256.as_str())
            || intent.cleanup_sidecar_sha256 != cleanup_sidecar_sha256
            || intent.cleanup_sidecar_schema_version != invocation.schema_version
        {
            return Err(format!(
                "existing remote recovery intent no longer matches marker/sidecar/quarantine identity for {job_id}; intent_sha256={intent_sha256}"
            ));
        }
    }
    if let (Some((intent, intent_sha256)), Some((outcome, _))) =
        (existing_intent.as_ref(), existing_outcome.as_ref())
        && (outcome.recovery_id != intent.recovery_id || outcome.intent_sha256 != *intent_sha256)
    {
        return Err(format!(
            "remote recovery outcome is not bound to the immutable intent for {job_id}"
        ));
    }
    let (liveness_before_readback, liveness_before_status) = run_corrupt_job_remote_liveness_probe(
        &invocation,
        &pid,
        &pgid,
        "startup remote liveness probe",
    )?;
    let liveness_before_evidence = shell_job_remote_command_evidence(
        "liveness_before",
        &liveness_before_readback,
        &liveness_before_status,
    );
    let liveness_before = Some(liveness_before_evidence);
    if liveness_before_status == "already_gone" {
        if let Some((intent, intent_sha256)) = existing_intent.as_ref() {
            let resume_liveness_after = shell_job_remote_command_evidence(
                "resume_liveness_after",
                &liveness_before_readback,
                &liveness_before_status,
            );
            let (cleanup, outcome_sha256, verdict) =
                if let Some((outcome, outcome_sha256)) = existing_outcome.as_ref() {
                    (
                        outcome.cleanup.clone(),
                        outcome_sha256.clone(),
                        "remote_cleanup_recovery_resumed_verified".to_owned(),
                    )
                } else {
                    // A prior daemon may have crashed after the pidfd signal but
                    // before committing the outcome. This independent liveness
                    // read proves the target is now gone; complete the SAME
                    // recovery id without manufacturing a second intent.
                    let outcome = ShellJobRemoteRecoveryOutcome {
                        schema_version: 1,
                        recovery_id: intent.recovery_id.clone(),
                        job_id: job_id.to_owned(),
                        completed_at: chrono::Utc::now().to_rfc3339(),
                        intent_sha256: intent_sha256.clone(),
                        cleanup: None,
                        liveness_after: resume_liveness_after.clone(),
                        verdict: "remote_already_gone_after_durable_cleanup_intent".to_owned(),
                    };
                    let digest = persist_remote_recovery_outcome(paths, &outcome)?;
                    (None, digest, outcome.verdict)
                };
            return Ok(ShellJobQuarantineRemoteVerification {
                sidecar_present: true,
                process_marker_present: true,
                remote_identity_sha256,
                remote_pid: Some(pid),
                remote_pgid: Some(pgid),
                liveness_before,
                cleanup,
                liveness_after: Some(resume_liveness_after),
                recovery_intent_sha256: Some(intent_sha256.clone()),
                recovery_outcome_sha256: Some(outcome_sha256),
                verdict,
            });
        }
        return Ok(ShellJobQuarantineRemoteVerification {
            sidecar_present: true,
            process_marker_present: true,
            remote_identity_sha256,
            remote_pid: Some(pid),
            remote_pgid: Some(pgid),
            liveness_before,
            cleanup: None,
            liveness_after: None,
            recovery_intent_sha256: None,
            recovery_outcome_sha256: None,
            verdict: "remote_already_gone_verified".to_owned(),
        });
    }
    if liveness_before_status == "alive" {
        if !matches!(invocation.schema_version, 3 | 4) {
            return Err(format!(
                "startup remote process is alive for pid={pid} pgid={pgid}, but cleanup sidecar schema {} does not bind the exact executable and effective replay argv; retained without destructive cleanup",
                invocation.schema_version
            ));
        }
        let (Some(boot_id), Some(start_time), Some(ownership_token)) = (
            metadata.boot_id.as_ref(),
            metadata.start_time.as_ref(),
            metadata.ownership_token.as_ref(),
        ) else {
            return Err(format!(
                "startup remote process is alive for pid={pid} pgid={pgid}, but its marker has no complete boot/start/token ownership identity; retained without destructive cleanup"
            ));
        };
        if existing_outcome.is_some() {
            return Err(format!(
                "remote recovery outcome claims terminal state but pid={pid} pgid={pgid} is alive for {job_id}"
            ));
        }
        let ownership_identity = RemoteProcessOwnershipIdentity {
            boot_id: boot_id.clone(),
            start_time: start_time.clone(),
            ownership_token: ownership_token.clone(),
        };
        let (intent, intent_sha256) = if let Some((intent, digest)) = existing_intent {
            (intent, digest)
        } else {
            let intent = ShellJobRemoteRecoveryIntent {
                schema_version: 1,
                recovery_id: recovery_id.to_owned(),
                job_id: job_id.to_owned(),
                created_at: chrono::Utc::now().to_rfc3339(),
                quarantine_job_dir: path_string(quarantine_job_dir),
                remote_identity_sha256: remote_identity_digest,
                remote_pid: pid.clone(),
                remote_pgid: pgid.clone(),
                remote_boot_id: boot_id.clone(),
                remote_process_start_time: start_time.clone(),
                remote_ownership_token_sha256: sha256_hex(ownership_token.as_bytes()),
                cleanup_sidecar_sha256,
                cleanup_sidecar_schema_version: invocation.schema_version,
                reason: "startup_corrupt_status_identity_bound_remote_cleanup".to_owned(),
            };
            let digest = persist_remote_recovery_intent(paths, &intent)?;
            (intent, digest)
        };
        let (cleanup_readback, cleanup_status) = run_corrupt_job_identity_bound_remote_cleanup(
            &invocation,
            &pid,
            &pgid,
            &ownership_identity,
        )?;
        let cleanup = shell_job_remote_command_evidence(
            "identity_bound_cleanup",
            &cleanup_readback,
            &cleanup_status,
        );
        let (liveness_after_readback, liveness_after_status) =
            run_corrupt_job_remote_liveness_probe(
                &invocation,
                &pid,
                &pgid,
                "startup remote liveness-after probe",
            )?;
        if liveness_after_status != "already_gone" {
            return Err(format!(
                "identity-bound cleanup returned status={cleanup_status}, but separate liveness-after read found status={liveness_after_status} for pid={pid} pgid={pgid}"
            ));
        }
        let liveness_after = shell_job_remote_command_evidence(
            "liveness_after",
            &liveness_after_readback,
            &liveness_after_status,
        );
        let outcome = ShellJobRemoteRecoveryOutcome {
            schema_version: 1,
            recovery_id: intent.recovery_id,
            job_id: job_id.to_owned(),
            completed_at: chrono::Utc::now().to_rfc3339(),
            intent_sha256: intent_sha256.clone(),
            cleanup: Some(cleanup.clone()),
            liveness_after: liveness_after.clone(),
            verdict: "remote_identity_bound_cleanup_verified".to_owned(),
        };
        let outcome_sha256 = persist_remote_recovery_outcome(paths, &outcome)?;
        return Ok(ShellJobQuarantineRemoteVerification {
            sidecar_present: true,
            process_marker_present: true,
            remote_identity_sha256,
            remote_pid: Some(pid),
            remote_pgid: Some(pgid),
            liveness_before,
            cleanup: Some(cleanup),
            liveness_after: Some(liveness_after),
            recovery_intent_sha256: Some(intent_sha256),
            recovery_outcome_sha256: Some(outcome_sha256),
            verdict: outcome.verdict,
        });
    }
    Err(format!(
        "startup remote liveness probe returned unsupported status={liveness_before_status} for pid={pid} pgid={pgid}"
    ))
}

fn persist_shell_job_quarantine_manifest(
    job_dir: &Path,
    job_id: &str,
    recovery_id: &str,
    manifest: &ShellJobQuarantineManifest,
) -> Result<(PathBuf, String), String> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("failed to encode quarantine manifest for {job_id}: {error}"))?;
    let manifest_path = job_dir.join(format!("quarantine-manifest-{recovery_id}.json"));
    let tmp_path = shell_status_temp_path(&manifest_path);
    if let Err(error) = write_shell_job_status_staging(&tmp_path, &bytes) {
        let staging_cleanup = cleanup_failed_atomic_staging_file(&tmp_path);
        return Err(format!(
            "failed to durably stage quarantine manifest {}: {error}; {staging_cleanup}",
            manifest_path.display(),
        ));
    }
    if let Err(error) = commit_shell_job_status_file(&tmp_path, &manifest_path, job_id) {
        let staging_cleanup = cleanup_failed_atomic_staging_file(&tmp_path);
        return Err(format!(
            "failed to atomically commit quarantine manifest {}: {error}; {staging_cleanup}",
            manifest_path.display(),
        ));
    }
    let persisted = fs::read(&manifest_path).map_err(|error| {
        format!(
            "failed to read back quarantine manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    if persisted != bytes {
        return Err(format!(
            "quarantine manifest readback bytes differ at {}: expected_sha256={} actual_sha256={}",
            manifest_path.display(),
            sha256_hex(&bytes),
            sha256_hex(&persisted)
        ));
    }
    let decoded: ShellJobQuarantineManifest =
        serde_json::from_slice(&persisted).map_err(|error| {
            format!(
                "quarantine manifest readback did not decode at {}: {error}",
                manifest_path.display()
            )
        })?;
    if &decoded != manifest {
        return Err(format!(
            "quarantine manifest structured readback differs at {}",
            manifest_path.display()
        ));
    }
    Ok((manifest_path, sha256_hex(&persisted)))
}

#[cfg(windows)]
fn rename_shell_job_dir_to_quarantine(source: &Path, destination: &Path) -> io::Result<()> {
    use windows::{
        Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION},
        Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW},
        core::PCWSTR,
    };
    const RETRYABLE_CODES: [u32; 2] = [ERROR_ACCESS_DENIED.0, ERROR_SHARING_VIOLATION.0];
    const MAX_ATTEMPTS: u32 = 24;
    const BACKOFF_CAP_MS: u64 = 50;
    let source_wide = path_to_nul_terminated_wide(source);
    let destination_wide = path_to_nul_terminated_wide(destination);
    let mut backoff_ms = 1u64;
    for attempt in 1..=MAX_ATTEMPTS {
        // SAFETY: both vectors are NUL-terminated and live for the call.
        match unsafe {
            MoveFileExW(
                PCWSTR(source_wide.as_ptr()),
                PCWSTR(destination_wide.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        } {
            Ok(()) => return Ok(()),
            Err(error) => {
                let low_code = win32_error_low_code(&error);
                if attempt < MAX_ATTEMPTS && RETRYABLE_CODES.contains(&low_code) {
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = backoff_ms.saturating_mul(2).min(BACKOFF_CAP_MS);
                    continue;
                }
                return Err(io::Error::from_raw_os_error(low_code as i32));
            }
        }
    }
    Err(io::Error::other(
        "quarantine directory rename exhausted retries without a terminal result",
    ))
}

#[cfg(not(windows))]
fn rename_shell_job_dir_to_quarantine(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)?;
    let source_parent = source
        .parent()
        .ok_or_else(|| io::Error::other("quarantine source has no parent directory"))?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("quarantine destination has no parent directory"))?;
    sync_directory_entry_parent(source_parent)?;
    if destination_parent != source_parent {
        sync_directory_entry_parent(destination_parent)?;
    }
    Ok(())
}

fn verify_shell_job_quarantine_readback(
    source: &Path,
    destination: &Path,
    manifest_file_name: &str,
    expected: &ShellJobQuarantineManifest,
) -> Result<(), String> {
    if source.try_exists().map_err(|error| {
        format!(
            "failed to read source existence after quarantine move {}: {error}",
            source.display()
        )
    })? {
        return Err(format!(
            "source job directory still exists after quarantine move: {}",
            source.display()
        ));
    }
    if !destination.is_dir() {
        return Err(format!(
            "quarantine destination is not a directory after move: {}",
            destination.display()
        ));
    }
    let manifest_path = destination.join(manifest_file_name);
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        format!(
            "failed to read quarantine manifest after directory move {}: {error}",
            manifest_path.display()
        )
    })?;
    let actual: ShellJobQuarantineManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| {
            format!(
                "failed to decode quarantine manifest after directory move {}: {error}",
                manifest_path.display()
            )
        })?;
    if &actual != expected {
        return Err(format!(
            "quarantine manifest changed during directory move: {}",
            manifest_path.display()
        ));
    }
    validate_shell_job_quarantine_manifest_structure(&actual, destination, manifest_file_name)?;
    let mut expected_file_names = expected
        .artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect::<HashSet<_>>();
    if !expected_file_names.insert(manifest_file_name.to_owned()) {
        return Err(format!(
            "quarantine manifest name collides with an original artifact: {manifest_file_name}"
        ));
    }
    verify_shell_job_quarantine_exact_file_set(destination, &expected_file_names)?;
    verify_shell_job_quarantine_manifest_artifacts(destination, expected)?;
    Ok(())
}

fn verify_shell_job_quarantine_exact_file_set(
    destination: &Path,
    expected_file_names: &HashSet<String>,
) -> Result<(), String> {
    let entries = fs::read_dir(destination).map_err(|error| {
        format!(
            "failed to enumerate quarantine destination {}: {error}",
            destination.display()
        )
    })?;
    let mut actual_file_names = HashSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read quarantine destination entry under {}: {error}",
                destination.display()
            )
        })?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to classify quarantine destination entry {}: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            return Err(format!(
                "quarantine destination entry is not a regular file: {}",
                entry.path().display()
            ));
        }
        let name = entry.file_name().into_string().map_err(|name| {
            format!(
                "quarantine destination entry name is not UTF-8: {}",
                name.to_string_lossy()
            )
        })?;
        if !actual_file_names.insert(name.clone()) {
            return Err(format!(
                "quarantine destination contains a duplicate file name: {name}"
            ));
        }
    }
    if &actual_file_names != expected_file_names {
        let mut expected = expected_file_names.iter().cloned().collect::<Vec<_>>();
        let mut actual = actual_file_names.into_iter().collect::<Vec<_>>();
        expected.sort();
        actual.sort();
        return Err(format!(
            "quarantine destination artifact set differs: expected={expected:?} actual={actual:?}"
        ));
    }
    Ok(())
}

fn validate_shell_job_quarantine_remote_verification(
    manifest: &ShellJobQuarantineManifest,
) -> Result<(), String> {
    let remote = &manifest.remote_verification;
    if !remote.sidecar_present {
        if remote.process_marker_present
            || remote.remote_identity_sha256.is_some()
            || remote.remote_pid.is_some()
            || remote.remote_pgid.is_some()
            || remote.liveness_before.is_some()
            || remote.cleanup.is_some()
            || remote.liveness_after.is_some()
            || remote.recovery_intent_sha256.is_some()
            || remote.recovery_outcome_sha256.is_some()
            || remote.verdict != "not_remote_or_remote_process_never_tracked"
        {
            return Err(format!(
                "local quarantine manifest carries inconsistent remote evidence for {}",
                manifest.job_id
            ));
        }
        return Ok(());
    }
    if !remote.process_marker_present
        || remote.remote_identity_sha256.is_none()
        || remote.remote_pid.is_none()
        || remote.remote_pgid.is_none()
    {
        return Err(format!(
            "remote quarantine manifest lacks marker/identity/process evidence for {}",
            manifest.job_id
        ));
    }
    let remote_identity_sha256 = remote.remote_identity_sha256.as_deref().ok_or_else(|| {
        format!(
            "remote quarantine manifest lacks identity digest for {}",
            manifest.job_id
        )
    })?;
    validate_recovery_sha256(
        remote_identity_sha256,
        "manifest_remote_identity_sha256",
        &manifest.job_id,
    )?;
    let liveness_before = remote.liveness_before.as_ref().ok_or_else(|| {
        format!(
            "remote quarantine manifest lacks liveness-before evidence for {}",
            manifest.job_id
        )
    })?;
    validate_remote_command_evidence_digests(
        liveness_before,
        "manifest_liveness_before",
        &manifest.job_id,
    )?;
    if let Some(cleanup) = remote.cleanup.as_ref() {
        validate_remote_command_evidence_digests(cleanup, "manifest_cleanup", &manifest.job_id)?;
    }
    if let Some(liveness_after) = remote.liveness_after.as_ref() {
        validate_remote_command_evidence_digests(
            liveness_after,
            "manifest_liveness_after",
            &manifest.job_id,
        )?;
    }
    if liveness_before.operation != "liveness_before"
        || liveness_before.exit_code != Some(0)
        || !matches!(
            liveness_before.parsed_status.as_str(),
            "alive" | "already_gone"
        )
    {
        return Err(format!(
            "remote quarantine manifest has non-terminal liveness-before evidence for {}",
            manifest.job_id
        ));
    }
    match remote.verdict.as_str() {
        "remote_already_gone_verified" => {
            if liveness_before.parsed_status != "already_gone"
                || remote.cleanup.is_some()
                || remote.liveness_after.is_some()
                || remote.recovery_intent_sha256.is_some()
                || remote.recovery_outcome_sha256.is_some()
            {
                return Err(format!(
                    "already-gone quarantine verdict has inconsistent evidence for {}",
                    manifest.job_id
                ));
            }
        }
        "remote_identity_bound_cleanup_verified" => {
            let intent_sha256 = remote.recovery_intent_sha256.as_deref().ok_or_else(|| {
                format!(
                    "remote cleanup verdict lacks intent digest for {}",
                    manifest.job_id
                )
            })?;
            let outcome_sha256 = remote.recovery_outcome_sha256.as_deref().ok_or_else(|| {
                format!(
                    "remote cleanup verdict lacks outcome digest for {}",
                    manifest.job_id
                )
            })?;
            validate_recovery_sha256(intent_sha256, "manifest_intent_sha256", &manifest.job_id)?;
            validate_recovery_sha256(outcome_sha256, "manifest_outcome_sha256", &manifest.job_id)?;
            let cleanup = remote.cleanup.as_ref().ok_or_else(|| {
                format!(
                    "identity-bound cleanup verdict lacks cleanup evidence for {}",
                    manifest.job_id
                )
            })?;
            let liveness_after = remote.liveness_after.as_ref().ok_or_else(|| {
                format!(
                    "remote cleanup verdict lacks liveness-after evidence for {}",
                    manifest.job_id
                )
            })?;
            if liveness_before.parsed_status != "alive"
                || cleanup.operation != "identity_bound_cleanup"
                || cleanup.exit_code != Some(0)
                || !matches!(
                    cleanup.parsed_status.as_str(),
                    "terminated" | "already_gone"
                )
                || liveness_after.operation != "liveness_after"
                || liveness_after.exit_code != Some(0)
                || liveness_after.parsed_status != "already_gone"
            {
                return Err(format!(
                    "identity-bound cleanup verdict has an impossible evidence shape for {}",
                    manifest.job_id
                ));
            }
        }
        "remote_already_gone_after_durable_cleanup_intent" => {
            let intent_sha256 = remote.recovery_intent_sha256.as_deref().ok_or_else(|| {
                format!(
                    "already-gone-after-intent verdict lacks intent digest for {}",
                    manifest.job_id
                )
            })?;
            let outcome_sha256 = remote.recovery_outcome_sha256.as_deref().ok_or_else(|| {
                format!(
                    "already-gone-after-intent verdict lacks outcome digest for {}",
                    manifest.job_id
                )
            })?;
            validate_recovery_sha256(intent_sha256, "manifest_intent_sha256", &manifest.job_id)?;
            validate_recovery_sha256(outcome_sha256, "manifest_outcome_sha256", &manifest.job_id)?;
            let liveness_after = remote.liveness_after.as_ref().ok_or_else(|| {
                format!(
                    "already-gone-after-intent verdict lacks liveness-after for {}",
                    manifest.job_id
                )
            })?;
            if liveness_before.parsed_status != "already_gone"
                || remote.cleanup.is_some()
                || liveness_after.operation != "resume_liveness_after"
                || liveness_after.exit_code != Some(0)
                || liveness_after.parsed_status != "already_gone"
            {
                return Err(format!(
                    "already-gone-after-intent verdict has an impossible evidence shape for {}",
                    manifest.job_id
                ));
            }
        }
        "remote_cleanup_recovery_resumed_verified" => {
            let intent_sha256 = remote.recovery_intent_sha256.as_deref().ok_or_else(|| {
                format!(
                    "resumed cleanup verdict lacks intent digest for {}",
                    manifest.job_id
                )
            })?;
            let outcome_sha256 = remote.recovery_outcome_sha256.as_deref().ok_or_else(|| {
                format!(
                    "resumed cleanup verdict lacks outcome digest for {}",
                    manifest.job_id
                )
            })?;
            validate_recovery_sha256(intent_sha256, "manifest_intent_sha256", &manifest.job_id)?;
            validate_recovery_sha256(outcome_sha256, "manifest_outcome_sha256", &manifest.job_id)?;
            let liveness_after = remote.liveness_after.as_ref().ok_or_else(|| {
                format!(
                    "resumed cleanup verdict lacks liveness-after for {}",
                    manifest.job_id
                )
            })?;
            let cleanup_valid = remote.cleanup.as_ref().is_none_or(|cleanup| {
                cleanup.operation == "identity_bound_cleanup"
                    && cleanup.exit_code == Some(0)
                    && matches!(
                        cleanup.parsed_status.as_str(),
                        "terminated" | "already_gone"
                    )
            });
            if liveness_before.parsed_status != "already_gone"
                || !cleanup_valid
                || liveness_after.operation != "resume_liveness_after"
                || liveness_after.exit_code != Some(0)
                || liveness_after.parsed_status != "already_gone"
            {
                return Err(format!(
                    "resumed cleanup verdict has an impossible evidence shape for {}",
                    manifest.job_id
                ));
            }
        }
        verdict => {
            return Err(format!(
                "unsupported remote quarantine verdict={verdict} for {}",
                manifest.job_id
            ));
        }
    }
    Ok(())
}

fn validate_shell_job_quarantine_manifest_structure(
    manifest: &ShellJobQuarantineManifest,
    destination: &Path,
    manifest_file_name: &str,
) -> Result<(), String> {
    if !matches!(manifest.schema_version, 1 | 2)
        || manifest.quarantine_job_dir != path_string(destination)
        || manifest_file_name != format!("quarantine-manifest-{}.json", manifest.recovery_id)
        || destination.file_name().and_then(|name| name.to_str())
            != Some(format!("{}-{}", manifest.job_id, manifest.recovery_id).as_str())
    {
        return Err(format!(
            "quarantine manifest schema/path/name identity differs at {}",
            destination.display()
        ));
    }
    if manifest.original_artifact_count != manifest.artifacts.len() {
        return Err(format!(
            "quarantine manifest artifact count differs for {}: recorded={} actual={}",
            manifest.job_id,
            manifest.original_artifact_count,
            manifest.artifacts.len()
        ));
    }
    let mut names = HashSet::new();
    let mut bytes = 0u64;
    for artifact in &manifest.artifacts {
        let relative = Path::new(&artifact.relative_path);
        if relative.components().count() != 1
            || relative.file_name().and_then(|name| name.to_str())
                != Some(artifact.relative_path.as_str())
            || !names.insert(artifact.relative_path.clone())
            || artifact.relative_path == manifest_file_name
            || artifact.relative_path.starts_with("quarantine-complete-")
        {
            return Err(format!(
                "quarantine manifest contains unsafe/duplicate metadata-colliding artifact name {:?} for {}",
                artifact.relative_path, manifest.job_id
            ));
        }
        validate_recovery_sha256(&artifact.sha256, "artifact_sha256", &manifest.job_id)?;
        bytes = bytes.checked_add(artifact.byte_len).ok_or_else(|| {
            format!(
                "quarantine manifest artifact byte total overflow for {} while adding {:?}: before={bytes} add={}",
                manifest.job_id, artifact.relative_path, artifact.byte_len
            )
        })?;
    }
    if bytes != manifest.original_artifact_bytes {
        return Err(format!(
            "quarantine manifest artifact byte total differs for {}: recorded={} actual={bytes}",
            manifest.job_id, manifest.original_artifact_bytes
        ));
    }
    let (actual_pre_count, actual_pre_bytes, actual_generated_count, actual_generated_bytes) =
        shell_job_quarantine_artifact_accounting(&manifest.artifacts)?;
    if manifest.schema_version >= 2 {
        let recorded_count = manifest
            .pre_recovery_artifact_count
            .checked_add(manifest.recovery_generated_artifact_count)
            .ok_or_else(|| {
                format!(
                    "quarantine manifest recorded artifact count overflow for {}: pre={} generated={}",
                    manifest.job_id,
                    manifest.pre_recovery_artifact_count,
                    manifest.recovery_generated_artifact_count
                )
            })?;
        let recorded_bytes = manifest
            .pre_recovery_artifact_bytes
            .checked_add(manifest.recovery_generated_artifact_bytes)
            .ok_or_else(|| {
                format!(
                    "quarantine manifest recorded artifact byte overflow for {}: pre={} generated={}",
                    manifest.job_id,
                    manifest.pre_recovery_artifact_bytes,
                    manifest.recovery_generated_artifact_bytes
                )
            })?;
        if recorded_count != manifest.original_artifact_count
            || recorded_bytes != manifest.original_artifact_bytes
            || manifest.pre_recovery_artifact_count != actual_pre_count
            || manifest.pre_recovery_artifact_bytes != actual_pre_bytes
            || manifest.recovery_generated_artifact_count != actual_generated_count
            || manifest.recovery_generated_artifact_bytes != actual_generated_bytes
        {
            return Err(format!(
                "quarantine manifest pre-recovery/generated artifact accounting differs for {}",
                manifest.job_id
            ));
        }
    }
    validate_shell_job_quarantine_remote_verification(manifest)?;
    if let Some(intent_sha256) = manifest
        .remote_verification
        .recovery_intent_sha256
        .as_deref()
    {
        let name = format!("remote-recovery-intent-{}.json", manifest.recovery_id);
        if !manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.relative_path == name && artifact.sha256 == intent_sha256)
        {
            return Err(format!(
                "quarantine manifest does not inventory the exact remote recovery intent digest for {}",
                manifest.job_id
            ));
        }
    }
    if let Some(outcome_sha256) = manifest
        .remote_verification
        .recovery_outcome_sha256
        .as_deref()
    {
        let name = format!("remote-recovery-outcome-{}.json", manifest.recovery_id);
        if !manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.relative_path == name && artifact.sha256 == outcome_sha256)
        {
            return Err(format!(
                "quarantine manifest does not inventory the exact remote recovery outcome digest for {}",
                manifest.job_id
            ));
        }
    }
    Ok(())
}

fn verify_shell_job_quarantine_manifest_artifacts(
    directory: &Path,
    manifest: &ShellJobQuarantineManifest,
) -> Result<(), String> {
    for artifact in &manifest.artifacts {
        let path = directory.join(&artifact.relative_path);
        let bytes = fs::read(&path).map_err(|error| {
            format!(
                "failed to read quarantined artifact {}: {error}",
                path.display()
            )
        })?;
        let actual_len = u64::try_from(bytes.len()).map_err(|error| {
            format!(
                "quarantined artifact length cannot be represented at {}: {error}",
                path.display()
            )
        })?;
        if actual_len != artifact.byte_len || sha256_hex(&bytes) != artifact.sha256 {
            return Err(format!(
                "quarantined artifact digest/length differs at {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn remove_stale_atomic_staging_files(
    directory: &Path,
    final_file_name: &str,
) -> Result<usize, String> {
    let prefix = format!("{final_file_name}.tmp.");
    let mut removed = 0usize;
    for entry in fs::read_dir(directory).map_err(|error| {
        format!(
            "failed to enumerate stale atomic staging files under {}: {error}",
            directory.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read stale atomic staging entry under {}: {error}",
                directory.display()
            )
        })?;
        let name = entry.file_name().into_string().map_err(|name| {
            format!(
                "stale atomic staging filename is not UTF-8 under {}: {}",
                directory.display(),
                name.to_string_lossy()
            )
        })?;
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let mut segments = suffix.split('.');
        let owned_shape =
            segments.next().is_some_and(|value| {
                !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
            }) && segments.next().is_some_and(|value| {
                !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
            }) && segments.next().is_none();
        if !owned_shape {
            return Err(format!(
                "refusing to remove staging-like file with an unrecognized ownership suffix: {}",
                entry.path().display()
            ));
        }
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to classify stale atomic staging file {}: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            return Err(format!(
                "stale atomic staging path is not a regular file: {}",
                entry.path().display()
            ));
        }
        fs::remove_file(entry.path()).map_err(|error| {
            format!(
                "failed to remove stale atomic staging file {}: {error}",
                entry.path().display()
            )
        })?;
        removed = removed.saturating_add(1);
    }
    if removed > 0 {
        #[cfg(not(windows))]
        sync_directory_entry_parent(directory).map_err(|error| {
            format!(
                "failed to sync directory after stale staging cleanup {}: {error}",
                directory.display()
            )
        })?;
    }
    Ok(removed)
}

fn read_existing_source_quarantine_manifest(
    job_dir: &Path,
    job_id: &str,
    quarantine_root: &Path,
) -> Result<Option<(ShellJobQuarantineManifest, String, String)>, String> {
    let manifests = shell_job_recovery_record_paths(job_dir, "quarantine-manifest-")?;
    if manifests.len() > 1 {
        return Err(format!(
            "source job {job_id} has {} committed quarantine manifests; exactly zero or one is allowed",
            manifests.len()
        ));
    }
    let Some(manifest_path) = manifests.first() else {
        return Ok(None);
    };
    let manifest_file_name = manifest_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("quarantine manifest filename is not UTF-8 for {job_id}"))?
        .to_owned();
    let bytes = fs::read(manifest_path).map_err(|error| {
        format!(
            "failed to read committed source quarantine manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let manifest: ShellJobQuarantineManifest = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to decode committed source quarantine manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let destination = PathBuf::from(&manifest.quarantine_job_dir);
    if manifest.job_id != job_id
        || manifest.source_job_dir != path_string(job_dir)
        || destination.parent() != Some(quarantine_root)
    {
        return Err(format!(
            "committed source quarantine manifest identity/root differs for {job_id}"
        ));
    }
    if destination.try_exists().map_err(|error| {
        format!(
            "failed to inspect committed manifest destination {}: {error}",
            destination.display()
        )
    })? {
        return Err(format!(
            "both source and destination exist for committed quarantine manifest {}",
            manifest_path.display()
        ));
    }
    validate_shell_job_quarantine_manifest_structure(&manifest, &destination, &manifest_file_name)?;
    let _ = remove_stale_atomic_staging_files(job_dir, &manifest_file_name)?;
    let mut expected_names = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect::<HashSet<_>>();
    if !expected_names.insert(manifest_file_name.clone()) {
        return Err(format!(
            "committed quarantine manifest collides with an original artifact for {job_id}"
        ));
    }
    verify_shell_job_quarantine_exact_file_set(job_dir, &expected_names)?;
    verify_shell_job_quarantine_manifest_artifacts(job_dir, &manifest)?;
    Ok(Some((manifest, manifest_file_name, sha256_hex(&bytes))))
}

fn existing_remote_recovery_id(
    paths: &ShellJobPaths,
    job_id: &str,
) -> Result<Option<String>, String> {
    let intent = read_existing_remote_recovery_intent(paths, job_id)?;
    let outcome = read_existing_remote_recovery_outcome(paths, job_id)?;
    match (intent, outcome) {
        (None, None) => Ok(None),
        (Some((intent, _)), None) => Ok(Some(intent.recovery_id)),
        (Some((intent, intent_sha256)), Some((outcome, _)))
            if outcome.recovery_id == intent.recovery_id
                && outcome.intent_sha256 == intent_sha256 =>
        {
            Ok(Some(intent.recovery_id))
        }
        (Some(_), Some(_)) => Err(format!(
            "remote recovery intent/outcome identity differs for {job_id}"
        )),
        (None, Some(_)) => Err(format!(
            "remote recovery outcome exists without an intent for {job_id}"
        )),
    }
}

fn persist_shell_job_quarantine_completion(
    destination: &Path,
    recovery_id: &str,
    completion: &ShellJobQuarantineCompletion,
    expected: &ShellJobQuarantineManifest,
    manifest_file_name: &str,
) -> Result<String, String> {
    let completion_file_name = format!("quarantine-complete-{recovery_id}.json");
    let completion_path = destination.join(&completion_file_name);
    write_pretty_json_file(&completion_path, completion, "quarantine completion")
        .map_err(|error| format!("failed to persist quarantine completion: {}", error.message))?;
    let bytes = fs::read(&completion_path).map_err(|error| {
        format!(
            "failed to read quarantine completion {}: {error}",
            completion_path.display()
        )
    })?;
    let actual: ShellJobQuarantineCompletion = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to decode quarantine completion {}: {error}",
            completion_path.display()
        )
    })?;
    if &actual != completion {
        return Err(format!(
            "quarantine completion structured readback differs at {}",
            completion_path.display()
        ));
    }
    let mut expected_file_names = expected
        .artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect::<HashSet<_>>();
    if !expected_file_names.insert(manifest_file_name.to_owned())
        || !expected_file_names.insert(completion_file_name.clone())
    {
        return Err(
            "quarantine completion/manifest name collides with an original artifact".to_owned(),
        );
    }
    verify_shell_job_quarantine_exact_file_set(destination, &expected_file_names)?;
    Ok(completion_file_name)
}

fn validate_quarantine_recovery_records(
    destination: &Path,
    manifest: &ShellJobQuarantineManifest,
) -> Result<(), String> {
    let remote = &manifest.remote_verification;
    let (Some(intent_sha256), Some(outcome_sha256)) = (
        remote.recovery_intent_sha256.as_deref(),
        remote.recovery_outcome_sha256.as_deref(),
    ) else {
        if remote.recovery_intent_sha256.is_some() || remote.recovery_outcome_sha256.is_some() {
            return Err(format!(
                "quarantine manifest has only one remote recovery record digest for {}",
                manifest.job_id
            ));
        }
        return Ok(());
    };
    let intent_path = destination.join(format!(
        "remote-recovery-intent-{}.json",
        manifest.recovery_id
    ));
    let outcome_path = destination.join(format!(
        "remote-recovery-outcome-{}.json",
        manifest.recovery_id
    ));
    let intent_bytes = fs::read(&intent_path).map_err(|error| {
        format!(
            "failed to read quarantined remote recovery intent {}: {error}",
            intent_path.display()
        )
    })?;
    let outcome_bytes = fs::read(&outcome_path).map_err(|error| {
        format!(
            "failed to read quarantined remote recovery outcome {}: {error}",
            outcome_path.display()
        )
    })?;
    if sha256_hex(&intent_bytes) != intent_sha256 || sha256_hex(&outcome_bytes) != outcome_sha256 {
        return Err(format!(
            "remote recovery intent/outcome digest differs for {}",
            manifest.job_id
        ));
    }
    let intent: ShellJobRemoteRecoveryIntent = serde_json::from_slice(&intent_bytes)
        .map_err(|error| format!("failed to decode quarantined remote recovery intent: {error}"))?;
    let outcome: ShellJobRemoteRecoveryOutcome =
        serde_json::from_slice(&outcome_bytes).map_err(|error| {
            format!("failed to decode quarantined remote recovery outcome: {error}")
        })?;
    if intent.schema_version != 1
        || outcome.schema_version != 1
        || intent.recovery_id != manifest.recovery_id
        || outcome.recovery_id != manifest.recovery_id
        || intent.job_id != manifest.job_id
        || outcome.job_id != manifest.job_id
        || intent.quarantine_job_dir != path_string(destination)
        || outcome.intent_sha256 != intent_sha256
    {
        return Err(format!(
            "remote recovery intent/outcome structured identity differs for {}",
            manifest.job_id
        ));
    }
    validate_remote_recovery_outcome_semantics(&outcome, &manifest.job_id)?;
    let evidence_matches = match manifest.remote_verification.verdict.as_str() {
        "remote_identity_bound_cleanup_verified"
        | "remote_already_gone_after_durable_cleanup_intent" => {
            outcome.verdict == manifest.remote_verification.verdict
                && outcome.cleanup == manifest.remote_verification.cleanup
                && Some(&outcome.liveness_after)
                    == manifest.remote_verification.liveness_after.as_ref()
        }
        "remote_cleanup_recovery_resumed_verified" => {
            matches!(
                outcome.verdict.as_str(),
                "remote_identity_bound_cleanup_verified"
                    | "remote_already_gone_after_durable_cleanup_intent"
            ) && outcome.cleanup == manifest.remote_verification.cleanup
        }
        _ => false,
    };
    if !evidence_matches {
        return Err(format!(
            "remote recovery outcome evidence does not match manifest verdict for {}",
            manifest.job_id
        ));
    }
    Ok(())
}

fn verify_completed_quarantine_directory(
    destination: &Path,
    completion_name: &str,
) -> Result<(), String> {
    let completion_path = destination.join(completion_name);
    let completion_bytes = fs::read(&completion_path).map_err(|error| {
        format!(
            "failed to read quarantine completion {}: {error}",
            completion_path.display()
        )
    })?;
    let completion: ShellJobQuarantineCompletion = serde_json::from_slice(&completion_bytes)
        .map_err(|error| {
            format!(
                "failed to decode quarantine completion {}: {error}",
                completion_path.display()
            )
        })?;
    if !matches!(completion.schema_version, 1 | 2)
        || completion.quarantine_job_dir != path_string(destination)
        || completion_name != format!("quarantine-complete-{}.json", completion.recovery_id)
    {
        return Err(format!(
            "quarantine completion identity/schema differs at {}",
            completion_path.display()
        ));
    }
    let manifest_path = destination.join(&completion.manifest_file_name);
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        format!(
            "failed to read completed quarantine manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    if sha256_hex(&manifest_bytes) != completion.manifest_sha256 {
        return Err(format!(
            "completed quarantine manifest digest differs at {}",
            manifest_path.display()
        ));
    }
    let manifest: ShellJobQuarantineManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| {
            format!(
                "failed to decode completed quarantine manifest {}: {error}",
                manifest_path.display()
            )
        })?;
    validate_shell_job_quarantine_manifest_structure(
        &manifest,
        destination,
        &completion.manifest_file_name,
    )?;
    if manifest.schema_version != completion.schema_version
        || manifest.recovery_id != completion.recovery_id
        || manifest.job_id != completion.job_id
        || manifest.quarantine_job_dir != completion.quarantine_job_dir
        || manifest.original_artifact_count != completion.original_artifact_count
        || manifest.original_artifact_bytes != completion.original_artifact_bytes
        || manifest.pre_recovery_artifact_count != completion.pre_recovery_artifact_count
        || manifest.pre_recovery_artifact_bytes != completion.pre_recovery_artifact_bytes
        || manifest.recovery_generated_artifact_count
            != completion.recovery_generated_artifact_count
        || manifest.recovery_generated_artifact_bytes
            != completion.recovery_generated_artifact_bytes
        || manifest.remote_verification.verdict != completion.remote_verdict
    {
        return Err(format!(
            "quarantine manifest/completion fields differ at {}",
            destination.display()
        ));
    }
    if Path::new(&manifest.source_job_dir)
        .try_exists()
        .map_err(|error| {
            format!(
                "failed to inspect completed quarantine source {}: {error}",
                manifest.source_job_dir
            )
        })?
    {
        return Err(format!(
            "completed quarantine source unexpectedly exists: {}",
            manifest.source_job_dir
        ));
    }
    verify_shell_job_quarantine_manifest_artifacts(destination, &manifest)?;
    validate_quarantine_recovery_records(destination, &manifest)?;
    let _ = remove_stale_atomic_staging_files(destination, completion_name)?;
    let mut expected_names = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect::<HashSet<_>>();
    if !expected_names.insert(completion.manifest_file_name)
        || !expected_names.insert(completion_name.to_owned())
    {
        return Err(format!(
            "quarantine metadata names collide with original artifacts at {}",
            destination.display()
        ));
    }
    verify_shell_job_quarantine_exact_file_set(destination, &expected_names)
}

fn reconcile_incomplete_quarantine_directory(destination: &Path) -> Result<String, String> {
    let manifests = shell_job_recovery_record_paths(destination, "quarantine-manifest-")?;
    if manifests.len() != 1 {
        return Err(format!(
            "incomplete quarantine directory {} has {} committed manifests; exactly one is required",
            destination.display(),
            manifests.len()
        ));
    }
    let manifest_path = &manifests[0];
    let manifest_name = manifest_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "incomplete quarantine manifest filename is not UTF-8".to_owned())?
        .to_owned();
    let manifest_bytes = fs::read(manifest_path).map_err(|error| {
        format!(
            "failed to read incomplete quarantine manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let manifest: ShellJobQuarantineManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| {
            format!(
                "failed to decode incomplete quarantine manifest {}: {error}",
                manifest_path.display()
            )
        })?;
    validate_shell_job_quarantine_manifest_structure(&manifest, destination, &manifest_name)?;
    if Path::new(&manifest.source_job_dir)
        .try_exists()
        .map_err(|error| {
            format!(
                "failed to inspect incomplete quarantine source {}: {error}",
                manifest.source_job_dir
            )
        })?
    {
        return Err(format!(
            "cannot reconcile incomplete quarantine while source still exists: {}",
            manifest.source_job_dir
        ));
    }
    verify_shell_job_quarantine_manifest_artifacts(destination, &manifest)?;
    validate_quarantine_recovery_records(destination, &manifest)?;
    let completion_name = format!("quarantine-complete-{}.json", manifest.recovery_id);
    let mut allowed_names = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect::<HashSet<_>>();
    allowed_names.insert(manifest_name.clone());
    for entry in fs::read_dir(destination).map_err(|error| {
        format!(
            "failed to enumerate incomplete quarantine staging files {}: {error}",
            destination.display()
        )
    })? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{completion_name}.tmp.")) {
            allowed_names.insert(name);
        }
    }
    verify_shell_job_quarantine_exact_file_set(destination, &allowed_names)?;
    let _ = remove_stale_atomic_staging_files(destination, &completion_name)?;
    let completion = ShellJobQuarantineCompletion {
        schema_version: manifest.schema_version,
        recovery_id: manifest.recovery_id.clone(),
        job_id: manifest.job_id.clone(),
        completed_at: chrono::Utc::now().to_rfc3339(),
        quarantine_job_dir: path_string(destination),
        manifest_file_name: manifest_name.clone(),
        manifest_sha256: sha256_hex(&manifest_bytes),
        original_artifact_count: manifest.original_artifact_count,
        original_artifact_bytes: manifest.original_artifact_bytes,
        pre_recovery_artifact_count: manifest.pre_recovery_artifact_count,
        pre_recovery_artifact_bytes: manifest.pre_recovery_artifact_bytes,
        recovery_generated_artifact_count: manifest.recovery_generated_artifact_count,
        recovery_generated_artifact_bytes: manifest.recovery_generated_artifact_bytes,
        remote_verdict: manifest.remote_verification.verdict.clone(),
    };
    persist_shell_job_quarantine_completion(
        destination,
        &manifest.recovery_id,
        &completion,
        &manifest,
        &manifest_name,
    )
}

fn verify_existing_shell_job_quarantine_store(quarantine_root: &Path) -> Result<(), String> {
    match quarantine_root.try_exists() {
        Ok(false) => return Ok(()),
        Ok(true) => {}
        Err(error) => {
            return Err(format!(
                "failed to inspect quarantine root existence {}: {error}",
                quarantine_root.display()
            ));
        }
    }
    let entries = fs::read_dir(quarantine_root).map_err(|error| {
        format!(
            "failed to enumerate quarantine root {}: {error}",
            quarantine_root.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read quarantine root entry under {}: {error}",
                quarantine_root.display()
            )
        })?;
        if !entry
            .file_type()
            .map_err(|error| format!("failed to classify {}: {error}", entry.path().display()))?
            .is_dir()
        {
            return Err(format!(
                "quarantine root contains a non-directory entry: {}",
                entry.path().display()
            ));
        }
        let destination = entry.path();
        let completions = shell_job_recovery_record_paths(&destination, "quarantine-complete-")?;
        let completion_name = match completions.as_slice() {
            [] => reconcile_incomplete_quarantine_directory(&destination)?,
            [path] => path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| "quarantine completion filename is not UTF-8".to_owned())?
                .to_owned(),
            _ => {
                return Err(format!(
                    "quarantine directory {} has {} completion records; at most one is allowed",
                    destination.display(),
                    completions.len()
                ));
            }
        };
        verify_completed_quarantine_directory(&destination, &completion_name)?;
    }
    Ok(())
}

fn push_shell_job_recovery_sample(sample: &mut Vec<String>, value: String) {
    if sample.len() < SHELL_JOB_RECOVERY_ID_SAMPLE_CAP {
        sample.push(value);
    }
}

fn recover_corrupt_shell_jobs_on_startup() -> Result<ShellJobCorruptRecoveryReadback, ErrorData> {
    let root = shell_durable_job_root_dir()?;
    let quarantine_root = shell_job_quarantine_root_dir()?;
    let mut readback = ShellJobCorruptRecoveryReadback {
        job_root: Some(path_string(&root)),
        quarantine_root: Some(path_string(&quarantine_root)),
        scanned_job_dirs: 0,
        retained_valid_status_jobs: 0,
        corrupt_status_jobs: 0,
        quarantined_jobs: 0,
        remote_state_verified_jobs: 0,
        retained_unverifiable_remote_jobs: 0,
        unexpected_job_root_entries: 0,
        skipped_concurrently_mutated: 0,
        recovery_failures: 0,
        bytes_quarantined: 0,
        quarantined_job_ids_sample: Vec::new(),
        retained_job_ids_sample: Vec::new(),
        unexpected_job_root_entries_sample: Vec::new(),
        quarantine_paths_sample: Vec::new(),
        manifest_paths_sample: Vec::new(),
    };
    if let Err(detail) = verify_existing_shell_job_quarantine_store(&quarantine_root) {
        return Err(shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            "startup corrupt shell-job recovery found incomplete or changed quarantine evidence",
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": quarantine_root,
                "reason": "startup_quarantine_evidence_unverified",
                "detail": detail,
            }),
        ));
    }
    match root.try_exists() {
        Ok(false) => return Ok(readback),
        Ok(true) => {}
        Err(error) => {
            return Err(shell_tool_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "startup corrupt shell-job recovery could not determine whether the job root exists: {error}"
                ),
                json!({
                    "code": error_codes::STORAGE_READ_FAILED,
                    "path": root,
                    "reason": "startup_corrupt_job_root_existence_read_failed",
                }),
            ));
        }
    }
    let entries = fs::read_dir(&root).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("startup corrupt shell-job recovery could not read job root: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": root,
                "reason": "startup_corrupt_job_root_read_failed",
            }),
        )
    })?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                readback.skipped_concurrently_mutated =
                    readback.skipped_concurrently_mutated.saturating_add(1);
                continue;
            }
            Err(error) => {
                readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                tracing::error!(
                    code = "M4_SHELL_JOB_STARTUP_CORRUPT_DIR_ENTRY_FAILED",
                    error = %error,
                    "startup corrupt shell-job recovery could not read a directory entry"
                );
                continue;
            }
        };
        let job_dir = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                push_shell_job_recovery_sample(
                    &mut readback.retained_job_ids_sample,
                    path_string(&job_dir),
                );
                tracing::error!(
                    code = "M4_SHELL_JOB_STARTUP_ROOT_ENTRY_CLASSIFY_FAILED",
                    path = %path_string(&job_dir),
                    error = %error,
                    "startup corrupt shell-job recovery could not classify a durable job-root entry"
                );
                continue;
            }
        };
        if !file_type.is_dir() {
            readback.unexpected_job_root_entries =
                readback.unexpected_job_root_entries.saturating_add(1);
            push_shell_job_recovery_sample(
                &mut readback.unexpected_job_root_entries_sample,
                path_string(&job_dir),
            );
            tracing::error!(
                code = "M4_SHELL_JOB_STARTUP_UNEXPECTED_ROOT_ENTRY",
                path = %path_string(&job_dir),
                entry_kind = if file_type.is_symlink() { "symlink" } else if file_type.is_file() { "file" } else { "other" },
                reason = "durable_job_root_entries_must_be_job_directories",
                "startup retained an unexpected durable job-root entry and will refuse to serve requests"
            );
            continue;
        }
        let Some(job_id) = job_dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            readback.unexpected_job_root_entries =
                readback.unexpected_job_root_entries.saturating_add(1);
            push_shell_job_recovery_sample(
                &mut readback.unexpected_job_root_entries_sample,
                path_string(&job_dir),
            );
            tracing::error!(
                code = "M4_SHELL_JOB_STARTUP_UNEXPECTED_ROOT_ENTRY",
                path = %path_string(&job_dir),
                entry_kind = "directory",
                reason = "durable_job_directory_name_is_not_utf8",
                "startup retained an unexpected durable job-root entry and will refuse to serve requests"
            );
            continue;
        };
        if validate_shell_job_id(&job_id).is_err() {
            readback.unexpected_job_root_entries =
                readback.unexpected_job_root_entries.saturating_add(1);
            push_shell_job_recovery_sample(
                &mut readback.unexpected_job_root_entries_sample,
                path_string(&job_dir),
            );
            tracing::error!(
                code = "M4_SHELL_JOB_STARTUP_UNEXPECTED_ROOT_ENTRY",
                path = %path_string(&job_dir),
                entry_kind = "directory",
                reason = "durable_job_directory_name_is_invalid",
                "startup retained an unexpected durable job-root entry and will refuse to serve requests"
            );
            continue;
        }
        readback.scanned_job_dirs = readback.scanned_job_dirs.saturating_add(1);
        let paths = shell_job_paths_from_root(&root, &job_id);
        let status_read_error = match read_shell_job_status(&paths.status_path, &job_id) {
            Ok(_) => {
                readback.retained_valid_status_jobs =
                    readback.retained_valid_status_jobs.saturating_add(1);
                continue;
            }
            Err(error) => {
                let reason = error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("reason"))
                    .and_then(Value::as_str);
                let Some(reason) = reason else {
                    readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                    push_shell_job_recovery_sample(
                        &mut readback.retained_job_ids_sample,
                        job_id.clone(),
                    );
                    tracing::error!(
                        code = "M4_SHELL_JOB_STARTUP_STATUS_ERROR_REASON_MISSING",
                        job_id,
                        status_path = %path_string(&paths.status_path),
                        detail = %error.message,
                        data = ?error.data,
                        "startup retained a shell-job directory because status failure classification evidence was missing"
                    );
                    continue;
                };
                if reason != "job_status_decode_failed" {
                    readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                    push_shell_job_recovery_sample(
                        &mut readback.retained_job_ids_sample,
                        job_id.clone(),
                    );
                    tracing::error!(
                        code = "M4_SHELL_JOB_STARTUP_STATUS_READ_UNCERTAIN",
                        job_id,
                        status_path = %path_string(&paths.status_path),
                        reason,
                        detail = %error.message,
                        data = ?error.data,
                        "startup retained a shell-job directory and will refuse to serve because status I/O/missing uncertainty is not confirmed JSON corruption"
                    );
                    continue;
                }
                error.message.to_string()
            }
        };
        readback.corrupt_status_jobs = readback.corrupt_status_jobs.saturating_add(1);
        let existing_manifest = match read_existing_source_quarantine_manifest(
            &job_dir,
            &job_id,
            &quarantine_root,
        ) {
            Ok(manifest) => manifest,
            Err(detail) => {
                readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                push_shell_job_recovery_sample(
                    &mut readback.retained_job_ids_sample,
                    job_id.clone(),
                );
                tracing::error!(
                    code = "M4_SHELL_JOB_STARTUP_SOURCE_MANIFEST_UNVERIFIED",
                    job_id,
                    path = %path_string(&job_dir),
                    detail,
                    "startup retained a corrupt shell-job directory because its committed recovery manifest could not be resumed"
                );
                continue;
            }
        };
        let (manifest, manifest_file_name, manifest_sha256) = if let Some(existing) =
            existing_manifest
        {
            existing
        } else {
            let recovery_id = match existing_remote_recovery_id(&paths, &job_id) {
                Ok(Some(recovery_id)) => recovery_id,
                Ok(None) => new_reflex_id(),
                Err(detail) => {
                    readback.retained_unverifiable_remote_jobs =
                        readback.retained_unverifiable_remote_jobs.saturating_add(1);
                    push_shell_job_recovery_sample(
                        &mut readback.retained_job_ids_sample,
                        job_id.clone(),
                    );
                    tracing::error!(
                        code = "M4_SHELL_JOB_STARTUP_RECOVERY_ID_UNVERIFIED",
                        job_id,
                        detail,
                        "startup retained a corrupt shell-job directory because durable remote recovery records were inconsistent"
                    );
                    continue;
                }
            };
            let destination = quarantine_root.join(format!("{job_id}-{recovery_id}"));
            let remote_verification = match verify_corrupt_shell_job_remote_state(
                &paths,
                &job_id,
                &recovery_id,
                &destination,
            ) {
                Ok(verification) => verification,
                Err(detail) => {
                    readback.retained_unverifiable_remote_jobs =
                        readback.retained_unverifiable_remote_jobs.saturating_add(1);
                    push_shell_job_recovery_sample(
                        &mut readback.retained_job_ids_sample,
                        job_id.clone(),
                    );
                    tracing::error!(
                        code = "M4_SHELL_JOB_STARTUP_CORRUPT_REMOTE_UNVERIFIED",
                        job_id,
                        status_path = %path_string(&paths.status_path),
                        status_read_error,
                        detail,
                        recovery_id,
                        "startup retained a corrupt shell-job directory because remote process state could not be verified"
                    );
                    continue;
                }
            };
            let artifacts = match shell_job_quarantine_artifacts(&job_dir) {
                Ok(artifacts) => artifacts,
                Err(detail) => {
                    readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                    push_shell_job_recovery_sample(
                        &mut readback.retained_job_ids_sample,
                        job_id.clone(),
                    );
                    tracing::error!(
                        code = "M4_SHELL_JOB_STARTUP_CORRUPT_INVENTORY_FAILED",
                        job_id,
                        path = %path_string(&job_dir),
                        status_read_error,
                        detail,
                        "startup retained a corrupt shell-job directory because its evidence inventory could not be read"
                    );
                    continue;
                }
            };
            let (
                pre_recovery_artifact_count,
                pre_recovery_artifact_bytes,
                recovery_generated_artifact_count,
                recovery_generated_artifact_bytes,
            ) = match shell_job_quarantine_artifact_accounting(&artifacts) {
                Ok(accounting) => accounting,
                Err(detail) => {
                    readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                    push_shell_job_recovery_sample(
                        &mut readback.retained_job_ids_sample,
                        job_id.clone(),
                    );
                    tracing::error!(
                        code = "M4_SHELL_JOB_STARTUP_ARTIFACT_ACCOUNTING_FAILED",
                        job_id,
                        detail,
                        "startup retained a corrupt shell-job directory because artifact evidence accounting overflowed"
                    );
                    continue;
                }
            };
            let Some(original_artifact_bytes) =
                pre_recovery_artifact_bytes.checked_add(recovery_generated_artifact_bytes)
            else {
                readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                push_shell_job_recovery_sample(
                    &mut readback.retained_job_ids_sample,
                    job_id.clone(),
                );
                tracing::error!(
                    code = "M4_SHELL_JOB_STARTUP_ARTIFACT_TOTAL_OVERFLOW",
                    job_id,
                    pre_recovery_artifact_bytes,
                    recovery_generated_artifact_bytes,
                    "startup retained a corrupt shell-job directory because total artifact evidence bytes overflowed"
                );
                continue;
            };
            let manifest = ShellJobQuarantineManifest {
                schema_version: SHELL_JOB_QUARANTINE_MANIFEST_SCHEMA_VERSION,
                recovery_id: recovery_id.clone(),
                job_id: job_id.clone(),
                quarantined_at: chrono::Utc::now().to_rfc3339(),
                reason: "startup_unreadable_status_after_prior_daemon_exit".to_owned(),
                startup_safety_boundary:
                    "canonical_shell_job_store_lifetime_lock_held_before_daemon_accepts_requests"
                        .to_owned(),
                source_job_dir: path_string(&job_dir),
                quarantine_job_dir: path_string(&destination),
                status_read_error: status_read_error.clone(),
                original_artifact_count: artifacts.len(),
                original_artifact_bytes,
                pre_recovery_artifact_count,
                pre_recovery_artifact_bytes,
                recovery_generated_artifact_count,
                recovery_generated_artifact_bytes,
                artifacts,
                remote_verification,
            };
            let expected_manifest_name = format!("quarantine-manifest-{recovery_id}.json");
            if let Err(detail) = validate_shell_job_quarantine_manifest_structure(
                &manifest,
                &destination,
                &expected_manifest_name,
            ) {
                readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                push_shell_job_recovery_sample(
                    &mut readback.retained_job_ids_sample,
                    job_id.clone(),
                );
                tracing::error!(
                    code = "M4_SHELL_JOB_STARTUP_QUARANTINE_MANIFEST_INVALID",
                    job_id,
                    detail,
                    "startup retained a corrupt shell-job directory because its proposed manifest was inconsistent"
                );
                continue;
            }
            let (manifest_source_path, manifest_sha256) =
                match persist_shell_job_quarantine_manifest(
                    &job_dir,
                    &job_id,
                    &recovery_id,
                    &manifest,
                ) {
                    Ok(readback) => readback,
                    Err(detail) => {
                        readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                        push_shell_job_recovery_sample(
                            &mut readback.retained_job_ids_sample,
                            job_id.clone(),
                        );
                        tracing::error!(
                            code = "M4_SHELL_JOB_STARTUP_QUARANTINE_MANIFEST_FAILED",
                            job_id,
                            path = %path_string(&job_dir),
                            detail,
                            "startup retained a corrupt shell-job directory because its quarantine manifest was not durable"
                        );
                        continue;
                    }
                };
            let Some(manifest_file_name) = manifest_source_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
            else {
                readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                push_shell_job_recovery_sample(
                    &mut readback.retained_job_ids_sample,
                    job_id.clone(),
                );
                tracing::error!(
                    code = "M4_SHELL_JOB_STARTUP_MANIFEST_FILENAME_INVALID",
                    job_id,
                    path = %path_string(&manifest_source_path),
                    "startup retained a corrupt shell-job directory because the committed manifest filename was not UTF-8"
                );
                continue;
            };
            (manifest, manifest_file_name, manifest_sha256)
        };
        let recovery_id = manifest.recovery_id.clone();
        let destination = PathBuf::from(&manifest.quarantine_job_dir);
        let original_artifact_bytes = manifest.original_artifact_bytes;
        // The committed source manifest above is the crash-recovery checkpoint.
        // Keep the destination store physically untouched until every fail-closed
        // verification succeeds; a root-create/rename failure is resumed from
        // that source manifest on the next startup pass.
        if let Err(error) = fs::create_dir_all(&quarantine_root) {
            readback.recovery_failures = readback.recovery_failures.saturating_add(1);
            push_shell_job_recovery_sample(&mut readback.retained_job_ids_sample, job_id.clone());
            tracing::error!(
                code = "M4_SHELL_JOB_STARTUP_QUARANTINE_ROOT_CREATE_FAILED",
                job_id,
                quarantine_root = %path_string(&quarantine_root),
                error = %error,
                "startup retained a corrupt shell-job directory because quarantine root creation failed"
            );
            continue;
        }
        match rename_shell_job_dir_to_quarantine(&job_dir, &destination) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                readback.skipped_concurrently_mutated =
                    readback.skipped_concurrently_mutated.saturating_add(1);
                continue;
            }
            Err(error) => {
                readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                push_shell_job_recovery_sample(
                    &mut readback.retained_job_ids_sample,
                    job_id.clone(),
                );
                tracing::error!(
                    code = "M4_SHELL_JOB_STARTUP_QUARANTINE_RENAME_FAILED",
                    job_id,
                    source = %path_string(&job_dir),
                    destination = %path_string(&destination),
                    manifest_sha256,
                    error = %error,
                    "startup retained a corrupt shell-job directory because the atomic quarantine rename failed"
                );
                continue;
            }
        }
        if let Err(detail) = verify_shell_job_quarantine_readback(
            &job_dir,
            &destination,
            &manifest_file_name,
            &manifest,
        ) {
            readback.recovery_failures = readback.recovery_failures.saturating_add(1);
            tracing::error!(
                code = "M4_SHELL_JOB_STARTUP_QUARANTINE_READBACK_FAILED",
                job_id,
                source = %path_string(&job_dir),
                destination = %path_string(&destination),
                manifest_sha256,
                detail,
                "startup quarantine move completed but physical evidence readback failed"
            );
            continue;
        }
        let completion = ShellJobQuarantineCompletion {
            schema_version: manifest.schema_version,
            recovery_id: recovery_id.clone(),
            job_id: job_id.clone(),
            completed_at: chrono::Utc::now().to_rfc3339(),
            quarantine_job_dir: path_string(&destination),
            manifest_file_name: manifest_file_name.clone(),
            manifest_sha256: manifest_sha256.clone(),
            original_artifact_count: manifest.original_artifact_count,
            original_artifact_bytes,
            pre_recovery_artifact_count: manifest.pre_recovery_artifact_count,
            pre_recovery_artifact_bytes: manifest.pre_recovery_artifact_bytes,
            recovery_generated_artifact_count: manifest.recovery_generated_artifact_count,
            recovery_generated_artifact_bytes: manifest.recovery_generated_artifact_bytes,
            remote_verdict: manifest.remote_verification.verdict.clone(),
        };
        let completion_file_name = match persist_shell_job_quarantine_completion(
            &destination,
            &recovery_id,
            &completion,
            &manifest,
            &manifest_file_name,
        ) {
            Ok(file_name) => file_name,
            Err(detail) => {
                readback.recovery_failures = readback.recovery_failures.saturating_add(1);
                tracing::error!(
                    code = "M4_SHELL_JOB_STARTUP_QUARANTINE_COMPLETION_FAILED",
                    job_id,
                    destination = %path_string(&destination),
                    manifest_sha256,
                    detail,
                    "startup quarantine move is retained, but startup will fail because durable completion evidence could not be verified"
                );
                continue;
            }
        };
        readback.quarantined_jobs = readback.quarantined_jobs.saturating_add(1);
        if manifest.remote_verification.sidecar_present {
            readback.remote_state_verified_jobs =
                readback.remote_state_verified_jobs.saturating_add(1);
        }
        readback.bytes_quarantined = readback
            .bytes_quarantined
            .checked_add(original_artifact_bytes)
            .ok_or_else(|| {
                shell_tool_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "startup corrupt shell-job recovery byte accounting overflowed after a verified quarantine move",
                    json!({
                        "code": error_codes::TOOL_INTERNAL_ERROR,
                        "reason": "startup_quarantine_byte_accounting_overflow",
                        "job_id": job_id,
                        "bytes_before": readback.bytes_quarantined,
                        "bytes_added": original_artifact_bytes,
                        "quarantine_path": destination,
                    }),
                )
            })?;
        push_shell_job_recovery_sample(&mut readback.quarantined_job_ids_sample, job_id.clone());
        push_shell_job_recovery_sample(
            &mut readback.quarantine_paths_sample,
            path_string(&destination),
        );
        push_shell_job_recovery_sample(
            &mut readback.manifest_paths_sample,
            path_string(&destination.join(&manifest_file_name)),
        );
        tracing::warn!(
            code = "M4_SHELL_JOB_STARTUP_CORRUPT_QUARANTINED",
            job_id,
            source = %path_string(&job_dir),
            destination = %path_string(&destination),
            manifest = %path_string(&destination.join(&manifest_file_name)),
            completion = %path_string(&destination.join(&completion_file_name)),
            manifest_sha256,
            original_artifact_count = manifest.original_artifact_count,
            original_artifact_bytes,
            remote_verdict = %manifest.remote_verification.verdict,
            "readback=startup_corrupt_shell_job after=atomic_quarantine_move_and_artifact_hash_verification"
        );
    }
    tracing::info!(
        code = "M4_SHELL_JOB_STARTUP_CORRUPT_RECOVERY",
        job_root = ?readback.job_root,
        quarantine_root = ?readback.quarantine_root,
        scanned_job_dirs = readback.scanned_job_dirs,
        retained_valid_status_jobs = readback.retained_valid_status_jobs,
        corrupt_status_jobs = readback.corrupt_status_jobs,
        quarantined_jobs = readback.quarantined_jobs,
        remote_state_verified_jobs = readback.remote_state_verified_jobs,
        retained_unverifiable_remote_jobs = readback.retained_unverifiable_remote_jobs,
        unexpected_job_root_entries = readback.unexpected_job_root_entries,
        skipped_concurrently_mutated = readback.skipped_concurrently_mutated,
        recovery_failures = readback.recovery_failures,
        bytes_quarantined = readback.bytes_quarantined,
        "readback=startup_corrupt_shell_job after=job_root_and_quarantine_root_scan"
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

/// Required corrupt-job recovery gate plus best-effort stale-job retention for
/// daemon startup. A corrupt directory whose evidence disposition or remote
/// state cannot be proved prevents the daemon from accepting requests. Once
/// that gate is complete, ordinary TTL housekeeping remains best-effort and is
/// retried on later session teardown. Called once from each daemon entry point
/// after the canonical shell-job-store lifetime lock/root freeze and before a
/// transport is exposed.
pub fn reap_stale_shell_jobs_on_startup() -> Result<ShellJobCorruptRecoveryReadback, ErrorData> {
    let corrupt_readback = match recover_corrupt_shell_jobs_on_startup() {
        Ok(readback) => readback,
        Err(error) => {
            tracing::error!(
                code = "M4_SHELL_JOB_STARTUP_CORRUPT_RECOVERY_FAILED",
                detail = %error.message,
                data = ?error.data,
                "daemon startup corrupt shell-job recovery failed before a complete root readback"
            );
            return Err(error);
        }
    };
    if corrupt_readback.retained_unverifiable_remote_jobs > 0
        || corrupt_readback.unexpected_job_root_entries > 0
        || corrupt_readback.skipped_concurrently_mutated > 0
        || corrupt_readback.recovery_failures > 0
    {
        tracing::error!(
            code = "M4_SHELL_JOB_STARTUP_CORRUPT_RECOVERY_INCOMPLETE",
            corrupt_status_jobs = corrupt_readback.corrupt_status_jobs,
            quarantined_jobs = corrupt_readback.quarantined_jobs,
            retained_unverifiable_remote_jobs = corrupt_readback.retained_unverifiable_remote_jobs,
            unexpected_job_root_entries = corrupt_readback.unexpected_job_root_entries,
            skipped_concurrently_mutated = corrupt_readback.skipped_concurrently_mutated,
            recovery_failures = corrupt_readback.recovery_failures,
            retained_job_ids_sample = ?corrupt_readback.retained_job_ids_sample,
            unexpected_job_root_entries_sample = ?corrupt_readback.unexpected_job_root_entries_sample,
            "refusing daemon startup because one or more durable shell-job evidence dispositions could not be verified"
        );
        let code = if corrupt_readback.retained_unverifiable_remote_jobs > 0 {
            error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED
        } else {
            error_codes::STORAGE_WRITE_FAILED
        };
        return Err(shell_tool_error(
            code,
            "daemon startup refused: corrupt durable shell-job recovery is incomplete",
            json!({
                "code": code,
                "reason": "startup_corrupt_shell_job_recovery_incomplete",
                "readback": corrupt_readback,
            }),
        ));
    }
    tracing::info!(
        code = "M4_SHELL_JOB_STARTUP_CORRUPT_RECOVERY_COMPLETE",
        scanned_job_dirs = corrupt_readback.scanned_job_dirs,
        corrupt_status_jobs = corrupt_readback.corrupt_status_jobs,
        quarantined_jobs = corrupt_readback.quarantined_jobs,
        remote_state_verified_jobs = corrupt_readback.remote_state_verified_jobs,
        bytes_quarantined = corrupt_readback.bytes_quarantined,
        "daemon startup completed corrupt durable shell-job recovery"
    );
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
    Ok(corrupt_readback)
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
    launch_for_session_with_boundary(config, params, session_id, &allow_physical_mutation).await
}

fn cleanup_launched_process_after_boundary(error: ErrorData, pid: u32) -> ErrorData {
    let cleanup = terminate_owned_process_tree(pid);
    let cleanup_verified = cleanup.remaining_process_ids.is_empty();
    if !cleanup_verified {
        synapse_action::record_operator_panic_safety_incident();
    }
    physical_mutation_boundary_error(
        error,
        "act_launch_operator_panic_cleanup",
        json!({
            "source_of_truth": "exact launched process tree + separate process-table readback",
            "pid": pid,
            "termination": cleanup,
            "cleanup_verified": cleanup_verified,
        }),
    )
}

fn ensure_launched_process_mutation_boundary(
    boundary: &PhysicalMutationBoundary<'_>,
    stage: &'static str,
    pid: u32,
) -> Result<(), ErrorData> {
    boundary(stage).map_err(|error| cleanup_launched_process_after_boundary(error, pid))
}

pub(crate) async fn launch_for_session_with_boundary(
    config: &M4ServiceConfig,
    params: ActLaunchParams,
    session_id: Option<&str>,
    boundary: &PhysicalMutationBoundary<'_>,
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
    boundary("act_launch_immediately_before_create_process")?;
    let spawned = spawn_launch_child(&spawn_params, launch_desktop)?;
    let pid = spawned.pid;
    ensure_launched_process_mutation_boundary(
        boundary,
        "act_launch_immediately_after_create_process",
        pid,
    )?;
    let cdp = if let Some(launch) = &cdp_launch {
        let cdp = await_physical_mutation_boundary(
            boundary,
            "act_launch_while_resolving_cdp",
            resolve_launched_cdp_port(pid, launch),
        )
        .await
        .map_err(|error| cleanup_launched_process_after_boundary(error, pid))?;
        ensure_launched_process_mutation_boundary(boundary, "act_launch_after_resolving_cdp", pid)?;
        cdp
    } else {
        LaunchedCdp::default()
    };
    let cdp_target = await_physical_mutation_boundary(
        boundary,
        "act_launch_while_verifying_chromium_url",
        verify_launched_chromium_url(&params, cdp_launch.as_ref(), &cdp, params.timeout_ms),
    )
    .await
    .map_err(|error| cleanup_launched_process_after_boundary(error, pid))?;
    ensure_launched_process_mutation_boundary(
        boundary,
        "act_launch_after_verifying_chromium_url",
        pid,
    )?;
    let cdp_target =
        cdp_target.map_err(|error| cleanup_launched_process_after_boundary(error, pid))?;
    let window = if let Some(regex) = wait_regex {
        if let Some(desktop_lease) = spawned.desktop_lease.as_ref() {
            let window = await_physical_mutation_boundary(
                boundary,
                "act_launch_while_waiting_for_desktop_window",
                wait_for_launch_desktop_window(
                    pid,
                    &regex,
                    params.timeout_ms,
                    &excluded_hwnds,
                    &launch_target_name,
                    &params.args,
                    desktop_lease.name().to_owned(),
                    desktop_lease.raw_handle_value(),
                ),
            )
            .await
            .map_err(|error| cleanup_launched_process_after_boundary(error, pid))?;
            ensure_launched_process_mutation_boundary(
                boundary,
                "act_launch_after_waiting_for_desktop_window",
                pid,
            )?;
            window.map_err(|error| cleanup_launched_process_after_boundary(error, pid))?
        } else {
            let window = await_physical_mutation_boundary(
                boundary,
                "act_launch_while_waiting_for_window",
                wait_for_launch_window(
                    pid,
                    &regex,
                    params.timeout_ms,
                    &excluded_hwnds,
                    &launch_target_name,
                    &params.args,
                ),
            )
            .await
            .map_err(|error| cleanup_launched_process_after_boundary(error, pid))?;
            ensure_launched_process_mutation_boundary(
                boundary,
                "act_launch_after_waiting_for_window",
                pid,
            )?;
            window.map_err(|error| cleanup_launched_process_after_boundary(error, pid))?
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
    ensure_launched_process_mutation_boundary(boundary, "act_launch_before_response", pid)?;
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
        search.hwnds.push(hwnd_to_wire(hwnd.0 as isize));
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

    let hwnd = HWND(hwnd_from_wire(hwnd)? as *mut core::ffi::c_void);
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
        hwnd: hwnd_to_wire(hwnd.0 as isize),
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
/// Agents are told to prefer `rg` for fast bounded manual FSV scans, but `rg` may be
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

// The daemon acquires an OS lock against a canonical shell-job root, then
// freezes that exact path here before recovery or request serving. Every
// production shell-job operation subsequently resolves through this value, so
// retargeting a configured symlink/junction cannot redirect writes into an
// unlocked store. Test-only thread-local overrides remain higher priority.
static SHELL_JOB_ROOT_FOR_DAEMON: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn freeze_shell_job_root_for_daemon(root: &Path) -> Result<(), ErrorData> {
    let canonical = fs::canonicalize(root).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_OPEN_FAILED,
            format!(
                "cannot freeze the guarded durable shell-job root {}: {error}",
                root.display()
            ),
            json!({
                "code": error_codes::STORAGE_OPEN_FAILED,
                "reason": "shell_job_guarded_root_canonicalize_failed",
                "root": root.display().to_string(),
                "detail": error.to_string(),
            }),
        )
    })?;
    if !canonical.is_absolute() {
        return Err(shell_tool_error(
            error_codes::STORAGE_OPEN_FAILED,
            "guarded durable shell-job root did not resolve to an absolute path",
            json!({
                "code": error_codes::STORAGE_OPEN_FAILED,
                "reason": "shell_job_guarded_root_not_absolute",
                "root": canonical.display().to_string(),
            }),
        ));
    }
    if let Some(frozen) = SHELL_JOB_ROOT_FOR_DAEMON.get() {
        if frozen == &canonical {
            return Ok(());
        }
        return Err(shell_tool_error(
            error_codes::STORAGE_OPEN_FAILED,
            "durable shell-job root was already frozen to a different path",
            json!({
                "code": error_codes::STORAGE_OPEN_FAILED,
                "reason": "shell_job_guarded_root_conflict",
                "frozen_root": frozen.display().to_string(),
                "requested_root": canonical.display().to_string(),
            }),
        ));
    }
    if let Err(candidate) = SHELL_JOB_ROOT_FOR_DAEMON.set(canonical) {
        let Some(frozen) = SHELL_JOB_ROOT_FOR_DAEMON.get() else {
            return Err(shell_tool_error(
                error_codes::STORAGE_OPEN_FAILED,
                "durable shell-job root freeze lost its initialized value",
                json!({
                    "code": error_codes::STORAGE_OPEN_FAILED,
                    "reason": "shell_job_guarded_root_freeze_state_missing",
                    "requested_root": candidate.display().to_string(),
                }),
            ));
        };
        if frozen != &candidate {
            return Err(shell_tool_error(
                error_codes::STORAGE_OPEN_FAILED,
                "durable shell-job root was concurrently frozen to a different path",
                json!({
                    "code": error_codes::STORAGE_OPEN_FAILED,
                    "reason": "shell_job_guarded_root_concurrent_conflict",
                    "frozen_root": frozen.display().to_string(),
                    "requested_root": candidate.display().to_string(),
                }),
            ));
        }
    }
    Ok(())
}

#[inline]
fn shell_job_root_override() -> Option<PathBuf> {
    None
}

pub(crate) fn shell_job_root_dir() -> Result<PathBuf, ErrorData> {
    if let Some(override_root) = shell_job_root_override() {
        return Ok(override_root);
    }
    if let Some(frozen_root) = SHELL_JOB_ROOT_FOR_DAEMON.get() {
        return Ok(frozen_root.clone());
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

fn write_shell_remote_cleanup_invocation(
    paths: &ShellJobPaths,
    invocation: Option<&ShellRemoteCleanupInvocation>,
) -> Result<(), ErrorData> {
    let Some(invocation) = invocation else {
        return Ok(());
    };
    write_pretty_json_file(&paths.remote_cleanup_path, &invocation, "remote cleanup")
}

fn seed_shell_job_remote_ownership(
    job: &mut ActRunShellJobStatus,
    invocation: Option<&ShellRemoteCleanupInvocation>,
) {
    let Some(token) = invocation.and_then(|invocation| invocation.ownership_token.as_ref()) else {
        return;
    };
    job.remote_process_scope.remote_ownership_token = Some(token.clone());
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!(
            "remote_ownership_sidecar:token_sha256={}",
            sha256_hex(token.as_bytes())
        ),
    );
}

fn ssh_effective_config_readback(
    command: &str,
    control_args: &[String],
) -> Result<SshEffectiveConfigReadback, String> {
    let mut args = Vec::with_capacity(control_args.len() + 1);
    args.push("-G".to_owned());
    args.extend_from_slice(control_args);
    let readback = run_shell_cleanup_command_with_timeout(
        command,
        &args,
        Duration::from_millis(SHELL_SSH_CONFIG_PREFLIGHT_TIMEOUT_MS),
    )?;
    if readback.exit_code != Some(0) || readback.stdout_byte_len == 0 {
        return Err(format!(
            "ssh -G preflight failed; exit={:?}; stdout_bytes={}; stdout_sha256={}; stderr_bytes={}; stderr_sha256={}; stderr_excerpt={:?}",
            readback.exit_code,
            readback.stdout_byte_len,
            readback.stdout_sha256,
            readback.stderr_byte_len,
            readback.stderr_sha256,
            shell_cleanup_output_excerpt(&readback.stderr),
        ));
    }
    Ok(SshEffectiveConfigReadback {
        fingerprint: SshEffectiveConfigFingerprint {
            byte_len: readback.stdout_byte_len,
            sha256: readback.stdout_sha256,
        },
    })
}

fn read_shell_remote_cleanup_invocation(
    paths: &ShellJobPaths,
    job_id: &str,
) -> Result<Option<ShellRemoteCleanupInvocation>, String> {
    match paths.remote_cleanup_path.try_exists() {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(error) => {
            return Err(format!(
                "failed to inspect remote cleanup sidecar existence for {job_id}: {error}"
            ));
        }
    }
    let bytes = fs::read(&paths.remote_cleanup_path)
        .map_err(|error| format!("failed to read remote cleanup sidecar for {job_id}: {error}"))?;
    let invocation: ShellRemoteCleanupInvocation =
        serde_json::from_slice(&bytes).map_err(|error| {
            format!("failed to decode remote cleanup sidecar for {job_id}: {error}")
        })?;
    if !matches!(invocation.schema_version, 1..=4) {
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
    let trusted_command =
        trusted_ssh_automatic_replay_executable(&invocation.command).ok_or_else(|| {
            format!(
                "remote cleanup sidecar command is not a trusted SSH executable for {job_id}: {}",
                invocation.command
            )
        })?;
    if matches!(invocation.schema_version, 3 | 4)
        && (!Path::new(&invocation.command).is_absolute()
            || fs::canonicalize(&invocation.command).ok().as_ref() != Some(&trusted_command))
    {
        return Err(format!(
            "remote cleanup sidecar v3 does not bind the exact canonical SSH executable for {job_id}: recorded={} trusted={}",
            invocation.command,
            trusted_command.display()
        ));
    }
    let parts = ssh_direct_command_parts(&invocation.control_args).ok_or_else(|| {
        format!(
            "remote cleanup sidecar control_args do not contain an ssh destination for {job_id}"
        )
    })?;
    if parts.remote_command.is_some() {
        return Err(format!(
            "remote cleanup sidecar control_args unexpectedly contain a remote command for {job_id}"
        ));
    }
    if let Some(reason) = parts.tracking_unsupported_reason {
        return Err(format!(
            "remote cleanup sidecar control_args are unsafe for tracked replay for {job_id}: {reason}"
        ));
    }
    if let Some(option) = ssh_control_args_unsafe_for_automatic_replay(&invocation.control_args) {
        return Err(format!(
            "remote cleanup sidecar contains SSH control argv outside the automatic-replay allowlist for {job_id}: {option}"
        ));
    }
    if parts.remote_identity != invocation.remote_identity {
        return Err(format!(
            "remote cleanup sidecar identity differs from its parsed SSH destination for {job_id}: recorded={} parsed={}",
            invocation.remote_identity, parts.remote_identity
        ));
    }
    validate_lower_sha256(&invocation.args_sha256, "args_sha256", job_id)?;
    if matches!(invocation.schema_version, 2..=4) {
        let actual_control_args_sha256 = shell_args_sha256(&invocation.control_args);
        if actual_control_args_sha256 != invocation.args_sha256 {
            return Err(format!(
                "remote cleanup sidecar control argv digest differs for {job_id}: recorded={} actual={actual_control_args_sha256}",
                invocation.args_sha256
            ));
        }
        let request_args_sha256 = invocation.request_args_sha256.as_deref().ok_or_else(|| {
            format!(
                "remote cleanup sidecar v{} lacks request_args_sha256 for {job_id}",
                invocation.schema_version
            )
        })?;
        validate_lower_sha256(request_args_sha256, "request_args_sha256", job_id)?;
        let request_bytes = fs::read(&paths.request_path).map_err(|error| {
            format!(
                "failed to read request JSON while validating remote cleanup sidecar for {job_id}: {error}"
            )
        })?;
        let request: Value = serde_json::from_slice(&request_bytes).map_err(|error| {
            format!(
                "failed to decode request JSON while validating remote cleanup sidecar for {job_id}: {error}"
            )
        })?;
        let recorded_request_args_sha256 = request
            .get("args_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("request JSON lacks args_sha256 for {job_id}"))?;
        if recorded_request_args_sha256 != request_args_sha256 {
            return Err(format!(
                "remote cleanup sidecar is not bound to request argv for {job_id}: sidecar={request_args_sha256} request={recorded_request_args_sha256}"
            ));
        }
    }
    if matches!(invocation.schema_version, 3 | 4) {
        let effective_control_args =
            invocation.effective_control_args.as_ref().ok_or_else(|| {
                format!(
                    "remote cleanup sidecar v{} lacks effective_control_args for {job_id}",
                    invocation.schema_version
                )
            })?;
        let effective_args_sha256 =
            invocation.effective_args_sha256.as_deref().ok_or_else(|| {
                format!(
                    "remote cleanup sidecar v{} lacks effective_args_sha256 for {job_id}",
                    invocation.schema_version
                )
            })?;
        validate_lower_sha256(effective_args_sha256, "effective_args_sha256", job_id)?;
        let actual_effective_args_sha256 = shell_args_sha256(effective_control_args);
        if actual_effective_args_sha256 != effective_args_sha256 {
            return Err(format!(
                "remote cleanup sidecar effective argv digest differs for {job_id}: recorded={effective_args_sha256} actual={actual_effective_args_sha256}"
            ));
        }
        let current_policy_args = hardened_ssh_automatic_replay_args(&invocation.control_args)
            .map_err(|reason| {
                format!(
                    "remote cleanup sidecar v{} controls no longer satisfy replay policy for {job_id}: {reason}",
                    invocation.schema_version
                )
            })?;
        if current_policy_args != *effective_control_args {
            return Err(format!(
                "remote cleanup sidecar replay policy drifted for {job_id}; persisted effective argv digest={effective_args_sha256} current_policy_digest={}",
                shell_args_sha256(&current_policy_args)
            ));
        }
        if invocation.schema_version == 4 {
            let request_config = invocation
                .request_effective_config
                .as_ref()
                .ok_or_else(|| {
                    format!("remote cleanup sidecar v4 lacks request_effective_config for {job_id}")
                })?;
            validate_ssh_effective_config_fingerprint(
                request_config,
                "request_effective_config",
                job_id,
            )?;
            let mut isolated_request_args = Vec::with_capacity(invocation.control_args.len() + 2);
            isolated_request_args.push("-F".to_owned());
            isolated_request_args.push(SSH_AUTOMATIC_REPLAY_DISABLED_CONFIG.to_owned());
            isolated_request_args.extend_from_slice(&invocation.control_args);
            let current_isolated_request =
                ssh_effective_config_readback(&invocation.command, &isolated_request_args)?;
            if current_isolated_request.fingerprint != *request_config {
                return Err(format!(
                    "remote cleanup sidecar request ssh_config fingerprint no longer matches config-isolated request argv for {job_id}: recorded={request_config:?} actual={:?}",
                    current_isolated_request.fingerprint
                ));
            }
            let cleanup_config = invocation
                .cleanup_effective_config
                .as_ref()
                .ok_or_else(|| {
                    format!("remote cleanup sidecar v4 lacks cleanup_effective_config for {job_id}")
                })?;
            validate_ssh_effective_config_fingerprint(
                cleanup_config,
                "cleanup_effective_config",
                job_id,
            )?;
            let current_cleanup =
                ssh_effective_config_readback(&invocation.command, effective_control_args)?;
            if current_cleanup.fingerprint != *cleanup_config {
                return Err(format!(
                    "remote cleanup sidecar effective ssh_config fingerprint drifted for {job_id}: recorded={cleanup_config:?} actual={:?}",
                    current_cleanup.fingerprint
                ));
            }
            let ownership_token = invocation.ownership_token.as_deref().ok_or_else(|| {
                format!("remote cleanup sidecar v4 lacks ownership_token for {job_id}")
            })?;
            if !valid_remote_ownership_token(ownership_token) {
                return Err(format!(
                    "remote cleanup sidecar v4 ownership_token is malformed for {job_id}"
                ));
            }
        } else if invocation.request_effective_config.is_some()
            || invocation.cleanup_effective_config.is_some()
            || invocation.ownership_token.is_some()
        {
            return Err(format!(
                "remote cleanup sidecar v{} unexpectedly carries v4 ssh_config fingerprints for {job_id}",
                invocation.schema_version
            ));
        }
    } else if invocation.effective_control_args.is_some()
        || invocation.effective_args_sha256.is_some()
        || invocation.request_effective_config.is_some()
        || invocation.cleanup_effective_config.is_some()
        || invocation.ownership_token.is_some()
    {
        return Err(format!(
            "legacy remote cleanup sidecar unexpectedly carries v3 effective replay fields for {job_id}"
        ));
    }
    Ok(Some(invocation))
}

fn validate_ssh_effective_config_fingerprint(
    value: &SshEffectiveConfigFingerprint,
    field: &str,
    job_id: &str,
) -> Result<(), String> {
    if value.byte_len == 0 {
        return Err(format!(
            "remote cleanup sidecar {field}.byte_len is zero for {job_id}"
        ));
    }
    validate_lower_sha256(&value.sha256, field, job_id)
}

fn validate_lower_sha256(value: &str, field: &str, job_id: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
    {
        return Ok(());
    }
    Err(format!(
        "remote cleanup sidecar {field} is not a lowercase SHA-256 digest for {job_id}"
    ))
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
    let tmp_path = shell_status_temp_path(path);
    if let Err(error) = write_shell_job_status_staging(&tmp_path, &bytes) {
        let staging_cleanup = cleanup_failed_atomic_staging_file(&tmp_path);
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_run_shell failed to durably stage shell job {role}: {error}; {staging_cleanup}"
            ),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "path": path,
                "tmp_path": tmp_path,
                "reason": "job_json_stage_failed",
                "role": role,
                "staging_cleanup": staging_cleanup,
            }),
        ));
    }
    if let Err(error) = commit_shell_job_status_file(&tmp_path, path, role) {
        let staging_cleanup = cleanup_failed_atomic_staging_file(&tmp_path);
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_run_shell failed to atomically commit shell job {role}: {error}; {staging_cleanup}"
            ),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "path": path,
                "tmp_path": tmp_path,
                "reason": "job_json_commit_failed",
                "role": role,
                "staging_cleanup": staging_cleanup,
            }),
        ));
    }
    let persisted = fs::read(path).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell failed to read back shell job {role}: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "path": path,
                "reason": "job_json_readback_failed",
                "role": role,
            }),
        )
    })?;
    if persisted != bytes {
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("act_run_shell shell job {role} readback differed after commit"),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "path": path,
                "expected_sha256": sha256_hex(&bytes),
                "actual_sha256": sha256_hex(&persisted),
                "reason": "job_json_readback_mismatch",
                "role": role,
            }),
        ));
    }
    Ok(())
}

fn write_shell_job_status(path: &Path, status: &ActRunShellJobStatus) -> Result<(), ErrorData> {
    let write_lock = shell_status_write_lock(path);
    let _write_guard = write_lock.lock().map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_run_shell status writer lock was poisoned for {}: {error}",
                path.display()
            ),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": status.job_id,
                "path": path,
                "reason": "job_status_writer_lock_poisoned",
            }),
        )
    })?;
    write_shell_job_status_locked(path, status).map(|_| ())
}

/// Commit and independently read back a status while the caller holds the
/// destination's [`shell_status_write_lock`]. Keeping this lock outside the
/// primitive lets reconciliation make its latest-terminal-wins decision and
/// commit as one indivisible state transition.
fn write_shell_job_status_locked(
    path: &Path,
    status: &ActRunShellJobStatus,
) -> Result<ActRunShellJobStatus, ErrorData> {
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
    let tmp_path = shell_status_temp_path(path);
    if let Err(error) = write_shell_job_status_staging(&tmp_path, &bytes) {
        // Never leak the partial staging file on the write/fsync failure path.
        let staging_cleanup = cleanup_failed_atomic_staging_file(&tmp_path);
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_run_shell failed to write shell job status temp file: {error}; {staging_cleanup}"
            ),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": tmp_path,
                "reason": "job_status_temp_write_failed",
                "staging_cleanup": staging_cleanup,
            }),
        ));
    }
    if let Err(error) = commit_shell_job_status_file(&tmp_path, path, &safe_status.job_id) {
        // The rename never happened, so the staging file is orphaned — remove it.
        let staging_cleanup = cleanup_failed_atomic_staging_file(&tmp_path);
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_run_shell failed to commit shell job status file: {error}; {staging_cleanup}"
            ),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": path,
                "tmp_path": tmp_path,
                "reason": "job_status_rename_failed",
                "staging_cleanup": staging_cleanup,
            }),
        ));
    }
    let persisted = fs::read(path).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell failed to read back committed shell job status: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "job_id": safe_status.job_id,
                "path": path,
                "reason": "job_status_commit_readback_failed",
            }),
        )
    })?;
    if persisted != bytes {
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            "act_run_shell committed status bytes differed on independent readback",
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": path,
                "reason": "job_status_commit_bytes_mismatch",
                "expected_sha256": sha256_hex(&bytes),
                "actual_sha256": sha256_hex(&persisted),
                "expected_bytes": bytes.len(),
                "actual_bytes": persisted.len(),
            }),
        ));
    }
    let decoded: ActRunShellJobStatus = serde_json::from_slice(&persisted).map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!("act_run_shell committed status readback did not decode: {error}"),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "job_id": safe_status.job_id,
                "path": path,
                "reason": "job_status_commit_decode_failed",
                "sha256": sha256_hex(&persisted),
            }),
        )
    })?;
    if decoded != safe_status {
        return Err(shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            "act_run_shell committed status structured readback differed",
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": safe_status.job_id,
                "path": path,
                "reason": "job_status_commit_structured_mismatch",
                "sha256": sha256_hex(&persisted),
            }),
        ));
    }
    Ok(decoded)
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

fn cleanup_failed_atomic_staging_file(tmp_path: &Path) -> String {
    match fs::remove_file(tmp_path) {
        Ok(()) => format!("staging_cleanup=removed path={}", tmp_path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            format!("staging_cleanup=already_absent path={}", tmp_path.display())
        }
        Err(error) => format!(
            "staging_cleanup=failed path={} error={error}",
            tmp_path.display()
        ),
    }
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
    fs::rename(tmp_path, path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("atomic JSON destination has no parent directory"))?;
    sync_directory_entry_parent(parent)
}

#[cfg(not(windows))]
fn sync_directory_entry_parent(directory: &Path) -> io::Result<()> {
    fs::File::open(directory)?.sync_all()
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
    read_shell_status_bytes_with_retry_observer(path, |_| {})
}

#[cfg(windows)]
fn read_shell_status_bytes_with_retry_observer(
    path: &Path,
    mut before_retry: impl FnMut(u32),
) -> io::Result<Vec<u8>> {
    // Retry by ATTEMPT COUNT, not wall-clock — the same lesson the writer's
    // `commit_shell_job_status_file` already learned (#1568) and the reader had
    // not: under heavy CPU contention (a full parallel test suite, an AV sweep)
    // a wall-clock budget is spent while this thread is descheduled, so a 500 ms
    // window can yield only one or two real open attempts before "expiring" and
    // surfacing a spurious transient error to a status poll, cleanup scan, or
    // dashboard read (the confirmed class behind the #1608 multiwriter flake). A
    // fixed attempt count guarantees that many real retries regardless of
    // scheduling, with exponential backoff so a transient AV/indexer lock or an
    // in-flight atomic replace has escalating time to clear. Bounds mirror the
    // writer's move-retry: ~1s worst case, dominated by short early retries.
    const MAX_ATTEMPTS: u32 = 24;
    const BACKOFF_START_MS: u64 = 1;
    const BACKOFF_CAP_MS: u64 = 50;
    let mut backoff_ms = BACKOFF_START_MS;
    for attempt in 1..=MAX_ATTEMPTS {
        match read_status_file_share_delete(path) {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                // NOT_FOUND is overloaded: it is the legitimate "no such job"
                // signal, but it also fires transiently mid-replace when
                // `MoveFileExW(REPLACE_EXISTING)` has momentarily unlinked the
                // destination. It is retryable while a replace is plausibly in
                // flight, detected two ways so the check cannot race false at the
                // exact instant the replace lands (#1608): a writer's unique
                // `<name>.tmp.*` staging sibling is still on disk, OR the
                // destination itself now exists again — the tell that a
                // `MoveFileExW` completed between our failed open and this check
                // (its staging file already renamed away), so the very next open
                // will succeed. A genuinely-absent job matches neither predicate
                // and returns immediately with no added latency.
                let (replace_in_flight, destination_exists) = if error.kind()
                    == io::ErrorKind::NotFound
                {
                    let replace_in_flight = shell_status_replace_in_flight(path).map_err(
                            |inspection_error| {
                                io::Error::new(
                                    inspection_error.kind(),
                                    format!(
                                        "status open failed for {} ({error}); staging inspection failed: {inspection_error}",
                                        path.display()
                                    ),
                                )
                            },
                        )?;
                    let destination_exists = path.try_exists().map_err(|inspection_error| {
                            io::Error::new(
                                inspection_error.kind(),
                                format!(
                                    "status open failed for {} ({error}); destination existence inspection failed: {inspection_error}",
                                    path.display()
                                ),
                            )
                        })?;
                    (replace_in_flight, destination_exists)
                } else {
                    (false, false)
                };
                let retryable = shell_status_open_error_is_retryable(
                    error.kind(),
                    error.raw_os_error(),
                    replace_in_flight,
                    destination_exists,
                );
                if attempt < MAX_ATTEMPTS && retryable {
                    // Test coverage uses this synchronous observation point to
                    // land a real atomic replacement after the reader has
                    // classified NOT_FOUND as transient. Production supplies a
                    // no-op observer. This keeps the behavior test independent
                    // of thread scheduling without changing the retry contract.
                    before_retry(attempt);
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = backoff_ms.saturating_mul(2).min(BACKOFF_CAP_MS);
                    continue;
                }
                return Err(error);
            }
        }
    }
    // Unreachable: the final iteration returns `Err(error)` because
    // `attempt < MAX_ATTEMPTS` is false. Kept so the function has a total return.
    read_status_file_share_delete(path)
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
fn shell_status_replace_in_flight(path: &Path) -> io::Result<bool> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "status path has no parent for staging inspection: {}",
                path.display()
            ),
        )
    })?;
    let base = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "status path has no filename for staging inspection: {}",
                path.display()
            ),
        )
    })?;
    let base = base.to_string_lossy().into_owned();
    let prefix = format!("{base}.tmp.");
    let entries = fs::read_dir(dir).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to enumerate status staging directory {}: {error}",
                dir.display()
            ),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to read a status staging entry under {}: {error}",
                    dir.display()
                ),
            )
        })?;
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to classify status staging candidate {}: {error}",
                    entry.path().display()
                ),
            )
        })?;
        if !file_type.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "status staging candidate is not a regular file: {}",
                    entry.path().display()
                ),
            ));
        }
        return Ok(true);
    }
    Ok(false)
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
    if job.job_id != job_id {
        return Err(shell_tool_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "act_run_shell job status identity mismatch: requested/path job id {job_id}, persisted job id {}",
                job.job_id
            ),
            json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "job_id": job_id,
                "persisted_job_id": job.job_id,
                "path": path,
                "reason": "job_status_job_id_mismatch",
            }),
        ));
    }
    normalize_shell_job_remote_process_scope(&mut job);
    Ok(shell_job_status_with_safe_command_metadata(&job))
}

/// Persist a durable status and then perform a distinct store read to prove the
/// complete record that callers will subsequently observe. The lower-level
/// writer verifies its atomic byte commit while holding the writer lock; this
/// second read deliberately happens after that lock is released and validates
/// the public decode/normalization path as well.
fn persist_and_verify_shell_job_status(
    path: &Path,
    status: &ActRunShellJobStatus,
) -> Result<ActRunShellJobStatus, ShellJobStatusPersistenceFailure> {
    write_shell_job_status(path, status).map_err(|error| ShellJobStatusPersistenceFailure {
        error_code: error_codes::STORAGE_WRITE_FAILED,
        reason: "job_status_write_failed",
        detail: format!(
            "underlying_code={}; message={}",
            extract_error_code(&error),
            error.message
        ),
    })?;

    let readback = read_shell_job_status(path, &status.job_id).map_err(|error| {
        ShellJobStatusPersistenceFailure {
            error_code: error_codes::STORAGE_READ_FAILED,
            reason: "job_status_independent_readback_failed",
            detail: format!(
                "underlying_code={}; message={}",
                extract_error_code(&error),
                error.message
            ),
        }
    })?;
    let mut expected = shell_job_status_with_safe_command_metadata(status);
    normalize_shell_job_remote_process_scope(&mut expected);
    if readback != expected {
        return Err(ShellJobStatusPersistenceFailure {
            error_code: error_codes::STORAGE_READ_FAILED,
            reason: "job_status_independent_readback_mismatch",
            detail: format!("expected={expected:?}; actual={readback:?}"),
        });
    }
    Ok(readback)
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
        remote_boot_id: None,
        remote_process_start_time: None,
        remote_ownership_token: None,
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
fn trusted_ssh_automatic_replay_executable(command: &str) -> Option<PathBuf> {
    let mut candidates = windows_git_ssh_dir_candidates()
        .into_iter()
        .map(|dir| dir.join("ssh.exe"))
        .collect::<Vec<_>>();
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        candidates.push(
            PathBuf::from(system_root)
                .join("System32")
                .join("OpenSSH")
                .join("ssh.exe"),
        );
    }
    if is_bare_windows_executable_name(command)
        && ssh_family_client_for_executable(command) == Some("ssh")
    {
        return candidates
            .into_iter()
            .find_map(|candidate| fs::canonicalize(candidate).ok());
    }
    let actual = fs::canonicalize(command).ok()?;
    let actual = normalize_semicolon_path_part(&actual.to_string_lossy());
    candidates.into_iter().find_map(|candidate| {
        let candidate = fs::canonicalize(candidate).ok()?;
        (normalize_semicolon_path_part(&candidate.to_string_lossy()) == actual).then_some(candidate)
    })
}

#[cfg(not(windows))]
fn trusted_ssh_automatic_replay_executable(command: &str) -> Option<PathBuf> {
    let candidates = ["/usr/bin/ssh", "/bin/ssh", "/usr/local/bin/ssh"]
        .into_iter()
        .filter_map(|candidate| fs::canonicalize(candidate).ok())
        .collect::<Vec<_>>();
    if command == "ssh" {
        return candidates.into_iter().next();
    }
    let actual = fs::canonicalize(command).ok()?;
    candidates
        .into_iter()
        .find(|candidate| candidate == &actual)
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
    #[serde(default)]
    request_args_sha256: Option<String>,
    #[serde(default)]
    effective_control_args: Option<Vec<String>>,
    #[serde(default)]
    effective_args_sha256: Option<String>,
    /// Effective `ssh -G` configuration observed for the caller's original
    /// control argv after proving it is byte-identical with `-F none`. This is
    /// evidence that the initial target did not depend on mutable implicit
    /// ssh_config state.
    #[serde(default)]
    request_effective_config: Option<SshEffectiveConfigFingerprint>,
    /// Effective `ssh -G` configuration for the exact hardened cleanup argv.
    /// Recovery re-reads this fingerprint before it can use the sidecar.
    #[serde(default)]
    cleanup_effective_config: Option<SshEffectiveConfigFingerprint>,
    /// Raw correlation token retained only in the local durable sidecar. The
    /// remote guardian needs it in its environment for `/proc` identity
    /// verification, but markers expose only its SHA-256 digest and the
    /// payload environment explicitly removes it.
    #[serde(default)]
    ownership_token: Option<String>,
    created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SshEffectiveConfigFingerprint {
    byte_len: u64,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SshEffectiveConfigReadback {
    fingerprint: SshEffectiveConfigFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShellJobSpawnPlan {
    command: String,
    args: Vec<String>,
    remote_cleanup_invocation: Option<ShellRemoteCleanupInvocation>,
}

fn shell_job_spawn_plan(
    params: &ActRunShellStartParams,
    job_id: &str,
) -> Result<ShellJobSpawnPlan, ErrorData> {
    reject_new_durable_ssh_promotion(
        &params.command,
        &params.args,
        Some("durable_spawn_plan"),
        Some(job_id),
    )?;
    Ok(ShellJobSpawnPlan {
        command: params.command.clone(),
        args: params.args.clone(),
        remote_cleanup_invocation: None,
    })
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

fn reject_new_durable_ssh_promotion(
    command: &str,
    args: &[String],
    background_reason: Option<&str>,
    requested_job_id: Option<&str>,
) -> Result<(), ErrorData> {
    let Some(source_evidence) = durable_ssh_promotion_evidence(command, args) else {
        return Ok(());
    };
    Err(shell_tool_error(
        error_codes::ACTION_TARGET_INVALID,
        "durable SSH execution is refused because Synapse cannot preserve the inline SSH stdout/stderr, account-shell environment, and stdin contract while acquiring a remote cleanup handle; use bounded inline act_run_shell execution",
        json!({
            "code": error_codes::ACTION_TARGET_INVALID,
            "reason": "ssh_durable_semantic_preservation_unavailable",
            "remediation": "use_bounded_inline_execution",
            "recommended_tool": "act_run_shell",
            "recommended_execution_mode": "inline",
            "maximum_inline_timeout_ms": DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS,
            "background_reason": background_reason,
            "requested_job_id": requested_job_id,
            "source_evidence": source_evidence,
            "command": command,
            "args_sha256": shell_args_sha256(args),
            "no_child_spawned": true,
            "no_job_artifact_created": true,
        }),
    ))
}

fn durable_ssh_promotion_evidence(command: &str, args: &[String]) -> Option<String> {
    if let Some(invocation) = shell_job_ssh_command_invocation(command, args) {
        return Some(invocation.evidence.to_owned());
    }
    let shell = executable_leaf(command).to_ascii_lowercase();
    let (script, evidence) = match shell.as_str() {
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => (
            powershell_command_script_arg(args)?,
            "shell_wrapped_ssh:powershell_conservative_scan",
        ),
        "cmd" | "cmd.exe" => (
            cmd_command_script_arg(args)?,
            "shell_wrapped_ssh:cmd_conservative_scan",
        ),
        "sh" | "sh.exe" | "bash" | "bash.exe" | "zsh" | "zsh.exe" => (
            posix_shell_command_script_arg(args)?,
            "shell_wrapped_ssh:posix_shell_conservative_scan",
        ),
        _ => return None,
    };
    shell_script_starts_ssh_command(script).then(|| evidence.to_owned())
}

fn posix_shell_command_script_arg(args: &[String]) -> Option<&str> {
    args.windows(2).find_map(|pair| {
        matches!(trim_arg_quotes(&pair[0]), "-c" | "--command").then(|| pair[1].as_str())
    })
}

fn shell_script_starts_ssh_command(script: &str) -> bool {
    script
        .split([';', '|', '&', '{', '}', '(', ')', '\r', '\n'])
        .any(|segment| {
            let segment = segment.trim();
            let mut words = split_single_shell_command_words(segment).unwrap_or_else(|| {
                segment
                    .split_whitespace()
                    .next()
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect()
            });
            while words.first().is_some_and(|word| {
                matches!(
                    word.to_ascii_lowercase().as_str(),
                    "call" | "exec" | "command"
                )
            }) {
                words.remove(0);
            }
            words.first().is_some_and(|token| {
                let token = token.trim_matches(&['"', '\''][..]);
                ssh_family_client_for_executable(token) == Some("ssh")
            })
        })
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
            if let Some(remote_cleanup) = remote_cleanup {
                let original_parts = ssh_direct_command_parts(&invocation.args)?;
                let request_digest = shell_args_sha256(args);
                let canonical_original_command =
                    trusted_ssh_automatic_replay_executable(&invocation.command)
                        .map(|path| path.to_string_lossy().into_owned());
                if matches!(remote_cleanup.schema_version, 3 | 4)
                    && (remote_cleanup.request_args_sha256.as_deref()
                        != Some(request_digest.as_str())
                        || canonical_original_command.as_deref()
                            != Some(remote_cleanup.command.as_str())
                        || original_parts.control_args != remote_cleanup.control_args
                        || original_parts.remote_identity != remote_cleanup.remote_identity)
                {
                    return None;
                }
            }
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

const SHELL_REMOTE_GROUP_INSPECTION_FUNCTION_PY: &str = r#"import os
import sys

def live_process_ids_in_group(expected_pgid, excluded_pids=()):
    if expected_pgid <= 1:
        raise RuntimeError(f"invalid process group {expected_pgid}")
    excluded = set(excluded_pids)
    try:
        proc_entries = os.scandir("/proc")
    except OSError as error:
        raise RuntimeError(f"enumerate /proc failed: {error}") from error
    members = []
    with proc_entries:
        for entry in proc_entries:
            name = entry.name
            if not name.isascii() or not name.isdigit():
                continue
            candidate = int(name)
            if candidate in excluded:
                continue
            try:
                with open(f"/proc/{name}/stat", encoding="utf-8", errors="strict") as handle:
                    stat_line = handle.read().strip()
            except (FileNotFoundError, ProcessLookupError):
                continue
            except OSError as error:
                raise RuntimeError(f"read /proc/{name}/stat failed: {error}") from error
            _comm, separator, stat_tail = stat_line.rpartition(") ")
            if not separator:
                raise RuntimeError(f"/proc/{name}/stat has no command terminator")
            fields = stat_tail.split()
            if len(fields) < 3 or not fields[2].isascii() or not fields[2].isdigit():
                raise RuntimeError(f"/proc/{name}/stat has invalid process-group metadata")
            if int(fields[2]) == expected_pgid:
                members.append(candidate)
    return sorted(members)

def process_group_exists(expected_pgid):
    if expected_pgid <= 1:
        raise RuntimeError(f"invalid process group {expected_pgid}")
    try:
        os.kill(-expected_pgid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        # The group exists even when this account cannot inspect/signal one of
        # its members (for example after a payload changes uid under hidepid).
        return True
    except OSError as error:
        raise RuntimeError(f"probe process group {expected_pgid} failed: {error}") from error
    return True
"#;

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

/// SSH control arguments persisted for later automatic replay are a deliberately
/// small allowlist. A startup probe is a new privileged execution, not a replay
/// of arbitrary client behavior: any option that can execute local code, load
/// mutable configuration/providers, open a forwarding/tunnel/listener, reuse a
/// multiplexed connection, or redirect evidence must leave the job retained and
/// unverified. Unknown options fail closed so a future OpenSSH feature cannot
/// silently expand this boundary.
fn ssh_control_args_unsafe_for_automatic_replay(args: &[String]) -> Option<String> {
    let mut index = 0usize;
    while index < args.len() {
        let arg = trim_arg_quotes(&args[index]);
        if arg == "--" {
            index += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }

        if matches!(arg, "-4" | "-6" | "-a" | "-C" | "-k" | "-n" | "-T" | "-x") {
            index += 1;
            continue;
        }

        if matches!(arg, "-i" | "-l" | "-p") {
            let Some(raw_value) = args.get(index + 1) else {
                return Some(format!("{arg}:missing_value"));
            };
            let value = trim_arg_quotes(raw_value);
            let valid = match arg {
                "-i" => {
                    !value.is_empty()
                        && !value.chars().any(char::is_control)
                        && Path::new(value).is_absolute()
                }
                "-l" => {
                    !value.is_empty()
                        && value
                            .chars()
                            .all(|ch| !ch.is_control() && !ch.is_whitespace())
                }
                "-p" => value
                    .parse::<u16>()
                    .ok()
                    .is_some_and(|port| port > 0 && value.chars().all(|ch| ch.is_ascii_digit())),
                _ => false,
            };
            if !valid {
                return Some(format!("{arg}:invalid_value"));
            }
            index += 2;
            continue;
        }

        let (option_value, consumed) = if arg == "-o" {
            let Some(value) = args.get(index + 1) else {
                return Some("-o:missing_value".to_owned());
            };
            (trim_arg_quotes(value), 2usize)
        } else if let Some(value) = arg.strip_prefix("-o") {
            if value.is_empty() {
                return Some("-o:missing_value".to_owned());
            }
            (value, 1usize)
        } else {
            return Some(format!("{arg}:not_allowlisted"));
        };
        let Some((key, value)) = option_value.split_once('=') else {
            return Some("-o:expected_key_equals_value".to_owned());
        };
        if key.is_empty()
            || value.is_empty()
            || key.chars().any(char::is_whitespace)
            || value.chars().any(char::is_whitespace)
        {
            return Some("-o:invalid_key_or_value".to_owned());
        }
        let key = key.to_ascii_lowercase();
        let value = value.to_ascii_lowercase();
        let explicitly_safe = [
            ("batchmode", "yes"),
            ("clearallforwardings", "yes"),
            ("permitlocalcommand", "no"),
            ("proxycommand", "none"),
            ("proxyjump", "none"),
            ("controlmaster", "no"),
            ("controlpath", "none"),
            ("controlpersist", "no"),
            ("forwardagent", "no"),
            ("forwardx11", "no"),
            ("forwardx11trusted", "no"),
            ("tunnel", "no"),
            ("requesttty", "no"),
            ("forkafterauthentication", "no"),
            ("stdinnull", "yes"),
            ("enableescapecommandline", "no"),
            ("addkeystoagent", "no"),
            ("updatehostkeys", "no"),
            ("stricthostkeychecking", "yes"),
            ("numberofpasswordprompts", "0"),
            ("knownhostscommand", "none"),
            ("identitiesonly", "yes"),
        ]
        .contains(&(key.as_str(), value.as_str()));
        if !explicitly_safe {
            return Some(format!("-o{key}:not_allowlisted"));
        }
        index += consumed;
    }

    if index + 1 != args.len() {
        return Some(if index >= args.len() {
            "ssh_destination:missing".to_owned()
        } else {
            "ssh_control_args:unexpected_remote_command_or_trailing_argv".to_owned()
        });
    }
    let destination = trim_arg_quotes(&args[index]);
    if destination.is_empty()
        || destination
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Some("ssh_destination:invalid".to_owned());
    }
    None
}

// OpenSSH documents literal `none` as disabling both user and system config
// files. Unlike an OS null-device path it is portable across Windows and Unix.
const SSH_AUTOMATIC_REPLAY_DISABLED_CONFIG: &str = "none";

fn ssh_automatic_replay_safe_baseline_args() -> Vec<String> {
    [
        "-F",
        SSH_AUTOMATIC_REPLAY_DISABLED_CONFIG,
        "-o",
        "BatchMode=yes",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "ProxyCommand=none",
        "-o",
        "ProxyJump=none",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ControlPersist=no",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "Tunnel=no",
        "-o",
        "RequestTTY=no",
        "-o",
        "ForkAfterAuthentication=no",
        "-o",
        "StdinNull=yes",
        "-o",
        "EnableEscapeCommandline=no",
        "-o",
        "AddKeysToAgent=no",
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "NumberOfPasswordPrompts=0",
        "-o",
        "KnownHostsCommand=none",
        "-a",
        "-x",
        "-T",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

fn hardened_ssh_automatic_replay_args(control_args: &[String]) -> Result<Vec<String>, String> {
    if let Some(reason) = ssh_control_args_unsafe_for_automatic_replay(control_args) {
        return Err(format!(
            "SSH control argv is not safe for automatic replay: {reason}"
        ));
    }
    let mut args = ssh_automatic_replay_safe_baseline_args();
    args.extend_from_slice(control_args);
    Ok(args)
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
        remote_boot_id: None,
        remote_process_start_time: None,
        remote_ownership_token: None,
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
        ("remote_pidfd_unavailable", "error=pidfd_unavailable"),
        (
            "remote_proc_identity_unavailable",
            "error=proc_identity_unavailable",
        ),
        (
            "remote_boot_identity_unavailable",
            "error=boot_identity_unavailable",
        ),
        (
            "remote_identity_prerequisite_unavailable",
            "error=remote_identity_prerequisite_unavailable",
        ),
        (
            "remote_guardian_scope_unavailable",
            "error=guardian_scope_unavailable",
        ),
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
    boot_id: Option<String>,
    start_time: Option<String>,
    ownership_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteExitMetadata {
    job_id: String,
    pid: String,
    pgid: String,
    exit_code: i32,
    ownership_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteProcessOwnershipIdentity {
    boot_id: String,
    start_time: String,
    ownership_token: String,
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
    let ownership_token = job.remote_process_scope.remote_ownership_token.as_deref();
    let metadata =
        parse_remote_process_metadata_with_ownership(&stderr_prefix, &job.job_id, ownership_token)
            .or_else(|| {
                parse_remote_process_metadata_with_ownership(
                    &stderr_tail,
                    &job.job_id,
                    ownership_token,
                )
            });
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
    // Exit-evidence wins: a captured SYNAPSE_REMOTE_EXIT_V1 marker overrides a
    // *local* budget timeout. The scenario (#1604): the remote process exits and
    // emits its exit marker in a fraction of a second, but the local ssh.exe
    // wrapper keeps its control connection / pipe open past durable_timeout_ms
    // and is force-terminated by `wait_shell_job_child`. That local timeout must
    // not be allowed to shadow the captured remote exit — the job's verdict is
    // the remote exit; the local budget overrun is downgraded to a warning
    // (`downgrade_local_timeout_after_remote_exit`). A deliberate `cancel` is
    // still honored as an explicit operator verdict and is not overridden here.
    if job.remote_process_scope.transport != SHELL_REMOTE_TRANSPORT_SSH || job.cancel_requested {
        return Ok(false);
    }
    let overriding_local_timeout = job.timed_out;
    let stderr_prefix =
        read_file_prefix_lossy(&paths.stderr_path, SHELL_REMOTE_METADATA_PREFIX_BYTES)?;
    let stderr_tail = tail_file_lossy(&paths.stderr_path, SHELL_JOB_TAIL_DEFAULT_BYTES as usize)?;
    let ownership_token = job.remote_process_scope.remote_ownership_token.as_deref();
    let metadata =
        parse_remote_exit_metadata_with_ownership(&stderr_prefix, &job.job_id, ownership_token)
            .or_else(|| {
                parse_remote_exit_metadata_with_ownership(
                    &stderr_tail,
                    &job.job_id,
                    ownership_token,
                )
            });
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
        || job
            .remote_process_scope
            .remote_ownership_token
            .as_deref()
            .is_some_and(|token| metadata.ownership_token.as_deref() != Some(token))
    {
        push_unique_evidence(
            &mut job.remote_process_scope.detection_evidence,
            "remote_exit_marker_ignored:metadata_mismatch".to_owned(),
        );
        return Ok(false);
    }
    // A clean remote exit (code 0) with a live/mismatched local verdict is the
    // ordinary "already gone, local stale" reconciliation. A *non-zero* remote
    // exit is only reconciled when it is correcting a stale LOCAL timeout: in
    // that case exit-evidence (the real remote failure code) is strictly more
    // truthful than "timed_out", so it wins. Outside a local timeout a non-zero
    // local verdict already reflects the failure honestly and is left untouched
    // so we never manufacture an "already gone" success out of a remote failure
    // (regression guard for issue1274_remote_exit_marker_nonzero_*).
    if metadata.exit_code != 0 && !overriding_local_timeout {
        return Ok(false);
    }
    if !running && job.status == "ok" && job.exit_code == Some(0) {
        return Ok(false);
    }
    let termination = if running && job.pid.is_some() {
        Some(terminate_shell_job_from_status(job))
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
    if overriding_local_timeout {
        downgrade_local_timeout_after_remote_exit(job, trigger, metadata.exit_code);
    }
    Ok(true)
}

/// Downgrade a local `timed_out` verdict to a structured warning once a matching
/// remote exit marker has proven the remote process actually finished (#1604).
///
/// Exit-evidence is the verdict: `timed_out` is cleared and the caller-facing
/// `ACTION_BUDGET_EXPIRED` error code is dropped (the local budget overrun is no
/// longer the outcome). The overrun is preserved verbatim as detection evidence
/// so the loud, rich context — that the local ssh wrapper outran its budget
/// after the remote had already exited — is never silently swallowed.
fn downgrade_local_timeout_after_remote_exit(
    job: &mut ActRunShellJobStatus,
    trigger: &'static str,
    remote_exit_code: i32,
) {
    if !job.timed_out {
        return;
    }
    let budget_ms = job.timeout_ms.unwrap_or_default();
    job.timed_out = false;
    push_unique_evidence(
        &mut job.remote_process_scope.detection_evidence,
        format!(
            "local_timeout_overridden_by_remote_exit_marker:trigger={trigger}:budget_ms={budget_ms}:remote_exit_code={remote_exit_code}"
        ),
    );
    if job.error_code.as_deref() == Some(error_codes::ACTION_BUDGET_EXPIRED) {
        job.error_code = None;
    }
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
    let termination = job.pid.map(|_| terminate_shell_job_from_status(job));
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
    let mut liveness_args = match hardened_ssh_automatic_replay_args(&invocation.args) {
        Ok(args) => args,
        Err(_) => {
            push_unique_evidence(
                &mut job.remote_process_scope.detection_evidence,
                "remote_liveness_probe_failed:ssh_control_args_not_replay_safe".to_owned(),
            );
            return None;
        }
    };
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

fn parse_remote_process_metadata_with_ownership(
    stderr: &str,
    expected_job_id: &str,
    expected_ownership_token: Option<&str>,
) -> Option<RemoteProcessMetadata> {
    for line in stderr.lines() {
        let Some(marker_index) = line.find(SHELL_REMOTE_PROCESS_MARKER) else {
            continue;
        };
        let rest = &line[marker_index + SHELL_REMOTE_PROCESS_MARKER.len()..];
        let fields = parse_marker_fields(rest);
        let Some(job_id) = fields.get("job_id") else {
            continue;
        };
        if job_id != expected_job_id {
            continue;
        }
        let (Some(pid), Some(pgid)) = (fields.get("pid"), fields.get("pgid")) else {
            continue;
        };
        if !valid_remote_process_number(pid) || !valid_remote_process_number(pgid) {
            continue;
        }
        let sid = fields
            .get("sid")
            .filter(|value| valid_remote_process_number(value))
            .cloned();
        let (boot_id, start_time, ownership_token) = match (
            fields.get("boot_id"),
            fields.get("start_time"),
            fields.get("ownership_token"),
            fields.get("ownership_token_sha256"),
        ) {
            (None, None, None, None) => (None, None, None),
            (Some(boot_id), Some(start_time), Some(ownership_token), None)
                if valid_remote_boot_id(boot_id)
                    && valid_remote_process_start_time(start_time)
                    && valid_remote_ownership_token(ownership_token)
                    && expected_ownership_token
                        .is_none_or(|expected| expected == ownership_token) =>
            {
                (
                    Some(boot_id.clone()),
                    Some(start_time.clone()),
                    Some(ownership_token.clone()),
                )
            }
            (Some(boot_id), Some(start_time), None, Some(ownership_token_sha256))
                if valid_remote_boot_id(boot_id)
                    && valid_remote_process_start_time(start_time)
                    && validate_lower_sha256(
                        ownership_token_sha256,
                        "ownership_token_sha256 marker",
                        expected_job_id,
                    )
                    .is_ok()
                    && expected_ownership_token.is_some_and(|expected| {
                        valid_remote_ownership_token(expected)
                            && sha256_hex(expected.as_bytes()) == *ownership_token_sha256
                    }) =>
            {
                (
                    Some(boot_id.clone()),
                    Some(start_time.clone()),
                    expected_ownership_token.map(ToOwned::to_owned),
                )
            }
            // Partial or malformed identity is not legacy metadata. Ignoring it
            // prevents a truncated marker from silently downgrading into the
            // destructive PID/PGID-only cleanup path.
            _ => continue,
        };
        return Some(RemoteProcessMetadata {
            job_id: job_id.clone(),
            pid: pid.clone(),
            pgid: pgid.clone(),
            sid,
            boot_id,
            start_time,
            ownership_token,
        });
    }
    None
}

fn parse_remote_exit_metadata_with_ownership(
    stderr: &str,
    expected_job_id: &str,
    expected_ownership_token: Option<&str>,
) -> Option<RemoteExitMetadata> {
    let mut found = None;
    for line in stderr.lines() {
        let Some(marker_index) = line.find(SHELL_REMOTE_EXIT_MARKER) else {
            continue;
        };
        let rest = &line[marker_index + SHELL_REMOTE_EXIT_MARKER.len()..];
        let fields = parse_marker_fields(rest);
        let Some(job_id) = fields.get("job_id") else {
            continue;
        };
        if job_id != expected_job_id {
            continue;
        }
        let (Some(pid), Some(pgid), Some(exit_code)) = (
            fields.get("pid"),
            fields.get("pgid"),
            fields
                .get("exit_code")
                .and_then(|value| value.parse::<i32>().ok()),
        ) else {
            continue;
        };
        if !valid_remote_process_number(pid) || !valid_remote_process_number(pgid) {
            continue;
        }
        let ownership_token = match (
            fields.get("ownership_token"),
            fields.get("ownership_token_sha256"),
        ) {
            (Some(value), None)
                if valid_remote_ownership_token(value)
                    && expected_ownership_token.is_none_or(|expected| expected == value) =>
            {
                Some(value.clone())
            }
            (None, Some(value))
                if validate_lower_sha256(
                    value,
                    "ownership_token_sha256 marker",
                    expected_job_id,
                )
                .is_ok()
                    && expected_ownership_token.is_some_and(|expected| {
                        valid_remote_ownership_token(expected)
                            && sha256_hex(expected.as_bytes()) == *value
                    }) =>
            {
                expected_ownership_token.map(ToOwned::to_owned)
            }
            (None, None) => None,
            _ => continue,
        };
        found = Some(RemoteExitMetadata {
            job_id: job_id.clone(),
            pid: pid.clone(),
            pgid: pgid.clone(),
            exit_code,
            ownership_token,
        });
    }
    found
}

fn apply_remote_process_metadata(job: &mut ActRunShellJobStatus, metadata: RemoteProcessMetadata) {
    job.remote_process_scope.remote_process_id = Some(metadata.pid.clone());
    job.remote_process_scope.remote_process_group_id = Some(metadata.pgid.clone());
    job.remote_process_scope.remote_boot_id = metadata.boot_id.clone();
    job.remote_process_scope.remote_process_start_time = metadata.start_time.clone();
    job.remote_process_scope.remote_ownership_token = metadata.ownership_token.clone();
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
    if let (Some(boot_id), Some(start_time), Some(ownership_token)) = (
        metadata.boot_id,
        metadata.start_time,
        metadata.ownership_token,
    ) {
        push_unique_evidence(
            &mut job.remote_process_scope.detection_evidence,
            format!(
                "remote_process_ownership:boot_id={boot_id}:start_time={start_time}:token_sha256={}",
                sha256_hex(ownership_token.as_bytes())
            ),
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

fn valid_remote_boot_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn valid_remote_process_start_time(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|parsed| parsed > 0 && value.chars().all(|ch| ch.is_ascii_digit()))
}

fn valid_remote_ownership_token(value: &str) -> bool {
    value.len() == 32
        && value
            .chars()
            .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
}

fn remote_process_ownership_identity(
    scope: &ActRunShellRemoteProcessScope,
) -> Result<Option<RemoteProcessOwnershipIdentity>, String> {
    match (
        scope.remote_boot_id.as_deref(),
        scope.remote_process_start_time.as_deref(),
        scope.remote_ownership_token.as_deref(),
    ) {
        (None, None, None) => Ok(None),
        (Some(boot_id), Some(start_time), Some(ownership_token))
            if valid_remote_boot_id(boot_id)
                && valid_remote_process_start_time(start_time)
                && valid_remote_ownership_token(ownership_token) =>
        {
            Ok(Some(RemoteProcessOwnershipIdentity {
                boot_id: boot_id.to_owned(),
                start_time: start_time.to_owned(),
                ownership_token: ownership_token.to_owned(),
            }))
        }
        _ => Err(
            "remote ownership metadata is partial or malformed; expected canonical boot_id, positive /proc start_time, and 32-character lowercase hexadecimal ownership token"
                .to_owned(),
        ),
    }
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
    let Some(remote_cleanup) = remote_cleanup.as_ref() else {
        mark_shell_job_remote_cleanup_failed(
            job,
            trigger,
            "remote_cleanup_sidecar_missing",
            "remote process metadata exists but the durable remote cleanup sidecar is absent",
        );
        return Some("remote_cleanup_sidecar_missing".to_owned());
    };
    let Some(invocation) = shell_job_cleanup_invocation(job, original_args, Some(remote_cleanup))
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
    if let Some(reason) = ssh_control_args_unsafe_for_automatic_replay(&parts.control_args) {
        mark_shell_job_remote_cleanup_failed(
            job,
            trigger,
            "remote_cleanup_control_args_unsafe",
            &format!(
                "automatic SSH cleanup refused control option {reason}; replay could execute or load mutable local code"
            ),
        );
        return Some("remote_cleanup_control_args_unsafe".to_owned());
    }
    if !matches!(remote_cleanup.schema_version, 3 | 4) {
        let liveness = run_remote_liveness_probe(
            &invocation.command,
            &invocation.args,
            &pid,
            &pgid,
            "legacy remote cleanup liveness probe",
        );
        return match liveness {
            Ok((readback, status)) if status == "already_gone" => {
                job.remote_process_scope.remote_cleanup_verified = true;
                job.remote_process_scope.remote_cleanup_status =
                    SHELL_REMOTE_CLEANUP_ALREADY_GONE.to_owned();
                job.remote_process_scope.remote_cleanup_error_code = None;
                job.remote_process_scope.remote_cleanup_message = Some(format!(
                    "{trigger} verified schema-{} SSH remote pid {pid}, process group {pgid} is already gone without issuing a signal; stdout_sha256={}; stderr_sha256={}",
                    remote_cleanup.schema_version, readback.stdout_sha256, readback.stderr_sha256
                ));
                Some("remote_cleanup_already_gone".to_owned())
            }
            Ok((_readback, status)) => {
                mark_shell_job_remote_cleanup_failed(
                    job,
                    trigger,
                    "remote_cleanup_sidecar_liveness_only",
                    &format!(
                        "schema-{} remote pid {pid}, pgid {pgid} returned liveness status={status}, but the sidecar predates exact executable/argv/config/token binding and cannot authorize a destructive signal",
                        remote_cleanup.schema_version
                    ),
                );
                Some("remote_cleanup_sidecar_liveness_only".to_owned())
            }
            Err(message) => {
                mark_shell_job_remote_cleanup_failed(
                    job,
                    trigger,
                    "remote_cleanup_liveness_probe_failed",
                    &message,
                );
                Some("remote_cleanup_liveness_probe_failed".to_owned())
            }
        };
    }
    let ownership_identity = match remote_process_ownership_identity(&job.remote_process_scope) {
        Ok(identity) => identity,
        Err(message) => {
            mark_shell_job_remote_cleanup_failed(
                job,
                trigger,
                "remote_process_ownership_metadata_invalid",
                &message,
            );
            return Some("remote_cleanup_ownership_metadata_invalid".to_owned());
        }
    };
    let Some(ownership_identity) = ownership_identity else {
        // Legacy records have only reusable numeric identifiers. A read-only
        // liveness probe may prove the old process is gone, but an alive result
        // can never authorize a signal.
        let liveness = run_remote_liveness_probe(
            &invocation.command,
            &invocation.args,
            &pid,
            &pgid,
            "legacy remote cleanup liveness probe",
        );
        return match liveness {
            Ok((readback, status)) if status == "already_gone" => {
                job.remote_process_scope.remote_cleanup_verified = true;
                job.remote_process_scope.remote_cleanup_status =
                    SHELL_REMOTE_CLEANUP_ALREADY_GONE.to_owned();
                job.remote_process_scope.remote_cleanup_error_code = None;
                job.remote_process_scope.remote_cleanup_message = Some(format!(
                    "{trigger} verified legacy SSH remote pid {pid}, process group {pgid} is already gone without issuing a signal; stdout_sha256={}; stderr_sha256={}",
                    readback.stdout_sha256, readback.stderr_sha256
                ));
                if job.error_code.as_deref()
                    == Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
                {
                    job.error_code = None;
                    job.error_message = None;
                }
                Some("remote_cleanup_legacy_already_gone".to_owned())
            }
            Ok((_readback, status)) if status == "alive" => {
                mark_shell_job_remote_cleanup_failed(
                    job,
                    trigger,
                    "remote_process_ownership_identity_unavailable",
                    &format!(
                        "legacy remote pid {pid}, pgid {pgid} is alive, but the record has no boot_id/start_time/ownership_token; retained without destructive cleanup"
                    ),
                );
                Some("remote_cleanup_legacy_identity_unavailable".to_owned())
            }
            Ok((_readback, status)) => {
                mark_shell_job_remote_cleanup_failed(
                    job,
                    trigger,
                    "remote_liveness_status_unrecognized",
                    &format!("legacy remote liveness returned unsupported status={status}"),
                );
                Some("remote_cleanup_legacy_liveness_unrecognized".to_owned())
            }
            Err(message) => {
                mark_shell_job_remote_cleanup_failed(
                    job,
                    trigger,
                    "remote_liveness_probe_failed",
                    &message,
                );
                Some("remote_cleanup_legacy_liveness_failed".to_owned())
            }
        };
    };
    let cleanup_command = ssh_remote_cleanup_command(&pid, &pgid, &ownership_identity);
    let mut cleanup_args = match hardened_ssh_automatic_replay_args(&parts.control_args) {
        Ok(args) => args,
        Err(message) => {
            mark_shell_job_remote_cleanup_failed(
                job,
                trigger,
                "remote_cleanup_control_args_unsafe",
                &message,
            );
            return Some("remote_cleanup_control_args_unsafe".to_owned());
        }
    };
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
    let cleanup_status =
        parse_remote_cleanup_status(&readback.stdout, &pid, &pgid, Some(&ownership_identity));
    match cleanup_status.as_deref() {
        Some(status @ ("already_gone" | "terminated" | "killed"))
            if readback.exit_code == Some(0) =>
        {
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
        Some("identity_mismatch") => {
            mark_shell_job_remote_cleanup_failed(
                job,
                trigger,
                "remote_process_ownership_identity_mismatch",
                &format!(
                    "SSH remote cleanup refused to signal pid {pid}, pgid {pgid}: boot/start/token identity did not match the live process"
                ),
            );
            Some("remote_cleanup_identity_mismatch".to_owned())
        }
        _ => {
            mark_shell_job_remote_cleanup_failed(
                job,
                trigger,
                "cleanup_readback_unrecognized",
                &format!(
                    "SSH remote cleanup command did not produce a verified cleanup marker; exit={:?}; stdout_sha256={}; stderr_sha256={}; stdout_excerpt={:?}; stderr_excerpt={:?}",
                    readback.exit_code,
                    readback.stdout_sha256,
                    readback.stderr_sha256,
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
    stdout_byte_len: u64,
    stdout_sha256: String,
    stderr: String,
    stderr_byte_len: u64,
    stderr_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BoundedCleanupCapture {
    text: String,
    byte_len: u64,
    sha256: String,
}

#[derive(Clone, Debug, Serialize)]
struct ExactChildReapReadback {
    kill_error: Option<String>,
    reaped: bool,
    exit_code: Option<i32>,
    exit_status: Option<String>,
    timed_out: bool,
    poll_attempts: u64,
    poll_error_count: u64,
    last_poll_error: Option<String>,
    elapsed_ms: u64,
}

/// Poll one exact owned child handle until it is reaped or a hard cleanup
/// backstop expires. Both Tokio and std children expose the same nonblocking
/// `try_wait` contract, so all exceptional cleanup paths share this bounded
/// state machine rather than falling back to an unbounded blocking `wait`.
fn bounded_poll_exact_child_reap<F>(
    mut try_wait: F,
    kill_error: Option<String>,
    timeout: Duration,
) -> ExactChildReapReadback
where
    F: FnMut() -> io::Result<Option<std::process::ExitStatus>>,
{
    let started = Instant::now();
    let mut poll_attempts = 0_u64;
    let mut poll_error_count = 0_u64;
    let mut last_poll_error = None;
    loop {
        poll_attempts = poll_attempts.saturating_add(1);
        match try_wait() {
            Ok(Some(status)) => {
                return ExactChildReapReadback {
                    kill_error,
                    reaped: true,
                    exit_code: status.code(),
                    exit_status: Some(format!("{status:?}")),
                    timed_out: false,
                    poll_attempts,
                    poll_error_count,
                    last_poll_error,
                    elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                };
            }
            Ok(None) => {}
            Err(error) => {
                poll_error_count = poll_error_count.saturating_add(1);
                last_poll_error = Some(error.to_string());
            }
        }

        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return ExactChildReapReadback {
                kill_error,
                reaped: false,
                exit_code: None,
                exit_status: None,
                timed_out: true,
                poll_attempts,
                poll_error_count,
                last_poll_error,
                elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            };
        }
        std::thread::sleep(
            Duration::from_millis(SHELL_CHILD_REAP_POLL_INTERVAL_MS)
                .min(timeout.saturating_sub(elapsed)),
        );
    }
}

fn terminate_and_reap_tokio_child_bounded(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> ExactChildReapReadback {
    let kill_error = child.start_kill().err().map(|error| error.to_string());
    bounded_poll_exact_child_reap(|| child.try_wait(), kill_error, timeout)
}

async fn terminate_and_reap_tokio_child_async_bounded(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> ExactChildReapReadback {
    let kill_error = child.start_kill().err().map(|error| error.to_string());
    let started = Instant::now();
    let mut poll_attempts = 0_u64;
    let mut poll_error_count = 0_u64;
    let mut last_poll_error = None;
    loop {
        poll_attempts = poll_attempts.saturating_add(1);
        match child.try_wait() {
            Ok(Some(status)) => {
                return ExactChildReapReadback {
                    kill_error,
                    reaped: true,
                    exit_code: status.code(),
                    exit_status: Some(format!("{status:?}")),
                    timed_out: false,
                    poll_attempts,
                    poll_error_count,
                    last_poll_error,
                    elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                };
            }
            Ok(None) => {}
            Err(error) => {
                poll_error_count = poll_error_count.saturating_add(1);
                last_poll_error = Some(error.to_string());
            }
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return ExactChildReapReadback {
                kill_error,
                reaped: false,
                exit_code: None,
                exit_status: None,
                timed_out: true,
                poll_attempts,
                poll_error_count,
                last_poll_error,
                elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            };
        }
        tokio::time::sleep(
            Duration::from_millis(SHELL_CHILD_REAP_POLL_INTERVAL_MS)
                .min(timeout.saturating_sub(elapsed)),
        )
        .await;
    }
}

fn terminate_and_reap_std_child_only_bounded(
    child: &mut std::process::Child,
) -> ExactChildReapReadback {
    let kill_error = child.kill().err().map(|error| error.to_string());
    bounded_poll_exact_child_reap(
        || child.try_wait(),
        kill_error,
        Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
    )
}

#[derive(Clone, Debug, Serialize)]
struct CleanupChildTerminationReadback {
    owned_root_pid: u32,
    tree_termination_attempted: bool,
    tree_termination_status: String,
    remaining_process_ids: Vec<u32>,
    reap: ExactChildReapReadback,
}

impl CleanupChildTerminationReadback {
    fn diagnostic(&self) -> String {
        format!(
            "owned_root_pid={}; termination_attempted={}; termination_status={}; remaining_process_ids={:?}; direct_kill_error={:?}; reaped={}; exit_code={:?}; exit_status={:?}; reap_timed_out={}; reap_poll_attempts={}; reap_poll_error_count={}; reap_last_poll_error={:?}; reap_elapsed_ms={}",
            self.owned_root_pid,
            self.tree_termination_attempted,
            self.tree_termination_status,
            self.remaining_process_ids,
            self.reap.kill_error,
            self.reap.reaped,
            self.reap.exit_code,
            self.reap.exit_status,
            self.reap.timed_out,
            self.reap.poll_attempts,
            self.reap.poll_error_count,
            self.reap.last_poll_error,
            self.reap.elapsed_ms,
        )
    }
}

fn terminate_and_reap_cleanup_child_bounded(
    child: &mut std::process::Child,
    owned_root_identity: &ActRunShellLocalProcessIdentity,
) -> CleanupChildTerminationReadback {
    let termination = terminate_shell_job_process_tree(owned_root_identity);
    let kill_error = child.kill().err().map(|error| error.to_string());
    let reap = bounded_poll_exact_child_reap(
        || child.try_wait(),
        kill_error,
        Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
    );
    CleanupChildTerminationReadback {
        owned_root_pid: owned_root_identity.pid,
        tree_termination_attempted: termination.attempted,
        tree_termination_status: termination.status,
        remaining_process_ids: termination.remaining_process_ids,
        reap,
    }
}

#[derive(Debug)]
struct ContainedCleanupChildReadback {
    initial: CleanupChildTerminationReadback,
    job_close: Result<(), String>,
    post_job_close_reap: Option<ExactChildReapReadback>,
    final_identity_state: LocalProcessIdentityState,
}

impl ContainedCleanupChildReadback {
    fn tree_cleanup_verified(&self) -> bool {
        self.initial.remaining_process_ids.is_empty()
            && matches!(
                self.initial.tree_termination_status.as_str(),
                "terminated" | "already_exited"
            )
    }

    fn cleanup_verified(&self) -> bool {
        let exact_child_reaped = self.initial.reap.reaped
            || self
                .post_job_close_reap
                .as_ref()
                .is_some_and(|readback| readback.reaped);
        exact_child_reaped
            && self.job_close.is_ok()
            && self.tree_cleanup_verified()
            && terminal_local_process_identity_state(&self.final_identity_state)
    }

    fn diagnostic(&self) -> String {
        format!(
            "{}; job_close={:?}; post_job_close_reap={:?}; final_identity_state={:?}",
            self.initial.diagnostic(),
            self.job_close,
            self.post_job_close_reap,
            self.final_identity_state,
        )
    }
}

fn terminate_reap_and_close_cleanup_child_bounded(
    child: &mut std::process::Child,
    owned_root_identity: &ActRunShellLocalProcessIdentity,
    process_job: &mut OwnedProcessJob,
) -> ContainedCleanupChildReadback {
    let initial = terminate_and_reap_cleanup_child_bounded(child, owned_root_identity);
    let job_close = process_job.close_checked();
    let state_after_close = local_process_identity_state(owned_root_identity);
    let post_job_close_reap = (!initial.reap.reaped
        || matches!(
            state_after_close,
            LocalProcessIdentityState::Match | LocalProcessIdentityState::Unreadable(_)
        ))
    .then(|| terminate_and_reap_std_child_only_bounded(child));
    let final_identity_state = local_process_identity_state(owned_root_identity);
    ContainedCleanupChildReadback {
        initial,
        job_close,
        post_job_close_reap,
        final_identity_state,
    }
}

fn finalize_contained_cleanup_child_failure(
    child: std::process::Child,
    process_job: OwnedProcessJob,
    owned_root_identity: ActRunShellLocalProcessIdentity,
    stage: &'static str,
    cleanup: ContainedCleanupChildReadback,
) -> String {
    let cleanup_verified = cleanup.cleanup_verified();
    let process_job_close_verified = cleanup.job_close.is_ok();
    let tree_cleanup_verified = cleanup.tree_cleanup_verified();
    let diagnostic = cleanup.diagnostic();
    let retained_owner = (!cleanup_verified).then(|| {
        retain_unresolved_shell_child_owner(RetainedShellChildOwner {
            owner_id: new_reflex_id(),
            pid: Some(owned_root_identity.pid),
            stage: stage.to_owned(),
            child: RetainedExactShellChild::Std(Box::new(child)),
            process_job: Some(process_job),
            process_job_acquired: true,
            process_job_close_verified,
            tree_cleanup_verified,
            local_process_identity: Some(owned_root_identity),
            durable_spawn_failure: None,
        })
    });
    format!(
        "{diagnostic}; cleanup_verified={cleanup_verified}; exact_owner_retained={retained_owner:?}"
    )
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
    // Opportunistically advance any earlier exact-owner cleanup without ever
    // blocking this invocation on an unbounded wait.
    let _ = unresolved_shell_child_owner_report();
    let spawn_command = trusted_ssh_automatic_replay_executable(command).ok_or_else(|| {
        format!(
            "cleanup SSH executable is not one of the canonical trusted platform installations: {command}"
        )
    })?;
    let mut stdout_capture = tempfile::tempfile()
        .map_err(|error| format!("create cleanup ssh stdout capture failed: {error}"))?;
    let mut stderr_capture = tempfile::tempfile()
        .map_err(|error| format!("create cleanup ssh stderr capture failed: {error}"))?;
    let stdout_child = stdout_capture
        .try_clone()
        .map_err(|error| format!("clone cleanup ssh stdout capture failed: {error}"))?;
    let stderr_child = stderr_capture
        .try_clone()
        .map_err(|error| format!("clone cleanup ssh stderr capture failed: {error}"))?;
    let mut child = StdCommand::new(&spawn_command);
    child
        .args(args)
        .stdin(Stdio::null())
        // Regular files cannot fill a pipe buffer and do not require EOF from
        // inherited descendant handles. This keeps collection bounded even if
        // an SSH proxy/descendant outlives the direct client.
        .stdout(Stdio::from(stdout_child))
        .stderr(Stdio::from(stderr_child));
    apply_no_window_std(&mut child);
    let mut child = child
        .spawn()
        .map_err(|error| format!("spawn cleanup ssh failed: {error}"))?;
    let owned_root_pid = child.id();
    // As in durable spawn, establish containment while the exact child is
    // suspended before asking a second kernel API for creation identity.
    let mut process_job = match assign_owned_process_job(
        owned_root_pid,
        "act_run_shell_remote_cleanup",
        None,
    ) {
        Ok(process_job) => process_job,
        Err(assignment_error) => {
            let reap = terminate_and_reap_std_child_only_bounded(&mut child);
            let tree_cleanup_verified = cfg!(windows);
            let cleanup_verified = reap.reaped && tree_cleanup_verified;
            let retained_owner = (!cleanup_verified).then(|| {
                retain_unresolved_shell_child_owner(RetainedShellChildOwner {
                    owner_id: new_reflex_id(),
                    pid: Some(owned_root_pid),
                    stage: "cleanup_job_object_assignment_failed".to_owned(),
                    child: RetainedExactShellChild::Std(Box::new(child)),
                    process_job: None,
                    process_job_acquired: false,
                    process_job_close_verified: true,
                    tree_cleanup_verified,
                    local_process_identity: None,
                    durable_spawn_failure: None,
                })
            });
            return Err(format!(
                "cleanup ssh pid {owned_root_pid} job ownership failed before execution: {}; exact_child_cleanup={reap:?}; tree_cleanup_verified={tree_cleanup_verified}; cleanup_verified={cleanup_verified}; exact_owner_retained={retained_owner:?}",
                assignment_error.message,
            ));
        }
    };
    let owned_root_identity = match capture_local_process_identity(owned_root_pid) {
        Ok(identity) => identity,
        Err(identity_error) => {
            let initial = terminate_and_reap_std_child_only_bounded(&mut child);
            let job_close = process_job.close_checked();
            let post =
                (!initial.reaped).then(|| terminate_and_reap_std_child_only_bounded(&mut child));
            let reap = merge_exact_child_reap_readbacks(initial, post);
            let tree_cleanup_verified = cfg!(windows);
            let cleanup_verified = reap.reaped && job_close.is_ok() && tree_cleanup_verified;
            let retained_owner = (!cleanup_verified).then(|| {
                retain_unresolved_shell_child_owner(RetainedShellChildOwner {
                    owner_id: new_reflex_id(),
                    pid: Some(owned_root_pid),
                    stage: "cleanup_local_process_identity_capture_failed".to_owned(),
                    child: RetainedExactShellChild::Std(Box::new(child)),
                    process_job: Some(process_job),
                    process_job_acquired: true,
                    process_job_close_verified: job_close.is_ok(),
                    tree_cleanup_verified,
                    local_process_identity: None,
                    durable_spawn_failure: None,
                })
            });
            return Err(format!(
                "cleanup ssh pid {owned_root_pid} identity capture failed before execution: {identity_error}; exact_child_cleanup={reap:?}; job_close={job_close:?}; cleanup_verified={cleanup_verified}; exact_owner_retained={retained_owner:?}"
            ));
        }
    };
    if let Err(resume_error) = resume_suspended_shell_child(&owned_root_identity) {
        let cleanup = terminate_reap_and_close_cleanup_child_bounded(
            &mut child,
            &owned_root_identity,
            &mut process_job,
        );
        let cleanup_verified = cleanup.cleanup_verified();
        let diagnostic = cleanup.diagnostic();
        let retained_owner = (!cleanup_verified).then(|| {
            retain_unresolved_shell_child_owner(RetainedShellChildOwner {
                owner_id: new_reflex_id(),
                pid: Some(owned_root_pid),
                stage: "cleanup_contained_child_resume_failed".to_owned(),
                child: RetainedExactShellChild::Std(Box::new(child)),
                process_job: Some(process_job),
                process_job_acquired: true,
                process_job_close_verified: cleanup.job_close.is_ok(),
                tree_cleanup_verified: cleanup.tree_cleanup_verified(),
                local_process_identity: Some(owned_root_identity),
                durable_spawn_failure: None,
            })
        });
        return Err(format!(
            "cleanup ssh pid {owned_root_pid} contained resume failed: {resume_error}; {diagnostic}; cleanup_verified={cleanup_verified}; exact_owner_retained={retained_owner:?}"
        ));
    }
    let started = Instant::now();
    let exit_code = loop {
        let (stdout_len, stderr_len) = match (stdout_capture.metadata(), stderr_capture.metadata())
        {
            (Ok(stdout_len), Ok(stderr_len)) => (stdout_len, stderr_len),
            (stdout_result, stderr_result) => {
                let cleanup = terminate_reap_and_close_cleanup_child_bounded(
                    &mut child,
                    &owned_root_identity,
                    &mut process_job,
                );
                let cleanup_diagnostic = finalize_contained_cleanup_child_failure(
                    child,
                    process_job,
                    owned_root_identity,
                    "cleanup_capture_length_inspection_failed",
                    cleanup,
                );
                return Err(format!(
                    "cleanup ssh capture length inspection failed; stdout_error={:?}; stderr_error={:?}; {}",
                    stdout_result.err(),
                    stderr_result.err(),
                    cleanup_diagnostic,
                ));
            }
        };
        if stdout_len.len() > SHELL_CLEANUP_CAPTURE_CAP_BYTES
            || stderr_len.len() > SHELL_CLEANUP_CAPTURE_CAP_BYTES
        {
            let cleanup = terminate_reap_and_close_cleanup_child_bounded(
                &mut child,
                &owned_root_identity,
                &mut process_job,
            );
            let stdout_diagnostic = cleanup_capture_diagnostic(&mut stdout_capture, "stdout");
            let stderr_diagnostic = cleanup_capture_diagnostic(&mut stderr_capture, "stderr");
            let cleanup_diagnostic = finalize_contained_cleanup_child_failure(
                child,
                process_job,
                owned_root_identity,
                "cleanup_capture_cap_exceeded",
                cleanup,
            );
            return Err(format!(
                "cleanup ssh diagnostic output exceeded the {SHELL_CLEANUP_CAPTURE_CAP_BYTES}-byte per-stream cap; {cleanup_diagnostic}; {stdout_diagnostic}; {stderr_diagnostic}",
            ));
        }
        let poll = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let cleanup = terminate_reap_and_close_cleanup_child_bounded(
                    &mut child,
                    &owned_root_identity,
                    &mut process_job,
                );
                let stdout_diagnostic = cleanup_capture_diagnostic(&mut stdout_capture, "stdout");
                let stderr_diagnostic = cleanup_capture_diagnostic(&mut stderr_capture, "stderr");
                let cleanup_diagnostic = finalize_contained_cleanup_child_failure(
                    child,
                    process_job,
                    owned_root_identity,
                    "cleanup_exact_child_poll_failed",
                    cleanup,
                );
                return Err(format!(
                    "poll cleanup ssh failed: {error}; {cleanup_diagnostic}; {stdout_diagnostic}; {stderr_diagnostic}",
                ));
            }
        };
        match poll {
            Some(status) => break status.code(),
            None if started.elapsed() >= timeout => {
                let cleanup = terminate_reap_and_close_cleanup_child_bounded(
                    &mut child,
                    &owned_root_identity,
                    &mut process_job,
                );
                let stdout_diagnostic = cleanup_capture_diagnostic(&mut stdout_capture, "stdout");
                let stderr_diagnostic = cleanup_capture_diagnostic(&mut stderr_capture, "stderr");
                let cleanup_diagnostic = finalize_contained_cleanup_child_failure(
                    child,
                    process_job,
                    owned_root_identity,
                    "cleanup_command_timeout",
                    cleanup,
                );
                return Err(format!(
                    "cleanup ssh timed out after {} ms; {cleanup_diagnostic}; {stdout_diagnostic}; {stderr_diagnostic}",
                    timeout.as_millis(),
                ));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = match read_bounded_cleanup_capture(
        &mut stdout_capture,
        SHELL_CLEANUP_CAPTURE_CAP_BYTES,
        "stdout",
    ) {
        Ok(stdout) => stdout,
        Err(error) => {
            let job_close = process_job.close_checked();
            return Err(format!(
                "{error}; completed cleanup ssh job close readback={job_close:?}"
            ));
        }
    };
    let stderr = match read_bounded_cleanup_capture(
        &mut stderr_capture,
        SHELL_CLEANUP_CAPTURE_CAP_BYTES,
        "stderr",
    ) {
        Ok(stderr) => stderr,
        Err(error) => {
            let job_close = process_job.close_checked();
            return Err(format!(
                "{error}; completed cleanup ssh job close readback={job_close:?}"
            ));
        }
    };
    process_job
        .close_checked()
        .map_err(|error| format!("completed cleanup ssh job handle close failed: {error}"))?;
    Ok(CleanupCommandReadback {
        exit_code,
        stdout: stdout.text,
        stdout_byte_len: stdout.byte_len,
        stdout_sha256: stdout.sha256,
        stderr: stderr.text,
        stderr_byte_len: stderr.byte_len,
        stderr_sha256: stderr.sha256,
    })
}

fn cleanup_capture_diagnostic(file: &mut fs::File, stream: &str) -> String {
    const HASH_PREFIX_CAP_BYTES: u64 = 1024 * 1024;
    let len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => return format!("{stream}_capture_metadata_error={error:?}"),
    };
    if let Err(error) = file.seek(SeekFrom::Start(0)) {
        return format!("{stream}_bytes={len}; {stream}_capture_seek_error={error:?}");
    }
    let mut prefix = Vec::new();
    if let Err(error) = file.take(HASH_PREFIX_CAP_BYTES).read_to_end(&mut prefix) {
        return format!("{stream}_bytes={len}; {stream}_capture_read_error={error:?}");
    }
    let text = String::from_utf8_lossy(&prefix);
    format!(
        "{stream}_bytes={len}; {stream}_prefix_bytes={}; {stream}_prefix_sha256={}; {stream}_excerpt={:?}",
        prefix.len(),
        sha256_hex(&prefix),
        shell_cleanup_output_excerpt(&text),
    )
}

fn read_bounded_cleanup_capture(
    file: &mut fs::File,
    cap_bytes: u64,
    stream: &str,
) -> Result<BoundedCleanupCapture, String> {
    let len = file
        .metadata()
        .map_err(|error| format!("read cleanup ssh {stream} capture metadata failed: {error}"))?
        .len();
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek cleanup ssh {stream} capture failed: {error}"))?;
    let mut bytes = Vec::new();
    file.take(cap_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read cleanup ssh {stream} capture failed: {error}"))?;
    let buffered_len = u64::try_from(bytes.len()).map_err(|error| {
        format!(
            "cleanup ssh {stream} buffered capture length cannot be represented: {error}; physical_bytes={len}"
        )
    })?;
    if len > cap_bytes || buffered_len > cap_bytes {
        return Err(format!(
            "cleanup ssh {stream} capture exceeded the {cap_bytes}-byte diagnostic cap (actual_bytes={len})"
        ));
    }
    Ok(BoundedCleanupCapture {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        byte_len: buffered_len,
        sha256: sha256_hex(&bytes),
    })
}

fn ssh_remote_cleanup_command(
    pid: &str,
    pgid: &str,
    identity: &RemoteProcessOwnershipIdentity,
) -> String {
    // A pidfd is the stable kernel reference that numeric PIDs are not. The
    // script verifies boot/start/token both before and after pidfd_open, then
    // signals only that pidfd. The owned guardian's TERM trap performs the
    // process-group termination while it still anchors the original PGID.
    const SCRIPT: &str = r#"import select
import signal
import time

pid = int(sys.argv[1])
pgid_text = sys.argv[2]
expected_boot_id = sys.argv[3]
expected_start_time = sys.argv[4]
ownership_token = sys.argv[5]
marker = sys.argv[6]

def emit(status):
    print(
        f"{marker} pid={pid} pgid={pgid_text} boot_id={expected_boot_id} "
        f"start_time={expected_start_time} ownership_token={ownership_token} status={status}",
        flush=True,
    )

def inspect_group():
    try:
        expected_pgid = int(pgid_text)
        members = live_process_ids_in_group(expected_pgid)
        return members, process_group_exists(expected_pgid)
    except Exception as error:
        print(
            f"process_group_inspection_failed pgid={pgid_text} error={error}",
            file=sys.stderr,
            flush=True,
        )
        emit("inspection_failed")
        raise SystemExit(4)

def finish_if_group_empty(success_status):
    _members, group_exists = inspect_group()
    if not group_exists:
        emit(success_status)
        raise SystemExit(0)
    emit("still_running")
    raise SystemExit(1)

def read_identity():
    try:
        with open("/proc/sys/kernel/random/boot_id", encoding="ascii") as handle:
            boot_id = handle.read().strip()
    except OSError as error:
        raise RuntimeError(f"read remote boot identity failed: {error}") from error
    try:
        with open(f"/proc/{pid}/stat", encoding="utf-8", errors="strict") as handle:
            stat_line = handle.read().strip()
        _comm, separator, stat_tail = stat_line.rpartition(") ")
        if not separator:
            raise RuntimeError("/proc stat has no command terminator")
        fields = stat_tail.split()
        if len(fields) < 20:
            raise RuntimeError("/proc stat has fewer than 22 fields")
        actual_pgid = fields[2]
        start_time = fields[19]
        with open(f"/proc/{pid}/environ", "rb") as handle:
            environment = handle.read().split(b"\0")
    except (FileNotFoundError, ProcessLookupError):
        return None
    token_entry = f"SYNAPSE_REMOTE_JOB_TOKEN={ownership_token}".encode("ascii")
    return (actual_pgid, boot_id, start_time, token_entry in environment)

if pid <= 1 or not pgid_text.isascii() or not pgid_text.isdigit() or int(pgid_text) <= 1:
    emit("invalid_metadata")
    raise SystemExit(2)

expected = (pgid_text, expected_boot_id, expected_start_time, True)
before = read_identity()
if before is None:
    finish_if_group_empty("already_gone")
if before != expected:
    emit("identity_mismatch")
    raise SystemExit(3)

try:
    pidfd = os.pidfd_open(pid, 0)
except ProcessLookupError:
    finish_if_group_empty("already_gone")
try:
    after = read_identity()
    if after is None:
        finish_if_group_empty("already_gone")
    if after != expected:
        emit("identity_mismatch")
        raise SystemExit(3)
    try:
        signal.pidfd_send_signal(pidfd, signal.SIGTERM, None, 0)
    except ProcessLookupError:
        finish_if_group_empty("already_gone")
    poller = select.poll()
    poller.register(pidfd, select.POLLIN | select.POLLHUP | select.POLLERR)
    if poller.poll(__PIDFD_WAIT_MS__):
        for _attempt in range(__GROUP_ABSENCE_PROBE_ATTEMPTS__):
            _members, group_exists = inspect_group()
            if not group_exists:
                emit("terminated")
                raise SystemExit(0)
            time.sleep(__GROUP_ABSENCE_PROBE_INTERVAL_MS__ / 1000)
    emit("still_running")
    raise SystemExit(1)
finally:
    os.close(pidfd)
"#;
    let cleanup_script = SCRIPT
        .replace(
            "__PIDFD_WAIT_MS__",
            &SHELL_REMOTE_CLEANUP_PIDFD_WAIT_MS.to_string(),
        )
        .replace(
            "__GROUP_ABSENCE_PROBE_ATTEMPTS__",
            &SHELL_REMOTE_GROUP_ABSENCE_PROBE_ATTEMPTS.to_string(),
        )
        .replace(
            "__GROUP_ABSENCE_PROBE_INTERVAL_MS__",
            &SHELL_REMOTE_GROUP_ABSENCE_PROBE_INTERVAL_MS.to_string(),
        );
    let script = format!("{SHELL_REMOTE_GROUP_INSPECTION_FUNCTION_PY}\n{cleanup_script}");
    format!(
        "python3 -c {} {} {} {} {} {} {}",
        posix_single_quote(&script),
        posix_single_quote(pid),
        posix_single_quote(pgid),
        posix_single_quote(&identity.boot_id),
        posix_single_quote(&identity.start_time),
        posix_single_quote(&identity.ownership_token),
        posix_single_quote(SHELL_REMOTE_CLEANUP_MARKER),
    )
}

fn ssh_remote_liveness_command(pid: &str, pgid: &str) -> String {
    const SCRIPT: &str = r#"pid_text = sys.argv[1]
pgid_text = sys.argv[2]
marker = sys.argv[3]

def emit(status, member_count=0):
    print(
        f"{marker} pid={pid_text} pgid={pgid_text} status={status} member_count={member_count}",
        flush=True,
    )

if (
    not pid_text.isascii()
    or not pid_text.isdigit()
    or int(pid_text) <= 1
    or not pgid_text.isascii()
    or not pgid_text.isdigit()
    or int(pgid_text) <= 1
):
    emit("invalid_metadata")
    raise SystemExit(2)
try:
    expected_pgid = int(pgid_text)
    members = live_process_ids_in_group(expected_pgid)
    group_exists = process_group_exists(expected_pgid)
except Exception as error:
    print(
        f"process_group_inspection_failed pgid={pgid_text} error={error}",
        file=sys.stderr,
        flush=True,
    )
    emit("inspection_failed")
    raise SystemExit(3)
if group_exists:
    emit("alive", len(members))
else:
    emit("already_gone", 0)
"#;
    let script = format!("{SHELL_REMOTE_GROUP_INSPECTION_FUNCTION_PY}\n{SCRIPT}");
    format!(
        "python3 -c {} {} {} {}",
        posix_single_quote(&script),
        posix_single_quote(pid),
        posix_single_quote(pgid),
        posix_single_quote(SHELL_REMOTE_LIVENESS_MARKER),
    )
}

fn parse_remote_cleanup_status(
    stdout: &str,
    expected_pid: &str,
    expected_pgid: &str,
    expected_identity: Option<&RemoteProcessOwnershipIdentity>,
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
        if let Some(identity) = expected_identity {
            if fields.get("boot_id").map(String::as_str) != Some(identity.boot_id.as_str())
                || fields.get("start_time").map(String::as_str)
                    != Some(identity.start_time.as_str())
                || fields.get("ownership_token").map(String::as_str)
                    != Some(identity.ownership_token.as_str())
            {
                continue;
            }
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
        local_process_identity: None,
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
        spawn_failure: None,
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

enum SpawnShellJobChildFailure {
    BeforeSpawn(ErrorData),
    AfterSpawn(Box<PostSpawnShellJobChildFailure>),
}

struct PostSpawnShellJobChildFailure {
    error: ErrorData,
    child: tokio::process::Child,
    process_job: Option<OwnedProcessJob>,
    pid: Option<u32>,
    local_process_identity: Option<ActRunShellLocalProcessIdentity>,
    readback: ActRunShellSpawnFailureReadback,
}

fn merge_exact_child_reap_readbacks(
    initial: ExactChildReapReadback,
    post_job_close: Option<ExactChildReapReadback>,
) -> ExactChildReapReadback {
    let Some(post) = post_job_close else {
        return initial;
    };
    let reaped = initial.reaped || post.reaped;
    ExactChildReapReadback {
        kill_error: initial.kill_error.or(post.kill_error),
        reaped,
        exit_code: post.exit_code.or(initial.exit_code),
        exit_status: post.exit_status.or(initial.exit_status),
        timed_out: !reaped && (initial.timed_out || post.timed_out),
        poll_attempts: initial.poll_attempts.saturating_add(post.poll_attempts),
        poll_error_count: initial
            .poll_error_count
            .saturating_add(post.poll_error_count),
        last_poll_error: post.last_poll_error.or(initial.last_poll_error),
        elapsed_ms: initial.elapsed_ms.saturating_add(post.elapsed_ms),
    }
}

fn spawn_failure_readback(
    stage: &'static str,
    cleanup: &ExactChildReapReadback,
    process_job_acquired: bool,
    process_job_close: Option<&Result<(), String>>,
    tree_cleanup_verified: bool,
    final_identity_state: Option<&LocalProcessIdentityState>,
    cleanup_verified: bool,
) -> ActRunShellSpawnFailureReadback {
    ActRunShellSpawnFailureReadback {
        stage: stage.to_owned(),
        child_created: true,
        cleanup_verified,
        exact_child_reaped: cleanup.reaped,
        exact_child_kill_error: cleanup.kill_error.clone(),
        exact_child_reap_timed_out: cleanup.timed_out,
        exact_child_reap_poll_attempts: cleanup.poll_attempts,
        exact_child_reap_poll_error_count: cleanup.poll_error_count,
        exact_child_reap_last_poll_error: cleanup.last_poll_error.clone(),
        exact_child_reap_elapsed_ms: cleanup.elapsed_ms,
        process_job_acquired,
        process_job_close: process_job_close.map(|result| format!("{result:?}")),
        tree_cleanup_verified,
        final_identity_state: final_identity_state.map(|state| format!("{state:?}")),
        exact_owner_retained: false,
    }
}

fn spawn_shell_job_child(
    params: &ActRunShellStartParams,
    spawn_plan: &ShellJobSpawnPlan,
    stdout_file: fs::File,
    stderr_file: fs::File,
    context: Option<&ShellExecutionContext>,
) -> Result<SpawnedShellChild, SpawnShellJobChildFailure> {
    let spawn_command = shell_spawn_command(&spawn_plan.command);
    let mut command = TokioCommand::new(spawn_command.as_ref());
    command.args(&spawn_plan.args);
    if let Some(working_dir) = &params.working_dir {
        command.current_dir(working_dir);
    }
    command.env_clear();
    let mut env = child_base_environment();
    ensure_child_temp_environment(&mut env);
    validate_child_base_environment(&env, "act_run_shell")
        .map_err(SpawnShellJobChildFailure::BeforeSpawn)?;
    for (_sort_key, (key, value)) in env {
        command.env(key, value);
    }
    command.envs(&params.env);
    apply_shell_session_environment(&mut command, params.working_dir.as_deref(), context);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        // The monitor and the kill-on-close job are the intended long-lived
        // owners. Before job assignment succeeds, however, Tokio's exact child
        // handle is the only authority available on several error paths. Keep
        // kill-on-drop armed so an exceptional unwind/failed bounded reap
        // cannot detach a suspended or uncontained child.
        .kill_on_drop(true);
    apply_no_window_tokio(&mut command);

    let mut child = command
        .spawn()
        .map_err(|error| {
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
        })
        .map_err(SpawnShellJobChildFailure::BeforeSpawn)?;
    let Some(pid) = child.id() else {
        let cleanup = terminate_and_reap_tokio_child_bounded(
            &mut child,
            Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
        );
        let tree_cleanup_verified = cfg!(windows);
        let cleanup_verified = cleanup.reaped && tree_cleanup_verified;
        let readback = spawn_failure_readback(
            "pid_unavailable",
            &cleanup,
            false,
            None,
            tree_cleanup_verified,
            None,
            cleanup_verified,
        );
        let error = shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            if cleanup_verified {
                "act_run_shell_start spawned a child process without an observable pid; the exact child handle was terminated and reaped before refusing the spawn"
            } else {
                "act_run_shell_start spawned a child process without an observable pid and could not verify exact-child reaping before the cleanup backstop"
            },
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "command": params.command,
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": shell_command_metadata(&params.command, &params.args).args,
                "args_sha256": shell_args_sha256(&params.args),
                "working_dir": params.working_dir,
                "reason": "pid_unavailable",
                "cleanup_verified": cleanup.reaped,
                "cleanup": cleanup,
            }),
        );
        return Err(SpawnShellJobChildFailure::AfterSpawn(Box::new(
            PostSpawnShellJobChildFailure {
                error,
                child,
                process_job: None,
                pid: None,
                local_process_identity: None,
                readback,
            },
        )));
    };
    // Containment only needs the exact suspended child's PID. Acquire and read
    // back the kill-on-close job before identity capture so a GetProcessTimes
    // failure cannot leave an uncontained suspended process behind.
    let mut process_job = match assign_owned_process_job(
        pid,
        "act_run_shell_start",
        params.job_id.as_deref(),
    ) {
        Ok(process_job) => process_job,
        Err(assignment_error) => {
            let cleanup = terminate_and_reap_tokio_child_bounded(
                &mut child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            );
            let tree_cleanup_verified = cfg!(windows);
            let cleanup_verified = cleanup.reaped && tree_cleanup_verified;
            let readback = spawn_failure_readback(
                "job_object_assignment_failed",
                &cleanup,
                false,
                None,
                tree_cleanup_verified,
                None,
                cleanup_verified,
            );
            let error = shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "act_run_shell_start could not establish kill-on-close ownership for spawned pid {pid}; exact-child cleanup verified={}: {}",
                    cleanup_verified, assignment_error.message
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "reason": "job_object_assignment_failed",
                    "assignment_error": assignment_error,
                    "cleanup_verified": cleanup.reaped,
                    "cleanup": cleanup,
                }),
            );
            return Err(SpawnShellJobChildFailure::AfterSpawn(Box::new(
                PostSpawnShellJobChildFailure {
                    error,
                    child,
                    process_job: None,
                    pid: Some(pid),
                    local_process_identity: None,
                    readback,
                },
            )));
        }
    };
    let local_process_identity = match capture_local_process_identity(pid) {
        Ok(identity) => identity,
        Err(identity_error) => {
            let initial_cleanup = terminate_and_reap_tokio_child_bounded(
                &mut child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            );
            let job_close = process_job.close_checked();
            let post_job_close_cleanup = (!initial_cleanup.reaped).then(|| {
                terminate_and_reap_tokio_child_bounded(
                    &mut child,
                    Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
                )
            });
            let cleanup = merge_exact_child_reap_readbacks(initial_cleanup, post_job_close_cleanup);
            let tree_cleanup_verified = cfg!(windows);
            let cleanup_verified = cleanup.reaped && job_close.is_ok() && tree_cleanup_verified;
            let readback = spawn_failure_readback(
                "local_process_identity_capture_failed",
                &cleanup,
                true,
                Some(&job_close),
                tree_cleanup_verified,
                None,
                cleanup_verified,
            );
            let error = shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "act_run_shell_start could not bind contained pid {pid} to its kernel creation identity; cleanup_verified={cleanup_verified}; identity_error={identity_error}; job_close={job_close:?}"
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "reason": "local_process_identity_capture_failed",
                    "identity_error": identity_error,
                    "cleanup_verified": cleanup_verified,
                    "cleanup": cleanup,
                    "job_close": format!("{job_close:?}"),
                }),
            );
            return Err(SpawnShellJobChildFailure::AfterSpawn(Box::new(
                PostSpawnShellJobChildFailure {
                    error,
                    child,
                    process_job: Some(process_job),
                    pid: Some(pid),
                    local_process_identity: None,
                    readback,
                },
            )));
        }
    };
    if let Err(resume_error) = resume_suspended_shell_child(&local_process_identity) {
        let initial_cleanup = terminate_and_reap_tokio_child_bounded(
            &mut child,
            Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
        );
        let job_close = process_job.close_checked();
        let state_after_job_close = local_process_identity_state(&local_process_identity);
        let post_job_close_cleanup = (!initial_cleanup.reaped
            || !terminal_local_process_identity_state(&state_after_job_close))
        .then(|| {
            terminate_and_reap_tokio_child_bounded(
                &mut child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            )
        });
        let cleanup = merge_exact_child_reap_readbacks(initial_cleanup, post_job_close_cleanup);
        let final_identity_state = local_process_identity_state(&local_process_identity);
        let tree_cleanup_verified = cfg!(windows);
        let cleanup_verified = cleanup.reaped
            && job_close.is_ok()
            && tree_cleanup_verified
            && terminal_local_process_identity_state(&final_identity_state);
        let readback = spawn_failure_readback(
            "contained_child_resume_failed",
            &cleanup,
            true,
            Some(&job_close),
            tree_cleanup_verified,
            Some(&final_identity_state),
            cleanup_verified,
        );
        let error = shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "act_run_shell_start could not safely resume contained pid {pid}; cleanup_verified={cleanup_verified}; job_close={job_close:?}; final_identity_state={final_identity_state:?}: {resume_error}",
            ),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "reason": "contained_child_resume_failed",
                "resume_error": resume_error,
                "job_close": format!("{job_close:?}"),
                "cleanup_verified": cleanup_verified,
                "cleanup": cleanup,
                "final_identity_state": final_identity_state,
            }),
        );
        return Err(SpawnShellJobChildFailure::AfterSpawn(Box::new(
            PostSpawnShellJobChildFailure {
                error,
                child,
                process_job: Some(process_job),
                pid: Some(pid),
                local_process_identity: Some(local_process_identity),
                readback,
            },
        )));
    }
    Ok(SpawnedShellChild {
        child,
        process_job,
        local_process_identity,
    })
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
    mut process_job: OwnedProcessJob,
    mut status: ActRunShellJobStatus,
    paths: ShellJobPaths,
    started: Instant,
    original_args: Vec<String>,
) {
    let local_process_identity = status.local_process_identity.clone();
    let (exit_code, timed_out, wait_error) = wait_shell_job_child_with_identity(
        &mut child,
        local_process_identity.as_ref(),
        status.timeout_ms,
        started,
    )
    .await;
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
    if let Err(error) = process_job.close_checked() {
        let prior_error = status.error_message.take();
        status.status = "job_handle_close_failed".to_owned();
        status.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
        status.error_message = Some(match prior_error {
            Some(prior) => format!("{prior}; owned process job close failed: {error}"),
            None => format!("owned process job close failed: {error}"),
        });
        tracing::error!(
            code = "M4_ACT_RUN_SHELL_JOB_HANDLE_CLOSE_FAILED",
            job_id = %status.job_id,
            pid = ?status.pid,
            error,
            "durable shell monitor could not verify owned Windows job handle closure"
        );
    }
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

/// Independent OS handle to a shell job's child process, opened before the child
/// is reaped so its kernel-recorded creation/exit timing stays readable even
/// after tokio closes its own handle.
///
/// The measured runtime (`exit - creation`) is the source of truth for durable
/// timeout enforcement: unlike any wall clock the monitor task samples, it is
/// immune to how late scheduler starvation dispatches that task (#1580/#1588).
// The raw `HANDLE` value, not a `windows::HANDLE` (which is `!Send`): the probe
// is held across `child.wait().await`, and the monitor future must stay `Send`
// for `tokio::spawn`. A Windows process handle is process-wide, so using it from
// whichever worker polls the future is sound.
#[cfg(windows)]
struct ChildRuntimeProbe(isize);

#[cfg(windows)]
impl ChildRuntimeProbe {
    fn capture(child: &tokio::process::Child) -> Option<Self> {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // `id()` is Some until tokio reaps the child; our own handle then keeps
        // the (possibly already-exited) process object alive for the query.
        let pid = child.id()?;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        Some(Self(handle.0 as isize))
    }

    fn handle(&self) -> windows::Win32::Foundation::HANDLE {
        windows::Win32::Foundation::HANDLE(self.0 as *mut core::ffi::c_void)
    }

    /// Wall-clock process runtime (`exit - creation`), or `None` while the
    /// process is still running or if the timing query fails.
    fn runtime(&self) -> Option<Duration> {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::GetProcessTimes;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            GetProcessTimes(
                self.handle(),
                std::ptr::addr_of_mut!(creation),
                std::ptr::addr_of_mut!(exit),
                std::ptr::addr_of_mut!(kernel),
                std::ptr::addr_of_mut!(user),
            )
        }
        .ok()?;
        let creation = filetime_ticks_100ns(creation);
        let exit = filetime_ticks_100ns(exit);
        // `exit` reads zero until the process has actually exited.
        (exit != 0 && exit >= creation)
            .then(|| Duration::from_nanos((exit - creation).saturating_mul(100)))
    }
}

#[cfg(windows)]
impl Drop for ChildRuntimeProbe {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle()) };
    }
}

#[cfg(windows)]
fn filetime_ticks_100ns(value: windows::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

/// Non-Windows stub: the OS runtime probe is unavailable, so callers fall back
/// to the elapsed-since-spawn wall clock.
#[cfg(not(windows))]
struct ChildRuntimeProbe;

#[cfg(not(windows))]
impl ChildRuntimeProbe {
    fn capture(_child: &tokio::process::Child) -> Option<Self> {
        None
    }

    fn runtime(&self) -> Option<Duration> {
        None
    }
}

async fn wait_shell_job_child_with_identity(
    child: &mut tokio::process::Child,
    local_process_identity: Option<&ActRunShellLocalProcessIdentity>,
    timeout_ms: Option<u64>,
    started: Instant,
) -> (Option<i32>, bool, Option<String>) {
    match timeout_ms {
        Some(timeout_ms) => {
            let budget = Duration::from_millis(timeout_ms);
            // Open an independent OS handle to the child BEFORE waiting so its
            // kernel-recorded creation/exit timing stays readable even after
            // tokio reaps the child; that runtime is the budget source of truth.
            let runtime_probe = ChildRuntimeProbe::capture(child);
            // Arm the deadline against the SPAWN instant (`started`), not
            // wait-entry: under scheduler starvation this monitor task can be
            // dispatched only after the child already exited, and a
            // wait-entry-relative timer would grant a fresh full budget. Zero
            // once the cap has already elapsed, so the timeout fires promptly.
            let budget_remaining = budget.saturating_sub(started.elapsed());
            let child_outran_budget = || {
                runtime_probe
                    .as_ref()
                    .and_then(ChildRuntimeProbe::runtime)
                    .map_or(started.elapsed() >= budget, |runtime| runtime >= budget)
            };
            match tokio::time::timeout(budget_remaining, child.wait()).await {
                Ok(Ok(status)) => {
                    // SOURCE OF TRUTH: the OS-recorded process runtime
                    // (exit - creation) vs the cap. `tokio::time::timeout` polls
                    // the inner future first, so a child that self-exited after
                    // the deadline is still delivered here as `Ok(exit)`; and a
                    // starved monitor may observe the exit long after it happened
                    // (a wall clock this task samples then over- or under-counts
                    // in either direction). Only the kernel runtime reveals
                    // whether the child truly outran its cap (#1580/#1588). Fall
                    // back to elapsed-since-spawn only if the OS probe is
                    // unavailable.
                    (status.code(), child_outran_budget(), None)
                }
                Ok(Err(error)) => (None, false, Some(format!("wait_failed:{error}"))),
                Err(_elapsed) => {
                    // A zero/past deadline may win the select even though the
                    // exact root has already exited. Preserve that natural exit
                    // evidence before attempting destructive cleanup. The
                    // production monitor still owns the verified kill-on-close
                    // job until final status persistence and explicit close, so
                    // any descendant remains contained during this race.
                    if let Ok(Some(status)) = child.try_wait() {
                        return (status.code(), child_outran_budget(), None);
                    }
                    let termination = if let (Some(pid), Some(identity)) =
                        (child.id(), local_process_identity)
                    {
                        if pid == identity.pid {
                            // Exact-handle termination and identity-aware tree
                            // discovery are blocking OS work; keep them off the
                            // async executor so parallel timeouts remain causal.
                            let identity = identity.clone();
                            tokio::task::spawn_blocking(move || {
                                terminate_shell_job_process_tree(&identity)
                            })
                            .await
                            .unwrap_or_else(|join_error| {
                                ShellJobTerminationReadback {
                                    attempted: false,
                                    status: format!("termination_task_join_failed:{join_error}"),
                                    remaining_process_ids: vec![pid],
                                }
                            })
                        } else {
                            ShellJobTerminationReadback {
                                attempted: false,
                                status: format!(
                                    "timeout_identity_pid_mismatch:child_pid={pid}:identity_pid={}",
                                    identity.pid
                                ),
                                remaining_process_ids: vec![pid],
                            }
                        }
                    } else {
                        ShellJobTerminationReadback {
                            attempted: false,
                            status: "timeout_process_identity_unavailable".to_owned(),
                            remaining_process_ids: child.id().into_iter().collect(),
                        }
                    };
                    tracing::warn!(
                        code = "M4_ACT_RUN_SHELL_JOB_TIMEOUT_TREE_TERMINATED",
                        pid = ?child.id(),
                        attempted = termination.attempted,
                        status = %termination.status,
                        remaining_process_ids = ?termination.remaining_process_ids,
                        "act_run_shell_start timeout requested identity-bound process-tree termination"
                    );
                    let reap = terminate_and_reap_tokio_child_async_bounded(
                        child,
                        Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
                    )
                    .await;
                    let termination_verified = termination.verified_terminal_tree();
                    let cleanup_error = (!termination_verified
                        || !reap.reaped
                        || reap.poll_error_count > 0)
                        .then(|| {
                            format!(
                                "timeout_cleanup_unverified:tree={termination:?}:exact_child_reap={reap:?}"
                            )
                        });
                    // `TerminateProcess(..., 1)` produces an exit status, but
                    // that is cleanup evidence rather than a natural command
                    // verdict. The pre-termination `try_wait` branch above is
                    // the only timeout race that may preserve a real code.
                    let exit_code = if termination.attempted {
                        None
                    } else {
                        reap.exit_code
                    };
                    (exit_code, true, cleanup_error)
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
    if job.status == SHELL_JOB_STATUS_SPAWN_CLEANUP_UNVERIFIED {
        // Only the retained exact Child owner can prove reaping and promote
        // this record to `spawn_failed_reaped`. PID-table absence, especially
        // without a creation identity, is not an equivalent proof.
        return Ok(job);
    }
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
    if let Some(identity) = job.local_process_identity.as_ref() {
        if identity.pid != pid {
            return Err(shell_tool_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "shell job {} has inconsistent pid/creation identity: status pid={pid}, identity pid={}",
                    job.job_id, identity.pid
                ),
                json!({
                    "code": error_codes::STORAGE_READ_FAILED,
                    "job_id": job.job_id,
                    "reason": "job_local_process_identity_pid_mismatch",
                    "pid": pid,
                    "identity_pid": identity.pid,
                }),
            ));
        }
        match local_process_identity_state(identity) {
            LocalProcessIdentityState::Match => return Ok(job),
            LocalProcessIdentityState::Unreadable(detail) => {
                return Err(shell_tool_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "shell job {} local process identity could not be read: {detail}",
                        job.job_id
                    ),
                    json!({
                        "code": error_codes::STORAGE_READ_FAILED,
                        "job_id": job.job_id,
                        "reason": "job_local_process_identity_unreadable",
                        "pid": pid,
                        "detail": detail,
                    }),
                ));
            }
            LocalProcessIdentityState::Mismatch(actual) => {
                return Err(shell_tool_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "shell job {} local process identity mismatch for pid {pid}; refusing to reconcile or terminate a process whose creation identity differs from the persisted job owner",
                        job.job_id
                    ),
                    json!({
                        "code": error_codes::STORAGE_READ_FAILED,
                        "job_id": job.job_id,
                        "reason": "job_local_process_identity_mismatch",
                        "pid": pid,
                        "expected_identity": identity,
                        "actual_identity": actual,
                    }),
                ));
            }
            LocalProcessIdentityState::Exited | LocalProcessIdentityState::Absent => {}
        }
    } else if shell_job_live_process_ids(&[pid]).contains(&pid) {
        // Legacy status records have no immutable process creation identity.
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
    write_shell_job_reconciliation_status_before_lock(paths, candidate, || {})
}

/// Reconcile a status while serializing the latest-state read, preservation
/// decision, and optional commit under the same destination writer lock.
/// `before_lock` is a scheduling observation seam used by the concurrency
/// regression; production passes a no-op closure.
fn write_shell_job_reconciliation_status_before_lock<F>(
    paths: &ShellJobPaths,
    candidate: ActRunShellJobStatus,
    before_lock: F,
) -> Result<ActRunShellJobStatus, ErrorData>
where
    F: FnOnce(),
{
    before_lock();
    let write_lock = shell_status_write_lock(&paths.status_path);
    let _write_guard = write_lock.lock().map_err(|error| {
        shell_tool_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_run_shell reconciliation writer lock was poisoned for {}: {error}",
                paths.status_path.display()
            ),
            json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "job_id": candidate.job_id,
                "path": paths.status_path,
                "reason": "job_status_reconciliation_writer_lock_poisoned",
            }),
        )
    })?;
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
    write_shell_job_status_locked(&paths.status_path, &candidate)
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
    matches!(
        status,
        "running" | "cancel_requested" | SHELL_JOB_STATUS_SPAWN_CLEANUP_UNVERIFIED
    )
}

fn shell_job_terminal_status(status: &str) -> bool {
    !matches!(
        status,
        "running" | "cancel_requested" | "finalizing" | SHELL_JOB_STATUS_SPAWN_CLEANUP_UNVERIFIED
    )
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
    // Prevent user code from creating descendants before immutable process
    // identity capture and kill-on-close job ownership are verified.
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

#[cfg(not(windows))]
fn apply_no_window_tokio(_command: &mut TokioCommand) {}

#[cfg(windows)]
fn apply_no_window_std(command: &mut StdCommand) {
    use std::os::windows::process::CommandExt;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

#[cfg(not(windows))]
fn apply_no_window_std(_command: &mut StdCommand) {}

#[derive(Debug, Serialize)]
struct ShellJobTerminationReadback {
    attempted: bool,
    status: String,
    remaining_process_ids: Vec<u32>,
}

impl ShellJobTerminationReadback {
    fn verified_terminal_tree(&self) -> bool {
        if !self.remaining_process_ids.is_empty() {
            return false;
        }
        matches!(self.status.as_str(), "terminated" | "already_exited")
            || (self.status.starts_with("identity_verification_failed:")
                && (self.status.contains("root_readback=exited")
                    || self.status.contains("root_readback=absent")))
    }
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

    let expected_root = match capture_local_process_identity(pid) {
        Ok(identity) => identity,
        Err(error) => {
            return OwnedProcessTerminationReadback {
                pid,
                process_ids,
                live_process_ids_before,
                attempted: false,
                status: format!("identity_capture_failed:{error}"),
                remaining_process_ids: vec![pid],
            };
        }
    };
    let termination = terminate_shell_job_process_tree(&expected_root);
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
    let mut identities = Vec::new();
    let mut identity_failures = Vec::new();
    for pid in &live_process_ids_before {
        match capture_local_process_identity(*pid) {
            Ok(identity) => identities.push(identity),
            Err(error) => identity_failures.push(format!("pid {pid}: {error}")),
        }
    }
    if !identity_failures.is_empty() {
        return OwnedProcessTerminationReadback {
            pid: 0,
            process_ids,
            live_process_ids_before: live_process_ids_before.clone(),
            attempted: false,
            status: format!("identity_capture_failed:{}", identity_failures.join(" | ")),
            remaining_process_ids: live_process_ids_before,
        };
    }
    let termination = terminate_shell_job_process_tree_platform(&identities);
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

fn terminate_shell_job_process_tree(
    expected_root: &ActRunShellLocalProcessIdentity,
) -> ShellJobTerminationReadback {
    let identities = match shell_job_process_tree_identities(expected_root) {
        Ok(identities) if identities.is_empty() => {
            return ShellJobTerminationReadback {
                attempted: false,
                status: "already_exited".to_owned(),
                remaining_process_ids: Vec::new(),
            };
        }
        Ok(identities) => identities,
        Err(error) => {
            let (identity_readback, remaining_process_ids) =
                match local_process_identity_state(expected_root) {
                    LocalProcessIdentityState::Match => {
                        ("match".to_owned(), vec![expected_root.pid])
                    }
                    LocalProcessIdentityState::Mismatch(actual) => {
                        (format!("mismatch:{actual:?}"), Vec::new())
                    }
                    LocalProcessIdentityState::Exited => ("exited".to_owned(), Vec::new()),
                    LocalProcessIdentityState::Absent => ("absent".to_owned(), Vec::new()),
                    LocalProcessIdentityState::Unreadable(read_error) => {
                        (format!("unreadable:{read_error}"), vec![expected_root.pid])
                    }
                };
            return ShellJobTerminationReadback {
                attempted: false,
                status: format!(
                    "identity_verification_failed:{error}; root_readback={identity_readback}"
                ),
                remaining_process_ids,
            };
        }
    };
    terminate_shell_job_process_tree_platform(&identities)
}

fn terminate_shell_job_from_status(job: &ActRunShellJobStatus) -> ShellJobTerminationReadback {
    let Some(expected_root) = job.local_process_identity.as_ref() else {
        return ShellJobTerminationReadback {
            attempted: false,
            status: "identity_unavailable_refused".to_owned(),
            remaining_process_ids: job
                .pid
                .filter(|pid| shell_job_live_process_ids(&[*pid]).contains(pid))
                .into_iter()
                .collect(),
        };
    };
    if job.pid != Some(expected_root.pid) {
        return ShellJobTerminationReadback {
            attempted: false,
            status: "identity_pid_mismatch_refused".to_owned(),
            remaining_process_ids: job.pid.into_iter().collect(),
        };
    }
    terminate_shell_job_process_tree(expected_root)
}

#[cfg(windows)]
fn capture_local_process_identity(pid: u32) -> Result<ActRunShellLocalProcessIdentity, String> {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(|error| {
            format!("OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) failed for pid {pid}: {error}")
        })?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let query = unsafe {
        GetProcessTimes(
            handle,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .map(|()| filetime_ticks_100ns(creation))
    .map_err(|error| format!("GetProcessTimes failed for pid {pid}: {error}"));
    let close = unsafe { CloseHandle(handle) }
        .map_err(|error| format!("CloseHandle(process identity pid {pid}) failed: {error}"));
    match (query, close) {
        (Ok(start_time), Ok(())) => Ok(ActRunShellLocalProcessIdentity {
            pid,
            start_time,
            start_time_source: "windows_filetime_100ns".to_owned(),
        }),
        (Err(query_error), Ok(())) => Err(query_error),
        (Ok(_), Err(close_error)) => Err(close_error),
        (Err(query_error), Err(close_error)) => Err(format!("{query_error}; {close_error}")),
    }
}

#[cfg(not(windows))]
fn capture_local_process_identity(pid: u32) -> Result<ActRunShellLocalProcessIdentity, String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let pid_value = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid_value]), true);
    let process = system
        .process(pid_value)
        .ok_or_else(|| format!("process {pid} is absent while capturing its start time"))?;
    Ok(ActRunShellLocalProcessIdentity {
        pid,
        start_time: process.start_time(),
        start_time_source: "sysinfo_process_start_time".to_owned(),
    })
}

#[cfg(windows)]
fn resume_suspended_shell_child(identity: &ActRunShellLocalProcessIdentity) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{
                GetProcessTimes, OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, ResumeThread, THREAD_QUERY_LIMITED_INFORMATION,
                THREAD_SUSPEND_RESUME, WaitForSingleObject,
            },
        },
    };

    let thread_entry_size = u32::try_from(std::mem::size_of::<THREADENTRY32>())
        .map_err(|error| format!("THREADENTRY32 size conversion failed: {error}"))?;

    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            identity.pid,
        )
    }
    .map_err(|error| format!("open suspended child pid {} failed: {error}", identity.pid))?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let identity_result = unsafe {
        GetProcessTimes(
            process,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .map(|()| filetime_ticks_100ns(creation))
    .map_err(|error| format!("GetProcessTimes before resume failed: {error}"));
    if identity_result
        .as_ref()
        .is_ok_and(|start| *start != identity.start_time)
    {
        let actual = identity_result.unwrap_or_default();
        let close = unsafe { CloseHandle(process) }.map_err(|error| error.to_string());
        return Err(format!(
            "suspended child pid {} identity changed before resume: expected={} actual={actual}; process_close={close:?}",
            identity.pid, identity.start_time
        ));
    }
    if let Err(error) = identity_result {
        let close = unsafe { CloseHandle(process) }.map_err(|error| error.to_string());
        return Err(format!(
            "could not verify suspended child pid {} identity before resume: {error}; process_close={close:?}",
            identity.pid
        ));
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.map_err(|error| {
        let process_close = unsafe { CloseHandle(process) }.map_err(|error| error.to_string());
        format!("thread snapshot before resume failed: {error}; process_close={process_close:?}")
    })?;
    let mut entry = THREADENTRY32 {
        dwSize: thread_entry_size,
        ..Default::default()
    };
    let mut thread_ids = Vec::new();
    if unsafe { Thread32First(snapshot, &mut entry) }.is_ok() {
        loop {
            if entry.th32OwnerProcessID == identity.pid {
                thread_ids.push(entry.th32ThreadID);
            }
            if unsafe { Thread32Next(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let snapshot_close = unsafe { CloseHandle(snapshot) }.map_err(|error| error.to_string());
    if snapshot_close.is_err() || thread_ids.len() != 1 {
        let process_close = unsafe { CloseHandle(process) }.map_err(|error| error.to_string());
        return Err(format!(
            "CREATE_SUSPENDED child pid {} must expose exactly one primary thread before resume; thread_ids={thread_ids:?}; snapshot_close={snapshot_close:?}; process_close={process_close:?}",
            identity.pid
        ));
    }

    let thread_id = thread_ids[0];
    let thread = unsafe {
        OpenThread(
            THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
            false,
            thread_id,
        )
    }
    .map_err(|error| {
        let process_close = unsafe { CloseHandle(process) }.map_err(|error| error.to_string());
        format!(
            "open CREATE_SUSPENDED primary thread {thread_id} for pid {} failed: {error}; process_close={process_close:?}",
            identity.pid
        )
    })?;
    let previous_suspend_count = unsafe { ResumeThread(thread) };
    let thread_close = unsafe { CloseHandle(thread) }.map_err(|error| error.to_string());
    let initial_wait = unsafe { WaitForSingleObject(process, 0) };
    let wait_error = (initial_wait == WAIT_FAILED)
        .then(windows::core::Error::from_thread)
        .map(|error| error.to_string());
    let (state_valid, state_detail) = if initial_wait == WAIT_OBJECT_0 {
        (
            true,
            "exact_process_handle_signaled_after_resume".to_owned(),
        )
    } else if initial_wait == WAIT_TIMEOUT {
        let states = process_tree_suspend_state_platform(&[identity.pid]);
        if states.len() == 1 && states[0].suspended_threads == 0 {
            (true, format!("live_thread_state={states:?}"))
        } else {
            // The process may have exited between the initial exact-handle wait
            // and the thread-table read. Keep the exact handle open and accept
            // only a now-signaled readback; an empty state alone is ambiguous.
            let post_state_wait = unsafe { WaitForSingleObject(process, 0) };
            (
                post_state_wait == WAIT_OBJECT_0,
                format!(
                    "thread_state={states:?}; post_state_exact_handle_wait={post_state_wait:?}"
                ),
            )
        }
    } else {
        (
            false,
            format!("unexpected_exact_handle_wait={initial_wait:?}"),
        )
    };
    let process_close = unsafe { CloseHandle(process) }.map_err(|error| error.to_string());
    if previous_suspend_count != 1
        || thread_close.is_err()
        || process_close.is_err()
        || !state_valid
    {
        return Err(format!(
            "documented primary-thread resume readback failed for pid {}: previous_suspend_count={previous_suspend_count}; thread_close={thread_close:?}; initial_process_wait={initial_wait:?}; wait_error={wait_error:?}; state_detail={state_detail}; process_close={process_close:?}",
            identity.pid
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn resume_suspended_shell_child(_identity: &ActRunShellLocalProcessIdentity) -> Result<(), String> {
    Ok(())
}

/// Physical readback for one immutable process identity. Destructive callers
/// must not collapse an unreadable process table/handle into "not running": an
/// access/query failure is uncertainty, while a different creation time is a
/// safe refusal and a genuine not-found result is the only absence proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
enum LocalProcessIdentityState {
    Match,
    Mismatch(ActRunShellLocalProcessIdentity),
    Exited,
    Absent,
    Unreadable(String),
}

#[cfg(windows)]
fn local_process_identity_state(
    expected: &ActRunShellLocalProcessIdentity,
) -> LocalProcessIdentityState {
    use windows::Win32::{
        Foundation::{
            CloseHandle, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND, FILETIME, WAIT_FAILED,
            WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
        },
        System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    };

    let handle = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            expected.pid,
        )
    } {
        Ok(handle) => handle,
        Err(error) => {
            return match WIN32_ERROR::from_error(&error) {
                Some(code) if code == ERROR_INVALID_PARAMETER || code == ERROR_NOT_FOUND => {
                    LocalProcessIdentityState::Absent
                }
                _ => LocalProcessIdentityState::Unreadable(format!(
                    "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) failed for pid {}: {error}",
                    expected.pid
                )),
            };
        }
    };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let query = unsafe {
        GetProcessTimes(
            handle,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .map_err(|error| format!("GetProcessTimes failed for pid {}: {error}", expected.pid))
    .and_then(|()| {
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        if wait == WAIT_FAILED {
            return Err(format!(
                "WaitForSingleObject(identity pid {}) failed: {}",
                expected.pid,
                windows::core::Error::from_thread()
            ));
        }
        if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT {
            return Err(format!(
                "WaitForSingleObject(identity pid {}) returned unexpected state {wait:?}",
                expected.pid
            ));
        }
        Ok((
            ActRunShellLocalProcessIdentity {
                pid: expected.pid,
                start_time: filetime_ticks_100ns(creation),
                start_time_source: "windows_filetime_100ns".to_owned(),
            },
            wait == WAIT_OBJECT_0,
        ))
    });
    let close = unsafe { CloseHandle(handle) }.map_err(|error| {
        format!(
            "CloseHandle(identity readback pid {}) failed: {error}",
            expected.pid
        )
    });
    match (query, close) {
        (Ok((actual, _)), Ok(())) if actual != *expected => {
            LocalProcessIdentityState::Mismatch(actual)
        }
        (Ok((_actual, true)), Ok(())) => LocalProcessIdentityState::Exited,
        (Ok((_actual, false)), Ok(())) => LocalProcessIdentityState::Match,
        (Err(query_error), Ok(())) => LocalProcessIdentityState::Unreadable(query_error),
        (Ok(_), Err(close_error)) => LocalProcessIdentityState::Unreadable(close_error),
        (Err(query_error), Err(close_error)) => {
            LocalProcessIdentityState::Unreadable(format!("{query_error}; {close_error}"))
        }
    }
}

#[cfg(not(windows))]
fn local_process_identity_state(
    expected: &ActRunShellLocalProcessIdentity,
) -> LocalProcessIdentityState {
    use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};

    let pid = Pid::from_u32(expected.pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let Some(process) = system.process(pid) else {
        return LocalProcessIdentityState::Absent;
    };
    let actual = ActRunShellLocalProcessIdentity {
        pid: expected.pid,
        start_time: process.start_time(),
        start_time_source: "sysinfo_process_start_time".to_owned(),
    };
    if actual != *expected {
        LocalProcessIdentityState::Mismatch(actual)
    } else if process.status() == ProcessStatus::Zombie {
        LocalProcessIdentityState::Exited
    } else {
        LocalProcessIdentityState::Match
    }
}

#[cfg(windows)]
fn local_identity_sysinfo_start_time(
    identity: &ActRunShellLocalProcessIdentity,
) -> Result<u64, String> {
    const WINDOWS_TO_UNIX_EPOCH_TICKS_100NS: u64 = 116_444_736_000_000_000;
    if identity.start_time_source != "windows_filetime_100ns" {
        return Err(format!(
            "unexpected Windows process identity source {}",
            identity.start_time_source
        ));
    }
    identity
        .start_time
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_TICKS_100NS)
        .map(|ticks| ticks / 10_000_000)
        .ok_or_else(|| {
            format!(
                "Windows process creation FILETIME {} predates Unix epoch",
                identity.start_time
            )
        })
}

#[cfg(not(windows))]
fn local_identity_sysinfo_start_time(
    identity: &ActRunShellLocalProcessIdentity,
) -> Result<u64, String> {
    if identity.start_time_source != "sysinfo_process_start_time" {
        return Err(format!(
            "unexpected process identity source {}",
            identity.start_time_source
        ));
    }
    Ok(identity.start_time)
}

fn shell_job_process_tree_identities(
    expected_root: &ActRunShellLocalProcessIdentity,
) -> Result<Vec<ActRunShellLocalProcessIdentity>, String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    match local_process_identity_state(expected_root) {
        LocalProcessIdentityState::Match => {}
        LocalProcessIdentityState::Exited => return Ok(Vec::new()),
        LocalProcessIdentityState::Absent => return Ok(Vec::new()),
        LocalProcessIdentityState::Mismatch(actual) => {
            return Err(format!(
                "root pid {} identity changed: expected start={} source={}; actual start={} source={}",
                expected_root.pid,
                expected_root.start_time,
                expected_root.start_time_source,
                actual.start_time,
                actual.start_time_source,
            ));
        }
        LocalProcessIdentityState::Unreadable(error) => {
            return Err(format!(
                "root pid {} identity readback was unavailable: {error}",
                expected_root.pid
            ));
        }
    }

    let mut system = System::new_all();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let root_pid = Pid::from_u32(expected_root.pid);
    let root_process = system.process(root_pid).ok_or_else(|| {
        format!(
            "root pid {} disappeared after exact identity verification",
            expected_root.pid
        )
    })?;
    let mut process_ids = vec![expected_root.pid];
    process_ids.extend(shell_job_descendant_process_ids(
        &system,
        expected_root.pid,
        root_process.start_time(),
    ));
    process_ids.sort_unstable();
    process_ids.dedup();
    let snapshot_start_times = process_ids
        .iter()
        .filter_map(|pid| {
            system
                .process(Pid::from_u32(*pid))
                .map(|process| (*pid, process.start_time()))
        })
        .collect::<Vec<_>>();

    let mut identities = Vec::with_capacity(process_ids.len());
    for pid in process_ids {
        let identity = capture_local_process_identity(pid).map_err(|error| {
            format!("could not bind process-tree pid {pid} before termination: {error}")
        })?;
        let snapshot_start_time = snapshot_start_times
            .iter()
            .find_map(|(snapshot_pid, start_time)| (*snapshot_pid == pid).then_some(*start_time))
            .ok_or_else(|| format!("process-tree pid {pid} had no snapshot start time"))?;
        let exact_start_time = local_identity_sysinfo_start_time(&identity)?;
        if exact_start_time != snapshot_start_time {
            return Err(format!(
                "process-tree pid {pid} changed between ancestry snapshot and exact identity capture: snapshot_start={snapshot_start_time} exact_start={exact_start_time}"
            ));
        }
        identities.push(identity);
    }
    if identities
        .iter()
        .find(|identity| identity.pid == expected_root.pid)
        != Some(expected_root)
    {
        return Err(format!(
            "root pid {} identity changed during process-tree capture",
            expected_root.pid
        ));
    }

    // Re-snapshot ancestry after exact identity capture. This closes the window
    // where a descendant exits and its numeric PID is rebound between the first
    // process-table walk and identity capture. A rebound process is authorized
    // only if it is now still a descendant of the same verified root; otherwise
    // the entire destructive transition fails closed.
    let mut ancestry_readback = System::new_all();
    ancestry_readback.refresh_processes(ProcessesToUpdate::All, true);
    let root_process = ancestry_readback.process(root_pid).ok_or_else(|| {
        format!(
            "root pid {} disappeared before ancestry readback",
            expected_root.pid
        )
    })?;
    let mut verified_tree = vec![expected_root.pid];
    verified_tree.extend(shell_job_descendant_process_ids(
        &ancestry_readback,
        expected_root.pid,
        root_process.start_time(),
    ));
    for identity in &identities {
        let process = ancestry_readback
            .process(Pid::from_u32(identity.pid))
            .ok_or_else(|| {
                format!(
                    "process-tree pid {} disappeared before ancestry readback",
                    identity.pid
                )
            })?;
        if !verified_tree.contains(&identity.pid)
            || process.start_time() != local_identity_sysinfo_start_time(identity)?
        {
            return Err(format!(
                "process-tree pid {} no longer has the captured identity beneath verified root {}",
                identity.pid, expected_root.pid
            ));
        }
    }
    match local_process_identity_state(expected_root) {
        LocalProcessIdentityState::Match => {}
        LocalProcessIdentityState::Mismatch(actual) => {
            return Err(format!(
                "root pid {} identity changed before termination authorization: actual={actual:?}",
                expected_root.pid
            ));
        }
        LocalProcessIdentityState::Exited => {
            return Err(format!(
                "root pid {} exited before termination authorization",
                expected_root.pid
            ));
        }
        LocalProcessIdentityState::Absent => {
            return Err(format!(
                "root pid {} disappeared before termination authorization",
                expected_root.pid
            ));
        }
        LocalProcessIdentityState::Unreadable(error) => {
            return Err(format!(
                "root pid {} identity became unreadable before termination authorization: {error}",
                expected_root.pid
            ));
        }
    }
    Ok(identities)
}

#[cfg(windows)]
fn terminate_shell_job_process_tree_platform(
    identities: &[ActRunShellLocalProcessIdentity],
) -> ShellJobTerminationReadback {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME, WAIT_FAILED, WAIT_OBJECT_0},
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
        },
    };

    const STILL_ACTIVE_EXIT_CODE: u32 = 259;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut attempted = false;
    let mut failures = Vec::new();
    for identity in identities.iter().rev() {
        let handle = match unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                identity.pid,
            )
        } {
            Ok(handle) => handle,
            Err(error) => {
                match local_process_identity_state(identity) {
                    LocalProcessIdentityState::Match => failures.push(format!(
                        "pid {} exact-handle open failed while identity remained live: {error}",
                        identity.pid
                    )),
                    LocalProcessIdentityState::Mismatch(actual) => failures.push(format!(
                        "pid {} changed identity after exact-handle open failure; termination refused: expected={identity:?} actual={actual:?}; open_error={error}",
                        identity.pid
                    )),
                    LocalProcessIdentityState::Exited => {}
                    LocalProcessIdentityState::Absent => {}
                    LocalProcessIdentityState::Unreadable(read_error) => failures.push(format!(
                        "pid {} exact-handle open failed and independent identity readback was unavailable: open_error={error}; readback_error={read_error}",
                        identity.pid
                    )),
                }
                continue;
            }
        };
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let actual_start = unsafe {
            GetProcessTimes(
                handle,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        }
        .map(|()| filetime_ticks_100ns(creation));
        match actual_start {
            Ok(actual_start) if actual_start == identity.start_time => {}
            Ok(actual_start) => {
                failures.push(format!(
                    "pid {} creation identity changed at destructive boundary: expected={} actual={}; termination refused",
                    identity.pid, identity.start_time, actual_start
                ));
                if let Err(error) = unsafe { CloseHandle(handle) } {
                    failures.push(format!(
                        "CloseHandle(mismatched pid {}) failed: {error}",
                        identity.pid
                    ));
                }
                continue;
            }
            Err(error) => {
                failures.push(format!(
                    "GetProcessTimes failed at destructive boundary for pid {}: {error}",
                    identity.pid
                ));
                if let Err(error) = unsafe { CloseHandle(handle) } {
                    failures.push(format!(
                        "CloseHandle(unverified pid {}) failed: {error}",
                        identity.pid
                    ));
                }
                continue;
            }
        }
        let mut exit_code = 0_u32;
        let read_before = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) };
        if let Err(error) = read_before {
            failures.push(format!(
                "GetExitCodeProcess before termination failed for pid {}: {error}",
                identity.pid
            ));
        } else if exit_code == STILL_ACTIVE_EXIT_CODE {
            attempted = true;
            if let Err(error) = unsafe { TerminateProcess(handle, 1) } {
                failures.push(format!(
                    "TerminateProcess exact handle failed for pid {}: {error}",
                    identity.pid
                ));
            }
        }
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        let wait_ms = u32::try_from(remaining_ms).unwrap_or(u32::MAX);
        let wait = unsafe { WaitForSingleObject(handle, wait_ms) };
        if wait != WAIT_OBJECT_0 {
            let last_error = (wait == WAIT_FAILED)
                .then(windows::core::Error::from_thread)
                .map_or_else(|| "not_available".to_owned(), |error| error.to_string());
            failures.push(format!(
                "pid {} exact handle did not signal before cleanup deadline: wait={wait:?} last_error={last_error}",
                identity.pid
            ));
        }
        let mut exit_after = 0_u32;
        match unsafe { GetExitCodeProcess(handle, &raw mut exit_after) } {
            Ok(()) if exit_after == STILL_ACTIVE_EXIT_CODE => failures.push(format!(
                "pid {} remained STILL_ACTIVE after exact-handle termination",
                identity.pid
            )),
            Ok(()) => {}
            Err(error) => failures.push(format!(
                "GetExitCodeProcess after termination failed for pid {}: {error}",
                identity.pid
            )),
        }
        if let Err(error) = unsafe { CloseHandle(handle) } {
            failures.push(format!(
                "CloseHandle(terminated pid {}) failed: {error}",
                identity.pid
            ));
        }
    }
    let mut remaining_process_ids = Vec::new();
    for identity in identities {
        match local_process_identity_state(identity) {
            LocalProcessIdentityState::Match => remaining_process_ids.push(identity.pid),
            LocalProcessIdentityState::Exited => {}
            LocalProcessIdentityState::Absent => {}
            LocalProcessIdentityState::Mismatch(actual) => failures.push(format!(
                "pid {} was reused before final termination readback: expected={identity:?} actual={actual:?}",
                identity.pid
            )),
            LocalProcessIdentityState::Unreadable(error) => {
                failures.push(format!(
                    "pid {} final identity readback was unavailable: {error}",
                    identity.pid
                ));
                // Uncertainty is represented conservatively as remaining; it
                // must never be reported as successful absence.
                remaining_process_ids.push(identity.pid);
            }
        }
    }
    ShellJobTerminationReadback {
        attempted,
        status: if failures.is_empty() && remaining_process_ids.is_empty() {
            if attempted {
                "terminated".to_owned()
            } else {
                "already_exited".to_owned()
            }
        } else {
            format!("termination_failed:{}", failures.join(" | "))
        },
        remaining_process_ids,
    }
}

#[cfg(not(windows))]
fn terminate_shell_job_process_tree_platform(
    identities: &[ActRunShellLocalProcessIdentity],
) -> ShellJobTerminationReadback {
    let mut attempted = false;
    let mut failures = Vec::new();
    for identity in identities.iter().rev() {
        match local_process_identity_state(identity) {
            LocalProcessIdentityState::Match => {
                attempted = true;
                match StdCommand::new("kill")
                    .args(["-TERM", &identity.pid.to_string()])
                    .output()
                {
                    Ok(output) if output.status.success() => {}
                    Ok(output) => failures.push(format!(
                        "kill -TERM pid {} exited {:?}: {}",
                        identity.pid,
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr)
                    )),
                    Err(error) => failures.push(format!(
                        "kill -TERM spawn failed for pid {}: {error}",
                        identity.pid
                    )),
                }
            }
            LocalProcessIdentityState::Mismatch(actual) => failures.push(format!(
                "pid {} start identity changed at destructive boundary: expected={} actual={}; TERM refused",
                identity.pid, identity.start_time, actual.start_time
            )),
            LocalProcessIdentityState::Exited => {}
            LocalProcessIdentityState::Absent => {}
            LocalProcessIdentityState::Unreadable(error) => failures.push(format!(
                "pid {} identity was unreadable at TERM boundary; TERM refused: {error}",
                identity.pid
            )),
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let mut any_match = false;
        let mut uncertainty = None;
        for identity in identities {
            match local_process_identity_state(identity) {
                LocalProcessIdentityState::Match => any_match = true,
                LocalProcessIdentityState::Unreadable(error) => {
                    uncertainty = Some(format!(
                        "pid {} identity became unreadable while awaiting TERM: {error}",
                        identity.pid
                    ));
                    break;
                }
                LocalProcessIdentityState::Mismatch(_)
                | LocalProcessIdentityState::Exited
                | LocalProcessIdentityState::Absent => {}
            }
        }
        if let Some(error) = uncertainty {
            failures.push(error);
            break;
        }
        if !any_match {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let remaining = identities
        .iter()
        .filter(|identity| {
            matches!(
                local_process_identity_state(identity),
                LocalProcessIdentityState::Match
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    for identity in &remaining {
        match local_process_identity_state(identity) {
            LocalProcessIdentityState::Match => match StdCommand::new("kill")
                .args(["-KILL", &identity.pid.to_string()])
                .output()
            {
                Ok(output) if output.status.success() => {}
                Ok(output) => failures.push(format!(
                    "kill -KILL pid {} exited {:?}: {}",
                    identity.pid,
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                )),
                Err(error) => failures.push(format!(
                    "kill -KILL spawn failed for pid {}: {error}",
                    identity.pid
                )),
            },
            LocalProcessIdentityState::Mismatch(actual) => failures.push(format!(
                "pid {} start identity changed before KILL: expected={} actual={}; KILL refused",
                identity.pid, identity.start_time, actual.start_time
            )),
            LocalProcessIdentityState::Exited => {}
            LocalProcessIdentityState::Absent => {}
            LocalProcessIdentityState::Unreadable(error) => failures.push(format!(
                "pid {} identity was unreadable at KILL boundary; KILL refused: {error}",
                identity.pid
            )),
        }
    }
    let mut remaining_process_ids = Vec::new();
    for identity in identities {
        match local_process_identity_state(identity) {
            LocalProcessIdentityState::Match => remaining_process_ids.push(identity.pid),
            LocalProcessIdentityState::Exited => {}
            LocalProcessIdentityState::Absent => {}
            LocalProcessIdentityState::Mismatch(actual) => failures.push(format!(
                "pid {} was reused before final termination readback: expected={identity:?} actual={actual:?}",
                identity.pid
            )),
            LocalProcessIdentityState::Unreadable(error) => {
                failures.push(format!(
                    "pid {} final identity readback was unavailable: {error}",
                    identity.pid
                ));
                remaining_process_ids.push(identity.pid);
            }
        }
    }
    ShellJobTerminationReadback {
        attempted,
        status: if failures.is_empty() && remaining_process_ids.is_empty() {
            if attempted {
                "terminated".to_owned()
            } else {
                "already_exited".to_owned()
            }
        } else {
            format!("termination_failed:{}", failures.join(" | "))
        },
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

async fn run_allowlisted_shell_with_boundary(
    params: ActRunShellParams,
    inline_await_limit_ms: u64,
    context: Option<&ShellExecutionContext>,
    boundary: &PhysicalMutationBoundary<'_>,
) -> Result<ActRunShellResponse, ErrorData> {
    let started = Instant::now();
    // Arm an absolute Tokio deadline before spawning. Unlike a relative timeout
    // constructed when `child.wait()` is first polled, this cannot grant extra
    // runtime if the async worker is descheduled between spawn and wait.
    let timeout_budget = inline_shell_timeout_budget(params.timeout_ms);
    let timeout_deadline = tokio::time::Instant::now()
        .checked_add(timeout_budget)
        .ok_or_else(|| {
            shell_tool_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_run_shell timeout_ms is outside the platform monotonic-clock range",
                json!({
                    "code": error_codes::TOOL_PARAMS_INVALID,
                    "timeout_ms": params.timeout_ms,
                    "reason": "timeout_deadline_out_of_range",
                }),
            )
        })?;
    let requested_execution_mode = params.execution_mode;
    boundary("act_run_shell_immediately_before_create_process")?;
    let mut spawned = spawn_shell_child(&params, context)?;
    if let Err(error) = boundary("act_run_shell_immediately_after_create_process") {
        let cleanup = cleanup_inline_shell_child_after_boundary(&mut spawned).await;
        return Err(physical_mutation_boundary_error(
            error,
            "act_run_shell_immediately_after_create_process",
            cleanup,
        ));
    }
    let (stdout_task, stderr_task) = match spawn_capped_readers(&mut spawned.child) {
        Ok(tasks) => tasks,
        Err(reader_error) => {
            let tree_termination =
                terminate_shell_job_process_tree(&spawned.local_process_identity);
            let initial_reap = terminate_and_reap_tokio_child_bounded(
                &mut spawned.child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            );
            let job_close = spawned.process_job.close_checked();
            let post_job_close_reap = terminate_and_reap_tokio_child_bounded(
                &mut spawned.child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            );
            let final_identity_state =
                local_process_identity_state(&spawned.local_process_identity);
            return Err(shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "act_run_shell could not start output readers after spawning pid {}; owned cleanup followed: {}",
                    spawned.local_process_identity.pid, reader_error.message
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "underlying_code": extract_error_code(&reader_error),
                    "reason": "output_reader_setup_failed_after_spawn",
                    "pid": spawned.local_process_identity.pid,
                    "reader_error": reader_error,
                    "tree_termination": tree_termination,
                    "initial_reap": initial_reap,
                    "job_close": format!("{job_close:?}"),
                    "post_job_close_reap": post_job_close_reap,
                    "final_identity_state": final_identity_state,
                }),
            ));
        }
    };
    let wait_result = wait_shell_child_with_identity(
        &mut spawned.child,
        Some(&spawned.local_process_identity),
        params.timeout_ms,
        started,
        timeout_deadline,
        Some(boundary),
    )
    .await;
    let job_close = spawned.process_job.close_checked();
    let (exit_code, timed_out) = match wait_result {
        Ok(result) if job_close.is_ok() => result,
        Ok(_) => {
            let close_error = job_close.err();
            let final_identity_state =
                local_process_identity_state(&spawned.local_process_identity);
            return Err(shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "act_run_shell could not verify owned job-handle closure for pid {}: {close_error:?}",
                    spawned.local_process_identity.pid
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "reason": "job_object_close_failed_after_inline_wait",
                    "pid": spawned.local_process_identity.pid,
                    "job_close_error": close_error,
                    "final_identity_state": final_identity_state,
                }),
            ));
        }
        Err(wait_error)
            if extract_error_code(&wait_error) == error_codes::SAFETY_OPERATOR_HOTKEY_FIRED =>
        {
            let post_job_close_reap = terminate_and_reap_tokio_child_bounded(
                &mut spawned.child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            );
            let final_identity_state =
                local_process_identity_state(&spawned.local_process_identity);
            let cleanup_verified = job_close.is_ok()
                && post_job_close_reap.poll_error_count == 0
                && matches!(
                    final_identity_state,
                    LocalProcessIdentityState::Exited
                        | LocalProcessIdentityState::Absent
                        | LocalProcessIdentityState::Mismatch(_)
                );
            if !cleanup_verified {
                synapse_action::record_operator_panic_safety_incident();
            }
            return Err(physical_mutation_boundary_error(
                wait_error,
                "act_run_shell_after_operator_panic_job_close",
                json!({
                    "source_of_truth": "kill-on-close job + exact Tokio child + final process identity",
                    "job_close": format!("{job_close:?}"),
                    "post_job_close_reap": post_job_close_reap,
                    "final_identity_state": final_identity_state,
                    "cleanup_verified": cleanup_verified,
                }),
            ));
        }
        Err(wait_error) => {
            let post_job_close_reap = terminate_and_reap_tokio_child_bounded(
                &mut spawned.child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            );
            let final_identity_state =
                local_process_identity_state(&spawned.local_process_identity);
            return Err(shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "act_run_shell inline wait failed after spawning pid {}; job_close={job_close:?}; post_job_close_reap={post_job_close_reap:?}; final_identity_state={final_identity_state:?}: {}",
                    spawned.local_process_identity.pid, wait_error.message
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "underlying_code": extract_error_code(&wait_error),
                    "reason": "inline_wait_failed_after_spawn",
                    "pid": spawned.local_process_identity.pid,
                    "wait_error": wait_error,
                    "job_close": format!("{job_close:?}"),
                    "post_job_close_reap": post_job_close_reap,
                    "final_identity_state": final_identity_state,
                }),
            ));
        }
    };
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

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UnresolvedShellChildOwnerSnapshot {
    pub(crate) owner_id: String,
    pub(crate) pid: Option<u32>,
    pub(crate) stage: String,
    pub(crate) child_kind: String,
    pub(crate) process_job_acquired: bool,
    pub(crate) tree_cleanup_verified: bool,
    pub(crate) durable_job_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UnresolvedShellChildOwnerReport {
    pub(crate) active_owner_count: usize,
    pub(crate) owners: Vec<UnresolvedShellChildOwnerSnapshot>,
    pub(crate) retry_attempted: usize,
    pub(crate) reaped_owner_count: usize,
    pub(crate) registry_poisoned: bool,
}

impl UnresolvedShellChildOwnerReport {
    pub(crate) const fn safe_to_unlock(&self) -> bool {
        self.active_owner_count == 0 && !self.registry_poisoned
    }
}

enum RetainedExactShellChild {
    Tokio(Box<tokio::process::Child>),
    Std(Box<std::process::Child>),
}

impl RetainedExactShellChild {
    fn kind(&self) -> &'static str {
        match self {
            Self::Tokio(_) => "tokio",
            Self::Std(_) => "std",
        }
    }

    fn request_termination(&mut self) -> Option<String> {
        match self {
            Self::Tokio(child) => child.start_kill().err().map(|error| error.to_string()),
            Self::Std(child) => child.kill().err().map(|error| error.to_string()),
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        match self {
            Self::Tokio(child) => child.try_wait(),
            Self::Std(child) => child.try_wait(),
        }
    }
}

struct RetainedDurableSpawnFailure {
    status_path: PathBuf,
    status: ActRunShellJobStatus,
}

struct RetainedShellChildOwner {
    owner_id: String,
    pid: Option<u32>,
    stage: String,
    child: RetainedExactShellChild,
    process_job: Option<OwnedProcessJob>,
    process_job_acquired: bool,
    process_job_close_verified: bool,
    tree_cleanup_verified: bool,
    local_process_identity: Option<ActRunShellLocalProcessIdentity>,
    durable_spawn_failure: Option<RetainedDurableSpawnFailure>,
}

static RETAINED_UNRESOLVED_SHELL_CHILD_OWNERS: OnceLock<Mutex<Vec<RetainedShellChildOwner>>> =
    OnceLock::new();

fn retained_unresolved_shell_child_owners() -> &'static Mutex<Vec<RetainedShellChildOwner>> {
    RETAINED_UNRESOLVED_SHELL_CHILD_OWNERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn retain_unresolved_shell_child_owner(owner: RetainedShellChildOwner) -> (String, usize) {
    let owner_id = owner.owner_id.clone();
    let mut owners = retained_unresolved_shell_child_owners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    owners.push(owner);
    (owner_id, owners.len())
}

fn terminal_local_process_identity_state(state: &LocalProcessIdentityState) -> bool {
    matches!(
        state,
        LocalProcessIdentityState::Exited
            | LocalProcessIdentityState::Absent
            | LocalProcessIdentityState::Mismatch(_)
    )
}

fn retry_retained_shell_child_owner(owner: &mut RetainedShellChildOwner) -> bool {
    let retry_kill_error = owner.child.request_termination();
    if !owner.process_job_close_verified
        && let Some(process_job) = owner.process_job.as_mut()
        && process_job.close_checked().is_ok()
    {
        owner.process_job_close_verified = true;
    }
    let exact_child_reaped = match owner.child.try_wait() {
        Ok(Some(_)) => true,
        Ok(None) | Err(_) => false,
    };
    let final_identity_state = owner
        .local_process_identity
        .as_ref()
        .map(local_process_identity_state);
    let identity_terminal = final_identity_state
        .as_ref()
        .is_none_or(terminal_local_process_identity_state);
    let cleanup_verified = exact_child_reaped
        && owner.process_job_close_verified
        && owner.tree_cleanup_verified
        && identity_terminal;
    if !cleanup_verified {
        return false;
    }

    let Some(durable) = owner.durable_spawn_failure.as_mut() else {
        return true;
    };
    durable.status.status = "spawn_failed_reaped".to_owned();
    durable.status.completed_at = Some(chrono::Utc::now().to_rfc3339());
    durable.status.duration_ms =
        Some(elapsed_ms_since_rfc3339(&durable.status.started_at).unwrap_or_default());
    if let Some(readback) = durable.status.spawn_failure.as_mut() {
        readback.cleanup_verified = true;
        readback.exact_child_reaped = true;
        readback.exact_child_reap_timed_out = false;
        readback.exact_owner_retained = false;
        if readback.exact_child_kill_error.is_none() {
            readback.exact_child_kill_error = retry_kill_error;
        }
        readback.process_job_close = Some("Ok(())".to_owned());
        readback.final_identity_state = final_identity_state.map(|state| format!("{state:?}"));
    }
    match persist_and_verify_shell_job_status(&durable.status_path, &durable.status) {
        Ok(_) => true,
        Err(error) => {
            tracing::error!(
                code = "M4_SHELL_UNRESOLVED_CHILD_TERMINAL_STATUS_PERSIST_FAILED",
                owner_id = owner.owner_id,
                pid = ?owner.pid,
                job_id = %durable.status.job_id,
                reason = error.reason,
                detail = error.detail,
                "exact child was reaped but its durable terminal spawn-failure status remains unverified"
            );
            false
        }
    }
}

/// Perform one nonblocking termination/reap/status retry for every retained
/// exact owner, then return the physical registry Source of Truth. Shutdown
/// gates use `safe_to_unlock`: an unresolved child or durable terminal commit
/// keeps both daemon lifetime locks held until process teardown.
pub(crate) fn unresolved_shell_child_owner_report() -> UnresolvedShellChildOwnerReport {
    const OWNER_SAMPLE_CAP: usize = 16;
    let lock = retained_unresolved_shell_child_owners().lock();
    let (mut owners, registry_poisoned) = match lock {
        Ok(owners) => (owners, false),
        Err(poisoned) => (poisoned.into_inner(), true),
    };
    let retry_attempted = owners.len();
    let before = owners.len();
    owners.retain_mut(|owner| !retry_retained_shell_child_owner(owner));
    let reaped_owner_count = before.saturating_sub(owners.len());
    let snapshots = owners
        .iter()
        .take(OWNER_SAMPLE_CAP)
        .map(|owner| UnresolvedShellChildOwnerSnapshot {
            owner_id: owner.owner_id.clone(),
            pid: owner.pid,
            stage: owner.stage.clone(),
            child_kind: owner.child.kind().to_owned(),
            process_job_acquired: owner.process_job_acquired,
            tree_cleanup_verified: owner.tree_cleanup_verified,
            durable_job_id: owner
                .durable_spawn_failure
                .as_ref()
                .map(|durable| durable.status.job_id.clone()),
        })
        .collect();
    UnresolvedShellChildOwnerReport {
        active_owner_count: owners.len(),
        owners: snapshots,
        retry_attempted,
        reaped_owner_count,
        registry_poisoned,
    }
}

struct SpawnedShellChild {
    child: tokio::process::Child,
    process_job: OwnedProcessJob,
    local_process_identity: ActRunShellLocalProcessIdentity,
}

async fn cleanup_inline_shell_child_after_boundary(spawned: &mut SpawnedShellChild) -> Value {
    let tree_termination = {
        let identity = spawned.local_process_identity.clone();
        tokio::task::spawn_blocking(move || terminate_shell_job_process_tree(&identity))
            .await
            .unwrap_or_else(|join_error| ShellJobTerminationReadback {
                attempted: false,
                status: format!("termination_task_join_failed:{join_error}"),
                remaining_process_ids: vec![spawned.local_process_identity.pid],
            })
    };
    let initial_reap = terminate_and_reap_tokio_child_async_bounded(
        &mut spawned.child,
        Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
    )
    .await;
    let state_while_job_held = local_process_identity_state(&spawned.local_process_identity);
    let job_close = spawned.process_job.close_checked();
    let post_job_close_reap = terminate_and_reap_tokio_child_async_bounded(
        &mut spawned.child,
        Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
    )
    .await;
    let final_identity_state = local_process_identity_state(&spawned.local_process_identity);
    let tree_verified = tree_termination.remaining_process_ids.is_empty()
        && matches!(
            tree_termination.status.as_str(),
            "terminated" | "already_exited"
        );
    let identity_absent = matches!(
        final_identity_state,
        LocalProcessIdentityState::Exited
            | LocalProcessIdentityState::Absent
            | LocalProcessIdentityState::Mismatch(_)
    );
    let cleanup_verified = tree_verified
        && (initial_reap.reaped || post_job_close_reap.reaped)
        && initial_reap.poll_error_count == 0
        && post_job_close_reap.poll_error_count == 0
        && identity_absent
        && job_close.is_ok();
    if !cleanup_verified {
        synapse_action::record_operator_panic_safety_incident();
    }
    json!({
        "source_of_truth": "identity-bound process tree + exact Tokio child + kill-on-close job + final process identity",
        "pid": spawned.local_process_identity.pid,
        "tree_termination": tree_termination,
        "initial_reap": initial_reap,
        "state_while_job_held": state_while_job_held,
        "job_close": format!("{job_close:?}"),
        "post_job_close_reap": post_job_close_reap,
        "final_identity_state": final_identity_state,
        "cleanup_verified": cleanup_verified,
    })
}

fn persist_running_shell_job_status_or_cleanup(
    paths: &ShellJobPaths,
    status: &mut ActRunShellJobStatus,
    child: &mut tokio::process::Child,
    local_process_identity: &ActRunShellLocalProcessIdentity,
    process_job: &mut OwnedProcessJob,
    started: Instant,
) -> Result<(), ErrorData> {
    let initial_failure = match persist_and_verify_shell_job_status(&paths.status_path, status) {
        Ok(_) => return Ok(()),
        Err(failure) => failure,
    };

    // A resumed child is already observable reality. A status-store failure
    // therefore becomes an owned cleanup transaction, not an early-return `?`:
    // terminate the identity-bound tree and reap the exact Tokio child while
    // the verified job authority is still held, then close that job explicitly
    // and perform an independent process-identity readback.
    let tree_termination = terminate_shell_job_process_tree(local_process_identity);
    let initial_reap = terminate_and_reap_tokio_child_bounded(
        child,
        Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
    );
    let state_while_job_held = local_process_identity_state(local_process_identity);
    let job_close = process_job.close_checked();
    let state_after_job_close = local_process_identity_state(local_process_identity);
    let post_job_close_reap = (!initial_reap.reaped
        || matches!(
            state_after_job_close,
            LocalProcessIdentityState::Match | LocalProcessIdentityState::Unreadable(_)
        ))
    .then(|| {
        terminate_and_reap_tokio_child_bounded(
            child,
            Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
        )
    });
    let final_identity_state = local_process_identity_state(local_process_identity);
    let exact_child_reaped = initial_reap.reaped
        || post_job_close_reap
            .as_ref()
            .is_some_and(|readback| readback.reaped);
    let identity_absent = matches!(
        final_identity_state,
        LocalProcessIdentityState::Exited
            | LocalProcessIdentityState::Absent
            | LocalProcessIdentityState::Mismatch(_)
    );
    let tree_verified = tree_termination.remaining_process_ids.is_empty()
        && matches!(
            tree_termination.status.as_str(),
            "terminated" | "already_exited"
        );
    let cleanup_verified =
        exact_child_reaped && identity_absent && tree_verified && job_close.is_ok();

    status.status = if cleanup_verified {
        "start_status_persist_failed_reaped".to_owned()
    } else {
        "start_status_persist_failed_cleanup_unverified".to_owned()
    };
    status.completed_at = Some(chrono::Utc::now().to_rfc3339());
    status.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    status.error_code = Some(initial_failure.error_code.to_owned());
    status.error_message = Some(format!(
        "initial running status persistence failed: reason={} detail={}; cleanup_verified={cleanup_verified}; tree={tree_termination:?}; initial_reap={initial_reap:?}; state_while_job_held={state_while_job_held:?}; job_close={job_close:?}; post_job_close_reap={post_job_close_reap:?}; final_identity_state={final_identity_state:?}",
        initial_failure.reason, initial_failure.detail
    ));

    // The storage fault may be transient. Preserve a truthful terminal record
    // if it became writable after cleanup; otherwise return both the original
    // store failure and this second durable-state failure to the caller/log.
    let terminal_persistence = persist_and_verify_shell_job_status(&paths.status_path, status);
    let terminal_status_persisted = terminal_persistence.is_ok();
    let terminal_persistence_failure = terminal_persistence.err();
    tracing::error!(
        code = "M4_ACT_RUN_SHELL_INITIAL_STATUS_UNVERIFIED_CHILD_CLEANED",
        job_id = %status.job_id,
        pid = local_process_identity.pid,
        initial_failure_reason = initial_failure.reason,
        initial_failure_detail = initial_failure.detail,
        cleanup_verified,
        tree_termination = ?tree_termination,
        initial_reap = ?initial_reap,
        state_while_job_held = ?state_while_job_held,
        job_close = ?job_close,
        post_job_close_reap = ?post_job_close_reap,
        final_identity_state = ?final_identity_state,
        terminal_status_persisted,
        terminal_persistence_failure = ?terminal_persistence_failure,
        "initial durable running status failed after child resume; exact owned cleanup was attempted and independently read back"
    );
    Err(shell_tool_error(
        initial_failure.error_code,
        format!(
            "act_run_shell_start could not verify its initial running status after resuming pid {}; cleanup_verified={cleanup_verified}; terminal_status_persisted={terminal_status_persisted}",
            local_process_identity.pid
        ),
        json!({
            "code": initial_failure.error_code,
            "job_id": status.job_id,
            "pid": local_process_identity.pid,
            "reason": "initial_running_status_persistence_failed",
            "initial_status_failure": initial_failure,
            "cleanup_verified": cleanup_verified,
            "tree_termination": tree_termination,
            "initial_reap": initial_reap,
            "state_while_job_held": state_while_job_held,
            "job_close": format!("{job_close:?}"),
            "post_job_close_reap": post_job_close_reap,
            "final_identity_state": final_identity_state,
            "terminal_status_persisted": terminal_status_persisted,
            "terminal_persistence_failure": terminal_persistence_failure,
            "status_path": paths.status_path,
        }),
    ))
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct OwnedProcessJob {
    handle: Option<windows::Win32::Foundation::HANDLE>,
}

#[cfg(not(windows))]
#[derive(Debug)]
pub(crate) struct OwnedProcessJob;

#[cfg(windows)]
static RETAINED_OWNED_PROCESS_JOB_HANDLES: OnceLock<Mutex<Vec<isize>>> = OnceLock::new();

#[cfg(windows)]
fn retry_retained_owned_process_job_handles() -> usize {
    let retained = RETAINED_OWNED_PROCESS_JOB_HANDLES.get_or_init(|| Mutex::new(Vec::new()));
    let mut retained = retained
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retained.retain(|raw| {
        let handle = windows::Win32::Foundation::HANDLE(*raw as *mut core::ffi::c_void);
        unsafe { windows::Win32::Foundation::CloseHandle(handle) }.is_err()
    });
    retained.len()
}

#[cfg(windows)]
impl Drop for OwnedProcessJob {
    fn drop(&mut self) {
        let _ = retry_retained_owned_process_job_handles();
        let Some(handle) = self.handle else {
            return;
        };
        match unsafe { windows::Win32::Foundation::CloseHandle(handle) } {
            Ok(()) => self.handle = None,
            Err(error) => {
                // Losing the raw handle here would silently disarm the only
                // kill-on-close ownership authority. Transfer it into a
                // process-lifetime registry; later job drops retry, and OS
                // process teardown remains the final close boundary.
                let retained = RETAINED_OWNED_PROCESS_JOB_HANDLES
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                    .map(|mut retained| {
                        retained.push(handle.0 as isize);
                        retained.len()
                    })
                    .unwrap_or_else(|poisoned| {
                        let mut retained = poisoned.into_inner();
                        retained.push(handle.0 as isize);
                        retained.len()
                    });
                self.handle = None;
                tracing::error!(
                    code = "M4_OWNED_PROCESS_JOB_DROP_CLOSE_FAILED",
                    error = %error,
                    retained_handles = ?retained,
                    "OwnedProcessJob Drop retained its unclosed Windows job handle for retry"
                );
                eprintln!(
                    "M4_OWNED_PROCESS_JOB_DROP_CLOSE_FAILED: CloseHandle(job) failed and ownership was retained for retry: {error}; retained={retained:?}"
                );
            }
        }
    }
}

#[cfg(windows)]
unsafe impl Send for OwnedProcessJob {}

#[cfg(windows)]
impl OwnedProcessJob {
    fn handle(
        &self,
        tool_name: &'static str,
        pid: u32,
    ) -> Result<windows::Win32::Foundation::HANDLE, ErrorData> {
        self.handle.ok_or_else(|| {
            shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{tool_name} Windows job handle was already closed for pid {pid}"),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "reason": "job_object_handle_already_closed",
                }),
            )
        })
    }

    fn close_checked(&mut self) -> Result<(), String> {
        let Some(handle) = self.handle else {
            return Ok(());
        };
        unsafe { windows::Win32::Foundation::CloseHandle(handle) }
            .map_err(|error| format!("CloseHandle(owned process job) failed: {error}"))?;
        self.handle = None;
        Ok(())
    }

    pub(crate) fn disarm_kill_on_close(
        &self,
        tool_name: &'static str,
        pid: u32,
        resource_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        use windows::Win32::System::JobObjects::{
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let handle = self.handle(tool_name, pid)?;
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
                handle,
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
        })?;
        query_owned_process_job_kill_on_close(handle, false).map_err(|error| {
            shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "{tool_name} could not verify Windows job object kill-on-close disarm for pid {pid}: {error}"
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "resource_id": resource_id,
                    "reason": "job_object_kill_on_close_disarm_readback_failed",
                    "detail": error,
                }),
            )
        })
    }
}

#[cfg(not(windows))]
impl OwnedProcessJob {
    fn close_checked(&mut self) -> Result<(), String> {
        Ok(())
    }

    pub(crate) fn disarm_kill_on_close(
        &self,
        _tool_name: &'static str,
        _pid: u32,
        _resource_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }
}

#[cfg(windows)]
fn query_owned_process_job_kill_on_close(
    handle: windows::Win32::Foundation::HANDLE,
    expected: bool,
) -> Result<(), String> {
    use windows::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
    };

    let limit_size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
        .map_err(|error| format!("Windows job limit size conversion failed: {error}"))?;
    let mut readback = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let mut returned_bytes = 0_u32;
    unsafe {
        QueryInformationJobObject(
            Some(handle),
            JobObjectExtendedLimitInformation,
            (&raw mut readback).cast(),
            limit_size,
            Some(&raw mut returned_bytes),
        )
    }
    .map_err(|error| format!("QueryInformationJobObject limit readback failed: {error}"))?;
    if returned_bytes != limit_size {
        return Err(format!(
            "QueryInformationJobObject returned {returned_bytes} bytes; expected {limit_size}; limit_flags={:#x}",
            readback.BasicLimitInformation.LimitFlags.0
        ));
    }
    let actual = readback
        .BasicLimitInformation
        .LimitFlags
        .contains(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
    if actual != expected {
        return Err(format!(
            "Windows job kill-on-close limit readback mismatch: expected={expected} actual={actual} limit_flags={:#x}",
            readback.BasicLimitInformation.LimitFlags.0
        ));
    }
    Ok(())
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
                    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                    JobObjectExtendedLimitInformation, SetInformationJobObject,
                },
                Threading::{
                    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
                    PROCESS_TERMINATE,
                },
            },
        },
        core::{BOOL, PCWSTR},
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
    let mut owner = OwnedProcessJob { handle: Some(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let limit_size = match u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
    {
        Ok(size) => size,
        Err(error) => {
            let job_close = owner.close_checked();
            return Err(shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "{tool_name} failed to size Windows job object limits: {error}; job_close={job_close:?}"
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "resource_id": resource_id,
                    "reason": "job_object_limit_size_failed",
                    "job_close": format!("{job_close:?}"),
                }),
            ));
        }
    };
    if let Err(error) = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            limit_size,
        )
    } {
        let job_close = owner.close_checked();
        return Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "{tool_name} failed to set Windows job object kill-on-close: {error}; job_close={job_close:?}"
            ),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "resource_id": resource_id,
                "reason": "job_object_limit_failed",
                "job_close": format!("{job_close:?}"),
            }),
        ));
    }
    if let Err(error) = query_owned_process_job_kill_on_close(job, true) {
        let job_close = owner.close_checked();
        return Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "{tool_name} could not verify Windows job object kill-on-close before assigning pid {pid}: {error}; job_close={job_close:?}"
            ),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "resource_id": resource_id,
                "reason": "job_object_limit_readback_failed",
                "detail": error,
                "job_close": format!("{job_close:?}"),
            }),
        ));
    }
    let process = match unsafe {
        OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
    } {
        Ok(process) => process,
        Err(error) => {
            let job_close = owner.close_checked();
            return Err(shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "{tool_name} failed to open child process for job assignment: {error}; job_close={job_close:?}"
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "resource_id": resource_id,
                    "reason": "job_object_process_open_failed",
                    "job_close": format!("{job_close:?}"),
                }),
            ));
        }
    };
    let (assignment_error, membership) = match unsafe { AssignProcessToJobObject(job, process) } {
        Ok(()) => {
            let mut in_job = BOOL::default();
            (
                None,
                unsafe { IsProcessInJob(process, Some(job), &raw mut in_job) }
                    .map(|()| in_job.as_bool())
                    .map_err(|error| error.to_string()),
            )
        }
        Err(error) => (
            Some(error.to_string()),
            Err("assignment failed; membership readback was not attempted".to_owned()),
        ),
    };
    let process_close = unsafe { CloseHandle(process) }.map_err(|error| error.to_string());
    let assignment_verified =
        membership.as_ref().is_ok_and(|in_job| *in_job) && process_close.is_ok();
    if !assignment_verified {
        let membership_detail = match membership {
            Ok(true) => "verified".to_owned(),
            Ok(false) => "IsProcessInJob returned false".to_owned(),
            Err(error) => format!("IsProcessInJob failed: {error}"),
        };
        let close_detail = process_close.err();
        let job_close = owner.close_checked();
        return Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "{tool_name} could not verify child pid {pid} kill-on-close job ownership; assignment_error={assignment_error:?}; membership={membership_detail}; process_close_error={close_detail:?}; job_close={job_close:?}"
            ),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "resource_id": resource_id,
                "reason": "job_object_membership_unverified",
                "assignment_error": assignment_error,
                "membership": membership_detail,
                "process_close_error": close_detail,
                "job_close": format!("{job_close:?}"),
            }),
        ));
    }
    Ok(owner)
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
        let cleanup = terminate_and_reap_tokio_child_bounded(
            &mut child,
            Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
        );
        return Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            if cleanup.reaped {
                "act_run_shell spawned a child process without an observable pid; the exact child handle was terminated and reaped before refusing the spawn"
            } else {
                "act_run_shell spawned a child process without an observable pid and could not verify exact-child reaping before the cleanup backstop"
            },
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "command": params.command,
                "command_metadata_policy": SHELL_COMMAND_METADATA_POLICY,
                "args": shell_command_metadata(&params.command, &params.args).args,
                "args_sha256": shell_args_sha256(&params.args),
                "working_dir": params.working_dir,
                "reason": "pid_unavailable",
                "cleanup_verified": cleanup.reaped,
                "cleanup": cleanup,
            }),
        ));
    };
    let local_process_identity = capture_local_process_identity(pid).map_err(|identity_error| {
        let cleanup = terminate_and_reap_tokio_child_bounded(
            &mut child,
            Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
        );
        shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "act_run_shell could not bind spawned pid {pid} to its kernel creation identity; exact-child cleanup followed: {identity_error}"
            ),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "reason": "local_process_identity_capture_failed",
                "identity_error": identity_error,
                "cleanup_verified": cleanup.reaped,
                "cleanup": cleanup,
            }),
        )
    })?;
    let mut process_job = match assign_owned_process_job(pid, "act_run_shell", None) {
        Ok(process_job) => process_job,
        Err(assignment_error) => {
            let cleanup = terminate_and_reap_tokio_child_bounded(
                &mut child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            );
            return Err(shell_tool_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "act_run_shell could not establish kill-on-close ownership for spawned pid {pid}; exact-child cleanup verified={}: {}",
                    cleanup.reaped, assignment_error.message
                ),
                json!({
                    "code": error_codes::TOOL_INTERNAL_ERROR,
                    "pid": pid,
                    "reason": "job_object_assignment_failed",
                    "assignment_error": assignment_error,
                    "cleanup_verified": cleanup.reaped,
                    "cleanup": cleanup,
                }),
            ));
        }
    };
    if let Err(resume_error) = resume_suspended_shell_child(&local_process_identity) {
        let job_close = process_job.close_checked();
        let cleanup = terminate_and_reap_tokio_child_bounded(
            &mut child,
            Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
        );
        return Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "act_run_shell could not safely resume contained pid {pid}; job_close={job_close:?}; exact-child cleanup verified={}: {resume_error}",
                cleanup.reaped,
            ),
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "pid": pid,
                "reason": "contained_child_resume_failed",
                "resume_error": resume_error,
                "job_close": format!("{job_close:?}"),
                "cleanup_verified": cleanup.reaped,
                "cleanup": cleanup,
            }),
        ));
    }
    Ok(SpawnedShellChild {
        child,
        process_job,
        local_process_identity,
    })
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

async fn wait_shell_child_with_identity(
    child: &mut tokio::process::Child,
    local_process_identity: Option<&ActRunShellLocalProcessIdentity>,
    timeout_ms: u64,
    started: Instant,
    timeout_deadline: tokio::time::Instant,
    boundary: Option<&PhysicalMutationBoundary<'_>>,
) -> Result<(Option<i32>, bool), ErrorData> {
    let budget = inline_shell_timeout_budget(timeout_ms);
    // Source of truth for `timed_out`, at parity with the durable path (#1588):
    // the kernel-recorded process runtime, captured BEFORE the wait so it stays
    // readable after tokio reaps the child. `tokio::time::timeout_at` polls the
    // inner future first and delivers a past-deadline self-exit as `Ok(exit)`,
    // and under runtime starvation this task may be dispatched long after both
    // the deadline and the child's exit — so neither the timer branch nor any
    // wall clock this task samples can classify correctly on its own. Only the
    // OS runtime reveals whether the child truly outran `timeout_ms`.
    let runtime_probe = ChildRuntimeProbe::capture(child);
    let child_outran_budget = || -> Result<bool, ErrorData> {
        let observed_elapsed = started.elapsed();
        if let Some(verdict) = completed_inline_timeout_verdict(
            runtime_probe.as_ref().and_then(ChildRuntimeProbe::runtime),
            observed_elapsed,
            budget,
        ) {
            return Ok(verdict);
        }
        Err(shell_tool_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "act_run_shell child exited near/past the deadline, but this platform could not provide an exact process-runtime verdict",
            json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "reason": "inline_timeout_runtime_verdict_unavailable",
                "timeout_ms": timeout_ms,
                "observed_elapsed_ms": u64::try_from(observed_elapsed.as_millis()).unwrap_or(u64::MAX),
                "platform": std::env::consts::OS,
            }),
        ))
    };
    // `timeout_deadline` was armed before spawn, so scheduler starvation before
    // this future's first poll never extends a still-live child's safety cap.
    // A child that has already exited is classified from kernel process runtime
    // below: completed process evidence wins over total spawn/scheduling
    // overhead, while a still-live child is terminated at the absolute deadline.
    let wait_result = if let Some(boundary) = boundary {
        let mut wait = Box::pin(wait_with_inline_shell_timeout_at(
            timeout_deadline,
            child.wait(),
        ));
        loop {
            tokio::select! {
                result = &mut wait => break Ok(result),
                _ = tokio::time::sleep(PHYSICAL_MUTATION_BOUNDARY_POLL_INTERVAL) => {
                    if let Err(error) = boundary("act_run_shell_while_process_live") {
                        break Err(error);
                    }
                }
            }
        }
    } else {
        Ok(wait_with_inline_shell_timeout_at(timeout_deadline, child.wait()).await)
    };
    let wait_result = match wait_result {
        Ok(wait_result) => wait_result,
        Err(error) => {
            let cleanup = if let Some(identity) = local_process_identity {
                let termination_identity = identity.clone();
                let termination = tokio::task::spawn_blocking(move || {
                    terminate_shell_job_process_tree(&termination_identity)
                })
                .await
                .unwrap_or_else(|join_error| ShellJobTerminationReadback {
                    attempted: false,
                    status: format!("termination_task_join_failed:{join_error}"),
                    remaining_process_ids: child.id().into_iter().collect(),
                });
                let reap = terminate_and_reap_tokio_child_async_bounded(
                    child,
                    Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
                )
                .await;
                let final_identity_state = local_process_identity_state(identity);
                let cleanup_verified = termination.verified_terminal_tree()
                    && reap.reaped
                    && reap.poll_error_count == 0
                    && matches!(
                        final_identity_state,
                        LocalProcessIdentityState::Exited
                            | LocalProcessIdentityState::Absent
                            | LocalProcessIdentityState::Mismatch(_)
                    );
                if !cleanup_verified {
                    synapse_action::record_operator_panic_safety_incident();
                }
                json!({
                    "source_of_truth": "identity-bound process tree + exact Tokio child + final process identity",
                    "termination": termination,
                    "reap": reap,
                    "final_identity_state": final_identity_state,
                    "cleanup_verified": cleanup_verified,
                })
            } else {
                let reap = terminate_and_reap_tokio_child_async_bounded(
                    child,
                    Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
                )
                .await;
                let cleanup_verified = reap.reaped && reap.poll_error_count == 0;
                if !cleanup_verified {
                    synapse_action::record_operator_panic_safety_incident();
                }
                json!({
                    "source_of_truth": "exact Tokio child",
                    "reap": reap,
                    "cleanup_verified": cleanup_verified,
                })
            };
            return Err(physical_mutation_boundary_error(
                error,
                "act_run_shell_while_process_live",
                cleanup,
            ));
        }
    };
    let result = match wait_result {
        // The child exited (possibly just after a late-delivered deadline).
        // Exit-evidence wins: return its real code, and flag `timed_out` only if
        // the OS runtime confirms it actually ran past the cap.
        Ok(Ok(status)) => (status.code(), child_outran_budget()?),
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
            // Deadline fired with the child still pending at poll time. It may
            // have self-exited in the gap before we terminate — grab that exit
            // evidence with a non-blocking `try_wait` so a genuine exit code is
            // never discarded in favor of a manufactured timeout verdict.
            if let Ok(Some(status)) = child.try_wait() {
                return Ok((status.code(), child_outran_budget()?));
            }
            let termination = if let (Some(pid), Some(local_process_identity)) =
                (child.id(), local_process_identity)
            {
                if pid == local_process_identity.pid {
                    // Keep exact-handle termination and process-table reads off
                    // the async executor (#1589).
                    let identity = local_process_identity.clone();
                    tokio::task::spawn_blocking(move || terminate_shell_job_process_tree(&identity))
                        .await
                        .unwrap_or_else(|join_error| ShellJobTerminationReadback {
                            attempted: false,
                            status: format!("termination_task_join_failed:{join_error}"),
                            remaining_process_ids: vec![pid],
                        })
                } else {
                    ShellJobTerminationReadback {
                        attempted: false,
                        status: format!(
                            "timeout_identity_pid_mismatch:child_pid={pid}:identity_pid={}",
                            local_process_identity.pid
                        ),
                        remaining_process_ids: vec![pid],
                    }
                }
            } else {
                ShellJobTerminationReadback {
                    attempted: false,
                    status: "timeout_process_identity_unavailable".to_owned(),
                    remaining_process_ids: Vec::new(),
                }
            };
            tracing::warn!(
                code = "M4_ACT_RUN_SHELL_TIMEOUT_TREE_TERMINATED",
                pid = ?child.id(),
                attempted = termination.attempted,
                status = %termination.status,
                remaining_process_ids = ?termination.remaining_process_ids,
                "act_run_shell timeout requested identity-bound process-tree termination"
            );
            let reap = terminate_and_reap_tokio_child_async_bounded(
                child,
                Duration::from_millis(SHELL_CHILD_REAP_BACKSTOP_MS),
            )
            .await;
            let termination_verified = termination.verified_terminal_tree();
            if !termination_verified || !reap.reaped || reap.poll_error_count > 0 {
                return Err(shell_tool_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_run_shell timeout cleanup could not be fully verified",
                    json!({
                        "code": error_codes::TOOL_INTERNAL_ERROR,
                        "reason": "timeout_cleanup_unverified",
                        "tree_termination": format!("{termination:?}"),
                        "exact_child_reap": format!("{reap:?}"),
                    }),
                ));
            }
            // `TerminateProcess(..., 1)` produces an exit status, but that is
            // cleanup evidence rather than a natural command verdict. Preserve
            // a real code only for the race where the tree was already gone
            // before any destructive action was attempted.
            let exit_code = if termination.attempted {
                None
            } else {
                reap.exit_code
            };
            (exit_code, true)
        }
    };
    Ok(result)
}

#[cfg(windows)]
fn shell_status_open_error_is_retryable(
    kind: io::ErrorKind,
    raw_os_error: Option<i32>,
    replace_in_flight: bool,
    destination_exists: bool,
) -> bool {
    // ERROR_ACCESS_DENIED = 5, ERROR_SHARING_VIOLATION = 32.
    const TRANSIENT_OPEN_CODES: [i32; 2] = [5, 32];
    let transient_open = raw_os_error.is_some_and(|code| TRANSIENT_OPEN_CODES.contains(&code));
    let mid_replace = kind == io::ErrorKind::NotFound && (replace_in_flight || destination_exists);
    transient_open || mid_replace
}

fn inline_shell_timeout_budget(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms)
}

/// Classify an already-completed child without confusing scheduler delay with
/// process runtime. `None` is deliberate: when the kernel runtime is unavailable
/// and the host clock is already past the budget, either verdict is possible and
/// callers must fail loud instead of inventing `timed_out`.
fn completed_inline_timeout_verdict(
    kernel_runtime: Option<Duration>,
    observed_elapsed: Duration,
    budget: Duration,
) -> Option<bool> {
    kernel_runtime
        .map(|runtime| runtime >= budget)
        .or_else(|| (observed_elapsed < budget).then_some(false))
}

async fn wait_with_inline_shell_timeout_at<F>(
    deadline: tokio::time::Instant,
    future: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    tokio::time::timeout_at(deadline, future).await
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

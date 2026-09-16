#![allow(
    clippy::borrow_as_ptr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::collapsible_if,
    clippy::doc_markdown,
    clippy::filter_map_bool_then,
    clippy::ignored_unit_patterns,
    clippy::implicit_clone,
    clippy::items_after_statements,
    clippy::manual_clamp,
    clippy::manual_strip,
    clippy::map_unwrap_or,
    clippy::match_same_arms,
    clippy::missing_const_for_fn,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_pass_by_value,
    clippy::needless_question_mark,
    clippy::needless_return,
    clippy::nonminimal_bool,
    clippy::option_if_let_else,
    clippy::question_mark,
    clippy::redundant_closure_for_method_calls,
    clippy::redundant_closure,
    clippy::redundant_guards,
    clippy::redundant_pub_crate,
    clippy::ref_option,
    clippy::result_large_err,
    clippy::significant_drop_tightening,
    clippy::similar_names,
    clippy::single_match_else,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_wraps,
    clippy::unused_async,
    reason = "synapse-mcp keeps pedantic/nursery style lint debt explicit while using clippy -D warnings for behavior-level regressions"
)]
#![cfg_attr(
    test,
    allow(
        clippy::bool_assert_comparison,
        clippy::expect_used,
        clippy::float_cmp,
        clippy::items_after_test_module,
        clippy::manual_let_else,
        clippy::needless_raw_string_hashes,
        clippy::redundant_clone,
        clippy::unreadable_literal,
        clippy::unwrap_used
    )
)]
mod approval_protocol;
mod chrome_debugger_bridge;
mod connect;
mod daemon_lifecycle;
mod desktop_worker;
mod doctor;
mod emitter_shutdown;
mod http;
mod local_agent;
mod m1;
mod m2;
mod m3;
mod m4;
mod safety;
mod secret_crypto;
mod server;
mod single_instance;
mod stdio_eof;
#[cfg(test)]
mod test_support;

use std::{num::NonZeroUsize, path::PathBuf, process::ExitCode, time::Duration};

use anyhow::Context;
use clap::{ArgAction, Parser, ValueEnum};
use rmcp::ServiceExt;
use synapse_telemetry::{TelemetryConfig, TelemetryGuard, init_tracing};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::filter::LevelFilter;

use crate::stdio_eof::CancelOnEofRead;
use crate::{
    emitter_shutdown::{
        M2EmitterDrainReport, M2EmitterOwner, ShutdownTaskOwner, drain_m2_emitter_owner,
        take_m2_emitter_owner,
    },
    server::SynapseService,
};

const ALLOW_SHELL_ENV: &str = "SYNAPSE_ALLOW_SHELL";
const ALLOW_LAUNCH_ENV: &str = "SYNAPSE_ALLOW_LAUNCH";
const STDIO_SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(5);

type StdioServiceTask =
    ShutdownTaskOwner<Result<rmcp::service::QuitReason, tokio::task::JoinError>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Mode {
    Stdio,
    Http,
    /// Thin stdio<->HTTP bridge to the shared daemon (for stdio-only clients).
    Connect,
    /// Chrome native-messaging host for the bundled debugger extension.
    ChromeNativeHost,
    /// Internal protocol-activation child for actionable approval toasts.
    ApprovalProtocol,
    /// Internal child process bound to a hidden desktop for UIA/PrintWindow work.
    DesktopWorker,
    /// Enumerate/classify synapse-mcp processes; with --kill-stray, clean them.
    Doctor,
    /// Run a registry-backed local model as a Synapse MCP client/agent.
    LocalAgent,
}

#[derive(Debug, Parser)]
#[command(name = "synapse-mcp", version, about = "Synapse MCP daemon")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "CLI flags intentionally mirror independent operator startup gates"
)]
struct Cli {
    #[arg(long, value_enum, default_value_t = Mode::Stdio, env = "SYNAPSE_MODE")]
    mode: Mode,
    #[arg(long, default_value = "127.0.0.1:7700", env = "SYNAPSE_BIND")]
    bind: String,
    #[arg(long, env = "SYNAPSE_ALLOW_NON_LOOPBACK")]
    allow_non_loopback: bool,
    #[arg(long, env = "SYNAPSE_DB")]
    db: Option<PathBuf>,
    #[arg(long, env = "SYNAPSE_PROFILE_DIR")]
    profile_dir: Option<PathBuf>,
    #[arg(long, env = "SYNAPSE_LOG_LEVEL", default_value = "info")]
    log_level: String,
    #[arg(long, env = "SYNAPSE_REFLEX_DISABLED")]
    reflex_disabled: bool,
    /// In `--mode doctor`, kill matching stray synapse-mcp processes once a
    /// live lock-holder daemon is identified for the selected DB path.
    #[arg(long)]
    kill_stray: bool,
    #[arg(long, env = "SYNAPSE_ENABLE_AUDIO")]
    enable_audio: bool,
    /// Restrict action dispatch to reviewed profiles. Off by default: Synapse
    /// is general Windows computer-control, so unknown/unprofiled foreground
    /// apps are actionable out of the box. Set to fail closed on unknown scope.
    #[arg(long, env = "SYNAPSE_RESTRICT_UNKNOWN_PROFILE")]
    restrict_unknown_profile: bool,
    /// Explicit M3 permission grant allowlist. Unset defaults to read-only.
    #[arg(long, env = "SYNAPSE_MCP_ALLOWED_PERMISSIONS", value_name = "LIST")]
    allowed_permissions: Option<String>,
    #[arg(long, env = "SYNAPSE_REFLEX_FORCE_DEGRADED")]
    reflex_force_degraded: bool,
    #[arg(
        long,
        env = "SYNAPSE_STORAGE_PRESSURE_FREE_BYTES_SAMPLE",
        value_name = "BYTES"
    )]
    storage_pressure_free_bytes_sample: Option<u64>,
    #[arg(
        long,
        env = "SYNAPSE_MAX_SUBSCRIPTIONS",
        default_value_t = synapse_reflex::DEFAULT_MAX_SUBSCRIPTIONS_NONZERO,
        value_name = "COUNT"
    )]
    max_subscriptions: NonZeroUsize,
    #[arg(
        long,
        value_name = "REGEX",
        action = ArgAction::Append,
        help = "Allow act_run_shell command-line regex; repeat for multiple entries. Env: SYNAPSE_ALLOW_SHELL comma-separated"
    )]
    allow_shell: Vec<String>,
    #[arg(
        long,
        value_name = "REGEX",
        action = ArgAction::Append,
        help = "Allow act_launch target regex; repeat for multiple entries. Env: SYNAPSE_ALLOW_LAUNCH comma-separated"
    )]
    allow_launch: Vec<String>,
    #[arg(
        long,
        env = "SYNAPSE_RUN_SHELL_INLINE_AWAIT_LIMIT_MS",
        default_value_t = m4::DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
        value_name = "MILLISECONDS",
        help = "Inline await budget for act_run_shell before it returns a durable job handle. Set 0 to background every direct shell request."
    )]
    run_shell_inline_await_limit_ms: u64,
    #[arg(
        long,
        env = "SYNAPSE_CHROME_NATIVE_ORIGIN",
        default_value = "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/",
        help = "Origin used only for explicit --mode chrome-native-host diagnostics. Chrome native messaging normally passes the real origin as argv[1]."
    )]
    chrome_native_origin: String,
    #[arg(long, hide = true)]
    approval_uri: Option<String>,
    #[arg(long, value_enum, hide = true)]
    desktop_worker_op: Option<desktop_worker::DesktopWorkerOp>,
    #[arg(long, hide = true)]
    desktop_worker_hwnd: Option<i64>,
    #[arg(long, hide = true)]
    desktop_worker_region: Option<String>,
    #[arg(long, hide = true)]
    desktop_worker_client_region: bool,
    #[arg(long, hide = true)]
    desktop_worker_depth: Option<u32>,
    #[arg(long, hide = true)]
    desktop_worker_json: Option<PathBuf>,
    #[arg(long, hide = true)]
    desktop_worker_bgra: Option<PathBuf>,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_MODEL", value_name = "NAME")]
    local_agent_model: Option<String>,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_TASK", value_name = "TEXT")]
    local_agent_task: Option<String>,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_TASK_FILE", value_name = "PATH")]
    local_agent_task_file: Option<PathBuf>,
    #[arg(
        long,
        env = "SYNAPSE_LOCAL_AGENT_MCP_URL",
        default_value = "http://127.0.0.1:7700/mcp"
    )]
    local_agent_mcp_url: String,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_SPAWN_ID", value_name = "ID")]
    local_agent_spawn_id: Option<String>,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_LOG_DIR", value_name = "PATH")]
    local_agent_log_dir: Option<PathBuf>,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_TARGET_JSON", value_name = "JSON")]
    local_agent_target_json: Option<String>,
    // Computer-use tasks (open an app, take several actions, verify) need many
    // model turns; 8 was far too few to finish anything beyond a single action.
    // The per-turn wall-clock `--local-agent-timeout-ms` guard is the real
    // safety bound, so a generous turn budget is safe. Override per-run with
    // SYNAPSE_LOCAL_AGENT_MAX_TURNS.
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_MAX_TURNS", default_value_t = 40)]
    local_agent_max_turns: u32,
    #[arg(
        long,
        env = "SYNAPSE_LOCAL_AGENT_TIMEOUT_MS",
        default_value_t = 120_000
    )]
    local_agent_timeout_ms: u64,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_HOLD_OPEN_MS", default_value_t = 0)]
    local_agent_hold_open_ms: u64,
    #[arg(
        long,
        env = "SYNAPSE_LOCAL_AGENT_CONTEXT_CHAR_LIMIT",
        default_value_t = 120_000
    )]
    local_agent_context_char_limit: usize,
    #[arg(
        long,
        env = "SYNAPSE_LOCAL_AGENT_TOOL_PARSE_RETRY_LIMIT",
        default_value_t = 2
    )]
    local_agent_tool_parse_retry_limit: u32,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_NO_STREAM")]
    local_agent_no_stream: bool,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_ALLOW_NON_LOOPBACK")]
    local_agent_allow_non_loopback: bool,
    #[arg(long, env = "SYNAPSE_LOCAL_AGENT_TRUSTED_UNATTENDED_EXACT_CONTRACT")]
    local_agent_trusted_unattended_exact_contract: bool,
}

impl Cli {
    fn m2_config() -> m2::M2ServiceConfig {
        m2::M2ServiceConfig::from_env()
    }

    fn m3_config(&self) -> m3::M3ServiceConfig {
        m3::M3ServiceConfig::from_cli_parts(
            self.db.clone(),
            self.profile_dir.clone(),
            self.reflex_disabled,
            self.bind.clone(),
            self.max_subscriptions,
            self.enable_audio,
            !self.restrict_unknown_profile,
            self.allowed_permissions.clone(),
            self.reflex_force_degraded,
            self.storage_pressure_free_bytes_sample,
        )
    }

    fn m4_config(&self) -> anyhow::Result<m4::M4ServiceConfig> {
        let mut allow_shell = parse_env_list(ALLOW_SHELL_ENV);
        allow_shell.extend(self.allow_shell.clone());
        let mut allow_launch = parse_env_list(ALLOW_LAUNCH_ENV);
        allow_launch.extend(self.allow_launch.clone());
        m4::M4ServiceConfig::from_cli_parts(
            allow_shell,
            allow_launch,
            self.run_shell_inline_await_limit_ms,
        )
    }
}

fn parse_env_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .map(|raw| raw.split(',').map(ToOwned::to_owned).collect())
        .unwrap_or_default()
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return top_level_error_exit(anyhow::anyhow!("initialize tokio runtime: {error:#}"));
        }
    };
    let result = runtime.block_on(run());
    runtime.shutdown_timeout(Duration::from_secs(5));
    match result {
        Ok(code) => code,
        Err(err) => top_level_error_exit(err),
    }
}

fn top_level_error_exit(err: anyhow::Error) -> ExitCode {
    if let Err(lifecycle_error) = daemon_lifecycle::record_top_level_error(&format!("{err:#}")) {
        eprintln!("synapse-mcp lifecycle error: {lifecycle_error:#}");
    }
    eprintln!("synapse-mcp error: {err:#}");
    ExitCode::from(1)
}

async fn run() -> anyhow::Result<ExitCode> {
    if let Some(invocation) =
        chrome_debugger_bridge::native_host_invocation_from_args(std::env::args_os().skip(1))
    {
        let bind = std::env::var("SYNAPSE_BIND").unwrap_or_else(|_| "127.0.0.1:7700".to_owned());
        let telemetry_guard = configure_telemetry_from_level(
            &std::env::var("SYNAPSE_LOG_LEVEL").unwrap_or_else(|_| "info".to_owned()),
        )?;
        let result = chrome_debugger_bridge::run_native_host(&bind, invocation).await;
        drop(telemetry_guard);
        return result;
    }

    let cli = Cli::parse();

    let telemetry_guard = configure_telemetry(&cli)?;

    // The connect bridge is a thin stdio<->HTTP proxy; it does not initialize
    // perception/action/storage, so return before the daemon-only setup below.
    if matches!(cli.mode, Mode::Connect) {
        let result = connect::run_connect(&cli.bind, cli.db.as_deref()).await;
        drop(telemetry_guard);
        return result;
    }
    if matches!(cli.mode, Mode::Doctor) {
        let code = doctor::run_doctor(cli.kill_stray, cli.db.as_deref());
        drop(telemetry_guard);
        return Ok(code);
    }
    if matches!(cli.mode, Mode::ChromeNativeHost) {
        let result = chrome_debugger_bridge::run_native_host(
            &cli.bind,
            chrome_debugger_bridge::NativeHostInvocation {
                origin: cli.chrome_native_origin.clone(),
                parent_window: None,
            },
        )
        .await;
        drop(telemetry_guard);
        return result;
    }
    if matches!(cli.mode, Mode::ApprovalProtocol) {
        let approval_uri = cli
            .approval_uri
            .as_deref()
            .context("--approval-uri is required for --mode approval-protocol")?;
        let result = approval_protocol::run_protocol_activation(approval_uri).await;
        drop(telemetry_guard);
        return result;
    }
    if matches!(cli.mode, Mode::DesktopWorker) {
        let code = desktop_worker::run_worker_from_cli(desktop_worker::DesktopWorkerCli {
            op: cli.desktop_worker_op,
            hwnd: cli.desktop_worker_hwnd,
            region: cli.desktop_worker_region.clone(),
            client_region: cli.desktop_worker_client_region,
            depth: cli.desktop_worker_depth,
            json_path: cli.desktop_worker_json.clone(),
            bgra_path: cli.desktop_worker_bgra.clone(),
        })?;
        drop(telemetry_guard);
        return Ok(code);
    }
    if matches!(cli.mode, Mode::LocalAgent) {
        let result = local_agent::run_from_cli(local_agent::LocalAgentCli {
            model_name: cli.local_agent_model.clone(),
            task: cli.local_agent_task.clone(),
            task_file: cli.local_agent_task_file.clone(),
            mcp_url: cli.local_agent_mcp_url.clone(),
            spawn_id: cli.local_agent_spawn_id.clone(),
            log_dir: cli.local_agent_log_dir.clone(),
            target_json: cli.local_agent_target_json.clone(),
            max_turns: cli.local_agent_max_turns,
            timeout_ms: cli.local_agent_timeout_ms,
            hold_open_ms: cli.local_agent_hold_open_ms,
            context_char_limit: cli.local_agent_context_char_limit,
            tool_parse_retry_limit: cli.local_agent_tool_parse_retry_limit,
            no_stream: cli.local_agent_no_stream,
            allow_non_loopback: cli.local_agent_allow_non_loopback,
            trusted_unattended_exact_contract: cli.local_agent_trusted_unattended_exact_contract,
        })
        .await;
        drop(telemetry_guard);
        return result;
    }

    let dpi_awareness = synapse_capture::init_process_dpi_awareness()
        .context("initialize per-monitor DPI awareness")?;
    tracing::info!(?cli, code = "MCP_CLI_PARSED", "synapse-mcp cli parsed");
    tracing::info!(
        ?dpi_awareness,
        code = "CAPTURE_DPI_AWARENESS_INITIALIZED",
        "capture dpi awareness initialized"
    );
    let recovery_file = synapse_action::configure_crash_recovery_file(cli.db.as_deref())
        .context("configure action crash recovery ledger")?;
    let recovery_report = synapse_action::recover_stale_inputs_from_configured_path()
        .context("recover stale action inputs from previous daemon")?;
    tracing::info!(
        code = "ACTION_CRASH_RECOVERY_CONFIGURED",
        recovery_file = %recovery_file.display(),
        recovered_keys = recovery_report.recovered_keys,
        recovered_buttons = recovery_report.recovered_buttons,
        recovered_pads = recovery_report.recovered_pads,
        ignored_trailing_bytes = recovery_report.ignored_trailing_bytes,
        "action crash recovery ledger configured"
    );

    let m2_config = Cli::m2_config();
    let m3_config = cli.m3_config();
    let m4_config = match cli.m4_config() {
        Ok(config) => config,
        Err(error) => {
            if let Some(broad_pattern) = error.downcast_ref::<m4::BroadAllowPatternError>() {
                tracing::error!(
                    event = "CONFIG_INVALID",
                    code = broad_pattern.code(),
                    tool = broad_pattern.tool_name(),
                    source = broad_pattern.source_name(),
                    pattern = broad_pattern.raw(),
                    reason = broad_pattern.reason(),
                    "CONFIG_INVALID code={}",
                    broad_pattern.code()
                );
                eprintln!("synapse-mcp error: {error:#}");
                drop(telemetry_guard);
                return Ok(ExitCode::from(2));
            }
            return Err(error);
        }
    };

    match cli.mode {
        Mode::Stdio => run_stdio(telemetry_guard, &m2_config, m3_config, m4_config).await,
        Mode::Http => {
            let code = http::serve(
                &cli.bind,
                cli.allow_non_loopback,
                &m2_config,
                m3_config,
                m4_config,
            )
            .await?;
            drop(telemetry_guard);
            Ok(code)
        }
        Mode::Connect
        | Mode::ChromeNativeHost
        | Mode::ApprovalProtocol
        | Mode::DesktopWorker
        | Mode::Doctor
        | Mode::LocalAgent => {
            unreachable!(
                "connect, chrome-native-host, approval-protocol, desktop-worker, doctor, and local-agent modes are handled before daemon setup"
            )
        }
    }
}

fn configure_telemetry(cli: &Cli) -> anyhow::Result<TelemetryGuard> {
    configure_telemetry_from_level(&cli.log_level)
}

fn configure_telemetry_from_level(log_level: &str) -> anyhow::Result<TelemetryGuard> {
    let level = log_level
        .parse::<LevelFilter>()
        .with_context(|| format!("invalid log level {log_level}"))?;
    let log_dir = std::env::var_os("SYNAPSE_LOG_DIR").map(PathBuf::from);
    init_tracing(TelemetryConfig {
        log_dir,
        file_level: level,
        console_level: level,
        ..TelemetryConfig::default()
    })
    .context("initialize telemetry")
}

fn aggregate_stdio_shutdown_results(
    reason: &'static str,
    results: Vec<(&'static str, anyhow::Result<()>)>,
) -> anyhow::Result<()> {
    let failures = results
        .into_iter()
        .filter_map(|(phase, result)| result.err().map(|error| (phase, format!("{error:#}"))))
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return Ok(());
    }

    let detail = failures
        .iter()
        .map(|(phase, error)| format!("{phase}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");
    tracing::error!(
        code = "MCP_STDIO_SHUTDOWN_INCOMPLETE",
        reason,
        failure_count = failures.len(),
        failures = ?failures,
        "stdio shutdown completed all cleanup attempts but one or more physical postconditions failed"
    );
    use std::io::Write as _;
    if let Err(stderr_error) = writeln!(
        std::io::stderr().lock(),
        "synapse-mcp stdio shutdown error: reason={reason} failures={detail}"
    ) {
        tracing::error!(
            code = "MCP_STDIO_SHUTDOWN_STDERR_WRITE_FAILED",
            reason,
            error = %stderr_error,
            "failed to write stdio shutdown failure to stderr"
        );
    }
    anyhow::bail!("stdio shutdown incomplete ({reason}): {detail}")
}

fn inspect_stdio_service_task_join(
    result: Result<
        Result<rmcp::service::QuitReason, tokio::task::JoinError>,
        tokio::task::JoinError,
    >,
    source: &'static str,
) -> (bool, anyhow::Result<()>) {
    match result {
        Ok(Ok(rmcp::service::QuitReason::JoinError(error))) => {
            // `waiting()` awaited the private rmcp JoinHandle, so the service
            // task is terminal even though one of its owned send tasks failed.
            (
                true,
                Err(anyhow::Error::new(error)).with_context(|| {
                    format!("rmcp stdio service reported an internal join failure ({source})")
                }),
            )
        }
        Ok(Ok(reason)) => {
            tracing::info!(
                code = "MCP_STDIO_SERVICE_TASK_JOINED",
                source,
                quit_reason = ?reason,
                "owned rmcp stdio service task reached a terminal join"
            );
            (true, Ok(()))
        }
        Ok(Err(error)) => {
            // The private rmcp task was awaited and is terminal; preserve its
            // failed JoinError in the process verdict without retaining locks
            // for a task that no longer exists.
            (
                true,
                Err(anyhow::Error::new(error))
                    .with_context(|| format!("join rmcp stdio service task ({source})")),
            )
        }
        Err(error) => {
            // The outer owner failed before it could publish the result of
            // `RunningService::waiting`. Its Drop can detach rmcp's private
            // JoinHandle, so terminal ownership is unproven even if this outer
            // JoinError itself is terminal.
            (
                false,
                Err(anyhow::Error::new(error)).with_context(|| {
                    format!("stdio service join owner failed before inner join readback ({source})")
                }),
            )
        }
    }
}

async fn stop_stdio_service_task_after_cancel(
    service_task: &mut StdioServiceTask,
    source: &'static str,
) -> (bool, anyhow::Result<()>) {
    match tokio::time::timeout(STDIO_SERVICE_STOP_TIMEOUT, &mut *service_task).await {
        Ok(result) => inspect_stdio_service_task_join(result, source),
        Err(_elapsed) => {
            let detail = format!(
                "rmcp stdio service did not reach an owned terminal join within {} ms after cancellation ({source}); retaining the live outer waiting() JoinHandle and daemon lifetime locks until process teardown so rmcp's private service-task owner is never detached",
                STDIO_SERVICE_STOP_TIMEOUT.as_millis()
            );
            tracing::error!(
                code = "MCP_STDIO_SERVICE_STOP_TIMEOUT",
                source,
                stop_timeout_ms = STDIO_SERVICE_STOP_TIMEOUT.as_millis(),
                "rmcp stdio service-task ownership did not reach a trustworthy terminal state; retaining its exact outer owner"
            );
            (false, Err(anyhow::anyhow!(detail)))
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct StdioLifetimeLockReadiness {
    authority_safe_to_unlock: bool,
    server_dispatch_quiescent: bool,
    m2_emitter_safe: bool,
    win_event_owners_quiescent: bool,
    hotkey_owners_quiescent: bool,
    k2_tasks_quiescent: bool,
    desktop_worker_owners_quiescent: bool,
    retained_shutdown_task_owners_quiescent: bool,
    unresolved_shell_child_owners_quiescent: bool,
    activity_recorder_retained_owners_quiescent: bool,
}

const fn stdio_lifetime_locks_safe_to_close(readiness: StdioLifetimeLockReadiness) -> bool {
    readiness.authority_safe_to_unlock
        && readiness.server_dispatch_quiescent
        && readiness.m2_emitter_safe
        && readiness.win_event_owners_quiescent
        && readiness.hotkey_owners_quiescent
        && readiness.k2_tasks_quiescent
        && readiness.desktop_worker_owners_quiescent
        && readiness.retained_shutdown_task_owners_quiescent
        && readiness.unresolved_shell_child_owners_quiescent
        && readiness.activity_recorder_retained_owners_quiescent
}

fn close_stdio_lifetime_locks(
    shell_job_store: crate::single_instance::ShellJobStoreLockGuard,
    single_instance: crate::single_instance::SingleInstanceGuard,
    mut readiness: StdioLifetimeLockReadiness,
    reason: &'static str,
) -> anyhow::Result<()> {
    let desktop_worker_owner_report = crate::desktop_worker::desktop_worker_retained_owner_report();
    let desktop_worker_owners_quiescent = desktop_worker_owner_report.active_owner_count == 0;
    let retained_shutdown_task_owner_report =
        crate::emitter_shutdown::retained_shutdown_task_owner_report();
    let retained_shutdown_task_owners_quiescent =
        retained_shutdown_task_owner_report.safe_to_unlock();
    let unresolved_shell_child_owner_report = crate::m4::unresolved_shell_child_owner_report();
    let unresolved_shell_child_owners_quiescent =
        unresolved_shell_child_owner_report.safe_to_unlock();
    let activity_recorder_retained_owner_readback =
        crate::m3::activity_recorder::retained_owner_readback();
    let activity_recorder_retained_owners_quiescent =
        activity_recorder_retained_owner_readback.safe_to_unlock();
    readiness.desktop_worker_owners_quiescent = desktop_worker_owners_quiescent;
    readiness.retained_shutdown_task_owners_quiescent = retained_shutdown_task_owners_quiescent;
    readiness.unresolved_shell_child_owners_quiescent = unresolved_shell_child_owners_quiescent;
    readiness.activity_recorder_retained_owners_quiescent =
        activity_recorder_retained_owners_quiescent;
    if !stdio_lifetime_locks_safe_to_close(readiness) {
        tracing::error!(
            code = "MCP_STDIO_LIFETIME_LOCKS_RETAINED",
            reason,
            authority_safe_to_unlock = readiness.authority_safe_to_unlock,
            server_dispatch_quiescent = readiness.server_dispatch_quiescent,
            m2_emitter_safe = readiness.m2_emitter_safe,
            win_event_owners_quiescent = readiness.win_event_owners_quiescent,
            hotkey_owners_quiescent = readiness.hotkey_owners_quiescent,
            k2_tasks_quiescent = readiness.k2_tasks_quiescent,
            desktop_worker_owners_quiescent = readiness.desktop_worker_owners_quiescent,
            desktop_worker_owner_report = ?desktop_worker_owner_report,
            retained_shutdown_task_owners_quiescent = readiness.retained_shutdown_task_owners_quiescent,
            retained_shutdown_task_owner_report = ?retained_shutdown_task_owner_report,
            unresolved_shell_child_owners_quiescent = readiness.unresolved_shell_child_owners_quiescent,
            unresolved_shell_child_owner_report = ?unresolved_shell_child_owner_report,
            activity_recorder_retained_owners_quiescent = readiness.activity_recorder_retained_owners_quiescent,
            activity_recorder_retained_owner_readback = ?activity_recorder_retained_owner_readback,
            "one or more stdio daemon task owners remained live; retaining both daemon lifetime locks until process teardown"
        );
        use std::io::Write as _;
        if let Err(stderr_error) = writeln!(
            std::io::stderr().lock(),
            "synapse-mcp fatal shutdown error: reason={reason} readiness={readiness:?} desktop_worker_owner_report={desktop_worker_owner_report:?} retained_shutdown_task_owner_report={retained_shutdown_task_owner_report:?} unresolved_shell_child_owner_report={unresolved_shell_child_owner_report:?} activity_recorder_retained_owner_readback={activity_recorder_retained_owner_readback:?}; daemon lifetime locks retained until process teardown"
        ) {
            tracing::error!(
                code = "MCP_STDIO_LIFETIME_LOCK_RETAIN_STDERR_WRITE_FAILED",
                reason,
                error = %stderr_error,
                "failed to write retained lifetime-lock failure to stderr"
            );
        }
        // These guards are deliberately retained, not ordinarily dropped. A
        // nonzero authority owner or an unproven rmcp dispatch owner means
        // releasing either lock could admit a successor while old work still
        // owns storage, rollback, audit, or transport state.
        // Windows closes the owned handles at process teardown after Tokio has
        // torn down the remaining task; startup stale-sidecar recovery handles
        // the deliberately unwritten graceful-close evidence.
        std::mem::forget(shell_job_store);
        std::mem::forget(single_instance);
        anyhow::bail!(
            "refused to release daemon lifetime locks after {reason}: readiness={readiness:?} desktop_worker_owner_report={desktop_worker_owner_report:?} retained_shutdown_task_owner_report={retained_shutdown_task_owner_report:?} unresolved_shell_child_owner_report={unresolved_shell_child_owner_report:?} activity_recorder_retained_owner_readback={activity_recorder_retained_owner_readback:?}"
        );
    }
    crate::single_instance::close_daemon_lifetime_locks(shell_job_store, single_instance)
        .map(|_readback| ())
        .map_err(anyhow::Error::new)
        .with_context(|| format!("close daemon lifetime locks after {reason}"))
}

fn inspect_authority_finalizer_drain(
    result: Result<
        crate::server::AuthorityFinalizerDrainReadback,
        crate::server::AuthorityFinalizerDrainFailure,
    >,
    context: &'static str,
) -> (bool, anyhow::Result<()>) {
    let safe_to_unlock = match &result {
        Ok(readback) => readback.safe_to_unlock(),
        Err(error) => error.readback.safe_to_unlock(),
    };
    let verdict = result
        .map(|_readback| ())
        .map_err(anyhow::Error::new)
        .context(context);
    (safe_to_unlock, verdict)
}

async fn drain_stdio_m2_owner(
    owner: &mut Option<M2EmitterOwner>,
    reason: &'static str,
) -> M2EmitterDrainReport {
    drain_m2_emitter_owner(owner.take(), "stdio", reason).await
}

#[derive(Clone, Debug)]
pub(crate) struct WinEventShutdownHistoryReadback {
    report_count: usize,
    retained_owner_count: usize,
    unsafe_owner_ids: Vec<u64>,
    failures: Vec<String>,
}

impl WinEventShutdownHistoryReadback {
    pub(crate) fn owners_quiescent(&self) -> bool {
        self.report_count >= self.unsafe_owner_ids.len()
            && self.retained_owner_count == 0
            && self.unsafe_owner_ids.is_empty()
    }

    pub(crate) fn verdict(&self) -> anyhow::Result<()> {
        if self.failures.is_empty() && self.owners_quiescent() {
            Ok(())
        } else {
            anyhow::bail!(
                "WinEvent shutdown history contains an unsafe physical owner: {}; readback={self:?}",
                self.failures.join("; ")
            )
        }
    }
}

pub(crate) fn win_event_shutdown_history_readback() -> WinEventShutdownHistoryReadback {
    let history = synapse_a11y::win_event_shutdown_report_history();
    let retained_owner_count = synapse_a11y::retained_win_event_owner_count();
    win_event_shutdown_history_readback_from(&history, retained_owner_count)
}

fn win_event_shutdown_history_readback_from(
    history: &[synapse_a11y::WinEventSubscriptionShutdownRecord],
    retained_owner_count: usize,
) -> WinEventShutdownHistoryReadback {
    let mut unsafe_owner_ids = Vec::new();
    let mut failures = Vec::new();
    for record in history {
        if let Err(error) = record.report.verdict() {
            unsafe_owner_ids.push(record.owner_id);
            failures.push(format!(
                "owner_id={} failed immutable shutdown readback: {error}",
                record.owner_id
            ));
        }
    }
    if retained_owner_count != 0 {
        failures.push(format!(
            "{retained_owner_count} exact WinEvent owner(s) remain retained"
        ));
    }
    WinEventShutdownHistoryReadback {
        report_count: history.len(),
        retained_owner_count,
        unsafe_owner_ids,
        failures,
    }
}

fn inspect_win_event_shutdown_history(context: &'static str) -> (bool, anyhow::Result<()>) {
    let readback = win_event_shutdown_history_readback();
    let owners_quiescent = readback.owners_quiescent();
    tracing::info!(
        code = "MCP_WIN_EVENT_SHUTDOWN_HISTORY_FINAL_READBACK",
        context,
        readback = ?readback,
        "readback=win_event_shutdown_history edge=transport_shutdown after_service_owner_drop"
    );
    let verdict = readback.verdict().with_context(|| context);
    (owners_quiescent, verdict)
}

struct StdioOperatorOwnerDrain {
    hotkey_owners_quiescent: bool,
    k2_tasks_quiescent: bool,
    hotkey_verdict: anyhow::Result<()>,
    k2_verdict: anyhow::Result<()>,
}

async fn drain_stdio_operator_owners(
    guard: &mut Option<synapse_action::OperatorHotkeyGuard>,
    reason: &'static str,
) -> StdioOperatorOwnerDrain {
    let k2_before = safety::operator_panic_k2_task_owner_readback();
    let hotkey_report = safety::shutdown_operator_hotkey(guard, reason);
    let install_unwind_retained_live_owner =
        safety::operator_hotkey_install_unwind_retained_live_owner();
    let hotkey_owners_quiescent = !install_unwind_retained_live_owner
        && hotkey_report
            .as_ref()
            .is_none_or(synapse_action::OperatorHotkeyShutdownReport::owners_quiescent);
    let hotkey_verdict = if install_unwind_retained_live_owner {
        Err(anyhow::anyhow!(
            "operator-hotkey installation unwind retained a live exact owner"
        ))
    } else {
        hotkey_report.as_ref().map_or(Ok(()), |report| {
            report.verdict().map_err(|error| anyhow::anyhow!("{error}"))
        })
    };
    let k2_report = safety::drain_operator_panic_k2_tasks(reason, hotkey_owners_quiescent).await;
    let k2_tasks_quiescent = k2_report.owners_quiescent();
    let k2_verdict = k2_report.verdict();
    tracing::info!(
        code = "MCP_STDIO_OPERATOR_OWNER_DRAIN_READBACK",
        reason,
        hotkey_report = ?hotkey_report,
        install_unwind_retained_live_owner,
        k2_before = ?k2_before,
        k2_report = ?k2_report,
        "readback=operator_hotkey_and_k2_owners edge=stdio_shutdown after_checked_drain"
    );
    if !hotkey_owners_quiescent {
        safety::retain_operator_hotkey_guard_to_process_exit(guard, reason);
    }
    StdioOperatorOwnerDrain {
        hotkey_owners_quiescent,
        k2_tasks_quiescent,
        hotkey_verdict,
        k2_verdict,
    }
}

fn begin_stdio_graceful_exit_finalization(
    reason: &'static str,
) -> (
    Option<daemon_lifecycle::GracefulExitFinalizationGuard>,
    anyhow::Result<()>,
) {
    let unresolved_shell_child_owner_report = crate::m4::unresolved_shell_child_owner_report();
    if !unresolved_shell_child_owner_report.safe_to_unlock() {
        tracing::error!(
            code = "MCP_STDIO_LIFECYCLE_FINALIZATION_REFUSED",
            reason,
            unresolved_shell_child_owner_report = ?unresolved_shell_child_owner_report,
            "refusing graceful lifecycle finalization while an exact shell child/job owner remains unresolved"
        );
        return (
            None,
            Err(anyhow::anyhow!(
                "refused graceful stdio lifecycle finalization ({reason}): unresolved_shell_child_owner_report={unresolved_shell_child_owner_report:?}"
            )),
        );
    }
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_EXIT_WRITE_START",
        source = reason,
        pid = std::process::id(),
        "locking daemon lifecycle finalization before releasing stdio lifetime locks"
    );
    match daemon_lifecycle::begin_graceful_exit_finalization() {
        Ok(finalization) => (Some(finalization), Ok(())),
        Err(error) => (
            None,
            Err(error).with_context(|| {
                format!(
                    "lock graceful lifecycle transaction before stdio lifetime-lock close ({reason})"
                )
            }),
        ),
    }
}

fn finish_stdio_graceful_exit_finalization(
    finalization: Option<daemon_lifecycle::GracefulExitFinalizationGuard>,
    source: &'static str,
) -> anyhow::Result<()> {
    let finalization = finalization.ok_or_else(|| {
        anyhow::anyhow!(
            "stdio shutdown postconditions passed without an owned lifecycle finalization guard ({source})"
        )
    })?;
    daemon_lifecycle::record_graceful_exit_after_lifetime_lock_close(finalization, source)
        .with_context(|| format!("record daemon lifecycle graceful stdio exit ({source})"))?;
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_EXIT_WRITE_OK",
        source,
        pid = std::process::id(),
        "daemon lifecycle graceful stdio exit written before successor configuration"
    );
    Ok(())
}

async fn run_stdio(
    _telemetry_guard: TelemetryGuard,
    m2_config: &m2::M2ServiceConfig,
    m3_config: m3::M3ServiceConfig,
    m4_config: m4::M4ServiceConfig,
) -> anyhow::Result<ExitCode> {
    tracing::info!(code = "MCP_STDIO_STARTED", "starting stdio MCP transport");

    // Single-instance guard (epic #717 / single-daemon invariant): an embedded
    // stdio daemon is a FULL daemon: it opens RocksDB and owns its own
    // process-global input lease + per-session registries. Without this guard a
    // stray or misconfigured stdio launch would run a SECOND parallel daemon
    // whose lease/state cannot coordinate with the canonical HTTP daemon, which
    // silently breaks multi-agent isolation. The HTTP path already acquires this
    // lock before binding the port; the stdio path must obey the same rule.
    // Fail loud, naming the current holder, and point the operator at --mode
    // connect (the supported way for a stdio-only client to reach the shared
    // daemon) instead of crashing later on a cryptic RocksDB LOCK error.
    let db_path = m3_config
        .db_path
        .clone()
        .unwrap_or_else(crate::m3::default_db_path);
    let single_instance_guard = match crate::single_instance::SingleInstanceGuard::acquire(&db_path)
    {
        Ok(guard) => {
            tracing::info!(
                code = "MCP_DAEMON_SINGLE_INSTANCE_ACQUIRED",
                lock_path = %guard.lock_path().display(),
                db_path = %db_path.display(),
                pid = std::process::id(),
                mode = "stdio",
                "daemon single-instance lock acquired"
            );
            guard
        }
        Err(crate::single_instance::SingleInstanceError::AlreadyRunning {
            lock_path,
            holder_pid,
        }) => {
            let holder = holder_pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string());
            tracing::error!(
                code = "MCP_DAEMON_ALREADY_RUNNING",
                lock_path = %lock_path.display(),
                holder_pid = %holder,
                db_path = %db_path.display(),
                mode = "stdio",
                "refusing to start: another synapse-mcp daemon already owns this DB path"
            );
            eprintln!(
                "synapse-mcp error: another synapse-mcp daemon already owns {} (holder pid {holder}); use --mode connect to reach the shared daemon instead of starting a second one",
                db_path.display()
            );
            return Ok(ExitCode::from(3));
        }
        Err(err @ crate::single_instance::SingleInstanceError::Io { .. }) => {
            return Err(anyhow::Error::new(err)).context("acquire daemon single-instance lock");
        }
    };

    let shell_job_root = match m4::shell_job_root_dir() {
        Ok(root) => root,
        Err(error) => {
            let detail = error.message.to_string();
            let error_data = error.data.unwrap_or(serde_json::Value::Null);
            tracing::error!(
                code = "MCP_DAEMON_SHELL_JOB_STORE_ROOT_RESOLUTION_FAILED",
                mode = "stdio",
                db_path = %db_path.display(),
                detail = %detail,
                error_data = ?error_data,
                "refusing to start: durable shell-job store root could not be resolved"
            );
            eprintln!(
                "synapse-mcp error: durable shell-job store root could not be resolved: {detail}; data={error_data}"
            );
            anyhow::bail!("resolve durable shell-job store root: {detail}; data={error_data}");
        }
    };
    let shell_job_store_lock_guard = match crate::single_instance::ShellJobStoreLockGuard::acquire(
        &shell_job_root,
    ) {
        Ok(guard) => {
            tracing::info!(
                code = "MCP_DAEMON_SHELL_JOB_STORE_LOCK_ACQUIRED",
                mode = "stdio",
                store_root = %guard.store_root().display(),
                lock_path = %guard.lock_path().display(),
                pid = std::process::id(),
                "daemon acquired exclusive shell-job store ownership"
            );
            guard
        }
        Err(crate::single_instance::ShellJobStoreLockError::AlreadyOwned {
            store_root,
            lock_path,
            holder_pid,
        }) => {
            let holder = holder_pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string());
            tracing::error!(
                code = "MCP_DAEMON_SHELL_JOB_STORE_ALREADY_OWNED",
                mode = "stdio",
                store_root = %store_root.display(),
                lock_path = %lock_path.display(),
                holder_pid = %holder,
                db_path = %db_path.display(),
                "refusing to start: another daemon owns the durable shell-job store"
            );
            eprintln!(
                "synapse-mcp error: another daemon owns shell-job store {} via {} (holder pid {holder}); stop it or configure a different SYNAPSE_SHELL_JOB_ROOT",
                store_root.display(),
                lock_path.display()
            );
            return Ok(ExitCode::from(3));
        }
        Err(error @ crate::single_instance::ShellJobStoreLockError::Io { .. }) => {
            tracing::error!(
                code = "MCP_DAEMON_SHELL_JOB_STORE_LOCK_FAILED",
                mode = "stdio",
                detail = %error,
                db_path = %db_path.display(),
                "refusing to start: durable shell-job store ownership could not be acquired"
            );
            return Err(anyhow::Error::new(error))
                .context("acquire durable shell-job store lifetime lock");
        }
    };
    let canonical_shell_job_root = shell_job_store_lock_guard.store_root().to_path_buf();
    if let Err(error) = m4::freeze_shell_job_root_for_daemon(&canonical_shell_job_root) {
        let detail = error.message.to_string();
        let error_data = error.data.unwrap_or(serde_json::Value::Null);
        tracing::error!(
            code = "MCP_DAEMON_SHELL_JOB_STORE_ROOT_FREEZE_FAILED",
            mode = "stdio",
            db_path = %db_path.display(),
            shell_job_root = %canonical_shell_job_root.display(),
            detail = %detail,
            error_data = ?error_data,
            "refusing to start: guarded shell-job store root could not be frozen for daemon operations"
        );
        eprintln!(
            "synapse-mcp error: guarded shell-job store root {} could not be frozen: {detail}; data={error_data}",
            canonical_shell_job_root.display()
        );
        anyhow::bail!(
            "freeze guarded durable shell-job store root {}: {detail}; data={error_data}",
            canonical_shell_job_root.display()
        );
    }

    let lifecycle_paths = daemon_lifecycle::configure(daemon_lifecycle::DaemonLifecycleConfig {
        mode: "stdio",
        bind_addr: None,
        db_path: db_path.clone(),
    })
    .context("configure daemon lifecycle ledger")?;
    daemon_lifecycle::install_panic_hook();
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_READY",
        run_current_path = %lifecycle_paths.run_current_path,
        tool_last_path = %lifecycle_paths.tool_last_path,
        tool_events_path = %lifecycle_paths.tool_events_path,
        exit_events_path = %lifecycle_paths.exit_events_path,
        "daemon lifecycle ledger ready"
    );

    // #1568: corrupt durable shell-job evidence is a startup safety gate. Run
    // it only after the independent shell-job lifetime lock proves this process
    // owns that store, and before the stdio transport can accept any request.
    // Ordinary TTL retention inside this pass remains best-effort.
    if let Err(error) = m4::reap_stale_shell_jobs_on_startup() {
        let detail = error.message.to_string();
        let error_data = error.data.unwrap_or(serde_json::Value::Null);
        tracing::error!(
            code = "MCP_DAEMON_STARTUP_SHELL_JOB_RECOVERY_FAILED",
            mode = "stdio",
            db_path = %db_path.display(),
            shell_job_root = %canonical_shell_job_root.display(),
            detail = %detail,
            error_data = ?error_data,
            "refusing to start: corrupt durable shell-job recovery did not reach a verified terminal disposition"
        );
        daemon_lifecycle::record_startup_exit(
            "startup_corrupt_shell_job_recovery_failed",
            serde_json::json!({
                "mode": "stdio",
                "db_path": db_path.display().to_string(),
                "shell_job_root": canonical_shell_job_root.display().to_string(),
                "detail": detail,
                "error_data": error_data,
            }),
        )
        .context("record daemon lifecycle startup corrupt-shell-job recovery failure")?;
        return Ok(ExitCode::from(4));
    }

    let rmcp_token = CancellationToken::new();
    let emitter_shutdown_token = CancellationToken::new();
    let emitter_connection_closed_token = CancellationToken::new();
    let service = SynapseService::try_with_m2_shutdown_reason_and_m3_config(
        emitter_shutdown_token.clone(),
        "sigint",
        emitter_connection_closed_token.clone(),
        m2_config,
        m3_config,
        m4_config,
    )
    .context("initialize Synapse service state")?;
    {
        let m3_state = service.m3_state_handle();
        let maintenance_result = match m3_state.lock() {
            Ok(mut state) => state
                .ensure_storage_maintenance_tasks()
                .context("start stdio storage maintenance"),
            Err(poisoned) => {
                drop(poisoned);
                Err(anyhow::anyhow!(
                    "m3 service state lock poisoned during stdio storage maintenance startup"
                ))
            }
        };
        if let Err(error) = maintenance_result {
            tracing::error!(
                code = "STORAGE_OPEN_OR_MAINTENANCE_START_FAILED",
                mode = "stdio",
                db_path = %db_path.display(),
                detail = %error,
                "refusing to start: stdio storage open/maintenance startup failed"
            );
            daemon_lifecycle::record_startup_exit(
                "stdio_storage_open_or_maintenance_start_failed",
                serde_json::json!({
                    "mode": "stdio",
                    "db_path": db_path.display().to_string(),
                    "detail": error.to_string(),
                }),
            )
            .context("record daemon lifecycle stdio storage maintenance startup failure")?;
            return Err(error);
        }
    }
    synapse_action::install_panic_hook();
    let authority_finalizer_service = service.clone();
    let mut m2_emitter_owner = Some(take_m2_emitter_owner(&service));
    let mut operator_hotkey_guard = match safety::install_operator_hotkey(service.clone())
        .context("install operator panic hotkey")
    {
        Ok(guard) => guard,
        Err(install_error) => {
            rmcp_token.cancel();
            emitter_shutdown_token.cancel();
            emitter_connection_closed_token.cancel();
            let mut no_hotkey_guard = None;
            let operator_drain = drain_stdio_operator_owners(
                &mut no_hotkey_guard,
                "stdio_operator_hotkey_install_failed",
            )
            .await;
            let (authority_safe_to_unlock, authority_drain) = inspect_authority_finalizer_drain(
                authority_finalizer_service
                    .drain_authority_finalizers()
                    .await,
                "drain authority finalizers after stdio hotkey install failure",
            );
            let emitter_report = drain_stdio_m2_owner(
                &mut m2_emitter_owner,
                "stdio_operator_hotkey_install_failed",
            )
            .await;
            let m2_emitter_safe = emitter_report.safe_to_unlock();
            let emitter_drain = emitter_report
                .verdict()
                .context("drain M2 emitter after stdio hotkey install failure");
            drop(authority_finalizer_service);
            drop(service);
            let (win_event_owners_quiescent, win_event_shutdown_history) =
                inspect_win_event_shutdown_history(
                    "inspect WinEvent shutdown history after stdio hotkey install failure",
                );
            let lifetime_lock_close = close_stdio_lifetime_locks(
                shell_job_store_lock_guard,
                single_instance_guard,
                StdioLifetimeLockReadiness {
                    authority_safe_to_unlock,
                    server_dispatch_quiescent: true,
                    m2_emitter_safe,
                    win_event_owners_quiescent,
                    hotkey_owners_quiescent: operator_drain.hotkey_owners_quiescent,
                    k2_tasks_quiescent: operator_drain.k2_tasks_quiescent,
                    desktop_worker_owners_quiescent: false,
                    retained_shutdown_task_owners_quiescent: false,
                    unresolved_shell_child_owners_quiescent: false,
                    activity_recorder_retained_owners_quiescent: false,
                },
                "stdio operator hotkey install failure",
            );
            return aggregate_stdio_shutdown_results(
                "stdio_operator_hotkey_install_failed",
                vec![
                    ("operator_hotkey_install", Err(install_error)),
                    ("authority_finalizer_drain", authority_drain),
                    ("m2_emitter_drain", emitter_drain),
                    ("win_event_shutdown_history", win_event_shutdown_history),
                    ("operator_hotkey_shutdown", operator_drain.hotkey_verdict),
                    ("operator_panic_k2_drain", operator_drain.k2_verdict),
                    ("lifetime_lock_close", lifetime_lock_close),
                ],
            )
            .map(|()| ExitCode::from(1));
        }
    };
    let (stdin, stdout) = rmcp::transport::stdio();
    let stdin = CancelOnEofRead::new(
        stdin,
        emitter_connection_closed_token.clone(),
        rmcp_token.clone(),
        "MCP_STDIO_EOF_CONNECTION_CLOSED",
        "stdio",
    );
    let mut start = Box::pin(service.serve_with_ct((stdin, stdout), rmcp_token.clone()));
    let service = tokio::select! {
        service = &mut start => match service {
            Ok(service) => service,
            Err(err) if err.to_string().contains("connection closed") => {
                tracing::info!(code = "MCP_STDIO_CLOSED_BEFORE_INIT", "stdio closed before init");
                rmcp_token.cancel();
                emitter_connection_closed_token.cancel();
                let operator_drain = drain_stdio_operator_owners(
                    &mut operator_hotkey_guard,
                    "stdio_connection_closed_before_init",
                )
                .await;
                let (authority_safe_to_unlock, authority_drain) =
                    inspect_authority_finalizer_drain(
                        authority_finalizer_service.drain_authority_finalizers().await,
                        "drain authority finalizers after stdio closed before init",
                    );
                let emitter_report = drain_stdio_m2_owner(
                    &mut m2_emitter_owner,
                    "stdio_connection_closed_before_init",
                )
                .await;
                let m2_emitter_safe = emitter_report.safe_to_unlock();
                let emitter_drain = emitter_report
                    .verdict()
                    .context("drain M2 emitter after stdio closed before init");
                drop(start);
                drop(authority_finalizer_service);
                let (win_event_owners_quiescent, win_event_shutdown_history) =
                    inspect_win_event_shutdown_history(
                        "inspect WinEvent shutdown history after stdio closed before init",
                    );
                let (lifecycle_finalization, lifecycle_finalization_begin) =
                    begin_stdio_graceful_exit_finalization("stdio_closed_before_init");
                let lifetime_lock_close = close_stdio_lifetime_locks(
                    shell_job_store_lock_guard,
                    single_instance_guard,
                    StdioLifetimeLockReadiness {
                        authority_safe_to_unlock,
                        server_dispatch_quiescent: true,
                        m2_emitter_safe,
                        win_event_owners_quiescent,
                        hotkey_owners_quiescent: operator_drain.hotkey_owners_quiescent,
                        k2_tasks_quiescent: operator_drain.k2_tasks_quiescent,
                        desktop_worker_owners_quiescent: false,
                        retained_shutdown_task_owners_quiescent: false,
                        unresolved_shell_child_owners_quiescent: false,
                        activity_recorder_retained_owners_quiescent: false,
                    },
                    "stdio connection closed before init",
                );
                aggregate_stdio_shutdown_results(
                    "stdio_connection_closed_before_init",
                    vec![
                        ("authority_finalizer_drain", authority_drain),
                        ("m2_emitter_drain", emitter_drain),
                        ("win_event_shutdown_history", win_event_shutdown_history),
                        ("operator_hotkey_shutdown", operator_drain.hotkey_verdict),
                        ("operator_panic_k2_drain", operator_drain.k2_verdict),
                        (
                            "lifecycle_finalization_begin",
                            lifecycle_finalization_begin,
                        ),
                        ("lifetime_lock_close", lifetime_lock_close),
                    ],
                )?;
                finish_stdio_graceful_exit_finalization(
                    lifecycle_finalization,
                    "stdio_closed_before_init",
                )?;
                return Ok(ExitCode::SUCCESS);
            }
            Err(err) => {
                let start_error = anyhow::Error::new(err).context("start rmcp stdio service");
                rmcp_token.cancel();
                emitter_shutdown_token.cancel();
                let operator_drain = drain_stdio_operator_owners(
                    &mut operator_hotkey_guard,
                    "stdio_start_failed_before_init",
                )
                .await;
                let (authority_safe_to_unlock, authority_drain) =
                    inspect_authority_finalizer_drain(
                        authority_finalizer_service.drain_authority_finalizers().await,
                        "drain authority finalizers after stdio startup failure",
                    );
                let emitter_report = drain_stdio_m2_owner(
                    &mut m2_emitter_owner,
                    "stdio_start_failed_before_init",
                )
                .await;
                let m2_emitter_safe = emitter_report.safe_to_unlock();
                let emitter_drain = emitter_report
                    .verdict()
                    .context("drain M2 emitter after stdio startup failure");
                drop(start);
                drop(authority_finalizer_service);
                let (win_event_owners_quiescent, win_event_shutdown_history) =
                    inspect_win_event_shutdown_history(
                        "inspect WinEvent shutdown history after stdio startup failure",
                    );
                let lifetime_lock_close = close_stdio_lifetime_locks(
                    shell_job_store_lock_guard,
                    single_instance_guard,
                    StdioLifetimeLockReadiness {
                        authority_safe_to_unlock,
                        server_dispatch_quiescent: true,
                        m2_emitter_safe,
                        win_event_owners_quiescent,
                        hotkey_owners_quiescent: operator_drain.hotkey_owners_quiescent,
                        k2_tasks_quiescent: operator_drain.k2_tasks_quiescent,
                        desktop_worker_owners_quiescent: false,
                        retained_shutdown_task_owners_quiescent: false,
                        unresolved_shell_child_owners_quiescent: false,
                        activity_recorder_retained_owners_quiescent: false,
                    },
                    "stdio startup failure",
                );
                return aggregate_stdio_shutdown_results(
                    "stdio_start_failed_before_init",
                    vec![
                        ("rmcp_start", Err(start_error)),
                        ("authority_finalizer_drain", authority_drain),
                        ("m2_emitter_drain", emitter_drain),
                        ("win_event_shutdown_history", win_event_shutdown_history),
                        ("operator_hotkey_shutdown", operator_drain.hotkey_verdict),
                        ("operator_panic_k2_drain", operator_drain.k2_verdict),
                        ("lifetime_lock_close", lifetime_lock_close),
                    ],
                )
                .map(|()| ExitCode::from(1));
            }
        },
        signal = wait_for_shutdown_signal("during startup") => {
            if let Err(error) = &signal {
                tracing::error!(
                    code = "MCP_STDIO_SHUTDOWN_SIGNAL_WAIT_FAILED",
                    phase = "during_startup",
                    error = %error,
                    "stdio shutdown-signal listener failed before initialization"
                );
            }
            rmcp_token.cancel();
            emitter_shutdown_token.cancel();
            if signal.is_ok() {
                tracing::info!(code = "MCP_SHUTDOWN_GRACEFUL", "shutdown signal received before init");
            }
            // No request may outlive the daemon-owned lock guards. Close
            // authority admission and drain the emitter before returning
            // through ordinary Rust control flow so every PID sidecar/lock and
            // telemetry guard runs its destructor.
            let operator_drain = drain_stdio_operator_owners(
                &mut operator_hotkey_guard,
                "stdio_signal_before_init",
            )
            .await;
            let (authority_safe_to_unlock, authority_drain) =
                inspect_authority_finalizer_drain(
                    authority_finalizer_service.drain_authority_finalizers().await,
                    "drain authority finalizers after stdio signal before init",
                );
            let emitter_report =
                drain_stdio_m2_owner(&mut m2_emitter_owner, "stdio_signal_before_init").await;
            let m2_emitter_safe = emitter_report.safe_to_unlock();
            let emitter_drain = emitter_report
                .verdict()
                .context("drain M2 emitter after stdio signal before init");
            drop(start);
            drop(authority_finalizer_service);
            let (win_event_owners_quiescent, win_event_shutdown_history) =
                inspect_win_event_shutdown_history(
                    "inspect WinEvent shutdown history after stdio signal before init",
                );
            let (lifecycle_finalization, lifecycle_finalization_begin) =
                begin_stdio_graceful_exit_finalization("stdio_signal_before_init");
            let lifetime_lock_close = close_stdio_lifetime_locks(
                shell_job_store_lock_guard,
                single_instance_guard,
                StdioLifetimeLockReadiness {
                    authority_safe_to_unlock,
                    server_dispatch_quiescent: true,
                    m2_emitter_safe,
                    win_event_owners_quiescent,
                    hotkey_owners_quiescent: operator_drain.hotkey_owners_quiescent,
                    k2_tasks_quiescent: operator_drain.k2_tasks_quiescent,
                    desktop_worker_owners_quiescent: false,
                    retained_shutdown_task_owners_quiescent: false,
                    unresolved_shell_child_owners_quiescent: false,
                    activity_recorder_retained_owners_quiescent: false,
                },
                "stdio signal before init",
            );
            aggregate_stdio_shutdown_results(
                "stdio_signal_before_init",
                vec![
                    (
                        "shutdown_signal",
                        signal.context("wait for stdio shutdown signal during startup"),
                    ),
                    ("authority_finalizer_drain", authority_drain),
                    ("m2_emitter_drain", emitter_drain),
                    ("win_event_shutdown_history", win_event_shutdown_history),
                    ("operator_hotkey_shutdown", operator_drain.hotkey_verdict),
                    ("operator_panic_k2_drain", operator_drain.k2_verdict),
                    (
                        "lifecycle_finalization_begin",
                        lifecycle_finalization_begin,
                    ),
                    ("lifetime_lock_close", lifetime_lock_close),
                ],
            )?;
            finish_stdio_graceful_exit_finalization(
                lifecycle_finalization,
                "stdio_signal_before_init",
            )?;
            return Ok(ExitCode::SUCCESS);
        }
    };
    drop(start);
    let shutdown = service.cancellation_token();
    // `RunningService::waiting()` takes rmcp's private JoinHandle before its
    // first await. Keep that future inside an exact outer JoinHandle: a completed
    // outer join proves the private service task was joined; dropping/aborting
    // an in-flight waiting future would detach it and is never a stop verdict.
    let mut service_task = ShutdownTaskOwner::new(
        "stdio_rmcp_service_waiting",
        tokio::spawn(service.waiting()),
    );

    let code = tokio::select! {
        wait = &mut service_task => {
            let (server_dispatch_quiescent, service_task_join) =
                inspect_stdio_service_task_join(wait, "stdio_service_completed");
            emitter_connection_closed_token.cancel();
            let operator_drain = drain_stdio_operator_owners(
                &mut operator_hotkey_guard,
                "stdio_service_completed",
            )
            .await;
            let (authority_safe_to_unlock, authority_drain) =
                inspect_authority_finalizer_drain(
                    authority_finalizer_service.drain_authority_finalizers().await,
                    "drain authority finalizers after stdio service completion",
                );
            let emitter_report =
                drain_stdio_m2_owner(&mut m2_emitter_owner, "stdio_service_completed").await;
            let m2_emitter_safe = emitter_report.safe_to_unlock();
            let emitter_drain = emitter_report
                .verdict()
                .context("drain M2 emitter after stdio service completion");
            drop(authority_finalizer_service);
            let (win_event_owners_quiescent, win_event_shutdown_history) =
                inspect_win_event_shutdown_history(
                    "inspect WinEvent shutdown history after stdio service completion",
                );
            let (lifecycle_finalization, lifecycle_finalization_begin) =
                begin_stdio_graceful_exit_finalization("stdio_service_completed");
            debug_assert!(service_task.terminal_join_observed());
            service_task.acknowledge_terminal_outcome();
            drop(service_task);
            let lifetime_lock_close = close_stdio_lifetime_locks(
                shell_job_store_lock_guard,
                single_instance_guard,
                StdioLifetimeLockReadiness {
                    authority_safe_to_unlock,
                    server_dispatch_quiescent,
                    m2_emitter_safe,
                    win_event_owners_quiescent,
                    hotkey_owners_quiescent: operator_drain.hotkey_owners_quiescent,
                    k2_tasks_quiescent: operator_drain.k2_tasks_quiescent,
                    desktop_worker_owners_quiescent: false,
                    retained_shutdown_task_owners_quiescent: false,
                    unresolved_shell_child_owners_quiescent: false,
                    activity_recorder_retained_owners_quiescent: false,
                },
                "stdio service completion",
            );
            aggregate_stdio_shutdown_results(
                "stdio_service_completed",
                vec![
                    ("rmcp_service_task_join", service_task_join),
                    ("authority_finalizer_drain", authority_drain),
                    ("m2_emitter_drain", emitter_drain),
                    ("win_event_shutdown_history", win_event_shutdown_history),
                    ("operator_hotkey_shutdown", operator_drain.hotkey_verdict),
                    ("operator_panic_k2_drain", operator_drain.k2_verdict),
                    (
                        "lifecycle_finalization_begin",
                        lifecycle_finalization_begin,
                    ),
                    ("lifetime_lock_close", lifetime_lock_close),
                ],
            )?;
            finish_stdio_graceful_exit_finalization(
                lifecycle_finalization,
                "stdio_service_completed",
            )?;
            ExitCode::SUCCESS
        }
        signal = wait_for_shutdown_signal("after init") => {
            if let Err(error) = &signal {
                tracing::error!(
                    code = "MCP_STDIO_SHUTDOWN_SIGNAL_WAIT_FAILED",
                    phase = "after_init",
                    error = %error,
                    "stdio shutdown-signal listener failed after initialization"
                );
            } else {
                tracing::info!(code = "MCP_SHUTDOWN_GRACEFUL", "shutdown signal received");
            }
            emitter_shutdown_token.cancel();
            shutdown.cancel();
            let (server_dispatch_quiescent, service_task_join) =
                stop_stdio_service_task_after_cancel(
                    &mut service_task,
                    "stdio_signal_after_init",
            )
            .await;
            let operator_drain = drain_stdio_operator_owners(
                &mut operator_hotkey_guard,
                "stdio_signal_after_init",
            )
            .await;
            let (authority_safe_to_unlock, authority_drain) =
                inspect_authority_finalizer_drain(
                    authority_finalizer_service.drain_authority_finalizers().await,
                    "drain authority finalizers after stdio signal after init",
                );
            let emitter_report =
                drain_stdio_m2_owner(&mut m2_emitter_owner, "stdio_signal_after_init").await;
            let m2_emitter_safe = emitter_report.safe_to_unlock();
            let emitter_drain = emitter_report
                .verdict()
                .context("drain M2 emitter after stdio signal after init");
            drop(authority_finalizer_service);
            let (win_event_owners_quiescent, win_event_shutdown_history) =
                inspect_win_event_shutdown_history(
                    "inspect WinEvent shutdown history after stdio signal after init",
                );
            let (lifecycle_finalization, lifecycle_finalization_begin) =
                begin_stdio_graceful_exit_finalization("stdio_signal_after_init");
            if service_task.terminal_join_observed() {
                service_task.acknowledge_terminal_outcome();
            }
            drop(service_task);
            let lifetime_lock_close = close_stdio_lifetime_locks(
                shell_job_store_lock_guard,
                single_instance_guard,
                StdioLifetimeLockReadiness {
                    authority_safe_to_unlock,
                    server_dispatch_quiescent,
                    m2_emitter_safe,
                    win_event_owners_quiescent,
                    hotkey_owners_quiescent: operator_drain.hotkey_owners_quiescent,
                    k2_tasks_quiescent: operator_drain.k2_tasks_quiescent,
                    desktop_worker_owners_quiescent: false,
                    retained_shutdown_task_owners_quiescent: false,
                    unresolved_shell_child_owners_quiescent: false,
                    activity_recorder_retained_owners_quiescent: false,
                },
                "stdio signal after init",
            );
            aggregate_stdio_shutdown_results(
                "stdio_signal_after_init",
                vec![
                    (
                        "shutdown_signal",
                        signal.context("wait for stdio shutdown signal after init"),
                    ),
                    ("rmcp_service_task_join", service_task_join),
                    ("authority_finalizer_drain", authority_drain),
                    ("m2_emitter_drain", emitter_drain),
                    ("win_event_shutdown_history", win_event_shutdown_history),
                    ("operator_hotkey_shutdown", operator_drain.hotkey_verdict),
                    ("operator_panic_k2_drain", operator_drain.k2_verdict),
                    (
                        "lifecycle_finalization_begin",
                        lifecycle_finalization_begin,
                    ),
                    ("lifetime_lock_close", lifetime_lock_close),
                ],
            )?;
            finish_stdio_graceful_exit_finalization(
                lifecycle_finalization,
                "stdio_signal_after_init",
            )?;
            return Ok(ExitCode::SUCCESS);
        }
    };

    Ok(code)
}

#[cfg(not(windows))]
async fn wait_for_shutdown_signal(phase: &'static str) -> anyhow::Result<()> {
    tokio::signal::ctrl_c()
        .await
        .with_context(|| format!("wait for ctrl-c {phase}"))
}

#[cfg(windows)]
async fn wait_for_shutdown_signal(phase: &'static str) -> anyhow::Result<()> {
    // Ctrl+Break can be delivered to one CREATE_NEW_PROCESS_GROUP without
    // broadcasting Ctrl+C into the operator's terminal. Treat both Windows
    // console shutdown controls as the same graceful lifecycle boundary.
    let mut ctrl_c = tokio::signal::windows::ctrl_c()
        .with_context(|| format!("register ctrl-c handler {phase}"))?;
    let mut ctrl_break = tokio::signal::windows::ctrl_break()
        .with_context(|| format!("register ctrl-break handler {phase}"))?;
    tokio::select! {
        signal = ctrl_c.recv() => signal
            .ok_or_else(|| anyhow::anyhow!("ctrl-c handler closed while waiting {phase}")),
        signal = ctrl_break.recv() => signal
            .ok_or_else(|| anyhow::anyhow!("ctrl-break handler closed while waiting {phase}")),
    }
}

#[cfg(test)]
mod stdio_shutdown_tests {
    use super::*;

    fn clean_win_event_shutdown_record(
        owner_id: u64,
    ) -> synapse_a11y::WinEventSubscriptionShutdownRecord {
        synapse_a11y::WinEventSubscriptionShutdownRecord {
            owner_id,
            report: synapse_a11y::WinEventSubscriptionShutdownReport {
                reason: "synthetic_stdio_history",
                thread_id: owner_id as u32,
                hook_count: 2,
                stop_requested: true,
                stop_wake_sent: true,
                sender_disconnected: true,
                subscription_slot_released: true,
                thread_owner_present: true,
                thread_terminal: true,
                thread_joined: true,
                thread_exit_report_received: true,
                unregister_attempted: 2,
                unregister_succeeded: 2,
                unregister_failed_event_ids: Vec::new(),
                exact_owner_retained: false,
                failures: Vec::new(),
            },
        }
    }

    #[test]
    fn stdio_shutdown_aggregation_retains_every_failed_phase() {
        let error = aggregate_stdio_shutdown_results(
            "synthetic_shutdown",
            vec![
                (
                    "rmcp_service_task_join",
                    Err(anyhow::anyhow!("dispatch stuck")),
                ),
                (
                    "authority_finalizer_drain",
                    Err(anyhow::anyhow!("admission poisoned")),
                ),
                ("m2_emitter_drain", Err(anyhow::anyhow!("emitter timeout"))),
            ],
        )
        .expect_err("any failed shutdown phase must reject a graceful verdict");
        let detail = format!("{error:#}");

        assert!(detail.contains("rmcp_service_task_join: dispatch stuck"));
        assert!(detail.contains("authority_finalizer_drain: admission poisoned"));
        assert!(detail.contains("m2_emitter_drain: emitter timeout"));
    }

    #[tokio::test]
    async fn owned_service_join_completion_is_quiescent() {
        let service_task = tokio::spawn(async {
            tokio::task::yield_now().await;
            Ok(rmcp::service::QuitReason::Closed)
        });
        let (quiescent, result) =
            inspect_stdio_service_task_join(service_task.await, "owned_join_completed");

        assert!(
            quiescent,
            "completed outer join proves the inner join result"
        );
        result.expect("clean owned join should pass the shutdown phase");
    }

    #[tokio::test]
    async fn cancelled_outer_service_join_owner_is_not_quiescent() {
        let service_task = tokio::spawn(async {
            std::future::pending::<Result<rmcp::service::QuitReason, tokio::task::JoinError>>()
                .await
        });
        service_task.abort();
        let (quiescent, result) =
            inspect_stdio_service_task_join(service_task.await, "outer_owner_cancelled");
        let detail = format!(
            "{:#}",
            result.expect_err("cancelled outer owner must fail the shutdown phase")
        );

        assert!(
            !quiescent,
            "an aborted waiting() owner can detach rmcp's private task"
        );
        assert!(detail.contains("before inner join readback"));
    }

    #[test]
    fn stdio_lifetime_unlock_requires_every_owner_set_quiescent() {
        let safe = StdioLifetimeLockReadiness {
            authority_safe_to_unlock: true,
            server_dispatch_quiescent: true,
            m2_emitter_safe: true,
            win_event_owners_quiescent: true,
            hotkey_owners_quiescent: true,
            k2_tasks_quiescent: true,
            desktop_worker_owners_quiescent: true,
            retained_shutdown_task_owners_quiescent: true,
            unresolved_shell_child_owners_quiescent: true,
            activity_recorder_retained_owners_quiescent: true,
        };
        assert!(stdio_lifetime_locks_safe_to_close(safe));

        let mut readiness = safe;
        readiness.authority_safe_to_unlock = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.server_dispatch_quiescent = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.m2_emitter_safe = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.win_event_owners_quiescent = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.hotkey_owners_quiescent = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.k2_tasks_quiescent = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.desktop_worker_owners_quiescent = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.retained_shutdown_task_owners_quiescent = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.unresolved_shell_child_owners_quiescent = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));

        let mut readiness = safe;
        readiness.activity_recorder_retained_owners_quiescent = false;
        assert!(!stdio_lifetime_locks_safe_to_close(readiness));
    }

    #[test]
    fn win_event_history_rejects_any_older_failed_owner_and_retained_owner() {
        let clean = clean_win_event_shutdown_record(22);
        let clean_readback =
            win_event_shutdown_history_readback_from(std::slice::from_ref(&clean), 0);
        assert!(clean_readback.owners_quiescent());
        clean_readback.verdict().expect(
            "a nonzero installed-hook count is safe when every hook was physically unregistered",
        );

        let mut failed = clean_win_event_shutdown_record(21);
        failed.report.stop_wake_sent = false;
        let history = [clean, failed];
        let failed_readback = win_event_shutdown_history_readback_from(&history, 0);
        assert!(!failed_readback.owners_quiescent());
        let detail = failed_readback
            .verdict()
            .expect_err("an older immutable failed owner must remain fail-closed")
            .to_string();
        assert!(detail.contains("owner_id=21"));

        let retained_readback = win_event_shutdown_history_readback_from(&history[..1], 1);
        assert!(!retained_readback.owners_quiescent());
        assert!(retained_readback.verdict().is_err());
    }
}

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, bail};
use fs2::FileExt as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use synapse_core::SubsystemHealth;

const SCHEMA_VERSION: u32 = 1;
/// Schema version for records in `daemon-tool-events.jsonl`.
///
/// Version 2 added the typed `terminal_error` projection but still serialized
/// the raw RMCP error beside it. Version 3 persists the projection only, so a
/// rejected argument echoed by a deserializer cannot enter the lifecycle
/// ledger. The outer run/exit records keep their independent v1 schema:
/// changing one ledger must not silently relabel every other lifecycle record.
const TOOL_EVENT_SCHEMA_VERSION: u32 = 3;
const TERMINAL_ERROR_PROJECTION_SCHEMA_VERSION: u32 = 1;
const MAX_CANONICAL_ERROR_CODE_BYTES: usize = 128;
const TOOL_SURFACE_SHA256_BYTES: usize = 71;
const MAX_TOOL_USAGE_DECODE_ERRORS: usize = 32;
const MAX_ERROR_CODES_PER_AGGREGATE: usize = 32;

/// Boot-time verdict on how the previous run of this vault ended (#2083).
///
/// The previous daemon wrote `ended_at_unix_ms` + `ended_reason` into
/// `daemon-run-current.json` as the last durable act of its graceful exit
/// (`record_exit_for_state_locked`). Their presence is therefore the
/// clean-shutdown discriminator, exactly the role `pg_control`'s
/// `DB_SHUTDOWNED` plays for PostgreSQL and `.kafka_cleanshutdown` plays for
/// Kafka -- and the same role
/// [`crate::m4::ShellJobSupervisorMarker::clean_shutdown_at`] plays for the
/// shell-job store.
const PREVIOUS_SHUTDOWN_CLEAN: &str = "clean";
/// A previous run record exists but never recorded an end: the process died
/// without reaching its graceful-exit finalization.
const PREVIOUS_SHUTDOWN_DIRTY: &str = "dirty";
/// No previous run record at all -- a first boot on this vault.
const PREVIOUS_SHUTDOWN_NONE: &str = "none";
/// The previous run declared a commanded graceful shutdown and began its close,
/// but died before finalizing the exit record (#2100).
///
/// # Why this is its own verdict and not `dirty`
///
/// `dirty` used to cover two materially different events. One is "the process
/// died while running": nothing was flushed on purpose, the WAL tail is whatever
/// the last group commit left, and an operator should look for a crash. The
/// other is "an operator or a deploy commanded a shutdown, the daemon flushed
/// durably, and it was then killed part-way through the close" — which is
/// exactly what #2100 reports, three times over, and which reads as *identical*
/// to a crash at the next boot.
///
/// The distinction is only knowable if the intent is recorded **before** the
/// close begins, so [`record_exit_intent`] writes an `ending_*` marker first and
/// [`record_exit_for_state_locked`] finalizes it into `ended_*`. Marker without
/// finalization is this verdict. That is the same two-phase shape PostgreSQL's
/// `pg_control` uses between `DB_SHUTDOWNING` and `DB_SHUTDOWNED`: the
/// in-progress state is a distinct recorded value precisely so a crash during
/// shutdown is distinguishable from a crash during operation.
///
/// It is deliberately NOT reported as clean. Nothing about it proves the close
/// finished; it proves only that the daemon was trying to. The evidence rides
/// with it (`previous_ending_reason`, `previous_ending_phase`,
/// `previous_ending_at_unix_ms`) so an operator can see how far it got.
const PREVIOUS_SHUTDOWN_INTERRUPTED_GRACEFUL: &str = "interrupted_graceful";

/// `ended_reason` values that name a shutdown which actually *finished* (#2131).
///
/// # Why the verdict cannot be `ended_at.is_some()`
///
/// It used to be. The exit record is written by one funnel
/// ([`record_exit_for_state_locked`]) that stamps `ended_at_unix_ms` for every
/// caller — including the callers whose entire job is to kill a daemon that did
/// not finish. The HTTP shutdown watchdog is exactly that: when it expires it
/// records `ended_reason=http_shutdown_watchdog_expired` and then calls
/// `std::process::exit(1)` in the middle of the close. The next boot read
/// `ended_at` and reported
/// `previous_shutdown=clean previous_ended_reason="http_shutdown_watchdog_expired"`
/// on production (#2131) — a rollup verdict contradicted by the very field
/// beside it.
///
/// So `clean` is now reserved for a *finalized close whose terminal cause names
/// a completed drain*: the `graceful` funnel and the #2090 OS-shutdown triggers.
/// Everything else — watchdog kills, panics, startup aborts, top-level errors —
/// is a forced exit, and a forced exit is `interrupted_graceful` when the
/// phase-one marker proves a close was underway and `dirty` when it does not.
///
/// This is an allowlist, not a denylist, precisely so a cause added later
/// defaults to the *unflattering* reading instead of silently inheriting
/// `clean`.
const GRACEFUL_EXIT_CAUSES: &[&str] = &[
    // `record_graceful_exit_after_lifetime_lock_close`: the full drain reached
    // its end, after the vault close and the lifetime-lock release.
    "graceful",
    // #2090 `record_os_shutdown_exit`: a bounded but complete OS-triggered
    // drain. `os_shutdown.rs` downgrades these to a `*_vault_not_closed`
    // variant when the vault did not actually reach a close, so reaching this
    // list means the vault closed.
    "os_console_close",
    "os_window_close",
    "os_logoff",
    "os_shutdown",
    "os_session_end",
];

/// `ended_reason` values known to name a forced or aborted exit.
///
/// Membership here changes nothing about the verdict — anything outside
/// [`GRACEFUL_EXIT_CAUSES`] is treated as forced either way. It exists so an
/// *unrecognized* cause can be reported as a contradiction rather than quietly
/// classified, which is the difference between "this build knows this is a kill"
/// and "this build has never heard of this cause".
const KNOWN_FORCED_EXIT_CAUSES: &[&str] = &[
    "http_shutdown_watchdog_expired",
    "http_shutdown_watchdog_spawn_failed",
    "panic",
    "top_level_error",
    "stdio_storage_or_calyx_open_or_maintenance_start_failed",
];

/// How a previous run's terminal `ended_reason` classifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitCauseClass {
    /// A drain that reached its declared end.
    Graceful,
    /// A kill, panic, or abort. The exit record exists because something wrote
    /// it on the way out, not because the shutdown succeeded.
    Forced,
    /// Neither list knows this cause. Treated as [`Self::Forced`] and reported
    /// as a contradiction: an unknown terminal cause must never be read
    /// optimistically.
    Unrecognized,
}

fn classify_exit_cause(cause: &str) -> ExitCauseClass {
    if GRACEFUL_EXIT_CAUSES.contains(&cause) {
        return ExitCauseClass::Graceful;
    }
    if KNOWN_FORCED_EXIT_CAUSES.contains(&cause)
        // `record_startup_exit` causes are named by their caller and grow with
        // the startup path; every one of them is an abort before the daemon
        // ever served, so the family is classified by prefix rather than by an
        // enumeration that would silently go stale.
        || cause.starts_with("startup_")
        // #2131: the #2090 OS drain's honest downgrade when its bounded budget
        // did not get the vault closed.
        || cause.ends_with("_vault_not_closed")
    {
        return ExitCauseClass::Forced;
    }
    ExitCauseClass::Unrecognized
}

/// The boot verdict on the previous run plus the evidence it was derived from.
struct PreviousShutdownVerdict {
    verdict: &'static str,
    /// One line naming the basis and every contradiction found, so the rollup
    /// can never be the only thing an operator has to trust (#2131).
    detail: String,
}

/// Derives the previous run's shutdown verdict from *all* the evidence its
/// record carries, not from `ended_at_unix_ms` alone (#2131).
///
/// # The law
///
/// * `clean` — the exit record was finalized (`ended_at_unix_ms`) **and** its
///   `ended_reason` names a completed drain **and** nothing in the record
///   contradicts that.
/// * `interrupted_graceful` — a close was commanded (the phase-one
///   `ending_at_unix_ms` marker is present) but the record does not prove it
///   finished: either no finalization at all, or a finalization whose cause is a
///   forced exit (the watchdog case), or a finalization contradicted by its own
///   fields. `ending_phase` names the phase that was in progress.
/// * `dirty` — no phase-one marker: nothing proves a shutdown was ever
///   commanded, so this is indistinguishable from a crash while serving.
///
/// # Fail closed
///
/// Contradictions never resolve in favour of the flattering reading. A record
/// that says `ended_at` without `ended_reason`, or whose `ended_at` precedes the
/// `ending_at` it supposedly supersedes, or whose cause this build cannot
/// classify, loses `clean` and reports *why* in [`PreviousShutdownVerdict::detail`].
fn classify_previous_shutdown(previous: &RunRecord) -> PreviousShutdownVerdict {
    let ended_reason = previous
        .ended_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    let cause_class = ended_reason.map(classify_exit_cause);
    let marker_present = previous.ending_at_unix_ms.is_some();

    let mut contradictions: Vec<String> = Vec::new();
    match (previous.ended_at_unix_ms, ended_reason) {
        (Some(ended_at), None) => contradictions.push(format!(
            "ended_at_unix_ms={ended_at} was finalized with no ended_reason naming the cause"
        )),
        (None, Some(reason)) => contradictions.push(format!(
            "ended_reason={reason} was recorded with no ended_at_unix_ms finalizing it"
        )),
        _ => {}
    }
    if cause_class == Some(ExitCauseClass::Unrecognized)
        && let Some(reason) = ended_reason
    {
        contradictions.push(format!(
            "ended_reason={reason} is not a terminal cause this build can classify"
        ));
    }
    if let (Some(ended_at), Some(ending_at)) =
        (previous.ended_at_unix_ms, previous.ending_at_unix_ms)
        && ended_at < ending_at
    {
        contradictions.push(format!(
            "ended_at_unix_ms={ended_at} precedes the ending_at_unix_ms={ending_at} it supersedes"
        ));
    }

    let finalized_graceful =
        previous.ended_at_unix_ms.is_some() && cause_class == Some(ExitCauseClass::Graceful);
    let verdict = if finalized_graceful && contradictions.is_empty() {
        PREVIOUS_SHUTDOWN_CLEAN
    } else if marker_present {
        PREVIOUS_SHUTDOWN_INTERRUPTED_GRACEFUL
    } else {
        PREVIOUS_SHUTDOWN_DIRTY
    };

    let basis = match (verdict, cause_class) {
        (PREVIOUS_SHUTDOWN_CLEAN, _) => {
            "the exit record was finalized and its cause names a completed drain"
        }
        (PREVIOUS_SHUTDOWN_INTERRUPTED_GRACEFUL, Some(ExitCauseClass::Graceful)) => {
            "a close was commanded and its finalization is contradicted by its own fields"
        }
        (PREVIOUS_SHUTDOWN_INTERRUPTED_GRACEFUL, Some(_)) => {
            "a close was commanded and then ended by a forced/abnormal cause before it finished"
        }
        (PREVIOUS_SHUTDOWN_INTERRUPTED_GRACEFUL, None) => {
            "a close was commanded and no exit record finalized it"
        }
        (_, Some(_)) => "no close was ever commanded and the run ended by a forced/abnormal cause",
        (_, None) => "no close was ever commanded and no exit record finalized the run",
    };
    let detail = format!(
        "basis={basis}; ended_at_unix_ms={} ended_reason={} ended_cause_class={} ending_marker={} ending_reason={} ending_phase={} contradictions={}",
        previous
            .ended_at_unix_ms
            .map_or_else(|| "none".to_owned(), |value| value.to_string()),
        ended_reason.unwrap_or("none"),
        cause_class.map_or("none", |class| match class {
            ExitCauseClass::Graceful => "graceful",
            ExitCauseClass::Forced => "forced",
            ExitCauseClass::Unrecognized => "unrecognized",
        }),
        if marker_present { "present" } else { "absent" },
        previous.ending_reason.as_deref().unwrap_or("none"),
        previous.ending_phase.as_deref().unwrap_or("none"),
        if contradictions.is_empty() {
            "none".to_owned()
        } else {
            contradictions.join(" | ")
        }
    );
    PreviousShutdownVerdict { verdict, detail }
}
// These files live inside the vault directory, so their names are owned by
// `synapse_calyx::vault_runtime`: a vault backup must exclude exactly this set
// by name (they are the running process's state, never vault data) and the two
// definitions must not be able to drift apart.
const RUN_CURRENT_FILE: &str = synapse_calyx::vault_runtime::DAEMON_RUN_CURRENT_FILE;
const TOOL_LAST_FILE: &str = synapse_calyx::vault_runtime::DAEMON_TOOL_LAST_FILE;
const TOOL_EVENTS_FILE: &str = synapse_calyx::vault_runtime::DAEMON_TOOL_EVENTS_FILE;
const EXIT_EVENTS_FILE: &str = synapse_calyx::vault_runtime::DAEMON_EXIT_EVENTS_FILE;
const LIFECYCLE_LOCK_FILE: &str = synapse_calyx::vault_runtime::DAEMON_LIFECYCLE_LOCK_FILE;

/// Maximum size in bytes the active daemon tool-event ledger
/// (`daemon-tool-events.jsonl`) may reach before it is rotated to a numbered
/// segment. Set to 8 MiB: small enough that a single segment opens and scans
/// quickly, large enough that rotation stays rare on the hot append path.
///
/// Before this cap existed the ledger grew unbounded (~141 MiB in five weeks);
/// segmented rotation plus [`MAX_LEDGER_SEGMENTS`] now bounds total disk usage.
const MAX_LEDGER_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum number of rotated tool-event segments retained on disk
/// (`daemon-tool-events.jsonl.1` .. `.5`, newest suffix `.1`). Older segments
/// are pruned during rotation, so total retained ledger bytes are bounded by
/// roughly `MAX_LEDGER_SEGMENT_BYTES * (MAX_LEDGER_SEGMENTS + 1)`.
const MAX_LEDGER_SEGMENTS: usize = synapse_calyx::vault_runtime::MAX_LIFECYCLE_LEDGER_SEGMENTS;
const MAX_RETAINED_LEDGER_FILES: usize = MAX_LEDGER_SEGMENTS + 1;

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
    /// When a commanded shutdown *began*, written before the close runs (#2100).
    ///
    /// This is phase one of the two-phase exit record. Its presence without
    /// `ended_at_unix_ms` is what makes an escalated kill mid-close
    /// distinguishable from a crash-while-running; see
    /// [`PREVIOUS_SHUTDOWN_INTERRUPTED_GRACEFUL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ending_at_unix_ms: Option<u64>,
    /// Why the shutdown was commanded (`http_endpoint`, `os_shutdown`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ending_reason: Option<String>,
    /// The drain phase in progress when the marker was written, so a boot can
    /// say *how far* the interrupted close got rather than only that it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ending_phase: Option<String>,
    ended_at_unix_ms: Option<u64>,
    ended_reason: Option<String>,
    /// How the *previous* run on this vault ended, decided at boot (#2083).
    ///
    /// [`PREVIOUS_SHUTDOWN_CLEAN`] / [`PREVIOUS_SHUTDOWN_DIRTY`] /
    /// [`PREVIOUS_SHUTDOWN_NONE`]. `configure` already appended a
    /// `previous_run_unclean` exit event for the dirty case, but that evidence
    /// only existed inside the append-only exit ledger: nothing in the live
    /// `daemon-run-current.json`, in the boot log, or in `/health` said whether
    /// this daemon inherited a clean stop or a crash. An operator stopping the
    /// daemon (`synapse-setup.ps1 -Stop`) and restarting it (`-Start`) needs
    /// that verdict at the *next* boot to prove the stop was clean, so it is
    /// carried on the run record itself.
    ///
    /// `#[serde(default)]` on all four fields: run records written before this
    /// change do not carry them and must still deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_shutdown: Option<String>,
    /// The evidence the verdict above was derived from, and every contradiction
    /// found while deriving it (#2131). A rollup that cannot be audited against
    /// the record it summarizes is how `clean` came to sit beside
    /// `ended_reason="http_shutdown_watchdog_expired"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_shutdown_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_ended_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_ended_at_unix_ms: Option<u64>,
    /// The previous run's phase-one shutdown marker, carried onto this run's
    /// record so `interrupted_graceful` arrives with its evidence (#2100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_ending_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_ending_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_ending_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ToolCallStart {
    pub tool: String,
    pub operation: Option<String>,
    pub route_id: Option<String>,
    pub profile: Option<String>,
    pub tool_surface_sha256: Option<String>,
    pub tool_profile_read_error: Option<Value>,
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

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolUsageAggregate {
    pub tool: String,
    pub operation: Option<String>,
    pub route_id: Option<String>,
    pub profile: Option<String>,
    pub tool_surface_sha256: Option<String>,
    pub calls_total: u64,
    pub ok_total: u64,
    pub error_total: u64,
    pub panic_total: u64,
    pub total_duration_ms: u64,
    pub max_duration_ms: u64,
    pub latest_status: String,
    pub latest_error_code: Option<String>,
    pub distinct_error_code_count: usize,
    pub error_code_counts_truncated: bool,
    pub error_code_counts: Vec<ToolUsageErrorCodeCount>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolUsageErrorCodeCount {
    pub error_code: String,
    pub count: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolUsageDecodeError {
    pub segment: String,
    pub line: usize,
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolUsageTelemetry {
    pub source_of_truth: String,
    pub max_rows: usize,
    pub rows_scanned: usize,
    pub segment_count: usize,
    pub terminal_error_projection_schema_version: u32,
    pub canonical_terminal_error_rows: usize,
    pub compatibility_terminal_error_rows: usize,
    pub decode_error_total: usize,
    pub decode_errors_truncated: bool,
    pub decode_errors: Vec<ToolUsageDecodeError>,
    pub aggregate_count: usize,
    pub aggregates_truncated: bool,
    pub max_error_codes_per_aggregate: usize,
    pub aggregates: Vec<ToolUsageAggregate>,
    pub read_error: Option<String>,
}

type ToolUsageKey = (String, Option<String>, Option<String>, Option<String>);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalErrorProjection {
    schema_version: u32,
    facade: String,
    operation: Option<String>,
    route_id: Option<String>,
    status: String,
    error_code: String,
    duration_ms: u64,
    profile: Option<String>,
    tool_surface_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalErrorDecodeSource {
    Canonical,
    Compatibility,
}

#[derive(Clone, Debug)]
struct ToolUsageAccumulator {
    aggregate: ToolUsageAggregate,
    error_code_counts: BTreeMap<String, u64>,
}

#[derive(Clone, Debug)]
struct ToolUsageDecodeFailure {
    code: &'static str,
    detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ToolEvent {
    schema_version: u32,
    run_id: String,
    pid: u32,
    seq: u64,
    event_kind: String,
    tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_surface_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_profile_read_error: Option<Value>,
    status: String,
    started_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_context: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_context_read_error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    foreground: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    foreground_read_error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_target: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_target_read_error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_target: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_error: Option<TerminalErrorProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    panic: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FinishedToolCallReadback {
    pub schema_version: u32,
    pub run_id: String,
    pub pid: u32,
    pub seq: u64,
    pub event_kind: String,
    pub tool: String,
    pub operation: Option<String>,
    pub route_id: Option<String>,
    pub profile: Option<String>,
    pub tool_surface_sha256: Option<String>,
    pub status: String,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: u64,
    pub duration_ms: u64,
    pub mcp_session_id: Option<String>,
    pub effective_target: Option<Value>,
    pub error: Option<Value>,
    pub panic: Option<Value>,
}

impl TryFrom<ToolEvent> for FinishedToolCallReadback {
    type Error = anyhow::Error;

    fn try_from(event: ToolEvent) -> anyhow::Result<Self> {
        let finished_at_unix_ms = event.finished_at_unix_ms.ok_or_else(|| {
            anyhow::anyhow!(
                "daemon lifecycle terminal event {} is missing finished_at_unix_ms",
                event.seq
            )
        })?;
        let duration_ms = event.duration_ms.ok_or_else(|| {
            anyhow::anyhow!(
                "daemon lifecycle terminal event {} is missing duration_ms",
                event.seq
            )
        })?;
        Ok(Self {
            schema_version: event.schema_version,
            run_id: event.run_id,
            pid: event.pid,
            seq: event.seq,
            event_kind: event.event_kind,
            tool: event.tool,
            operation: event.operation,
            route_id: event.route_id,
            profile: event.profile,
            tool_surface_sha256: event.tool_surface_sha256,
            status: event.status,
            started_at_unix_ms: event.started_at_unix_ms,
            finished_at_unix_ms,
            duration_ms,
            mcp_session_id: event.mcp_session_id,
            effective_target: event.effective_target,
            error: event.error,
            panic: event.panic,
        })
    }
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

#[derive(Debug)]
struct DaemonLifecycleState {
    run: RunRecord,
    paths: DaemonLifecyclePaths,
    in_flight: BTreeMap<u64, ToolEvent>,
    seq: u64,
    last_error: Option<String>,
    /// The most recent tool event this run appended to the ledger.
    ///
    /// This used to be a second durable file (`daemon-tool-last.json`) written
    /// with a temp-create + fsync + rename on **every** tool event, beside the
    /// append that had already fsync'd the identical record into the ledger.
    /// That is a dual write of one fact: the two files can disagree across a
    /// crash between the append and the rename, and the ledger's own last line
    /// is the same record and is never staler. Measured on the deployment host
    /// the atomic replace cost 1.295 ms per event, twice per tool call, for
    /// information already on disk (#1936).
    ///
    /// The live readers are all in-process (diagnostic events, exit
    /// finalization), so they read this field. The one cold reader that
    /// genuinely survives a crash -- [`configure`] reconstructing the previous
    /// run -- reads the ledger tail via [`last_tool_event_from_ledger`], which
    /// is strictly more current than the retired pointer file could be.
    last_tool_event: Option<ToolEvent>,
    /// Append state for the active `daemon-tool-events.jsonl` segment: the
    /// in-memory byte counter (so the hot path never stats the file) plus the
    /// persistent append handle.
    tool_events: LedgerAppender,
    /// Append state for the active `daemon-exit.jsonl` segment. Exit events
    /// share the same bounded JSONL ledger implementation as tool events so
    /// daemon lifecycle diagnostics cannot grow without retention.
    exit_events: LedgerAppender,
    /// Size cap the active tool-event segment may reach before rotation. Seeded
    /// from [`MAX_LEDGER_SEGMENT_BYTES`]; overridable only in tests via
    /// [`set_max_segment_bytes_for_test`] to force rotation without writing MiB.
    max_segment_bytes: u64,
}

/// Append state for one bounded JSONL lifecycle ledger.
///
/// Holds the active segment's byte counter and a persistent append handle. The
/// handle is kept open across appends because the daemon appends twice per MCP
/// tool call and, measured on the deployment host, `open + append + flush +
/// fsync + close` costs 0.684 ms per record against 0.340 ms for `append +
/// fsync` on a handle that is already open (#1936). The `create_dir_all` the
/// old path ran before every open cost a further 0.083 ms and is now paid only
/// when the handle is actually opened.
///
/// The fsync itself is deliberately kept per record. It is what makes a
/// `started` record with no matching `finished` record trustworthy evidence
/// that the daemon died mid-call, which is the whole point of the ledger for a
/// process that drives real input into the operating system.
#[derive(Debug, Default)]
struct LedgerAppender {
    active_bytes: u64,
    handle: Option<File>,
}

impl LedgerAppender {
    fn new(active_bytes: u64) -> Self {
        Self {
            active_bytes,
            handle: None,
        }
    }

    /// Release the append handle. Called before rotation so Windows never has
    /// to rename a file this process still holds open, and so the next append
    /// reopens against whatever path rotation left in place.
    fn close(&mut self) {
        self.handle = None;
    }

    /// Borrow the append handle, opening it (and its parent directory) first if
    /// this is the first append since startup or since a rotation.
    fn handle(&mut self, path: &Path) -> anyhow::Result<&mut File> {
        match self.handle {
            Some(ref mut file) => Ok(file),
            None => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                let file = open_ledger_append(path)
                    .with_context(|| format!("open append {}", path.display()))?;
                Ok(self.handle.insert(file))
            }
        }
    }
}

/// Open a lifecycle ledger for appending.
///
/// On Windows the share mode must include `FILE_SHARE_DELETE` in addition to
/// read and write: without it, holding this handle open would make
/// [`rotate_ledger`]'s rename of the active segment fail with a sharing
/// violation, and would block any external reader that opens the ledger for
/// forensic inspection while the daemon is live. This is the same sharing
/// contract the durable shell-job status writes already depend on (#1568).
fn open_ledger_append(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        OpenOptions::new()
            .create(true)
            .append(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        OpenOptions::new().create(true).append(true).open(path)
    }
}

#[derive(Clone, Debug)]
struct LedgerSource {
    path: PathBuf,
    suffix: Option<usize>,
}

#[derive(Clone, Debug)]
struct StagedLedgerSegment {
    path: PathBuf,
    bytes: u64,
    records: u64,
    oversized_records: u64,
}

#[derive(Debug)]
struct LedgerRewrite {
    segments: Vec<StagedLedgerSegment>,
    source_bytes: u64,
    source_records: u64,
    missing_newline_repairs: u64,
}

#[derive(Debug)]
pub(crate) struct ToolCallGuard {
    run_id: String,
    seq: Option<u64>,
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

/// Holds the lifecycle transaction and in-process lifecycle state across the
/// exact daemon-lifetime-lock release boundary. A successor may acquire the
/// daemon lock once the caller closes it, but its lifecycle `configure` call
/// cannot inspect `daemon-run-current.json` until this guard has durably
/// published the predecessor's graceful exit and released the transaction.
pub(crate) struct GracefulExitFinalizationGuard {
    // Keep the ledger field first: on unwind its Drop unlocks the cross-process
    // transaction before the state mutex is released to another local writer.
    ledger: LifecycleLedgerLock,
    state: MutexGuard<'static, Option<DaemonLifecycleState>>,
}

struct LifecycleLedgerLock {
    file: Option<File>,
    path: PathBuf,
    operation: &'static str,
}

impl LifecycleLedgerLock {
    fn acquire(db_path: &Path, operation: &'static str) -> anyhow::Result<Self> {
        fs::create_dir_all(db_path)
            .with_context(|| format!("create lifecycle lock directory {}", db_path.display()))?;
        let path = db_path.join(LIFECYCLE_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open lifecycle transaction lock {}", path.display()))?;
        file.lock_exclusive().with_context(|| {
            format!(
                "lock lifecycle transaction {} for {operation}",
                path.display()
            )
        })?;
        Ok(Self {
            file: Some(file),
            path,
            operation,
        })
    }

    fn unlock_checked(&mut self) -> anyhow::Result<()> {
        let Some(file) = self.file.as_ref() else {
            return Ok(());
        };
        fs2::FileExt::unlock(file).with_context(|| {
            format!(
                "unlock lifecycle transaction {} after {}",
                self.path.display(),
                self.operation
            )
        })?;
        self.file = None;
        Ok(())
    }
}

impl Drop for LifecycleLedgerLock {
    fn drop(&mut self) {
        if self.file.is_none() {
            return;
        }
        if let Err(error) = self.unlock_checked() {
            tracing::error!(
                code = "MCP_DAEMON_LIFECYCLE_LOCK_DROP_FAILED",
                operation = self.operation,
                lock_path = %self.path.display(),
                error = %error,
                "failed to unlock daemon lifecycle transaction during Drop; closing the owned file handle as the final OS-lock backstop"
            );
            eprintln!(
                "synapse-mcp daemon lifecycle lock cleanup failed: operation={} path={} error={error:#}",
                self.operation,
                self.path.display()
            );
        }
    }
}

fn combine_lifecycle_action_and_unlock<T>(
    operation: &'static str,
    action_result: anyhow::Result<T>,
    unlock_result: anyhow::Result<()>,
) -> anyhow::Result<T> {
    match (action_result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_value), Err(unlock_error)) => Err(unlock_error),
        (Err(error), Err(unlock_error)) => Err(anyhow::anyhow!(
            "{operation} failed: {error:#}; lifecycle transaction unlock also failed: {unlock_error:#}"
        )),
    }
}

fn with_lifecycle_ledger_lock<T>(
    db_path: &Path,
    operation: &'static str,
    action: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut lock = LifecycleLedgerLock::acquire(db_path, operation)?;
    let action_result = action();
    let unlock_result = lock.unlock_checked();
    combine_lifecycle_action_and_unlock(operation, action_result, unlock_result)
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

    let mut run = RunRecord {
        schema_version: SCHEMA_VERSION,
        run_id: format!(
            "{}-{}-{}",
            now_unix_ms(),
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ),
        pid: std::process::id(),
        mode: config.mode.to_owned(),
        bind_addr: config.bind_addr,
        db_path: paths.db_path.clone(),
        started_at_unix_ms: now_unix_ms(),
        ending_at_unix_ms: None,
        ending_reason: None,
        ending_phase: None,
        ended_at_unix_ms: None,
        ended_reason: None,
        previous_shutdown: None,
        previous_shutdown_detail: None,
        previous_run_id: None,
        previous_ended_reason: None,
        previous_ended_at_unix_ms: None,
        previous_ending_reason: None,
        previous_ending_phase: None,
        previous_ending_at_unix_ms: None,
    };
    let max_segment_bytes = configured_max_segment_bytes();
    let (tool_events, exit_events) = with_lifecycle_ledger_lock(
        &config.db_path,
        "configure daemon lifecycle",
        || {
            let tool_events_bytes = reconcile_jsonl_ledger(
                Path::new(&paths.tool_events_path),
                max_segment_bytes,
                "tool_events",
            )
            .with_context(|| {
                format!(
                    "reconcile daemon tool-event ledger {}",
                    paths.tool_events_path
                )
            })?;
            let exit_events_bytes = reconcile_jsonl_ledger(
                Path::new(&paths.exit_events_path),
                max_segment_bytes,
                "exit_events",
            )
            .with_context(|| format!("reconcile daemon exit ledger {}", paths.exit_events_path))?;
            let mut tool_events = LedgerAppender::new(tool_events_bytes);
            let mut exit_events = LedgerAppender::new(exit_events_bytes);
            let previous_run = read_optional_json::<RunRecord>(Path::new(&paths.run_current_path))
                .with_context(|| {
                    format!(
                        "read daemon lifecycle current run {}",
                        paths.run_current_path
                    )
                })?;
            // Derived from the ledger this daemon just reconciled, not from the
            // retired `daemon-tool-last.json` pointer. The ledger holds the same
            // records and is never staler: the pointer was written after the
            // append it duplicated, so a crash between the two left it behind.
            let previous_last_tool = last_tool_event_from_ledger(Path::new(
                &paths.tool_events_path,
            ))
            .with_context(|| {
                format!(
                    "derive previous last tool event from daemon tool-event ledger {}",
                    paths.tool_events_path
                )
            })?;
            retire_legacy_tool_last_pointer(Path::new(&paths.tool_last_path));

            // #2083: decide the clean/dirty verdict for the previous run and
            // carry it on THIS run's record, before that record is written. The
            // `previous_run_unclean` exit event below is append-only evidence
            // that only a ledger reader ever sees; the run record is what
            // `/health`, the boot log, and `synapse-setup.ps1 -Start` read.
            match previous_run.as_ref() {
                None => {
                    run.previous_shutdown = Some(PREVIOUS_SHUTDOWN_NONE.to_owned());
                    run.previous_shutdown_detail =
                        Some("basis=no previous run record exists on this vault".to_owned());
                }
                Some(previous) => {
                    run.previous_run_id = Some(previous.run_id.clone());
                    run.previous_ended_reason = previous.ended_reason.clone();
                    run.previous_ended_at_unix_ms = previous.ended_at_unix_ms;
                    run.previous_ending_reason = previous.ending_reason.clone();
                    run.previous_ending_phase = previous.ending_phase.clone();
                    run.previous_ending_at_unix_ms = previous.ending_at_unix_ms;
                    // #2100 made this three-way; #2131 made it read the whole
                    // record instead of one field. `ended_at` alone is NOT the
                    // discriminator: the watchdog writes it on its way to
                    // killing a close. See `classify_previous_shutdown`.
                    let decided = classify_previous_shutdown(previous);
                    run.previous_shutdown = Some(decided.verdict.to_owned());
                    run.previous_shutdown_detail = Some(decided.detail);
                }
            }

            // #2131: keyed off the verdict, not off `ended_at_unix_ms`. A
            // watchdog-killed close DOES carry `ended_at`, and gating the
            // append-only forensic event on that field is what let the loudest
            // evidence of an unclean stop go unwritten for exactly the stops
            // that most needed it.
            if let Some(previous) = previous_run.as_ref()
                && run.previous_shutdown.as_deref() != Some(PREVIOUS_SHUTDOWN_CLEAN)
            {
                append_bounded_json_line(
                Path::new(&paths.exit_events_path),
                &ExitEvent {
                    schema_version: SCHEMA_VERSION,
                    run_id: previous.run_id.clone(),
                    pid: previous.pid,
                    event_kind: "previous_run_unclean".to_owned(),
                    cause: if previous.ending_at_unix_ms.is_some() {
                        "killed_during_commanded_close"
                    } else if previous.ended_at_unix_ms.is_some() {
                        // #2131: finalized, but by a cause that names a kill or
                        // an abort, with nothing proving a close was commanded.
                        "ended_by_forced_cause_without_commanded_close"
                    } else {
                        "process_missing_on_startup"
                    }
                    .to_owned(),
                    detail: json!({
                        "new_pid": std::process::id(),
                        "new_run_id": run.run_id.clone(),
                        "reason": "daemon-run-current did not prove a completed graceful close when this daemon acquired the DB lock",
                        "previous_shutdown_detail": run.previous_shutdown_detail.clone(),
                        // #2100: the phase-one marker, carried into the
                        // append-only ledger as well as onto the run record, so
                        // the evidence survives even if the successor's record
                        // is later superseded.
                        "previous_shutdown_verdict": run.previous_shutdown.clone(),
                        "previous_ending_at_unix_ms": previous.ending_at_unix_ms,
                        "previous_ending_reason": previous.ending_reason.clone(),
                        "previous_ending_phase": previous.ending_phase.clone(),
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
                &mut exit_events,
                max_segment_bytes,
                "exit_events",
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
            // The reconciliation handles are released here: this daemon's own
            // appends reopen under the state mutex, after the cross-process
            // lifecycle-ledger lock this closure holds has been dropped.
            tool_events.close();
            exit_events.close();
            Ok((tool_events, exit_events))
        },
    )?;
    // Captured before `run` moves into the state: these are the #2083 boot
    // verdict fields. No default is substituted -- both arms of the match above
    // set `previous_shutdown`, so a `None` here means the verdict logic itself
    // was bypassed, and booting without knowing whether the last stop was clean
    // is exactly the blindness this exists to remove.
    let previous_shutdown = run.previous_shutdown.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "daemon lifecycle configure reached state installation without deciding a previous-shutdown verdict: run_id={} run_current_path={}",
            run.run_id,
            paths.run_current_path
        )
    })?;
    let previous_shutdown_detail = run
        .previous_shutdown_detail
        .clone()
        .unwrap_or_else(|| "unrecorded".to_owned());
    let previous_run_id = run.previous_run_id.clone();
    let previous_ended_reason = run.previous_ended_reason.clone();
    let previous_ended_at_unix_ms = run.previous_ended_at_unix_ms;
    let previous_ending_reason = run.previous_ending_reason.clone();
    let previous_ending_phase = run.previous_ending_phase.clone();
    let previous_ending_at_unix_ms = run.previous_ending_at_unix_ms;
    let run_id = run.run_id.clone();
    let state = DaemonLifecycleState {
        run,
        paths: paths.clone(),
        in_flight: BTreeMap::new(),
        seq: 0,
        last_error: None,
        last_tool_event: None,
        tool_events,
        exit_events,
        max_segment_bytes,
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
        previous_shutdown = %previous_shutdown,
        "daemon lifecycle ledger configured"
    );
    // #2083: one line, emitted on every boot, that answers "was the last stop
    // clean?" without reading a ledger. `dirty` is a warning because it means a
    // daemon died without finishing its drain -- storage close, input-lease
    // release and the graceful exit record all failed to run.
    if previous_shutdown == PREVIOUS_SHUTDOWN_DIRTY {
        tracing::warn!(
            code = "MCP_DAEMON_PREVIOUS_SHUTDOWN",
            previous_shutdown = %previous_shutdown,
            previous_run_id = previous_run_id.as_deref().unwrap_or("<none>"),
            previous_ended_reason = previous_ended_reason.as_deref().unwrap_or("<none>"),
            previous_ended_at_unix_ms,
            previous_shutdown_detail = %previous_shutdown_detail,
            run_id = %run_id,
            run_current_path = %paths.run_current_path,
            exit_events_path = %paths.exit_events_path,
            "previous daemon run ended without recording a graceful exit; a previous_run_unclean event was appended to the exit ledger"
        );
    } else if previous_shutdown == PREVIOUS_SHUTDOWN_INTERRUPTED_GRACEFUL {
        // #2100: a warning, like `dirty`, because the close did not finish --
        // but a *different* warning, because the remediation is different. This
        // one says the shutdown was commanded and got as far as the named phase
        // before something killed it, which points at the drain's exit-wait
        // budget rather than at a crash.
        tracing::warn!(
            code = "MCP_DAEMON_PREVIOUS_SHUTDOWN",
            previous_shutdown = %previous_shutdown,
            previous_run_id = previous_run_id.as_deref().unwrap_or("<none>"),
            previous_ended_reason = previous_ended_reason.as_deref().unwrap_or("<none>"),
            previous_ended_at_unix_ms,
            previous_ending_reason = previous_ending_reason.as_deref().unwrap_or("<none>"),
            previous_ending_phase = previous_ending_phase.as_deref().unwrap_or("<none>"),
            previous_ending_at_unix_ms,
            previous_shutdown_detail = %previous_shutdown_detail,
            run_id = %run_id,
            run_current_path = %paths.run_current_path,
            exit_events_path = %paths.exit_events_path,
            "previous daemon run was commanded to shut down, wrote its phase-one ending marker, and \
             did not prove the close finished; this is an interrupted graceful close, not a crash \
             while running and not a clean stop"
        );
    } else {
        tracing::info!(
            code = "MCP_DAEMON_PREVIOUS_SHUTDOWN",
            previous_shutdown = %previous_shutdown,
            previous_run_id = previous_run_id.as_deref().unwrap_or("<none>"),
            previous_ended_reason = previous_ended_reason.as_deref().unwrap_or("<none>"),
            previous_ended_at_unix_ms,
            previous_shutdown_detail = %previous_shutdown_detail,
            run_id = %run_id,
            run_current_path = %paths.run_current_path,
            "previous daemon shutdown verdict decided at boot"
        );
    }
    Ok(paths)
}

/// Installs the daemon-lifecycle panic hook.
///
/// # Release before record (#2082)
///
/// This hook used to go straight to [`record_panic`], which opens files, takes
/// the lifecycle state mutex and serializes JSON. All of that is fine for
/// forensics and useless to the human whose keyboard just died: `SendInput` key
/// and button state is **global to the OS input queue**, not owned by this
/// process, so a panic between a key-down and its key-up strands that key
/// system-wide and killing the daemon does not clear it. The daemon really did
/// panic mid-action (#2079), which is how #2082 was reported.
///
/// So the release runs first, unconditionally, through the raw allocation-free
/// `SendInput` sweep, and only then does the panic get recorded and the previous
/// hook chained. The sweep is idempotent, so it is safe that
/// [`synapse_action::install_panic_hook`] does the same thing: whichever hook
/// the runtime happens to run first, the input is released before anything that
/// can block, allocate, or take a lock the panicking thread already holds.
pub(crate) fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _report = synapse_action::release_all_synthetic_input_on_panic();
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
        schema_version: TOOL_EVENT_SCHEMA_VERSION,
        run_id: state.run.run_id.clone(),
        pid: state.run.pid,
        seq,
        event_kind: "tool_call".to_owned(),
        tool: start.tool,
        operation: start.operation,
        route_id: start.route_id,
        profile: start.profile,
        tool_surface_sha256: start.tool_surface_sha256,
        tool_profile_read_error: start.tool_profile_read_error,
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
        effective_target: None,
        error: None,
        terminal_error: None,
        panic: None,
        detail: None,
    };
    let mut started_event = event.clone();
    started_event.audit_context = None;
    started_event.audit_context_read_error = None;
    started_event.foreground = None;
    started_event.foreground_read_error = None;
    started_event.session_target = None;
    started_event.session_target_read_error = None;
    write_tool_event(state, &started_event)?;
    state.in_flight.insert(seq, event);
    Ok(ToolCallGuard {
        run_id: state.run.run_id.clone(),
        seq: Some(seq),
    })
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
        schema_version: TOOL_EVENT_SCHEMA_VERSION,
        run_id: state.run.run_id.clone(),
        pid: state.run.pid,
        seq,
        event_kind: input.event_kind.to_owned(),
        tool: input.tool.to_owned(),
        operation: None,
        route_id: None,
        profile: None,
        tool_surface_sha256: None,
        tool_profile_read_error: None,
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
        effective_target: None,
        error: None,
        terminal_error: None,
        panic: None,
        detail: Some(input.detail),
    };
    write_tool_event(state, &event)?;
    Ok(seq)
}

impl ToolCallGuard {
    pub(crate) fn finish_ok_with_effective_target(
        mut self,
        effective_target: Option<Value>,
    ) -> anyhow::Result<FinishedToolCallReadback> {
        self.finish("ok", None, None, effective_target)
    }

    pub(crate) fn finish_error(mut self, error: Value) -> anyhow::Result<FinishedToolCallReadback> {
        self.finish("error", Some(error), None, None)
    }

    pub(crate) fn finish_error_with_effective_target(
        mut self,
        error: Value,
        effective_target: Option<Value>,
    ) -> anyhow::Result<FinishedToolCallReadback> {
        self.finish("error", Some(error), None, effective_target)
    }

    pub(crate) fn finish_panic(mut self, panic: Value) -> anyhow::Result<FinishedToolCallReadback> {
        self.finish("panic", None, Some(panic), None)
    }

    fn finish(
        &mut self,
        status: &'static str,
        error: Option<Value>,
        panic: Option<Value>,
        effective_target: Option<Value>,
    ) -> anyhow::Result<FinishedToolCallReadback> {
        let seq = self
            .seq
            .ok_or_else(|| anyhow::anyhow!("daemon lifecycle tool guard is already terminal"))?;
        let result = finish_tool_call(&self.run_id, seq, status, error, panic, effective_target);
        if result.is_ok() {
            self.seq = None;
        }
        result
    }
}

impl Drop for ToolCallGuard {
    fn drop(&mut self) {
        let Some(seq) = self.seq.take() else {
            return;
        };
        let run_id = self.run_id.clone();
        let fallback = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            finish_tool_call(
                &run_id,
                seq,
                "error",
                Some(json!({
                    "code": synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "detail_code": "MCP_TOOL_CALL_GUARD_DROPPED_UNFINISHED",
                    "detail": "the routed MCP call owner was dropped before explicit lifecycle finalization",
                    "source_of_truth": "daemon lifecycle ToolCallGuard Drop backstop",
                })),
                None,
                None,
            )
        }));
        match fallback {
            Ok(Ok(_readback)) => {
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    detail_code = "MCP_TOOL_CALL_GUARD_DROPPED_UNFINISHED",
                    run_id,
                    seq,
                    "an unfinished MCP tool lifecycle owner was finalized by its Drop backstop"
                );
            }
            Ok(Err(error)) => {
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    detail_code = "MCP_TOOL_CALL_GUARD_DROP_FINALIZATION_FAILED",
                    run_id,
                    seq,
                    error = %error,
                    "an unfinished MCP tool lifecycle owner could not publish its Drop backstop"
                );
                eprintln!(
                    "synapse-mcp unfinished tool lifecycle cleanup failed: run_id={run_id} seq={seq} error={error:#}"
                );
            }
            Err(payload) => {
                let detail = consume_panic_payload(payload);
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    detail_code = "MCP_TOOL_CALL_GUARD_DROP_PANICKED",
                    run_id,
                    seq,
                    detail,
                    "an unfinished MCP tool lifecycle Drop backstop panicked"
                );
                eprintln!(
                    "synapse-mcp unfinished tool lifecycle cleanup panicked: run_id={run_id} seq={seq} detail={detail}"
                );
            }
        }
    }
}

pub(crate) fn begin_graceful_exit_finalization() -> anyhow::Result<GracefulExitFinalizationGuard> {
    let state = state_slot()
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let configured = state
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("daemon lifecycle ledger is not configured"))?;
    let ledger = LifecycleLedgerLock::acquire(
        Path::new(&configured.paths.db_path),
        "finalize graceful daemon exit across lifetime-lock release",
    )?;
    Ok(GracefulExitFinalizationGuard { ledger, state })
}

pub(crate) fn record_graceful_exit_after_lifetime_lock_close(
    mut finalization: GracefulExitFinalizationGuard,
    source: &'static str,
) -> anyhow::Result<()> {
    let action_result = finalization
        .state
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("daemon lifecycle ledger became unconfigured"))
        .and_then(|state| {
            record_exit_for_state_locked(
                state,
                "daemon_exit",
                "graceful",
                json!({
                    "source": source,
                }),
            )
        });
    let unlock_result = finalization.ledger.unlock_checked();
    combine_lifecycle_action_and_unlock(
        "finalize graceful daemon exit across lifetime-lock release",
        action_result,
        unlock_result,
    )
}

/// Phase one of the two-phase exit record: the daemon is *about to* close
/// (#2100).
///
/// # What it buys
///
/// The daemon's graceful drain flushes durably and then runs a close whose
/// tail — vault teardown, lineage record, GPU release, lock release — took 63
/// and 87+ seconds on the deployment host. The deploy drain's exit-wait
/// escalated inside that window and killed the process, so `ended_at_unix_ms`
/// was never written and the next boot read `previous_shutdown=dirty
/// previous_ended_reason=none`: the same verdict a crash-while-serving produces.
///
/// Writing the intent first makes the two distinguishable. It is cheap and
/// bounded by construction — one atomic JSON replace of a file that is already
/// open, taken before anything that can block — so the marker lands even when
/// everything after it stalls.
///
/// Idempotent: a second call for the same run refreshes `ending_phase` so a long
/// close can say how far it got, and never moves `ending_at_unix_ms` backwards
/// off the first declaration.
///
/// # Errors
///
/// Returns an error when the lifecycle ledger is not configured, its lock or
/// state cannot be taken, or the run record cannot be republished.
pub(crate) fn record_exit_intent(cause: &'static str, phase: &'static str) -> anyhow::Result<()> {
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
        bail!("daemon lifecycle ledger is not configured for exit intent ({cause})");
    };
    let db_path = PathBuf::from(&state.paths.db_path);
    with_lifecycle_ledger_lock(&db_path, "record daemon exit intent", || {
        record_exit_intent_for_state_locked(state, cause, phase)
    })
}

fn record_exit_intent_for_state_locked(
    state: &mut DaemonLifecycleState,
    cause: &'static str,
    phase: &'static str,
) -> anyhow::Result<()> {
    let mut run = state.run.clone();
    let first_declaration = run.ending_at_unix_ms.is_none();
    if first_declaration {
        run.ending_at_unix_ms = Some(now_unix_ms());
        run.ending_reason = Some(cause.to_owned());
    }
    run.ending_phase = Some(phase.to_owned());
    // Never overwrite a successor's record. Same rule as the exit finalizer: if
    // another daemon already owns `daemon-run-current.json`, this run's marker
    // would be a lie about which process is closing.
    let current = read_optional_json::<RunRecord>(Path::new(&state.paths.run_current_path))
        .with_context(|| {
            format!(
                "read daemon current run before exit intent {}",
                state.paths.run_current_path
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "daemon current run disappeared before exit intent: {}",
                state.paths.run_current_path
            )
        })?;
    if current.run_id != state.run.run_id {
        tracing::info!(
            code = "MCP_DAEMON_LIFECYCLE_RUN_CURRENT_SUPERSEDED",
            run_id = %state.run.run_id,
            run_current_path = %state.paths.run_current_path,
            "skipped the phase-one ending marker because a successor already owns the current-run record"
        );
        state.run = run;
        return Ok(());
    }
    write_json_atomic(Path::new(&state.paths.run_current_path), &run).with_context(|| {
        format!(
            "write daemon ending current run {}",
            state.paths.run_current_path
        )
    })?;
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_EXIT_INTENT_RECORDED",
        run_id = %run.run_id,
        pid = run.pid,
        cause,
        phase,
        first_declaration,
        ending_at_unix_ms = run.ending_at_unix_ms,
        run_current_path = %state.paths.run_current_path,
        "recorded the phase-one shutdown marker before the close began; a kill after this point \
         reads as interrupted_graceful rather than dirty at the next boot"
    );
    state.run = run;
    Ok(())
}

pub(crate) fn record_startup_exit(cause: &'static str, detail: Value) -> anyhow::Result<()> {
    record_exit("daemon_exit", cause, detail)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TopLevelErrorRecordOutcome {
    Recorded,
    NotConfigured,
}

/// Record a top-level failure when daemon lifecycle state exists.
///
/// Argument, telemetry, runtime, and other preflight failures can occur before
/// lifecycle configuration by construction. That expected absence is distinct
/// from a poisoned state lock or a failed durable ledger write, both of which
/// remain errors so the operator sees the secondary recording fault.
pub(crate) fn record_top_level_error(detail: &str) -> anyhow::Result<TopLevelErrorRecordOutcome> {
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
        return Ok(TopLevelErrorRecordOutcome::NotConfigured);
    };
    record_exit_for_state(
        state,
        "daemon_exit",
        "top_level_error",
        json!({
            "error": detail,
        }),
    )?;
    Ok(TopLevelErrorRecordOutcome::Recorded)
}

pub(crate) fn record_forced_exit_nonblocking(
    cause: &'static str,
    detail: Value,
) -> anyhow::Result<()> {
    let slot = state_slot();
    let mut guard = slot.try_lock().map_err(|error| {
        anyhow::anyhow!(
            "daemon lifecycle state lock unavailable for forced exit ({cause}): {error}"
        )
    })?;
    let Some(state) = guard.as_mut() else {
        bail!("daemon lifecycle ledger is not configured for forced exit ({cause})");
    };
    record_exit_for_state_locked(state, "daemon_exit", cause, detail)
}

/// Writes the graceful exit record from the #2090 OS-shutdown drain.
///
/// # Why this is not [`record_forced_exit_nonblocking`]
///
/// That one takes the state lock with `try_lock` and gives up immediately,
/// which is right for a panic hook already unwinding. This runs on an
/// OS-created handler thread with a real, documented budget (`SPI_GETHUNGAPPTIMEOUT`
/// / `SPI_GETWAITTOKILLTIMEOUT`, ~5 s), while the tokio runtime is still live
/// and may legitimately hold the lock for a few milliseconds. Losing the exit
/// record to a momentary lock hold would report `previous_shutdown=dirty` on the
/// next boot for a shutdown that was in fact orderly, so the lock is retried
/// until `deadline` and only then reported as a failure.
///
/// `cause` becomes `ended_reason` on `daemon-run-current.json`, so it names the
/// OS trigger (`os_console_close` / `os_window_close` / `os_logoff` / `os_shutdown` / `os_session_end`)
/// rather than the generic `graceful`.
pub(crate) fn record_os_shutdown_exit(
    cause: &'static str,
    detail: Value,
    deadline: Instant,
) -> anyhow::Result<()> {
    let slot = state_slot();
    loop {
        match slot.try_lock() {
            Ok(mut guard) => {
                let Some(state) = guard.as_mut() else {
                    bail!(
                        "daemon lifecycle ledger is not configured for OS shutdown exit ({cause})"
                    );
                };
                return record_exit_for_state(state, "daemon_exit", cause, detail);
            }
            Err(std::sync::TryLockError::Poisoned(_poisoned)) => {
                bail!("daemon lifecycle state lock poisoned during OS shutdown exit ({cause})");
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    bail!(
                        "daemon lifecycle state lock stayed held for the whole OS shutdown budget slice ({cause})"
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Appends the post-exit-record readback of the #2090 OS-shutdown drain
/// (lifetime-lock sidecar release) to the exit ledger.
///
/// It is a diagnostic event, not a second exit event: the run record already
/// ended at [`record_os_shutdown_exit`], and rewriting it here would move
/// `ended_at_unix_ms` after the fact.
pub(crate) fn record_os_shutdown_diagnostic(
    cause: &'static str,
    detail: Value,
) -> anyhow::Result<()> {
    append_diagnostic_event("os_shutdown_lock_release", cause, detail)
}

pub(crate) fn health_subsystem() -> SubsystemHealth {
    let slot = state_slot();
    let guard = match slot.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => {
            return SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(
                    "daemon lifecycle state lock is busy; health is fail-closed and does not wait behind lifecycle writes"
                        .to_owned(),
                ),
                ..SubsystemHealth::default()
            };
        }
        Err(std::sync::TryLockError::Poisoned(_error)) => {
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
        Some(state) => {
            let tool_ledger = ledger_diagnostic_value(
                Path::new(&state.paths.tool_events_path),
                state.max_segment_bytes,
                "tool_events",
            );
            let exit_ledger = ledger_diagnostic_value(
                Path::new(&state.paths.exit_events_path),
                state.max_segment_bytes,
                "exit_events",
            );
            json!({
                "status": if state.last_error.is_some() { "error" } else { "ok" },
                "run_id": state.run.run_id,
                "pid": state.run.pid,
                "paths": state.paths,
                "last_error": state.last_error,
                "in_flight_count": state.in_flight.len(),
                "ledgers": {
                    "tool_events": tool_ledger,
                    "exit_events": exit_ledger,
                },
            })
        }
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

/// This run's phase-one shutdown marker as `(ending_at_unix_ms, reason, phase)`,
/// read from memory without ever blocking (#2131).
///
/// The HTTP shutdown watchdog calls this from its own thread, while the drain it
/// is supervising may be holding locks anywhere. `try_lock` is therefore the
/// contract, not an optimization: a watchdog that could block on the lifecycle
/// state would be a watchdog that can hang, which defeats the only job it has.
/// `None` means "not knowable right now" and is reported as such.
pub(crate) fn current_exit_intent_snapshot() -> Option<(u64, String, String)> {
    let slot = state_slot();
    let guard = slot.try_lock().ok()?;
    let state = guard.as_ref()?;
    let ending_at = state.run.ending_at_unix_ms?;
    Some((
        ending_at,
        state
            .run
            .ending_reason
            .clone()
            .unwrap_or_else(|| "unrecorded".to_owned()),
        state
            .run
            .ending_phase
            .clone()
            .unwrap_or_else(|| "unrecorded".to_owned()),
    ))
}

fn tool_usage_empty(
    source_of_truth: String,
    max_rows: usize,
    read_error: Option<String>,
) -> ToolUsageTelemetry {
    ToolUsageTelemetry {
        source_of_truth,
        max_rows,
        rows_scanned: 0,
        segment_count: 0,
        terminal_error_projection_schema_version: TERMINAL_ERROR_PROJECTION_SCHEMA_VERSION,
        canonical_terminal_error_rows: 0,
        compatibility_terminal_error_rows: 0,
        decode_error_total: 0,
        decode_errors_truncated: false,
        decode_errors: Vec::new(),
        aggregate_count: 0,
        aggregates_truncated: false,
        max_error_codes_per_aggregate: MAX_ERROR_CODES_PER_AGGREGATE,
        aggregates: Vec::new(),
        read_error,
    }
}

fn tool_usage_decode_failure(
    code: &'static str,
    detail: impl Into<String>,
) -> ToolUsageDecodeFailure {
    ToolUsageDecodeFailure {
        code,
        detail: detail.into(),
    }
}

fn validate_bounded_projection_field(
    field: &'static str,
    value: Option<&str>,
    max_bytes: usize,
) -> Result<(), ToolUsageDecodeFailure> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty() || value.len() > max_bytes {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_PROJECTION_FIELD_OUT_OF_BOUNDS",
            format!(
                "field={field} byte_length={} allowed=1..={max_bytes}",
                value.len()
            ),
        ));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'.'
    }) {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_PROJECTION_FIELD_INVALID",
            format!(
                "field={field} must contain only lowercase ASCII letters, digits, underscore, or dot"
            ),
        ));
    }
    Ok(())
}

fn validate_canonical_error_code(code: &str) -> Result<(), ToolUsageDecodeFailure> {
    if code.is_empty() || code.len() > MAX_CANONICAL_ERROR_CODE_BYTES {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_ERROR_CODE_OUT_OF_BOUNDS",
            format!(
                "canonical error-code byte_length={} allowed=1..={MAX_CANONICAL_ERROR_CODE_BYTES}",
                code.len()
            ),
        ));
    }
    if !code
        .bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        || !code.as_bytes()[0].is_ascii_uppercase()
    {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_ERROR_CODE_INVALID",
            "canonical error code must start with an uppercase ASCII letter and contain only uppercase ASCII letters, digits, or underscore",
        ));
    }
    Ok(())
}

fn validate_tool_surface_sha256(value: Option<&str>) -> Result<(), ToolUsageDecodeFailure> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_TOOL_SURFACE_SHA256_INVALID",
            "tool_surface_sha256 must use the canonical sha256:<64 lowercase hex digits> form",
        ));
    };
    if value.len() != TOOL_SURFACE_SHA256_BYTES
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_TOOL_SURFACE_SHA256_INVALID",
            format!(
                "tool_surface_sha256 byte_length={} required={TOOL_SURFACE_SHA256_BYTES}; digest must contain exactly 64 lowercase hex digits",
                value.len()
            ),
        ));
    }
    Ok(())
}

fn canonical_error_code_from_error(error: &Value) -> Result<String, ToolUsageDecodeFailure> {
    fn codes_at_paths<'a>(
        error: &'a Value,
        paths: &[(&'static str, &'static str)],
    ) -> Result<Vec<(&'static str, &'a str)>, ToolUsageDecodeFailure> {
        let mut present = Vec::new();
        for (label, pointer) in paths {
            let Some(value) = error.pointer(pointer).filter(|value| !value.is_null()) else {
                continue;
            };
            let Some(code) = value.as_str() else {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_ERROR_CODE_TYPE_INVALID",
                    format!("{label} is present but is not a string"),
                ));
            };
            validate_canonical_error_code(code)?;
            present.push((*label, code));
        }
        Ok(present)
    }

    // The v1 writer emitted both of these paths for the same canonical code.
    // A `detail_code` beside them classifies a narrower internal cause and is
    // deliberately not allowed to override the public error class.
    let primary = codes_at_paths(
        error,
        &[
            ("error.synapse_code", "/synapse_code"),
            ("error.data.code", "/data/code"),
        ],
    )?;
    // Explicit compatibility for pre-v1 writer shapes. This is reached only
    // when neither canonical v1 path exists; it is not a precedence fallback.
    let present = if primary.is_empty() {
        codes_at_paths(
            error,
            &[
                ("error.code", "/code"),
                ("error.detail_code", "/detail_code"),
                ("error.data.detail_code", "/data/detail_code"),
            ],
        )?
    } else {
        primary
    };
    let Some((_, canonical)) = present.first().copied() else {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_ERROR_CODE_MISSING",
            "terminal error has no canonical code in any documented schema path",
        ));
    };
    if present.iter().any(|(_, code)| *code != canonical) {
        let path_list = present
            .iter()
            .map(|(path, _)| *path)
            .collect::<Vec<_>>()
            .join(",");
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_ERROR_CODE_MISMATCH",
            format!("documented canonical-code paths disagree: {path_list}"),
        ));
    }
    Ok(canonical.to_owned())
}

fn validate_terminal_error_projection(
    event: &ToolEvent,
    projection: &TerminalErrorProjection,
) -> Result<(), ToolUsageDecodeFailure> {
    if projection.schema_version != TERMINAL_ERROR_PROJECTION_SCHEMA_VERSION {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_TERMINAL_ERROR_SCHEMA_UNSUPPORTED",
            format!(
                "terminal_error.schema_version={} supported={TERMINAL_ERROR_PROJECTION_SCHEMA_VERSION}",
                projection.schema_version
            ),
        ));
    }
    validate_bounded_projection_field("facade", Some(&projection.facade), 64)?;
    validate_bounded_projection_field("operation", projection.operation.as_deref(), 64)?;
    validate_bounded_projection_field("route_id", projection.route_id.as_deref(), 129)?;
    validate_bounded_projection_field("profile", projection.profile.as_deref(), 32)?;
    validate_canonical_error_code(&projection.error_code)?;
    validate_tool_surface_sha256(projection.tool_surface_sha256.as_deref())?;
    let payload_code = event
        .error
        .as_ref()
        .map(canonical_error_code_from_error)
        .transpose()?;
    let projected_facade = projection.facade.as_str();
    let event_tool = event.tool.as_str();
    let routing_matches = projected_facade == event_tool
        && projection.operation == event.operation
        && projection.route_id == event.route_id;
    let outcome_matches = projection.status == event.status
        && payload_code
            .as_deref()
            .is_none_or(|payload_code| projection.error_code == payload_code)
        && Some(projection.duration_ms) == event.duration_ms;
    let context_matches = projection.profile == event.profile
        && projection.tool_surface_sha256 == event.tool_surface_sha256;
    if !(routing_matches && outcome_matches && context_matches) {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_TERMINAL_ERROR_PROJECTION_MISMATCH",
            "terminal_error projection disagrees with its enclosing tool event",
        ));
    }
    Ok(())
}

fn terminal_error_projection_for_writer(
    event: &ToolEvent,
) -> Result<Option<TerminalErrorProjection>, ToolUsageDecodeFailure> {
    if event.event_kind != "tool_call" || event.status != "error" {
        return Ok(None);
    }
    if event.schema_version != TOOL_EVENT_SCHEMA_VERSION {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_EVENT_SCHEMA_WRITE_INVALID",
            format!(
                "new terminal event schema_version={} required={TOOL_EVENT_SCHEMA_VERSION}",
                event.schema_version
            ),
        ));
    }
    let duration_ms = event.duration_ms.ok_or_else(|| {
        tool_usage_decode_failure(
            "MCP_TOOL_USAGE_TERMINAL_DURATION_MISSING",
            "status=error event is missing duration_ms",
        )
    })?;
    let error = event.error.as_ref().ok_or_else(|| {
        tool_usage_decode_failure(
            "MCP_TOOL_USAGE_TERMINAL_ERROR_PAYLOAD_MISSING",
            "status=error event is missing its structured error payload",
        )
    })?;
    let projection = TerminalErrorProjection {
        schema_version: TERMINAL_ERROR_PROJECTION_SCHEMA_VERSION,
        facade: event.tool.clone(),
        operation: event.operation.clone(),
        route_id: event.route_id.clone(),
        status: event.status.clone(),
        error_code: canonical_error_code_from_error(error)?,
        duration_ms,
        profile: event.profile.clone(),
        tool_surface_sha256: event.tool_surface_sha256.clone(),
    };
    validate_terminal_error_projection(event, &projection)?;
    Ok(Some(projection))
}

fn decode_tool_usage_line(
    line: &str,
) -> Result<
    (
        ToolEvent,
        Option<(TerminalErrorProjection, TerminalErrorDecodeSource)>,
    ),
    ToolUsageDecodeFailure,
> {
    let value: Value = serde_json::from_str(line).map_err(|error| {
        tool_usage_decode_failure(
            "MCP_TOOL_USAGE_JSON_INVALID",
            format!(
                "JSON decode failed at line {} column {}: {:?}",
                error.line(),
                error.column(),
                error.classify()
            ),
        )
    })?;
    let schema_version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            tool_usage_decode_failure(
                "MCP_TOOL_USAGE_EVENT_SCHEMA_MISSING",
                "lifecycle row has no unsigned integer schema_version",
            )
        })?;
    if schema_version != 1
        && schema_version != 2
        && schema_version != u64::from(TOOL_EVENT_SCHEMA_VERSION)
    {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_EVENT_SCHEMA_UNSUPPORTED",
            format!(
                "tool event schema_version={schema_version} supported=1,2,{TOOL_EVENT_SCHEMA_VERSION}"
            ),
        ));
    }
    let event: ToolEvent = serde_json::from_value(value).map_err(|error| {
        tool_usage_decode_failure(
            "MCP_TOOL_USAGE_EVENT_SHAPE_INVALID",
            format!("schema_version={schema_version} typed decode failed: {error}"),
        )
    })?;
    if event.event_kind != "tool_call" || event.status == "started" {
        return Ok((event, None));
    }
    if event.duration_ms.is_none() || event.finished_at_unix_ms.is_none() {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_TERMINAL_TIMING_MISSING",
            "terminal tool_call event is missing duration_ms or finished_at_unix_ms",
        ));
    }
    match event.status.as_str() {
        "ok" => {
            if event.error.is_some() || event.panic.is_some() || event.terminal_error.is_some() {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_OK_EVENT_CONTRADICTED",
                    "status=ok event carries error, panic, or terminal_error data",
                ));
            }
            Ok((event, None))
        }
        "panic" => {
            if event.panic.is_none() || event.error.is_some() || event.terminal_error.is_some() {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_PANIC_EVENT_CONTRADICTED",
                    "status=panic event must carry panic data and no error projection",
                ));
            }
            Ok((event, None))
        }
        "error" if schema_version == u64::from(TOOL_EVENT_SCHEMA_VERSION) => {
            if event.error.is_some() {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_V3_RAW_ERROR_PRESENT",
                    "v3 status=error event must persist terminal_error only, never the raw error payload",
                ));
            }
            let projection = event.terminal_error.clone().ok_or_else(|| {
                tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_TERMINAL_ERROR_PROJECTION_MISSING",
                    "v3 status=error event has no terminal_error projection",
                )
            })?;
            validate_terminal_error_projection(&event, &projection)?;
            Ok((
                event,
                Some((projection, TerminalErrorDecodeSource::Canonical)),
            ))
        }
        "error" if schema_version == 2 => {
            let projection = event.terminal_error.clone().ok_or_else(|| {
                tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_TERMINAL_ERROR_PROJECTION_MISSING",
                    "v2 status=error event has no terminal_error projection",
                )
            })?;
            if event.error.is_none() {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_V2_ERROR_PAYLOAD_MISSING",
                    "v2 status=error compatibility row has no structured error payload",
                ));
            }
            validate_terminal_error_projection(&event, &projection)?;
            Ok((
                event,
                Some((projection, TerminalErrorDecodeSource::Compatibility)),
            ))
        }
        "error" => {
            let error = event.error.as_ref().ok_or_else(|| {
                tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_TERMINAL_ERROR_PAYLOAD_MISSING",
                    "v1 status=error event has no structured error payload",
                )
            })?;
            let duration_ms = event.duration_ms.unwrap_or_default();
            let projection = TerminalErrorProjection {
                schema_version: TERMINAL_ERROR_PROJECTION_SCHEMA_VERSION,
                facade: event.tool.clone(),
                operation: event.operation.clone(),
                route_id: event.route_id.clone(),
                status: event.status.clone(),
                error_code: canonical_error_code_from_error(error)?,
                duration_ms,
                profile: event.profile.clone(),
                tool_surface_sha256: event.tool_surface_sha256.clone(),
            };
            validate_terminal_error_projection(&event, &projection)?;
            Ok((
                event,
                Some((projection, TerminalErrorDecodeSource::Compatibility)),
            ))
        }
        status => Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_TERMINAL_STATUS_UNSUPPORTED",
            format!("terminal tool_call status={status:?} is not ok, error, or panic"),
        )),
    }
}

fn push_tool_usage_decode_error(
    errors: &mut Vec<ToolUsageDecodeError>,
    total: &mut usize,
    path: &Path,
    line: usize,
    failure: ToolUsageDecodeFailure,
) {
    *total = total.saturating_add(1);
    if errors.len() >= MAX_TOOL_USAGE_DECODE_ERRORS {
        return;
    }
    errors.push(ToolUsageDecodeError {
        segment: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<non-utf8-segment>")
            .to_owned(),
        line,
        code: failure.code.to_owned(),
        detail: failure.detail,
    });
}

fn finalize_tool_usage_aggregates(
    accumulators: BTreeMap<ToolUsageKey, ToolUsageAccumulator>,
    max_aggregates: usize,
) -> (Vec<ToolUsageAggregate>, usize, bool) {
    let aggregate_count = accumulators.len();
    let mut values = accumulators
        .into_values()
        .map(|mut accumulator| {
            let distinct_error_code_count = accumulator.error_code_counts.len();
            let mut error_code_counts = accumulator
                .error_code_counts
                .into_iter()
                .map(|(error_code, count)| ToolUsageErrorCodeCount { error_code, count })
                .collect::<Vec<_>>();
            error_code_counts.sort_by(|left, right| {
                right
                    .count
                    .cmp(&left.count)
                    .then(left.error_code.cmp(&right.error_code))
            });
            error_code_counts.truncate(MAX_ERROR_CODES_PER_AGGREGATE);
            accumulator.aggregate.distinct_error_code_count = distinct_error_code_count;
            accumulator.aggregate.error_code_counts_truncated =
                distinct_error_code_count > error_code_counts.len();
            accumulator.aggregate.error_code_counts = error_code_counts;
            accumulator.aggregate
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .calls_total
            .cmp(&left.calls_total)
            .then(left.tool.cmp(&right.tool))
            .then(left.operation.cmp(&right.operation))
    });
    values.truncate(max_aggregates);
    let aggregates_truncated = aggregate_count > values.len();
    (values, aggregate_count, aggregates_truncated)
}

pub(crate) fn recent_tool_usage(max_rows: usize, max_aggregates: usize) -> ToolUsageTelemetry {
    let Some(paths) = current_paths() else {
        return tool_usage_empty(
            "daemon lifecycle ledger not configured".to_owned(),
            max_rows,
            Some("daemon lifecycle ledger not configured".to_owned()),
        );
    };
    let active = PathBuf::from(&paths.tool_events_path);
    let ledger_paths = match lifecycle_ledger_paths_oldest_first(&active) {
        Ok(paths) => paths,
        Err(error) => {
            return tool_usage_empty(
                active.display().to_string(),
                max_rows,
                Some(format!("{error:#}")),
            );
        }
    };
    let mut rows_scanned = 0_usize;
    let mut canonical_terminal_error_rows = 0_usize;
    let mut compatibility_terminal_error_rows = 0_usize;
    let mut decode_error_total = 0_usize;
    let mut decode_errors = Vec::new();
    let mut read_error = None;
    let mut aggregates: BTreeMap<ToolUsageKey, ToolUsageAccumulator> = BTreeMap::new();
    for path in ledger_paths.iter().rev() {
        if rows_scanned >= max_rows {
            break;
        }
        let lines = match File::open(path).map(BufReader::new) {
            Ok(reader) => reader.lines().collect::<Result<Vec<_>, _>>(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        };
        let lines = match lines {
            Ok(lines) => lines,
            Err(error) => {
                read_error = Some(format!("read {}: {error}", path.display()));
                break;
            }
        };
        for (line_index, line) in lines.into_iter().enumerate().rev() {
            if rows_scanned >= max_rows {
                break;
            }
            rows_scanned = rows_scanned.saturating_add(1);
            let (event, terminal_error) = match decode_tool_usage_line(&line) {
                Ok(decoded) => decoded,
                Err(failure) => {
                    push_tool_usage_decode_error(
                        &mut decode_errors,
                        &mut decode_error_total,
                        path,
                        line_index.saturating_add(1),
                        failure,
                    );
                    continue;
                }
            };
            if event.event_kind != "tool_call" || event.status == "started" {
                continue;
            }
            let key = (
                event.tool.clone(),
                event.operation.clone(),
                event.route_id.clone(),
                event.profile.clone(),
            );
            let accumulator = aggregates
                .entry(key)
                .or_insert_with(|| ToolUsageAccumulator {
                    aggregate: ToolUsageAggregate {
                        tool: event.tool.clone(),
                        operation: event.operation.clone(),
                        route_id: event.route_id.clone(),
                        profile: event.profile.clone(),
                        tool_surface_sha256: event.tool_surface_sha256.clone(),
                        calls_total: 0,
                        ok_total: 0,
                        error_total: 0,
                        panic_total: 0,
                        total_duration_ms: 0,
                        max_duration_ms: 0,
                        latest_status: event.status.clone(),
                        latest_error_code: None,
                        distinct_error_code_count: 0,
                        error_code_counts_truncated: false,
                        error_code_counts: Vec::new(),
                    },
                    error_code_counts: BTreeMap::new(),
                });
            accumulator.aggregate.calls_total = accumulator.aggregate.calls_total.saturating_add(1);
            match event.status.as_str() {
                "ok" => {
                    accumulator.aggregate.ok_total =
                        accumulator.aggregate.ok_total.saturating_add(1);
                }
                "panic" => {
                    accumulator.aggregate.panic_total =
                        accumulator.aggregate.panic_total.saturating_add(1);
                }
                "error" => {
                    accumulator.aggregate.error_total =
                        accumulator.aggregate.error_total.saturating_add(1);
                    let Some((projection, source)) = terminal_error else {
                        unreachable!("decoded status=error event always carries a projection");
                    };
                    match source {
                        TerminalErrorDecodeSource::Canonical => {
                            canonical_terminal_error_rows =
                                canonical_terminal_error_rows.saturating_add(1);
                        }
                        TerminalErrorDecodeSource::Compatibility => {
                            compatibility_terminal_error_rows =
                                compatibility_terminal_error_rows.saturating_add(1);
                        }
                    }
                    accumulator
                        .aggregate
                        .latest_error_code
                        .get_or_insert_with(|| projection.error_code.clone());
                    let count = accumulator
                        .error_code_counts
                        .entry(projection.error_code)
                        .or_default();
                    *count = count.saturating_add(1);
                }
                _ => unreachable!("decoder rejects unsupported terminal statuses"),
            }
            let duration_ms = event.duration_ms.unwrap_or_default();
            accumulator.aggregate.total_duration_ms = accumulator
                .aggregate
                .total_duration_ms
                .saturating_add(duration_ms);
            accumulator.aggregate.max_duration_ms =
                accumulator.aggregate.max_duration_ms.max(duration_ms);
        }
    }
    let (aggregates, aggregate_count, aggregates_truncated) =
        finalize_tool_usage_aggregates(aggregates, max_aggregates);
    ToolUsageTelemetry {
        source_of_truth: active.display().to_string(),
        max_rows,
        rows_scanned,
        segment_count: ledger_paths.len(),
        terminal_error_projection_schema_version: TERMINAL_ERROR_PROJECTION_SCHEMA_VERSION,
        canonical_terminal_error_rows,
        compatibility_terminal_error_rows,
        decode_error_total,
        decode_errors_truncated: decode_error_total > decode_errors.len(),
        decode_errors,
        aggregate_count,
        aggregates_truncated,
        max_error_codes_per_aggregate: MAX_ERROR_CODES_PER_AGGREGATE,
        aggregates,
        read_error,
    }
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

/// Extract a useful panic diagnostic and consume the payload. Unknown payloads
/// are explicitly dropped under a second unwind boundary because user-defined
/// payload destructors can themselves panic. Only an unknown *secondary* panic
/// payload is leaked, after it has been logged, to prevent recursive destructor
/// panics from aborting the process during safety cleanup.
pub(crate) fn consume_panic_payload(payload: Box<dyn std::any::Any + Send>) -> String {
    let payload = match payload.downcast::<String>() {
        Ok(message) => return *message,
        Err(payload) => payload,
    };
    let payload = match payload.downcast::<&'static str>() {
        Ok(message) => return (*message).to_owned(),
        Err(payload) => payload,
    };
    let original_type_id = format!("{:?}", payload.as_ref().type_id());
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(payload))) {
        Ok(()) => format!("non-string panic payload (type_id={original_type_id})"),
        Err(secondary) => {
            let secondary_text = secondary
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| secondary.downcast_ref::<&'static str>().copied())
                .map(str::to_owned);
            let secondary_type_id = format!("{:?}", secondary.as_ref().type_id());
            tracing::error!(
                code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                detail_code = "PANIC_PAYLOAD_DROP_PANICKED",
                original_type_id = %original_type_id,
                secondary_type_id = %secondary_type_id,
                secondary = secondary_text.as_deref().unwrap_or("non-string panic payload"),
                "dropping a caught panic payload panicked; preserving process safety"
            );
            if secondary_text.is_some() {
                drop(secondary);
            } else {
                // Log first, then leak only this unknown secondary payload. Its
                // destructor just panicked and retrying it risks process abort.
                std::mem::forget(secondary);
            }
            format!(
                "non-string panic payload (type_id={original_type_id}); payload Drop panicked: {}",
                secondary_text.unwrap_or_else(|| {
                    format!("non-string secondary payload (type_id={secondary_type_id})")
                })
            )
        }
    }
}

fn finish_tool_call(
    run_id: &str,
    seq: u64,
    status: &'static str,
    error: Option<Value>,
    panic: Option<Value>,
    effective_target: Option<Value>,
) -> anyhow::Result<FinishedToolCallReadback> {
    let slot = state_slot();
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
        bail!("daemon lifecycle ledger is not configured");
    };
    if state.run.run_id != run_id {
        bail!(
            "daemon lifecycle tool event {seq} belongs to superseded run {run_id}, current run is {}",
            state.run.run_id
        );
    }
    let Some(mut event) = state.in_flight.get(&seq).cloned() else {
        bail!("daemon lifecycle in-flight tool event {seq} is missing");
    };
    let finished_at_unix_ms = now_unix_ms();
    status.clone_into(&mut event.status);
    event.finished_at_unix_ms = Some(finished_at_unix_ms);
    event.duration_ms = Some(finished_at_unix_ms.saturating_sub(event.started_at_unix_ms));
    event.effective_target = effective_target;
    event.error = error;
    event.panic = panic;
    event.terminal_error = terminal_error_projection_for_writer(&event).map_err(|failure| {
        tracing::error!(
            code = "MCP_DAEMON_TERMINAL_ERROR_PROJECTION_INVALID",
            detail_code = failure.code,
            tool = %event.tool,
            operation = event.operation.as_deref().unwrap_or("<none>"),
            route_id = event.route_id.as_deref().unwrap_or("<none>"),
            status = %event.status,
            detail = %failure.detail,
            "refused to publish a terminal tool event without a valid canonical error projection"
        );
        anyhow::anyhow!("{}: {}", failure.code, failure.detail)
    })?;
    // The full error belongs to the immediate MCP response only. It may echo a
    // rejected selector, URL, operation, path, or other caller input. Persist
    // only the bounded typed projection; otherwise the lifecycle ledger and
    // every downstream aggregate inherit unbounded/sensitive request data.
    let mut persisted_event = event.clone();
    persisted_event.error = None;
    write_tool_event(state, &persisted_event)?;
    state.in_flight.remove(&seq);
    FinishedToolCallReadback::try_from(event)
}

fn record_panic(info: &std::panic::PanicHookInfo<'_>) -> anyhow::Result<()> {
    let location = info.location().map(|location| {
        json!({
            "file": location.file(),
            "line": location.line(),
            "column": location.column(),
        })
    });
    let payload = panic_payload_message(info.payload());
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
    let mut guard = slot
        .lock()
        .map_err(|_error| anyhow::anyhow!("daemon lifecycle state lock poisoned"))?;
    let Some(state) = guard.as_mut() else {
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
        last_tool_event: state.last_tool_event.clone(),
        in_flight_tool_events: state.in_flight.values().cloned().collect(),
        paths: state.paths.clone(),
    };
    let db_path = PathBuf::from(&state.paths.db_path);
    let exit_events_path = state.paths.exit_events_path.clone();
    with_lifecycle_ledger_lock(&db_path, "append daemon diagnostic event", || {
        append_exit_event(state, &event)
            .with_context(|| format!("append daemon diagnostic event {exit_events_path}"))
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
    record_exit_for_state(state, event_kind, cause, detail)
}

fn record_exit_for_state(
    state: &mut DaemonLifecycleState,
    event_kind: &'static str,
    cause: &'static str,
    detail: Value,
) -> anyhow::Result<()> {
    let db_path = PathBuf::from(&state.paths.db_path);
    with_lifecycle_ledger_lock(&db_path, "record daemon exit", || {
        record_exit_for_state_locked(state, event_kind, cause, detail)
    })
}

fn record_exit_for_state_locked(
    state: &mut DaemonLifecycleState,
    event_kind: &'static str,
    cause: &'static str,
    detail: Value,
) -> anyhow::Result<()> {
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
        last_tool_event: state.last_tool_event.clone(),
        in_flight_tool_events: state.in_flight.values().cloned().collect(),
        paths: state.paths.clone(),
    };
    let current = read_optional_json::<RunRecord>(Path::new(&state.paths.run_current_path))
        .with_context(|| {
            format!(
                "read daemon current run before exit finalization {}",
                state.paths.run_current_path
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "daemon current run disappeared before exit finalization: {}",
                state.paths.run_current_path
            )
        })?;
    let owns_run_current = current.run_id == state.run.run_id;
    append_exit_event(state, &event)
        .with_context(|| format!("append daemon exit event {}", state.paths.exit_events_path))?;
    if owns_run_current {
        write_json_atomic(Path::new(&state.paths.run_current_path), &run).with_context(|| {
            format!(
                "write daemon ended current run {}",
                state.paths.run_current_path
            )
        })?;
    }
    if !owns_run_current {
        tracing::info!(
            code = "MCP_DAEMON_LIFECYCLE_RUN_CURRENT_SUPERSEDED",
            run_id = %state.run.run_id,
            run_current_path = %state.paths.run_current_path,
            "recorded this daemon's exit event without overwriting a successor's current-run record"
        );
    }
    state.run = run;
    Ok(())
}

fn write_tool_event(state: &mut DaemonLifecycleState, event: &ToolEvent) -> anyhow::Result<()> {
    match write_tool_event_inner(state, event) {
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

fn write_tool_event_inner(
    state: &mut DaemonLifecycleState,
    event: &ToolEvent,
) -> anyhow::Result<()> {
    validate_tool_event_for_write(event)
        .map_err(|failure| anyhow::anyhow!("{}: {}", failure.code, failure.detail))?;
    append_tool_event(state, event)?;
    // The ledger append above already fsync'd this exact record. Recording it
    // in memory rather than re-publishing it to `daemon-tool-last.json` removes
    // the dual write described on `DaemonLifecycleState::last_tool_event`.
    state.last_tool_event = Some(event.clone());
    Ok(())
}

fn validate_tool_event_for_write(event: &ToolEvent) -> Result<(), ToolUsageDecodeFailure> {
    if event.schema_version != TOOL_EVENT_SCHEMA_VERSION {
        return Err(tool_usage_decode_failure(
            "MCP_TOOL_USAGE_EVENT_SCHEMA_WRITE_INVALID",
            format!(
                "new tool event schema_version={} required={TOOL_EVENT_SCHEMA_VERSION}",
                event.schema_version
            ),
        ));
    }
    if event.event_kind != "tool_call" {
        if event.terminal_error.is_some() {
            return Err(tool_usage_decode_failure(
                "MCP_TOOL_USAGE_CONTEXT_ERROR_PROJECTION_INVALID",
                "non-tool-call lifecycle event carries terminal_error",
            ));
        }
        return Ok(());
    }
    match event.status.as_str() {
        "started" => {
            if event.finished_at_unix_ms.is_some()
                || event.duration_ms.is_some()
                || event.error.is_some()
                || event.panic.is_some()
                || event.terminal_error.is_some()
            {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_STARTED_EVENT_CONTRADICTED",
                    "status=started event carries terminal timing or outcome data",
                ));
            }
        }
        "ok" => {
            if event.finished_at_unix_ms.is_none()
                || event.duration_ms.is_none()
                || event.error.is_some()
                || event.panic.is_some()
                || event.terminal_error.is_some()
            {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_OK_EVENT_CONTRADICTED",
                    "status=ok event must carry timing and no error, panic, or terminal_error data",
                ));
            }
        }
        "panic" => {
            if event.finished_at_unix_ms.is_none()
                || event.duration_ms.is_none()
                || event.panic.is_none()
                || event.error.is_some()
                || event.terminal_error.is_some()
            {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_PANIC_EVENT_CONTRADICTED",
                    "status=panic event must carry timing and panic data only",
                ));
            }
        }
        "error" => {
            let projection = event.terminal_error.as_ref().ok_or_else(|| {
                tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_TERMINAL_ERROR_PROJECTION_MISSING",
                    "new status=error event has no terminal_error projection",
                )
            })?;
            if event.finished_at_unix_ms.is_none()
                || event.duration_ms.is_none()
                || event.panic.is_some()
                || event.error.is_some()
            {
                return Err(tool_usage_decode_failure(
                    "MCP_TOOL_USAGE_ERROR_EVENT_CONTRADICTED",
                    "status=error event must carry timing and terminal_error only, with no panic or raw error payload",
                ));
            }
            validate_terminal_error_projection(event, projection)?;
        }
        status => {
            return Err(tool_usage_decode_failure(
                "MCP_TOOL_USAGE_STATUS_WRITE_INVALID",
                format!("new tool_call event status={status:?} is unsupported"),
            ));
        }
    }
    Ok(())
}

fn append_tool_event(state: &mut DaemonLifecycleState, event: &ToolEvent) -> anyhow::Result<()> {
    let tool_events_path = state.paths.tool_events_path.clone();
    append_bounded_json_line(
        Path::new(&tool_events_path),
        event,
        &mut state.tool_events,
        state.max_segment_bytes,
        "tool_events",
    )
    .with_context(|| format!("append daemon tool event {tool_events_path}"))
}

fn append_exit_event(state: &mut DaemonLifecycleState, event: &ExitEvent) -> anyhow::Result<()> {
    let exit_events_path = state.paths.exit_events_path.clone();
    match append_bounded_json_line(
        Path::new(&exit_events_path),
        event,
        &mut state.exit_events,
        state.max_segment_bytes,
        "exit_events",
    ) {
        Ok(()) => {
            state.last_error = None;
            tracing::info!(
                code = "MCP_DAEMON_LIFECYCLE_EXIT_EVENT_RECORDED",
                cause = %event.cause,
                event_kind = %event.event_kind,
                "daemon lifecycle exit event recorded"
            );
            Ok(())
        }
        Err(error) => {
            let detail = format!("{error:#}");
            state.last_error = Some(detail.clone());
            tracing::error!(
                code = "MCP_DAEMON_LIFECYCLE_EXIT_WRITE_FAILED",
                cause = %event.cause,
                event_kind = %event.event_kind,
                detail = %detail,
                "daemon lifecycle exit event write failed"
            );
            Err(error)
        }
    }
}

/// Append one JSON line to a bounded lifecycle ledger, rotating the active
/// segment first when this record would push it past the size cap.
///
/// The active byte counter is tracked in memory so the append hot path never
/// stats the file. Rotation runs before the active file is opened, so Windows
/// never has to rename a file with an open append handle. If a single record is
/// larger than the cap, it is written to an empty segment and reported as an
/// explicit oversize exception instead of being dropped.
fn append_bounded_json_line<T: Serialize>(
    path: &Path,
    value: &T,
    appender: &mut LedgerAppender,
    max_segment_bytes: u64,
    ledger_name: &'static str,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(value)
        .with_context(|| format!("encode JSON line {}", path.display()))?;
    line.push(b'\n');
    let line_len = u64::try_from(line.len()).unwrap_or(u64::MAX);

    if appender.active_bytes > 0
        && appender.active_bytes.saturating_add(line_len) > max_segment_bytes
    {
        // Release the append handle before the rename: rotation must move the
        // active segment aside, and the reopened handle must land on the new
        // empty active file rather than following the rotated one.
        let rotated_from_bytes = appender.active_bytes;
        appender.close();
        if let Err(error) = rotate_ledger(path, ledger_name) {
            let detail = format!("{error:#}");
            tracing::error!(
                code = "DAEMON_LEDGER_ROTATE_FAILED",
                ledger = ledger_name,
                path = %path.display(),
                active_bytes = rotated_from_bytes,
                next_record_bytes = line_len,
                max_segment_bytes,
                detail = %detail,
                "daemon lifecycle ledger rotation failed"
            );
            return Err(error);
        }
        appender.active_bytes = 0;
        tracing::info!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_ROTATED",
            ledger = ledger_name,
            path = %path.display(),
            max_segment_bytes,
            max_segments = MAX_LEDGER_SEGMENTS,
            "daemon lifecycle ledger rotated"
        );
    }

    if line_len > max_segment_bytes {
        tracing::warn!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_OVERSIZED_RECORD",
            ledger = ledger_name,
            path = %path.display(),
            record_bytes = line_len,
            max_segment_bytes,
            "daemon lifecycle ledger record exceeds the segment cap and is retained as an explicit oversize exception"
        );
    }

    // A failed write must not leave a handle whose file position or validity is
    // unknown to the next append: drop it so the next call reopens from a known
    // state, and report the failure rather than retrying silently.
    let append_result = (|| -> anyhow::Result<()> {
        let file = appender.handle(path)?;
        file.write_all(&line)
            .with_context(|| format!("write daemon lifecycle ledger {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flush {}", path.display()))?;
        file.sync_data()
            .with_context(|| format!("sync {}", path.display()))?;
        Ok(())
    })();
    if let Err(error) = append_result {
        appender.close();
        return Err(error);
    }
    appender.active_bytes = appender.active_bytes.saturating_add(line_len);
    Ok(())
}

fn reconcile_jsonl_ledger(
    active: &Path,
    max_segment_bytes: u64,
    ledger_name: &'static str,
) -> anyhow::Result<u64> {
    let sources = discover_ledger_sources(active)?;
    if sources.is_empty() {
        return Ok(0);
    }
    let parent = ledger_parent(active)?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let staging_dir = unique_ledger_dir(active, "rewrite")?;
    fs::create_dir(&staging_dir).with_context(|| {
        format!(
            "create ledger rewrite staging dir {}",
            staging_dir.display()
        )
    })?;

    let rewrite =
        match stage_rewritten_ledger(&sources, &staging_dir, max_segment_bytes, ledger_name) {
            Ok(rewrite) => rewrite,
            Err(error) => {
                remove_dir_all_best_effort(&staging_dir, "rewrite staging", ledger_name);
                return Err(error);
            }
        };

    let retained_start = rewrite
        .segments
        .len()
        .saturating_sub(MAX_RETAINED_LEDGER_FILES);
    let pruned = &rewrite.segments[..retained_start];
    let pruned_bytes: u64 = pruned.iter().map(|segment| segment.bytes).sum();
    let pruned_records: u64 = pruned.iter().map(|segment| segment.records).sum();
    let pruned_oversized_records: u64 =
        pruned.iter().map(|segment| segment.oversized_records).sum();
    let retained = &rewrite.segments[retained_start..];
    let retained_oversized_records: u64 = retained
        .iter()
        .map(|segment| segment.oversized_records)
        .sum();
    let active_bytes = install_reconciled_ledger(active, &sources, retained, ledger_name)
        .with_context(|| {
            format!(
                "install reconciled daemon lifecycle {ledger_name} ledger {}",
                active.display()
            )
        })?;
    remove_dir_all_best_effort(&staging_dir, "rewrite staging", ledger_name);

    if pruned_records > 0 || pruned_bytes > 0 {
        tracing::warn!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_RETENTION_PRUNED",
            ledger = ledger_name,
            active_path = %active.display(),
            pruned_records,
            pruned_bytes,
            pruned_oversized_records,
            retained_files = retained.len(),
            max_retained_files = MAX_RETAINED_LEDGER_FILES,
            "daemon lifecycle ledger startup reconciliation pruned records outside retention"
        );
    }
    if rewrite.missing_newline_repairs > 0 {
        tracing::warn!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_MISSING_NEWLINE_REPAIRED",
            ledger = ledger_name,
            active_path = %active.display(),
            repaired_lines = rewrite.missing_newline_repairs,
            "daemon lifecycle ledger startup reconciliation repaired unterminated JSONL records before future appends"
        );
    }
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_LEDGER_RECONCILED",
        ledger = ledger_name,
        active_path = %active.display(),
        source_files = sources.len(),
        source_records = rewrite.source_records,
        source_bytes = rewrite.source_bytes,
        retained_files = retained.len(),
        retained_oversized_records,
        active_bytes,
        max_segment_bytes,
        max_retained_files = MAX_RETAINED_LEDGER_FILES,
        "daemon lifecycle ledger startup reconciliation complete"
    );
    Ok(active_bytes)
}

fn stage_rewritten_ledger(
    sources: &[LedgerSource],
    staging_dir: &Path,
    max_segment_bytes: u64,
    ledger_name: &'static str,
) -> anyhow::Result<LedgerRewrite> {
    let mut writer = LedgerRewriteWriter::new(staging_dir, max_segment_bytes, ledger_name);
    let mut source_bytes = 0_u64;
    let mut source_records = 0_u64;
    let mut missing_newline_repairs = 0_u64;

    for source in sources {
        let file = File::open(&source.path)
            .with_context(|| format!("open lifecycle ledger segment {}", source.path.display()))?;
        let mut reader = BufReader::new(file);
        loop {
            let mut line = Vec::new();
            let read = reader.read_until(b'\n', &mut line).with_context(|| {
                format!("read lifecycle ledger segment {}", source.path.display())
            })?;
            if read == 0 {
                break;
            }
            source_records = source_records.saturating_add(1);
            source_bytes = source_bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            if !line.ends_with(b"\n") {
                line.push(b'\n');
                missing_newline_repairs = missing_newline_repairs.saturating_add(1);
            }
            writer.append_line(&line)?;
        }
    }

    Ok(LedgerRewrite {
        segments: writer.finish()?,
        source_bytes,
        source_records,
        missing_newline_repairs,
    })
}

struct LedgerRewriteWriter<'a> {
    dir: &'a Path,
    max_segment_bytes: u64,
    ledger_name: &'static str,
    next_index: usize,
    current_file: Option<File>,
    current_path: PathBuf,
    current_bytes: u64,
    current_records: u64,
    current_oversized_records: u64,
    segments: Vec<StagedLedgerSegment>,
}

impl<'a> LedgerRewriteWriter<'a> {
    fn new(dir: &'a Path, max_segment_bytes: u64, ledger_name: &'static str) -> Self {
        Self {
            dir,
            max_segment_bytes,
            ledger_name,
            next_index: 0,
            current_file: None,
            current_path: PathBuf::new(),
            current_bytes: 0,
            current_records: 0,
            current_oversized_records: 0,
            segments: Vec::new(),
        }
    }

    fn append_line(&mut self, line: &[u8]) -> anyhow::Result<()> {
        let line_len = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if self.current_bytes > 0
            && self.current_bytes.saturating_add(line_len) > self.max_segment_bytes
        {
            self.finish_current()?;
        }
        if self.current_file.is_none() {
            self.start_segment()?;
        }
        let file = self
            .current_file
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ledger rewrite segment was not opened"))?;
        file.write_all(line).with_context(|| {
            format!(
                "write staged lifecycle ledger {}",
                self.current_path.display()
            )
        })?;
        self.current_bytes = self.current_bytes.saturating_add(line_len);
        self.current_records = self.current_records.saturating_add(1);
        if line_len > self.max_segment_bytes {
            self.current_oversized_records = self.current_oversized_records.saturating_add(1);
            tracing::warn!(
                code = "MCP_DAEMON_LIFECYCLE_LEDGER_OVERSIZED_RECORD",
                ledger = self.ledger_name,
                staged_path = %self.current_path.display(),
                record_bytes = line_len,
                max_segment_bytes = self.max_segment_bytes,
                "daemon lifecycle ledger reconciliation retained a single record larger than the segment cap"
            );
            self.finish_current()?;
        }
        Ok(())
    }

    fn start_segment(&mut self) -> anyhow::Result<()> {
        let path = self.dir.join(format!(
            "segment-{index:020}.jsonl",
            index = self.next_index
        ));
        self.next_index = self.next_index.saturating_add(1);
        let file = File::create(&path)
            .with_context(|| format!("create staged ledger {}", path.display()))?;
        self.current_file = Some(file);
        self.current_path = path;
        self.current_bytes = 0;
        self.current_records = 0;
        self.current_oversized_records = 0;
        Ok(())
    }

    fn finish_current(&mut self) -> anyhow::Result<()> {
        let Some(mut file) = self.current_file.take() else {
            return Ok(());
        };
        file.flush()
            .with_context(|| format!("flush staged ledger {}", self.current_path.display()))?;
        file.sync_data()
            .with_context(|| format!("sync staged ledger {}", self.current_path.display()))?;
        self.segments.push(StagedLedgerSegment {
            path: self.current_path.clone(),
            bytes: self.current_bytes,
            records: self.current_records,
            oversized_records: self.current_oversized_records,
        });
        self.current_path = PathBuf::new();
        self.current_bytes = 0;
        self.current_records = 0;
        self.current_oversized_records = 0;
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<Vec<StagedLedgerSegment>> {
        self.finish_current()?;
        Ok(self.segments)
    }
}

fn install_reconciled_ledger(
    active: &Path,
    sources: &[LedgerSource],
    retained: &[StagedLedgerSegment],
    ledger_name: &'static str,
) -> anyhow::Result<u64> {
    let backup_dir = unique_ledger_dir(active, "backup")?;
    fs::create_dir(&backup_dir)
        .with_context(|| format!("create ledger rewrite backup dir {}", backup_dir.display()))?;
    for source in sources {
        let backup_path = backup_dir.join(source.path.file_name().ok_or_else(|| {
            anyhow::anyhow!("ledger source has no file name: {}", source.path.display())
        })?);
        fs::rename(&source.path, &backup_path).with_context(|| {
            format!(
                "move existing daemon lifecycle {ledger_name} ledger segment {} to backup {}",
                source.path.display(),
                backup_path.display()
            )
        })?;
    }

    for (newest_offset, segment) in retained.iter().rev().enumerate() {
        let destination = if newest_offset == 0 {
            active.to_path_buf()
        } else {
            segment_path(active, newest_offset)
        };
        fs::rename(&segment.path, &destination).with_context(|| {
            format!(
                "install daemon lifecycle {ledger_name} ledger segment {} to {}",
                segment.path.display(),
                destination.display()
            )
        })?;
    }
    let active_bytes = retained.last().map_or(0, |segment| segment.bytes);

    match fs::remove_dir_all(&backup_dir) {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(
                code = "MCP_DAEMON_LIFECYCLE_LEDGER_BACKUP_CLEANUP_FAILED",
                ledger = ledger_name,
                backup_dir = %backup_dir.display(),
                error = %error,
                "daemon lifecycle ledger rewrite succeeded but backup cleanup failed"
            );
        }
    }
    Ok(active_bytes)
}

fn remove_dir_all_best_effort(path: &Path, role: &'static str, ledger_name: &'static str) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                code = "MCP_DAEMON_LIFECYCLE_LEDGER_TEMP_CLEANUP_FAILED",
                ledger = ledger_name,
                role,
                path = %path.display(),
                error = %error,
                "daemon lifecycle ledger temporary directory cleanup failed"
            );
        }
    }
}

fn unique_ledger_dir(active: &Path, role: &str) -> anyhow::Result<PathBuf> {
    let parent = ledger_parent(active)?;
    let file_name = active
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("ledger path has no file name: {}", active.display()))?
        .to_string_lossy();
    Ok(parent.join(format!(
        ".{file_name}.{role}.{}.{}",
        std::process::id(),
        uuid::Uuid::now_v7().simple()
    )))
}

fn ledger_parent(active: &Path) -> anyhow::Result<&Path> {
    active
        .parent()
        .ok_or_else(|| anyhow::anyhow!("ledger path has no parent: {}", active.display()))
}

/// Rotate an active lifecycle ledger using a fixed shift scheme.
///
/// `<ledger>.1` is always the most recently rotated segment and
/// `<ledger>.{MAX_LEDGER_SEGMENTS}` the oldest. The oldest slot is pruned before
/// shifting so the file count cannot exceed the retention cap. Every error is
/// propagated so callers never continue appending into an oversized active file.
fn rotate_ledger(active: &Path, ledger_name: &'static str) -> anyhow::Result<()> {
    let oldest = segment_path(active, MAX_LEDGER_SEGMENTS);
    match fs::metadata(&oldest) {
        Ok(metadata) => {
            fs::remove_file(&oldest).with_context(|| {
                format!(
                    "prune oldest daemon lifecycle {ledger_name} segment {}",
                    oldest.display()
                )
            })?;
            tracing::warn!(
                code = "MCP_DAEMON_LIFECYCLE_LEDGER_RETENTION_PRUNED",
                ledger = ledger_name,
                path = %oldest.display(),
                pruned_bytes = metadata.len(),
                max_segments = MAX_LEDGER_SEGMENTS,
                "daemon lifecycle ledger rotation pruned oldest retained segment"
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "stat oldest daemon lifecycle {ledger_name} segment {}",
                    oldest.display()
                )
            });
        }
    }
    for index in (1..MAX_LEDGER_SEGMENTS).rev() {
        let from = segment_path(active, index);
        let to = segment_path(active, index + 1);
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "shift daemon lifecycle {ledger_name} segment {} to {}",
                        from.display(),
                        to.display()
                    )
                });
            }
        }
    }
    let newest = segment_path(active, 1);
    fs::rename(active, &newest).with_context(|| {
        format!(
            "rotate active daemon lifecycle {ledger_name} ledger {} to {}",
            active.display(),
            newest.display()
        )
    })
}

fn discover_ledger_sources(active: &Path) -> anyhow::Result<Vec<LedgerSource>> {
    let parent = ledger_parent(active)?;
    let file_name = active
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("ledger path has no file name: {}", active.display()))?
        .to_string_lossy()
        .into_owned();
    let rotated_prefix = format!("{file_name}.");
    let mut sources = Vec::new();
    match fs::read_dir(parent) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.with_context(|| format!("read entry in {}", parent.display()))?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == file_name {
                    sources.push(LedgerSource {
                        path: entry.path(),
                        suffix: None,
                    });
                } else if let Some(suffix) = name.strip_prefix(&rotated_prefix)
                    && let Ok(index) = suffix.parse::<usize>()
                    && index > 0
                {
                    sources.push(LedgerSource {
                        path: entry.path(),
                        suffix: Some(index),
                    });
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", parent.display())),
    }
    sources.sort_by(|left, right| match (left.suffix, right.suffix) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    Ok(sources)
}

pub(crate) fn lifecycle_ledger_paths_oldest_first(active: &Path) -> anyhow::Result<Vec<PathBuf>> {
    discover_ledger_sources(active)
        .map(|sources| sources.into_iter().map(|source| source.path).collect())
}

/// The most recent tool event durably recorded in the tool-event ledger, read
/// newest segment first.
///
/// This replaces reading `daemon-tool-last.json`. The pointer file was written
/// *after* the ledger append of the identical record, so the ledger is the
/// earlier and therefore never-staler of the two; deriving from it removes a
/// dual write rather than trading one source of truth for another.
///
/// A line that does not parse as a [`ToolEvent`] is skipped and counted, not
/// treated as end-of-ledger: a torn tail from a hard kill is exactly the
/// condition this function exists to survive, and silently reporting "no
/// previous tool call" for a crashed run would erase the forensic signal.
fn last_tool_event_from_ledger(active: &Path) -> anyhow::Result<Option<ToolEvent>> {
    let segments = lifecycle_ledger_paths_oldest_first(active)
        .with_context(|| format!("discover tool-event ledger segments {}", active.display()))?;
    let mut unparsable_lines = 0_u64;
    for path in segments.iter().rev() {
        let lines = match File::open(path).map(BufReader::new) {
            Ok(reader) => reader
                .lines()
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| format!("read tool-event ledger segment {}", path.display()))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("open tool-event ledger segment {}", path.display())));
            }
        };
        for line in lines.into_iter().rev() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ToolEvent>(&line) {
                Ok(event) => {
                    if unparsable_lines > 0 {
                        tracing::warn!(
                            code = "MCP_DAEMON_LIFECYCLE_LEDGER_TAIL_UNPARSABLE",
                            path = %active.display(),
                            unparsable_lines,
                            recovered_seq = event.seq,
                            "skipped unparsable trailing tool-event ledger lines before recovering the last durable tool event"
                        );
                    }
                    return Ok(Some(event));
                }
                Err(_) => unparsable_lines = unparsable_lines.saturating_add(1),
            }
        }
    }
    if unparsable_lines > 0 {
        tracing::warn!(
            code = "MCP_DAEMON_LIFECYCLE_LEDGER_TAIL_UNPARSABLE",
            path = %active.display(),
            unparsable_lines,
            "tool-event ledger held no parsable tool event"
        );
    }
    Ok(None)
}

/// Delete the retired `daemon-tool-last.json` pointer if a previous build left
/// one behind.
///
/// It is removed rather than ignored: nothing writes it any more, so a file
/// left on disk is a record frozen at the moment of the upgrade that a future
/// reader could mistake for current state. Failure to remove it is logged and
/// not fatal — it is stale bytes nothing reads, so refusing to start the daemon
/// over it would be a worse outcome than the warning.
fn retire_legacy_tool_last_pointer(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => tracing::info!(
            code = "MCP_DAEMON_LIFECYCLE_LEGACY_TOOL_LAST_RETIRED",
            path = %path.display(),
            "removed the retired daemon-tool-last.json pointer; the tool-event ledger is now the only record of the last tool event"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            code = "MCP_DAEMON_LIFECYCLE_LEGACY_TOOL_LAST_RETIRE_FAILED",
            path = %path.display(),
            error = %error,
            "could not remove the retired daemon-tool-last.json pointer; it is no longer written or read, so it holds bytes frozen at the moment of this upgrade"
        ),
    }
}

fn ledger_diagnostic_value(
    active: &Path,
    max_segment_bytes: u64,
    ledger_name: &'static str,
) -> Value {
    match ledger_segment_values(active, max_segment_bytes) {
        Ok((segments, total_bytes, oversized_segment_count)) => json!({
            "status": "ok",
            "ledger": ledger_name,
            "active_path": active.display().to_string(),
            "max_segment_bytes": max_segment_bytes,
            "max_segments": MAX_LEDGER_SEGMENTS,
            "max_retained_files": MAX_RETAINED_LEDGER_FILES,
            "active_bytes": segments
                .iter()
                .find(|segment| segment.get("suffix").is_none_or(Value::is_null))
                .and_then(|segment| segment.get("bytes"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "rotated_segment_count": segments
                .iter()
                .filter(|segment| !segment.get("suffix").is_none_or(Value::is_null))
                .count(),
            "segment_count": segments.len(),
            "total_bytes": total_bytes,
            "oversized_segment_count": oversized_segment_count,
            "segments": segments,
        }),
        Err(error) => json!({
            "status": "error",
            "ledger": ledger_name,
            "active_path": active.display().to_string(),
            "max_segment_bytes": max_segment_bytes,
            "detail": format!("{error:#}"),
        }),
    }
}

fn ledger_summary_for_health(active: &Path, max_segment_bytes: u64) -> String {
    match ledger_segment_values(active, max_segment_bytes) {
        Ok((segments, total_bytes, oversized_segment_count)) => {
            let active_bytes = segments
                .iter()
                .find(|segment| segment.get("suffix").is_none_or(Value::is_null))
                .and_then(|segment| segment.get("bytes"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            format!(
                "active_bytes:{active_bytes},segments:{},total_bytes:{total_bytes},oversized_segments:{oversized_segment_count},max_segment_bytes:{max_segment_bytes}",
                segments.len()
            )
        }
        Err(error) => format!("error:{error:#}"),
    }
}

fn ledger_segment_values(
    active: &Path,
    max_segment_bytes: u64,
) -> anyhow::Result<(Vec<Value>, u64, usize)> {
    let mut sources = discover_ledger_sources(active)?;
    sources.sort_by(|left, right| match (left.suffix, right.suffix) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => left.cmp(&right),
    });
    let mut total_bytes = 0_u64;
    let mut oversized_segment_count = 0_usize;
    let mut values = Vec::with_capacity(sources.len());
    for source in sources {
        let bytes = fs::metadata(&source.path)
            .with_context(|| format!("stat lifecycle ledger segment {}", source.path.display()))?
            .len();
        total_bytes = total_bytes.saturating_add(bytes);
        let oversized = bytes > max_segment_bytes;
        if oversized {
            oversized_segment_count = oversized_segment_count.saturating_add(1);
        }
        values.push(json!({
            "path": source.path.display().to_string(),
            "role": if source.suffix.is_some() { "rotated" } else { "active" },
            "suffix": source.suffix,
            "bytes": bytes,
            "oversized": oversized,
        }));
    }
    Ok((values, total_bytes, oversized_segment_count))
}

/// Build the path of rotated segment `index` for `active` by appending
/// `.{index}` to the active file name (e.g. `daemon-tool-events.jsonl.1`).
fn segment_path(active: &Path, index: usize) -> PathBuf {
    let mut name = active
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".{index}"));
    active.with_file_name(name)
}

fn configured_max_segment_bytes() -> u64 {
    MAX_LEDGER_SEGMENT_BYTES
}

fn state_slot() -> &'static Mutex<Option<DaemonLifecycleState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn health_detail_for_state(state: &DaemonLifecycleState) -> String {
    let last_error = state
        .last_error
        .as_deref()
        .map_or_else(|| "none".to_owned(), ToOwned::to_owned);
    let tool_ledger = ledger_summary_for_health(
        Path::new(&state.paths.tool_events_path),
        state.max_segment_bytes,
    );
    let exit_ledger = ledger_summary_for_health(
        Path::new(&state.paths.exit_events_path),
        state.max_segment_bytes,
    );
    // #2083: `previous_shutdown` rides on /health so an operator (or the
    // -Start path) can prove a stop was clean without opening the vault.
    let previous_shutdown = state
        .run
        .previous_shutdown
        .as_deref()
        .unwrap_or("unrecorded");
    let previous_run_id = state.run.previous_run_id.as_deref().unwrap_or("none");
    let previous_ended_reason = state.run.previous_ended_reason.as_deref().unwrap_or("none");
    // #2100: an `interrupted_graceful` verdict is only actionable with the
    // phase-one evidence beside it, so /health carries the marker too.
    let previous_ending_reason = state
        .run
        .previous_ending_reason
        .as_deref()
        .unwrap_or("none");
    let previous_ending_phase = state.run.previous_ending_phase.as_deref().unwrap_or("none");
    // #2131: the rollup and the evidence it was derived from travel together.
    // The detail is quoted because it contains spaces; `previous_shutdown=` is
    // still a single token so the -Start readback regex keeps working.
    let previous_shutdown_detail = state
        .run
        .previous_shutdown_detail
        .as_deref()
        .unwrap_or("unrecorded")
        .replace('"', "'");
    format!(
        "run_id={} pid={} run_current_path={} tool_last_path={} tool_events_path={} exit_events_path={} in_flight_count={} tool_ledger={} exit_ledger={} previous_shutdown={} previous_run_id={} previous_ended_reason={} previous_ending_reason={} previous_ending_phase={} previous_shutdown_detail=\"{}\" last_error={}",
        state.run.run_id,
        state.run.pid,
        state.paths.run_current_path,
        state.paths.tool_last_path,
        state.paths.tool_events_path,
        state.paths.exit_events_path,
        state.in_flight.len(),
        tool_ledger,
        exit_ledger,
        previous_shutdown,
        previous_run_id,
        previous_ended_reason,
        previous_ending_reason,
        previous_ending_phase,
        previous_shutdown_detail,
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

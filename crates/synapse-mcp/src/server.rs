use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::FutureExt as _;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use synapse_action::{ActionHandle, ActionStateSnapshot, RecordingBackend};
use synapse_core::{Health, SubsystemHealth, error_codes, types::TimelineActor};
use tokio::sync::{Notify, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::{
    http::sse::SseState,
    m1::{
        BrowserAddInitScriptParams, BrowserAddInitScriptResponse, BrowserAddScriptTagParams,
        BrowserAddStyleTagParams, BrowserAddTagResponse, BrowserAdoptActiveTabParams,
        BrowserAdoptActiveTabResponse, BrowserBindingCall, BrowserConsoleMessagesParams,
        BrowserConsoleMessagesResponse, BrowserContentParams, BrowserContentResponse,
        BrowserDownloadEntry, BrowserDownloadEvent, BrowserDownloadsOperation,
        BrowserDownloadsParams, BrowserDownloadsResponse, BrowserEvaluateParams,
        BrowserEvaluateResponse, BrowserExposeBindingOperation, BrowserExposeBindingParams,
        BrowserExposeBindingResponse, BrowserFrameLocator, BrowserInitScriptOperation,
        BrowserInspectParams, BrowserInspectResponse, BrowserLayoutRelation, BrowserLocateEngine,
        BrowserLocateParams, BrowserLocateResponse, BrowserLocatedFrame, BrowserNavOperation,
        BrowserNavParams, BrowserNavResponse, BrowserNetworkWaitEntry, BrowserPdfParams,
        BrowserPdfResponse, BrowserScreenshotParams, BrowserScreenshotResponse,
        BrowserScreenshotScope, BrowserScrollIntoViewParams, BrowserScrollIntoViewResponse,
        BrowserSetContentParams, BrowserSetContentResponse, BrowserTabEntry, BrowserTabsParams,
        BrowserTabsResponse, BrowserWaitConditionKind, BrowserWaitForFunctionParams,
        BrowserWaitForFunctionResponse, BrowserWaitForLoadStateParams,
        BrowserWaitForLoadStateResponse, BrowserWaitForLoadStateState,
        BrowserWaitForNetworkResponseParams, BrowserWaitForNetworkResponseResponse,
        BrowserWaitForParams, BrowserWaitForRequestParams, BrowserWaitForRequestResponse,
        BrowserWaitForResponse, BrowserWaitForSelectorParams, BrowserWaitForSelectorResponse,
        BrowserWaitForSelectorState, BrowserWaitForState, BrowserWaitForUrlMatchKind,
        BrowserWaitForUrlParams, BrowserWaitForUrlResponse, BrowserWaitParams, BrowserWaitResponse,
        CaptureGifParams, CaptureGifResponse, CaptureScreenshotFormat, CaptureScreenshotParams,
        CaptureScreenshotResponse, CdpActivateTabParams, CdpActivateTabResponse,
        CdpActiveElementInfo, CdpBridgeHostReadback, CdpBridgeReloadAckReadback,
        CdpBridgeReloadParams, CdpBridgeReloadResponse, CdpCloseTabParams, CdpCloseTabResponse,
        CdpLargestContentfulPaintInfo, CdpNavigateAction, CdpNavigateTabParams,
        CdpNavigateTabResponse, CdpOpenTabParams, CdpOpenTabResponse, CdpPageTextInfo,
        CdpPageVitalsInfo, CdpTargetInfoParams, CdpTargetInfoResponse, ConsoleMessage,
        ElementInspection, FindParams, FindResponse, HiddenDesktopPipFrameParams,
        HiddenDesktopPipFrameResponse, HiddenDesktopPipStreamStatus, M1State, ObserveParams,
        ReadTextParams, ScreenshotOperation, ScreenshotParams, ScreenshotResponse,
        SetCaptureTargetParams, SetCaptureTargetResponse, SetPerceptionModeParams,
        SetPerceptionModeResponse, SetTargetParam, SetTargetParams, SharedM1State, TargetResponse,
        TargetWire, WindowListEntry, WindowListParams, WindowListResponse,
        apply_profile_runtime_config_in_state, build_find_input, current_input, empty_input_schema,
        enrich_input_with_browser_ocr, enrich_input_with_cdp_for_target, find_cdp_max_nodes,
        find_snapshot_depth, match_find_input, mcp_error, observe_include, observe_input,
        populate_clipboard_summary, populate_detection_from_state, populate_fs_recent,
        read_text_request_uncached, resolve_read_text_request, set_capture_target_in_state,
        set_perception_mode_in_state, set_target_input_schema,
    },
    m2::{
        ActClickParams, ActClickResponse, ActClipboardParams, ActClipboardResponse,
        ActFocusWindowParams, ActFocusWindowResponse, ActKeymapParams, ActKeymapResponse,
        ActPadParams, ActPadResponse, ActPressParams, ActPressResponse, ActScrollParams,
        ActScrollResponse, ActSetValueParams, ActSetValueResponse, ActStrokeParams,
        ActStrokeResponse, ActTypeParams, ActTypeResponse, M2ServiceConfig, ReleaseAllParams,
        ReleaseAllResponse, SharedM2State, SharedSessionClipboardBuffers,
        act_click_with_handle_and_lease, act_clipboard_session_buffer,
        act_focus_window_request_details, act_focus_window_target_hwnd,
        act_set_value_request_details, act_stroke_validation_failure_details,
        new_session_clipboards, release_all_with_handles,
        shared_m2_state_from_config_with_shutdown_reason, shared_m2_state_from_env,
        validate_act_stroke_params,
    },
    m3::{
        M3ServiceConfig, SharedM3State,
        activity_recorder::BrowserNavigationEvent,
        approvals::{
            ApprovalDecideParams, ApprovalDecideResponse, ApprovalListParams, ApprovalListResponse,
            ApprovalRequestParams, ApprovalRequestResponse, ApprovalToastDelivery, decide_approval,
            list_approvals, prepare_activation_links, request_approval,
            update_approval_toast_state,
        },
        audio::{
            AudioTailParams, AudioTailResponse, AudioTranscribeParams, AudioTranscribeResponse,
            populate_audio_summary, tail_audio, transcribe_audio,
        },
        audit_export::{AuditExportBundleParams, AuditExportBundleResponse, export_audit_bundle},
        demo_recording::{
            DemoRecordStartParams, DemoRecordStartResponse, DemoRecordStopParams,
            DemoRecordStopResponse, start_demo_recording, stop_demo_recording,
        },
        episodes::{
            EpisodeGetParams, EpisodeGetResponse, EpisodeListParams, EpisodeListResponse,
            EpisodeSegmentParams, EpisodeSegmentResponse, get_episode, list_episodes,
            segment_episodes,
        },
        hygiene::{
            HygieneFlagsParams, HygieneFlagsResponse, HygieneScanStorageParams,
            HygieneScanStorageResponse, HygieneScanTextParams, HygieneScanTextResponse,
            query_flags, scan_storage, scan_text_tool,
        },
        local_models::{
            LocalModelListParams, LocalModelListResponse, LocalModelProbeParams,
            LocalModelProbeResponse, LocalModelRegisterParams, LocalModelRegisterResponse,
            LocalModelRemoveParams, LocalModelRemoveResponse, LocalModelUpdateParams,
            LocalModelUpdateResponse, list_local_models, probe_local_model, register_local_model,
            remove_local_model, update_local_model,
        },
        permissions::{RequiredPermissions, authorization_error},
        profile::{
            ProfileActivateParams, ProfileActivateResponse, ProfileListParams, ProfileListResponse,
            activate_profile, list_profiles,
        },
        profile_authoring::{
            ProfileAuthoringDecideParams, ProfileAuthoringDecideResponse,
            ProfileAuthoringExportParams, ProfileAuthoringExportResponse,
            ProfileAuthoringGenerateParams, ProfileAuthoringGenerateResponse,
            ProfileAuthoringInspectParams, ProfileAuthoringInspectResponse,
            ProfileAuthoringListParams, ProfileAuthoringListResponse, RoutineAutomateParams,
            RoutineAutomateResponse, decide_profile_authoring_candidate,
            export_profile_authoring_candidate, generate_profile_authoring_candidate,
            generate_routine_automation_candidate, inspect_profile_authoring_candidate,
            list_profile_authoring_candidates,
        },
        profile_quality::{
            ProfileQualityRefreshParams, ProfileQualityRefreshResponse, refresh_profile_quality,
        },
        profile_registry::{
            AuditIntelligenceQueryParams, AuditIntelligenceQueryResponse,
            ProfileRegistryDisableParams, ProfileRegistryDisableResponse,
            ProfileRegistryExportParams, ProfileRegistryExportResponse,
            ProfileRegistryImportParams, ProfileRegistryImportResponse,
            ProfileRegistryInstallParams, ProfileRegistryInstallResponse,
            ProfileRegistryQueryParams, ProfileRegistryQueryResponse,
            ProfileRegistryRollbackParams, ProfileRegistryRollbackResponse,
            disable_registry_profile, export_registry, import_registry, install_registry_package,
            query_audit_intelligence, query_registry, rollback_registry_profile,
        },
        reflex::{
            ReflexCancelParams, ReflexCancelResponse, ReflexHistoryParams, ReflexHistoryResponse,
            ReflexListParams, ReflexListResponse, ReflexRegisterParams, ReflexRegisterResponse,
            cancel_file_jsonl_tail_watcher, cancel_reflex, history_reflexes,
            install_file_jsonl_tail_watcher, list_reflexes, register_reflex,
        },
        replay::{ReplayRecordParams, ReplayRecordResponse, record_replay},
        routines::{
            RoutineInspectParams, RoutineInspectResponse, RoutineListParams, RoutineListResponse,
            RoutineMineParams, RoutineMineResponse, RoutineUpdateParams, RoutineUpdateResponse,
            inspect_routine, list_routines, mine_and_store_routines, update_routine,
        },
        shared_m3_state_from_config_with_shutdown_reason_and_sse_state, shared_m3_state_from_env,
        storage::{
            StorageGcOnceParams, StorageGcOnceResponse, StorageInspectParams,
            StorageInspectResponse, StoragePressureSampleParams, StoragePressureSampleResponse,
            StoragePutProbeRowsParams, StoragePutProbeRowsResponse, apply_storage_pressure_sample,
            inspect_storage, put_probe_rows, run_storage_gc_once,
        },
        subscribe::{
            SubscribeCancelParams, SubscribeCancelResponse, SubscribeParams, SubscribeResponse,
            cancel_subscription, subscribe_to_events,
        },
        timeline::{
            TimelinePurgeParams, TimelinePurgeResponse, TimelineSearchParams,
            TimelineSearchResponse, purge_timeline, search_timeline,
        },
        timeline_control::{
            TimelineExclusionsParams, TimelineExclusionsResponse, TimelinePauseParams,
            TimelinePauseResponse, TimelineResumeParams, TimelineResumeResponse, pause_timeline,
            resume_timeline, update_timeline_exclusions,
        },
    },
    m4::{
        ActComboParams, ActComboResponse, ActLaunchParams, ActLaunchResponse,
        ActRunShellCancelResponse, ActRunShellJobIdParams, ActRunShellParams, ActRunShellResponse,
        ActRunShellStartParams, ActRunShellStartResponse, ActRunShellStatusParams,
        ActRunShellStatusResponse, ActSpawnAgentCli, ActSpawnAgentLogPaths, ActSpawnAgentParams,
        ActSpawnAgentRequest, ActSpawnAgentResponse, ActSpawnAgentTarget,
        AgentSpawnTaskStartedParams, AgentSpawnTaskStartedResponse, LaunchWindowState,
        M4ServiceConfig, MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS, RunShellAuthorization,
        ShellExecutionContext, assign_owned_process_job, authorize_run_shell,
        authorize_run_shell_start, cancel_shell_job, execute_combo_with_boundary,
        launch_for_session_with_boundary, launch_process_history_row,
        launch_process_history_row_key, launch_request_details,
        prepare_run_shell_params_for_context, prepare_run_shell_start_params_for_context,
        required_combo_permissions, run_authorized_shell_with_boundary,
        run_shell_idempotency_completed_row, run_shell_idempotency_replay,
        run_shell_idempotency_reservation_row, run_shell_idempotency_row_key,
        run_shell_request_details, run_shell_start_request_details,
        shell_execution_context_for_session, shell_job_status,
        start_authorized_shell_job_with_boundary, validate_agent_spawn_params,
        validate_run_shell_execution_plan,
    },
};

mod action_audit;
mod action_preflight;
pub(crate) mod agent_control;
pub(crate) mod agent_cost;
pub(crate) mod agent_event_ingress;
pub(crate) mod agent_events;
mod agent_facades;
mod agent_mailbox;
pub(crate) mod agent_query;
pub(crate) mod agent_state;
pub(crate) mod agent_stats;
pub(crate) mod agent_tasks;
pub(crate) mod agent_templates;
pub(crate) mod agent_transcripts;
pub(crate) mod ambient_agents;
mod approval_facades;
mod audit_context;
mod audit_replay_facades;
pub(crate) mod codex_app_server_bridge;
pub(crate) mod command_audit;
mod context;
pub(crate) use context::AgentTranscriptSnapshotRow;
pub(crate) use context::{
    APPROVAL_DECISION_EVENT_KIND, APPROVAL_REQUEST_EVENT_KIND, APPROVAL_TIMEOUT_EVENT_KIND,
};
mod background_router;
mod browser_assert;
mod browser_batch;
mod browser_clock_events;
mod browser_dialog;
mod browser_dnd;
mod browser_emulate;
mod browser_facades;
mod browser_field;
mod browser_files;
mod browser_frames;
mod browser_network;
mod browser_storage;
mod capture_gif;
mod data_cleaning;
pub(crate) mod drain;
pub(crate) mod escalation;
mod handler;
mod health;
pub(crate) use health::HealthParams;
mod hygiene_report;
mod intent_tools;
mod lease_tools;
mod m1_tools;
mod m2_tools;
mod m3_tools;
pub(crate) mod m4_tools;
mod notify_tools;
mod operational_facades;
pub(crate) mod operator_panic_boundary;
mod param_hints;
mod permission_gate;
pub(crate) mod permission_policy;
mod plan_tools;
mod reality;
mod routine_assist_facades;
mod routine_feedback;
mod routine_labeling;
mod schema_sanitize;
pub(crate) mod session_continuity;
pub(crate) mod session_lifecycle;
pub(crate) mod session_registry;
mod session_tools;
pub(crate) mod suggestions;
pub(crate) mod target_claims;
mod target_policy;
pub(crate) mod terminal_capture;
#[cfg(test)]
mod tests;
pub(crate) mod timeline_digest;
mod timeline_facades;
mod timeline_query;
mod tool_profiles;
pub(crate) mod url_redaction;
mod verification;
mod workspace_blackboard;

use session_registry::{SessionRegistry, SharedSessionRegistry};
use target_claims::SharedTargetClaims;

/// A single MCP session's active perception target (epic #720). When set,
/// `observe`/`find`/`read_text`/`capture_screenshot` perceive this target
/// instead of the global foreground, so many agents observe different windows
/// or browser tabs concurrently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SessionTarget {
    Window {
        hwnd: i64,
    },
    Cdp {
        window_hwnd: i64,
        cdp_target_id: String,
    },
}

/// Per-session active-target registry keyed by `Mcp-Session-Id`. A small mutex
/// held only to clone an entry out — never across a perception `.await` — so
/// target reads never serialize behind another session's snapshot.
pub(crate) type SharedSessionTargets = Arc<Mutex<HashMap<String, SessionTarget>>>;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct CdpTargetOwner {
    pub session_id: String,
    pub window_hwnd: i64,
    pub endpoint: String,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub capture_window_hwnd: Option<i64>,
    pub cdp_target_id: String,
    pub requested_url: String,
    pub target_url: String,
    pub created_at_unix_ms: u64,
}

/// Per-CDP-target ownership registry keyed by browser surface + `TargetID`.
/// Only the creating MCP session may close a registered target; unowned targets
/// may be observed by explicit `set_target` but are never closed by Synapse.
pub(crate) type SharedCdpTargetOwners = Arc<Mutex<HashMap<String, CdpTargetOwner>>>;

/// Leak-safe keyed transaction gates for authority mutations. The registry
/// keeps only weak references and prunes dead entries whenever a gate is
/// resolved, so reconnect/session churn cannot grow it without bound.
type SharedSessionAuthorityGates = Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>;

const AUTHORITY_CANCELLATION_GRACE: Duration = Duration::from_secs(1);
const AUTHORITY_ABORT_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
const AUTHORITY_TASK_PHASE_REGISTERED: u8 = 0;
const AUTHORITY_TASK_PHASE_RUNNING: u8 = 1;
const AUTHORITY_TASK_PHASE_ROLLBACK_COMPLETE: u8 = 2;
const AUTHORITY_TASK_PHASE_COMPLETED: u8 = 3;
const AUTHORITY_TASK_PHASE_PANICKED: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorityCancellationPolicy {
    /// The complete future owns only rollback-safe destructors, so shutdown
    /// may drop it and then reap the exact Tokio task.
    DropFutureAfterSignal,
    /// The future owns storage-backed rollback/final-audit state. Shutdown may
    /// signal it, but only that exact owner may drive the state to terminal
    /// readback; the supervisor must retain it rather than drop or abort it.
    CooperativeTerminalReadback,
}

impl AuthorityCancellationPolicy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DropFutureAfterSignal => "drop_future_after_signal",
            Self::CooperativeTerminalReadback => "cooperative_terminal_readback",
        }
    }
}

#[derive(Clone, Debug)]
struct AuthorityTaskControl {
    abort_handle: tokio::task::AbortHandle,
    cancellation: CancellationToken,
    cancellation_policy: AuthorityCancellationPolicy,
    phase: Arc<AtomicU8>,
}

#[derive(Clone, Debug)]
enum AuthorityTaskExit {
    Completed,
    CancelledAfterRollback,
    Panicked(String),
    StartGateClosed,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthorityTransactionJoinError {
    kind: AuthorityTransactionJoinErrorKind,
    detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorityTransactionJoinErrorKind {
    Cancelled,
    Panicked,
}

impl AuthorityTransactionJoinError {
    pub(crate) const fn is_cancelled(&self) -> bool {
        matches!(self.kind, AuthorityTransactionJoinErrorKind::Cancelled)
    }

    pub(crate) const fn is_panic(&self) -> bool {
        matches!(self.kind, AuthorityTransactionJoinErrorKind::Panicked)
    }

    fn cancelled(detail: impl Into<String>) -> Self {
        Self {
            kind: AuthorityTransactionJoinErrorKind::Cancelled,
            detail: detail.into(),
        }
    }

    fn panicked(detail: impl Into<String>) -> Self {
        Self {
            kind: AuthorityTransactionJoinErrorKind::Panicked,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for AuthorityTransactionJoinError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for AuthorityTransactionJoinError {}

#[derive(Clone)]
struct AuthorityFinalizerSupervisor {
    /// Tracks the exact join-owner task for each authority transaction. The
    /// join owner does not finish until it has awaited the transaction's Tokio
    /// JoinHandle and persisted the outcome in `join_failures`.
    tasks: TaskTracker,
    admission_closed: Arc<Mutex<bool>>,
    transactions: Arc<Mutex<BTreeMap<u64, AuthorityTaskControl>>>,
    join_failures: Arc<Mutex<Vec<String>>>,
    next_task_id: Arc<AtomicU64>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AuthorityFinalizerDrainReadback {
    pub(crate) admission_closed: bool,
    pub(crate) tracker_closed: bool,
    pub(crate) registered_tasks_before: usize,
    pub(crate) cancellation_signals_sent: usize,
    pub(crate) abort_requests_sent: usize,
    pub(crate) registered_tasks_after: usize,
    pub(crate) tracked_tasks_after: usize,
}

impl AuthorityFinalizerDrainReadback {
    /// Physical lifetime locks may be released only after both independent
    /// ownership Sources of Truth agree that no authority transaction remains.
    /// Other drain failures still make shutdown fail, but they do not require
    /// retaining the lifetime locks once ownership is proven empty.
    pub(crate) const fn safe_to_unlock(&self) -> bool {
        self.admission_closed
            && self.tracker_closed
            && self.registered_tasks_after == 0
            && self.tracked_tasks_after == 0
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AuthorityFinalizerDrainFailure {
    pub(crate) readback: AuthorityFinalizerDrainReadback,
    detail: String,
}

impl std::fmt::Display for AuthorityFinalizerDrainFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}; readback={:?}", self.detail, self.readback)
    }
}

impl std::error::Error for AuthorityFinalizerDrainFailure {}

fn authority_task_phase_name(phase: u8) -> &'static str {
    match phase {
        AUTHORITY_TASK_PHASE_REGISTERED => "registered",
        AUTHORITY_TASK_PHASE_RUNNING => "running",
        AUTHORITY_TASK_PHASE_ROLLBACK_COMPLETE => "rollback_complete",
        AUTHORITY_TASK_PHASE_COMPLETED => "completed",
        AUTHORITY_TASK_PHASE_PANICKED => "panicked",
        _ => "unknown",
    }
}

fn drop_authority_owned<T>(owned: T) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(owned)))
        .map_err(crate::daemon_lifecycle::consume_panic_payload)
}

fn transactions_phase(
    transactions: &Arc<Mutex<BTreeMap<u64, AuthorityTaskControl>>>,
    task_id: u64,
) -> &'static str {
    match transactions.lock() {
        Ok(transactions) => transactions
            .get(&task_id)
            .map_or("registry_entry_missing", |control| {
                authority_task_phase_name(control.phase.load(Ordering::Acquire))
            }),
        Err(poisoned) => poisoned
            .get_ref()
            .get(&task_id)
            .map_or("registry_poisoned_entry_missing", |control| {
                authority_task_phase_name(control.phase.load(Ordering::Acquire))
            }),
    }
}

fn push_authority_join_failure(failures: &Arc<Mutex<Vec<String>>>, failure: String) {
    match failures.lock() {
        Ok(mut failures) => failures.push(failure),
        Err(poisoned) => {
            let mut failures = poisoned.into_inner();
            failures.push("authority transaction join-failure ledger lock poisoned".to_owned());
            failures.push(failure);
        }
    }
}

fn snapshot_authority_controls(
    transactions: &Arc<Mutex<BTreeMap<u64, AuthorityTaskControl>>>,
    errors: &mut Vec<String>,
) -> (usize, Vec<(u64, AuthorityTaskControl)>) {
    match transactions.lock() {
        Ok(transactions) => (
            transactions.len(),
            transactions
                .iter()
                .map(|(task_id, control)| (*task_id, control.clone()))
                .collect(),
        ),
        Err(poisoned) => {
            errors.push(
                "authority transaction registry lock poisoned during shutdown snapshot".to_owned(),
            );
            let transactions = poisoned.into_inner();
            (
                transactions.len(),
                transactions
                    .iter()
                    .map(|(task_id, control)| (*task_id, control.clone()))
                    .collect(),
            )
        }
    }
}

fn authority_registry_len(
    transactions: &Arc<Mutex<BTreeMap<u64, AuthorityTaskControl>>>,
    errors: &mut Vec<String>,
    phase: &'static str,
) -> usize {
    match transactions.lock() {
        Ok(transactions) => transactions.len(),
        Err(poisoned) => {
            errors.push(format!(
                "authority transaction registry lock poisoned during {phase}"
            ));
            poisoned.into_inner().len()
        }
    }
}

fn take_authority_join_failures(failures: &Arc<Mutex<Vec<String>>>, errors: &mut Vec<String>) {
    match failures.lock() {
        Ok(mut failures) => errors.extend(failures.drain(..)),
        Err(poisoned) => {
            errors.push(
                "authority transaction join-failure ledger lock poisoned during shutdown readback"
                    .to_owned(),
            );
            errors.extend(poisoned.into_inner().drain(..));
        }
    }
}

impl Default for AuthorityFinalizerSupervisor {
    fn default() -> Self {
        Self {
            tasks: TaskTracker::new(),
            admission_closed: Arc::new(Mutex::new(false)),
            transactions: Arc::new(Mutex::new(BTreeMap::new())),
            join_failures: Arc::new(Mutex::new(Vec::new())),
            next_task_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl std::fmt::Debug for AuthorityFinalizerSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorityFinalizerSupervisor")
            .field("task_count", &self.tasks.len())
            .field(
                "registered_transaction_count",
                &self
                    .transactions
                    .lock()
                    .map_or(usize::MAX, |tasks| tasks.len()),
            )
            .field(
                "admission_closed",
                &self
                    .admission_closed
                    .lock()
                    .map_or(None, |closed| Some(*closed)),
            )
            .field(
                "join_failure_count",
                &self
                    .join_failures
                    .lock()
                    .map_or(usize::MAX, |failures| failures.len()),
            )
            .field("next_task_id", &self.next_task_id.load(Ordering::SeqCst))
            .field("closed", &self.tasks.is_closed())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct SynapseService {
    started_at: Instant,
    tool_router: ToolRouter<Self>,
    m1_state: SharedM1State,
    m2_state: SharedM2State,
    m3_state: SharedM3State,
    m4_config: M4ServiceConfig,
    drain_state: drain::DaemonDrainState,
    session_targets: SharedSessionTargets,
    cdp_target_owners: SharedCdpTargetOwners,
    session_clipboards: SharedSessionClipboardBuffers,
    session_registry: SharedSessionRegistry,
    mailbox_notify: Arc<Notify>,
    target_claims: SharedTargetClaims,
    session_authority_gates: SharedSessionAuthorityGates,
    authority_finalizers: AuthorityFinalizerSupervisor,
    session_processes: session_lifecycle::SharedSessionProcessResources,
    terminated_sessions: session_lifecycle::SharedTerminatedSessions,
}

fn install_chrome_browser_navigation_sink(m3_state: &SharedM3State) {
    // The process-global bridge callback must never become a hidden lifetime
    // owner of the daemon DB. A failed startup or completed shutdown drops the
    // last strong M3 owner; later bridge events observe that physical state
    // instead of keeping RocksDB alive past the daemon locks.
    let m3_state = Arc::downgrade(m3_state);
    crate::chrome_debugger_bridge::set_browser_navigation_sink(Arc::new(move |event| {
        let Some(m3_state) = m3_state.upgrade() else {
            tracing::warn!(
                code = "TIMELINE_BROWSER_NAV_SINK_OWNER_GONE",
                "browser-navigation event arrived after the daemon M3 owner was released"
            );
            return;
        };
        let recorder = match m3_state.lock() {
            Ok(state) => state.activity_recorder.clone(),
            Err(_error) => {
                tracing::error!(
                    code = "TIMELINE_BROWSER_NAV_M3_LOCK_POISONED",
                    "M3 service state lock poisoned while recording Chrome browser navigation"
                );
                return;
            }
        };
        if let Some(recorder) = recorder {
            let _ = recorder.record_browser_navigation(BrowserNavigationEvent {
                actor: TimelineActor::Human,
                app: Some("chrome.exe".to_owned()),
                source: event.source,
                event: event.event,
                action: None,
                url: event.url,
                title: event.title,
                tab_id: event.tab_id,
                chrome_window_id: event.chrome_window_id,
                window_hwnd: None,
                cdp_target_id: event.cdp_target_id,
                endpoint: event.endpoint,
                transport: event.transport,
                requested_url: None,
                before_url: None,
                before_title: None,
                ready_state: event.ready_state,
                observed_at_unix_ms: event.observed_at_unix_ms,
                active: event.active,
                highlighted: event.highlighted,
                pinned: event.pinned,
            });
        } else {
            tracing::error!(
                code = "TIMELINE_BROWSER_NAV_RECORDER_MISSING",
                "Chrome browser navigation event arrived before the activity recorder was available"
            );
        }
    }));
}

impl SynapseService {
    #[must_use]
    pub fn new() -> Self {
        match Self::try_new() {
            Ok(service) => service,
            Err(error) => panic!("M3 state should initialize from environment: {error:#}"),
        }
    }

    /// Gives each test its own process-global daemon singletons — the input
    /// lease and the agent-state tracker — so parallel tests can never
    /// cross-contaminate one another's `session_list` projections (root cause
    /// of issue #1574). Called from every constructor so no test can forget it;
    /// idempotent per thread and compiled out entirely in production.
    #[cfg(test)]
    fn isolate_process_globals_for_test() {
        synapse_action::lease::isolate_for_test();
        synapse_action::isolate_interrupt_epochs_for_test();
        crate::server::agent_state::isolate_for_test();
    }

    pub fn try_new() -> anyhow::Result<Self> {
        #[cfg(test)]
        Self::isolate_process_globals_for_test();
        let m3_state = shared_m3_state_from_env()?;
        install_chrome_browser_navigation_sink(&m3_state);
        Ok(Self {
            started_at: Instant::now(),
            tool_router: Self::tool_router(),
            m1_state: SharedM1State::default(),
            m2_state: shared_m2_state_from_env()?,
            m3_state,
            m4_config: M4ServiceConfig::from_env()?,
            drain_state: drain::DaemonDrainState::default(),
            session_targets: Arc::new(Mutex::new(HashMap::new())),
            cdp_target_owners: Arc::new(Mutex::new(HashMap::new())),
            session_clipboards: new_session_clipboards(),
            session_registry: Arc::new(Mutex::new(SessionRegistry::default())),
            mailbox_notify: Arc::new(Notify::new()),
            target_claims: Arc::new(Mutex::new(target_claims::TargetClaimRegistry::default())),
            session_authority_gates: Arc::new(Mutex::new(HashMap::new())),
            authority_finalizers: AuthorityFinalizerSupervisor::default(),
            session_processes: Arc::new(Mutex::new(BTreeMap::new())),
            terminated_sessions: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    pub fn try_with_m2_shutdown_reason_and_m3_config(
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: CancellationToken,
        m2_config: &M2ServiceConfig,
        m3_config: M3ServiceConfig,
        m4_config: M4ServiceConfig,
    ) -> anyhow::Result<Self> {
        #[cfg(test)]
        Self::isolate_process_globals_for_test();
        let sse_state = SseState::with_max_subscriptions(m3_config.max_subscriptions);
        let m3_state = shared_m3_state_from_config_with_shutdown_reason_and_sse_state(
            m3_config,
            shutdown_cancel.clone(),
            shutdown_reason,
            Some(connection_closed_cancel.clone()),
            sse_state,
        )?;
        install_chrome_browser_navigation_sink(&m3_state);
        Ok(Self {
            started_at: Instant::now(),
            tool_router: Self::tool_router(),
            m1_state: SharedM1State::default(),
            m2_state: shared_m2_state_from_config_with_shutdown_reason(
                m2_config,
                shutdown_cancel,
                shutdown_reason,
                Some(connection_closed_cancel),
            )?,
            m3_state,
            m4_config,
            drain_state: drain::DaemonDrainState::default(),
            session_targets: Arc::new(Mutex::new(HashMap::new())),
            cdp_target_owners: Arc::new(Mutex::new(HashMap::new())),
            session_clipboards: new_session_clipboards(),
            session_registry: Arc::new(Mutex::new(SessionRegistry::default())),
            mailbox_notify: Arc::new(Notify::new()),
            target_claims: Arc::new(Mutex::new(target_claims::TargetClaimRegistry::default())),
            session_authority_gates: Arc::new(Mutex::new(HashMap::new())),
            authority_finalizers: AuthorityFinalizerSupervisor::default(),
            session_processes: Arc::new(Mutex::new(BTreeMap::new())),
            terminated_sessions: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    pub fn try_with_m2_shutdown_reason_and_sse_state_and_m3_config(
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: CancellationToken,
        sse_state: SseState,
        m2_config: &M2ServiceConfig,
        m3_config: M3ServiceConfig,
        m4_config: M4ServiceConfig,
    ) -> anyhow::Result<Self> {
        #[cfg(test)]
        Self::isolate_process_globals_for_test();
        let m3_state = shared_m3_state_from_config_with_shutdown_reason_and_sse_state(
            m3_config,
            shutdown_cancel.clone(),
            shutdown_reason,
            Some(connection_closed_cancel.clone()),
            sse_state,
        )?;
        install_chrome_browser_navigation_sink(&m3_state);
        Ok(Self {
            started_at: Instant::now(),
            tool_router: Self::tool_router(),
            m1_state: SharedM1State::default(),
            m2_state: shared_m2_state_from_config_with_shutdown_reason(
                m2_config,
                shutdown_cancel,
                shutdown_reason,
                Some(connection_closed_cancel),
            )?,
            m3_state,
            m4_config,
            drain_state: drain::DaemonDrainState::default(),
            session_targets: Arc::new(Mutex::new(HashMap::new())),
            cdp_target_owners: Arc::new(Mutex::new(HashMap::new())),
            session_clipboards: new_session_clipboards(),
            session_registry: Arc::new(Mutex::new(SessionRegistry::default())),
            mailbox_notify: Arc::new(Notify::new()),
            target_claims: Arc::new(Mutex::new(target_claims::TargetClaimRegistry::default())),
            session_authority_gates: Arc::new(Mutex::new(HashMap::new())),
            authority_finalizers: AuthorityFinalizerSupervisor::default(),
            session_processes: Arc::new(Mutex::new(BTreeMap::new())),
            terminated_sessions: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    pub fn m2_emitter_done_receiver(&self) -> Option<watch::Receiver<Option<ActionStateSnapshot>>> {
        self.m2_state
            .lock()
            .ok()
            .and_then(|state| state.emitter_done_receiver())
    }

    pub(crate) fn take_m2_emitter_task(
        &self,
    ) -> Result<Option<tokio::task::JoinHandle<ActionStateSnapshot>>, String> {
        self.m2_state
            .lock()
            .map_err(|_poisoned| {
                "m2 state lock poisoned while taking the daemon emitter task owner".to_owned()
            })
            .map(|mut state| state.take_emitter_task())
    }

    pub(crate) fn m3_state_handle(&self) -> SharedM3State {
        Arc::clone(&self.m3_state)
    }

    pub(crate) fn drain_state_handle(&self) -> drain::DaemonDrainState {
        self.drain_state.clone()
    }

    fn session_authority_gate(
        &self,
        session_id: &str,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, ErrorData> {
        let mut gates = self.session_authority_gates.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session authority gate registry lock poisoned",
            )
        })?;
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(session_id).and_then(Weak::upgrade) {
            return Ok(gate);
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(session_id.to_owned(), Arc::downgrade(&gate));
        Ok(gate)
    }

    /// Serialize every authority mutation for one MCP session. The returned
    /// owned guard is cancellation-safe and does not borrow the registry.
    pub(crate) async fn lock_session_authority(
        &self,
        session_id: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, ErrorData> {
        self.lock_session_authority_after_resolve(session_id, || {})
            .await
    }

    /// Resolve the exact keyed gate, report that admission has reached the
    /// queue boundary, then wait for ownership. The observer makes causal
    /// concurrency tests independent of scheduler timing while production
    /// callers use [`Self::lock_session_authority`].
    pub(crate) async fn lock_session_authority_after_resolve<F>(
        &self,
        session_id: &str,
        on_resolved: F,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, ErrorData>
    where
        F: FnOnce(),
    {
        let gate = self.session_authority_gate(session_id)?;
        on_resolved();
        Ok(gate.lock_owned().await)
    }

    pub(crate) async fn lock_session_authority_for_tool(
        &self,
        tool_name: &str,
        session_id: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, ErrorData> {
        self.lock_session_authority_for_tool_after_resolve(tool_name, session_id, || {})
            .await
    }

    pub(crate) async fn lock_session_authority_for_tool_after_resolve<F>(
        &self,
        tool_name: &str,
        session_id: &str,
        on_resolved: F,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, ErrorData>
    where
        F: FnOnce(),
    {
        let guard = self
            .lock_session_authority_after_resolve(session_id, on_resolved)
            .await?;
        self.reject_terminated_session_tool_call(tool_name, session_id)?;
        Ok(guard)
    }

    /// Lock multiple session authority domains in lexical order. Handoff uses
    /// this to cover both the old and new lease owner without lock inversion.
    pub(crate) async fn lock_session_authorities(
        &self,
        session_ids: &[&str],
    ) -> Result<Vec<tokio::sync::OwnedMutexGuard<()>>, ErrorData> {
        let mut session_ids = session_ids.to_vec();
        session_ids.sort_unstable();
        session_ids.dedup();
        let mut guards = Vec::with_capacity(session_ids.len());
        for session_id in session_ids {
            guards.push(self.lock_session_authority(session_id).await?);
        }
        Ok(guards)
    }

    /// Start one complete authority transaction under daemon supervision. The
    /// caller owns only a result receiver; the supervisor retains the exact
    /// Tokio task join through a tracked join-owner. Dropping the caller keeps
    /// the transaction alive through physical cleanup and durable final audit.
    /// Admission is serialized with shutdown close so no transaction can
    /// appear after a successful empty+closed drain.
    #[allow(dead_code)] // Drop-safe policy seam is exercised by causal shutdown regressions.
    pub(crate) fn spawn_authority_transaction<F>(
        &self,
        future: F,
    ) -> Result<
        impl Future<Output = Result<F::Output, AuthorityTransactionJoinError>> + Send + 'static,
        ErrorData,
    >
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_authority_transaction_with_policy(
            |_cancellation| future,
            AuthorityCancellationPolicy::DropFutureAfterSignal,
        )
    }

    /// Start an authority transaction whose exact async owner must perform
    /// storage-backed rollback and terminal audit after shutdown cancellation.
    /// The factory receives the supervisor's cancellation token. Unlike the
    /// drop-safe variant, shutdown never drops or aborts this future: a task
    /// that does not finish inside the bounded drain remains registered so the
    /// daemon lifetime locks cannot be released over live authority.
    pub(crate) fn spawn_cooperative_authority_transaction<M, F>(
        &self,
        make_future: M,
    ) -> Result<
        impl Future<Output = Result<F::Output, AuthorityTransactionJoinError>> + Send + 'static,
        ErrorData,
    >
    where
        M: FnOnce(CancellationToken) -> F,
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_authority_transaction_with_policy(
            make_future,
            AuthorityCancellationPolicy::CooperativeTerminalReadback,
        )
    }

    fn spawn_authority_transaction_with_policy<M, F>(
        &self,
        make_future: M,
        cancellation_policy: AuthorityCancellationPolicy,
    ) -> Result<
        impl Future<Output = Result<F::Output, AuthorityTransactionJoinError>> + Send + 'static,
        ErrorData,
    >
    where
        M: FnOnce(CancellationToken) -> F,
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "authority transaction scheduled outside a Tokio runtime",
            ));
        }
        let admission_closed =
            self.authority_finalizers
                .admission_closed
                .lock()
                .map_err(|_error| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "authority transaction admission lock poisoned",
                    )
                })?;
        if *admission_closed {
            return Err(mcp_error(
                error_codes::DAEMON_RESTARTING,
                "authority transaction supervisor is closed for daemon shutdown",
            ));
        }
        let mut transactions = self
            .authority_finalizers
            .transactions
            .lock()
            .map_err(|_error| {
                self.drain_state_handle()
                    .mark_draining("authority_transaction_registry_poisoned");
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "authority transaction registry lock poisoned",
                )
            })?;
        let task_id = self
            .authority_finalizers
            .next_task_id
            .fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        let future = make_future(cancellation.clone());
        let phase = Arc::new(AtomicU8::new(AUTHORITY_TASK_PHASE_REGISTERED));
        let (start_sender, start_receiver) = oneshot::channel::<()>();
        let (result_sender, result_receiver) = oneshot::channel();
        let (reaped_sender, reaped_receiver) = oneshot::channel();
        let transaction_cancellation = cancellation.clone();
        let transaction_phase = Arc::clone(&phase);
        let transaction_cancellation_policy = cancellation_policy;
        let transaction = tokio::spawn(async move {
            if start_receiver.await.is_err() {
                transaction_phase.store(AUTHORITY_TASK_PHASE_ROLLBACK_COMPLETE, Ordering::Release);
                let _send_result = result_sender.send(Err(
                    AuthorityTransactionJoinError::cancelled(format!(
                        "authority transaction {task_id} start gate closed before admission completed"
                    )),
                ));
                return AuthorityTaskExit::StartGateClosed;
            }
            transaction_phase.store(AUTHORITY_TASK_PHASE_RUNNING, Ordering::Release);
            let mut future = Box::pin(std::panic::AssertUnwindSafe(future).catch_unwind());
            let supervisor_cancellation = async move {
                if transaction_cancellation_policy
                    == AuthorityCancellationPolicy::DropFutureAfterSignal
                {
                    transaction_cancellation.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                biased;
                _ = supervisor_cancellation => {
                    // Dropping the complete transaction future runs the exact
                    // synchronous profile/lease/audit rollback guards. Store
                    // the completion phase only after every Drop returned.
                    match drop_authority_owned(future) {
                        Ok(()) => {
                            transaction_phase.store(
                                AUTHORITY_TASK_PHASE_ROLLBACK_COMPLETE,
                                Ordering::Release,
                            );
                            if result_sender
                                .send(Err(AuthorityTransactionJoinError::cancelled(format!(
                                    "authority transaction {task_id} cancelled for daemon shutdown after rollback"
                                ))))
                                .is_err()
                            {
                                tracing::debug!(
                                    code = "AUTHORITY_TRANSACTION_CALLER_GONE",
                                    task_id,
                                    outcome = "cancelled_after_rollback",
                                    "authority transaction caller dropped before shutdown cancellation completed"
                                );
                            }
                            AuthorityTaskExit::CancelledAfterRollback
                        }
                        Err(drop_panic) => {
                            let detail = format!(
                                "authority transaction {task_id} rollback destructor panicked during shutdown cancellation: {drop_panic}"
                            );
                            transaction_phase.store(
                                AUTHORITY_TASK_PHASE_PANICKED,
                                Ordering::Release,
                            );
                            let _send_result = result_sender.send(Err(
                                AuthorityTransactionJoinError::panicked(detail.clone()),
                            ));
                            AuthorityTaskExit::Panicked(detail)
                        }
                    }
                }
                outcome = future.as_mut() => {
                    match outcome {
                        Ok(output) => {
                            // A completed async future can retain locals until
                            // the future object itself is dropped. Contain a
                            // faulty destructor before publishing completion.
                            match drop_authority_owned(future) {
                                Ok(()) => {
                                    transaction_phase.store(
                                        AUTHORITY_TASK_PHASE_COMPLETED,
                                        Ordering::Release,
                                    );
                                    if result_sender.send(Ok(output)).is_err() {
                                        tracing::debug!(
                                            code = "AUTHORITY_TRANSACTION_CALLER_GONE",
                                            task_id,
                                            outcome = "completed",
                                            "authority transaction completed after its caller disconnected"
                                        );
                                    }
                                    AuthorityTaskExit::Completed
                                }
                                Err(future_drop_panic) => {
                                    let output_drop_panic = drop_authority_owned(output).err();
                                    let detail = match output_drop_panic {
                                        Some(output_drop_panic) => format!(
                                            "authority transaction {task_id} future destructor panicked after completion: {future_drop_panic}; completed output destructor also panicked: {output_drop_panic}"
                                        ),
                                        None => format!(
                                            "authority transaction {task_id} future destructor panicked after completion: {future_drop_panic}"
                                        ),
                                    };
                                    transaction_phase.store(
                                        AUTHORITY_TASK_PHASE_PANICKED,
                                        Ordering::Release,
                                    );
                                    let _send_result = result_sender.send(Err(
                                        AuthorityTransactionJoinError::panicked(detail.clone()),
                                    ));
                                    AuthorityTaskExit::Panicked(detail)
                                }
                            }
                        }
                        Err(payload) => {
                            // Consume the payload through the daemon-wide
                            // hardened decoder. Unknown payload destructors are
                            // user code and can panic a second time; the decoder
                            // contains that destructor panic so the supervisor
                            // can still publish phase/result and reap the exact
                            // owned task.
                            let poll_panic =
                                crate::daemon_lifecycle::consume_panic_payload(payload);
                            let detail = match drop_authority_owned(future) {
                                Ok(()) => poll_panic,
                                Err(drop_panic) => format!(
                                    "{poll_panic}; authority transaction {task_id} future destructor also panicked: {drop_panic}"
                                ),
                            };
                            transaction_phase.store(
                                AUTHORITY_TASK_PHASE_PANICKED,
                                Ordering::Release,
                            );
                            if result_sender
                                .send(Err(AuthorityTransactionJoinError::panicked(
                                    detail.clone(),
                                )))
                                .is_err()
                            {
                                tracing::error!(
                                    code = error_codes::TOOL_INTERNAL_ERROR,
                                    detail_code = "AUTHORITY_TRANSACTION_DETACHED_PANIC",
                                    task_id,
                                    panic = %detail,
                                    "detached authority transaction panicked after its caller disconnected"
                                );
                            }
                            AuthorityTaskExit::Panicked(detail)
                        }
                    }
                }
            }
        });
        let abort_handle = transaction.abort_handle();
        transactions.insert(
            task_id,
            AuthorityTaskControl {
                abort_handle,
                cancellation,
                cancellation_policy,
                phase,
            },
        );

        let transaction_registry = Arc::clone(&self.authority_finalizers.transactions);
        let join_failures = Arc::clone(&self.authority_finalizers.join_failures);
        let drain_state = self.drain_state_handle();
        self.authority_finalizers.tasks.spawn(async move {
            let join_result = transaction.await;
            let failure = match join_result {
                Ok(AuthorityTaskExit::Completed | AuthorityTaskExit::CancelledAfterRollback) => {
                    None
                }
                Ok(AuthorityTaskExit::Panicked(detail)) => Some(format!(
                    "authority transaction {task_id} panicked: {detail}"
                )),
                Ok(AuthorityTaskExit::StartGateClosed) => Some(format!(
                    "authority transaction {task_id} start gate closed before execution"
                )),
                Err(error) => {
                    let phase = transactions_phase(&transaction_registry, task_id);
                    let cancelled = error.is_cancelled();
                    let panicked = error.is_panic();
                    let detail = if panicked {
                        match error.try_into_panic() {
                            Ok(payload) => {
                                crate::daemon_lifecycle::consume_panic_payload(payload)
                            }
                            Err(error) => format!("panic payload unavailable: {error}"),
                        }
                    } else {
                        error.to_string()
                    };
                    Some(format!(
                        "authority transaction {task_id} join failed: cancelled={cancelled} panicked={panicked} phase={phase} detail={detail}",
                    ))
                }
            };
            let reap_result = match transaction_registry.lock() {
                Ok(mut transactions) => {
                    if transactions.remove(&task_id).is_some() {
                        Ok(())
                    } else {
                        let detail = format!(
                            "authority transaction {task_id} registry entry was missing after exact task join"
                        );
                        push_authority_join_failure(&join_failures, detail.clone());
                        drain_state.mark_draining("authority_transaction_registry_entry_missing");
                        Err(AuthorityTransactionJoinError::panicked(detail))
                    }
                }
                Err(poisoned) => {
                    poisoned.into_inner().remove(&task_id);
                    let detail = format!(
                        "authority transaction registry lock poisoned while reaping task {task_id}"
                    );
                    push_authority_join_failure(&join_failures, detail.clone());
                    drain_state.mark_draining("authority_transaction_registry_poisoned");
                    Err(AuthorityTransactionJoinError::panicked(detail))
                }
            };
            if let Some(failure) = failure {
                push_authority_join_failure(&join_failures, failure.clone());
                drain_state.mark_draining("authority_transaction_join_failed");
                tracing::error!(
                    code = error_codes::TOOL_INTERNAL_ERROR,
                    detail_code = "AUTHORITY_TRANSACTION_JOIN_FAILED",
                    task_id,
                    error = %failure,
                    "supervised authority transaction failed before a clean join"
                );
            }
            if reaped_sender.send(reap_result).is_err() {
                tracing::debug!(
                    code = "AUTHORITY_TRANSACTION_CALLER_GONE",
                    task_id,
                    outcome = "joined_and_registry_reaped",
                    "authority transaction join/reap acknowledgement outlived its caller"
                );
            }
        });
        drop(transactions);
        if start_sender.send(()).is_err() {
            self.drain_state_handle()
                .mark_draining("authority_transaction_start_gate_failed");
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("authority transaction {task_id} failed to open its start gate"),
            ));
        }
        drop(admission_closed);
        Ok(async move {
            let result = result_receiver.await.unwrap_or_else(|error| {
                Err(AuthorityTransactionJoinError::cancelled(format!(
                    "authority transaction {task_id} result channel closed before a supervised outcome: {error}"
                )))
            });
            match reaped_receiver.await {
                Ok(Ok(())) => result,
                Ok(Err(reap_error)) => {
                    if let Ok(output) = result {
                        let _drop_result = drop_authority_owned(output);
                    }
                    Err(reap_error)
                }
                Err(error) => {
                    let mut detail = format!(
                        "authority transaction {task_id} join/reap acknowledgement channel closed before registry removal was proven: {error}"
                    );
                    if let Ok(output) = result
                        && let Err(drop_panic) = drop_authority_owned(output)
                    {
                        detail.push_str(&format!(
                            "; completed output destructor also panicked: {drop_panic}"
                        ));
                    }
                    Err(AuthorityTransactionJoinError::panicked(detail))
                }
            }
        })
    }

    /// Close admission to the daemon's supervised authority-transaction set
    /// and wait until every previously registered mutation/cleanup/audit task
    /// exits. `TaskTracker::close` does not reject later spawns; the same mutex
    /// therefore orders the explicit admission bit against registration.
    pub(crate) async fn drain_authority_finalizers(
        &self,
    ) -> Result<AuthorityFinalizerDrainReadback, AuthorityFinalizerDrainFailure> {
        let mut errors = Vec::new();
        match self.authority_finalizers.admission_closed.lock() {
            Ok(mut admission_closed) => {
                *admission_closed = true;
            }
            Err(poisoned) => {
                self.drain_state_handle()
                    .mark_draining("authority_transaction_admission_poisoned");
                // A poisoned mutex still owns its protected value. Recover the
                // guard solely to close admission, then report the poison after
                // every previously admitted task has been drained.
                *poisoned.into_inner() = true;
                errors.push(
                    "authority transaction admission lock poisoned during shutdown drain"
                        .to_owned(),
                );
                tracing::error!(
                    code = error_codes::TOOL_INTERNAL_ERROR,
                    "authority transaction admission lock poisoned during shutdown drain"
                );
            }
        }
        self.authority_finalizers.tasks.close();
        let (registered_tasks_before, controls) =
            snapshot_authority_controls(&self.authority_finalizers.transactions, &mut errors);
        for (task_id, control) in &controls {
            tracing::info!(
                code = "AUTHORITY_TRANSACTION_CANCELLATION_REQUESTED",
                task_id,
                phase = authority_task_phase_name(control.phase.load(Ordering::Acquire)),
                cancellation_policy = control.cancellation_policy.as_str(),
                "signalling cancellation to a supervised authority transaction"
            );
            control.cancellation.cancel();
        }
        let cancellation_signals_sent = controls.len();
        let mut abort_requests_sent = 0_usize;
        if tokio::time::timeout(
            AUTHORITY_CANCELLATION_GRACE,
            self.authority_finalizers.tasks.wait(),
        )
        .await
        .is_err()
        {
            let (remaining_before_abort, remaining_controls) =
                snapshot_authority_controls(&self.authority_finalizers.transactions, &mut errors);
            let mut cooperative_owners_retained = 0_usize;
            for (task_id, control) in remaining_controls {
                if control.cancellation_policy
                    == AuthorityCancellationPolicy::CooperativeTerminalReadback
                {
                    cooperative_owners_retained += 1;
                    tracing::error!(
                        code = error_codes::ACTION_POSTCONDITION_FAILED,
                        detail_code = "AUTHORITY_TRANSACTION_COOPERATIVE_OWNER_RETAINED",
                        task_id,
                        phase = authority_task_phase_name(control.phase.load(Ordering::Acquire)),
                        cancellation_policy = control.cancellation_policy.as_str(),
                        "storage-backed authority owner did not reach terminal readback inside cancellation grace; retaining its exact task and daemon lifetime locks"
                    );
                    continue;
                }
                tracing::error!(
                    code = error_codes::TOOL_INTERNAL_ERROR,
                    detail_code = "AUTHORITY_TRANSACTION_ABORT_REQUIRED",
                    task_id,
                    phase = authority_task_phase_name(control.phase.load(Ordering::Acquire)),
                    cancellation_policy = control.cancellation_policy.as_str(),
                    "authority transaction did not join inside cancellation grace; aborting its exact owned Tokio task"
                );
                control.abort_handle.abort();
                abort_requests_sent += 1;
            }
            errors.push(format!(
                "authority cancellation grace expired with {remaining_before_abort} transaction(s) still registered; aborted {abort_requests_sent} drop-safe task(s) and retained {cooperative_owners_retained} storage-backed owner(s)"
            ));
            if abort_requests_sent > 0
                && tokio::time::timeout(
                    AUTHORITY_ABORT_JOIN_TIMEOUT,
                    self.authority_finalizers.tasks.wait(),
                )
                .await
                .is_err()
            {
                errors.push(format!(
                    "authority task join owners did not finish within {} ms after exact task abort",
                    AUTHORITY_ABORT_JOIN_TIMEOUT.as_millis()
                ));
            }
        }
        let admission_closed = match self.authority_finalizers.admission_closed.lock() {
            Ok(admission_closed) => *admission_closed,
            Err(poisoned) => **poisoned.get_ref(),
        };
        let registered_tasks_after = authority_registry_len(
            &self.authority_finalizers.transactions,
            &mut errors,
            "shutdown final readback",
        );
        let readback = AuthorityFinalizerDrainReadback {
            admission_closed,
            tracker_closed: self.authority_finalizers.tasks.is_closed(),
            registered_tasks_before,
            cancellation_signals_sent,
            abort_requests_sent,
            registered_tasks_after,
            tracked_tasks_after: self.authority_finalizers.tasks.len(),
        };
        // Join owners remove their registry row, publish any join failure, and
        // only then leave the TaskTracker. Consume the failure ledger after
        // both owner counts are read: when `tracked_tasks_after == 0`, every
        // possible publisher has completed, so a failure cannot appear in the
        // gap between ledger consumption and the zero-owner verdict. A
        // non-zero readback already fails closed and a later drain can consume
        // any outcome published by the retained owner.
        take_authority_join_failures(&self.authority_finalizers.join_failures, &mut errors);
        tracing::info!(
            code = "AUTHORITY_TRANSACTIONS_DRAINED",
            admission_closed = readback.admission_closed,
            tracker_closed = readback.tracker_closed,
            registered_tasks_before = readback.registered_tasks_before,
            cancellation_signals_sent = readback.cancellation_signals_sent,
            abort_requests_sent = readback.abort_requests_sent,
            registered_tasks_after = readback.registered_tasks_after,
            tracked_tasks_after = readback.tracked_tasks_after,
            safe_to_unlock = readback.safe_to_unlock(),
            "all supervised authority transactions completed through cleanup and audit"
        );
        if !readback.admission_closed {
            errors.push("authority transaction admission remained open after drain".to_owned());
        }
        if !readback.tracker_closed {
            errors.push("authority transaction task tracker remained open after drain".to_owned());
        }
        if readback.registered_tasks_after != 0 {
            errors.push(format!(
                "{} authority transaction registry entry/entries remained after drain",
                readback.registered_tasks_after
            ));
        }
        if readback.tracked_tasks_after != 0 {
            errors.push(format!(
                "{} authority transaction task(s) remained after drain",
                readback.tracked_tasks_after
            ));
        }
        if errors.is_empty() {
            Ok(readback)
        } else {
            let failure = AuthorityFinalizerDrainFailure {
                readback,
                detail: errors.join("; "),
            };
            tracing::error!(
                code = "AUTHORITY_TRANSACTIONS_DRAIN_FAILED",
                error = %failure,
                "authority transaction shutdown drain did not reach a trustworthy state"
            );
            Err(failure)
        }
    }

    pub(crate) fn shutdown_cancel_token(&self) -> Result<CancellationToken, ErrorData> {
        self.m3_state
            .lock()
            .map(|state| state.shutdown_cancel.clone())
            .map_err(|_err| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned while reading daemon shutdown token",
                )
            })
    }

    pub(crate) const fn session_targets_ref(&self) -> &SharedSessionTargets {
        &self.session_targets
    }

    pub(crate) const fn cdp_target_owners_ref(&self) -> &SharedCdpTargetOwners {
        &self.cdp_target_owners
    }

    pub(crate) const fn session_clipboards_ref(&self) -> &SharedSessionClipboardBuffers {
        &self.session_clipboards
    }

    pub(crate) fn session_registry_handle(&self) -> SharedSessionRegistry {
        Arc::clone(&self.session_registry)
    }

    pub(crate) const fn session_registry_ref(&self) -> &SharedSessionRegistry {
        &self.session_registry
    }

    pub(crate) fn mailbox_notify_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.mailbox_notify)
    }

    /// Resolves the session's active target, if any. The cloned value is
    /// returned after the map guard is dropped.
    pub(crate) fn session_target(
        &self,
        session_id: Option<&str>,
    ) -> Result<Option<SessionTarget>, ErrorData> {
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        self.restore_session_target_if_needed(session_id)
    }

    /// Resolves the foreground-equivalent target owned by one MCP session.
    /// This never falls back to the human OS foreground window; callers that
    /// need the real OS foreground must ask for it explicitly.
    pub(crate) fn agent_logical_foreground(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionTarget>, ErrorData> {
        self.session_target(Some(session_id))
    }

    /// Resolves the effective target for an ACTION call (#984): an explicit
    /// per-call `window_hwnd` / `cdp_target_id` override wins over the session's
    /// bound target, mirroring how `observe` / `capture_screenshot` accept an
    /// explicit `window_hwnd`. This makes multi-window / multi-agent action
    /// routing deterministic instead of depending on per-session `set_target`
    /// state that may not persist across reconnects. `cdp_target_id` requires
    /// `window_hwnd` (the browser window whose CDP endpoint owns the target) and
    /// gives a stable routing handle that survives window-handle recycling.
    pub(crate) fn action_session_target_override(
        &self,
        explicit_window_hwnd: Option<i64>,
        explicit_cdp_target_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Option<SessionTarget>, ErrorData> {
        let target = match explicit_action_target(explicit_window_hwnd, explicit_cdp_target_id)? {
            Some(target) => Ok(Some(target)),
            None => self.session_target(session_id),
        }?;
        if let Some(target) = target.as_ref() {
            let hwnd = match target {
                SessionTarget::Window { hwnd } => *hwnd,
                SessionTarget::Cdp { window_hwnd, .. } => *window_hwnd,
            };
            crate::m1::validate_window_hwnd_shape("action_target_override", hwnd)?;
        }
        Ok(target)
    }

    pub(crate) fn unscoped_action_handle(&self) -> anyhow::Result<ActionHandle> {
        self.m2_state
            .lock()
            .map(|state| state.emitter_handle.clone().with_session_id(None))
            .map_err(|_err| anyhow::anyhow!("M2 service state lock poisoned"))
    }

    fn tool_router() -> ToolRouter<Self> {
        Self::build_tool_router(debug_tools_enabled())
    }

    /// Builds the full tool router, then removes the synthetic debug-only
    /// fault-injector / synthetic-write routes unless `debug_enabled`.
    ///
    /// Split out from [`Self::tool_router`] so the gating decision is a pure
    /// function of an explicit bool and can be unit-tested deterministically
    /// via [`rmcp::handler::server::router::tool::ToolRouter::has_route`],
    /// without mutating the process-global `SYNAPSE_DEBUG_TOOLS` env var (which
    /// would race with other tests). See [`DEBUG_ONLY_TOOL_ROUTES`] for the
    /// gated set and its rationale (#1348, #1595).
    fn build_tool_router(debug_enabled: bool) -> ToolRouter<Self> {
        let mut router = Self::m1_tool_router()
            + Self::m2_tool_router()
            + Self::lease_tool_router()
            + Self::session_tool_router()
            + Self::agent_mailbox_tool_router()
            + Self::agent_cost_tool_router()
            + Self::cost_facade_tool_router()
            + Self::agent_stats_tool_router()
            + Self::agent_query_tool_router()
            + Self::agent_control_tool_router()
            + Self::agent_template_tool_router()
            + Self::agent_task_tool_router()
            + Self::agent_facade_tool_router()
            + Self::approval_facade_tool_router()
            + Self::audit_replay_facade_tool_router()
            + Self::timeline_facade_tool_router()
            + Self::routine_assist_facade_tool_router()
            + Self::operational_facade_tool_router()
            + Self::workspace_blackboard_tool_router()
            + Self::target_claim_tool_router()
            + Self::reality_tool_router()
            + Self::m3_tool_router()
            + Self::intent_tool_router()
            + Self::routine_labeling_tool_router()
            + Self::routine_feedback_tool_router()
            + Self::plan_tool_router()
            + Self::suggestions_tool_router()
            + Self::timeline_query_tool_router()
            + Self::timeline_digest_tool_router()
            + Self::m4_tool_router()
            + Self::notify_tool_router()
            + Self::hygiene_report_tool_router()
            + Self::data_cleaning_tool_router()
            + Self::permission_gate_tool_router()
            + Self::escalation_tool_router()
            + Self::background_router_tool_router()
            + Self::browser_batch_tool_router()
            + Self::capture_gif_tool_router()
            + Self::verification_tool_router()
            + Self::browser_assert_tool_router()
            + Self::browser_clock_events_tool_router()
            + Self::browser_dialog_tool_router()
            + Self::browser_dnd_tool_router()
            + Self::browser_emulate_tool_router()
            + Self::browser_facade_tool_router()
            + Self::browser_field_tool_router()
            + Self::browser_files_tool_router()
            + Self::browser_frames_tool_router()
            + Self::browser_network_tool_router()
            + Self::browser_storage_tool_router()
            + Self::tool_profile_tool_router();
        // Gate the synthetic test/support tools off the default agent
        // surface; they remain reachable only when SYNAPSE_DEBUG_TOOLS is set.
        // `remove_route` drops the route from the router entirely, so the tool
        // is unreachable even by an unbound call (the tool-profile visibility
        // filter only runs once a session is bound — see
        // `tools_for_session_profile`), matching the least-privilege /
        // exclude-by-default posture: a gated tool cannot be called even when
        // explicitly named. Every test that drives these sets
        // SYNAPSE_DEBUG_TOOLS=1.
        if !debug_enabled {
            for route in DEBUG_ONLY_TOOL_ROUTES {
                router.remove_route(route);
            }
        }
        router
    }
}

/// Synthetic test/support tool routes that must NOT ship on the normal,
/// default agent surface; they are only registered when `SYNAPSE_DEBUG_TOOLS` is
/// set (see [`SynapseService::build_tool_router`]).
///
/// - `storage_put_probe_rows` — writes bounded synthetic probe rows into a real
///   RocksDB column family; a synthetic-write diagnostic used by storage/GC and
///   timeline regression checks to seed rows, never a production capability
///   (#1595). Manual FSV remains separate.
/// - `storage_pressure_sample` — simulates disk pressure to exercise the storage
///   pressure signal.
/// - `action_diagnostic_rate_limit_override` / `action_diagnostic_queue_full_setup`
///   — force `ACTION_RATE_LIMITED` / `ACTION_QUEUE_FULL` to exercise the action
///   emitter's backpressure paths.
///
/// All four have no production callers (#1348, #1595).
pub(crate) const DEBUG_ONLY_TOOL_ROUTES: &[&str] = &[
    "storage_put_probe_rows",
    "storage_pressure_sample",
    "action_diagnostic_rate_limit_override",
    "action_diagnostic_queue_full_setup",
];

/// Pure decision for [`SynapseService::action_session_target_override`] (#984):
/// maps explicit per-call routing params to a [`SessionTarget`], or `Ok(None)`
/// when neither is supplied (caller should fall back to the bound session
/// target). A `cdp_target_id` without a `window_hwnd` is rejected because the
/// browser window's HWND is needed to reach the CDP endpoint that owns the
/// target. Extracted as a free function so the routing precedence is unit-tested
/// without standing up a full service.
pub(crate) fn explicit_action_target(
    explicit_window_hwnd: Option<i64>,
    explicit_cdp_target_id: Option<&str>,
) -> Result<Option<SessionTarget>, ErrorData> {
    let explicit_window_hwnd = explicit_window_hwnd
        .map(|hwnd| crate::m1::validate_window_hwnd_shape("action_target_override", hwnd))
        .transpose()?;
    let explicit_cdp_target_id = explicit_cdp_target_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (explicit_window_hwnd, explicit_cdp_target_id) {
        (Some(window_hwnd), Some(cdp_target_id)) => Ok(Some(SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id: cdp_target_id.to_owned(),
        })),
        (Some(hwnd), None) => Ok(Some(SessionTarget::Window { hwnd })),
        (None, Some(_)) => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_target_id requires window_hwnd (the browser window whose CDP endpoint owns the target)",
        )),
        (None, None) => Ok(None),
    }
}

/// Whether test-only/debug MCP tools should be exposed on the surface.
fn debug_tools_enabled() -> bool {
    std::env::var("SYNAPSE_DEBUG_TOOLS")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

impl Default for SynapseService {
    fn default() -> Self {
        Self::new()
    }
}

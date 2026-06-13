use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

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
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    http::sse::SseState,
    m1::{
        CaptureScreenshotFormat, CaptureScreenshotParams, CaptureScreenshotResponse,
        CdpCloseTabParams, CdpCloseTabResponse, CdpNavigateAction, CdpNavigateTabParams,
        CdpNavigateTabResponse, CdpOpenTabParams, CdpOpenTabResponse, FindParams, FindResponse,
        HiddenDesktopPipFrameParams, HiddenDesktopPipFrameResponse, HiddenDesktopPipStreamStatus,
        M1State, ObserveParams, ReadTextParams, SetCaptureTargetParams, SetCaptureTargetResponse,
        SetPerceptionModeParams, SetPerceptionModeResponse, SetTargetParam, SetTargetParams,
        SharedM1State, TargetResponse, TargetWire, apply_profile_runtime_config_in_state,
        build_find_input, current_input, empty_input_schema, enrich_input_with_browser_ocr,
        enrich_input_with_cdp_for_target, find_cdp_max_nodes, find_snapshot_depth,
        match_find_input, mcp_error, observe_include, observe_input, populate_clipboard_summary,
        populate_detection_from_state, populate_fs_recent, read_text_request_uncached,
        resolve_read_text_request, set_capture_target_in_state, set_perception_mode_in_state,
        set_target_input_schema,
    },
    m2::{
        ActClickParams, ActClickResponse, ActClipboardParams, ActClipboardResponse,
        ActFocusWindowParams, ActFocusWindowResponse, ActKeymapParams, ActKeymapResponse,
        ActPadParams, ActPadResponse, ActPressParams, ActPressResponse, ActScrollParams,
        ActScrollResponse, ActSetValueParams, ActSetValueResponse, ActStrokeParams,
        ActStrokeResponse, ActTypeParams, ActTypeResponse, M2ServiceConfig, ReleaseAllParams,
        ReleaseAllResponse, SharedM2State, SharedSessionClipboardBuffers,
        act_click_with_handle_and_lease, act_clipboard_session_buffer, act_focus_window,
        act_focus_window_request_details, act_focus_window_target_hwnd, act_keymap_with_handle,
        act_pad_with_handle, act_press_with_handle, act_scroll_with_handle, act_set_value,
        act_set_value_request_details, act_stroke_validation_failure_details,
        act_stroke_with_handle, act_type_with_handle, new_session_clipboards,
        release_all_with_handles, shared_m2_state_from_config_with_shutdown_reason,
        shared_m2_state_from_env, validate_act_stroke_params,
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
            ProfileAuthoringListParams, ProfileAuthoringListResponse,
            decide_profile_authoring_candidate, export_profile_authoring_candidate,
            generate_profile_authoring_candidate, inspect_profile_authoring_candidate,
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
        ActSpawnAgentRequest, ActSpawnAgentResponse, ActSpawnAgentTarget, LaunchWindowState,
        M4ServiceConfig, MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS, RunShellAuthorization,
        ShellExecutionContext, assign_owned_process_job, authorize_run_shell,
        authorize_run_shell_start, cancel_shell_job, execute_combo, launch, launch_for_session,
        launch_process_history_row, launch_process_history_row_key, launch_request_details,
        prepare_run_shell_params_for_context, prepare_run_shell_start_params_for_context,
        required_combo_permissions, run_authorized_shell, run_shell_idempotency_completed_row,
        run_shell_idempotency_replay, run_shell_idempotency_reservation_row,
        run_shell_idempotency_row_key, run_shell_request_details, run_shell_start_request_details,
        shell_execution_context_for_session, shell_job_status, start_authorized_shell_job,
        validate_agent_spawn_params, validate_run_shell_execution_plan,
    },
};

mod action_audit;
mod action_preflight;
pub(crate) mod agent_control;
pub(crate) mod agent_cost;
pub(crate) mod agent_event_ingress;
pub(crate) mod agent_events;
mod agent_mailbox;
pub(crate) mod agent_query;
pub(crate) mod agent_state;
pub(crate) mod agent_stats;
pub(crate) mod agent_templates;
pub(crate) mod agent_transcripts;
mod audit_context;
pub(crate) mod command_audit;
mod context;
pub(crate) use context::AgentTranscriptSnapshotRow;
pub(crate) mod drain;
pub(crate) mod escalation;
mod everquest_autocombat;
mod everquest_contextgraph;
mod everquest_domain;
mod everquest_episode_export;
mod everquest_guard;
mod everquest_log;
mod everquest_map_sensor;
mod everquest_memory;
mod everquest_outcome;
mod everquest_predictive_model;
mod everquest_route;
mod everquest_scorecard;
mod everquest_state;
mod everquest_surprise;
mod everquest_tools;
mod everquest_trajectory;
mod everquest_ui_context;
mod everquest_world_model;
mod everquest_world_summary;
mod handler;
mod health;
mod hygiene_report;
mod intent_tools;
mod lease_tools;
mod m1_tools;
mod m2_tools;
mod m3_tools;
pub(crate) mod m4_tools;
mod notify_tools;
mod reality;
mod schema_sanitize;
pub(crate) mod session_continuity;
pub(crate) mod session_lifecycle;
pub(crate) mod session_registry;
mod session_tools;
mod target_claims;
mod target_policy;
#[cfg(test)]
mod tests;
mod timeline_query;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CdpTargetOwner {
    pub session_id: String,
    pub window_hwnd: i64,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub requested_url: String,
    pub target_url: String,
    pub created_at_unix_ms: u64,
}

/// Per-CDP-target ownership registry keyed by browser surface + `TargetID`.
/// Only the creating MCP session may close a registered target; unowned targets
/// may be observed by explicit `set_target` but are never closed by Synapse.
pub(crate) type SharedCdpTargetOwners = Arc<Mutex<HashMap<String, CdpTargetOwner>>>;

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
    session_processes: session_lifecycle::SharedSessionProcessResources,
    terminated_sessions: session_lifecycle::SharedTerminatedSessions,
}

fn install_chrome_browser_navigation_sink(m3_state: &SharedM3State) {
    let m3_state = Arc::clone(m3_state);
    crate::chrome_debugger_bridge::set_browser_navigation_sink(Arc::new(move |event| {
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

    pub fn try_new() -> anyhow::Result<Self> {
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
                shutdown_cancel.clone(),
                shutdown_reason,
                Some(connection_closed_cancel.clone()),
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
                shutdown_cancel.clone(),
                shutdown_reason,
                Some(connection_closed_cancel.clone()),
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

    pub(crate) fn m3_state_handle(&self) -> SharedM3State {
        Arc::clone(&self.m3_state)
    }

    pub(crate) fn drain_state_handle(&self) -> drain::DaemonDrainState {
        self.drain_state.clone()
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

    /// Resolves the session's active **window** target to an HWND, if any. The
    /// map guard is dropped before returning (a copied `i64`), so it is never
    /// held across the non-`Send` perception path or an `.await`.
    pub(crate) fn session_target_hwnd(
        &self,
        session_id: Option<&str>,
    ) -> Result<Option<i64>, ErrorData> {
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        match self.session_target(Some(session_id))? {
            Some(SessionTarget::Window { hwnd }) => Ok(Some(hwnd)),
            Some(SessionTarget::Cdp { window_hwnd, .. }) => Ok(Some(window_hwnd)),
            None => Ok(None),
        }
    }

    pub(crate) fn unscoped_action_handle(&self) -> anyhow::Result<ActionHandle> {
        self.m2_state
            .lock()
            .map(|state| state.emitter_handle.clone().with_session_id(None))
            .map_err(|_err| anyhow::anyhow!("M2 service state lock poisoned"))
    }

    fn tool_router() -> ToolRouter<Self> {
        let mut router = Self::m1_tool_router()
            + Self::m2_tool_router()
            + Self::lease_tool_router()
            + Self::session_tool_router()
            + Self::agent_mailbox_tool_router()
            + Self::agent_cost_tool_router()
            + Self::agent_stats_tool_router()
            + Self::agent_query_tool_router()
            + Self::agent_control_tool_router()
            + Self::agent_template_tool_router()
            + Self::workspace_blackboard_tool_router()
            + Self::target_claim_tool_router()
            + Self::reality_tool_router()
            + Self::m3_tool_router()
            + Self::intent_tool_router()
            + Self::timeline_query_tool_router()
            + Self::m4_tool_router()
            + Self::notify_tool_router()
            + Self::hygiene_report_tool_router()
            + Self::escalation_tool_router();
        // The EverQuest domain pack (25 tools) is off the general-agent surface
        // unless the operator opts in (SYNAPSE_ENABLE_EVERQUEST). No capability
        // is lost — visibility is gated. rmcp builds the tool list once per
        // service, so a startup opt-in flag is the gating mechanism (dynamic
        // per-profile re-listing would require tools/list_changed plumbing).
        if everquest_enabled() {
            router = router
                + Self::everquest_tool_router()
                + Self::everquest_autocombat_tool_router()
                + Self::everquest_contextgraph_tool_router()
                + Self::everquest_domain_tool_router()
                + Self::everquest_episode_export_tool_router()
                + Self::everquest_guard_tool_router()
                + Self::everquest_state_tool_router()
                + Self::everquest_map_sensor_tool_router()
                + Self::everquest_memory_tool_router()
                + Self::everquest_outcome_tool_router()
                + Self::everquest_predictive_model_tool_router()
                + Self::everquest_route_tool_router()
                + Self::everquest_scorecard_tool_router()
                + Self::everquest_surprise_tool_router()
                + Self::everquest_trajectory_tool_router()
                + Self::everquest_world_model_tool_router()
                + Self::everquest_world_summary_tool_router();
        }
        // Gate test-only storage probes off the default agent surface; they
        // remain available (and callable) only when SYNAPSE_DEBUG_TOOLS is set.
        if !debug_tools_enabled() {
            router.remove_route("storage_put_probe_rows");
            router.remove_route("storage_pressure_sample");
        }
        router
    }
}

/// Whether test-only/debug MCP tools should be exposed on the surface.
fn debug_tools_enabled() -> bool {
    std::env::var("SYNAPSE_DEBUG_TOOLS")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Whether the EverQuest domain tool pack should be advertised.
fn everquest_enabled() -> bool {
    std::env::var("SYNAPSE_ENABLE_EVERQUEST")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

impl Default for SynapseService {
    fn default() -> Self {
        Self::new()
    }
}

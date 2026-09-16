use super::notify_tools::{
    NotifyHumanParams, NotifyKind, SYNAPSE_TOAST_GROUP, ToastAction, ToastActionActivationType,
    ToastActivationCallback, run_internal_toast_with_activation, toast_tag_for,
};
use super::{
    ApprovalDecideParams, ApprovalDecideResponse, ApprovalListParams, ApprovalListResponse,
    ApprovalRequestParams, ApprovalRequestResponse, ApprovalToastDelivery, AudioTailParams,
    AudioTailResponse, AudioTranscribeParams, AudioTranscribeResponse, AuditExportBundleParams,
    AuditExportBundleResponse, AuditIntelligenceQueryParams, AuditIntelligenceQueryResponse,
    DemoRecordStartParams, DemoRecordStartResponse, DemoRecordStopParams, DemoRecordStopResponse,
    EpisodeGetParams, EpisodeGetResponse, EpisodeListParams, EpisodeListResponse,
    EpisodeSegmentParams, EpisodeSegmentResponse, ErrorData, HygieneFlagsParams,
    HygieneFlagsResponse, HygieneScanStorageParams, HygieneScanStorageResponse,
    HygieneScanTextParams, HygieneScanTextResponse, Json, LocalModelListParams,
    LocalModelListResponse, LocalModelProbeParams, LocalModelProbeResponse,
    LocalModelRegisterParams, LocalModelRegisterResponse, LocalModelRemoveParams,
    LocalModelRemoveResponse, LocalModelUpdateParams, LocalModelUpdateResponse, Parameters,
    ProfileActivateParams, ProfileActivateResponse, ProfileAuthoringDecideParams,
    ProfileAuthoringDecideResponse, ProfileAuthoringExportParams, ProfileAuthoringExportResponse,
    ProfileAuthoringGenerateParams, ProfileAuthoringGenerateResponse,
    ProfileAuthoringInspectParams, ProfileAuthoringInspectResponse, ProfileAuthoringListParams,
    ProfileAuthoringListResponse, ProfileListParams, ProfileListResponse,
    ProfileQualityRefreshParams, ProfileQualityRefreshResponse, ProfileRegistryDisableParams,
    ProfileRegistryDisableResponse, ProfileRegistryExportParams, ProfileRegistryExportResponse,
    ProfileRegistryImportParams, ProfileRegistryImportResponse, ProfileRegistryInstallParams,
    ProfileRegistryInstallResponse, ProfileRegistryQueryParams, ProfileRegistryQueryResponse,
    ProfileRegistryRollbackParams, ProfileRegistryRollbackResponse, ReflexCancelParams,
    ReflexCancelResponse, ReflexHistoryParams, ReflexHistoryResponse, ReflexListParams,
    ReflexListResponse, ReflexRegisterParams, ReflexRegisterResponse, ReplayRecordParams,
    ReplayRecordResponse, RoutineAutomateParams, RoutineAutomateResponse, RoutineInspectParams,
    RoutineInspectResponse, RoutineListParams, RoutineListResponse, RoutineMineParams,
    RoutineMineResponse, RoutineUpdateParams, RoutineUpdateResponse, StorageGcOnceParams,
    StorageGcOnceResponse, StorageInspectParams, StorageInspectResponse,
    StoragePressureSampleParams, StoragePressureSampleResponse, StoragePutProbeRowsParams,
    StoragePutProbeRowsResponse, SubscribeCancelParams, SubscribeCancelResponse, SubscribeParams,
    SubscribeResponse, SynapseService, TimelineExclusionsParams, TimelineExclusionsResponse,
    TimelinePauseParams, TimelinePauseResponse, TimelinePurgeParams, TimelinePurgeResponse,
    TimelineResumeParams, TimelineResumeResponse, TimelineSearchParams, TimelineSearchResponse,
    apply_storage_pressure_sample, cancel_file_jsonl_tail_watcher, cancel_reflex,
    cancel_subscription, decide_approval, decide_profile_authoring_candidate,
    disable_registry_profile, export_audit_bundle, export_profile_authoring_candidate,
    export_registry, generate_profile_authoring_candidate, generate_routine_automation_candidate,
    get_episode, history_reflexes, import_registry, inspect_profile_authoring_candidate,
    inspect_routine, inspect_storage, install_file_jsonl_tail_watcher, install_registry_package,
    list_approvals, list_episodes, list_local_models, list_profile_authoring_candidates,
    list_profiles, list_reflexes, list_routines, mine_and_store_routines, pause_timeline,
    prepare_activation_links, probe_local_model, purge_timeline, put_probe_rows,
    query_audit_intelligence, query_flags, query_registry, record_replay, refresh_profile_quality,
    register_local_model, register_reflex, remove_local_model, request_approval, resume_timeline,
    rollback_registry_profile, run_storage_gc_once, scan_storage, scan_text_tool, search_timeline,
    segment_episodes, start_demo_recording, stop_demo_recording, subscribe_to_events, tail_audio,
    tool, tool_router, transcribe_audio, update_approval_toast_state, update_local_model,
    update_routine, update_timeline_exclusions,
};
use rmcp::{RoleServer, service::RequestContext};
use serde_json::{Value, json};
use std::sync::Arc;

fn approval_toast_activation_callback(
    db: Arc<synapse_storage::Db>,
    bind: String,
) -> ToastActivationCallback {
    Arc::new(move |arguments| {
        let params = match crate::m3::approvals::parse_activation_uri(&arguments) {
            Ok(params) => params,
            Err(error) => {
                tracing::warn!(
                    code = "APPROVAL_TOAST_ACTIVATION_URI_INVALID",
                    detail = %error.message,
                    "approval toast activation argument rejected"
                );
                return;
            }
        };
        if params.bind != bind {
            tracing::warn!(
                code = "APPROVAL_TOAST_ACTIVATION_BIND_MISMATCH",
                approval_id = %params.approval_id,
                activation_id = %params.activation_id,
                decision = %params.decision,
                expected_bind = %bind,
                actual_bind = %params.bind,
                "approval toast activation refused because bind does not match this daemon"
            );
            return;
        }
        let approval_id = params.approval_id.clone();
        let activation_id = params.activation_id.clone();
        let decision = params.decision.clone();
        match crate::m3::approvals::decide_approval_from_activation(
            &db,
            &params,
            "approval_toast_activated",
        ) {
            Ok(response) => {
                match super::escalation::ack_from_approval_item_decision(
                    &db,
                    &response.decision.item,
                    decision.as_str(),
                    response.decision.item.decision_note.as_deref(),
                    "approval_toast_activated",
                    super::session_registry::unix_time_ms_now(),
                ) {
                    Ok(_maybe_escalation) => tracing::info!(
                        code = "APPROVAL_TOAST_ACTIVATION_DECIDED",
                        approval_id = %approval_id,
                        activation_id = %activation_id,
                        decision = %decision,
                        after_status = response.decision.after_status.as_str(),
                        "approval toast activation updated durable queue row"
                    ),
                    Err(error) => tracing::error!(
                        code = "ESCALATION_TOAST_ACTIVATION_ACK_FAILED",
                        approval_id = %approval_id,
                        activation_id = %activation_id,
                        decision = %decision,
                        detail = %error.message,
                        "approval toast activation updated durable queue row but failed to acknowledge linked escalation"
                    ),
                }
            }
            Err(error) => tracing::warn!(
                code = "APPROVAL_TOAST_ACTIVATION_FAILED",
                approval_id = %approval_id,
                activation_id = %activation_id,
                decision = %decision,
                detail = %error.message,
                "approval toast activation failed"
            ),
        }
    })
}

#[tool_router(router = m3_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(description = "Subscribe to filtered event notifications")]
    pub async fn subscribe(
        &self,
        params: Parameters<SubscribeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<SubscribeResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "subscribe",
            kinds_count = params.0.kinds.len(),
            snapshot_first = params.0.snapshot_first,
            buffer_size = params.0.buffer_size,
            "tool.invocation kind=subscribe"
        );
        self.require_m3_permissions(
            "subscribe",
            &crate::m3::subscribe::required_permissions(&params.0),
        )?;
        if crate::m3::subscribe::requires_a11y_event_bridge(&params.0) {
            self.ensure_a11y_event_bridge()?;
        }
        let owner_session_id =
            super::context::mcp_session_id_from_request_context(&request_context)?;
        let sse_state = self.sse_state()?;
        subscribe_to_events(&sse_state, &params.0, owner_session_id).map(Json)
    }

    #[tool(description = "Cancel an event subscription")]
    pub async fn subscribe_cancel(
        &self,
        params: Parameters<SubscribeCancelParams>,
    ) -> Result<Json<SubscribeCancelResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "subscribe_cancel",
            subscription_id = %params.0.subscription_id,
            "tool.invocation kind=subscribe_cancel"
        );
        self.require_m3_permissions(
            "subscribe_cancel",
            &crate::m3::subscribe::required_permissions_cancel(&params.0),
        )?;
        let sse_state = self.sse_state()?;
        cancel_subscription(&sse_state, &params.0).map(Json)
    }

    #[tool(description = "Register a reflex")]
    pub async fn reflex_register(
        &self,
        params: Parameters<ReflexRegisterParams>,
    ) -> Result<Json<ReflexRegisterResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "reflex_register",
            reflex_kind = %params.0.kind,
            priority = params.0.priority,
            "tool.invocation kind=reflex_register"
        );
        let params = params.0;
        let required = crate::m3::reflex::required_permissions_register(&params)?;
        self.require_m3_permissions("reflex_register", &required)?;
        let preflight = match self.ensure_supported_use_allows_action("reflex_register") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied("reflex_register", &error);
                return Err(error);
            }
        };
        self.refresh_reflex_audit_context()?;
        if crate::m3::reflex::requires_a11y_event_bridge(&params) {
            self.ensure_a11y_event_bridge()?;
        }
        let runtime = self.reflex_runtime()?;
        self.install_reflex_action_gate(&runtime)?;
        preflight.ensure_operator_panic_boundary("immediately_before_reflex_register")?;
        let response = register_reflex(&runtime, params.clone())?;
        if let Err(error) =
            preflight.ensure_operator_panic_boundary("immediately_after_reflex_register")
        {
            let rollback = ReflexCancelParams {
                reflex_id: response.reflex_id,
            };
            let _rollback_result = cancel_reflex(&runtime, &rollback);
            return Err(error);
        }
        if let Some(request) = params.file_jsonl_tail_watcher_request(response.reflex_id.clone()) {
            if let Err(error) =
                preflight.ensure_operator_panic_boundary("before_reflex_file_watcher_install")
            {
                let rollback = ReflexCancelParams {
                    reflex_id: response.reflex_id,
                };
                let _rollback_result = cancel_reflex(&runtime, &rollback);
                return Err(error);
            }
            let m3_state = self.m3_state_handle();
            let event_bus = self.sse_state()?.event_bus();
            if let Err(error) = install_file_jsonl_tail_watcher(&m3_state, request, event_bus) {
                let rollback = ReflexCancelParams {
                    reflex_id: response.reflex_id,
                };
                let _rollback_result = cancel_reflex(&runtime, &rollback);
                return Err(error);
            }
            if let Err(error) =
                preflight.ensure_operator_panic_boundary("after_reflex_file_watcher_install")
            {
                let _cancelled_watcher =
                    cancel_file_jsonl_tail_watcher(&m3_state, &response.reflex_id);
                let rollback = ReflexCancelParams {
                    reflex_id: response.reflex_id,
                };
                let _rollback_result = cancel_reflex(&runtime, &rollback);
                return Err(error);
            }
        }
        Ok(Json(response))
    }

    #[tool(description = "Cancel a reflex")]
    pub async fn reflex_cancel(
        &self,
        params: Parameters<ReflexCancelParams>,
    ) -> Result<Json<ReflexCancelResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "reflex_cancel",
            reflex_id = %params.0.reflex_id,
            "tool.invocation kind=reflex_cancel"
        );
        self.require_m3_permissions(
            "reflex_cancel",
            &crate::m3::reflex::required_permissions_cancel(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        let response = cancel_reflex(&runtime, &params.0)?;
        if response.cancelled {
            let _cancelled_watcher =
                cancel_file_jsonl_tail_watcher(&self.m3_state_handle(), &params.0.reflex_id)?;
        }
        Ok(Json(response))
    }

    #[tool(description = "List registered reflexes")]
    pub async fn reflex_list(
        &self,
        params: Parameters<ReflexListParams>,
    ) -> Result<Json<ReflexListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "reflex_list",
            include_expired = params.0.include_expired,
            "tool.invocation kind=reflex_list"
        );
        self.require_m3_permissions(
            "reflex_list",
            &crate::m3::reflex::required_permissions_list(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        list_reflexes(&runtime, &params.0).map(Json)
    }

    #[tool(description = "Return persisted reflex audit history")]
    pub async fn reflex_history(
        &self,
        params: Parameters<ReflexHistoryParams>,
    ) -> Result<Json<ReflexHistoryResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "reflex_history",
            reflex_id = ?params.0.reflex_id,
            limit = params.0.limit,
            "tool.invocation kind=reflex_history"
        );
        self.require_m3_permissions(
            "reflex_history",
            &crate::m3::reflex::required_permissions_history(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        history_reflexes(&runtime, &params.0).map(Json)
    }

    #[tool(description = "List loaded profiles")]
    pub async fn profile_list(
        &self,
        params: Parameters<ProfileListParams>,
    ) -> Result<Json<ProfileListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_list",
            "tool.invocation kind=profile_list"
        );
        self.require_m3_permissions(
            "profile_list",
            &crate::m3::profile::required_permissions_list(&params.0),
        )?;
        let runtime = self.profile_runtime()?;
        list_profiles(&runtime, &params.0).map(Json)
    }

    #[tool(description = "Activate a loaded profile by id")]
    pub async fn profile_activate(
        &self,
        params: Parameters<ProfileActivateParams>,
    ) -> Result<Json<ProfileActivateResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_activate",
            profile_id = %params.0.profile_id,
            "tool.invocation kind=profile_activate"
        );
        self.require_m3_permissions(
            "profile_activate",
            &crate::m3::profile::required_permissions_activate(&params.0),
        )?;
        let response = match self.activate_profile_locked(&params.0, self.allow_unknown_profile()?)
        {
            Ok(response) => response,
            Err(error) => {
                self.persist_profile_activation_denied(&params.0.profile_id, &error);
                return Err(error);
            }
        };
        self.apply_profile_runtime_config_for_profile(&response.active_profile_id)?;
        self.persist_profile_activation_success(&response.active_profile_id, response.changed)?;
        Ok(Json(response))
    }

    #[tool(description = "Generate a local candidate profile patch from replay and audit evidence")]
    pub async fn profile_authoring_generate(
        &self,
        params: Parameters<ProfileAuthoringGenerateParams>,
    ) -> Result<Json<ProfileAuthoringGenerateResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_authoring_generate",
            profile_id = %params.0.profile_id,
            max_audit_rows = params.0.max_audit_rows,
            max_replay_rows = params.0.max_replay_rows,
            replay_path = ?params.0.replay_path,
            "tool.invocation kind=profile_authoring_generate"
        );
        self.require_m3_permissions(
            "profile_authoring_generate",
            &crate::m3::profile_authoring::required_permissions_generate(&params.0),
        )?;
        let profile_runtime = self.profile_runtime()?;
        let reflex_runtime = self.reflex_runtime()?;
        generate_profile_authoring_candidate(&profile_runtime, &reflex_runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Promote one mined routine into a reviewable profile-authoring automation candidate (#861). Loads the mined routine, compiles/stores its setup plan, writes a normal profile_authoring candidate whose patch describes the full agent task including judgment-required steps, and writes routine_automation/v1/<routine_id> status so routine_inspect shows candidate/installed/rejected state. Use profile_authoring_inspect/decide/export for review and installation."
    )]
    pub async fn routine_automate(
        &self,
        params: Parameters<RoutineAutomateParams>,
    ) -> Result<Json<RoutineAutomateResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "routine_automate",
            routine_id = %params.0.routine_id,
            profile_id = ?params.0.profile_id,
            candidate_id = ?params.0.candidate_id,
            store_plan = params.0.store_plan,
            "tool.invocation kind=routine_automate"
        );
        self.require_m3_permissions(
            "routine_automate",
            &crate::m3::profile_authoring::required_permissions_routine_automate(&params.0),
        )?;
        let profile_runtime = self.profile_runtime()?;
        let reflex_runtime = self.reflex_runtime()?;
        let db = self.m3_storage()?;
        generate_routine_automation_candidate(&profile_runtime, &reflex_runtime, &db, &params.0)
            .map(Json)
    }

    #[tool(description = "List local profile authoring candidates")]
    pub async fn profile_authoring_list(
        &self,
        params: Parameters<ProfileAuthoringListParams>,
    ) -> Result<Json<ProfileAuthoringListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_authoring_list",
            profile_id = ?params.0.profile_id,
            state = ?params.0.state,
            limit = params.0.limit,
            "tool.invocation kind=profile_authoring_list"
        );
        self.require_m3_permissions(
            "profile_authoring_list",
            &crate::m3::profile_authoring::required_permissions_list(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        list_profile_authoring_candidates(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Inspect one local profile authoring candidate")]
    pub async fn profile_authoring_inspect(
        &self,
        params: Parameters<ProfileAuthoringInspectParams>,
    ) -> Result<Json<ProfileAuthoringInspectResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_authoring_inspect",
            candidate_id = %params.0.candidate_id,
            "tool.invocation kind=profile_authoring_inspect"
        );
        self.require_m3_permissions(
            "profile_authoring_inspect",
            &crate::m3::profile_authoring::required_permissions_inspect(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        inspect_profile_authoring_candidate(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Accept or reject one local profile authoring candidate")]
    pub async fn profile_authoring_decide(
        &self,
        params: Parameters<ProfileAuthoringDecideParams>,
    ) -> Result<Json<ProfileAuthoringDecideResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_authoring_decide",
            candidate_id = %params.0.candidate_id,
            decision = ?params.0.decision,
            "tool.invocation kind=profile_authoring_decide"
        );
        self.require_m3_permissions(
            "profile_authoring_decide",
            &crate::m3::profile_authoring::required_permissions_decide(&params.0),
        )?;
        let profile_runtime = self.profile_runtime()?;
        let reflex_runtime = self.reflex_runtime()?;
        let db = self.m3_storage()?;
        decide_profile_authoring_candidate(&profile_runtime, &reflex_runtime, &db, &params.0)
            .map(Json)
    }

    #[tool(description = "Export a local profile authoring candidate bundle")]
    pub async fn profile_authoring_export(
        &self,
        params: Parameters<ProfileAuthoringExportParams>,
    ) -> Result<Json<ProfileAuthoringExportResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_authoring_export",
            candidate_id = %params.0.candidate_id,
            output_path = %params.0.output_path,
            "tool.invocation kind=profile_authoring_export"
        );
        self.require_m3_permissions(
            "profile_authoring_export",
            &crate::m3::profile_authoring::required_permissions_export(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        export_profile_authoring_candidate(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Refresh local profile quality scoring from stored action, observation, and event rows"
    )]
    pub async fn profile_quality_refresh(
        &self,
        params: Parameters<ProfileQualityRefreshParams>,
    ) -> Result<Json<ProfileQualityRefreshResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_quality_refresh",
            profile_id = %params.0.profile_id,
            max_audit_rows = params.0.max_audit_rows,
            "tool.invocation kind=profile_quality_refresh"
        );
        self.require_m3_permissions(
            "profile_quality_refresh",
            &crate::m3::profile_quality::required_permissions_refresh(&params.0),
        )?;
        let profile_runtime = self.profile_runtime()?;
        let reflex_runtime = self.reflex_runtime()?;
        refresh_profile_quality(&profile_runtime, &reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Query local profile registry rows by view: search, inspect, or report")]
    pub async fn profile_registry_query(
        &self,
        params: Parameters<ProfileRegistryQueryParams>,
    ) -> Result<Json<ProfileRegistryQueryResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_registry_query",
            view = ?params.0.view,
            row_kind = ?params.0.row_kind,
            include_disabled = params.0.include_disabled,
            row_key = ?params.0.row_key,
            package_id = ?params.0.package_id,
            profile_id = ?params.0.profile_id,
            installed_profile_id = ?params.0.installed_profile_id,
            limit = params.0.limit,
            max_audit_rows = params.0.max_audit_rows,
            "tool.invocation kind=profile_registry_query"
        );
        self.require_m3_permissions(
            "profile_registry_query",
            &crate::m3::profile_registry::required_permissions_query(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        query_registry(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Install or update a local profile registry package manifest")]
    pub async fn profile_registry_install(
        &self,
        params: Parameters<ProfileRegistryInstallParams>,
    ) -> Result<Json<ProfileRegistryInstallResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_registry_install",
            source_id = %params.0.source_id,
            manifest_path = %params.0.manifest_path,
            "tool.invocation kind=profile_registry_install"
        );
        self.require_m3_permissions(
            "profile_registry_install",
            &crate::m3::profile_registry::required_permissions_install(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        install_registry_package(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Disable or remove an installed local profile registry row")]
    pub async fn profile_registry_disable(
        &self,
        params: Parameters<ProfileRegistryDisableParams>,
    ) -> Result<Json<ProfileRegistryDisableResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_registry_disable",
            profile_id = %params.0.profile_id,
            state = %params.0.state,
            "tool.invocation kind=profile_registry_disable"
        );
        self.require_m3_permissions(
            "profile_registry_disable",
            &crate::m3::profile_registry::required_permissions_disable(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        disable_registry_profile(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Export local profile registry rows to a JSON bundle")]
    pub async fn profile_registry_export(
        &self,
        params: Parameters<ProfileRegistryExportParams>,
    ) -> Result<Json<ProfileRegistryExportResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_registry_export",
            output_path = %params.0.output_path,
            row_kind = ?params.0.row_kind,
            limit = params.0.limit,
            "tool.invocation kind=profile_registry_export"
        );
        self.require_m3_permissions(
            "profile_registry_export",
            &crate::m3::profile_registry::required_permissions_export(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        export_registry(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Import a local profile registry JSON bundle")]
    pub async fn profile_registry_import(
        &self,
        params: Parameters<ProfileRegistryImportParams>,
    ) -> Result<Json<ProfileRegistryImportResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_registry_import",
            bundle_path = %params.0.bundle_path,
            "tool.invocation kind=profile_registry_import"
        );
        self.require_m3_permissions(
            "profile_registry_import",
            &crate::m3::profile_registry::required_permissions_import(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        import_registry(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Rollback an installed profile registry row to a prior trusted package")]
    pub async fn profile_registry_rollback(
        &self,
        params: Parameters<ProfileRegistryRollbackParams>,
    ) -> Result<Json<ProfileRegistryRollbackResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile_registry_rollback",
            profile_id = %params.0.profile_id,
            target_package_id = ?params.0.target_package_id,
            target_package_version = ?params.0.target_package_version,
            "tool.invocation kind=profile_registry_rollback"
        );
        self.require_m3_permissions(
            "profile_registry_rollback",
            &crate::m3::profile_registry::required_permissions_rollback(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        rollback_registry_profile(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Summarize profile-linked audit outcomes for registry intelligence")]
    pub async fn audit_intelligence_query(
        &self,
        params: Parameters<AuditIntelligenceQueryParams>,
    ) -> Result<Json<AuditIntelligenceQueryResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "audit_intelligence_query",
            profile_id = %params.0.profile_id,
            max_rows = params.0.max_rows,
            "tool.invocation kind=audit_intelligence_query"
        );
        self.require_m3_permissions(
            "audit_intelligence_query",
            &crate::m3::profile_registry::required_permissions_audit(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        query_audit_intelligence(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Export a local redacted audit bundle after explicit consent")]
    pub async fn audit_export_bundle(
        &self,
        params: Parameters<AuditExportBundleParams>,
    ) -> Result<Json<AuditExportBundleResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "audit_export_bundle",
            profile_id = %params.0.profile_id,
            output_path = %params.0.output_path,
            redaction_policy = ?params.0.redaction_policy,
            consent_present = params.0.consent.is_some(),
            max_rows = params.0.max_rows,
            "tool.invocation kind=audit_export_bundle"
        );
        self.require_m3_permissions(
            "audit_export_bundle",
            &crate::m3::audit_export::required_permissions_bundle(&params.0),
        )?;
        let reflex_runtime = self.reflex_runtime()?;
        export_audit_bundle(&reflex_runtime, &params.0).map(Json)
    }

    #[tool(description = "Record observations and/or events to a replay JSONL file")]
    pub async fn replay_record(
        &self,
        params: Parameters<ReplayRecordParams>,
    ) -> Result<Json<ReplayRecordResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "replay_record",
            target = %params.0.target,
            format = %params.0.format,
            duration_ms = params.0.duration_ms,
            "tool.invocation kind=replay_record"
        );
        self.require_m3_permissions(
            "replay_record",
            &crate::m3::replay::required_permissions(&params.0),
        )?;
        let sse_state = self.sse_state()?;
        record_replay(self.m1_state.clone(), sse_state, &params.0)
            .await
            .map(Json)
    }

    #[tool(
        description = "Arm an explicit high-fidelity UIA demonstration recording for profile authoring. While active, the existing WinEvent bridge writes TimelineKind::DemoMarker rows for focus/value/name/selection/menu/alert events; demo_record_stop exports a replay JSONL bundle consumable by profile_authoring_generate."
    )]
    pub async fn demo_record_start(
        &self,
        params: Parameters<DemoRecordStartParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<DemoRecordStartResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "demo_record_start",
            profile_id = %params.0.profile_id,
            duration_ms = params.0.duration_ms,
            path = ?params.0.path,
            "tool.invocation kind=demo_record_start"
        );
        self.require_m3_permissions(
            "demo_record_start",
            &crate::m3::demo_recording::required_permissions_start(&params.0),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let params = params.0;
        let command_payload = json!({
            "profile_id": &params.profile_id,
            "duration_ms": params.duration_ms,
            "path": &params.path,
            "label": &params.label,
        });
        let command_before = json!({
            "source_of_truth": "CF_KV timeline/demo-record/v1 plus CF_TIMELINE DemoMarker rows",
            "by_session": &by_session,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "demo_record_start",
            "demo_record_start",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let result = start_demo_recording(&self.m3_state, &params, &by_session);
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "demo_record_start",
                    "demo_record_start",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV timeline/demo-record/v1 plus CF_TIMELINE DemoMarker rows",
                        "response": response,
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "demo_record_start",
                    "demo_record_start",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV timeline/demo-record/v1 plus CF_TIMELINE DemoMarker rows",
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        result.map(Json)
    }

    #[tool(
        description = "Stop the active explicit UIA demonstration recording and export its DemoMarker rows to replay JSONL. The returned replay_path can be passed directly to profile_authoring_generate for the same profile_id."
    )]
    pub async fn demo_record_stop(
        &self,
        params: Parameters<DemoRecordStopParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<DemoRecordStopResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "demo_record_stop",
            demo_id = ?params.0.demo_id,
            "tool.invocation kind=demo_record_stop"
        );
        self.require_m3_permissions(
            "demo_record_stop",
            &crate::m3::demo_recording::required_permissions_stop(&params.0),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let params = params.0;
        let command_payload = json!({
            "demo_id": &params.demo_id,
        });
        let command_before = json!({
            "source_of_truth": "CF_KV timeline/demo-record/v1, CF_TIMELINE DemoMarker rows, and replay JSONL file",
            "by_session": &by_session,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "demo_record_stop",
            "demo_record_stop",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let result = stop_demo_recording(&self.m3_state, &params, &by_session);
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "demo_record_stop",
                    "demo_record_stop",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV timeline/demo-record/v1, CF_TIMELINE DemoMarker rows, and replay JSONL file",
                        "response": response,
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "demo_record_stop",
                    "demo_record_stop",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV timeline/demo-record/v1, CF_TIMELINE DemoMarker rows, and replay JSONL file",
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        result.map(Json)
    }

    #[tool(description = "Return the latest loopback audio tail as PCM s16le bytes")]
    pub async fn audio_tail(
        &self,
        params: Parameters<AudioTailParams>,
    ) -> Result<Json<AudioTailResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "audio_tail",
            seconds = params.0.seconds,
            "tool.invocation kind=audio_tail"
        );
        self.require_m3_permissions(
            "audio_tail",
            &crate::m3::audio::required_permissions_tail(&params.0),
        )?;
        tail_audio(&self.m3_state, &params.0).map(Json)
    }

    #[tool(description = "Transcribe the latest loopback audio tail with Whisper tiny")]
    pub async fn audio_transcribe(
        &self,
        params: Parameters<AudioTranscribeParams>,
    ) -> Result<Json<AudioTranscribeResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "audio_transcribe",
            seconds = params.0.seconds,
            language = %params.0.language,
            "tool.invocation kind=audio_transcribe"
        );
        self.require_m3_permissions(
            "audio_transcribe",
            &crate::m3::audio::required_permissions_transcribe(&params.0),
        )?;
        transcribe_audio(&self.m3_state, &params.0).map(Json)
    }

    #[tool(
        description = "Enqueue a durable human decision request in CF_KV. Supports suggestion, agent_escalation, and armed_run_review items; timeout defaults can ignore/decline but never accept. Optional payload_json is JSON text, not an open schema value."
    )]
    pub async fn approval_request(
        &self,
        params: Parameters<ApprovalRequestParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ApprovalRequestResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "approval_request",
            approval_kind = ?params.kind,
            has_timeout = params.timeout_ms.is_some(),
            destructive = params.destructive,
            notify = params.notify,
            suppress_popup = params.suppress_popup,
            "tool.invocation kind=approval_request"
        );
        self.require_m3_permissions(
            "approval_request",
            &crate::m3::approvals::required_permissions_request(&params),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let db = self.m3_storage()?;
        let command_payload = json!({
            "kind": &params.kind,
            "title": &params.title,
            "body": &params.body,
            "payload_json": &params.payload_json,
            "dedupe_key": &params.dedupe_key,
            "destructive": params.destructive,
            "notify": params.notify,
            "suppress_popup": params.suppress_popup,
            "timeout_ms": params.timeout_ms,
            "timeout_decision": &params.timeout_decision,
        });
        let command_before = json!({
            "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
            "by_session": &by_session,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "approval_request",
            "approval_request",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let mut response = match request_approval(&db, &params, &by_session) {
            Ok(response) => response,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "approval_request",
                        "approval_request",
                        Some(by_session.clone()),
                        Some(by_session.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                        }),
                        "error",
                    )
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };
        if params.notify && !response.deduped {
            let (delivery, toast_audit_row) =
                match crate::approval_protocol::ensure_protocol_handler_registered() {
                    Ok(_protocol_readback) => {
                        let bind = self.m3_bind_addr()?;
                        match prepare_activation_links(&db, &response.item.approval_id, &bind) {
                            Ok(links) => {
                                let tag = toast_tag_for(Some(&format!(
                                    "approval:{}",
                                    response.item.approval_id
                                )));
                                let notify = NotifyHumanParams {
                                    title: response.item.title.clone(),
                                    body: response.item.body.clone(),
                                    kind: if response.item.destructive {
                                        NotifyKind::Warning
                                    } else {
                                        NotifyKind::Info
                                    },
                                    dedupe_key: Some(format!(
                                        "approval:{}",
                                        response.item.approval_id
                                    )),
                                    suppress_popup: response.item.toast.suppress_popup,
                                };
                                let actions = vec![
                                    ToastAction {
                                        content: "Accept".to_owned(),
                                        arguments: links.accept_uri,
                                        activation_type: ToastActionActivationType::Foreground,
                                    },
                                    ToastAction {
                                        content: "Decline".to_owned(),
                                        arguments: links.decline_uri,
                                        activation_type: ToastActionActivationType::Foreground,
                                    },
                                    ToastAction {
                                        content: "Snooze".to_owned(),
                                        arguments: links.snooze_uri,
                                        activation_type: ToastActionActivationType::Foreground,
                                    },
                                ];
                                let activation_callback =
                                    approval_toast_activation_callback(db.clone(), bind);
                                match run_internal_toast_with_activation(
                                    notify,
                                    tag.clone(),
                                    actions,
                                    activation_callback,
                                )
                                .await
                                {
                                    Ok(toast) => {
                                        let delivery = ApprovalToastDelivery {
                                            requested: true,
                                            suppress_popup: response.item.toast.suppress_popup,
                                            actionable_buttons: true,
                                            activation_id: Some(links.activation_id),
                                            protocol_handler_registered: Some(true),
                                            unavailable_reason: None,
                                            notify_tag: Some(toast.tag),
                                            notify_group: Some(toast.group),
                                            notification_setting: Some(toast.notification_setting),
                                            verified_in_history: Some(toast.verified_in_history),
                                        };
                                        let (item, item_row, audit_row) =
                                            match update_approval_toast_state(
                                                &db,
                                                &response.item.approval_id,
                                                delivery.clone(),
                                                &by_session,
                                            ) {
                                                Ok(readback) => readback,
                                                Err(error) => {
                                                    self.command_audit_final(
                                                    super::command_audit::CommandAuditInput::mcp(
                                                        "approval_request",
                                                        "approval_request",
                                                        Some(by_session.clone()),
                                                        Some(by_session.clone()),
                                                        command_payload.clone(),
                                                        command_before.clone(),
                                                        json!({
                                                            "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                                                            "approval_id": &response.item.approval_id,
                                                            "deduped": response.deduped,
                                                            "item_row": &response.item_row,
                                                            "audit_row": &response.audit_row,
                                                        }),
                                                        "error",
                                                    )
                                                    .with_error(
                                                        super::command_audit::command_audit_error_from_error_data(&error),
                                                    ),
                                                )?;
                                                    return Err(error);
                                                }
                                            };
                                        response.item = item;
                                        response.item_row = item_row;
                                        (delivery, Some(audit_row))
                                    }
                                    Err(error) => {
                                        let delivery = ApprovalToastDelivery {
                                            requested: true,
                                            suppress_popup: response.item.toast.suppress_popup,
                                            actionable_buttons: false,
                                            activation_id: Some(links.activation_id),
                                            protocol_handler_registered: Some(true),
                                            unavailable_reason: Some(format!(
                                                "approval actionable toast delivery failed: {}",
                                                error.message
                                            )),
                                            notify_tag: Some(tag),
                                            notify_group: Some(SYNAPSE_TOAST_GROUP.to_owned()),
                                            notification_setting: None,
                                            verified_in_history: Some(false),
                                        };
                                        let (item, item_row, audit_row) =
                                            match update_approval_toast_state(
                                                &db,
                                                &response.item.approval_id,
                                                delivery.clone(),
                                                &by_session,
                                            ) {
                                                Ok(readback) => readback,
                                                Err(error) => {
                                                    self.command_audit_final(
                                                    super::command_audit::CommandAuditInput::mcp(
                                                        "approval_request",
                                                        "approval_request",
                                                        Some(by_session.clone()),
                                                        Some(by_session.clone()),
                                                        command_payload.clone(),
                                                        command_before.clone(),
                                                        json!({
                                                            "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                                                            "approval_id": &response.item.approval_id,
                                                            "deduped": response.deduped,
                                                            "item_row": &response.item_row,
                                                            "audit_row": &response.audit_row,
                                                        }),
                                                        "error",
                                                    )
                                                    .with_error(
                                                        super::command_audit::command_audit_error_from_error_data(&error),
                                                    ),
                                                )?;
                                                    return Err(error);
                                                }
                                            };
                                        response.item = item;
                                        response.item_row = item_row;
                                        (delivery, Some(audit_row))
                                    }
                                }
                            }
                            Err(error) => {
                                let delivery = ApprovalToastDelivery {
                                    requested: true,
                                    suppress_popup: response.item.toast.suppress_popup,
                                    actionable_buttons: false,
                                    activation_id: None,
                                    protocol_handler_registered: Some(true),
                                    unavailable_reason: Some(format!(
                                        "approval activation link preparation failed: {}",
                                        error.message
                                    )),
                                    notify_tag: None,
                                    notify_group: None,
                                    notification_setting: None,
                                    verified_in_history: Some(false),
                                };
                                let (item, item_row, audit_row) = match update_approval_toast_state(
                                    &db,
                                    &response.item.approval_id,
                                    delivery.clone(),
                                    &by_session,
                                ) {
                                    Ok(readback) => readback,
                                    Err(error) => {
                                        self.command_audit_final(
                                            super::command_audit::CommandAuditInput::mcp(
                                                "approval_request",
                                                "approval_request",
                                                Some(by_session.clone()),
                                                Some(by_session.clone()),
                                                command_payload.clone(),
                                                command_before.clone(),
                                                json!({
                                                    "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                                                    "approval_id": &response.item.approval_id,
                                                    "deduped": response.deduped,
                                                    "item_row": &response.item_row,
                                                    "audit_row": &response.audit_row,
                                                }),
                                                "error",
                                            )
                                            .with_error(
                                                super::command_audit::command_audit_error_from_error_data(&error),
                                            ),
                                        )?;
                                        return Err(error);
                                    }
                                };
                                response.item = item;
                                response.item_row = item_row;
                                (delivery, Some(audit_row))
                            }
                        }
                    }
                    Err(message) => {
                        let delivery = ApprovalToastDelivery {
                            requested: true,
                            suppress_popup: response.item.toast.suppress_popup,
                            actionable_buttons: false,
                            activation_id: None,
                            protocol_handler_registered: Some(false),
                            unavailable_reason: Some(format!(
                                "approval protocol handler registration failed: {message}"
                            )),
                            notify_tag: None,
                            notify_group: None,
                            notification_setting: None,
                            verified_in_history: Some(false),
                        };
                        let (item, item_row, audit_row) = match update_approval_toast_state(
                            &db,
                            &response.item.approval_id,
                            delivery.clone(),
                            &by_session,
                        ) {
                            Ok(readback) => readback,
                            Err(error) => {
                                self.command_audit_final(
                                    super::command_audit::CommandAuditInput::mcp(
                                        "approval_request",
                                        "approval_request",
                                        Some(by_session.clone()),
                                        Some(by_session.clone()),
                                        command_payload.clone(),
                                        command_before.clone(),
                                        json!({
                                            "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                                            "approval_id": &response.item.approval_id,
                                            "deduped": response.deduped,
                                            "item_row": &response.item_row,
                                            "audit_row": &response.audit_row,
                                        }),
                                        "error",
                                    )
                                    .with_error(
                                        super::command_audit::command_audit_error_from_error_data(&error),
                                    ),
                                )?;
                                return Err(error);
                            }
                        };
                        response.item = item;
                        response.item_row = item_row;
                        (delivery, Some(audit_row))
                    }
                };
            tracing::info!(
                code = "APPROVAL_TOAST_DELIVERY_RECORDED",
                approval_id = %response.item.approval_id,
                actionable_buttons = delivery.actionable_buttons,
                unavailable_reason = delivery.unavailable_reason.as_deref().unwrap_or(""),
                "approval_request toast delivery state recorded"
            );
            response.toast_audit_row = toast_audit_row;
        }
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "approval_request",
            "approval_request",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                "approval_id": &response.item.approval_id,
                "deduped": response.deduped,
                "item_row": &response.item_row,
                "audit_row": &response.audit_row,
                "toast_audit_row": &response.toast_audit_row,
            }),
            "ok",
        ))?;
        self.publish_approval_queue_event(
            crate::server::APPROVAL_REQUEST_EVENT_KIND,
            &response.item.approval_id,
            Some(response.item.status.as_str()),
            &by_session,
            "approval_request",
            json!({
                "kind": response.item.kind.as_str(),
                "deduped": response.deduped,
                "item_row": &response.item_row,
                "audit_row": &response.audit_row,
                "toast_audit_row": &response.toast_audit_row,
            }),
        );
        Ok(Json(response))
    }

    #[tool(
        description = "List durable approval/suggestion queue rows from CF_KV. Materializes expired pending/snoozed rows to their timeout default and audit-logs that transition before returning."
    )]
    pub async fn approval_list(
        &self,
        params: Parameters<ApprovalListParams>,
    ) -> Result<Json<ApprovalListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "approval_list",
            include_terminal = params.0.include_terminal,
            limit = params.0.limit,
            has_cursor = params.0.cursor.is_some(),
            "tool.invocation kind=approval_list"
        );
        self.require_m3_permissions(
            "approval_list",
            &crate::m3::approvals::required_permissions_list(&params.0),
        )?;
        let db = self.m3_storage()?;
        list_approvals(&db, &params.0).map(Json)
    }

    #[tool(
        description = "Resolve one durable approval queue item as accept, decline, or snooze. Writes and separately reads back the CF_KV item row plus transition audit row."
    )]
    pub async fn approval_decide(
        &self,
        params: Parameters<ApprovalDecideParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ApprovalDecideResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "approval_decide",
            approval_id = %params.0.approval_id,
            decision = ?params.0.decision,
            has_note = params.0.note.is_some(),
            snooze_ms = params.0.snooze_ms,
            "tool.invocation kind=approval_decide"
        );
        self.require_m3_permissions(
            "approval_decide",
            &crate::m3::approvals::required_permissions_decide(&params.0),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let db = self.m3_storage()?;
        let params = params.0;
        let command_payload = json!({
            "approval_id": &params.approval_id,
            "decision": params.decision.as_str(),
            "note_present": params.note.is_some(),
            "snooze_ms": params.snooze_ms,
        });
        let command_before = json!({
            "source_of_truth": "CF_KV approval queue rows plus approval transition audit rows",
            "approval_id": &params.approval_id,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "approval_decide",
            "approval_decision",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let result = match decide_approval(&db, &params, &by_session) {
            Ok(response) => {
                match super::escalation::ack_from_approval_item_decision(
                    &db,
                    &response.item,
                    params.decision.as_str(),
                    response.item.decision_note.as_deref(),
                    &by_session,
                    super::session_registry::unix_time_ms_now(),
                ) {
                    Ok(_maybe_escalation) => Ok(response),
                    Err(error) => {
                        tracing::error!(
                            code = "ESCALATION_APPROVAL_ACK_FAILED",
                            approval_id = %params.approval_id,
                            decision = params.decision.as_str(),
                            detail = %error.message,
                            "approval row was decided but linked escalation ack failed"
                        );
                        Err(error)
                    }
                }
            }
            Err(error) => Err(error),
        };
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "approval_decide",
                    "approval_decision",
                    Some(by_session.clone()),
                    Some(by_session.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV approval queue rows plus approval transition audit rows",
                        "approval_id": &response.approval_id,
                        "before_status": response.before_status.as_str(),
                        "after_status": response.after_status.as_str(),
                        "item_row": &response.item_row,
                        "audit_row": &response.audit_row,
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "approval_decide",
                    "approval_decision",
                    Some(by_session.clone()),
                    Some(by_session.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV approval queue rows plus approval transition audit rows",
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        if let Ok(response) = &result {
            self.publish_approval_queue_event(
                crate::server::APPROVAL_DECISION_EVENT_KIND,
                &response.approval_id,
                Some(response.after_status.as_str()),
                &by_session,
                "approval_decide",
                json!({
                    "before_status": response.before_status.as_str(),
                    "after_status": response.after_status.as_str(),
                    "item_row": &response.item_row,
                    "audit_row": &response.audit_row,
                }),
            );
        }
        result.map(Json)
    }

    #[tool(
        description = "Score one text blob for prompt-injection/adversarial instruction heuristics; optionally persist source-linked flag rows"
    )]
    pub async fn hygiene_scan_text(
        &self,
        params: Parameters<HygieneScanTextParams>,
    ) -> Result<Json<HygieneScanTextResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "hygiene_scan_text",
            text_bytes = params.0.text.len(),
            persist = params.0.persist,
            "tool.invocation kind=hygiene_scan_text"
        );
        self.require_m3_permissions(
            "hygiene_scan_text",
            &crate::m3::hygiene::required_permissions_scan_text(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        scan_text_tool(&runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Batch-scan CF_OBSERVATIONS/CF_TIMELINE text fields for prompt-injection heuristics and persist source-linked flag rows"
    )]
    pub async fn hygiene_scan_storage(
        &self,
        params: Parameters<HygieneScanStorageParams>,
    ) -> Result<Json<HygieneScanStorageResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "hygiene_scan_storage",
            limit_rows = params.0.limit_rows,
            flag_limit = params.0.flag_limit,
            has_cursor = params.0.cursor.is_some(),
            "tool.invocation kind=hygiene_scan_storage"
        );
        self.require_m3_permissions(
            "hygiene_scan_storage",
            &crate::m3::hygiene::required_permissions_scan_storage(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        scan_storage(&runtime, &params.0).map(Json)
    }

    #[tool(description = "Query prompt-injection hygiene flag rows persisted in CF_KV")]
    pub async fn hygiene_flags(
        &self,
        params: Parameters<HygieneFlagsParams>,
    ) -> Result<Json<HygieneFlagsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "hygiene_flags",
            source_cf = ?params.0.source_cf,
            source_key_hex = ?params.0.source_key_hex,
            limit = params.0.limit,
            has_cursor = params.0.cursor.is_some(),
            "tool.invocation kind=hygiene_flags"
        );
        self.require_m3_permissions(
            "hygiene_flags",
            &crate::m3::hygiene::required_permissions_flags(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        query_flags(&runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Register one operator-supplied OpenAI-compatible local model endpoint in CF_KV after a forced structured tool-call probe. Stores no API key value; api_key_env_var is an environment variable name only."
    )]
    pub async fn local_model_register(
        &self,
        params: Parameters<LocalModelRegisterParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<LocalModelRegisterResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "local_model_register",
            name = %params.name,
            model_id = %params.model_id,
            has_api_key_env_var = params.api_key_env_var.is_some(),
            "tool.invocation kind=local_model_register"
        );
        self.require_m3_permissions(
            "local_model_register",
            &crate::m3::local_models::required_permissions_register(&params),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let db = self.m3_storage()?;
        register_local_model(&db, params, &by_session)
            .await
            .map(Json)
    }

    #[tool(description = "List operator-supplied local model registry rows from CF_KV")]
    pub async fn local_model_list(
        &self,
        params: Parameters<LocalModelListParams>,
    ) -> Result<Json<LocalModelListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "local_model_list",
            name = ?params.0.name,
            include_disabled = params.0.include_disabled,
            limit = params.0.limit,
            "tool.invocation kind=local_model_list"
        );
        self.require_m3_permissions(
            "local_model_list",
            &crate::m3::local_models::required_permissions_list(&params.0),
        )?;
        let db = self.m3_storage()?;
        list_local_models(&db, &params.0).map(Json)
    }

    #[tool(
        description = "Update one local model registry row in CF_KV. Endpoint/model/API-key changes are accepted only after the same forced structured tool-call probe passes."
    )]
    pub async fn local_model_update(
        &self,
        params: Parameters<LocalModelUpdateParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<LocalModelUpdateResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "local_model_update",
            name = %params.name,
            new_name = ?params.new_name,
            changes_endpoint = params.base_url.is_some() || params.model_id.is_some() || params.api_shape.is_some(),
            "tool.invocation kind=local_model_update"
        );
        self.require_m3_permissions(
            "local_model_update",
            &crate::m3::local_models::required_permissions_update(&params),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let db = self.m3_storage()?;
        update_local_model(&db, params, &by_session).await.map(Json)
    }

    #[tool(description = "Remove one operator-supplied local model registry row from CF_KV")]
    pub async fn local_model_remove(
        &self,
        params: Parameters<LocalModelRemoveParams>,
    ) -> Result<Json<LocalModelRemoveResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "local_model_remove",
            name = %params.0.name,
            "tool.invocation kind=local_model_remove"
        );
        self.require_m3_permissions(
            "local_model_remove",
            &crate::m3::local_models::required_permissions_remove(&params.0),
        )?;
        let db = self.m3_storage()?;
        remove_local_model(&db, &params.0).map(Json)
    }

    #[tool(
        description = "Re-probe one local model registry row with a forced structured tool-call request, then persist healthy/unhealthy latency and token-rate metadata in CF_KV."
    )]
    pub async fn local_model_probe(
        &self,
        params: Parameters<LocalModelProbeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<LocalModelProbeResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "local_model_probe",
            name = %params.name,
            timeout_ms = ?params.timeout_ms,
            "tool.invocation kind=local_model_probe"
        );
        self.require_m3_permissions(
            "local_model_probe",
            &crate::m3::local_models::required_permissions_probe(&params),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let db = self.m3_storage()?;
        probe_local_model(&db, &params, &by_session).await.map(Json)
    }

    #[tool(description = "Inspect storage sizes, row counts, and pressure state")]
    pub async fn storage_inspect(
        &self,
        params: Parameters<StorageInspectParams>,
    ) -> Result<Json<StorageInspectResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "storage_inspect",
            "tool.invocation kind=storage_inspect"
        );
        self.require_m3_permissions(
            "storage_inspect",
            &crate::m3::storage::required_permissions_inspect(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        inspect_storage(&runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Search the operator activity timeline (CF_TIMELINE) by time range, app, kind, actor, and case-insensitive text over titles/paths/URLs; pages via cursor"
    )]
    pub async fn timeline_search(
        &self,
        params: Parameters<TimelineSearchParams>,
    ) -> Result<Json<TimelineSearchResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_search",
            start_ts_ns = params.0.start_ts_ns,
            end_ts_ns = params.0.end_ts_ns,
            has_text = params.0.text.is_some(),
            limit = params.0.limit,
            has_cursor = params.0.cursor.is_some(),
            "tool.invocation kind=timeline_search"
        );
        self.require_m3_permissions(
            "timeline_search",
            &crate::m3::timeline::required_permissions(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        search_timeline(&runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Pause the operator activity timeline recorder: zero new rows across all feeds until timeline_resume. State survives daemon restart; optional duration_ms arms an auto-resume deadline"
    )]
    pub async fn timeline_pause(
        &self,
        params: Parameters<TimelinePauseParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TimelinePauseResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_pause",
            duration_ms = params.0.duration_ms,
            "tool.invocation kind=timeline_pause"
        );
        self.require_m3_permissions(
            "timeline_pause",
            &crate::m3::timeline_control::required_permissions_pause(&params.0),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let params = params.0;
        let command_payload = json!({ "duration_ms": params.duration_ms });
        let command_before = json!({
            "source_of_truth": "CF_KV timeline/control/v1 plus CF_TIMELINE boundary rows",
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "timeline_pause",
            "pause",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let result = pause_timeline(&self.m3_state, &params, &by_session);
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "timeline_pause",
                    "pause",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV timeline/control/v1 plus CF_TIMELINE boundary rows",
                        "response": response,
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "timeline_pause",
                    "pause",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV timeline/control/v1 plus CF_TIMELINE boundary rows",
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        result.map(Json)
    }

    #[tool(
        description = "Resume the operator activity timeline recorder after timeline_pause; writes and verifies a session_start boundary row"
    )]
    pub async fn timeline_resume(
        &self,
        params: Parameters<TimelineResumeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TimelineResumeResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_resume",
            "tool.invocation kind=timeline_resume"
        );
        self.require_m3_permissions(
            "timeline_resume",
            &crate::m3::timeline_control::required_permissions_resume(&params.0),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let params = params.0;
        let command_payload = json!({});
        let command_before = json!({
            "source_of_truth": "CF_KV timeline/control/v1 plus CF_TIMELINE boundary rows",
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "timeline_resume",
            "resume",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let result = resume_timeline(&self.m3_state, &params, &by_session);
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "timeline_resume",
                    "resume",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV timeline/control/v1 plus CF_TIMELINE boundary rows",
                        "response": response,
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "timeline_resume",
                    "resume",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV timeline/control/v1 plus CF_TIMELINE boundary rows",
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        result.map(Json)
    }

    #[tool(
        description = "List or mutate the timeline recorder's per-process exclusion list; excluded executables (e.g. keepass.exe) never produce timeline rows. Env baseline (SYNAPSE_TIMELINE_EXCLUDE) is immutable at runtime"
    )]
    pub async fn timeline_exclusions(
        &self,
        params: Parameters<TimelineExclusionsParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TimelineExclusionsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_exclusions",
            add_count = params.0.add.as_deref().unwrap_or_default().len(),
            remove_count = params.0.remove.as_deref().unwrap_or_default().len(),
            "tool.invocation kind=timeline_exclusions"
        );
        self.require_m3_permissions(
            "timeline_exclusions",
            &crate::m3::timeline_control::required_permissions_exclusions(&params.0),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let params = params.0;
        let command_payload = json!({
            "add": &params.add,
            "remove": &params.remove,
        });
        let command_before = json!({
            "source_of_truth": "timeline control runtime exclusion set plus CF_KV timeline control rows",
            "by_session": &by_session,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "timeline_exclusions",
            "timeline_exclusions",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let result = update_timeline_exclusions(&self.m3_state, &params, &by_session);
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "timeline_exclusions",
                    "timeline_exclusions",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "timeline control runtime exclusion set plus CF_KV timeline control rows",
                        "response": response,
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "timeline_exclusions",
                    "timeline_exclusions",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "timeline control runtime exclusion set plus CF_KV timeline control rows",
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        result.map(Json)
    }

    #[tool(
        description = "Hard-delete operator timeline rows matching the filters (same semantics as timeline_search) and write a counts-only audit row. Requires at least one filter or all=true; supports dry_run; purge audit rows are only deleted when kinds explicitly includes \"purge\""
    )]
    pub async fn timeline_purge(
        &self,
        params: Parameters<TimelinePurgeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TimelinePurgeResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_purge",
            all = params.0.all,
            dry_run = params.0.dry_run,
            has_text = params.0.text.is_some(),
            has_cursor = params.0.cursor.is_some(),
            "tool.invocation kind=timeline_purge"
        );
        self.require_m3_permissions(
            "timeline_purge",
            &crate::m3::timeline::required_permissions_purge(&params.0),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let runtime = self.reflex_runtime()?;
        let params = params.0;
        let command_payload = json!({
            "start_ts_ns": params.start_ts_ns,
            "end_ts_ns": params.end_ts_ns,
            "apps": &params.apps,
            "text_present": params.text.is_some(),
            "kinds": &params.kinds,
            "actor": &params.actor,
            "all": params.all,
            "dry_run": params.dry_run,
            "cursor": &params.cursor,
        });
        let command_before = json!({
            "source_of_truth": "CF_TIMELINE rows plus CF_TIMELINE purge audit row",
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "timeline_purge",
            "purge",
            Some(by_session.clone()),
            Some(by_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let result = purge_timeline(&runtime, &params, &by_session);
        match &result {
            Ok(response) => {
                self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
                    "timeline_purge",
                    "purge",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_TIMELINE rows plus CF_TIMELINE purge audit row",
                        "response": response,
                    }),
                    "ok",
                ))?
            }
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "timeline_purge",
                    "purge",
                    Some(by_session.clone()),
                    Some(by_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_TIMELINE rows plus CF_TIMELINE purge audit row",
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(error),
                ),
            )?,
        };
        result.map(Json)
    }

    #[tool(
        description = "Segment the operator activity timeline (CF_TIMELINE) into derived episodes (CF_EPISODES): contiguous spans of focused work with app, document, duration, and interaction summary. Deterministic and re-runnable; the range snaps outward to whole local days and each day is replaced atomically, so re-segmentation is idempotent"
    )]
    pub async fn episode_segment(
        &self,
        params: Parameters<EpisodeSegmentParams>,
    ) -> Result<Json<EpisodeSegmentResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "episode_segment",
            start_ts_ns = params.0.start_ts_ns,
            end_ts_ns = params.0.end_ts_ns,
            include_agent_activity = params.0.include_agent_activity,
            dry_run = params.0.dry_run,
            "tool.invocation kind=episode_segment"
        );
        self.require_m3_permissions(
            "episode_segment",
            &crate::m3::episodes::required_permissions(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        segment_episodes(&runtime, &params.0).map(Json)
    }

    #[tool(
        description = "List derived episodes (CF_EPISODES) overlapping a time range, ordered chronologically with stable ids, durations, app/document identity, and interaction summaries; filters by app, actor, and minimum duration; pages via cursor. Run episode_segment first to materialize episodes"
    )]
    pub async fn episode_list(
        &self,
        params: Parameters<EpisodeListParams>,
    ) -> Result<Json<EpisodeListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "episode_list",
            start_ts_ns = params.0.start_ts_ns,
            end_ts_ns = params.0.end_ts_ns,
            apps_count = params.0.apps.as_deref().unwrap_or_default().len(),
            actor = ?params.0.actor,
            min_duration_ms = params.0.min_duration_ms,
            limit = params.0.limit,
            has_cursor = params.0.cursor.is_some(),
            "tool.invocation kind=episode_list"
        );
        self.require_m3_permissions(
            "episode_list",
            &crate::m3::episodes::required_permissions_list(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        list_episodes(&runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Fetch one derived episode by its stable id, with the underlying CF_TIMELINE evidence row references inside its span (paged via refs_cursor). Optional start_ts_ns seeks the id lookup; refs include the closing boundary row and agent rows"
    )]
    pub async fn episode_get(
        &self,
        params: Parameters<EpisodeGetParams>,
    ) -> Result<Json<EpisodeGetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "episode_get",
            episode_id = %params.0.episode_id,
            start_ts_ns = params.0.start_ts_ns,
            refs_limit = params.0.refs_limit,
            has_refs_cursor = params.0.refs_cursor.is_some(),
            "tool.invocation kind=episode_get"
        );
        self.require_m3_permissions(
            "episode_get",
            &crate::m3::episodes::required_permissions_get(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        get_episode(&runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Mine recurring routines from derived episodes (CF_EPISODES) into CF_ROUTINES: frequent episode-identity sequences with temporally regular schedules (circular time-of-day clustering, day-of-week classification) and honest Wilson-lower-bound confidence. Deterministic and re-runnable; each run replaces the routine store atomically. Run episode_segment first to materialize episodes"
    )]
    pub async fn routine_mine(
        &self,
        params: Parameters<RoutineMineParams>,
    ) -> Result<Json<RoutineMineResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "routine_mine",
            start_ts_ns = params.0.start_ts_ns,
            end_ts_ns = params.0.end_ts_ns,
            min_support_days = params.0.min_support_days,
            max_pattern_len = params.0.max_pattern_len,
            include_agent_activity = params.0.include_agent_activity,
            dry_run = params.0.dry_run,
            "tool.invocation kind=routine_mine"
        );
        self.require_m3_permissions(
            "routine_mine",
            &crate::m3::routines::required_permissions(&params.0),
        )?;
        let db = self.m3_storage()?;
        mine_and_store_routines(&db, &params.0).map(Json)
    }

    #[tool(
        description = "List mined routines (CF_ROUTINES) joined with their operator lifecycle state (CF_ROUTINE_STATE) and hygiene taint ledger status: candidate/confirmed/disabled/archived, labels, confidence, schedule, tainted flag, and taint provenance when a hygiene/taint/v1 routine row exists. Filters by lifecycle, minimum confidence, app, and granularity; include_unmined also lists lifecycle rows whose routine the last mine no longer derived. Run routine_mine first to materialize routines"
    )]
    pub async fn routine_list(
        &self,
        params: Parameters<RoutineListParams>,
    ) -> Result<Json<RoutineListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "routine_list",
            lifecycle = ?params.0.lifecycle,
            min_confidence = params.0.min_confidence,
            app = params.0.app.as_deref(),
            granularity = ?params.0.granularity,
            include_unmined = params.0.include_unmined,
            limit = params.0.limit,
            "tool.invocation kind=routine_list"
        );
        self.require_m3_permissions(
            "routine_list",
            &crate::m3::routines::required_permissions_list(&params.0),
        )?;
        let db = self.m3_storage()?;
        list_routines(&db, &params.0).map(Json)
    }

    #[tool(
        description = "Fetch one routine by stable id: the full mined record (template steps, schedule signature, support evidence with episode ids resolvable via episode_get), its operator lifecycle state (transitions audit trail, confidence history), hygiene taint provenance, routine automation install state, and armed auto-run state when those rows exist"
    )]
    pub async fn routine_inspect(
        &self,
        params: Parameters<RoutineInspectParams>,
    ) -> Result<Json<RoutineInspectResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "routine_inspect",
            routine_id = %params.0.routine_id,
            "tool.invocation kind=routine_inspect"
        );
        self.require_m3_permissions(
            "routine_inspect",
            &crate::m3::routines::required_permissions_inspect(&params.0),
        )?;
        let db = self.m3_storage()?;
        inspect_routine(&db, &params.0).map(Json)
    }

    #[tool(
        description = "Apply one routine lifecycle/arming mutation. Lifecycle actions (confirm, disable, enable, archive, rename) update CF_ROUTINE_STATE with audit/readback. Arming actions (arm, disarm) update CF_KV armed_routine/v1 for installed routine automations without changing lifecycle: arm_schedule/arm_intent select schedule and live-intent triggers, failure_threshold controls self-disarm after consecutive failed runs."
    )]
    pub async fn routine_update(
        &self,
        params: Parameters<RoutineUpdateParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<RoutineUpdateResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "routine_update",
            routine_id = %params.0.routine_id,
            action = ?params.0.action,
            has_label = params.0.label.is_some(),
            has_note = params.0.note.is_some(),
            "tool.invocation kind=routine_update"
        );
        self.require_m3_permissions(
            "routine_update",
            &crate::m3::routines::required_permissions_update(&params.0),
        )?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let db = self.m3_storage()?;
        update_routine(&db, &params.0, &by_session).map(Json)
    }

    #[tool(description = "Write bounded synthetic probe rows to a storage column family")]
    pub async fn storage_put_probe_rows(
        &self,
        params: Parameters<StoragePutProbeRowsParams>,
    ) -> Result<Json<StoragePutProbeRowsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "storage_put_probe_rows",
            cf_name = %params.0.cf_name,
            rows = params.0.rows,
            value_bytes = params.0.value_bytes,
            "tool.invocation kind=storage_put_probe_rows"
        );
        self.require_m3_permissions(
            "storage_put_probe_rows",
            &crate::m3::storage::required_permissions_put(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        put_probe_rows(&runtime, &params.0).map(Json)
    }

    #[tool(description = "Run one row-cap storage GC pass for diagnostics")]
    pub async fn storage_gc_once(
        &self,
        params: Parameters<StorageGcOnceParams>,
    ) -> Result<Json<StorageGcOnceResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "storage_gc_once",
            cf_name = %params.0.cf_name,
            soft_cap_rows = params.0.soft_cap_rows,
            hard_cap_rows = params.0.hard_cap_rows,
            "tool.invocation kind=storage_gc_once"
        );
        self.require_m3_permissions(
            "storage_gc_once",
            &crate::m3::storage::required_permissions_gc(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        run_storage_gc_once(&runtime, &params.0).map(Json)
    }

    #[tool(description = "Apply one synthetic free-byte sample through storage pressure handling")]
    pub async fn storage_pressure_sample(
        &self,
        params: Parameters<StoragePressureSampleParams>,
    ) -> Result<Json<StoragePressureSampleResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "storage_pressure_sample",
            free_bytes = params.0.free_bytes,
            "tool.invocation kind=storage_pressure_sample"
        );
        self.require_m3_permissions(
            "storage_pressure_sample",
            &crate::m3::storage::required_permissions_pressure(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        apply_storage_pressure_sample(&runtime, &params.0).map(Json)
    }
}

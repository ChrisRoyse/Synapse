use super::{
    Arc, CancellationToken, ErrorData, M1State, Mutex, MutexGuard, ProfileActivateParams,
    ProfileActivateResponse, RecordingBackend, RequiredPermissions, SseState, SynapseService,
    action_preflight::{
        ActionPreflightReadback, ForegroundProof, attach_action_preflight_to_error,
    },
    activate_profile, apply_profile_runtime_config_in_state, authorization_error, error_codes,
    mcp_error,
};
use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use serde::Serialize;
use serde_json::{Value, json};
use synapse_core::{
    AccessibleNode, Action, AgentTranscriptRecord, ElementId, Event, EventSource, FocusedElement,
    ForegroundContext, Profile, ProfileUseScope, ReflexId,
};
use synapse_profiles::ForegroundProfileTransition;
use synapse_reflex::{
    AimTrackTargetSnapshot, AimTrackTargetSource, EventBus, ReflexActionGate,
    ReflexActionGateHandle, ReflexActionPermissionDenied, ResolvedElementBox,
};

type M2ActionContext = (
    synapse_action::ActionHandle,
    Option<Arc<RecordingBackend>>,
    Option<CancellationToken>,
);
type M2ReleaseAllContext = (
    synapse_action::ActionHandle,
    synapse_action::ActionEmitterSnapshotHandle,
    Option<Arc<Mutex<synapse_reflex::ReflexRuntime>>>,
);

const PROFILE_CHANGED_KIND: &str = "profile-changed";
const SCOPE_TRANSITIONED_KIND: &str = "scope-transitioned";
pub(crate) const APPROVAL_REQUEST_EVENT_KIND: &str = "approval_request";
pub(crate) const APPROVAL_DECISION_EVENT_KIND: &str = "approval_decision";
pub(crate) const APPROVAL_TIMEOUT_EVENT_KIND: &str = "approval_timeout";
const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";
// Match observe's default shallow tree so targets selected from an observation
// can be resolved on scheduler ticks without requiring a deep UIA walk.
const AIM_TRACK_TARGET_SOURCE_DEPTH: u32 = 2;
static NEXT_PROFILE_EVENT_SEQ: AtomicU64 = AtomicU64::new(1);
static NEXT_APPROVAL_EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AgentTranscriptSnapshotRow {
    pub key_hex: String,
    pub spawn_id: String,
    pub line_no: u64,
    pub record: AgentTranscriptRecord,
}

impl SynapseService {
    pub(super) fn m1_state(&self) -> Result<MutexGuard<'_, M1State>, ErrorData> {
        self.m1_state.lock().map_err(|_err| {
            mcp_error(
                synapse_core::error_codes::OBSERVE_INTERNAL,
                "M1 service state lock poisoned",
            )
        })
    }

    pub(super) fn instructions(&self) -> &'static str {
        let recording_enabled = self
            .m2_state
            .lock()
            .is_ok_and(|state| state.recording_enabled());
        let m3_has_tools = !crate::m3::m3_tool_stubs().is_empty();
        let m3_scaffold_ready = self.m3_state.lock().is_ok_and(|state| {
            let _state_readback = (
                state.db_path.as_ref(),
                state.profile_dir.as_ref(),
                state.reflex_disabled,
                state.bearer_token.as_ref(),
                state.permission_grants.names(),
                state.enable_audio,
                state.allow_unknown_profile,
                state.shutdown_cancel.is_cancelled(),
                state.shutdown_reason,
                state
                    .connection_closed_cancel
                    .as_ref()
                    .map(CancellationToken::is_cancelled),
            );
            state.scaffold_ready() && m3_has_tools
        });
        match (recording_enabled, m3_scaffold_ready) {
            (true, true) => {
                "Synapse MCP server with M1 perception, M2 action, M3 autonomy, and 40-tool facade surface (recording enabled)"
            }
            (false, true) => {
                "Synapse MCP server with M1 perception, M2 action, M3 autonomy, and 40-tool facade surface"
            }
            (true, false) => {
                "Synapse MCP server with M1 perception and M2 action (recording enabled)"
            }
            (false, false) => "Synapse MCP server with M1 perception and M2 action",
        }
    }

    pub(super) fn require_m3_permissions(
        &self,
        tool: &'static str,
        required: &RequiredPermissions,
    ) -> Result<(), ErrorData> {
        let missing = self
            .m3_state
            .lock()
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned",
                )
            })?
            .permission_grants
            .first_missing(required);
        if let Some(missing) = missing {
            tracing::warn!(
                code = synapse_core::error_codes::SAFETY_PERMISSION_DENIED,
                tool,
                missing_permission = missing.as_str(),
                "tool.permission_denied tool={} missing_permission={}",
                tool,
                missing.as_str()
            );
            return Err(authorization_error(tool, missing));
        }
        Ok(())
    }

    pub(super) fn allow_unknown_profile(&self) -> Result<bool, ErrorData> {
        self.m3_state
            .lock()
            .map(|state| state.allow_unknown_profile)
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned",
                )
            })
    }

    pub(super) fn m2_action_context_for_request(
        &self,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<M2ActionContext, ErrorData> {
        self.m2_action_context_for_session_id(mcp_session_id_from_request_context(request_context)?)
    }

    pub(super) fn m2_action_context_for_session_id(
        &self,
        session_id: Option<String>,
    ) -> Result<M2ActionContext, ErrorData> {
        self.m2_state
            .lock()
            .map(|state| {
                (
                    state.emitter_handle.clone().with_session_id(session_id),
                    state.recording.clone(),
                    state.connection_closed_cancel.clone(),
                )
            })
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::OBSERVE_INTERNAL,
                    "M2 service state lock poisoned",
                )
            })
    }

    fn m2_unscoped_action_handle(&self) -> Result<synapse_action::ActionHandle, ErrorData> {
        self.m2_state
            .lock()
            .map(|state| state.emitter_handle.clone().with_session_id(None))
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::OBSERVE_INTERNAL,
                    "M2 service state lock poisoned",
                )
            })
    }

    pub(super) fn m2_snapshot_handle(
        &self,
    ) -> Result<synapse_action::ActionEmitterSnapshotHandle, ErrorData> {
        self.m2_state
            .lock()
            .map(|state| state.snapshot_handle.clone())
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::OBSERVE_INTERNAL,
                    "M2 service state lock poisoned",
                )
            })
    }

    pub(super) fn ensure_supported_use_allows_action(
        &self,
        tool: &'static str,
    ) -> Result<ActionPreflightReadback, ErrorData> {
        let runtime = self.profile_runtime()?;
        let active_profile_id_before = runtime
            .active_profile_id()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let initial_foreground = match self.current_audit_foreground() {
            Ok(foreground) => foreground,
            Err(error) => {
                // Daemon robustness (#1061): a missing foreground window (locked
                // screen, focus on the desktop, unattended background session)
                // must not block tools that never drive the foreground. Evaluate
                // scope against the active profile and continue. Foreground-driving
                // tools and every other error kind (forced no-perception, forced
                // internal, any non-A11Y_NO_FOREGROUND failure) stay fail-closed.
                if error_data_code(&error) == Some(synapse_core::error_codes::A11Y_NO_FOREGROUND)
                    && !super::action_preflight::tool_requires_live_foreground(tool)
                {
                    tracing::info!(
                        code = synapse_core::error_codes::A11Y_NO_FOREGROUND,
                        tool,
                        "action gate: no foreground window; evaluating scope against active profile (non-foreground tool, see #1061)"
                    );
                    let preflight = super::action_preflight::no_foreground_preflight(
                        tool,
                        active_profile_id_before,
                    );
                    ensure_profile_scope_allows_action(
                        &runtime,
                        tool,
                        self.allow_unknown_profile()?,
                    )
                    .map_err(|error| attach_action_preflight_to_error(&error, &preflight))?;
                    return Ok(preflight);
                }
                return Err(error);
            }
        };
        let (foreground, preflight) = self.preflight_action_foreground(
            tool,
            &runtime,
            active_profile_id_before,
            initial_foreground,
        )?;
        let transition = self
            .reevaluate_profile_for_foreground(&foreground)
            .map_err(|error| attach_action_preflight_to_error(&error, &preflight))?;
        if let Some(profile_id) = transition.active_profile_id.as_deref() {
            self.apply_profile_runtime_config_for_profile(profile_id)
                .map_err(|error| attach_action_preflight_to_error(&error, &preflight))?;
        }
        ensure_profile_scope_allows_action(&runtime, tool, self.allow_unknown_profile()?)
            .map_err(|error| attach_action_preflight_to_error(&error, &preflight))?;
        super::target_policy::ensure_supported_use_allows(&runtime, &foreground, tool)
            .map_err(|error| attach_action_preflight_to_error(&error, &preflight))?;
        Ok(preflight)
    }

    pub(super) fn m2_release_all_context(&self) -> Result<M2ReleaseAllContext, ErrorData> {
        let (handle, snapshot_handle) = self
            .m2_state
            .lock()
            .map(|state| (state.emitter_handle.clone(), state.snapshot_handle.clone()))
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::OBSERVE_INTERNAL,
                    "M2 service state lock poisoned",
                )
            })?;
        let reflex_runtime = self
            .m3_state
            .lock()
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned",
                )
            })?
            .reflex_runtime
            .clone();
        Ok((handle, snapshot_handle, reflex_runtime))
    }

    pub(super) fn m2_rate_limit_control(
        &self,
    ) -> Result<synapse_action::BackendRateLimitControl, ErrorData> {
        self.m2_state
            .lock()
            .map(|state| state.rate_limit_control.clone())
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::OBSERVE_INTERNAL,
                    "M2 service state lock poisoned",
                )
            })
    }

    pub(super) fn profile_runtime(
        &self,
    ) -> Result<Arc<synapse_profiles::ProfileRuntime>, ErrorData> {
        self.m3_state
            .lock()
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned",
                )
            })?
            .ensure_profile_runtime()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    pub(super) fn sse_state(&self) -> Result<SseState, ErrorData> {
        self.m3_state
            .lock()
            .map(|state| state.sse_state.clone())
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned",
                )
            })
    }

    /// Shared intent tracker handle (#855), advanced by the periodic detector
    /// and the `intent_detect_tick` tool.
    pub(super) fn intent_tracker(
        &self,
    ) -> Result<crate::m3::intent_events::SharedIntentTracker, ErrorData> {
        self.m3_state
            .lock()
            .map(|state| state.intent_tracker())
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned",
                )
            })
    }

    /// Opened M3 storage handle (the daemon-wide `RocksDB` instance).
    pub(super) fn m3_storage(&self) -> Result<Arc<synapse_storage::Db>, ErrorData> {
        let mut state = self.m3_state.lock().map_err(|_err| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned",
            )
        })?;
        state
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    pub(super) fn reflex_runtime(
        &self,
    ) -> Result<Arc<Mutex<synapse_reflex::ReflexRuntime>>, ErrorData> {
        let event_bus = self.sse_state()?.event_bus();
        let action_handle = self.m2_unscoped_action_handle()?;
        let mut state = self.m3_state.lock().map_err(|_err| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned",
            )
        })?;
        let runtime = state
            .ensure_reflex_runtime(action_handle, event_bus)
            .map_err(|error| m3_state_error(&error))?;
        drop(state);
        self.install_aim_track_target_source(&runtime)?;
        Ok(runtime)
    }

    pub(crate) fn storage_summary_snapshot(
        &self,
    ) -> Result<crate::m3::storage::StorageSummaryResponse, ErrorData> {
        let runtime = self.reflex_runtime()?;
        crate::m3::storage::inspect_storage_summary(&runtime)
    }

    pub(crate) fn m3_bind_addr(&self) -> Result<String, ErrorData> {
        self.m3_state
            .lock()
            .map(|state| state.bind.clone())
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned while reading HTTP bind address",
                )
            })
    }

    pub(crate) fn approval_queue_snapshot(
        &self,
        kind: Option<crate::m3::approvals::ApprovalKind>,
    ) -> Result<Vec<crate::m3::approvals::ApprovalQueueItem>, ErrorData> {
        let db = self.m3_storage()?;
        crate::m3::approvals::approval_snapshot(&db, kind)
    }

    pub(crate) fn publish_approval_queue_event(
        &self,
        kind: &'static str,
        approval_id: &str,
        status: Option<&str>,
        by_session: &str,
        trigger: &'static str,
        extra: serde_json::Value,
    ) {
        let event_seq = NEXT_APPROVAL_EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
        let event = Event {
            seq: event_seq,
            at: Utc::now(),
            source: EventSource::System,
            kind: kind.to_owned(),
            data: json!({
                "approval_id": approval_id,
                "status": status,
                "by_session": by_session,
                "trigger": trigger,
                "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                "extra": extra,
            }),
            correlations: Vec::new(),
        };
        match self.sse_state() {
            Ok(sse_state) => {
                let report = sse_state.event_bus().publish(event);
                tracing::debug!(
                    code = "APPROVAL_QUEUE_EVENT_PUBLISHED",
                    kind,
                    approval_id,
                    status = status.unwrap_or(""),
                    trigger,
                    matched = report.matched,
                    queued = report.queued,
                    dropped = report.dropped,
                    event_seq,
                    "approval queue SSE event published"
                );
            }
            Err(error) => {
                tracing::warn!(
                    code = "APPROVAL_QUEUE_EVENT_PUBLISH_FAILED",
                    kind,
                    approval_id,
                    trigger,
                    detail = %error.message,
                    "approval queue changed but SSE event could not be published"
                );
            }
        }
    }

    pub(crate) fn acked_open_attention_anchors_snapshot(
        &self,
    ) -> Result<BTreeSet<String>, ErrorData> {
        let db = self.m3_storage()?;
        Ok(super::escalation::acked_open_attention_anchors(&db)?
            .into_iter()
            .collect())
    }

    pub(crate) fn local_model_registry_snapshot(
        &self,
    ) -> Result<Vec<crate::m3::local_models::LocalModelRegistryRow>, ErrorData> {
        let db = self.m3_storage()?;
        crate::m3::local_models::local_model_snapshot(&db)
    }

    pub(crate) fn agent_transcript_snapshot(
        &self,
        limit: usize,
    ) -> Result<Vec<AgentTranscriptSnapshotRow>, ErrorData> {
        let db = self.m3_storage()?;
        let rows = db
            .scan_cf(synapse_storage::cf::CF_AGENT_TRANSCRIPTS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let mut decoded = Vec::new();
        for (key, value) in rows {
            let (spawn_id, line_no) =
                synapse_storage::agent_transcripts::decode_agent_transcript_key(&key)
                    .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            let record = synapse_storage::decode_json::<AgentTranscriptRecord>(&value)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            decoded.push(AgentTranscriptSnapshotRow {
                key_hex: hex_encode(&key),
                spawn_id,
                line_no,
                record,
            });
        }
        decoded.sort_by(|left, right| {
            right
                .record
                .ts_ns
                .cmp(&left.record.ts_ns)
                .then_with(|| right.spawn_id.cmp(&left.spawn_id))
                .then_with(|| right.line_no.cmp(&left.line_no))
        });
        decoded.truncate(limit);
        Ok(decoded)
    }

    pub(crate) fn hygiene_report_snapshot(
        &self,
        limit: u32,
    ) -> Result<crate::m3::hygiene::HygieneReportResponse, ErrorData> {
        let runtime = self.reflex_runtime()?;
        crate::m3::hygiene::report(
            &runtime,
            &crate::m3::hygiene::HygieneReportParams {
                limit: Some(limit),
                ..Default::default()
            },
        )
    }

    pub(crate) fn demo_record_status_snapshot(
        &self,
    ) -> Result<crate::m3::demo_recording::DemoRecordStatusResponse, ErrorData> {
        crate::m3::demo_recording::demo_record_status_snapshot(&self.m3_state)
    }

    pub(crate) fn approval_decide_from_activation(
        &self,
        params: &crate::m3::approvals::ApprovalActivationParams,
        by_session: &str,
    ) -> Result<crate::m3::approvals::ApprovalActivationDecisionResponse, ErrorData> {
        let db = self.m3_storage()?;
        let command_payload = approval_activation_command_payload(params);
        let command_before = json!({
            "source_of_truth": "CF_KV approval queue rows plus approval activation/audit rows",
            "approval_id": &params.approval_id,
            "activation_id": &params.activation_id,
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "approval_activate",
                "approval_decision",
                Some(by_session.to_owned()),
                Some(by_session.to_owned()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_channel("dashboard"),
        )?;
        let result = match crate::m3::approvals::decide_approval_from_activation(
            &db, params, by_session,
        ) {
            Ok(response) => {
                match super::escalation::ack_from_approval_item_decision(
                    &db,
                    &response.decision.item,
                    params.decision.as_str(),
                    response.decision.item.decision_note.as_deref(),
                    by_session,
                    super::session_registry::unix_time_ms_now(),
                ) {
                    Ok(_maybe_escalation) => Ok(response),
                    Err(error) => {
                        tracing::error!(
                            code = "ESCALATION_APPROVAL_ACTIVATION_ACK_FAILED",
                            approval_id = %params.approval_id,
                            activation_id = %params.activation_id,
                            decision = %params.decision,
                            detail = %error.message,
                            "approval activation decided durable queue row but failed to acknowledge linked escalation"
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
                    "approval_activate",
                    "approval_decision",
                    Some(by_session.to_owned()),
                    Some(by_session.to_owned()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV approval queue rows plus approval activation/audit rows",
                        "approval_id": &response.decision.approval_id,
                        "activation_id": &response.activation_id,
                        "before_status": response.decision.before_status.as_str(),
                        "after_status": response.decision.after_status.as_str(),
                        "activation_row": &response.activation_row,
                    }),
                    "ok",
                )
                .with_channel("dashboard"),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "approval_activate",
                    "approval_decision",
                    Some(by_session.to_owned()),
                    Some(by_session.to_owned()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV approval queue rows plus approval activation/audit rows",
                    }),
                    "error",
                )
                .with_channel("dashboard")
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        if let Ok(response) = &result {
            self.publish_approval_queue_event(
                APPROVAL_DECISION_EVENT_KIND,
                &response.decision.approval_id,
                Some(response.decision.after_status.as_str()),
                by_session,
                "approval_activate",
                json!({
                    "activation_id": &response.activation_id,
                    "before_status": response.decision.before_status.as_str(),
                    "after_status": response.decision.after_status.as_str(),
                    "activation_row": &response.activation_row,
                }),
            );
        }
        result
    }

    /// Decide a pending approval from the dashboard Approvals inbox (#927).
    ///
    /// Unlike [`Self::approval_decide_from_activation`] this needs no one-time
    /// token — the HTTP route is already loopback + CSRF guarded. It records
    /// the decision in the durable `CF_KV` queue, wakes any `approval_gate`
    /// call blocked on this approval so the agent resumes immediately, and
    /// acknowledges any linked escalation (a no-op for `agent_permission`
    /// gate rows, which carry no escalation).
    pub(crate) fn approval_decide_from_dashboard(
        &self,
        approval_id: &str,
        decision: crate::m3::approvals::ApprovalDecision,
        note: Option<&str>,
        edited_args: Option<&str>,
        response: Option<&str>,
        by_session: &str,
    ) -> Result<crate::m3::approvals::ApprovalDecideResponse, ErrorData> {
        let db = self.m3_storage()?;
        let command_payload = json!({
            "approval_id": approval_id,
            "decision": decision.as_str(),
            "note": note,
            // #1030: record approve-with-edits / respond in the command audit so
            // the operator's exact edit/answer is part of the durable trail.
            "edited_args": edited_args,
            "response": response,
        });
        let command_before = json!({
            "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
            "approval_id": approval_id,
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "approval_decide",
                "approval_decision",
                Some(by_session.to_owned()),
                Some(by_session.to_owned()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_channel("dashboard"),
        )?;
        let params = crate::m3::approvals::ApprovalDecideParams {
            approval_id: approval_id.to_owned(),
            decision,
            note: note.map(str::to_owned),
            snooze_ms: None,
            edited_args: edited_args.map(str::to_owned),
            response: response.map(str::to_owned),
        };
        let result = match crate::m3::approvals::decide_approval(&db, &params, by_session) {
            Ok(response) => {
                // Wake the blocked gate promptly (the gate also re-reads CF_KV
                // as source of truth, so this is an optimization, not the
                // correctness path).
                super::permission_gate::signal_decision(approval_id);
                match super::escalation::ack_from_approval_item_decision(
                    &db,
                    &response.item,
                    decision.as_str(),
                    response.item.decision_note.as_deref(),
                    by_session,
                    super::session_registry::unix_time_ms_now(),
                ) {
                    Ok(_maybe_escalation) => Ok(response),
                    Err(error) => {
                        tracing::error!(
                            code = "ESCALATION_APPROVAL_DASHBOARD_ACK_FAILED",
                            approval_id = %approval_id,
                            decision = %decision.as_str(),
                            detail = %error.message,
                            "dashboard decided durable queue row but failed to acknowledge linked escalation"
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
                    Some(by_session.to_owned()),
                    Some(by_session.to_owned()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                        "approval_id": &response.approval_id,
                        "before_status": response.before_status.as_str(),
                        "after_status": response.after_status.as_str(),
                        "item_row": &response.item_row,
                        "audit_row": &response.audit_row,
                    }),
                    "ok",
                )
                .with_channel("dashboard"),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "approval_decide",
                    "approval_decision",
                    Some(by_session.to_owned()),
                    Some(by_session.to_owned()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_KV approval queue rows plus approval audit rows",
                    }),
                    "error",
                )
                .with_channel("dashboard")
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(error),
                ),
            )?,
        };
        if let Ok(response) = &result {
            self.publish_approval_queue_event(
                APPROVAL_DECISION_EVENT_KIND,
                &response.approval_id,
                Some(response.after_status.as_str()),
                by_session,
                "dashboard.approval_decide",
                json!({
                    "before_status": response.before_status.as_str(),
                    "after_status": response.after_status.as_str(),
                    "item_row": &response.item_row,
                    "audit_row": &response.audit_row,
                }),
            );
        }
        result
    }

    fn install_aim_track_target_source(
        &self,
        runtime: &Arc<Mutex<synapse_reflex::ReflexRuntime>>,
    ) -> Result<(), ErrorData> {
        let target_source = Arc::new(M1AimTrackTargetSource {
            m1_state: Arc::clone(&self.m1_state),
        });
        runtime
            .lock()
            .map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "reflex runtime lock poisoned while setting aim_track target source",
                )
            })?
            .set_aim_track_target_source(Some(target_source));
        Ok(())
    }

    pub(super) fn install_reflex_action_gate(
        &self,
        runtime: &Arc<Mutex<synapse_reflex::ReflexRuntime>>,
    ) -> Result<(), ErrorData> {
        let gate = self.reflex_action_gate()?;
        runtime
            .lock()
            .map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "reflex runtime lock poisoned while setting action gate",
                )
            })?
            .set_action_gate(Some(gate));
        Ok(())
    }

    pub(super) fn reflex_action_gate(&self) -> Result<ReflexActionGateHandle, ErrorData> {
        Ok(Arc::new(ReflexScopeActionGate {
            profile_runtime: self.profile_runtime()?,
            m1_state: Arc::clone(&self.m1_state),
            allow_unknown_profile: self.allow_unknown_profile()?,
            event_bus: self.sse_state()?.event_bus(),
        }))
    }

    pub(super) fn ensure_a11y_event_bridge(&self) -> Result<(), ErrorData> {
        let event_bus = self.sse_state()?.event_bus();
        self.m3_state
            .lock()
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned",
                )
            })?
            .ensure_a11y_event_bridge(event_bus)
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    #[allow(clippy::significant_drop_tightening)]
    pub(super) fn activate_profile_locked(
        &self,
        params: &ProfileActivateParams,
        allow_unknown_profile: bool,
    ) -> Result<ProfileActivateResponse, ErrorData> {
        // Keep the M3 mutex held so concurrent activations preserve changed=false idempotency.
        let mut state = self.m3_state.lock().map_err(|_err| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned",
            )
        })?;
        let runtime = state
            .ensure_profile_runtime()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        activate_profile(&runtime, params, allow_unknown_profile)
    }

    pub(super) fn apply_profile_runtime_config_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<(), ErrorData> {
        let runtime = self.profile_runtime()?;
        let profile = runtime
            .profile(profile_id)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::PROFILE_NOT_FOUND,
                    format!("profile {profile_id} was not found after activation"),
                )
            })?;
        self.apply_backend_resolution_for_profile_data(&profile)?;
        self.apply_m1_runtime_config_for_profile(&profile)?;
        Ok(())
    }

    fn apply_backend_resolution_for_profile_data(
        &self,
        profile: &Profile,
    ) -> Result<(), ErrorData> {
        let policy =
            synapse_action::BackendResolutionPolicy::from_profile_backends(profile.backends);
        let source = format!("profile:{}", profile.id);
        self.m2_state
            .lock()
            .map_err(|_err| {
                mcp_error(
                    synapse_core::error_codes::OBSERVE_INTERNAL,
                    "M2 service state lock poisoned",
                )
            })?
            .set_backend_resolution(source.clone(), policy)
            .map_err(|error| {
                mcp_error(
                    error_codes::ACTION_BACKEND_UNAVAILABLE,
                    format!("could not update action backend resolution: {error}"),
                )
            })?;
        tracing::info!(
            code = "ACTION_BACKEND_RESOLUTION_UPDATED",
            profile_id = %profile.id,
            source,
            default_backend = ?policy.default_backend,
            keyboard_default = ?policy.keyboard_default,
            mouse_default = ?policy.mouse_default,
            pad_default = ?policy.pad_default,
            keyboard_auto = policy.keyboard_auto_backend().as_str(),
            mouse_auto = policy.mouse_auto_backend().as_str(),
            pad_auto = policy.pad_auto_backend().as_str(),
            release_all_auto = policy.release_all_auto_backend().as_str(),
            "action backend resolution updated from active profile"
        );
        Ok(())
    }

    pub(super) fn apply_m1_runtime_config_for_profile(
        &self,
        profile: &Profile,
    ) -> Result<(), ErrorData> {
        let capture = {
            let mut state = self.m1_state()?;
            apply_profile_runtime_config_in_state(&mut state, profile)?
        };
        tracing::info!(
            code = "PROFILE_M1_RUNTIME_CONFIG_APPLIED",
            profile_id = %profile.id,
            mode = ?profile.mode,
            capture_target = ?capture.target,
            capture_generation = capture.generation,
            capture_source = %capture.source,
            "profile perception and capture runtime config applied"
        );
        Ok(())
    }

    pub(super) fn reevaluate_profile_for_foreground(
        &self,
        foreground: &ForegroundContext,
    ) -> Result<ForegroundProfileTransition, ErrorData> {
        let runtime = self.profile_runtime()?;
        let transition = runtime
            .reevaluate_foreground(&foreground_window(foreground))
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let event_bus = self.sse_state()?.event_bus();
        publish_profile_transition_events(&event_bus, &transition, foreground);
        Ok(transition)
    }

    pub(super) fn ensure_act_type_foreground(
        &self,
        preflight: &ActionPreflightReadback,
        recording: Option<&Arc<RecordingBackend>>,
    ) -> Result<(), ErrorData> {
        let expected = preflight.after.as_ref().unwrap_or(&preflight.before);
        let actual = self.current_audit_foreground().map_err(|error| {
            act_type_foreground_lost_error(expected, None, Some(&error), recording)
        })?;
        if actual.hwnd == expected.hwnd {
            return Ok(());
        }

        Err(act_type_foreground_lost_error(
            expected,
            Some(&actual),
            None,
            recording,
        ))
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Extract the structured `code` string an `mcp_error` carries in its data
/// payload, used by the action gate to distinguish `A11Y_NO_FOREGROUND` from
/// other failure kinds before degrading gracefully (#1061).
fn error_data_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

fn act_type_foreground_lost_error(
    expected: &ForegroundProof,
    actual: Option<&ForegroundContext>,
    read_error: Option<&ErrorData>,
    recording: Option<&Arc<RecordingBackend>>,
) -> ErrorData {
    let recording_event_count_before = recording.map_or(0, |recording| recording.events().len());
    let recording_event_count_after = recording.map_or(0, |recording| recording.events().len());
    match actual {
        Some(actual) => {
            tracing::warn!(
                code = "M2_ACT_TYPE_FOREGROUND_LOST",
                expected_hwnd = expected.hwnd,
                actual_hwnd = actual.hwnd,
                expected_pid = expected.pid,
                actual_pid = actual.pid,
                expected_title = %expected.window_title,
                actual_title = %actual.window_title,
                recording_event_count_before,
                recording_event_count_after,
                "readback=foreground edge=lost before_hwnd=0x{:x} after_hwnd=0x{:x} code=ACTION_FOREGROUND_LOST recording_events_before={} recording_events_after={}",
                expected.hwnd,
                actual.hwnd,
                recording_event_count_before,
                recording_event_count_after
            );
            ErrorData::new(
                ErrorCode(-32099),
                format!(
                    "act_type expected preflight foreground hwnd 0x{:x} ({}) but current foreground is hwnd 0x{:x} ({})",
                    expected.hwnd, expected.window_title, actual.hwnd, actual.window_title
                ),
                Some(json!({
                    "code": error_codes::ACTION_FOREGROUND_LOST,
                    "reason": "act_type_foreground_changed_after_preflight",
                    "foreground_expected": expected,
                    "foreground_actual": foreground_context_details(actual),
                    "recording_event_count_before": recording_event_count_before,
                    "recording_event_count_after": recording_event_count_after,
                })),
            )
        }
        None => {
            let read_error_message = read_error
                .map(|error| error.message.to_string())
                .unwrap_or_else(|| "unknown foreground read error".to_owned());
            tracing::warn!(
                code = "M2_ACT_TYPE_FOREGROUND_LOST",
                expected_hwnd = expected.hwnd,
                expected_pid = expected.pid,
                expected_title = %expected.window_title,
                read_error = %read_error_message,
                recording_event_count_before,
                recording_event_count_after,
                "readback=foreground edge=read_failed before_hwnd=0x{:x} code=ACTION_FOREGROUND_LOST recording_events_before={} recording_events_after={}",
                expected.hwnd,
                recording_event_count_before,
                recording_event_count_after
            );
            ErrorData::new(
                ErrorCode(-32099),
                format!(
                    "act_type could not read current foreground for preflight hwnd 0x{:x} ({}): {}",
                    expected.hwnd, expected.window_title, read_error_message
                ),
                Some(json!({
                    "code": error_codes::ACTION_FOREGROUND_LOST,
                    "reason": "act_type_foreground_read_failed_after_preflight",
                    "foreground_expected": expected,
                    "foreground_actual": serde_json::Value::Null,
                    "foreground_read_error": read_error.map(|error| json!({
                        "message": error.message.to_string(),
                        "data": error.data.clone(),
                    })),
                    "recording_event_count_before": recording_event_count_before,
                    "recording_event_count_after": recording_event_count_after,
                })),
            )
        }
    }
}

fn foreground_context_details(foreground: &ForegroundContext) -> serde_json::Value {
    json!({
        "hwnd": foreground.hwnd,
        "pid": foreground.pid,
        "process_name": &foreground.process_name,
        "process_path": &foreground.process_path,
        "window_title": &foreground.window_title,
        "window_bounds": &foreground.window_bounds,
        "monitor_index": foreground.monitor_index,
        "dpi_scale": foreground.dpi_scale,
        "profile_id": &foreground.profile_id,
        "steam_appid": foreground.steam_appid,
        "is_fullscreen": foreground.is_fullscreen,
        "is_dwm_composed": foreground.is_dwm_composed,
    })
}

fn profile_action_scope_denied_error(
    tool: &'static str,
    reason: &'static str,
    profile_id: Option<&str>,
    use_scope: Option<ProfileUseScope>,
    detail: &'static str,
) -> ErrorData {
    tracing::warn!(
        code = error_codes::SAFETY_PROFILE_ACTION_DENIED,
        tool,
        reason,
        profile_id,
        use_scope = use_scope.map(profile_use_scope_label),
        detail,
        "profile scope denied action dispatch"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("profile scope denied {tool}: {detail}"),
        Some(json!({
            "code": error_codes::SAFETY_PROFILE_ACTION_DENIED,
            "tool": tool,
            "reason": reason,
            "profile_id": profile_id,
            "use_scope": use_scope.map(profile_use_scope_label),
            "detail": detail,
        })),
    )
}

fn ensure_profile_scope_allows_action(
    runtime: &synapse_profiles::ProfileRuntime,
    tool: &'static str,
    allow_unknown_profile: bool,
) -> Result<(), ErrorData> {
    let active_profile_id = runtime
        .active_profile_id()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let Some(active_profile_id) = active_profile_id else {
        // Default posture (allow_unknown_profile): general Windows
        // computer-control, so an unprofiled foreground is still actionable.
        // Functional safety (panic hotkey, release-all, rate limits, focus
        // stabilization) is unaffected by this allowance.
        if allow_unknown_profile {
            return Ok(());
        }
        return Err(profile_action_scope_denied_error(
            tool,
            "no_profile",
            None,
            None,
            "action tools require an active profile before dispatch",
        ));
    };

    let profile = runtime
        .profile(&active_profile_id)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .ok_or_else(|| {
            profile_action_scope_denied_error(
                tool,
                "active_profile_missing",
                Some(&active_profile_id),
                None,
                "active profile id does not resolve to a loaded profile",
            )
        })?;

    match profile.use_scope {
        ProfileUseScope::Productivity
        | ProfileUseScope::SinglePlayer
        | ProfileUseScope::OperatorOwnedTest
        | ProfileUseScope::SanctionedResearch => Ok(()),
        ProfileUseScope::Unknown if allow_unknown_profile => Ok(()),
        ProfileUseScope::Unknown => Err(profile_action_scope_denied_error(
            tool,
            "unknown_scope",
            Some(&profile.id),
            Some(profile.use_scope),
            "active profile has use_scope=\"unknown\"; start with --allow-unknown-profile to dispatch action tools",
        )),
    }
}

struct ReflexScopeActionGate {
    profile_runtime: Arc<synapse_profiles::ProfileRuntime>,
    m1_state: super::SharedM1State,
    allow_unknown_profile: bool,
    event_bus: EventBus,
}

impl ReflexActionGate for ReflexScopeActionGate {
    fn ensure_action_allowed(
        &self,
        _reflex_id: &ReflexId,
        _action: &Action,
    ) -> Result<(), ReflexActionPermissionDenied> {
        const TOOL: &str = "reflex_dispatch";
        (|| {
            let foreground = current_reflex_action_foreground(&self.m1_state)?;
            let transition = self
                .profile_runtime
                .reevaluate_foreground(&foreground_window(&foreground))
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            publish_profile_transition_events(&self.event_bus, &transition, &foreground);
            ensure_profile_scope_allows_action(
                &self.profile_runtime,
                TOOL,
                self.allow_unknown_profile,
            )
            .and_then(|()| {
                super::target_policy::ensure_supported_use_allows(
                    &self.profile_runtime,
                    &foreground,
                    TOOL,
                )
            })
        })()
        .map_err(|error| reflex_denial_from_error(&error))
    }
}

struct M1AimTrackTargetSource {
    m1_state: super::SharedM1State,
}

impl AimTrackTargetSource for M1AimTrackTargetSource {
    fn snapshot(&self) -> AimTrackTargetSnapshot {
        let input = {
            let state = match self.m1_state.lock() {
                Ok(state) => state,
                Err(_error) => {
                    return target_source_error_snapshot(
                        "M1 service state lock poisoned while resolving aim_track target",
                    );
                }
            };
            crate::m1::current_input(&state, AIM_TRACK_TARGET_SOURCE_DEPTH)
        };
        match input {
            Ok(input) => AimTrackTargetSnapshot {
                entities: input.entities,
                elements: resolved_elements_from_input(&input.focused, &input.elements),
                source_label: Some("m1_current_input".to_owned()),
                source_seq: None,
                source_error: None,
            },
            Err(error) => {
                tracing::warn!(
                    code = "AIM_TRACK_TARGET_SOURCE_UNAVAILABLE",
                    detail = %error,
                    "aim_track target source could not read current M1 input"
                );
                target_source_error_snapshot(error.to_string())
            }
        }
    }
}

fn target_source_error_snapshot(detail: impl Into<String>) -> AimTrackTargetSnapshot {
    AimTrackTargetSnapshot {
        source_label: Some("m1_current_input".to_owned()),
        source_error: Some(detail.into()),
        ..AimTrackTargetSnapshot::default()
    }
}

fn resolved_elements_from_input(
    focused: &Option<FocusedElement>,
    elements: &[AccessibleNode],
) -> Vec<ResolvedElementBox> {
    let mut seen = HashSet::<ElementId>::new();
    let mut resolved = Vec::new();
    if let Some(focused) = focused {
        push_resolved_element(&mut seen, &mut resolved, &focused.element_id, focused.bbox);
    }
    for element in elements {
        push_resolved_element(&mut seen, &mut resolved, &element.element_id, element.bbox);
    }
    resolved
}

fn push_resolved_element(
    seen: &mut HashSet<ElementId>,
    resolved: &mut Vec<ResolvedElementBox>,
    element_id: &ElementId,
    bbox: synapse_core::Rect,
) {
    if seen.insert(element_id.clone()) {
        resolved.push(ResolvedElementBox {
            element_id: element_id.clone(),
            bbox,
        });
    }
}

fn current_reflex_action_foreground(
    m1_state: &super::SharedM1State,
) -> Result<ForegroundContext, ErrorData> {
    {
        let state = m1_state.lock().map_err(|_err| {
            mcp_error(
                error_codes::OBSERVE_INTERNAL,
                "M1 service state lock poisoned while checking reflex dispatch scope",
            )
        })?;
        if state.force_observe_internal {
            return Err(mcp_error(
                error_codes::OBSERVE_INTERNAL,
                "forced observe internal error",
            ));
        }
        if state.force_no_perception {
            return Err(mcp_error(
                error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
                "no perception source is available",
            ));
        }
        if let Some(input) = &state.synthetic {
            return Ok(input.foreground.clone());
        }
    }
    synapse_a11y::current_foreground_context()
        .map_err(|error| mcp_error(error.code(), error.to_string()))
}

fn foreground_window(foreground: &ForegroundContext) -> synapse_profiles::ForegroundWindow {
    synapse_profiles::ForegroundWindow {
        exe: non_empty(&foreground.process_name),
        title: non_empty(&foreground.window_title),
        steam_appid: foreground.steam_appid,
        window_class: None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn publish_profile_transition_events(
    event_bus: &EventBus,
    transition: &ForegroundProfileTransition,
    foreground: &ForegroundContext,
) {
    if transition.changed {
        let report = event_bus.publish(profile_transition_event(
            PROFILE_CHANGED_KIND,
            profile_changed_event_data(transition, foreground),
        ));
        tracing::debug!(
            code = "PROFILE_CHANGED_EVENT_PUBLISHED",
            matched = report.matched,
            queued = report.queued,
            dropped = report.dropped,
            previous_profile_id = ?transition.previous_profile_id,
            active_profile_id = ?transition.active_profile_id,
            "profile-changed event published"
        );
    }
    if transition.scope_changed {
        let report = event_bus.publish(profile_transition_event(
            SCOPE_TRANSITIONED_KIND,
            scope_transition_event_data(transition, foreground),
        ));
        tracing::debug!(
            code = "SCOPE_TRANSITIONED_EVENT_PUBLISHED",
            matched = report.matched,
            queued = report.queued,
            dropped = report.dropped,
            old_scope = profile_use_scope_label(transition.effective_previous_scope),
            new_scope = profile_use_scope_label(transition.effective_active_scope),
            "scope-transitioned event published"
        );
    }
}

fn profile_transition_event(kind: &str, data: serde_json::Value) -> Event {
    Event {
        seq: NEXT_PROFILE_EVENT_SEQ.fetch_add(1, Ordering::Relaxed),
        at: Utc::now(),
        source: EventSource::System,
        kind: kind.to_owned(),
        data,
        correlations: Vec::new(),
    }
}

fn profile_changed_event_data(
    transition: &ForegroundProfileTransition,
    foreground: &ForegroundContext,
) -> serde_json::Value {
    json!({
        "old_profile_id": transition.previous_profile_id.clone(),
        "new_profile_id": transition.active_profile_id.clone(),
        "old_scope": transition.previous_scope.map(profile_use_scope_label),
        "new_scope": transition.active_scope.map(profile_use_scope_label),
        "effective_old_scope": profile_use_scope_label(transition.effective_previous_scope),
        "effective_new_scope": profile_use_scope_label(transition.effective_active_scope),
        "match_rank": transition.resolution.as_ref().map(|resolution| resolution.rank_name),
        "foreground": foreground_event_data(foreground),
    })
}

fn scope_transition_event_data(
    transition: &ForegroundProfileTransition,
    foreground: &ForegroundContext,
) -> serde_json::Value {
    json!({
        "old_profile_id": transition.previous_profile_id.clone(),
        "new_profile_id": transition.active_profile_id.clone(),
        "old_scope": profile_use_scope_label(transition.effective_previous_scope),
        "new_scope": profile_use_scope_label(transition.effective_active_scope),
        "old_profile_scope": transition.previous_scope.map(profile_use_scope_label),
        "new_profile_scope": transition.active_scope.map(profile_use_scope_label),
        "match_rank": transition.resolution.as_ref().map(|resolution| resolution.rank_name),
        "foreground": foreground_event_data(foreground),
    })
}

fn foreground_event_data(foreground: &ForegroundContext) -> serde_json::Value {
    json!({
        "hwnd": foreground.hwnd,
        "pid": foreground.pid,
        "process_name": foreground.process_name.clone(),
        "process_path": foreground.process_path.clone(),
        "window_title": foreground.window_title.clone(),
        "steam_appid": foreground.steam_appid,
    })
}

fn reflex_denial_from_error(error: &ErrorData) -> ReflexActionPermissionDenied {
    let data = error.data.as_ref();
    ReflexActionPermissionDenied {
        policy_code: data
            .and_then(|value| value.get("code"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        policy_reason: data
            .and_then(|value| value.get("reason"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        profile_id: data
            .and_then(|value| value.get("profile_id"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        use_scope: data
            .and_then(|value| value.get("use_scope"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        detail: data
            .and_then(|value| value.get("detail"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| error.message.to_string(), ToOwned::to_owned),
    }
}

pub(super) fn mcp_session_id_from_request_context(
    request_context: &RequestContext<RoleServer>,
) -> Result<Option<String>, ErrorData> {
    mcp_session_id_from_extensions(&request_context.extensions)
}

fn mcp_session_id_from_extensions(
    extensions: &rmcp::model::Extensions,
) -> Result<Option<String>, ErrorData> {
    let Some(parts) = extensions.get::<axum::http::request::Parts>() else {
        return Ok(crate::http::current_mcp_session_id());
    };
    let session_id = mcp_session_id_from_headers(&parts.headers)?;
    if session_id.is_some() {
        return Ok(session_id);
    }
    tracing::error!(
        code = synapse_core::error_codes::HTTP_SESSION_INVALID,
        method = %parts.method,
        uri = %parts.uri,
        "HTTP MCP action request reached tool dispatch without Mcp-Session-Id"
    );
    Err(mcp_error(
        synapse_core::error_codes::HTTP_SESSION_INVALID,
        "HTTP MCP action request reached tool dispatch without Mcp-Session-Id",
    ))
}

fn mcp_session_id_from_headers(
    headers: &axum::http::HeaderMap,
) -> Result<Option<String>, ErrorData> {
    let Some(value) = headers.get(MCP_SESSION_ID_HEADER) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_err| {
        tracing::error!(
            code = synapse_core::error_codes::HTTP_SESSION_INVALID,
            "HTTP MCP action request carried a non-ASCII Mcp-Session-Id header"
        );
        mcp_error(
            synapse_core::error_codes::HTTP_SESSION_INVALID,
            "Mcp-Session-Id header is not valid visible ASCII",
        )
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        tracing::error!(
            code = synapse_core::error_codes::HTTP_SESSION_INVALID,
            "HTTP MCP action request carried an invalid Mcp-Session-Id header value"
        );
        return Err(mcp_error(
            synapse_core::error_codes::HTTP_SESSION_INVALID,
            "Mcp-Session-Id header contains characters outside visible ASCII",
        ));
    }
    Ok(Some(value.to_owned()))
}

const fn profile_use_scope_label(scope: ProfileUseScope) -> &'static str {
    match scope {
        ProfileUseScope::Productivity => "productivity",
        ProfileUseScope::SinglePlayer => "single_player",
        ProfileUseScope::OperatorOwnedTest => "operator_owned_test",
        ProfileUseScope::SanctionedResearch => "sanctioned_research",
        ProfileUseScope::Unknown => "unknown",
    }
}

fn m3_state_error(error: &anyhow::Error) -> ErrorData {
    if let Some(reflex_error) = error.downcast_ref::<synapse_reflex::ReflexError>() {
        return mcp_error(reflex_error.code(), reflex_error.to_string());
    }
    mcp_error(
        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
        error.to_string(),
    )
}

fn approval_activation_command_payload(
    params: &crate::m3::approvals::ApprovalActivationParams,
) -> Value {
    json!({
        "bind": &params.bind,
        "approval_id": &params.approval_id,
        "activation_id": &params.activation_id,
        "token_sha256": crate::m3::approvals::activation_token_sha256(&params.token),
        "decision": &params.decision,
        "snooze_ms": params.snooze_ms,
    })
}

#[cfg(debug_assertions)]
pub(super) fn maybe_force_panic_during_act(tool: &'static str) {
    if std::env::var("SYNAPSE_MCP_FORCE_PANIC_DURING_ACT").as_deref() == Ok("1") {
        tokio::task::block_in_place(|| panic!("forced panic during {tool}"));
    }
}

#[cfg(not(debug_assertions))]
pub(super) fn maybe_force_panic_during_act(_tool: &'static str) {}

#[cfg(test)]
mod scope_gate_tests {
    use std::{fs, num::NonZeroUsize, path::Path, time::Duration};

    use rmcp::{handler::server::wrapper::Parameters, model::Extensions};
    use serde_json::{Value, json};
    use synapse_core::{
        AccessibleNode, Action, EventFilter, FocusedElement, ForegroundContext, Rect, SensorStatus,
        UiaPattern, element_id,
    };
    use synapse_perception::ObservationInput;
    use synapse_storage::cf;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        m1::{FindParams, FindScope, ObserveParams, ReadTextParams},
        m2::{ActClipboardFormat, ActClipboardParams, ActClipboardVerb, M2ServiceConfig},
        m3::{M3ServiceConfig, subscribe::SubscribeParams},
        m4::M4ServiceConfig,
    };

    const ACTION_WRITE_TOOLS: [&str; 12] = [
        "act_click",
        "act_type",
        "act_press",
        "act_keymap",
        "act_stroke",
        "act_scroll",
        "act_pad",
        "act_clipboard",
        "act_combo",
        "act_run_shell",
        "act_launch",
        "reflex_register",
    ];

    #[test]
    fn request_context_session_id_reads_http_parts_header() -> anyhow::Result<()> {
        let extensions = extensions_with_http_parts(Some("session-677"))?;

        assert_eq!(
            mcp_session_id_from_extensions(&extensions)?.as_deref(),
            Some("session-677")
        );
        Ok(())
    }

    #[test]
    fn request_context_session_id_rejects_http_parts_without_header() -> anyhow::Result<()> {
        let extensions = extensions_with_http_parts(None)?;

        let error = match mcp_session_id_from_extensions(&extensions) {
            Ok(value) => anyhow::bail!("missing HTTP session header returned {value:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::HTTP_SESSION_INVALID))
        );
        Ok(())
    }

    #[test]
    fn request_context_session_id_rejects_invalid_http_header_value() -> anyhow::Result<()> {
        let extensions = extensions_with_http_parts(Some("session 677"))?;

        let error = match mcp_session_id_from_extensions(&extensions) {
            Ok(value) => anyhow::bail!("invalid HTTP session header returned {value:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::HTTP_SESSION_INVALID))
        );
        Ok(())
    }

    #[test]
    fn approval_activation_command_payload_uses_token_hash_only() {
        let params = crate::m3::approvals::ApprovalActivationParams {
            bind: "127.0.0.1:7700".to_owned(),
            approval_id: "appr-issue1540".to_owned(),
            activation_id: "actv1-issue1540".to_owned(),
            token: "synapse-activation-v1-raw-token-issue1540".to_owned(),
            decision: "accept".to_owned(),
            snooze_ms: None,
        };

        let payload = approval_activation_command_payload(&params);
        let encoded = payload.to_string();

        assert!(payload.get("token").is_none());
        assert_eq!(
            payload["token_sha256"],
            crate::m3::approvals::activation_token_sha256(&params.token)
        );
        assert!(!encoded.contains(&params.token));
    }

    #[test]
    fn instructions_advertise_m3_when_current_m3_tools_are_registered() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        let service = service_with_profiles(profiles.path(), false)?;
        let m3_tools = crate::m3::m3_tool_stubs();
        let unique_tool_names = m3_tools
            .iter()
            .map(|stub| stub.name)
            .collect::<HashSet<_>>();

        assert!(!m3_tools.is_empty());
        assert_eq!(unique_tool_names.len(), m3_tools.len());
        assert!(service.instructions().contains("M3 autonomy"));

        Ok(())
    }

    #[test]
    fn action_scope_gate_denies_no_active_profile_before_dispatch() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(&profiles.path().join("known.toml"), "known", "productivity")?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;

        let error = match service.ensure_supported_use_allows_action("act_type") {
            Ok(_) => anyhow::bail!("action tools must fail closed without an active profile"),
            Err(error) => error,
        };

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::SAFETY_PROFILE_ACTION_DENIED))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("tool")),
            Some(&json!("act_type"))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("reason")),
            Some(&json!("no_profile"))
        );
        Ok(())
    }

    #[test]
    fn action_scope_gate_denies_active_unknown_profile_without_override() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(&profiles.path().join("unknown.toml"), "unknown", "unknown")?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_process_input(&service, "unknown.exe", "Unknown App", 0x4567)?;
        let runtime = service.profile_runtime()?;
        runtime.activate("unknown")?;

        for tool in ACTION_WRITE_TOOLS {
            let error = match service.ensure_supported_use_allows_action(tool) {
                Ok(_) => anyhow::bail!(
                    "unknown scope must fail closed for {tool} without the explicit override"
                ),
                Err(error) => error,
            };

            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("code")),
                Some(&json!(error_codes::SAFETY_PROFILE_ACTION_DENIED))
            );
            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("tool")),
                Some(&json!(tool))
            );
            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("reason")),
                Some(&json!("unknown_scope"))
            );
            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("profile_id")),
                Some(&json!("unknown"))
            );
            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("use_scope")),
                Some(&json!("unknown"))
            );
        }
        Ok(())
    }

    #[test]
    fn action_scope_gate_allows_single_player_profile_for_all_action_write_tools()
    -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("single-player.toml"),
            "single-player",
            "single_player",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_process_input(&service, "single-player.exe", "Single Player", 0x4567)?;
        let runtime = service.profile_runtime()?;
        runtime.activate("single-player")?;

        for tool in ACTION_WRITE_TOOLS {
            service.ensure_supported_use_allows_action(tool)?;
        }
        Ok(())
    }

    #[test]
    fn action_scope_gate_no_foreground_allows_non_foreground_tools() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("single-player.toml"),
            "single-player",
            "single_player",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        let runtime = service.profile_runtime()?;
        runtime.activate("single-player")?;
        {
            let mut state = service
                .m1_state
                .lock()
                .map_err(|_err| anyhow::anyhow!("M1 service state lock poisoned"))?;
            state.force_no_foreground = true;
        }

        let preflight = service.ensure_supported_use_allows_action("act_run_shell")?;

        assert_eq!(preflight.status, "no_foreground_scope_evaluated");
        assert!(!preflight.applied);
        assert_eq!(
            preflight.active_profile_id_before.as_deref(),
            Some("single-player")
        );
        assert_eq!(preflight.before.hwnd, 0);
        Ok(())
    }

    #[test]
    fn action_scope_gate_no_foreground_still_denies_foreground_driving_tools() -> anyhow::Result<()>
    {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("single-player.toml"),
            "single-player",
            "single_player",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        let runtime = service.profile_runtime()?;
        runtime.activate("single-player")?;
        {
            let mut state = service
                .m1_state
                .lock()
                .map_err(|_err| anyhow::anyhow!("M1 service state lock poisoned"))?;
            state.force_no_foreground = true;
        }

        let error = match service.ensure_supported_use_allows_action("act_type") {
            Ok(value) => anyhow::bail!("foreground-driving act_type returned {value:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::A11Y_NO_FOREGROUND))
        );
        Ok(())
    }

    #[test]
    fn action_scope_gate_no_foreground_does_not_bypass_profile_scope() -> anyhow::Result<()> {
        // The no-foreground degrade must relax ONLY the foreground requirement,
        // never the profile scope gate. In strict posture with no active profile,
        // a non-foreground tool is still denied (no_profile) — proving scope
        // enforcement survives the no-foreground path (#1061).
        let profiles = TempDir::new()?;
        let service = service_with_profiles(profiles.path(), false)?;
        {
            let mut state = service
                .m1_state
                .lock()
                .map_err(|_err| anyhow::anyhow!("M1 service state lock poisoned"))?;
            state.synthetic = None;
            state.force_no_foreground = true;
        }

        let error = match service.ensure_supported_use_allows_action("act_run_shell") {
            Ok(value) => {
                anyhow::bail!("strict posture must still enforce profile scope, got {value:?}")
            }
            Err(error) => error,
        };
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("reason")),
            Some(&json!("no_profile")),
            "no-foreground degrade must not bypass the profile scope gate, got {error:?}"
        );
        Ok(())
    }

    #[test]
    fn action_scope_gate_no_foreground_degrade_is_scoped_to_a11y_no_foreground_only()
    -> anyhow::Result<()> {
        // The degrade must trigger ONLY on A11Y_NO_FOREGROUND. A forced
        // no-perception condition (a different failure kind) must still fail
        // closed even for an exempt non-foreground tool — the tolerance is never
        // widened to other perception errors (#1061).
        let profiles = TempDir::new()?;
        let service = service_with_profiles(profiles.path(), true)?;
        {
            let mut state = service
                .m1_state
                .lock()
                .map_err(|_err| anyhow::anyhow!("M1 service state lock poisoned"))?;
            state.synthetic = None;
            state.force_no_foreground = false;
            state.force_no_perception = true;
        }

        let error = match service.ensure_supported_use_allows_action("act_run_shell") {
            Ok(value) => {
                anyhow::bail!(
                    "forced no-perception must fail closed even for non-foreground tools, got {value:?}"
                )
            }
            Err(error) => error,
        };
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE)),
            "non-A11Y_NO_FOREGROUND errors must stay fail-closed, got {error:?}"
        );
        Ok(())
    }

    #[test]
    fn act_type_foreground_uses_preflight_instead_of_stale_observed_foreground()
    -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        let service = service_with_profiles(profiles.path(), true)?;
        let current = synthetic_process_input("obs64.exe", "YouTube Broadcast Setup", 0x691390);
        let expected_foreground = current.foreground.clone();
        install_synthetic_input(&service, current)?;
        {
            let mut state = service.m1_state.lock().map_err(|_err| {
                anyhow::anyhow!("M1 service state lock poisoned while installing stale foreground")
            })?;
            state.last_observed_foreground = Some(
                synthetic_process_input(
                    "chrome.exe",
                    "Synapse 751 CDP FSV - Google Chrome",
                    0x373137a,
                )
                .foreground,
            );
        }
        let preflight = act_type_preflight_for(&expected_foreground);

        service.ensure_act_type_foreground(&preflight, None)?;

        println!(
            "readback=act_type_foreground edge=stale_observed_ignored stale_hwnd=0x373137a preflight_hwnd=0x{:x}",
            expected_foreground.hwnd
        );
        Ok(())
    }

    #[test]
    fn act_type_foreground_still_fails_closed_when_focus_changes_after_preflight()
    -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        let service = service_with_profiles(profiles.path(), true)?;
        let expected = synthetic_process_input("obs64.exe", "YouTube Broadcast Setup", 0x691390);
        let actual = synthetic_process_input("notepad.exe", "manual.txt - Notepad", 0x1234);
        let preflight = act_type_preflight_for(&expected.foreground);
        install_synthetic_input(&service, actual)?;

        let error = match service.ensure_act_type_foreground(&preflight, None) {
            Ok(()) => anyhow::bail!("act_type foreground check accepted changed foreground"),
            Err(error) => error,
        };
        let data = error
            .data
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("foreground error should carry structured data"))?;

        assert_eq!(
            data.get("code").and_then(serde_json::Value::as_str),
            Some(error_codes::ACTION_FOREGROUND_LOST)
        );
        assert_eq!(
            data.get("reason").and_then(serde_json::Value::as_str),
            Some("act_type_foreground_changed_after_preflight")
        );
        assert_eq!(
            data.pointer("/foreground_expected/hwnd")
                .and_then(serde_json::Value::as_i64),
            Some(0x691390)
        );
        assert_eq!(
            data.pointer("/foreground_actual/hwnd")
                .and_then(serde_json::Value::as_i64),
            Some(0x1234)
        );
        println!(
            "readback=act_type_foreground edge=changed_after_preflight before_hwnd=0x691390 after_hwnd=0x1234 code={:?} reason={:?}",
            data.get("code"),
            data.get("reason")
        );
        Ok(())
    }

    #[tokio::test]
    async fn observe_persists_audit_session_observation_and_event_rows() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("notepad.toml"),
            "notepad",
            "productivity",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        let runtime = service.reflex_runtime()?;
        let before_counts = runtime
            .lock()
            .map_err(|_err| anyhow::anyhow!("reflex runtime lock poisoned"))?
            .storage_cf_row_counts()?;
        assert_eq!(cf_count(&before_counts, cf::CF_EVENTS), 0);
        assert_eq!(cf_count(&before_counts, cf::CF_OBSERVATIONS), 0);
        assert_eq!(cf_count(&before_counts, cf::CF_SESSIONS), 0);

        install_synthetic_notepad_input(&service)?;
        let observation = service
            .observe_without_request_context_for_test(Parameters(ObserveParams::default()))
            .await?;
        assert_eq!(
            observation.0.foreground.profile_id.as_deref(),
            Some("notepad")
        );

        let runtime = service.reflex_runtime()?;
        let runtime = runtime
            .lock()
            .map_err(|_err| anyhow::anyhow!("reflex runtime lock poisoned"))?;
        let after_counts = runtime.storage_cf_row_counts()?;
        assert_eq!(cf_count(&after_counts, cf::CF_EVENTS), 1);
        assert_eq!(cf_count(&after_counts, cf::CF_OBSERVATIONS), 1);
        assert_eq!(cf_count(&after_counts, cf::CF_SESSIONS), 1);

        let observation_rows = runtime.storage_cf_tail_rows(cf::CF_OBSERVATIONS, 1)?;
        let stored_observation: Value = serde_json::from_slice(&observation_rows[0].1)?;
        assert_eq!(stored_observation["reason"], "observe");
        assert_eq!(stored_observation["foreground"]["profile_id"], "notepad");
        assert_eq!(
            stored_observation["foreground"]["process_name"],
            "notepad.exe"
        );

        let event_rows = runtime.storage_cf_tail_rows(cf::CF_EVENTS, 1)?;
        let stored_event: Value = serde_json::from_slice(&event_rows[0].1)?;
        assert_eq!(stored_event["kind"], "perception.observed");
        assert_eq!(stored_event["source"], "perception");
        assert_eq!(
            stored_event["data"]["observation_id"],
            stored_observation["observation_id"]
        );
        assert_eq!(stored_event["data"]["hud_fields"], json!([]));
        assert_eq!(stored_event["data"]["hud_error_fields"], json!([]));

        let session_rows = runtime.storage_cf_tail_rows(cf::CF_SESSIONS, 1)?;
        let stored_session: Value = serde_json::from_slice(&session_rows[0].1)?;
        assert_eq!(
            stored_session["session_id"],
            stored_observation["session_id"]
        );
        assert_eq!(stored_session["active_profile"], "notepad");
        drop(runtime);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_observe_audit_rows_are_partitioned_by_mcp_session_id() -> anyhow::Result<()>
    {
        const SESSION_COUNT: usize = 8;
        const OBSERVES_PER_SESSION: usize = 50;

        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("notepad.toml"),
            "notepad",
            "productivity",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;

        let mut handles = Vec::new();
        for session_index in 0..SESSION_COUNT {
            let service = service.clone();
            handles.push(tokio::spawn(async move {
                let session_id = format!("issue800-session-{session_index}");
                for _ in 0..OBSERVES_PER_SESSION {
                    service
                        .observe_for_mcp_session_id_for_test(
                            Parameters(ObserveParams::default()),
                            &session_id,
                        )
                        .await
                        .map_err(|error| format!("{error:?}"))?;
                }
                Ok::<_, String>(session_id)
            }));
        }

        let mut expected_sessions = Vec::new();
        for handle in handles {
            expected_sessions.push(
                handle
                    .await
                    .map_err(|error| anyhow::anyhow!("observe worker join failed: {error}"))?
                    .map_err(|error| anyhow::anyhow!("observe worker failed: {error}"))?,
            );
        }
        expected_sessions.sort();

        let runtime = service.reflex_runtime()?;
        let runtime = runtime
            .lock()
            .map_err(|_err| anyhow::anyhow!("reflex runtime lock poisoned"))?;
        let counts = runtime.storage_cf_row_counts()?;
        assert_eq!(
            cf_count(&counts, cf::CF_OBSERVATIONS),
            (SESSION_COUNT * OBSERVES_PER_SESSION) as u64
        );
        assert_eq!(
            cf_count(&counts, cf::CF_EVENTS),
            (SESSION_COUNT * OBSERVES_PER_SESSION) as u64
        );
        assert_eq!(cf_count(&counts, cf::CF_SESSIONS), SESSION_COUNT as u64);

        let observation_rows = runtime
            .storage_cf_tail_rows(cf::CF_OBSERVATIONS, SESSION_COUNT * OBSERVES_PER_SESSION)?;
        let event_rows =
            runtime.storage_cf_tail_rows(cf::CF_EVENTS, SESSION_COUNT * OBSERVES_PER_SESSION)?;
        let session_rows = runtime.storage_cf_prefix_rows(
            cf::CF_SESSIONS,
            b"session/v1/issue800-session-",
            SESSION_COUNT,
        )?;
        drop(runtime);

        let mut observation_counts = std::collections::BTreeMap::<String, usize>::new();
        for (_key, row) in observation_rows {
            let stored_observation: Value = serde_json::from_slice(&row)?;
            let session_id = stored_observation["session_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("stored observation missing session_id"))?
                .to_owned();
            assert!(expected_sessions.binary_search(&session_id).is_ok());
            assert_eq!(stored_observation["reason"], "observe");
            assert_eq!(stored_observation["foreground"]["profile_id"], "notepad");
            assert_eq!(
                stored_observation["foreground"]["process_name"],
                "notepad.exe"
            );
            *observation_counts.entry(session_id).or_default() += 1;
        }
        for session_id in &expected_sessions {
            assert_eq!(
                observation_counts.get(session_id).copied(),
                Some(OBSERVES_PER_SESSION)
            );
        }

        let mut event_counts = std::collections::BTreeMap::<String, usize>::new();
        for (_key, row) in event_rows {
            let stored_event: Value = serde_json::from_slice(&row)?;
            let session_id = stored_event["session_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("stored event missing session_id"))?
                .to_owned();
            assert!(expected_sessions.binary_search(&session_id).is_ok());
            assert_eq!(stored_event["kind"], "perception.observed");
            assert_eq!(stored_event["source"], "perception");
            *event_counts.entry(session_id).or_default() += 1;
        }
        for session_id in &expected_sessions {
            assert_eq!(
                event_counts.get(session_id).copied(),
                Some(OBSERVES_PER_SESSION)
            );
        }

        let mut stored_sessions = Vec::new();
        for (_key, row) in session_rows {
            let stored_session: Value = serde_json::from_slice(&row)?;
            let session_id = stored_session["session_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("stored session missing session_id"))?
                .to_owned();
            assert!(expected_sessions.binary_search(&session_id).is_ok());
            assert_eq!(stored_session["active_profile"], "notepad");
            assert_eq!(
                stored_session["profile_history"][0]["profile_id"],
                "notepad"
            );
            stored_sessions.push(session_id);
        }
        stored_sessions.sort();
        assert_eq!(stored_sessions, expected_sessions);

        Ok(())
    }

    #[tokio::test]
    async fn session_teardown_removes_observe_audit_session_cache() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("notepad.toml"),
            "notepad",
            "productivity",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;
        let session_id = "issue800-session-end";

        service
            .observe_for_mcp_session_id_for_test(Parameters(ObserveParams::default()), session_id)
            .await?;
        {
            let state = service
                .m3_state
                .lock()
                .map_err(|_err| anyhow::anyhow!("M3 service state lock poisoned"))?;
            assert!(state.mcp_audit_sessions.contains_key(session_id));
            assert_eq!(state.mcp_audit_sessions.len(), 1);
        }

        let lifecycle = service.session_lifecycle_state()?;
        let report = lifecycle
            .teardown_session(session_id, "test_session_end")
            .await?;
        assert!(report.audit_session.removed);
        assert_eq!(report.audit_session.cache_sessions_before, 1);
        assert_eq!(report.audit_session.cache_sessions_after, 0);
        assert_eq!(report.failure_count, 0);
        {
            let state = service
                .m3_state
                .lock()
                .map_err(|_err| anyhow::anyhow!("M3 service state lock poisoned"))?;
            assert!(!state.mcp_audit_sessions.contains_key(session_id));
            assert!(state.mcp_audit_sessions.is_empty());
        }

        let runtime = service.reflex_runtime()?;
        let runtime = runtime
            .lock()
            .map_err(|_err| anyhow::anyhow!("reflex runtime lock poisoned"))?;
        let counts = runtime.storage_cf_row_counts()?;
        assert_eq!(cf_count(&counts, cf::CF_OBSERVATIONS), 1);
        assert_eq!(cf_count(&counts, cf::CF_EVENTS), 1);
        assert_eq!(cf_count(&counts, cf::CF_SESSIONS), 1);

        Ok(())
    }

    #[tokio::test]
    async fn act_clipboard_records_redacted_action_audit_rows() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("notepad.toml"),
            "notepad",
            "productivity",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;

        let before_count = {
            let runtime = service.reflex_runtime()?;
            let runtime = runtime
                .lock()
                .map_err(|_err| anyhow::anyhow!("reflex runtime lock poisoned"))?;
            let before_counts = runtime.storage_cf_row_counts()?;
            cf_count(&before_counts, cf::CF_ACTION_LOG)
        };

        let result = service
            .act_clipboard_for_session_test_entrypoint(
                Parameters(ActClipboardParams {
                    verb: ActClipboardVerb::Read,
                    text: None,
                    format: ActClipboardFormat::Unicode,
                    verify_delta: false,
                    verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
                }),
                "clipboard-audit-test-session",
            )
            .await;

        let (after_count, rows) = {
            let runtime = service.reflex_runtime()?;
            let runtime = runtime
                .lock()
                .map_err(|_err| anyhow::anyhow!("reflex runtime lock poisoned"))?;
            let after_counts = runtime.storage_cf_row_counts()?;
            (
                cf_count(&after_counts, cf::CF_ACTION_LOG),
                runtime.storage_cf_tail_rows(cf::CF_ACTION_LOG, 2)?,
            )
        };

        assert_eq!(after_count, before_count + 2);
        let started: Value = serde_json::from_slice(&rows[0].1)?;
        let finished: Value = serde_json::from_slice(&rows[1].1)?;
        assert_eq!(started["tool"], "act_clipboard");
        assert_eq!(started["status"], "started");
        assert_eq!(started["session_id"], "clipboard-audit-test-session");
        assert_eq!(
            started["audit_context"]["session_id"],
            "clipboard-audit-test-session"
        );
        assert_eq!(started["details"]["verb"], "read");
        assert_eq!(started["details"]["format"], "unicode");
        assert!(started["details"].get("text").is_none());

        assert_eq!(finished["tool"], "act_clipboard");
        assert_eq!(finished["session_id"], "clipboard-audit-test-session");
        assert_eq!(
            finished["audit_context"]["session_id"],
            "clipboard-audit-test-session"
        );
        assert_eq!(
            finished["status"],
            if result.is_ok() { "ok" } else { "error" }
        );
        assert!(finished["details"]["response"].get("text").is_none());
        if result.is_ok() {
            assert_eq!(finished["details"]["response"]["text_present"], true);
        } else {
            assert!(finished["details"].get("request").is_some());
        }
        Ok(())
    }

    #[tokio::test]
    async fn generic_action_audit_result_inherits_current_mcp_session_id() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("notepad.toml"),
            "notepad",
            "productivity",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;
        let session_id = "generic-action-audit-session";

        let before_count = {
            let runtime = service.reflex_runtime()?;
            let runtime = runtime
                .lock()
                .map_err(|_err| anyhow::anyhow!("reflex runtime lock poisoned"))?;
            let before_counts = runtime.storage_cf_row_counts()?;
            cf_count(&before_counts, cf::CF_ACTION_LOG)
        };

        let result: Result<Value, rmcp::ErrorData> = Ok(json!({
            "ok": true,
            "backend_used": "software",
            "backend_tier_used": "foreground",
            "required_foreground": true,
        }));
        crate::http::with_current_mcp_session_id_for_test(session_id, async {
            service.audit_action_result("act_press", &result)
        })
        .await?;

        let (after_count, rows) = {
            let runtime = service.reflex_runtime()?;
            let runtime = runtime
                .lock()
                .map_err(|_err| anyhow::anyhow!("reflex runtime lock poisoned"))?;
            let after_counts = runtime.storage_cf_row_counts()?;
            (
                cf_count(&after_counts, cf::CF_ACTION_LOG),
                runtime.storage_cf_tail_rows(cf::CF_ACTION_LOG, 1)?,
            )
        };

        assert_eq!(after_count, before_count + 1);
        let stored: Value = serde_json::from_slice(&rows[0].1)?;
        assert_eq!(stored["tool"], "act_press");
        assert_eq!(stored["status"], "ok");
        assert_eq!(stored["session_id"], session_id);
        assert_eq!(stored["audit_context"]["session_id"], session_id);
        assert_eq!(
            stored["details"]["response"]["backend_tier_used"],
            "foreground"
        );
        assert_eq!(stored["details"]["response"]["required_foreground"], true);
        Ok(())
    }

    #[tokio::test]
    async fn session_teardown_removes_virtual_clipboard_buffer() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        let service = service_with_profiles(profiles.path(), false)?;
        let session_id = "clipboard-cleanup-session";

        service.act_clipboard_for_session(
            ActClipboardParams {
                verb: ActClipboardVerb::Write,
                text: Some("cleanup-known-input".to_owned()),
                format: ActClipboardFormat::Unicode,
                verify_delta: false,
                verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
            },
            session_id,
            "session_clipboard_buffer",
        )?;

        let before = service.act_clipboard_for_session(
            ActClipboardParams {
                verb: ActClipboardVerb::Read,
                text: None,
                format: ActClipboardFormat::Unicode,
                verify_delta: false,
                verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
            },
            session_id,
            "session_clipboard_buffer",
        )?;
        assert_eq!(before.text.as_deref(), Some("cleanup-known-input"));

        let report = service
            .session_lifecycle_state()?
            .teardown_session(session_id, "test_clipboard_cleanup")
            .await?;

        assert!(report.clipboard.buffer_existed_before);
        assert!(report.clipboard.removed);
        assert!(!report.clipboard.buffer_exists_after);
        assert_eq!(report.failure_count, 0);

        let after = service.act_clipboard_for_session(
            ActClipboardParams {
                verb: ActClipboardVerb::Read,
                text: None,
                format: ActClipboardFormat::Unicode,
                verify_delta: false,
                verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
            },
            session_id,
            "session_clipboard_buffer",
        )?;
        assert_eq!(after.text.as_deref(), Some(""));

        Ok(())
    }

    #[tokio::test]
    async fn read_only_tools_remain_available_with_active_unknown_scope() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(&profiles.path().join("unknown.toml"), "unknown", "unknown")?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;
        let runtime = service.profile_runtime()?;
        runtime.activate("unknown")?;

        assert!(service.health_payload().ok);

        let observation = service
            .observe_without_request_context_for_test(Parameters(ObserveParams::default()))
            .await?;
        assert_eq!(observation.0.foreground.process_name, "notepad.exe");

        let matches = service
            .find_without_request_context_for_test(Parameters(FindParams {
                query: Some("Document".to_owned()),
                role: None,
                name_substring: None,
                automation_id: None,
                scope: Some(FindScope::Elements),
                limit: Some(5),
                in_window: None,
                window_hwnd: None,
            }))
            .await?;
        assert!(
            matches
                .0
                .results
                .iter()
                .any(|result| result.name.as_deref() == Some("Document"))
        );

        let ocr =
            service.read_text_without_request_context_for_test(Parameters(ReadTextParams {
                region: Some(Rect {
                    x: 12,
                    y: 80,
                    w: 120,
                    h: 40,
                }),
                element_id: None,
                window_hwnd: None,
                backend: synapse_core::OcrBackend::Winrt,
                lang_hint: None,
                require_text: false,
            }))?;
        assert_eq!(ocr.0.full_text, "Synapse");

        let subscription = crate::m3::subscribe::subscribe_to_events(
            &service.sse_state()?,
            &SubscribeParams {
                kinds: Vec::new(),
                filter: Some(EventFilter::All),
                snapshot_first: false,
                buffer_size: 4096,
            },
            Some("scope-gate-test-session".to_owned()),
        )?;
        assert!(!subscription.subscription_id.is_empty());
        Ok(())
    }

    #[test]
    fn reflex_action_gate_rechecks_active_profile_scope_on_dispatch() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("single-player.toml"),
            "single-player",
            "single_player",
        )?;
        write_profile(&profiles.path().join("unknown.toml"), "unknown", "unknown")?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_process_input(&service, "single-player.exe", "Single Player", 0x4567)?;
        let runtime = service.profile_runtime()?;
        runtime.activate("single-player")?;
        let gate = service.reflex_action_gate()?;
        let reflex_id = "reflex-profile-transition".to_owned();
        let action = Action::ReleaseAll;

        gate.ensure_action_allowed(&reflex_id, &action)
            .map_err(|denial| anyhow::anyhow!("single-player dispatch denied: {denial:?}"))?;

        install_synthetic_process_input(&service, "unknown.exe", "Unknown App", 0x4568)?;
        runtime.activate("unknown")?;
        let denial = match gate.ensure_action_allowed(&reflex_id, &action) {
            Ok(()) => anyhow::bail!("unknown active profile must deny reflex dispatch"),
            Err(denial) => denial,
        };
        assert_eq!(denial.policy_reason.as_deref(), Some("unknown_scope"));
        assert_eq!(denial.profile_id.as_deref(), Some("unknown"));
        assert_eq!(denial.use_scope.as_deref(), Some("unknown"));
        Ok(())
    }

    #[tokio::test]
    async fn observe_reevaluates_foreground_and_publishes_scope_transition_within_200ms()
    -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("notepad.toml"),
            "notepad",
            "productivity",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        let subscription = service.sse_state()?.event_bus().subscribe(
            EventFilter::All,
            vec![
                PROFILE_CHANGED_KIND.to_owned(),
                SCOPE_TRANSITIONED_KIND.to_owned(),
            ],
            false,
        )?;

        install_synthetic_notepad_input(&service)?;
        let first = service
            .observe_without_request_context_for_test(Parameters(ObserveParams::default()))
            .await?;
        assert_eq!(first.0.foreground.profile_id.as_deref(), Some("notepad"));
        let runtime = service.profile_runtime()?;
        assert_eq!(runtime.active_profile_id()?.as_deref(), Some("notepad"));
        let _initial_events = subscription.drain();

        install_synthetic_process_input(&service, "unprofiled.exe", "Unprofiled App", 0x6789)?;
        let started = std::time::Instant::now();
        let second = service
            .observe_without_request_context_for_test(Parameters(ObserveParams::default()))
            .await?;
        let elapsed = started.elapsed();
        assert!(elapsed <= Duration::from_millis(200));
        assert_eq!(second.0.foreground.profile_id, None);
        assert_eq!(runtime.active_profile_id()?, None);

        let events = subscription.drain();
        assert!(
            events
                .iter()
                .any(|event| event.kind == PROFILE_CHANGED_KIND)
        );
        let Some(scope_event) = events
            .iter()
            .find(|event| event.kind == SCOPE_TRANSITIONED_KIND)
        else {
            anyhow::bail!("scope-transitioned event missing: {events:?}");
        };
        assert_eq!(
            scope_event.data.get("old_scope"),
            Some(&json!("productivity"))
        );
        assert_eq!(scope_event.data.get("new_scope"), Some(&json!("unknown")));
        assert_eq!(scope_event.data.get("new_profile_id"), Some(&json!(null)));
        Ok(())
    }

    #[test]
    fn action_scope_gate_reevaluates_foreground_and_denies_no_profile_after_transition()
    -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("notepad.toml"),
            "notepad",
            "productivity",
        )?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;

        service.ensure_supported_use_allows_action("act_press")?;
        let runtime = service.profile_runtime()?;
        assert_eq!(runtime.active_profile_id()?.as_deref(), Some("notepad"));

        install_synthetic_process_input(&service, "unprofiled.exe", "Unprofiled App", 0x6789)?;
        let error = match service.ensure_supported_use_allows_action("act_press") {
            Ok(_) => anyhow::bail!("unprofiled foreground must fail closed"),
            Err(error) => error,
        };
        assert_eq!(runtime.active_profile_id()?, None);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::SAFETY_PROFILE_ACTION_DENIED))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("reason")),
            Some(&json!("no_profile"))
        );
        Ok(())
    }

    #[test]
    fn action_scope_gate_reevaluates_foreground_and_denies_unknown_scope_after_transition()
    -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        write_profile(
            &profiles.path().join("notepad.toml"),
            "notepad",
            "productivity",
        )?;
        write_profile(&profiles.path().join("unknown.toml"), "unknown", "unknown")?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;

        service.ensure_supported_use_allows_action("act_press")?;
        let runtime = service.profile_runtime()?;
        assert_eq!(runtime.active_profile_id()?.as_deref(), Some("notepad"));

        install_synthetic_process_input(&service, "unknown.exe", "Unknown App", 0x4568)?;
        let error = match service.ensure_supported_use_allows_action("act_press") {
            Ok(_) => anyhow::bail!("unknown-scope foreground must fail closed"),
            Err(error) => error,
        };
        assert_eq!(runtime.active_profile_id()?.as_deref(), Some("unknown"));
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("reason")),
            Some(&json!("unknown_scope"))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("profile_id")),
            Some(&json!("unknown"))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("use_scope")),
            Some(&json!("unknown"))
        );
        Ok(())
    }

    #[test]
    fn aim_track_target_source_reads_shallow_observe_child_elements() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        let service = service_with_profiles(profiles.path(), false)?;
        install_synthetic_notepad_input(&service)?;

        let source = M1AimTrackTargetSource {
            m1_state: service.m1_state.clone(),
        };
        let snapshot = source.snapshot();

        assert_eq!(AIM_TRACK_TARGET_SOURCE_DEPTH, 2);
        assert!(snapshot.source_error.is_none());
        assert!(
            snapshot
                .elements
                .iter()
                .any(|element| { element.element_id == element_id(0x1234, "0000002a00000000") })
        );
        assert!(
            snapshot
                .elements
                .iter()
                .any(|element| { element.element_id == element_id(0x1234, "0000002a00000001") })
        );

        Ok(())
    }

    fn service_with_profiles(
        profile_dir: &Path,
        allow_unknown_profile: bool,
    ) -> anyhow::Result<SynapseService> {
        let shutdown_cancel = CancellationToken::new();
        let connection_closed_cancel = CancellationToken::new();
        SynapseService::try_with_m2_shutdown_reason_and_m3_config(
            shutdown_cancel,
            "test",
            connection_closed_cancel,
            &M2ServiceConfig::default(),
            M3ServiceConfig::from_cli_parts(
                Some(profile_dir.join("db")),
                Some(profile_dir.to_path_buf()),
                false,
                "127.0.0.1:0".to_owned(),
                NonZeroUsize::new(4)
                    .ok_or_else(|| anyhow::anyhow!("max subscriptions must be nonzero"))?,
                false,
                allow_unknown_profile,
                None,
                false,
                None,
            ),
            M4ServiceConfig::default(),
        )
    }

    fn cf_count(counts: &std::collections::BTreeMap<String, u64>, cf_name: &str) -> u64 {
        counts.get(cf_name).copied().unwrap_or(0)
    }

    fn extensions_with_http_parts(session_id: Option<&str>) -> anyhow::Result<Extensions> {
        let mut builder = axum::http::Request::builder().method("POST").uri("/mcp");
        if let Some(session_id) = session_id {
            builder = builder.header(MCP_SESSION_ID_HEADER, session_id);
        }
        let request = builder.body(())?;
        let (parts, _body) = request.into_parts();
        let mut extensions = Extensions::new();
        extensions.insert(parts);
        Ok(extensions)
    }

    fn write_profile(path: &Path, id: &str, use_scope: &str) -> anyhow::Result<()> {
        fs::write(
            path,
            format!(
                r#"
id = "{id}"
label = "{id}"
schema_version = 2
use_scope = "{use_scope}"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "{id}.exe"

[detection]
classes_of_interest = ["window"]
confidence_threshold = 0.50
max_detections = 8
"#
            ),
        )?;
        Ok(())
    }

    fn install_synthetic_notepad_input(service: &SynapseService) -> anyhow::Result<()> {
        install_synthetic_input(service, synthetic_notepad_input())
    }

    fn install_synthetic_process_input(
        service: &SynapseService,
        process_name: &str,
        window_title: &str,
        hwnd: i64,
    ) -> anyhow::Result<()> {
        install_synthetic_input(
            service,
            synthetic_process_input(process_name, window_title, hwnd),
        )
    }

    fn install_synthetic_input(
        service: &SynapseService,
        input: ObservationInput,
    ) -> anyhow::Result<()> {
        let mut state = service.m1_state.lock().map_err(|_err| {
            anyhow::anyhow!("M1 service state lock poisoned while installing synthetic input")
        })?;
        state.synthetic = Some(input);
        drop(state);
        Ok(())
    }

    fn act_type_preflight_for(foreground: &ForegroundContext) -> ActionPreflightReadback {
        let proof = foreground_proof_for(foreground);
        ActionPreflightReadback {
            tool: "act_type",
            target_profile_id: None,
            active_profile_id_before: None,
            applied: false,
            status: "not_applicable",
            before: proof.clone(),
            candidate_count: None,
            focus_attempted: false,
            focus_hwnd: None,
            focus_error: None,
            after: Some(proof),
            readback_error: None,
        }
    }

    fn foreground_proof_for(foreground: &ForegroundContext) -> ForegroundProof {
        ForegroundProof {
            hwnd: foreground.hwnd,
            pid: foreground.pid,
            process_name: foreground.process_name.clone(),
            process_path: foreground.process_path.clone(),
            window_title: foreground.window_title.clone(),
            is_minimized: Some(false),
            minimized_readback_error: None,
            observed_profile_id: foreground.profile_id.clone(),
        }
    }

    fn synthetic_process_input(
        process_name: &str,
        window_title: &str,
        hwnd: i64,
    ) -> ObservationInput {
        let mut input = synthetic_notepad_input();
        input.foreground.hwnd = hwnd;
        input.foreground.pid = u32::try_from(hwnd & 0xffff).unwrap_or(0);
        input.foreground.process_name = process_name.to_owned();
        input.foreground.process_path = format!("C:\\Synthetic\\{process_name}");
        input.foreground.window_title = window_title.to_owned();
        input.foreground.profile_id = None;
        input
    }

    fn synthetic_notepad_input() -> ObservationInput {
        let document_id = element_id(0x1234, "0000002a00000001");
        let mut input = ObservationInput::new(ForegroundContext {
            hwnd: 0x1234,
            pid: 44,
            process_name: "notepad.exe".to_owned(),
            process_path: "C:\\Windows\\System32\\notepad.exe".to_owned(),
            window_title: "manual.txt - Notepad".to_owned(),
            window_bounds: Rect {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
            },
            monitor_index: 0,
            dpi_scale: 1.0,
            profile_id: None,
            steam_appid: None,
            is_fullscreen: false,
            is_dwm_composed: true,
        });
        input.focused = Some(FocusedElement {
            element_id: document_id.clone(),
            name: "Document".to_owned(),
            role: "Edit".to_owned(),
            automation_id: Some("15".to_owned()),
            bbox: Rect {
                x: 12,
                y: 80,
                w: 760,
                h: 480,
            },
            enabled: true,
            patterns: vec![UiaPattern::Text, UiaPattern::Value],
            value: Some("Synthetic Synapse text".to_owned()),
            selected_text: None,
        });
        input.elements = vec![
            AccessibleNode {
                element_id: element_id(0x1234, "0000002a00000000"),
                parent: None,
                name: "Notepad".to_owned(),
                role: "Window".to_owned(),
                automation_id: None,
                value: None,
                bbox: Rect {
                    x: 10,
                    y: 20,
                    w: 800,
                    h: 600,
                },
                enabled: true,
                focused: false,
                patterns: Vec::new(),
                children_count: 1,
                depth: 0,
            },
            AccessibleNode {
                element_id: document_id,
                parent: Some(element_id(0x1234, "0000002a00000000")),
                name: "Document".to_owned(),
                role: "Edit".to_owned(),
                automation_id: Some("15".to_owned()),
                value: None,
                bbox: Rect {
                    x: 12,
                    y: 80,
                    w: 760,
                    h: 480,
                },
                enabled: true,
                focused: true,
                patterns: vec![UiaPattern::Text, UiaPattern::Value],
                children_count: 0,
                depth: 1,
            },
        ];
        input.a11y_status = SensorStatus::Healthy;
        input.capture_status = SensorStatus::Healthy;
        input.detection_status = SensorStatus::Disabled;
        input.audio_status = SensorStatus::Disabled;
        input
    }
}

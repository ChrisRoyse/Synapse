use rmcp::{RoleServer, service::RequestContext};
use serde_json::{Value, json};

use crate::{
    m3::{
        audit_export::export_audit_bundle,
        demo_recording::{demo_record_status_snapshot, start_demo_recording, stop_demo_recording},
        profile_registry::query_audit_intelligence,
        replay::record_replay,
    },
    server::{
        ErrorData, Json, Parameters, SynapseService,
        command_audit::{CommandAuditInput, command_audit_error_from_error_data},
        context::mcp_session_id_from_request_context,
        tool, tool_router,
    },
};

use super::{
    AUDIT_SOT, AUDIT_TOOL, DEFAULT_ARTIFACT_MAX_BYTES, DEFAULT_ARTIFACT_MAX_RECORDS, REPLAY_SOT,
    REPLAY_TOOL,
    artifact::inspect_replay_artifact,
    command_query::summarize_command_query,
    errors::{delegate_error, missing_spec},
    lifecycle::{lifecycle_path, read_lifecycle_tail},
    response::{audit_response, replay_response},
    types::{
        AuditOperation, AuditParams, AuditResponse, ReplayArtifactInspectParams, ReplayOperation,
        ReplayParams, ReplayResponse,
    },
    validation::{validate_audit_params, validate_replay_params},
};
#[tool_router(router = audit_replay_facade_tool_router, vis = "pub(in crate::server)")]
impl SynapseService {
    #[tool(
        description = "Public audit facade for the <=40 MCP surface. operation=command_query reads bounded CF_ACTION_LOG metadata without raw payloads (default is newest-first: with no start_key_hex/start_ts_ns it returns the most recent matches as a complete page and reports has_older + oldest_returned_ts_ns; supplying start_ts_ns or start_key_hex switches to forward paging); lifecycle_events/lifecycle_exits read sanitized daemon JSONL ledgers; profile_intelligence summarizes profile-linked audit rows; export_bundle writes a redacted local bundle only with explicit consent."
    )]
    pub async fn audit(
        &self,
        params: Parameters<AuditParams>,
    ) -> Result<Json<AuditResponse>, ErrorData> {
        let operation = validate_audit_params(&params.0)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = AUDIT_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=audit"
        );
        match operation {
            AuditOperation::CommandQuery => {
                let spec = params
                    .0
                    .command_query
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), AUDIT_SOT))?;
                let response = self
                    .command_audit_query(spec.into())
                    .map_err(|error| delegate_error(AUDIT_TOOL, operation.as_str(), "CF_ACTION_LOG", AUDIT_SOT, error, "tighten the audit filters or repair CF_ACTION_LOG before retrying command_query"))?;
                let sanitized = summarize_command_query(response)?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "CF_ACTION_LOG scanned_rows={} returned_count={}",
                        sanitized.scanned_rows, sanitized.returned_count
                    ),
                    |out| out.command_query = Some(sanitized),
                )))
            }
            AuditOperation::LifecycleEvents => {
                let spec = params.0.lifecycle_events.unwrap_or_default();
                let path = lifecycle_path("tool_events_path")?;
                let response = read_lifecycle_tail(&path, &spec)?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "{} lines_read={} returned_count={}",
                        path.display(),
                        response.total_lines_read,
                        response.returned_count
                    ),
                    |out| out.lifecycle_events = Some(response),
                )))
            }
            AuditOperation::LifecycleExits => {
                let spec = params.0.lifecycle_exits.unwrap_or_default();
                let path = lifecycle_path("exit_events_path")?;
                let response = read_lifecycle_tail(&path, &spec)?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "{} lines_read={} returned_count={}",
                        path.display(),
                        response.total_lines_read,
                        response.returned_count
                    ),
                    |out| out.lifecycle_exits = Some(response),
                )))
            }
            AuditOperation::ProfileIntelligence => {
                let spec = params
                    .0
                    .profile_intelligence
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), AUDIT_SOT))?;
                self.require_m3_permissions(
                    AUDIT_TOOL,
                    &crate::m3::profile_registry::required_permissions_audit(&spec),
                )?;
                let reflex_runtime = self.reflex_runtime().map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "reflex_runtime",
                        AUDIT_SOT,
                        error,
                        "repair M3 storage initialization before retrying profile_intelligence",
                    )
                })?;
                let response = query_audit_intelligence(&reflex_runtime, &spec).map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        &spec.profile_id,
                        AUDIT_SOT,
                        error,
                        "inspect profile id and audit CF health before retrying profile_intelligence",
                    )
                })?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "profile_id={} max_rows={}",
                        response.profile_id, response.max_rows
                    ),
                    |out| out.profile_intelligence = Some(response),
                )))
            }
            AuditOperation::ExportBundle => {
                let spec = params
                    .0
                    .export_bundle
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), AUDIT_SOT))?;
                self.require_m3_permissions(
                    AUDIT_TOOL,
                    &crate::m3::audit_export::required_permissions_bundle(&spec),
                )?;
                let reflex_runtime = self.reflex_runtime().map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "reflex_runtime",
                        AUDIT_SOT,
                        error,
                        "repair M3 storage initialization before retrying export_bundle",
                    )
                })?;
                let response = export_audit_bundle(&reflex_runtime, &spec).map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        &spec.profile_id,
                        AUDIT_SOT,
                        error,
                        "provide explicit enabled strict consent and inspect consent/output-file readbacks",
                    )
                })?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "manifest={} rows={} redacted_fields={}",
                        response.manifest_path, response.rows_exported, response.redacted_fields
                    ),
                    |out| out.export_bundle = Some(response),
                )))
            }
        }
    }

    #[tool(
        description = "Public replay facade for the <=40 MCP surface. operation=record writes a replay JSONL file and immediately inspects it; demo_status/demo_start/demo_stop manage explicit UIA demo recording through CF_KV/CF_TIMELINE; artifact_inspect validates replay JSONL bytes and structure without raw payload dumps."
    )]
    pub async fn replay(
        &self,
        params: Parameters<ReplayParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ReplayResponse>, ErrorData> {
        let operation = validate_replay_params(&params.0)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = REPLAY_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=replay"
        );
        match operation {
            ReplayOperation::Record => {
                let spec = params
                    .0
                    .record
                    .ok_or_else(|| missing_spec(REPLAY_TOOL, operation.as_str(), REPLAY_SOT))?;
                self.require_m3_permissions(
                    REPLAY_TOOL,
                    &crate::m3::replay::required_permissions(&spec),
                )?;
                let sse_state = self.sse_state().map_err(|error| {
                    delegate_error(
                        REPLAY_TOOL,
                        operation.as_str(),
                        "sse_state",
                        REPLAY_SOT,
                        error,
                        "repair SSE state initialization before retrying replay record",
                    )
                })?;
                let response = record_replay(self.m1_state.clone(), sse_state, &spec)
                    .await
                    .map_err(|error| {
                        delegate_error(
                            REPLAY_TOOL,
                            operation.as_str(),
                            spec.path.as_deref().unwrap_or("default_replay_path"),
                            REPLAY_SOT,
                            error,
                            "fix replay target/format/path and inspect the replay artifact root",
                        )
                    })?;
                let artifact = inspect_replay_artifact(&ReplayArtifactInspectParams {
                    path: response.path.clone(),
                    max_bytes: DEFAULT_ARTIFACT_MAX_BYTES,
                    max_records: DEFAULT_ARTIFACT_MAX_RECORDS,
                })?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "{} bytes={} records_read={}",
                        artifact.path, artifact.bytes, artifact.records_read
                    ),
                    |out| {
                        out.record = Some(response);
                        out.artifact_readback = Some(artifact);
                    },
                )))
            }
            ReplayOperation::DemoStatus => {
                let _spec = params.0.demo_status.unwrap_or_default();
                let status = demo_record_status_snapshot(&self.m3_state).map_err(|error| {
                    delegate_error(
                        REPLAY_TOOL,
                        operation.as_str(),
                        "timeline/demo-record/v1",
                        REPLAY_SOT,
                        error,
                        "inspect CF_KV timeline/demo-record/v1 and retry demo_status",
                    )
                })?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "{} armed={} expired_active_row={}",
                        status.source_of_truth, status.armed, status.expired_active_row
                    ),
                    |out| out.demo_status = Some(status),
                )))
            }
            ReplayOperation::DemoStart => {
                let spec = params
                    .0
                    .demo_start
                    .ok_or_else(|| missing_spec(REPLAY_TOOL, operation.as_str(), REPLAY_SOT))?;
                self.require_m3_permissions(
                    REPLAY_TOOL,
                    &crate::m3::demo_recording::required_permissions_start(&spec),
                )?;
                let by_session = mcp_session_id_from_request_context(&request_context)?
                    .unwrap_or_else(|| "stdio".to_owned());
                let command_payload = json!({
                    "profile_id": &spec.profile_id,
                    "duration_ms": spec.duration_ms,
                    "path": &spec.path,
                    "label": &spec.label,
                });
                let command_before = json!({
                    "source_of_truth": REPLAY_SOT,
                    "by_session": &by_session,
                    "operation": "demo_start",
                });
                self.command_audit_intent(CommandAuditInput::mcp(
                    REPLAY_TOOL,
                    "demo_start",
                    Some(by_session.clone()),
                    Some(by_session.clone()),
                    command_payload.clone(),
                    command_before.clone(),
                    Value::Null,
                    "pending",
                ))?;
                let result = start_demo_recording(&self.m3_state, &spec, &by_session);
                match &result {
                    Ok(response) => {
                        self.command_audit_final(CommandAuditInput::mcp(
                            REPLAY_TOOL,
                            "demo_start",
                            Some(by_session.clone()),
                            Some(by_session),
                            command_payload,
                            command_before,
                            json!({
                                "source_of_truth": REPLAY_SOT,
                                "demo_id": response.demo_id,
                                "replay_path": response.replay_path,
                                "persisted": response.persisted,
                                "marker_row_written": response.marker_row_written,
                            }),
                            "ok",
                        ))?;
                    }
                    Err(error) => {
                        self.command_audit_final(
                            CommandAuditInput::mcp(
                                REPLAY_TOOL,
                                "demo_start",
                                Some(by_session.clone()),
                                Some(by_session),
                                command_payload,
                                command_before,
                                json!({
                                    "source_of_truth": REPLAY_SOT,
                                    "operation": "demo_start",
                                }),
                                "error",
                            )
                            .with_error(command_audit_error_from_error_data(error)),
                        )?;
                    }
                }
                let response = result.map_err(|error| {
                    delegate_error(
                        REPLAY_TOOL,
                        operation.as_str(),
                        &spec.profile_id,
                        REPLAY_SOT,
                        error,
                        "fix demo profile/duration/path and inspect CF_KV/CF_TIMELINE rows",
                    )
                })?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "demo_id={} persisted={} marker_row_written={}",
                        response.demo_id, response.persisted, response.marker_row_written
                    ),
                    |out| out.demo_start = Some(response),
                )))
            }
            ReplayOperation::DemoStop => {
                let spec = params
                    .0
                    .demo_stop
                    .ok_or_else(|| missing_spec(REPLAY_TOOL, operation.as_str(), REPLAY_SOT))?;
                self.require_m3_permissions(
                    REPLAY_TOOL,
                    &crate::m3::demo_recording::required_permissions_stop(&spec),
                )?;
                let by_session = mcp_session_id_from_request_context(&request_context)?
                    .unwrap_or_else(|| "stdio".to_owned());
                let command_payload = json!({
                    "demo_id": &spec.demo_id,
                });
                let command_before = json!({
                    "source_of_truth": REPLAY_SOT,
                    "by_session": &by_session,
                    "operation": "demo_stop",
                });
                self.command_audit_intent(CommandAuditInput::mcp(
                    REPLAY_TOOL,
                    "demo_stop",
                    Some(by_session.clone()),
                    Some(by_session.clone()),
                    command_payload.clone(),
                    command_before.clone(),
                    Value::Null,
                    "pending",
                ))?;
                let result = stop_demo_recording(&self.m3_state, &spec, &by_session);
                match &result {
                    Ok(response) => {
                        self.command_audit_final(CommandAuditInput::mcp(
                            REPLAY_TOOL,
                            "demo_stop",
                            Some(by_session.clone()),
                            Some(by_session),
                            command_payload,
                            command_before,
                            json!({
                                "source_of_truth": REPLAY_SOT,
                                "demo_id": response.demo_id,
                                "replay_path": response.replay_path,
                                "records_written": response.records_written,
                                "bytes": response.bytes,
                            }),
                            "ok",
                        ))?;
                    }
                    Err(error) => {
                        self.command_audit_final(
                            CommandAuditInput::mcp(
                                REPLAY_TOOL,
                                "demo_stop",
                                Some(by_session.clone()),
                                Some(by_session),
                                command_payload,
                                command_before,
                                json!({
                                    "source_of_truth": REPLAY_SOT,
                                    "operation": "demo_stop",
                                }),
                                "error",
                            )
                            .with_error(command_audit_error_from_error_data(error)),
                        )?;
                    }
                }
                let response = result.map_err(|error| {
                    delegate_error(
                        REPLAY_TOOL,
                        operation.as_str(),
                        spec.demo_id.as_deref().unwrap_or("active_demo_recording"),
                        REPLAY_SOT,
                        error,
                        "inspect active demo status and CF_TIMELINE DemoMarker rows before retrying demo_stop",
                    )
                })?;
                let artifact = inspect_replay_artifact(&ReplayArtifactInspectParams {
                    path: response.replay_path.clone(),
                    max_bytes: DEFAULT_ARTIFACT_MAX_BYTES,
                    max_records: DEFAULT_ARTIFACT_MAX_RECORDS,
                })?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "{} bytes={} records_read={}",
                        artifact.path, artifact.bytes, artifact.records_read
                    ),
                    |out| {
                        out.demo_stop = Some(response);
                        out.artifact_readback = Some(artifact);
                    },
                )))
            }
            ReplayOperation::ArtifactInspect => {
                let spec = params
                    .0
                    .artifact_inspect
                    .ok_or_else(|| missing_spec(REPLAY_TOOL, operation.as_str(), REPLAY_SOT))?;
                let response = inspect_replay_artifact(&spec)?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "{} bytes={} records_read={}",
                        response.path, response.bytes, response.records_read
                    ),
                    |out| out.artifact_readback = Some(response),
                )))
            }
        }
    }
}

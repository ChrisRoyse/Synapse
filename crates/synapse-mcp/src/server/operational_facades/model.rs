use rmcp::{RoleServer, service::RequestContext};

use crate::{
    m3::local_models::{LocalModelListParams, LocalModelListResponse},
    server::{ErrorData, Json, Parameters, SynapseService},
};

use super::{
    MODEL_SOT, MODEL_TOOL,
    errors::{facade_delegate_error, missing_spec},
    policy::{require_maintenance_profile, session_or_stdio},
    response::model_response,
    types::{ModelOperation, ModelParams, ModelResponse, ModelStatusResponse},
    validation::validate_model_params,
};
pub(super) async fn handle(
    service: &SynapseService,
    params: Parameters<ModelParams>,
    request_context: RequestContext<RoleServer>,
) -> Result<Json<ModelResponse>, ErrorData> {
    validate_model_params(&params.0)?;
    let operation = params.0.operation;
    tracing::info!(
        code = "MCP_TOOL_INVOCATION",
        kind = MODEL_TOOL,
        operation = operation.as_str(),
        "tool.invocation kind=model"
    );
    let by_session = session_or_stdio(&request_context)?;
    let db = service.m3_storage().map_err(|error| {
        facade_delegate_error(
            MODEL_TOOL,
            operation.as_str(),
            "m3_storage",
            MODEL_SOT,
            error,
            "repair M3 storage and retry the model registry operation",
        )
    })?;
    match operation {
        ModelOperation::List => {
            let spec = params.0.list.unwrap_or(LocalModelListParams {
                name: None,
                include_disabled: true,
                limit: 100,
            });
            service.require_m3_permissions(
                MODEL_TOOL,
                &crate::m3::local_models::required_permissions_list(&spec),
            )?;
            let response =
                crate::m3::local_models::list_local_models(&db, &spec).map_err(|error| {
                    facade_delegate_error(
                        MODEL_TOOL,
                        operation.as_str(),
                        spec.name.as_deref().unwrap_or("registry"),
                        MODEL_SOT,
                        error,
                        "inspect CF_KV local model registry rows and corrupt row diagnostics",
                    )
                })?;
            Ok(Json(model_response(
                operation,
                format!(
                    "registry rows={} corrupt_rows={}",
                    response.rows.len(),
                    response.corrupt_rows.len()
                ),
                |out| out.list = Some(response),
            )))
        }
        ModelOperation::Status => {
            let spec = params.0.status.unwrap_or_default();
            let list_params = LocalModelListParams {
                name: None,
                include_disabled: spec.include_disabled,
                limit: 1000,
            };
            service.require_m3_permissions(
                MODEL_TOOL,
                &crate::m3::local_models::required_permissions_list(&list_params),
            )?;
            let list =
                crate::m3::local_models::list_local_models(&db, &list_params).map_err(|error| {
                    facade_delegate_error(
                        MODEL_TOOL,
                        operation.as_str(),
                        "registry_status",
                        MODEL_SOT,
                        error,
                        "inspect CF_KV local model registry rows and storage health",
                    )
                })?;
            let status = model_status(&list);
            Ok(Json(model_response(
                operation,
                format!(
                    "registry visible_rows={} healthy_rows={} corrupt_rows={}",
                    status.visible_rows, status.healthy_rows, status.corrupt_rows
                ),
                |out| out.status = Some(status),
            )))
        }
        ModelOperation::Probe => {
            let spec = params
                .0
                .probe
                .ok_or_else(|| missing_spec(MODEL_TOOL, "probe"))?;
            service.require_m3_permissions(
                MODEL_TOOL,
                &crate::m3::local_models::required_permissions_probe(&spec),
            )?;
            let response =
                    crate::m3::local_models::probe_local_model(&db, &spec, &by_session)
                        .await
                        .map_err(|error| {
                            facade_delegate_error(
                                MODEL_TOOL,
                                operation.as_str(),
                                &spec.name,
                                MODEL_SOT,
                                error,
                                "repair the real backend endpoint/socket/credentials and retry model operation=probe",
                            )
                        })?;
            Ok(Json(model_response(
                operation,
                format!(
                    "{} probe_status={} healthy={}",
                    response.row.name, response.probe.status, response.probe.healthy
                ),
                |out| out.probe = Some(response),
            )))
        }
        ModelOperation::Register => {
            let spec = params
                .0
                .register
                .ok_or_else(|| missing_spec(MODEL_TOOL, "register"))?;
            require_maintenance_profile(
                service,
                &request_context,
                MODEL_TOOL,
                operation.as_str(),
                &spec.name,
                MODEL_SOT,
            )?;
            service.require_m3_permissions(
                MODEL_TOOL,
                &crate::m3::local_models::required_permissions_register(&spec),
            )?;
            let response =
                    crate::m3::local_models::register_local_model(&db, spec, &by_session)
                        .await
                        .map_err(|error| {
                            facade_delegate_error(
                                MODEL_TOOL,
                                operation.as_str(),
                                "register",
                                MODEL_SOT,
                                error,
                                "fix endpoint/model/key settings until the real structured tool-call probe passes",
                            )
                        })?;
            Ok(Json(model_response(
                operation,
                format!("{} row_key={}", response.row.name, response.row.row_key),
                |out| out.register = Some(response),
            )))
        }
        ModelOperation::Update => {
            let spec = params
                .0
                .update
                .ok_or_else(|| missing_spec(MODEL_TOOL, "update"))?;
            require_maintenance_profile(
                service,
                &request_context,
                MODEL_TOOL,
                operation.as_str(),
                &spec.name,
                MODEL_SOT,
            )?;
            service.require_m3_permissions(
                MODEL_TOOL,
                &crate::m3::local_models::required_permissions_update(&spec),
            )?;
            let response = crate::m3::local_models::update_local_model(&db, spec, &by_session)
                    .await
                    .map_err(|error| {
                        facade_delegate_error(
                            MODEL_TOOL,
                            operation.as_str(),
                            "update",
                            MODEL_SOT,
                            error,
                            "fix endpoint/model/key settings until the real structured tool-call probe passes",
                        )
                    })?;
            Ok(Json(model_response(
                operation,
                format!("{} row_key={}", response.row.name, response.row.row_key),
                |out| out.update = Some(response),
            )))
        }
        ModelOperation::Remove => {
            let spec = params
                .0
                .remove
                .ok_or_else(|| missing_spec(MODEL_TOOL, "remove"))?;
            require_maintenance_profile(
                service,
                &request_context,
                MODEL_TOOL,
                operation.as_str(),
                &spec.name,
                MODEL_SOT,
            )?;
            service.require_m3_permissions(
                MODEL_TOOL,
                &crate::m3::local_models::required_permissions_remove(&spec),
            )?;
            let response =
                crate::m3::local_models::remove_local_model(&db, &spec).map_err(|error| {
                    facade_delegate_error(
                        MODEL_TOOL,
                        operation.as_str(),
                        &spec.name,
                        MODEL_SOT,
                        error,
                        "inspect the exact registry row and retry only if removal is intended",
                    )
                })?;
            Ok(Json(model_response(
                operation,
                format!(
                    "{} after_row_present={}",
                    response.removed_row.name, response.after_row_present
                ),
                |out| out.remove = Some(response),
            )))
        }
    }
}

fn model_status(list: &LocalModelListResponse) -> ModelStatusResponse {
    let enabled_rows = list.rows.iter().filter(|row| row.enabled).count();
    let probed_rows = list
        .rows
        .iter()
        .filter(|row| row.last_probe.is_some())
        .count();
    let healthy_rows = list
        .rows
        .iter()
        .filter(|row| row.last_probe.as_ref().is_some_and(|probe| probe.healthy))
        .count();
    ModelStatusResponse {
        source_of_truth: "CF_KV prefix local_model_registry/v1/model/name_hex/",
        scanned_rows: list.scanned_rows,
        visible_rows: list.rows.len(),
        corrupt_rows: list.corrupt_rows.len(),
        enabled_rows,
        disabled_rows: list.rows.len().saturating_sub(enabled_rows),
        probed_rows,
        healthy_rows,
        unhealthy_rows: probed_rows.saturating_sub(healthy_rows),
        rows_with_api_key_secret: list
            .rows
            .iter()
            .filter(|row| row.has_api_key_secret)
            .count(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_status_counts_probe_health() {
        let list = LocalModelListResponse {
            schema_version: synapse_core::SCHEMA_VERSION,
            source_of_truth: "test".to_owned(),
            scanned_rows: 2,
            rows: vec![
                crate::m3::local_models::LocalModelRegistryRow {
                    schema_version: synapse_core::SCHEMA_VERSION,
                    row_key: "r1".to_owned(),
                    name: "a".to_owned(),
                    base_url: "http://127.0.0.1:1/v1".to_owned(),
                    model_id: "m".to_owned(),
                    api_shape: crate::m3::local_models::LocalModelApiShape::default(),
                    runtime_preset: crate::m3::local_models::LocalModelRuntimePreset::default(),
                    context_length: None,
                    max_tools: None,
                    notes: None,
                    enabled: true,
                    allow_non_loopback: false,
                    api_key_env_var: None,
                    created_at_unix_ms: 1,
                    updated_at_unix_ms: 1,
                    created_by_session: "test".to_owned(),
                    updated_by_session: "test".to_owned(),
                    last_probe: Some(crate::m3::local_models::LocalModelProbeReport {
                        schema_version: synapse_core::SCHEMA_VERSION,
                        observed_at_unix_ms: 1,
                        endpoint_url: "http://127.0.0.1:1/v1/chat/completions".to_owned(),
                        healthy: true,
                        status: "ok".to_owned(),
                        latency_ms: 1,
                        tokens_per_second: Some(1.0),
                        prompt_tokens: Some(1),
                        completion_tokens: Some(1),
                        total_tokens: Some(2),
                        error_code: None,
                        error_phase: None,
                        error_kind: None,
                        error_detail: None,
                        raw_response_sha256: Some("sha256:x".to_owned()),
                        raw_response_excerpt: None,
                        raw_response_truncated: false,
                    }),
                    has_api_key_secret: false,
                },
                crate::m3::local_models::LocalModelRegistryRow {
                    schema_version: synapse_core::SCHEMA_VERSION,
                    row_key: "r2".to_owned(),
                    name: "b".to_owned(),
                    base_url: "http://127.0.0.1:2/v1".to_owned(),
                    model_id: "m".to_owned(),
                    api_shape: crate::m3::local_models::LocalModelApiShape::default(),
                    runtime_preset: crate::m3::local_models::LocalModelRuntimePreset::default(),
                    context_length: None,
                    max_tools: None,
                    notes: None,
                    enabled: false,
                    allow_non_loopback: false,
                    api_key_env_var: None,
                    created_at_unix_ms: 1,
                    updated_at_unix_ms: 1,
                    created_by_session: "test".to_owned(),
                    updated_by_session: "test".to_owned(),
                    last_probe: None,
                    has_api_key_secret: true,
                },
            ],
            corrupt_rows: Vec::new(),
        };
        let status = model_status(&list);
        assert_eq!(status.visible_rows, 2);
        assert_eq!(status.enabled_rows, 1);
        assert_eq!(status.disabled_rows, 1);
        assert_eq!(status.probed_rows, 1);
        assert_eq!(status.healthy_rows, 1);
        assert_eq!(status.rows_with_api_key_secret, 1);
    }
}

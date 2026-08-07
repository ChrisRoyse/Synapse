use rmcp::{RoleServer, service::RequestContext};

use crate::server::agent_cost::{AgentCostGroupBy, AgentCostParams};
use crate::{
    m3::local_models::{LocalModelListParams, LocalModelListResponse},
    server::{ErrorData, Json, Parameters, SynapseService},
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use synapse_core::types::CostOutcome;
use synapse_storage::{RevisionGuard, cf};

use super::{
    MODEL_SOT, MODEL_TOOL,
    errors::{facade_delegate_error, missing_spec},
    policy::{require_maintenance_profile, session_or_stdio},
    response::model_response,
    types::{
        ModelOperation, ModelOverrideParams, ModelOverrideReadback, ModelParams,
        ModelRecommendResponse, ModelRecommendationEvidence, ModelResponse, ModelStatusResponse,
    },
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
        ModelOperation::Recommend => {
            let spec = params
                .0
                .recommend
                .ok_or_else(|| missing_spec(MODEL_TOOL, "recommend"))?;
            service.require_m3_permissions(
                MODEL_TOOL,
                &crate::m3::permissions::required([
                    crate::m3::permissions::Permission::ReadStorage,
                    crate::m3::permissions::Permission::WriteStorage,
                ]),
            )?;
            let recommendation =
                recommend_model(service, &db, &spec.task_class, spec.min_evidence)?;
            service.audit_action_ok_with_details_for_request(
                "steering_model_recommend",
                &serde_json::to_value(&recommendation).map_err(|error| {
                    crate::m1::mcp_error(
                        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                        format!("STEERING_DECISION_AUDIT_ENCODE_FAILED: {error}"),
                    )
                })?,
                &request_context,
            )?;
            Ok(Json(model_response(
                operation,
                format!(
                    "CF_KV task_attempts={} grounding={} decision_row={}",
                    recommendation.evidence_count,
                    recommendation.grounding,
                    recommendation.decision_row_key
                ),
                |out| out.recommend = Some(recommendation),
            )))
        }
        ModelOperation::Override => {
            let spec = params
                .0
                .r#override
                .ok_or_else(|| missing_spec(MODEL_TOOL, "override"))?;
            service.require_m3_permissions(
                MODEL_TOOL,
                &crate::m3::permissions::required([
                    crate::m3::permissions::Permission::ReadStorage,
                    crate::m3::permissions::Permission::WriteStorage,
                ]),
            )?;
            let readback = persist_model_override(&db, &spec)?;
            service.audit_action_ok_with_details_for_request(
                "steering_model_override",
                &serde_json::to_value(&readback).map_err(|error| {
                    crate::m1::mcp_error(
                        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                        format!("STEERING_OVERRIDE_AUDIT_ENCODE_FAILED: {error}"),
                    )
                })?,
                &request_context,
            )?;
            Ok(Json(model_response(
                operation,
                format!(
                    "override task_class={} selected_model={} row={}",
                    readback.task_class, readback.selected_model, readback.history_row_key
                ),
                |out| out.r#override = Some(readback),
            )))
        }
    }
}

#[derive(Default)]
struct CandidateCounts {
    success: u64,
    failure: u64,
    priced_total: u64,
    priced_count: u64,
}

fn model_override_current_key(task_class: &str) -> String {
    format!(
        "steering/v1/override/model/current/{}",
        hex_sha256(task_class.as_bytes())
    )
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct StoredModelOverride {
    schema: String,
    task_class: String,
    decision_id: String,
    selected_model: String,
    reason: String,
    observed_unix_ns: u128,
    history_row_key: String,
}

fn model_override_readback(
    stored: StoredModelOverride,
    current_row_key: String,
    bytes: &[u8],
) -> ModelOverrideReadback {
    ModelOverrideReadback {
        schema: stored.schema,
        task_class: stored.task_class,
        decision_id: stored.decision_id,
        selected_model: stored.selected_model,
        reason: stored.reason,
        observed_unix_ns: stored.observed_unix_ns,
        current_row_key,
        history_row_key: stored.history_row_key,
        value_len_bytes: bytes.len() as u64,
        value_sha256: hex_sha256(bytes),
    }
}

fn read_model_override(
    db: &synapse_storage::Db,
    task_class: &str,
) -> Result<Option<ModelOverrideReadback>, ErrorData> {
    let key = model_override_current_key(task_class);
    let Some(bytes) = db.get_cf(cf::CF_KV, key.as_bytes()).map_err(|error| {
        crate::m1::mcp_error(
            error.code(),
            format!("STEERING_OVERRIDE_READ_FAILED: key={key} detail={error}"),
        )
    })?
    else {
        return Ok(None);
    };
    let stored: StoredModelOverride = serde_json::from_slice(&bytes).map_err(|error| {
        crate::m1::mcp_error(
            synapse_core::error_codes::STORAGE_READ_FAILED,
            format!(
                "STEERING_OVERRIDE_ROW_INVALID: key={key} detail={error}; remediation=repair or remove the corrupt override row with explicit operator intent"
            ),
        )
    })?;
    if stored.task_class != task_class || stored.schema != "synapse.steering.model_override.v1" {
        return Err(crate::m1::mcp_error(
            synapse_core::error_codes::STORAGE_READ_FAILED,
            format!(
                "STEERING_OVERRIDE_IDENTITY_MISMATCH: key={key} decoded task_class={:?} schema={:?}; remediation=repair the corrupt override row",
                stored.task_class, stored.schema
            ),
        ));
    }
    Ok(Some(model_override_readback(stored, key, &bytes)))
}

fn persist_model_override(
    db: &synapse_storage::Db,
    spec: &ModelOverrideParams,
) -> Result<ModelOverrideReadback, ErrorData> {
    let task_class = spec.task_class.trim();
    let decision_id = spec.decision_id.trim();
    let decision_row_key = spec.decision_row_key.trim();
    let selected_model = spec.selected_model.trim();
    let reason = spec.reason.trim();
    if task_class.is_empty()
        || selected_model.is_empty()
        || reason.is_empty()
        || decision_row_key.is_empty()
        || decision_id.len() != 64
        || !decision_id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(crate::m1::mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "model override requires nonblank task_class/decision_row_key/selected_model/reason and a 64-character hex decision_id",
        ));
    }
    if task_class.len() > 512 || selected_model.len() > 512 || reason.len() > 4_096 {
        return Err(crate::m1::mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "model override caps task_class and selected_model at 512 bytes and reason at 4096 bytes",
        ));
    }
    let expected_suffix = format!("/{decision_id}");
    if !decision_row_key.starts_with("steering/v1/decision/model/")
        || !decision_row_key.ends_with(&expected_suffix)
    {
        return Err(crate::m1::mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "model override decision_row_key must identify the supplied decision_id under steering/v1/decision/model/",
        ));
    }
    let decision_bytes = db
        .get_cf(cf::CF_KV, decision_row_key.as_bytes())
        .map_err(|error| crate::m1::mcp_error(error.code(), error.to_string()))?
        .ok_or_else(|| {
            crate::m1::mcp_error(
                synapse_core::error_codes::STORAGE_READ_FAILED,
                format!(
                    "STEERING_OVERRIDE_DECISION_ABSENT: decision row {decision_row_key:?} does not exist"
                ),
            )
        })?;
    let decision: serde_json::Value = serde_json::from_slice(&decision_bytes).map_err(|error| {
        crate::m1::mcp_error(
            synapse_core::error_codes::STORAGE_READ_FAILED,
            format!(
                "STEERING_OVERRIDE_DECISION_INVALID: decision row {decision_row_key:?} failed to decode: {error}"
            ),
        )
    })?;
    if decision.get("schema").and_then(serde_json::Value::as_str)
        != Some("synapse.steering.model_decision.v1")
        || decision
            .get("decision_id")
            .and_then(serde_json::Value::as_str)
            != Some(decision_id)
        || decision
            .get("task_class")
            .and_then(serde_json::Value::as_str)
            != Some(task_class)
    {
        return Err(crate::m1::mcp_error(
            synapse_core::error_codes::STORAGE_READ_FAILED,
            format!(
                "STEERING_OVERRIDE_DECISION_IDENTITY_MISMATCH: row {decision_row_key:?} does not bind the supplied task_class and decision_id"
            ),
        ));
    }
    let observed_unix_ns = unix_time_ns();
    let current_key = model_override_current_key(task_class);
    let history_key =
        format!("steering/v1/override/model/history/{observed_unix_ns}/{decision_id}");
    let stored = StoredModelOverride {
        schema: "synapse.steering.model_override.v1".to_owned(),
        task_class: task_class.to_owned(),
        decision_id: decision_id.to_ascii_lowercase(),
        selected_model: selected_model.to_owned(),
        reason: reason.to_owned(),
        observed_unix_ns,
        history_row_key: history_key.clone(),
    };
    let encoded = serde_json::to_vec(&stored).map_err(|error| {
        crate::m1::mcp_error(
            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            format!("STEERING_OVERRIDE_ENCODE_FAILED: {error}"),
        )
    })?;
    for _ in 0..8 {
        let current = db
            .get_cf_revisioned(cf::CF_KV, current_key.as_bytes())
            .map_err(|error| crate::m1::mcp_error(error.code(), error.to_string()))?;
        let guard = RevisionGuard::new(
            current_key.as_bytes(),
            current.as_ref().map(|value| value.revision_sha256),
        );
        let outcome = db
            .mutate_batch_if_revisions_pressure_bypass(
                cf::CF_KV,
                [guard, RevisionGuard::new(history_key.as_bytes(), None)],
                std::iter::empty::<Vec<u8>>(),
                [
                    (current_key.as_bytes(), encoded.as_slice()),
                    (history_key.as_bytes(), encoded.as_slice()),
                ],
            )
            .map_err(|error| {
                crate::m1::mcp_error(
                    error.code(),
                    format!("STEERING_OVERRIDE_COMMIT_FAILED: {error}"),
                )
            })?;
        if !outcome.applied {
            continue;
        }
        for key in [&current_key, &history_key] {
            let readback = db
                .get_cf(cf::CF_KV, key.as_bytes())
                .map_err(|error| crate::m1::mcp_error(error.code(), error.to_string()))?;
            if readback.as_deref() != Some(encoded.as_slice()) {
                return Err(crate::m1::mcp_error(
                    synapse_core::error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "STEERING_OVERRIDE_READBACK_MISMATCH: key={key} differs from the committed override bytes"
                    ),
                ));
            }
        }
        return Ok(model_override_readback(stored, current_key, &encoded));
    }
    Err(crate::m1::mcp_error(
        synapse_core::error_codes::STORAGE_WRITE_FAILED,
        "STEERING_OVERRIDE_REVISION_CONFLICT: current override changed during 8 guarded attempts; retry after concurrent operator changes stop",
    ))
}

fn recommend_model(
    service: &SynapseService,
    db: &synapse_storage::Db,
    task_class: &str,
    min_evidence: usize,
) -> Result<ModelRecommendResponse, ErrorData> {
    let task_class = task_class.trim();
    if task_class.is_empty() || min_evidence == 0 || min_evidence > 100_000 {
        return Err(crate::m1::mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "model recommend requires a nonblank task_class and min_evidence in 1..=100000",
        ));
    }
    let tasks = SynapseService::read_all_tasks(db)?;
    let costs = service.agent_cost_impl(AgentCostParams {
        spawn_id: None,
        all_history: true,
        since_ns: None,
        until_ns: None,
        include_per_turn: false,
        group_by: vec![AgentCostGroupBy::Task],
    })?;
    let mut cost_by_spawn = BTreeMap::new();
    for spawn in costs.per_spawn {
        if let CostOutcome::Priced { cost } = spawn.cost {
            cost_by_spawn.insert(spawn.spawn_id, cost.total_micro_usd);
        }
    }

    let mut counts: BTreeMap<String, CandidateCounts> = BTreeMap::new();
    let mut excluded_legacy = 0_u64;
    for task in tasks.iter().filter(|task| task.template_id == task_class) {
        for attempt in &task.attempts {
            let terminal_success = match attempt.outcome {
                crate::server::agent_tasks::AttemptOutcome::Succeeded => Some(true),
                crate::server::agent_tasks::AttemptOutcome::Failed => Some(false),
                crate::server::agent_tasks::AttemptOutcome::Pending
                | crate::server::agent_tasks::AttemptOutcome::Orphaned => None,
            };
            let Some(success) = terminal_success else {
                continue;
            };
            let (Some(model), Some(_config_hash)) = (
                attempt.model.as_deref(),
                attempt.template_config_hash.as_deref(),
            ) else {
                excluded_legacy = excluded_legacy.saturating_add(1);
                continue;
            };
            let cell = counts.entry(model.to_owned()).or_default();
            if success {
                cell.success += 1;
            } else {
                cell.failure += 1;
            }
            if let Some(cost) = attempt
                .spawn_id
                .as_ref()
                .and_then(|id| cost_by_spawn.get(id))
            {
                cell.priced_total = cell.priced_total.saturating_add(*cost);
                cell.priced_count = cell.priced_count.saturating_add(1);
            }
        }
    }
    let total_success: u64 = counts.values().map(|cell| cell.success).sum();
    let total_failure: u64 = counts.values().map(|cell| cell.failure).sum();
    let total = total_success.saturating_add(total_failure);
    let mut candidates = counts
        .iter()
        .map(|(model, cell)| {
            let n = cell.success + cell.failure;
            let (low, high) = wilson_interval(cell.success, n);
            ModelRecommendationEvidence {
                model: model.clone(),
                successes: cell.success,
                failures: cell.failure,
                evidence_count: n,
                expected_success: (cell.success as f64 + 1.0) / (n as f64 + 2.0),
                success_ci95_low: low,
                success_ci95_high: high,
                outcome_information_bits: indicator_outcome_mi(
                    cell.success,
                    cell.failure,
                    total_success,
                    total_failure,
                ),
                // #2079: `then_some` evaluates its argument EAGERLY, so the
                // guard never protected the division — priced_count == 0
                // panicked (divide by zero) and killed the daemon. `then`
                // takes a closure and is lazy.
                expected_cost_micro_usd: (cell.priced_count > 0)
                    .then(|| cell.priced_total / cell.priced_count),
                priced_observations: cell.priced_count,
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .expected_success
            .total_cmp(&left.expected_success)
            .then_with(|| {
                left.expected_cost_micro_usd
                    .cmp(&right.expected_cost_micro_usd)
            })
            .then_with(|| left.model.cmp(&right.model))
    });
    let grounding = if total as usize >= min_evidence && candidates.len() >= 2 {
        "grounded"
    } else {
        "provisional_insufficient_evidence"
    };
    let operator_override = read_model_override(db, task_class)?;
    let recommended_model = operator_override
        .as_ref()
        .map(|override_row| override_row.selected_model.clone())
        .or_else(|| candidates.first().map(|candidate| candidate.model.clone()));
    let observed_ns = unix_time_ns();
    let decision_seed =
        serde_json::to_vec(&(&task_class, observed_ns, &candidates)).map_err(|error| {
            crate::m1::mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                format!("STEERING_DECISION_ENCODE_FAILED: {error}"),
            )
        })?;
    let decision_id = hex_sha256(&decision_seed);
    let decision_row_key = format!("steering/v1/decision/model/{observed_ns}/{decision_id}");
    let row = serde_json::to_vec(&serde_json::json!({
        "schema": "synapse.steering.model_decision.v1",
        "decision_id": decision_id,
        "observed_unix_ns": observed_ns,
        "task_class": task_class,
        "grounding": grounding,
        "evidence_count": total,
        "recommended_model": recommended_model,
        "candidates": candidates,
        "excluded_legacy_attempts": excluded_legacy,
        "operator_override": operator_override,
    }))
    .map_err(|error| {
        crate::m1::mcp_error(
            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            format!("STEERING_DECISION_ENCODE_FAILED: {error}"),
        )
    })?;
    let outcome = db
        .mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [RevisionGuard::new(decision_row_key.as_bytes(), None)],
            std::iter::empty::<Vec<u8>>(),
            [(decision_row_key.as_bytes(), row.as_slice())],
        )
        .map_err(|error| {
            crate::m1::mcp_error(
                error.code(),
                format!("STEERING_DECISION_COMMIT_FAILED: {error}"),
            )
        })?;
    if !outcome.applied {
        return Err(crate::m1::mcp_error(
            synapse_core::error_codes::STORAGE_WRITE_FAILED,
            "STEERING_DECISION_ID_COLLISION: append-only decision key already exists",
        ));
    }
    let readback = db
        .get_cf(cf::CF_KV, decision_row_key.as_bytes())
        .map_err(|error| {
            crate::m1::mcp_error(
                error.code(),
                format!("STEERING_DECISION_READBACK_FAILED: {error}"),
            )
        })?;
    if readback.as_deref() != Some(row.as_slice()) {
        return Err(crate::m1::mcp_error(
            synapse_core::error_codes::STORAGE_WRITE_FAILED,
            "STEERING_DECISION_READBACK_MISMATCH: committed row bytes differ from requested verdict",
        ));
    }
    Ok(ModelRecommendResponse {
        task_class: task_class.to_owned(),
        grounding: grounding.to_owned(),
        evidence_count: total,
        recommended_model,
        candidates,
        excluded_legacy_attempts: excluded_legacy,
        decision_id,
        decision_row_key,
        decision_row_sha256: hex_sha256(&row),
        operator_override,
    })
}

fn wilson_interval(success: u64, total: u64) -> (f64, f64) {
    if total == 0 {
        return (0.0, 1.0);
    }
    let n = total as f64;
    let p = success as f64 / n;
    let z = 1.959_963_984_540_054_f64;
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let half = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / denominator;
    ((center - half).max(0.0), (center + half).min(1.0))
}

fn indicator_outcome_mi(ms: u64, mf: u64, total_s: u64, total_f: u64) -> f64 {
    let other_s = total_s.saturating_sub(ms);
    let other_f = total_f.saturating_sub(mf);
    let total = total_s + total_f;
    if total == 0 {
        return 0.0;
    }
    let cells = [
        (ms, ms + mf, total_s),
        (mf, ms + mf, total_f),
        (other_s, other_s + other_f, total_s),
        (other_f, other_s + other_f, total_f),
    ];
    cells
        .into_iter()
        .filter(|(cell, row, col)| *cell > 0 && *row > 0 && *col > 0)
        .map(|(cell, row, col)| {
            let p = cell as f64 / total as f64;
            p * ((cell as f64 * total as f64) / (row as f64 * col as f64)).log2()
        })
        .sum::<f64>()
        .max(0.0)
}

fn unix_time_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn hex_sha256(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            out
        })
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

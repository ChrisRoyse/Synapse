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
        ModelOperation, ModelParams, ModelRecommendResponse, ModelRecommendationEvidence,
        ModelResponse, ModelStatusResponse,
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
    }
}

#[derive(Default)]
struct CandidateCounts {
    success: u64,
    failure: u64,
    priced_total: u64,
    priced_count: u64,
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
                cell.success += 1
            } else {
                cell.failure += 1
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
                expected_cost_micro_usd: (cell.priced_count > 0)
                    .then_some(cell.priced_total / cell.priced_count),
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
    let recommended_model = candidates.first().map(|candidate| candidate.model.clone());
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

use std::collections::BTreeSet;

use serde_json::Value;

use rmcp::{RoleServer, model::Tool, service::RequestContext};

use crate::server::{
    ErrorData, Json, Parameters, SynapseService, tool_profiles::ToolProfileSnapshot,
};

use super::{
    TELEMETRY_SOT, TELEMETRY_TOOL,
    errors::facade_delegate_error,
    types::{
        AgentEventIngressStats, TelemetryParams, TelemetryResponse, TelemetryStatusResponse,
        ToolSurfacePayloadContributor, ToolSurfacePayloadTelemetry, ToolSurfaceTelemetry,
    },
    validation::validate_telemetry_params,
};
pub(super) async fn handle(
    service: &SynapseService,
    params: Parameters<TelemetryParams>,
    request_context: RequestContext<RoleServer>,
) -> Result<Json<TelemetryResponse>, ErrorData> {
    validate_telemetry_params(&params.0)?;
    let operation = params.0.operation;
    tracing::info!(
        code = "MCP_TOOL_INVOCATION",
        kind = TELEMETRY_TOOL,
        operation = operation.as_str(),
        "tool.invocation kind=telemetry"
    );
    let session_id = crate::server::context::mcp_session_id_from_request_context(&request_context)?;
    let snapshot = service
        .tool_profile_snapshot(session_id.as_deref())
        .map_err(|error| {
            facade_delegate_error(
                TELEMETRY_TOOL,
                operation.as_str(),
                "tool_profile_snapshot",
                TELEMETRY_SOT,
                error,
                "inspect CF_SESSIONS tool profile rows and retry telemetry status",
            )
        })?;
    let storage_summary = service.storage_summary_snapshot().map_err(|error| {
        facade_delegate_error(
            TELEMETRY_TOOL,
            operation.as_str(),
            "storage_summary",
            TELEMETRY_SOT,
            error,
            "repair storage initialization and retry telemetry status",
        )
    })?;
    let ingress = agent_ingress_stats();
    let cf_row_counts = storage_summary.cf_row_counts.clone();
    let visible_tools = service
        .tools_for_session_profile(session_id.as_deref())
        .map_err(|error| {
            facade_delegate_error(
                TELEMETRY_TOOL,
                operation.as_str(),
                "tools_for_session_profile",
                TELEMETRY_SOT,
                error,
                "repair tool profile policy or schema sanitization before reading telemetry status",
            )
        })?;
    let status = TelemetryStatusResponse {
        source_of_truth: TELEMETRY_SOT,
        metrics_recorder: metrics_recorder_telemetry(),
        tool_surface: tool_surface_telemetry(&snapshot, &visible_tools),
        tool_usage: crate::daemon_lifecycle::recent_tool_usage(10_000, 128),
        storage_summary,
        agent_event_ingress: ingress,
        cf_row_counts,
    };
    Ok(Json(TelemetryResponse {
        operation,
        source_of_truth: TELEMETRY_SOT.to_owned(),
        readback_source_of_truth:
            "tool profile snapshot + RocksDB storage summary + ingress counters".to_owned(),
        status,
    }))
}

fn agent_ingress_stats() -> AgentEventIngressStats {
    let value = crate::server::agent_event_ingress::ingress_stats();
    AgentEventIngressStats {
        accepted_total: stat_u64(&value, "accepted_total"),
        rejected_unknown_spawn_total: stat_u64(&value, "rejected_unknown_spawn_total"),
        rejected_malformed_total: stat_u64(&value, "rejected_malformed_total"),
        rejected_storage_total: stat_u64(&value, "rejected_storage_total"),
    }
}

fn metrics_recorder_telemetry() -> super::types::MetricsRecorderTelemetry {
    let rendered = synapse_telemetry::metrics::render_prometheus();
    let recorded_metric_names = rendered
        .as_deref()
        .map(recorded_metric_names)
        .unwrap_or_default();
    let recorded_metric_samples = rendered
        .as_deref()
        .map(recorded_metric_samples)
        .unwrap_or_default();
    super::types::MetricsRecorderTelemetry {
        source_of_truth: synapse_telemetry::metrics::PROMETHEUS_RECORDER_SOURCE_OF_TRUTH,
        installed: synapse_telemetry::metrics::prometheus_recorder_installed(),
        recorder: "prometheus".to_owned(),
        registry_metric_count: synapse_telemetry::metrics::m3_metric_specs().len(),
        rendered_bytes: rendered.as_ref().map_or(0, String::len),
        recorded_metric_names,
        recorded_metric_samples,
    }
}

fn recorded_metric_names(rendered: &str) -> Vec<String> {
    rendered
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            line.split(['{', ' '])
                .next()
                .filter(|name| !name.is_empty())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(64)
        .map(ToOwned::to_owned)
        .collect()
}

fn recorded_metric_samples(rendered: &str) -> Vec<String> {
    let mut samples = rendered
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty() && !trimmed.starts_with('#')).then(|| trimmed.to_owned())
        })
        .collect::<Vec<_>>();
    samples.sort();
    samples.into_iter().take(256).collect()
}

const TOOL_PAYLOAD_SOURCE_OF_TRUTH: &str =
    "live sanitized tools_for_session_profile mapped to local_agent OpenAI tools[] wrapper";

fn tool_surface_telemetry(
    snapshot: &ToolProfileSnapshot,
    visible_tools: &[Tool],
) -> ToolSurfaceTelemetry {
    let visible_public_count = count_visible_public_tools(
        &snapshot.public_tool_registry.public_tool_names,
        &snapshot.visible_tool_names,
    );
    ToolSurfaceTelemetry {
        source_of_truth: snapshot.source_of_truth,
        profile: snapshot.profile.as_str().to_owned(),
        profile_label: snapshot.profile_label.to_owned(),
        profile_source: snapshot.source.clone(),
        visible_tool_count: snapshot.visible_tool_count,
        visible_public_tool_count: visible_public_count,
        implementation_tool_count: snapshot.implementation_tool_count,
        hidden_implementation_tool_count: snapshot
            .implementation_tool_count
            .saturating_sub(snapshot.visible_tool_count),
        public_tool_count: snapshot.public_tool_registry.public_tool_count,
        max_public_tool_count: snapshot.public_tool_registry.max_public_tool_count,
        over_public_tool_limit_by: snapshot.public_tool_registry.over_limit_by,
        profile_gated_public_tool_count: snapshot
            .public_tool_registry
            .public_tool_count
            .saturating_sub(visible_public_count),
        registered_public_tool_count: snapshot.public_tool_registry.registered_tools_present.len(),
        missing_public_tool_count: snapshot.public_tool_registry.registered_tools_missing.len(),
        denied_break_glass_tool_count: snapshot.denied_break_glass_tools.len(),
        hidden_tool_route_count: snapshot.hidden_tool_routes.len(),
        last_tool_surface_sha256: snapshot.visible_tool_sha256.clone(),
        visible_tool_sha256: snapshot.visible_tool_sha256.clone(),
        public_tool_sha256: snapshot.public_tool_registry.public_tool_sha256.clone(),
        facade_contract_sha256: snapshot.facade_contract.facade_contract_sha256.clone(),
        facade_contract_tool_count: snapshot.facade_contract.contract_tool_count,
        facade_contract_operation_count: snapshot.facade_contract.operation_count,
        facade_contract_mutating_operation_count: snapshot.facade_contract.mutating_operation_count,
        model_payload: tool_surface_payload_telemetry(visible_tools),
        codex_client_surface: snapshot.codex_client_surface.clone(),
    }
}

fn tool_surface_payload_telemetry(tools: &[Tool]) -> ToolSurfacePayloadTelemetry {
    let mut openai_tools = Vec::with_capacity(tools.len());
    let mut input_schema_bytes = 0_usize;
    let mut output_schema_bytes = 0_usize;
    let mut contributors = Vec::with_capacity(tools.len());
    for tool in tools {
        let description = tool
            .description
            .as_ref()
            .map(|desc| desc.as_ref())
            .unwrap_or("Synapse MCP tool");
        let input_schema = serde_json::Value::Object((*tool.input_schema).clone());
        let input_schema_json =
            serde_json::to_string(&input_schema).unwrap_or_else(|_| "{}".to_owned());
        input_schema_bytes = input_schema_bytes.saturating_add(input_schema_json.len());
        if let Some(output_schema) = &tool.output_schema {
            let output_schema_json =
                serde_json::to_string(&serde_json::Value::Object((**output_schema).clone()))
                    .unwrap_or_else(|_| "{}".to_owned());
            output_schema_bytes = output_schema_bytes.saturating_add(output_schema_json.len());
        }
        let openai_tool = serde_json::json!({
            "type": "function",
            "function": {
                "name": tool.name.as_ref(),
                "description": description,
                "parameters": input_schema,
            }
        });
        let openai_tool_json =
            serde_json::to_string(&openai_tool).unwrap_or_else(|_| "{}".to_owned());
        contributors.push(ToolSurfacePayloadContributor {
            name: tool.name.to_string(),
            openai_tool_bytes: openai_tool_json.len(),
            input_schema_bytes: input_schema_json.len(),
            description_bytes: description.len(),
        });
        openai_tools.push(openai_tool);
    }
    contributors.sort_by(|left, right| {
        right
            .openai_tool_bytes
            .cmp(&left.openai_tool_bytes)
            .then(left.name.cmp(&right.name))
    });
    contributors.truncate(10);
    let openai_tools_json = serde_json::to_string(&openai_tools).unwrap_or_default();
    let openai_tools_chars = openai_tools_json.chars().count();
    ToolSurfacePayloadTelemetry {
        source_of_truth: TOOL_PAYLOAD_SOURCE_OF_TRUTH,
        tool_count: tools.len(),
        openai_tools_bytes: openai_tools_json.len(),
        openai_tools_chars,
        approx_tokens_chars_div_4: (openai_tools_chars as f64 / 4.0).ceil() as u64,
        approx_tokens_chars_div_3_5: (openai_tools_chars as f64 / 3.5).ceil() as u64,
        input_schema_bytes,
        output_schema_bytes,
        budget_openai_tools_bytes:
            crate::server::tool_profiles::PUBLIC_TOOL_OPENAI_PAYLOAD_BUDGET_BYTES,
        over_budget_by_bytes: openai_tools_json
            .len()
            .saturating_sub(crate::server::tool_profiles::PUBLIC_TOOL_OPENAI_PAYLOAD_BUDGET_BYTES),
        top_contributors: contributors,
    }
}

fn count_visible_public_tools(
    public_tool_names: &[String],
    visible_tool_names: &[String],
) -> usize {
    visible_tool_names
        .iter()
        .filter(|visible| public_tool_names.iter().any(|public| public == *visible))
        .count()
}

fn stat_u64(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or_default()
}

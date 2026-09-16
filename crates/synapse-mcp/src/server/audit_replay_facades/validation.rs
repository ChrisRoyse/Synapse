use rmcp::model::ErrorCode;
use serde_json::json;
use synapse_core::error_codes;

use crate::server::ErrorData;

use super::{
    AUDIT_SOT, AUDIT_TOOL, MAX_ARTIFACT_BYTES, MAX_ARTIFACT_RECORDS, MAX_LIFECYCLE_LIMIT,
    MAX_LINE_BYTES, REPLAY_SOT, REPLAY_TOOL,
    errors::params_error,
    types::{
        AuditLifecycleTailParams, AuditOperation, AuditParams, ReplayArtifactInspectParams,
        ReplayOperation, ReplayParams,
    },
};
pub(super) fn validate_audit_params(params: &AuditParams) -> Result<AuditOperation, ErrorData> {
    let operation = AuditOperation::parse(params.operation.as_str())?;
    validate_exact_spec(
        AUDIT_TOOL,
        operation.as_str(),
        &[
            ("command_query", params.command_query.is_some()),
            ("lifecycle_events", params.lifecycle_events.is_some()),
            ("lifecycle_exits", params.lifecycle_exits.is_some()),
            (
                "profile_intelligence",
                params.profile_intelligence.is_some(),
            ),
            ("export_bundle", params.export_bundle.is_some()),
        ],
        AUDIT_SOT,
    )?;
    Ok(operation)
}

pub(super) fn validate_replay_params(params: &ReplayParams) -> Result<ReplayOperation, ErrorData> {
    let operation = ReplayOperation::parse(params.operation.as_str())?;
    validate_exact_spec(
        REPLAY_TOOL,
        operation.as_str(),
        &[
            ("record", params.record.is_some()),
            ("demo_status", params.demo_status.is_some()),
            ("demo_start", params.demo_start.is_some()),
            ("demo_stop", params.demo_stop.is_some()),
            ("artifact_inspect", params.artifact_inspect.is_some()),
        ],
        REPLAY_SOT,
    )?;
    Ok(operation)
}

fn validate_exact_spec(
    tool: &'static str,
    operation: &'static str,
    specs: &[(&'static str, bool)],
    source_of_truth: &'static str,
) -> Result<(), ErrorData> {
    let matching_present = specs
        .iter()
        .any(|(name, present)| *name == operation && *present);
    let extra = specs
        .iter()
        .filter_map(|(name, present)| (*present && *name != operation).then_some(*name))
        .collect::<Vec<_>>();
    if matching_present && extra.is_empty() {
        return Ok(());
    }
    Err(ErrorData::new(
        ErrorCode(-32602),
        format!("{tool} operation={operation} requires exactly one matching operation payload"),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_of_truth": source_of_truth,
            "source_id": operation,
            "matching_payload_present": matching_present,
            "extra_payloads": extra,
            "remediation": "pass exactly one payload object whose key matches operation",
        })),
    ))
}

pub(super) fn validate_lifecycle_params(
    params: &AuditLifecycleTailParams,
) -> Result<(), ErrorData> {
    if params.limit == 0 || params.limit > MAX_LIFECYCLE_LIMIT {
        return Err(params_error(
            AUDIT_TOOL,
            "lifecycle_tail",
            "limit",
            AUDIT_SOT,
            format!("limit must be 1..={MAX_LIFECYCLE_LIMIT}"),
        ));
    }
    if params.max_line_bytes == 0 || params.max_line_bytes > MAX_LINE_BYTES {
        return Err(params_error(
            AUDIT_TOOL,
            "lifecycle_tail",
            "max_line_bytes",
            AUDIT_SOT,
            format!("max_line_bytes must be 1..={MAX_LINE_BYTES}"),
        ));
    }
    Ok(())
}

pub(super) fn validate_replay_artifact_params(
    params: &ReplayArtifactInspectParams,
) -> Result<(), ErrorData> {
    if params.path.trim().is_empty() {
        return Err(params_error(
            REPLAY_TOOL,
            "artifact_inspect",
            "path",
            REPLAY_SOT,
            "path must not be empty",
        ));
    }
    if params.max_bytes == 0 || params.max_bytes > MAX_ARTIFACT_BYTES {
        return Err(params_error(
            REPLAY_TOOL,
            "artifact_inspect",
            "max_bytes",
            REPLAY_SOT,
            format!("max_bytes must be 1..={MAX_ARTIFACT_BYTES}"),
        ));
    }
    if params.max_records > MAX_ARTIFACT_RECORDS {
        return Err(params_error(
            REPLAY_TOOL,
            "artifact_inspect",
            "max_records",
            REPLAY_SOT,
            format!("max_records must be 0..={MAX_ARTIFACT_RECORDS}"),
        ));
    }
    Ok(())
}

use rmcp::model::ErrorCode;
use serde_json::json;
use synapse_core::error_codes;

use crate::server::ErrorData;

use super::{
    HYGIENE_TOOL, MODEL_TOOL, SETUP_TOOL, STORAGE_TOOL,
    types::{
        HygieneParams, ModelParams, SetupOperation, SetupParams, StorageParams, TelemetryOperation,
        TelemetryParams,
    },
};
pub(super) fn validate_storage_params(params: &StorageParams) -> Result<(), ErrorData> {
    validate_exact_spec(
        STORAGE_TOOL,
        params.operation.as_str(),
        &[
            ("inspect", params.inspect.is_some()),
            ("summary", params.summary.is_some()),
            ("gc_once", params.gc_once.is_some()),
            ("anchors", params.anchors.is_some()),
            ("temporal_panels", params.temporal_panels.is_some()),
            ("corpus_histogram", params.corpus_histogram.is_some()),
            ("panel_coverage", params.panel_coverage.is_some()),
            ("temporal_rerank", params.temporal_rerank.is_some()),
            ("temporal_backfill", params.temporal_backfill.is_some()),
            ("search_rebuild", params.search_rebuild.is_some()),
            ("panel_lifecycle", params.panel_lifecycle.is_some()),
            ("find_similar", params.find_similar.is_some()),
            (
                "retire_orphan_slot_cfs",
                params.retire_orphan_slot_cfs.is_some(),
            ),
            (
                "retire_search_generation",
                params.retire_search_generation.is_some(),
            ),
            ("backup", params.backup.is_some()),
            ("backup_status", params.backup_status.is_some()),
            ("restore_verify", params.restore_verify.is_some()),
            ("intelligence", params.intelligence.is_some()),
        ],
    )
}

pub(super) fn validate_model_params(params: &ModelParams) -> Result<(), ErrorData> {
    validate_exact_spec(
        MODEL_TOOL,
        params.operation.as_str(),
        &[
            ("list", params.list.is_some()),
            ("status", params.status.is_some()),
            ("probe", params.probe.is_some()),
            ("register", params.register.is_some()),
            ("update", params.update.is_some()),
            ("remove", params.remove.is_some()),
        ],
    )
}

pub(super) fn validate_hygiene_params(params: &HygieneParams) -> Result<(), ErrorData> {
    validate_exact_spec(
        HYGIENE_TOOL,
        params.operation.as_str(),
        &[
            ("scan_text", params.scan_text.is_some()),
            ("scan_storage", params.scan_storage.is_some()),
            ("flags", params.flags.is_some()),
            ("report", params.report.is_some()),
            ("grounding_gap", params.grounding_gap.is_some()),
            ("blind_spot", params.blind_spot.is_some()),
            ("drift", params.drift.is_some()),
            ("vault_verify", params.vault_verify.is_some()),
            ("kernel", params.kernel.is_some()),
            ("kernel_rebuild", params.kernel_rebuild.is_some()),
            ("guard_calibrate", params.guard_calibrate.is_some()),
            ("guard_verify", params.guard_verify.is_some()),
        ],
    )
}

pub(super) fn validate_setup_params(params: &SetupParams) -> Result<(), ErrorData> {
    match params.operation {
        SetupOperation::Status
            if params.doctor.is_none()
                && params.repair.is_none()
                && params.host_transition.is_none() =>
        {
            return Ok(());
        }
        SetupOperation::Doctor
            if params.status.is_none()
                && params.repair.is_none()
                && params.host_transition.is_none() =>
        {
            return Ok(());
        }
        _ => {}
    }
    validate_exact_spec(
        SETUP_TOOL,
        params.operation.as_str(),
        &[
            ("status", params.status.is_some()),
            ("doctor", params.doctor.is_some()),
            ("repair", params.repair.is_some()),
            ("host_transition", params.host_transition.is_some()),
        ],
    )
}

pub(super) fn validate_telemetry_params(params: &TelemetryParams) -> Result<(), ErrorData> {
    match params.operation {
        TelemetryOperation::Status => {
            let _status_payload_present = params.status.is_some();
            Ok(())
        }
    }
}

fn validate_exact_spec(
    tool: &'static str,
    operation: &'static str,
    specs: &[(&'static str, bool)],
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
        ErrorCode(-32099),
        format!("{tool} operation={operation} requires exactly one matching operation payload"),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_of_truth": "MCP request parameters",
            "source_id": operation,
            "matching_payload_present": matching_present,
            "extra_payloads": extra,
            "remediation": "pass exactly one payload object whose key matches operation",
        })),
    ))
}

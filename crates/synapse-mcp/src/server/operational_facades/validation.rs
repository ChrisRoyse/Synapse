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
        ],
    )
}

pub(super) fn validate_setup_params(params: &SetupParams) -> Result<(), ErrorData> {
    match params.operation {
        SetupOperation::Status if params.doctor.is_none() && params.repair.is_none() => {
            return Ok(());
        }
        SetupOperation::Doctor if params.status.is_none() && params.repair.is_none() => {
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
#[cfg(test)]
mod tests {
    use crate::m3::storage::StorageInspectParams;
    use serde_json::json;

    use super::*;
    use crate::server::operational_facades::types::{
        SetupParams, SetupRepairParams, SetupStatusParams, StorageOperation, TelemetryOperation,
        TelemetryStatusParams,
    };

    #[test]
    fn validation_requires_matching_storage_payload_only() {
        let ok = StorageParams {
            operation: StorageOperation::Inspect,
            inspect: Some(StorageInspectParams::default()),
            summary: None,
            gc_once: None,
        };
        validate_storage_params(&ok).expect("matching payload accepted");

        let missing = StorageParams {
            operation: StorageOperation::Inspect,
            inspect: None,
            summary: None,
            gc_once: None,
        };
        validate_storage_params(&missing).expect_err("missing payload rejected");

        let extra = StorageParams {
            operation: StorageOperation::Inspect,
            inspect: Some(StorageInspectParams::default()),
            summary: Some(StorageInspectParams::default()),
            gc_once: None,
        };
        validate_storage_params(&extra).expect_err("extra payload rejected");
    }

    #[test]
    fn storage_facade_rejects_probe_row_operation_schema() {
        let error = serde_json::from_value::<StorageParams>(json!({
            "operation": "put_probe_rows",
            "put_probe_rows": {
                "cf_name": "CF_ACTION_LOG",
                "key_prefix": "issue1595",
                "rows": 1,
                "value_bytes": 8
            }
        }))
        .expect_err("production storage facade must not deserialize synthetic write operation");

        let message = error.to_string();
        assert!(
            message.contains("unknown variant `put_probe_rows`")
                || message.contains("unknown field `put_probe_rows`"),
            "unexpected serde error: {message}"
        );
    }

    #[test]
    fn telemetry_status_accepts_schema_valid_empty_payload() {
        let missing = TelemetryParams {
            operation: TelemetryOperation::Status,
            status: None,
        };
        validate_telemetry_params(&missing).expect("status payload is optional");

        let ok = TelemetryParams {
            operation: TelemetryOperation::Status,
            status: Some(TelemetryStatusParams::default()),
        };
        validate_telemetry_params(&ok).expect("status payload accepted");
    }

    #[test]
    fn setup_status_and_doctor_accept_operation_only() {
        let status = SetupParams {
            operation: SetupOperation::Status,
            status: None,
            doctor: None,
            repair: None,
        };
        validate_setup_params(&status).expect("setup status has no required fields");

        let doctor = SetupParams {
            operation: SetupOperation::Doctor,
            status: None,
            doctor: None,
            repair: None,
        };
        validate_setup_params(&doctor).expect("setup doctor has no required fields");

        let explicit_status = SetupParams {
            operation: SetupOperation::Status,
            status: Some(SetupStatusParams::default()),
            doctor: None,
            repair: None,
        };
        validate_setup_params(&explicit_status).expect("explicit empty status payload accepted");
    }

    #[test]
    fn setup_payloadless_status_keeps_mismatches_fail_closed() {
        let mismatched_extra = SetupParams {
            operation: SetupOperation::Status,
            status: None,
            doctor: Some(SetupStatusParams::default()),
            repair: None,
        };
        validate_setup_params(&mismatched_extra)
            .expect_err("mismatched setup payload still rejected");

        let repair_missing_payload = SetupParams {
            operation: SetupOperation::Repair,
            status: None,
            doctor: None,
            repair: None,
        };
        validate_setup_params(&repair_missing_payload)
            .expect_err("setup repair still requires its payload");

        let repair_ok = SetupParams {
            operation: SetupOperation::Repair,
            status: None,
            doctor: None,
            repair: Some(SetupRepairParams {
                reason: "issue1493 regression".to_owned(),
            }),
        };
        validate_setup_params(&repair_ok).expect("setup repair payload still accepted");
    }
}

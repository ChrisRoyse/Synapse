use rmcp::{RoleServer, service::RequestContext};

use crate::server::{ErrorData, Json, Parameters, SynapseService};

use super::{
    HYGIENE_SOT, HYGIENE_TOOL,
    errors::{facade_delegate_error, missing_spec},
    policy::require_maintenance_profile,
    response::hygiene_response,
    types::{HygieneOperation, HygieneParams, HygieneResponse},
    validation::validate_hygiene_params,
};
pub(super) async fn handle(
    service: &SynapseService,
    params: Parameters<HygieneParams>,
    request_context: RequestContext<RoleServer>,
) -> Result<Json<HygieneResponse>, ErrorData> {
    validate_hygiene_params(&params.0)?;
    let operation = params.0.operation;
    tracing::info!(
        code = "MCP_TOOL_INVOCATION",
        kind = HYGIENE_TOOL,
        operation = operation.as_str(),
        "tool.invocation kind=hygiene"
    );
    let runtime = service.reflex_runtime().map_err(|error| {
        facade_delegate_error(
            HYGIENE_TOOL,
            operation.as_str(),
            "reflex_runtime",
            HYGIENE_SOT,
            error,
            "repair storage/reflex initialization and retry the hygiene operation",
        )
    })?;
    match operation {
        HygieneOperation::ScanText => {
            let spec = params
                .0
                .scan_text
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "scan_text"))?;
            if spec.persist {
                require_maintenance_profile(
                    service,
                    &request_context,
                    HYGIENE_TOOL,
                    operation.as_str(),
                    spec.source_cf.as_deref().unwrap_or("source_cf_missing"),
                    HYGIENE_SOT,
                )?;
            }
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_scan_text(&spec),
            )?;
            let response =
                crate::m3::hygiene::scan_text_tool(&runtime, &spec).map_err(|error| {
                    facade_delegate_error(
                        HYGIENE_TOOL,
                        operation.as_str(),
                        spec.source_key_hex.as_deref().unwrap_or("text_only"),
                        HYGIENE_SOT,
                        error,
                        "fix text/source row identity and inspect hygiene flags before retrying",
                    )
                })?;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "matches={} flags_written={}",
                    response.matches.len(),
                    response.flags_written
                ),
                |out| out.scan_text = Some(response),
            )))
        }
        HygieneOperation::ScanStorage => {
            let spec = params
                .0
                .scan_storage
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "scan_storage"))?;
            require_maintenance_profile(
                service,
                &request_context,
                HYGIENE_TOOL,
                operation.as_str(),
                "storage_scan",
                HYGIENE_SOT,
            )?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_scan_storage(&spec),
            )?;
            let response = crate::m3::hygiene::scan_storage(&runtime, &spec).map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "storage_scan",
                    HYGIENE_SOT,
                    error,
                    "fix source_cfs/cursor and inspect CF_KV hygiene flag rows",
                )
            })?;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "scanned_rows={} flags_written={}",
                    response.scanned_rows, response.flags_written
                ),
                |out| out.scan_storage = Some(response),
            )))
        }
        HygieneOperation::Flags => {
            let spec = params.0.flags.unwrap_or_default();
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_flags(&spec),
            )?;
            let response = crate::m3::hygiene::query_flags(&runtime, &spec).map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    spec.source_key_hex.as_deref().unwrap_or("flag_prefix"),
                    HYGIENE_SOT,
                    error,
                    "inspect CF_KV hygiene/flag/v1 rows and cursor format",
                )
            })?;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "flags={} scanned_rows={}",
                    response.flags.len(),
                    response.scanned_rows
                ),
                |out| out.flags = Some(response),
            )))
        }
        HygieneOperation::Report => {
            let spec = params.0.report.unwrap_or_default();
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_report(&spec),
            )?;
            let response = crate::m3::hygiene::report(&runtime, &spec).map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    spec.source_key_hex.as_deref().unwrap_or("report"),
                    HYGIENE_SOT,
                    error,
                    "inspect hygiene report joins and CF_KV flag/taint rows",
                )
            })?;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "flags_total={} impacted_routines={}",
                    response.summary.flags_total, response.summary.impacted_routine_count
                ),
                |out| out.report = Some(response),
            )))
        }
        HygieneOperation::GroundingGap => {
            let spec = params
                .0
                .grounding_gap
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "grounding_gap"))?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_grounding_gap(&spec),
            )?;
            // Grounding-gap is a read-only scan of the whole Base panel: blocking
            // CPU/IO work that must not occupy a runtime worker serving MCP.
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_lodestar",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the hygiene grounding_gap operation",
                )
            })?;
            let source_id = format!("panel_{}", spec.panel_version);
            let response = tokio::task::spawn_blocking(move || {
                crate::m3::hygiene::run_grounding_gap(&db, &spec)
            })
            .await
            .map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    &source_id,
                    HYGIENE_SOT,
                    crate::m1::mcp_error(
                        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                        format!("grounding_gap blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the grounding-gap task terminated abnormally",
                )
            })??;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "grounded_fraction={:.4} provisional={} ungrounded_records={} base_cf_rows={}",
                    response.grounded_fraction,
                    response.provisional,
                    response.ungrounded_records,
                    response.base_cf_rows
                ),
                |out| out.grounding_gap = Some(response),
            )))
        }
        HygieneOperation::BlindSpot => {
            let spec = params
                .0
                .blind_spot
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "blind_spot"))?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_blind_spot(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_loom",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the hygiene blind_spot operation",
                )
            })?;
            let source_id = format!("panel_{}", spec.panel_version);
            let response =
                tokio::task::spawn_blocking(move || crate::m3::hygiene::run_blind_spot(&db, &spec))
                    .await
                    .map_err(|error| {
                        facade_delegate_error(
                            HYGIENE_TOOL,
                            operation.as_str(),
                            &source_id,
                            HYGIENE_SOT,
                            crate::m1::mcp_error(
                                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                                format!("blind_spot blocking task failed to join: {error}"),
                            ),
                            "inspect daemon logs; the blind-spot task terminated abnormally",
                        )
                    })??;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "alerts_total={} slot_pairs_evaluated={} n_lenses={}",
                    response.alerts_total, response.slot_pairs_evaluated, response.n_lenses
                ),
                |out| out.blind_spot = Some(response),
            )))
        }
        HygieneOperation::Drift => {
            let spec = params
                .0
                .drift
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "drift"))?;
            // Drift persists findings to the native Reactive CF: maintenance-gated
            // exactly like the other mutating hygiene operations.
            require_maintenance_profile(
                service,
                &request_context,
                HYGIENE_TOOL,
                operation.as_str(),
                &format!("panel_{}", spec.panel_version),
                HYGIENE_SOT,
            )?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_drift(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_assay",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the hygiene drift operation",
                )
            })?;
            let source_id = format!("panel_{}", spec.panel_version);
            let response =
                tokio::task::spawn_blocking(move || crate::m3::hygiene::run_drift(&db, &spec))
                    .await
                    .map_err(|error| {
                        facade_delegate_error(
                            HYGIENE_TOOL,
                            operation.as_str(),
                            &source_id,
                            HYGIENE_SOT,
                            crate::m1::mcp_error(
                                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                                format!("drift blocking task failed to join: {error}"),
                            ),
                            "inspect daemon logs; the MMD drift task terminated abnormally",
                        )
                    })??;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "drifted_lenses={} drift_rows_persisted={} reactive_cf_rows_after={}",
                    response.drifted_lenses,
                    response.drift_rows_persisted,
                    response.reactive_cf_rows_after
                ),
                |out| out.drift = Some(response),
            )))
        }
        HygieneOperation::VaultVerify => {
            let spec = params
                .0
                .vault_verify
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "vault_verify"))?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_vault_verify(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_vault",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the hygiene vault_verify operation",
                )
            })?;
            // The verification re-derives SST/WAL bytes and re-hashes a ledger
            // window: strictly blocking CPU/IO work, exactly like the sibling
            // backup and restore-verify operations, so it must not occupy a
            // Tokio runtime worker serving MCP. Mutual exclusion with backup and
            // erase is enforced one layer down by the vault maintenance guard
            // those passes already share, so a scan can never race a tree
            // rewrite and report the torn intermediate state as corruption.
            let response = tokio::task::spawn_blocking(move || {
                crate::m3::hygiene::run_vault_verify(&db, &spec)
            })
            .await
            .map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_vault",
                    HYGIENE_SOT,
                    crate::m1::mcp_error(
                        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                        format!("vault_verify blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the vault verification task terminated abnormally",
                )
            })??;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "vault_verify green={} scan_mode={} verified=[{}..{}) head_height={} restore_success={} chain_intact={} raw_commitments_intact={} lineage_present={} tip={}",
                    response.green,
                    response.scan_mode,
                    response.verified_from_seq,
                    response.verified_to_seq,
                    response.ledger_head_height,
                    response.restore_success,
                    response.chain_intact,
                    response.raw_commitments_intact,
                    response.lineage_present,
                    response.ledger_tip_hash,
                ),
                |out| out.vault_verify = Some(response),
            )))
        }
    }
}

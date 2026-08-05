use rmcp::{RoleServer, service::RequestContext};

use crate::server::{ErrorData, Json, Parameters, SynapseService};

const VAULT_VERIFY_INTERVAL_ENV: &str = "SYNAPSE_VAULT_VERIFY_INTERVAL_SECS";
const VAULT_VERIFY_STARTUP_DELAY_ENV: &str = "SYNAPSE_VAULT_VERIFY_STARTUP_DELAY_SECS";
const DEFAULT_VAULT_VERIFY_INTERVAL_SECS: u64 = 24 * 60 * 60;
const DEFAULT_VAULT_VERIFY_STARTUP_DELAY_SECS: u64 = 5 * 60;

use super::{
    HYGIENE_SOT, HYGIENE_TOOL,
    errors::{facade_delegate_error, missing_spec},
    policy::require_maintenance_profile,
    response::hygiene_response,
    types::{HygieneOperation, HygieneParams, HygieneResponse},
    validation::validate_hygiene_params,
};

pub(crate) fn spawn_periodic_vault_verifier(
    service: SynapseService,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let interval_secs = strict_seconds_env(
        VAULT_VERIFY_INTERVAL_ENV,
        DEFAULT_VAULT_VERIFY_INTERVAL_SECS,
    )?;
    let startup_delay_secs = strict_seconds_env(
        VAULT_VERIFY_STARTUP_DELAY_ENV,
        DEFAULT_VAULT_VERIFY_STARTUP_DELAY_SECS,
    )?;
    if interval_secs == 0 {
        tracing::warn!(
            code = "VAULT_VERIFY_PERIODIC_DISABLED",
            env = VAULT_VERIFY_INTERVAL_ENV,
            "periodic physical vault verification explicitly disabled"
        );
        return Ok(None);
    }
    tracing::info!(
        code = "VAULT_VERIFY_PERIODIC_SCHEDULED",
        interval_secs,
        startup_delay_secs,
        "periodic physical vault verification scheduled"
    );
    Ok(Some(tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(startup_delay_secs);
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!(
                        code = "VAULT_VERIFY_PERIODIC_STOPPED",
                        "periodic physical vault verification stopped by daemon shutdown"
                    );
                    return;
                }
                () = tokio::time::sleep(delay) => {}
            }
            run_periodic_vault_verify_once(&service).await;
            delay = std::time::Duration::from_secs(interval_secs);
        }
    })))
}

fn strict_seconds_env(name: &'static str, default: u64) -> anyhow::Result<u64> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(default);
    };
    let raw = raw.into_string().map_err(|_| {
        anyhow::anyhow!(
            "{name} must be valid Unicode containing an unsigned integer number of seconds"
        )
    })?;
    raw.parse::<u64>().map_err(|error| {
        anyhow::anyhow!(
            "{name}={raw:?} is invalid: expected an unsigned integer number of seconds: {error}"
        )
    })
}

async fn run_periodic_vault_verify_once(service: &SynapseService) {
    let db = match service.m3_storage() {
        Ok(db) => db,
        Err(error) => {
            tracing::error!(
                code = "VAULT_VERIFY_PERIODIC_STORAGE_UNAVAILABLE",
                error_code = ?error.code,
                error = %error.message,
                remediation = "repair storage/Calyx initialization; scheduled verification cannot inspect the vault",
                "scheduled physical vault verification could not open its Source of Truth"
            );
            return;
        }
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::m3::hygiene::run_vault_verify(
            &db,
            &crate::m3::hygiene::HygieneVaultVerifyParams::default(),
        )
    })
    .await;
    match result {
        Ok(Ok(report)) => tracing::info!(
            code = "VAULT_VERIFY_PERIODIC_OK",
            vault_id = %report.vault_id,
            verified_from_seq = report.verified_from_seq,
            verified_to_seq = report.verified_to_seq,
            ledger_head_height = report.ledger_head_height,
            ledger_tip_hash = %report.ledger_tip_hash,
            raw_commitments_intact = report.raw_commitments_intact,
            "scheduled physical vault verification completed"
        ),
        Ok(Err(error)) => tracing::error!(
            code = "VAULT_VERIFY_PERIODIC_FAILED",
            error_code = ?error.code,
            error = %error.message,
            remediation = "stop writers, preserve the vault and lineage journal, and restore from a verified backup before trusting vault reads",
            "scheduled physical vault verification raised an integrity alarm"
        ),
        Err(error) => tracing::error!(
            code = "VAULT_VERIFY_PERIODIC_TASK_FAILED",
            error = %error,
            remediation = "inspect daemon logs and process health; the blocking verification task terminated abnormally",
            "scheduled physical vault verification task failed"
        ),
    }
}

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
    let reflex_runtime = || {
        service.reflex_runtime().map_err(|error| {
            facade_delegate_error(
                HYGIENE_TOOL,
                operation.as_str(),
                "reflex_runtime",
                HYGIENE_SOT,
                error,
                "repair storage/reflex initialization and retry the hygiene operation",
            )
        })
    };
    match operation {
        HygieneOperation::ScanText => {
            let runtime = reflex_runtime()?;
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
            let runtime = reflex_runtime()?;
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
            let runtime = reflex_runtime()?;
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
            let runtime = reflex_runtime()?;
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
            let mut response =
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
            let event_bus = service.sse_state()?.event_bus();
            for finding in &response.persisted_findings {
                let event_seq = finding
                    .observed_seq
                    .checked_mul(u64::from(u16::MAX) + 1)
                    .and_then(|base| base.checked_add(u64::from(finding.slot)))
                    .ok_or_else(|| {
                        crate::m1::mcp_error(
                            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                            format!(
                                "reactive drift event sequence overflow for observed_seq={} slot={}; remediation=preserve the Reactive CF row and inspect vault sequence exhaustion",
                                finding.observed_seq, finding.slot
                            ),
                        )
                    })?;
                let report = event_bus.publish(synapse_core::types::Event {
                    seq: event_seq,
                    at: chrono::Utc::now(),
                    source: synapse_core::types::EventSource::System,
                    kind: "calyx.reactive.drift".to_owned(),
                    data: serde_json::to_value(finding).map_err(|error| {
                        crate::m1::mcp_error(
                            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                            format!(
                                "serialize committed Reactive CF drift finding for delivery: {error}; remediation=inspect the typed finding schema and preserve the Reactive CF row"
                            ),
                        )
                    })?,
                    correlations: Vec::new(),
                });
                response.notifications_matched = response
                    .notifications_matched
                    .saturating_add(report.matched as u64);
                response.notifications_queued = response
                    .notifications_queued
                    .saturating_add(report.queued as u64);
                response.notifications_dropped = response
                    .notifications_dropped
                    .saturating_add(report.dropped);
            }
            if response.notifications_dropped > 0 {
                return Err(crate::m1::mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "reactive drift delivery dropped {} subscription notification(s) after persisting {} Reactive CF trigger row(s); remediation=consume or recreate the saturated subscription, then read the durable Reactive CF findings before retrying delivery",
                        response.notifications_dropped, response.drift_rows_persisted
                    ),
                ));
            }
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "drifted_lenses={} drift_rows_persisted={} reactive_cf_rows_after={} notifications_matched={} notifications_queued={}",
                    response.drifted_lenses,
                    response.drift_rows_persisted,
                    response.reactive_cf_rows_after,
                    response.notifications_matched,
                    response.notifications_queued,
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
        HygieneOperation::Kernel => {
            let spec = params
                .0
                .kernel
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "kernel"))?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_kernel(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_lodestar",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the hygiene kernel operation",
                )
            })?;
            let source_id = format!("panel_{}", spec.panel_version);
            // Health READS the persisted Kernel artifact (a CF read plus a JSON
            // decode of the full kernel), so it is blocking IO and must not
            // occupy a runtime worker serving MCP.
            let response =
                tokio::task::spawn_blocking(move || crate::m3::hygiene::run_kernel(&db, &spec))
                    .await
                    .map_err(|error| {
                        facade_delegate_error(
                            HYGIENE_TOOL,
                            operation.as_str(),
                            &source_id,
                            HYGIENE_SOT,
                            crate::m1::mcp_error(
                                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                                format!("kernel health blocking task failed to join: {error}"),
                            ),
                            "inspect daemon logs; the kernel-health task terminated abnormally",
                        )
                    })??;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "kernel_id={} trust={} size={} recall_ratio={:.4} min_recall_ratio={:.4} pass_mode={} grounded_fraction={:.4} artifact_bytes={}",
                    response.kernel_id,
                    response.trust,
                    response.size,
                    response.recall_ratio,
                    response.min_recall_ratio,
                    response.recall_pass_mode,
                    response.grounded_fraction,
                    response.artifact_bytes
                ),
                |out| out.kernel = Some(response),
            )))
        }
        HygieneOperation::KernelRebuild => {
            let spec = params
                .0
                .kernel_rebuild
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "kernel_rebuild"))?;
            // The rebuild persists Kernel artifacts: maintenance-gated exactly
            // like the other mutating hygiene operations. This gate is also what
            // keeps kernel selection off any latency-critical caller — the pass
            // is minutes of MFVS + recall measurement per domain and the Calyx
            // layer asserts a cold context (#1686) underneath it.
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
                &crate::m3::hygiene::required_permissions_kernel_rebuild(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_lodestar",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the hygiene kernel_rebuild operation",
                )
            })?;
            let source_id = format!("panel_{}", spec.panel_version);
            let response = tokio::task::spawn_blocking(move || {
                crate::m3::hygiene::run_kernel_rebuild(&db, &spec)
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
                        format!("kernel rebuild blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the kernel-rebuild task terminated abnormally",
                )
            })??;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "domains_discovered={} domains_built={} domains_refused={} all_domains_grounded={} artifacts_persisted={} kernel_cf_rows_after={}",
                    response.domains_discovered,
                    response.domains_built,
                    response.domains_refused,
                    response.all_domains_grounded,
                    response.artifacts_persisted,
                    response.kernel_cf_rows_after
                ),
                |out| out.kernel_rebuild = Some(response),
            )))
        }
        HygieneOperation::GuardCalibrate => {
            let spec = params
                .0
                .guard_calibrate
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "guard_calibrate"))?;
            if spec.persist.unwrap_or(true) {
                // A persisted profile changes what every profile-backed guarded
                // search will admit from then on: maintenance-gated.
                require_maintenance_profile(
                    service,
                    &request_context,
                    HYGIENE_TOOL,
                    operation.as_str(),
                    &format!("panel_{}", spec.panel_version),
                    HYGIENE_SOT,
                )?;
            }
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_guard_calibrate(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_ward",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the hygiene guard_calibrate operation",
                )
            })?;
            let source_id = format!("panel_{}", spec.panel_version);
            let response = tokio::task::spawn_blocking(move || {
                crate::m3::hygiene::run_guard_calibrate(&db, &spec)
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
                        format!("guard calibration blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the guard-calibration task terminated abnormally",
                )
            })??;
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "guard_id={} adjudicated_good={} adjudicated_bad={} unadjudicated={} slots={} persisted={} readback_calibrated={} guard_cf_profile_bytes={}",
                    response.guard_id,
                    response.adjudicated_good,
                    response.adjudicated_bad,
                    response.unadjudicated,
                    response.slots.len(),
                    response.persisted,
                    response.readback_calibrated,
                    response.guard_cf_profile_bytes
                ),
                |out| out.guard_calibrate = Some(response),
            )))
        }
        HygieneOperation::GuardVerify => {
            let spec = params
                .0
                .guard_verify
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "guard_verify"))?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_guard_verify(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_ward",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the hygiene guard_verify operation",
                )
            })?;
            let source_id = format!("panel_{}", spec.panel_version);
            let verify_db = std::sync::Arc::clone(&db);
            let mut response = tokio::task::spawn_blocking(move || {
                crate::m3::hygiene::run_guard_verify(&verify_db, &spec)
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
                        format!("guard verification blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the guard-verification task terminated abnormally",
                )
            })??;
            if response.persisted_novelty.is_some() {
                let relay = synapse_storage::derived_state::run_novelty_relay_once(&db).map_err(
                    |detail| {
                        crate::m1::mcp_error(
                            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                            format!("Ward novelty relay failed: {detail}"),
                        )
                    },
                )?;
                response.notifications_matched = relay.last_novelty_notifications_matched;
                response.notifications_queued = relay.last_novelty_notifications_queued;
                response.notifications_dropped = relay.last_novelty_notifications_dropped;
            }
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "guard_id={} overall_pass={} provisional={} policy={} required_slots={} failing_slots={} trusted_exemplars={} reactive_persisted={} notifications_matched={} notifications_queued={}",
                    response.guard_id,
                    response.overall_pass,
                    response.provisional,
                    response.policy,
                    response.required_slots.len(),
                    response.failing_slots.len(),
                    response.trusted_exemplars,
                    response.persisted_novelty.is_some(),
                    response.notifications_matched,
                    response.notifications_queued,
                ),
                |out| out.guard_verify = Some(response),
            )))
        }
        HygieneOperation::AnnealStatus => {
            let _spec = params
                .0
                .anneal_status
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "anneal_status"))?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_anneal_status(),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_anneal",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry hygiene anneal_status",
                )
            })?;
            let status = db.calyx_vault_status().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_anneal",
                    HYGIENE_SOT,
                    crate::m1::mcp_error(
                        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                        format!("read Calyx vault status for Anneal: {error}"),
                    ),
                    "inspect the structured Calyx vault error and repair the native Anneal state",
                )
            })?;
            let anneal = status.anneal.ok_or_else(|| {
                crate::m1::mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "Calyx vault status has no native Anneal readback",
                )
            })?;
            let tuning = anneal.effective_tuning;
            let response = super::types::HygieneAnnealStatusResponse {
                live_artifact_sha256: anneal.live_artifact_sha256,
                live_artifact_bytes: anneal.live_artifact_bytes,
                rollback_rows: anneal.rollback_rows,
                recent_changes: anneal.recent_changes.len(),
                fusion_k: tuning.fusion_k,
                index_m_max: tuning.index_m_max,
                index_ef_construction: tuning.index_ef_construction,
                index_beamwidth: tuning.index_beamwidth,
                index_ef_search: tuning.index_ef_search,
                index_alpha: tuning.index_alpha,
                budget_cpu_used_fraction: anneal.budget.cpu_used_fraction,
                budget_vram_used_bytes: anneal.budget.vram_used_bytes,
                budget_warning_code: anneal.budget.warning_code,
                tripwire_count: anneal.tripwires.len(),
            };
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "anneal_live_artifact={} rollback_rows={} recent_changes={} tripwires={}",
                    response.live_artifact_sha256,
                    response.rollback_rows,
                    response.recent_changes,
                    response.tripwire_count,
                ),
                |out| out.anneal_status = Some(response),
            )))
        }
        HygieneOperation::AnnealSearchPropose => {
            let spec = params
                .0
                .anneal_search_propose
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "anneal_search_propose"))?;
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
                &crate::m3::hygiene::required_permissions_anneal_mutation(),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_anneal",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the Anneal search proposal",
                )
            })?;
            let status = db.calyx_vault_status().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_anneal",
                    HYGIENE_SOT,
                    crate::m1::mcp_error(
                        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                        format!("read live Anneal tuning before proposal: {error}"),
                    ),
                    "repair the native Anneal pointer before proposing a search generation",
                )
            })?;
            let mut candidate = status
                .anneal
                .ok_or_else(|| {
                    crate::m1::mcp_error(
                        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                        "Calyx vault status has no native Anneal readback",
                    )
                })?
                .effective_tuning;
            candidate.index_m_max = spec.index_m_max;
            candidate.index_ef_construction = spec.index_ef_construction;
            candidate.index_beamwidth = spec.index_beamwidth;
            candidate.index_ef_search = spec.index_ef_search;
            candidate.index_alpha = spec.index_alpha;
            let panel_version = spec.panel_version;
            let description = spec.description;
            let report = tokio::task::spawn_blocking(move || {
                db.propose_calyx_search_tuning(panel_version, candidate, &description)
            })
            .await
            .map_err(|error| {
                crate::m1::mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    format!("Anneal search proposal blocking task failed to join: {error}"),
                )
            })?
            .map_err(|error| {
                crate::m1::mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    format!("Anneal search proposal failed: {error}"),
                )
            })?;
            let (outcome, change_id) = match report.change.outcome {
                calyx_anneal::ChangeOutcome::Promoted(id) => ("promoted".to_owned(), Some(id.0)),
                calyx_anneal::ChangeOutcome::Reverted { change_id, .. } => {
                    ("reverted".to_owned(), Some(change_id.0))
                }
            };
            let response = super::types::HygieneAnnealSearchProposeResponse {
                outcome,
                change_id,
                panel_version: report.panel_version,
                source_base_seq: report.source_base_seq,
                query_count: report.query_count,
                prior_artifact_sha256: report.change.prior_artifact_sha256,
                candidate_artifact_sha256: report.change.candidate_artifact_sha256,
                live_artifact_sha256_after: report.change.live_artifact_sha256_after,
                incumbent_manifest_sha256: report.incumbent_manifest_sha256,
                candidate_manifest_sha256: report.candidate_manifest_sha256,
                live_manifest_sha256_after: report.live_manifest_sha256_after,
                candidate_slot_metrics: report
                    .candidate_slot_metrics
                    .into_iter()
                    .map(|metric| super::types::HygieneAnnealSearchSlotMetrics {
                        slot: metric.slot,
                        query_count: metric.query_count,
                        recall_mean: metric.recall_mean,
                        recall_min: metric.recall_min,
                        search_p99_ms_mean: metric.search_p99_ms_mean,
                        search_p99_ms_max: metric.search_p99_ms_max,
                    })
                    .collect(),
                incumbent_slot_metrics: report
                    .incumbent_slot_metrics
                    .into_iter()
                    .map(|metric| super::types::HygieneAnnealSearchSlotMetrics {
                        slot: metric.slot,
                        query_count: metric.query_count,
                        recall_mean: metric.recall_mean,
                        recall_min: metric.recall_min,
                        search_p99_ms_mean: metric.search_p99_ms_mean,
                        search_p99_ms_max: metric.search_p99_ms_max,
                    })
                    .collect(),
            };
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "outcome={} change_id={:?} panel={} base_seq={} queries={} live_artifact={} live_manifest={}",
                    response.outcome,
                    response.change_id,
                    response.panel_version,
                    response.source_base_seq,
                    response.query_count,
                    response.live_artifact_sha256_after,
                    response.live_manifest_sha256_after,
                ),
                |out| out.anneal_search_propose = Some(response),
            )))
        }
        HygieneOperation::AnnealRollback => {
            let spec = params
                .0
                .anneal_rollback
                .ok_or_else(|| missing_spec(HYGIENE_TOOL, "anneal_rollback"))?;
            require_maintenance_profile(
                service,
                &request_context,
                HYGIENE_TOOL,
                operation.as_str(),
                &format!("change_{}", spec.change_id),
                HYGIENE_SOT,
            )?;
            service.require_m3_permissions(
                HYGIENE_TOOL,
                &crate::m3::hygiene::required_permissions_anneal_mutation(),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    HYGIENE_TOOL,
                    operation.as_str(),
                    "calyx_anneal",
                    HYGIENE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry the Anneal rollback",
                )
            })?;
            let report =
                tokio::task::spawn_blocking(move || db.rollback_calyx_anneal(spec.change_id))
                    .await
                    .map_err(|error| {
                        crate::m1::mcp_error(
                            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                            format!("Anneal rollback blocking task failed to join: {error}"),
                        )
                    })?
                    .map_err(|error| {
                        crate::m1::mcp_error(
                            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                            format!("Anneal rollback failed: {error}"),
                        )
                    })?;
            let response = super::types::HygieneAnnealRollbackResponse {
                change_id: report.change_id,
                candidate_artifact_sha256: report.candidate_artifact_sha256,
                restored_artifact_sha256: report.restored_artifact_sha256,
                restored_artifact_bytes: report.restored_artifact_bytes,
                rollback_rows_after: report.rollback_rows_after,
            };
            Ok(Json(hygiene_response(
                operation,
                format!(
                    "change_id={} restored_artifact={} bytes={} rollback_rows={}",
                    response.change_id,
                    response.restored_artifact_sha256,
                    response.restored_artifact_bytes,
                    response.rollback_rows_after,
                ),
                |out| out.anneal_rollback = Some(response),
            )))
        }
    }
}

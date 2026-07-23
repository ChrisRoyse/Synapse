use std::sync::{Arc, LazyLock};

use rmcp::{RoleServer, service::RequestContext};
use synapse_core::error_codes;
use tokio::sync::{Semaphore, TryAcquireError};

use crate::server::{ErrorData, Json, Parameters, SynapseService};

use super::{
    STORAGE_SOT, STORAGE_TOOL,
    errors::{facade_conflict_error, facade_delegate_error, missing_spec},
    policy::require_maintenance_profile,
    response::storage_response,
    types::{StorageOperation, StorageParams, StorageResponse},
    validation::validate_storage_params,
};

/// At most one persisted search-index rebuild may execute at a time. A rebuild
/// reloads the whole Base panel, builds DiskANN/id-map/raw-sidecar artifacts,
/// and republishes the exact-panel generation directory; a second concurrent
/// rebuild of the same vault is a staging/publish corruption hazard. Admission
/// is therefore bounded to a single permit and a concurrent caller fails closed
/// (`try_acquire`) rather than silently queueing behind the in-flight rebuild.
static SEARCH_REBUILD_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(1)));
pub(super) async fn handle(
    service: &SynapseService,
    params: Parameters<StorageParams>,
    request_context: RequestContext<RoleServer>,
) -> Result<Json<StorageResponse>, ErrorData> {
    validate_storage_params(&params.0)?;
    let operation = params.0.operation;
    tracing::info!(
        code = "MCP_TOOL_INVOCATION",
        kind = STORAGE_TOOL,
        operation = operation.as_str(),
        "tool.invocation kind=storage"
    );
    match operation {
        StorageOperation::Inspect => {
            let spec = params.0.inspect.unwrap_or_default();
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_inspect(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "storage",
                    STORAGE_SOT,
                    error,
                    "repair storage/reflex initialization and retry storage operation=inspect",
                )
            })?;
            let response = crate::m3::storage::inspect_storage(&db, &spec).map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "storage_inspect",
                    STORAGE_SOT,
                    error,
                    "inspect storage health and CF metadata before retrying",
                )
            })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "{} CF rows={} pressure={}",
                    response.storage_backend,
                    response.cf_row_counts.len(),
                    response.pressure_level.name
                ),
                |out| out.inspect = Some(response),
            )))
        }
        StorageOperation::Summary => {
            let spec = params.0.summary.unwrap_or_default();
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_inspect(&spec),
            )?;
            let response = service.storage_summary_snapshot().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "storage_summary",
                    STORAGE_SOT,
                    error,
                    "repair storage initialization and read storage backend CF metadata again",
                )
            })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "{} summary cf_count={} pressure={}",
                    response.storage_backend,
                    response.cf_row_counts.len(),
                    response.pressure_level.name
                ),
                |out| out.summary = Some(response),
            )))
        }
        StorageOperation::Anchors => {
            let spec = params
                .0
                .anchors
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "anchors"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_anchors(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "storage",
                    STORAGE_SOT,
                    error,
                    "repair storage/reflex initialization and retry storage operation=anchors",
                )
            })?;
            let response =
                crate::m3::storage::inspect_storage_anchors(&db, &spec).map_err(|error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        &spec.cf_name,
                        STORAGE_SOT,
                        error,
                        "pass an exact source cf_name/key_hex pair and inspect the source row before retrying",
                    )
                })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Calyx Anchors CF source_cf={} key={} cx_id={} anchors={}",
                    response.source_cf,
                    response.source_key_hex,
                    response.cx_id,
                    response.anchor_count
                ),
                |out| out.anchors = Some(response),
            )))
        }
        StorageOperation::TemporalPanels => {
            let spec = params
                .0
                .temporal_panels
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "temporal_panels"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_temporal_panels(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_registry",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=temporal_panels",
                )
            })?;
            let response = crate::m3::storage::inspect_temporal_panels(&db, &spec).map_err(
                |error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        "calyx_registry",
                        STORAGE_SOT,
                        error,
                        "inspect the native Registry CF and repair any malformed or missing panel contract",
                    )
                },
            )?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Calyx Registry CF temporal_panel_registrations={}",
                    response.registration_count
                ),
                |out| out.temporal_panels = Some(response),
            )))
        }
        StorageOperation::TemporalRerank => {
            let spec = params
                .0
                .temporal_rerank
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "temporal_rerank"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_temporal_rerank(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_registry",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=temporal_rerank",
                )
            })?;
            let response = crate::m3::storage::run_temporal_rerank(&db, &spec).map_err(
                |error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        "calyx_registry",
                        STORAGE_SOT,
                        error,
                        "supply one bounded content-only candidate set from an exact registered panel generation with active source event time",
                    )
                },
            )?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Calyx Base snapshot={} Registry panel={} generation={} ranked_hits={}",
                    response.snapshot_seq,
                    response.panel_name,
                    response.panel_version,
                    response.hits.len()
                ),
                |out| out.temporal_rerank = Some(response),
            )))
        }
        StorageOperation::TemporalBackfill => {
            let spec = params
                .0
                .temporal_backfill
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "temporal_backfill"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_temporal_backfill(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_base",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=temporal_backfill",
                )
            })?;
            let response = crate::m3::storage::run_temporal_backfill(&db, &spec).map_err(
                |error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        &spec.source_cf,
                        STORAGE_SOT,
                        error,
                        "inspect the exact authoritative source row and registered panel before retrying",
                    )
                },
            )?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Calyx Base temporal migration source_cf={} scope={} examined={} inserted={} changed={} current={} latest_seq={}",
                    response.source_cf,
                    response.source_scope,
                    response.examined_rows,
                    response.inserted_rows,
                    response.backfilled_rows,
                    response.already_current_rows,
                    response.latest_seq
                ),
                |out| out.temporal_backfill = Some(response),
            )))
        }
        StorageOperation::SearchRebuild => {
            let spec = params
                .0
                .search_rebuild
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "search_rebuild"))?;
            let source_id = format!("panel_{}", spec.expected_panel_version);
            require_maintenance_profile(
                service,
                &request_context,
                STORAGE_TOOL,
                operation.as_str(),
                &source_id,
                STORAGE_SOT,
            )?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_search_rebuild(&spec),
            )?;
            let db = service.m3_storage()?;
            // Fail closed if a rebuild is already in flight. A single admission
            // permit both bounds blocking-pool pressure and rejects a concurrent
            // rebuild of the same vault instead of serializing behind it.
            let permit = Arc::clone(&SEARCH_REBUILD_PERMITS)
                .try_acquire_owned()
                .map_err(|error| match error {
                    TryAcquireError::NoPermits => facade_conflict_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        &source_id,
                        STORAGE_SOT,
                        error_codes::STORAGE_SEARCH_REBUILD_IN_PROGRESS,
                        "a persisted search-index rebuild is already in progress for this vault"
                            .to_owned(),
                        "wait for the in-flight storage operation=search_rebuild to publish its generation, then retry",
                    ),
                    TryAcquireError::Closed => facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        &source_id,
                        STORAGE_SOT,
                        crate::m1::mcp_error(
                            error_codes::TOOL_INTERNAL_ERROR,
                            "search-rebuild admission semaphore was unexpectedly closed".to_owned(),
                        ),
                        "restart the daemon; the search-rebuild admission gate is no longer available",
                    ),
                })?;
            // The rebuild is strictly blocking, CPU/IO-bound work that must not
            // occupy a Tokio runtime worker serving MCP requests. Offload it to
            // the blocking pool and hold the admission permit for the task's
            // lifetime so a concurrent caller keeps failing closed until publish.
            let expected_panel_version = spec.expected_panel_version;
            let report = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                db.rebuild_calyx_search_indexes(expected_panel_version)
            })
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                    crate::m1::mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("search-rebuild blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs for the SYNAPSE_CALYX_SEARCH_REBUILD phase records; the rebuild task terminated abnormally",
                )
            })?
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                    crate::m1::mcp_error(error.code(), error.to_string()),
                    "inspect the exact durable panel state, rebuild marker, and named physical artifact before retrying",
                )
            })?;
            let response = crate::m3::storage::StorageSearchRebuildResponse {
                panel_version: report.generation.panel_version,
                base_seq: report.generation.base_seq,
                before_manifest_sha256: report.before_manifest_sha256,
                manifest_sha256: report.generation.manifest_sha256,
                manifest_path: report.manifest_path.display().to_string(),
                diskann_build_backend: report.generation.diskann_build_backend,
                slots: report
                    .generation
                    .slots
                    .into_iter()
                    .map(|slot| crate::m3::storage::StorageSearchRebuildSlot {
                        panel_version: slot.panel_slot.panel_version(),
                        slot_id: u32::from(slot.panel_slot.slot_id().get()),
                        kind: slot.kind,
                        shape: format!("{:?}", slot.shape),
                        len: slot.len,
                        built_at_seq: slot.built_at_seq,
                    })
                    .collect(),
                raw_sidecars: report
                    .raw_sidecars
                    .into_iter()
                    .map(|sidecar| crate::m3::storage::StorageSearchRawSidecar {
                        path: sidecar.path.display().to_string(),
                        layout: sidecar.layout,
                        len_bytes: sidecar.len_bytes,
                        file_count: sidecar.file_count,
                        sha256: sidecar.sha256,
                    })
                    .collect(),
            };
            Ok(Json(storage_response(
                operation,
                format!(
                    "panel={} base_seq={} manifest_sha256={} slots={} raw_sidecars={}",
                    response.panel_version,
                    response.base_seq,
                    response.manifest_sha256,
                    response.slots.len(),
                    response.raw_sidecars.len()
                ),
                |out| out.search_rebuild = Some(response),
            )))
        }
        StorageOperation::GcOnce => {
            let spec = params
                .0
                .gc_once
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "gc_once"))?;
            require_maintenance_profile(
                service,
                &request_context,
                STORAGE_TOOL,
                operation.as_str(),
                &spec.cf_name,
                STORAGE_SOT,
            )?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_gc(&spec),
            )?;
            let db = service.m3_storage()?;
            let response =
                crate::m3::storage::run_storage_gc_once(&db, &spec).map_err(|error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        &spec.cf_name,
                        STORAGE_SOT,
                        error,
                        "fix row caps / CF name and inspect CF row counts before retrying",
                    )
                })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "{} before_rows={} after_rows={} evicted={}",
                    response.cf_name,
                    response.before_rows,
                    response.after_rows,
                    response.total_evicted_rows
                ),
                |out| out.gc_once = Some(response),
            )))
        }
    }
}

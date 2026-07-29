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

/// At most one durable vault backup may execute at a time. A backup holds the
/// native-compaction guard and copies the whole sacred vault tree; a second
/// concurrent backup would contend on that guard and could interleave two copies
/// onto the blocking pool. Admission is a single permit and a concurrent caller
/// fails closed (`try_acquire`) rather than serializing behind the in-flight
/// backup.
static BACKUP_PERMITS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// At most one orphan physical slot-CF retirement may execute at a time. The
/// pass scans Base to derive the legitimate slot set and holds the exclusive
/// router lock per CF drop; a second concurrent pass would contend on that lock
/// and could race two removals. Admission is a single permit and a concurrent
/// caller fails closed (`try_acquire`) rather than serializing behind the
/// in-flight pass.
static ORPHAN_SLOT_GC_PERMITS: LazyLock<Arc<Semaphore>> =
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
        StorageOperation::FindSimilar => {
            let spec = params
                .0
                .find_similar
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "find_similar"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_find_similar(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_search",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=find_similar",
                )
            })?;
            // Fused find opens the persisted per-slot indexes and runs
            // DiskANN/BM25 recall — CPU/IO-bound work that must not park a runtime
            // worker serving MCP requests. Offload to the blocking pool.
            let response = tokio::task::spawn_blocking(move || {
                crate::m3::storage::run_find_similar(&db, &spec)
            })
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_search",
                    STORAGE_SOT,
                    crate::m1::mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("find-similar blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the find-similar task terminated abnormally",
                )
            })?
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_search",
                    STORAGE_SOT,
                    error,
                    "rebuild the panel search indexes if the persisted generation is missing/stale, or correct the query_mode/fusion/example before retrying",
                )
            })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Calyx find-similar panel={} fusion={} query={} hits={} temporal_applied={}",
                    response.panel_version,
                    response.fusion,
                    response.query_kind,
                    response.hits.len(),
                    response.temporal_applied
                ),
                |out| out.find_similar = Some(response),
            )))
        }
        StorageOperation::RetireOrphanSlotCfs => {
            let spec = params
                .0
                .retire_orphan_slot_cfs
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "retire_orphan_slot_cfs"))?;
            let source_id = "orphan_slot_cf_gc";
            require_maintenance_profile(
                service,
                &request_context,
                STORAGE_TOOL,
                operation.as_str(),
                source_id,
                STORAGE_SOT,
            )?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_retire_orphan_slot_cfs(&spec),
            )?;
            let db = service.m3_storage()?;
            // Fail closed if a retirement pass is already in flight. A single
            // admission permit bounds blocking-pool pressure and rejects a
            // concurrent orphan GC of the same vault instead of serializing.
            let permit = Arc::clone(&ORPHAN_SLOT_GC_PERMITS)
                .try_acquire_owned()
                .map_err(|error| match error {
                    TryAcquireError::NoPermits => facade_conflict_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        source_id,
                        STORAGE_SOT,
                        error_codes::STORAGE_ORPHAN_SLOT_GC_IN_PROGRESS,
                        "an orphan slot-CF retirement is already in progress for this vault"
                            .to_owned(),
                        "wait for the in-flight storage operation=retire_orphan_slot_cfs to finish, then retry",
                    ),
                    TryAcquireError::Closed => facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        source_id,
                        STORAGE_SOT,
                        crate::m1::mcp_error(
                            error_codes::TOOL_INTERNAL_ERROR,
                            "orphan slot-CF GC admission semaphore was unexpectedly closed"
                                .to_owned(),
                        ),
                        "restart the daemon; the orphan slot-CF GC admission gate is no longer available",
                    ),
                })?;
            // The pass scans Base and holds the exclusive router lock per CF
            // drop: strictly blocking, CPU/IO-bound work that must not occupy a
            // Tokio runtime worker. Offload it and hold the permit for the task's
            // lifetime so a concurrent caller keeps failing closed until it ends.
            let report = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                db.retire_orphan_slot_cfs()
            })
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    source_id,
                    STORAGE_SOT,
                    crate::m1::mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("orphan slot-CF GC blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the orphan slot-CF GC task terminated abnormally",
                )
            })?
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    source_id,
                    STORAGE_SOT,
                    crate::m1::mcp_error(error.code(), error.to_string()),
                    "inspect the vault slot-CF tree and live Base membership before retrying",
                )
            })?;
            let response = crate::m3::storage::storage_orphan_slot_gc_response(report);
            Ok(Json(storage_response(
                operation,
                format!(
                    "Calyx orphan slot-CF retirement base_rows_scanned={} present={} retired={} skipped_live={}",
                    response.base_rows_scanned,
                    response.present_slot_ids.len(),
                    response.retired.len(),
                    response.skipped_live.len()
                ),
                |out| out.retire_orphan_slot_cfs = Some(response),
            )))
        }
        StorageOperation::Backup => {
            let spec = params
                .0
                .backup
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "backup"))?;
            let source_id = spec.target_dir.clone();
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
                &crate::m3::storage::required_permissions_backup(&spec),
            )?;
            let db = service.m3_storage()?;
            // Fail closed if a backup is already in flight. A single admission
            // permit both bounds blocking-pool pressure and rejects a concurrent
            // backup of the same vault instead of contending on the guard.
            let permit = Arc::clone(&BACKUP_PERMITS)
                .try_acquire_owned()
                .map_err(|error| match error {
                    TryAcquireError::NoPermits => facade_conflict_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        &source_id,
                        STORAGE_SOT,
                        error_codes::STORAGE_BACKUP_IN_PROGRESS,
                        "a durable vault backup is already in progress for this vault".to_owned(),
                        "wait for the in-flight storage operation=backup to publish its manifest, then retry",
                    ),
                    TryAcquireError::Closed => facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        &source_id,
                        STORAGE_SOT,
                        crate::m1::mcp_error(
                            error_codes::TOOL_INTERNAL_ERROR,
                            "backup admission semaphore was unexpectedly closed".to_owned(),
                        ),
                        "restart the daemon; the backup admission gate is no longer available",
                    ),
                })?;
            // Backups are strictly blocking, CPU/IO-bound file copies that must
            // not occupy a Tokio runtime worker serving MCP requests. Offload to
            // the blocking pool and hold the permit for the task's lifetime.
            let response = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                crate::m3::storage::run_storage_backup(&db, &spec)
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
                        format!("backup blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs for the SYNAPSE_CALYX_VAULT_BACKUP records; the backup task terminated abnormally",
                )
            })??;
            Ok(Json(storage_response(
                operation,
                format!(
                    "backup vault_id={} durable_seq={} files={} bytes={} verify_success={} tip={} manifest_sha256={}",
                    response.vault_id,
                    response.durable_seq,
                    response.file_count,
                    response.total_bytes,
                    response.verify.success,
                    response.verify.ledger_tip_hash,
                    response.manifest_sha256,
                ),
                |out| out.backup = Some(response),
            )))
        }
        StorageOperation::RestoreVerify => {
            let spec = params
                .0
                .restore_verify
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "restore_verify"))?;
            let source_id = spec.vault_path.clone();
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_restore_verify(&spec),
            )?;
            let db = service.m3_storage()?;
            // Byte-level verification scans every SST/WAL of the target vault; it
            // is read-only but CPU/IO-bound, so run it off the runtime workers.
            let response = tokio::task::spawn_blocking(move || {
                crate::m3::storage::run_storage_restore_verify(&db, &spec)
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
                        format!("restore-verify blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the restore-verify task terminated abnormally",
                )
            })??;
            Ok(Json(storage_response(
                operation,
                format!(
                    "restore_verify vault_path={} success={} chain_intact={} constellations={} anchors={} ledger_entries={} tip={} wal_bytes={}",
                    response.verify.vault_path,
                    response.verify.success,
                    response.verify.chain_intact,
                    response.verify.constellation_count,
                    response.verify.anchor_count,
                    response.verify.ledger_entry_count,
                    response.verify.ledger_tip_hash,
                    response.verify.wal_bytes_present,
                ),
                |out| out.restore_verify = Some(response),
            )))
        }
        StorageOperation::Intelligence => {
            let spec = params
                .0
                .intelligence
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "intelligence"))?;
            let sub_operation = spec.operation;
            let source_id = format!("panel_{}", spec.panel_version);
            // The weave sub-operation persists derived XTerm/Graph rows, so it is
            // maintenance-gated exactly like the other mutating storage ops;
            // abundance is a read-only physical CF readback.
            if sub_operation.mutates_state() {
                require_maintenance_profile(
                    service,
                    &request_context,
                    STORAGE_TOOL,
                    sub_operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                )?;
            }
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_intelligence(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_loom",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=intelligence",
                )
            })?;
            // Weaving and abundance both scan the whole Base panel and run the
            // substrate math; that is blocking CPU/IO work that must not occupy a
            // runtime worker serving MCP requests. Offload to the blocking pool.
            let response = tokio::task::spawn_blocking(move || {
                use crate::m3::storage::{StorageIntelligenceOperation, StorageIntelligenceResponse};
                let base = StorageIntelligenceResponse {
                    operation: sub_operation,
                    weave: None,
                    abundance: None,
                    bits: None,
                    sufficiency: None,
                    redundancy: None,
                    synergy: None,
                    causality: None,
                    periodicity: None,
                    drift: None,
                    hazard: None,
                    kernel: None,
                    kernel_answer: None,
                };
                match sub_operation {
                    StorageIntelligenceOperation::Weave => {
                        crate::m3::storage::run_intelligence_weave(&db, &spec).map(|weave| {
                            StorageIntelligenceResponse {
                                weave: Some(weave),
                                ..base
                            }
                        })
                    }
                    StorageIntelligenceOperation::Abundance => {
                        crate::m3::storage::run_intelligence_abundance(&db, &spec).map(|abundance| {
                            StorageIntelligenceResponse {
                                abundance: Some(abundance),
                                ..base
                            }
                        })
                    }
                    StorageIntelligenceOperation::Bits => {
                        crate::m3::storage::run_intelligence_bits(&db, &spec).map(|bits| {
                            StorageIntelligenceResponse {
                                bits: Some(bits),
                                ..base
                            }
                        })
                    }
                    StorageIntelligenceOperation::Sufficiency => {
                        crate::m3::storage::run_intelligence_sufficiency(&db, &spec).map(
                            |sufficiency| StorageIntelligenceResponse {
                                sufficiency: Some(sufficiency),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::Redundancy => {
                        crate::m3::storage::run_intelligence_redundancy(&db, &spec).map(
                            |redundancy| StorageIntelligenceResponse {
                                redundancy: Some(redundancy),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::Synergy => {
                        crate::m3::storage::run_intelligence_synergy(&db, &spec).map(|synergy| {
                            StorageIntelligenceResponse {
                                synergy: Some(synergy),
                                ..base
                            }
                        })
                    }
                    StorageIntelligenceOperation::Causality => {
                        crate::m3::storage::run_intelligence_causality(&db, &spec).map(
                            |causality| StorageIntelligenceResponse {
                                causality: Some(causality),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::Periodicity => {
                        crate::m3::storage::run_intelligence_periodicity(&db, &spec).map(
                            |periodicity| StorageIntelligenceResponse {
                                periodicity: Some(periodicity),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::Drift => {
                        crate::m3::storage::run_intelligence_drift(&db, &spec).map(|drift| {
                            StorageIntelligenceResponse {
                                drift: Some(drift),
                                ..base
                            }
                        })
                    }
                    StorageIntelligenceOperation::Hazard => {
                        crate::m3::storage::run_intelligence_hazard(&db, &spec).map(|hazard| {
                            StorageIntelligenceResponse {
                                hazard: Some(hazard),
                                ..base
                            }
                        })
                    }
                    StorageIntelligenceOperation::Kernel => {
                        crate::m3::storage::run_intelligence_kernel(&db, &spec).map(|kernel| {
                            StorageIntelligenceResponse {
                                kernel: Some(kernel),
                                ..base
                            }
                        })
                    }
                    StorageIntelligenceOperation::KernelAnswer => {
                        crate::m3::storage::run_intelligence_kernel_answer(&db, &spec).map(
                            |kernel_answer| StorageIntelligenceResponse {
                                kernel_answer: Some(kernel_answer),
                                ..base
                            },
                        )
                    }
                }
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
                        format!("intelligence blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the intelligence weave/abundance task terminated abnormally",
                )
            })??;
            let summary = if let Some(weave) = &response.weave {
                format!(
                    "intelligence weave panel={} records_woven={} cross_terms={} agreement_edges={} between_record_edges={} xterm_rows={} graph_rows={} dda_signal_yield={} blind_spot_pairs={}/{} blind_spot_records={} outside_window={}",
                    weave.panel_version,
                    weave.records_woven,
                    weave.cross_terms_materialized,
                    weave.agreement_edges_persisted,
                    weave.between_record_edges_persisted,
                    weave.xterm_cf_rows_after,
                    weave.graph_cf_rows_after,
                    weave.dda_signal_yield,
                    weave.blind_spot_pairs,
                    weave.lens_pairs_possible,
                    weave.blind_spot_records,
                    weave.records_outside_window,
                )
            } else if let Some(abundance) = &response.abundance {
                format!(
                    "intelligence abundance panel={} n_lenses={} n_constellations={} c_n2={} materialized={} dda_signal_yield={} dpi_ceiling_bits={:?} dpi_ceiling_provisional={} xterm_rows={} graph_rows={}",
                    abundance.panel_version,
                    abundance.n_lenses,
                    abundance.n_constellations,
                    abundance.c_n2_upper_bound,
                    abundance.materialized,
                    abundance.dda_signal_yield,
                    abundance.dpi_ceiling_bits,
                    abundance.dpi_ceiling_provisional,
                    abundance.xterm_cf_rows,
                    abundance.graph_cf_rows,
                )
            } else if let Some(bits) = &response.bits {
                format!(
                    "intelligence bits panel={} anchor={} anchored_records={} total_bits={:.4} grounded={} domain_provisional={} domain_grounded_fraction={:.4} slots={} assay_rows={}",
                    bits.panel_version,
                    bits.anchor_kind,
                    bits.anchored_records,
                    bits.total_bits,
                    bits.grounded,
                    bits.domain_provisional,
                    bits.domain_grounded_fraction,
                    bits.slots.len(),
                    bits.assay_cf_rows_after,
                )
            } else if let Some(sufficiency) = &response.sufficiency {
                format!(
                    "intelligence sufficiency panel={} anchor={} panel_bits={:.4} anchor_entropy_bits={:.4} sufficient={} deficit_bits={:.4} domain_provisional={} domain_grounded_fraction={:.4} deficits={} assay_rows={}",
                    sufficiency.panel_version,
                    sufficiency.anchor_kind,
                    sufficiency.panel_bits,
                    sufficiency.anchor_entropy_bits,
                    sufficiency.sufficient,
                    sufficiency.deficit_bits,
                    sufficiency.domain_provisional,
                    sufficiency.domain_grounded_fraction,
                    sufficiency.deficits.len(),
                    sufficiency.assay_cf_rows_after,
                )
            } else if let Some(redundancy) = &response.redundancy {
                format!(
                    "intelligence redundancy panel={} n_lenses={} effective_rank={:.4} pairs_evaluated={} redundant_pairs={} domain_provisional={} domain_grounded_fraction={:.4} assay_rows={}",
                    redundancy.panel_version,
                    redundancy.n_lenses,
                    redundancy.effective_rank,
                    redundancy.pairs_evaluated,
                    redundancy.redundant_pairs.len(),
                    redundancy.domain_provisional,
                    redundancy.domain_grounded_fraction,
                    redundancy.assay_cf_rows_after,
                )
            } else if let Some(synergy) = &response.synergy {
                format!(
                    "intelligence synergy panel={} anchor={} anchored_records={} n_lenses={} lenses_paired={} pairs_evaluated={} synergistic_pairs={} max_gain_bits={:.4} domain_provisional={} domain_grounded_fraction={:.4} assay_rows={}",
                    synergy.panel_version,
                    synergy.anchor_kind,
                    synergy.anchored_records,
                    synergy.n_lenses,
                    synergy.lenses_paired,
                    synergy.pairs_evaluated,
                    synergy.synergistic_pairs,
                    synergy.max_gain_bits,
                    synergy.domain_provisional,
                    synergy.domain_grounded_fraction,
                    synergy.assay_cf_rows_after,
                )
            } else if let Some(causality) = &response.causality {
                format!(
                    "intelligence causality panel={} a={} b={} best_lag={} t_a_to_b={:.4} t_b_to_a={:.4} direction={} estimator={} grounded={} graph_rows={}",
                    causality.panel_version,
                    causality.group_a,
                    causality.group_b,
                    causality.best_lag,
                    causality.t_a_to_b,
                    causality.t_b_to_a,
                    causality.dominant_direction,
                    causality.estimator,
                    causality.grounded,
                    causality.graph_cf_rows_after,
                )
            } else if let Some(periodicity) = &response.periodicity {
                format!(
                    "intelligence periodicity panel={} n_samples={} dominant_period_seconds={:?} significant={} peaks={} temporal_xterm_rows={}",
                    periodicity.panel_version,
                    periodicity.n_samples,
                    periodicity.dominant_period_seconds,
                    periodicity.significant,
                    periodicity.peaks.len(),
                    periodicity.temporal_xterm_cf_rows_after,
                )
            } else if let Some(drift) = &response.drift {
                format!(
                    "intelligence drift panel={} n_gaps={} cusum_change_detected={} direction={:?} mmd_p_value={:?} temporal_xterm_rows={}",
                    drift.panel_version,
                    drift.n_gaps,
                    drift.cusum_change_detected,
                    drift.cusum_direction,
                    drift.mmd_p_value,
                    drift.temporal_xterm_cf_rows_after,
                )
            } else if let Some(hazard) = &response.hazard {
                format!(
                    "intelligence hazard panel={} n_gaps={} survival={:.4} overdue={} expected_next_seconds={:.1} temporal_xterm_rows={}",
                    hazard.panel_version,
                    hazard.n_gaps,
                    hazard.survival,
                    hazard.overdue,
                    hazard.expected_next_seconds,
                    hazard.temporal_xterm_cf_rows_after,
                )
            } else if let Some(kernel) = &response.kernel {
                format!(
                    "intelligence kernel panel={} content_slot={} kernel_id={} members={} recall_ratio={:.4} grounded={} kernel_rows={}",
                    kernel.panel_version,
                    kernel.content_slot,
                    kernel.kernel_id,
                    kernel.members,
                    kernel.recall_ratio,
                    kernel.grounded,
                    kernel.kernel_cf_rows_after,
                )
            } else if let Some(kernel_answer) = &response.kernel_answer {
                format!(
                    "intelligence kernel_answer panel={} query={} grounded={} anchor={} hops={} total_score={:.4} recall_ratio={:.4}",
                    kernel_answer.panel_version,
                    kernel_answer.query_cx_id,
                    kernel_answer.grounded,
                    kernel_answer.anchor_kernel_node,
                    kernel_answer.hop_count,
                    kernel_answer.total_score,
                    kernel_answer.recall_ratio,
                )
            } else {
                "intelligence".to_owned()
            };
            Ok(Json(storage_response(operation, summary, |out| {
                out.intelligence = Some(response);
            })))
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

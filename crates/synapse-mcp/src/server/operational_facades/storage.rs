use std::sync::{Arc, LazyLock};

use rmcp::{RoleServer, service::RequestContext};
use synapse_core::error_codes;
use tokio::sync::{Semaphore, TryAcquireError};

use crate::server::{ErrorData, Json, Parameters, SynapseService};

use super::{
    STORAGE_SOT, STORAGE_TOOL,
    errors::{facade_conflict_error, facade_delegate_error, missing_spec, storage_error_data},
    policy::{require_maintenance_profile, require_storage_operation_authority},
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

/// Transcript-order repair rewrites one durable secondary projection and its
/// publication rows. The projection module also serializes source writers, but
/// this non-waiting facade permit keeps a second operator call from occupying a
/// blocking worker while the first repair owns that serialization boundary.
static TRANSCRIPT_ORDER_REBUILD_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// Panel generation allocation and Registry publication are one ordered
/// mutation stream. Reject concurrent callers before either can reserve an id.
static PANEL_LIFECYCLE_PERMITS: LazyLock<Arc<Semaphore>> =
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
            // Inspection sweeps every column family three times (bytes, row
            // counts, tail samples) plus the whole vault once. Even with the
            // bounded row-guard holds of #2041 that is seconds of strictly
            // blocking CPU/IO, and running it inline parked a Tokio runtime
            // worker for the whole call — which is why two adjacent wired
            // `health` calls measured 5.339 s and 6.191 s while direct `/health`
            // reads on the same daemon generation stayed at 442-518 ms. Offload
            // it exactly like every other scan-bound storage operation in this
            // file.
            let response = Box::pin(
                synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
                    "storage_inspect",
                    move || crate::m3::storage::inspect_storage(&db, &spec),
                ),
            )
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "storage_inspect",
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* admission/completion and STORAGE_CALYX_INSPECT_SWEEP_DONE records",
                )
            })?
            .map_err(|error| {
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
        StorageOperation::RowRead => {
            let spec = params
                .0
                .row_read
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "row_read"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_row_read(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "storage",
                    STORAGE_SOT,
                    error,
                    "repair storage/reflex initialization and retry storage operation=row_read",
                )
            })?;
            let response = crate::m3::storage::read_storage_row(&db, &spec).map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    &spec.cf_name,
                    STORAGE_SOT,
                    error,
                    "name an allowlisted readable cf_name and exactly one of key_hex / \
                         observation_id, both of which every observe response returns under \
                         `diagnostics.persisted`",
                )
            })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "{} row key={} value_bytes={} decoded_as={} observation_id={}",
                    response.cf_name,
                    response.key_hex,
                    response.value_len_bytes,
                    response.decoded_as,
                    response.observation.observation_id
                ),
                |out| out.row_read = Some(Box::new(response)),
            )))
        }
        operation @ (StorageOperation::SnapshotGcStatus
        | StorageOperation::SnapshotOpen
        | StorageOperation::SnapshotRead
        | StorageOperation::SnapshotRelease) => handle_snapshot(service, params.0, operation),
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
        StorageOperation::CorpusHistogram => {
            let spec = params
                .0
                .corpus_histogram
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "corpus_histogram"))?;
            Box::pin(handle_corpus_histogram(service, spec)).await
        }
        StorageOperation::PanelCoverage => {
            let spec = params
                .0
                .panel_coverage
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "panel_coverage"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_panel_coverage(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_storage",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=panel_coverage",
                )
            })?;
            // A whole-Base scan plus one row count per declared full-CF source
            // is strictly blocking work and must never occupy a runtime worker
            // serving MCP requests.
            let response = Box::pin(
                synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
                    "storage_panel_coverage",
                    move || crate::m3::storage::inspect_panel_coverage(&db, &spec),
                ),
            )
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_storage",
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* admission/completion and SYNAPSE_CALYX_PANEL_CENSUS records",
                )
            })?
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_storage",
                    STORAGE_SOT,
                    error,
                    "the census is recomputed from the physical Base CF at call time; a failure here means the vault or a declared source CF could not be scanned, or the row accounting did not add up",
                )
            })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "panels={} base_cf_rows={} records_total={} decode_failures={} \
                     accounting_holds={} superseded_records={} coverage_deficient={:?} \
                     grounding_deficient={:?}",
                    response.panels.len(),
                    response.base_cf_rows,
                    response.records_total,
                    response.decode_failures,
                    response.accounting_holds,
                    response.superseded_records_total,
                    response.coverage_deficient_panels,
                    response.grounding_deficient_panels,
                ),
                |out| out.panel_coverage = Some(response),
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
                    "Calyx Base temporal migration source_cf={} scope={} examined={} inserted={} changed={} current={} temporal_ineligible={} latest_seq={}",
                    response.source_cf,
                    response.source_scope,
                    response.examined_rows,
                    response.inserted_rows,
                    response.backfilled_rows,
                    response.already_current_rows,
                    response.temporal_ineligible_rows,
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
            // The rebuild is strictly blocking, CPU/IO-bound whole-corpus work.
            // Admit it through the same exclusive lane as GC, derived-state
            // maintenance, and disk-pressure compaction so individually bounded
            // working sets cannot multiply into an unbounded process total. The
            // facade permit remains held while this call waits for and owns that
            // lane, so a concurrent explicit rebuild still fails closed.
            let expected_panel_version = spec.expected_panel_version;
            let report = synapse_storage::maintenance::run_admitted_maintenance(
                "storage_search_rebuild",
                move || {
                    let _permit = permit;
                    db.rebuild_calyx_search_indexes(expected_panel_version)
                },
            )
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* admission records and SYNAPSE_CALYX_SEARCH_REBUILD phase records; the exclusive rebuild pass failed",
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
                        scoring_law: slot.scoring_law,
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
        StorageOperation::TranscriptOrderStatus => {
            let spec = params
                .0
                .transcript_order_status
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "transcript_order_status"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_transcript_order_status(&spec),
            )?;
            let db = service.m3_storage()?;
            let response = Box::pin(
                synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
                    "storage_transcript_order_status",
                    move || crate::server::transcript_order::projection_status(&db),
                ),
            )
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "agent-transcript-order",
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* admission/completion records before retrying the projection census",
                )
            })?
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "agent-transcript-order",
                    STORAGE_SOT,
                    crate::m1::mcp_error(error_codes::STORAGE_CORRUPTED, error),
                    "preserve the vault and inspect the named projection Source of Truth",
                )
            })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "transcript order source_rows={} index_rows={} exact_match={} ready={} state_token_sha256={}",
                    response.source_rows,
                    response.index_rows,
                    response.exact_match,
                    response.ready,
                    response.state_token_sha256
                ),
                |out| out.transcript_order_status = Some(Box::new(response)),
            )))
        }
        StorageOperation::TranscriptOrderRebuild => {
            let spec = params
                .0
                .transcript_order_rebuild
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "transcript_order_rebuild"))?;
            require_maintenance_profile(
                service,
                &request_context,
                STORAGE_TOOL,
                operation.as_str(),
                "agent-transcript-order",
                STORAGE_SOT,
            )?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_transcript_order_rebuild(&spec),
            )?;
            let permit = Arc::clone(&TRANSCRIPT_ORDER_REBUILD_PERMITS)
                .try_acquire_owned()
                .map_err(|error| match error {
                    TryAcquireError::NoPermits => facade_conflict_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        "agent-transcript-order",
                        STORAGE_SOT,
                        error_codes::STORAGE_TRANSCRIPT_ORDER_REBUILD_IN_PROGRESS,
                        "a transcript timestamp-order projection rebuild is already in progress"
                            .to_owned(),
                        "wait for the in-flight transcript_order_rebuild call to finish, then read transcript_order_status again",
                    ),
                    TryAcquireError::Closed => facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        "agent-transcript-order",
                        STORAGE_SOT,
                        crate::m1::mcp_error(
                            error_codes::TOOL_INTERNAL_ERROR,
                            "transcript-order rebuild admission semaphore was unexpectedly closed"
                                .to_owned(),
                        ),
                        "restart the daemon; the transcript-order rebuild admission gate is unavailable",
                    ),
                })?;
            let db = service.m3_storage()?;
            let token = spec.expected_repair_token;
            let response = Box::pin(
                synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
                    "storage_transcript_order_rebuild",
                    move || {
                        let _permit = permit;
                        crate::server::transcript_order::rebuild_projection(&db, &token)
                    },
                ),
            )
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "agent-transcript-order",
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* admission/completion records and the durable repair marker before retrying",
                )
            })?
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "agent-transcript-order",
                    STORAGE_SOT,
                    crate::m1::mcp_error(error_codes::STORAGE_CORRUPTED, error),
                    "re-read transcript_order_status and resume only with its exact repair_token_sha256",
                )
            })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "transcript order deleted={} rebuilt={} source_rows={} index_rows={} exact_match={} ready={}",
                    response.deleted_index_rows,
                    response.rebuilt_index_rows,
                    response.after.source_rows,
                    response.after.index_rows,
                    response.after.exact_match,
                    response.after.ready
                ),
                |out| out.transcript_order_rebuild = Some(Box::new(response)),
            )))
        }
        StorageOperation::PanelLifecycle => {
            let spec = params
                .0
                .panel_lifecycle
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "panel_lifecycle"))?;
            let source_id = format!("panel_{}", spec.panel_version);
            let mutating = spec.action != crate::m3::storage::StoragePanelLifecycleAction::Read;
            if mutating {
                require_maintenance_profile(
                    service,
                    &request_context,
                    STORAGE_TOOL,
                    operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                )?;
            }
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_panel_lifecycle(&spec),
            )?;
            let db = service.m3_storage()?;
            let permit = if mutating {
                Some(
                    Arc::clone(&PANEL_LIFECYCLE_PERMITS)
                        .try_acquire_owned()
                        .map_err(|error| match error {
                            TryAcquireError::NoPermits => facade_conflict_error(
                                STORAGE_TOOL,
                                operation.as_str(),
                                &source_id,
                                STORAGE_SOT,
                                error_codes::STORAGE_PANEL_LIFECYCLE_IN_PROGRESS,
                                "a panel lifecycle mutation is already in progress for this vault"
                                    .to_owned(),
                                "wait for the in-flight panel_lifecycle mutation to publish its Registry CF row, read that row, then retry with an appropriate idempotent operation id",
                            ),
                            TryAcquireError::Closed => facade_delegate_error(
                                STORAGE_TOOL,
                                operation.as_str(),
                                &source_id,
                                STORAGE_SOT,
                                crate::m1::mcp_error(
                                    error_codes::TOOL_INTERNAL_ERROR,
                                    "panel-lifecycle admission semaphore was unexpectedly closed"
                                        .to_owned(),
                                ),
                                "restart the daemon; the panel-lifecycle admission gate is no longer available",
                            ),
                        })?,
                )
            } else {
                None
            };
            let response = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                crate::m3::storage::panel_lifecycle(&db, &spec)
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
                        format!("panel-lifecycle blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs for the panel_lifecycle phase record; the task terminated abnormally",
                )
            })??;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Calyx Registry CF panel={} action={:?}",
                    response.panel_version, response.action
                ),
                |out| out.panel_lifecycle = Some(response),
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
            let report = synapse_storage::maintenance::run_admitted_maintenance(
                "storage_retire_orphan_slot_cfs",
                move || {
                    let _permit = permit;
                    db.retire_orphan_slot_cfs()
                },
            )
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    source_id,
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* admission/completion records and the vault slot-CF tree",
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
        StorageOperation::RetireSearchGeneration => {
            let spec = params
                .0
                .retire_search_generation
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "retire_search_generation"))?;
            let source_id = format!("panel:{}", spec.panel_version);
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
                &crate::m3::storage::required_permissions_retire_search_generation(&spec),
            )?;
            let db = service.m3_storage()?;
            let panel_version = spec.panel_version;
            // A directory removal plus two index-root enumerations is blocking
            // filesystem work, so it is offloaded exactly like the other
            // maintenance-gated storage operations rather than run on a Tokio
            // runtime worker.
            let (report, lineage) = tokio::task::spawn_blocking(move || {
                db.retire_search_generation(panel_version)
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
                        format!("search-generation retirement blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the search-generation retirement task terminated abnormally",
                )
            })?
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                    storage_error_data(&error),
                    error.remediation().unwrap_or(
                        "read health.calyx_search_generations_retirable_panel_versions for the \
                         versions this operation accepts",
                    ),
                )
            })?;
            let response = crate::m3::storage::StorageRetireSearchGenerationResponse {
                source_of_truth: "calyx idx/search index root, re-enumerated after removal",
                panel_version: report.panel_version,
                panel_name: lineage.panel_name.to_owned(),
                live_panel_version: lineage.live_panel_version,
                directory: report.directory.clone(),
                files_removed: report.files_removed,
                bytes_reclaimed: report.bytes_reclaimed,
                published_before: report.published_before.clone(),
                published_after: report.published_after.clone(),
                active_panel_version: report.active_panel_version,
            };
            Ok(Json(storage_response(
                operation,
                format!(
                    "retired superseded search generation panel={} ({} superseded by {}) files={} bytes={} published_before={:?} published_after={:?}",
                    response.panel_version,
                    response.panel_name,
                    response.live_panel_version,
                    response.files_removed,
                    response.bytes_reclaimed,
                    response.published_before,
                    response.published_after,
                ),
                |out| out.retire_search_generation = Some(response),
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
            // A backup copies and re-verifies the whole physical vault. Keep its
            // per-operation conflict permit, and also admit the blocking owner
            // through the process-wide corpus lane so a bounded backup cannot
            // overlap search, GC, derived state, or a live verifier and multiply
            // their independent working sets.
            let response = Box::pin(
                synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
                    "storage_backup",
                    move || {
                        let _permit = permit;
                        crate::m3::storage::run_storage_backup(&db, &spec)
                    },
                ),
            )
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* admission/completion and SYNAPSE_CALYX_VAULT_BACKUP records",
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
        StorageOperation::BackupStatus => {
            let spec = params
                .0
                .backup_status
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "backup_status"))?;
            let source_id = spec.target_dir.clone();
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_backup_status(&spec),
            )?;
            let response = crate::m3::storage::run_storage_backup_status(&spec)?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "backup target={} state={} final_exists={} manifest_exists={} marker_exists={} staging_dirs={:?}",
                    source_id,
                    response.state,
                    response.final_exists,
                    response.manifest_exists,
                    response.marker_exists,
                    response.staging_dirs,
                ),
                |out| out.backup_status = Some(response),
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
            // Byte-level verification scans every SST/WAL of the target vault.
            // Read-only does not mean resource-free: share the same exclusive
            // whole-corpus owner as live verification and maintenance.
            let response = Box::pin(
                synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
                    "storage_restore_verify",
                    move || crate::m3::storage::run_storage_restore_verify(&db, &spec),
                ),
            )
            .await
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* admission/completion records and the named restore-verification error",
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
            crate::m3::storage::validate_intelligence_numeric_ranges(&spec)?;
            let sub_operation = spec.operation;
            let source_id = format!("panel_{}", spec.panel_version);
            // #2077: a state-changing sub-operation clears the gate its declared
            // class demands, never a gate inferred here. Control-class
            // sub-operations (weave, kernel, oracle_complete) keep break_glass +
            // the foreground input lease; measurement-class ones clear the
            // measurement grant and are runnable unattended. An undeclared
            // sub-operation resolves to control and fails closed. Read-only
            // sub-operations (abundance, kernel_answer, olap_aggregate) reach no
            // gate at all, exactly as before.
            if sub_operation.mutates_state() {
                let (class, rationale) =
                    crate::server::tool_profiles::classify_intelligence_operation(sub_operation);
                require_storage_operation_authority(
                    service,
                    &request_context,
                    STORAGE_TOOL,
                    sub_operation.as_str(),
                    class,
                    rationale,
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
            // Mutating intelligence and OLAP can traverse large physical
            // families, so they retain the exclusive whole-corpus ownership
            // required by #2243. Abundance and kernel_answer are different by
            // contract: both are read-only and hydrate at most the validated
            // `max_records` bound (<=20,000). Queueing those bounded foreground
            // reads behind a multi-minute scheduled pass made the public Calyx
            // surface time out before its actual work began. They use a
            // separate single-permit bounded-read lane; causal_map_read joins
            // it because it re-fingerprints at most the persisted 20,000-row
            // source contract and never reruns an estimator. No second
            // whole-corpus working set is admitted.
            let bounded_read = sub_operation.is_bounded_read();
            let work = move || {
                use crate::m3::storage::{
                    StorageIntelligenceOperation, StorageIntelligenceResponse,
                };
                let base = StorageIntelligenceResponse {
                    operation: sub_operation,
                    weave: None,
                    abundance: None,
                    bits: None,
                    sufficiency: None,
                    redundancy: None,
                    synergy: None,
                    causality: None,
                    causal_map: None,
                    periodicity: None,
                    drift: None,
                    hazard: None,
                    kernel: None,
                    kernel_answer: None,
                    oracle: None,
                    ensemble_card: None,
                    olap_aggregate: None,
                    search_commission: None,
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
                        crate::m3::storage::run_intelligence_abundance(&db, &spec).map(
                            |abundance| StorageIntelligenceResponse {
                                abundance: Some(abundance),
                                ..base
                            },
                        )
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
                    StorageIntelligenceOperation::CausalMap => {
                        crate::m3::storage::run_intelligence_causal_map(&db, &spec).map(
                            |causal_map| StorageIntelligenceResponse {
                                causal_map: Some(causal_map),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::CausalMapRead => {
                        crate::m3::storage::run_intelligence_causal_map_read(&db, &spec).map(
                            |causal_map| StorageIntelligenceResponse {
                                causal_map: Some(causal_map),
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
                    StorageIntelligenceOperation::OraclePredict => {
                        crate::m3::storage::run_intelligence_oracle_predict(&db, &spec).map(
                            |oracle| StorageIntelligenceResponse {
                                oracle: Some(oracle),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::OracleReverse => {
                        crate::m3::storage::run_intelligence_oracle_reverse(&db, &spec).map(
                            |oracle| StorageIntelligenceResponse {
                                oracle: Some(oracle),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::OracleComplete => {
                        crate::m3::storage::run_intelligence_oracle_complete(&db, &spec).map(
                            |oracle| StorageIntelligenceResponse {
                                oracle: Some(oracle),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::OracleValidate => {
                        crate::m3::storage::run_intelligence_oracle_validate(&db, &spec).map(
                            |oracle| StorageIntelligenceResponse {
                                oracle: Some(oracle),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::OracleReadiness => {
                        crate::m3::storage::run_intelligence_oracle_readiness(&db, &spec).map(
                            |oracle| StorageIntelligenceResponse {
                                oracle: Some(oracle),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::EnsembleCard => {
                        crate::m3::storage::run_intelligence_ensemble_card(&db, &spec).map(
                            |ensemble_card| StorageIntelligenceResponse {
                                ensemble_card: Some(ensemble_card),
                                ..base
                            },
                        )
                    }
                    StorageIntelligenceOperation::OlapAggregate => {
                        let slot = spec.content_slot.ok_or_else(|| {
                            crate::m1::mcp_error(
                                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                                "intelligence olap_aggregate requires content_slot",
                            )
                        })?;
                        let value_column = spec.value_column.ok_or_else(|| {
                            crate::m1::mcp_error(
                                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                                "intelligence olap_aggregate requires value_column",
                            )
                        })?;
                        db.olap_aggregate_slot(
                            spec.panel_version,
                            slot,
                            value_column as usize,
                            spec.group_by_column.map(|value| value as usize),
                            spec.olap_max_rows.unwrap_or(1_000_000) as usize,
                            spec.olap_max_groups.unwrap_or(4_096) as usize,
                        )
                        .map_err(|error| crate::m1::mcp_error(error.code(), error.to_string()))
                        .and_then(|report| {
                            serde_json::to_value(report).map_err(|error| {
                                crate::m1::mcp_error(
                                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                                    format!("serialize native OLAP aggregate: {error}"),
                                )
                            })
                        })
                        .map(|olap_aggregate| {
                            StorageIntelligenceResponse {
                                olap_aggregate: Some(olap_aggregate),
                                ..base
                            }
                        })
                    }
                    StorageIntelligenceOperation::SearchKernelCommission => {
                        crate::m3::storage::run_intelligence_search_kernel_commission(&db, &spec)
                            .map(|search_commission| StorageIntelligenceResponse {
                                search_commission: Some(search_commission),
                                ..base
                            })
                    }
                }
            };
            let response = if bounded_read {
                Box::pin(
                    synapse_storage::maintenance::run_admitted_bounded_read_preserving_error(
                        "storage_intelligence_bounded_read",
                        work,
                    ),
                )
                .await
            } else {
                Box::pin(
                    synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
                        "storage_intelligence",
                        work,
                    ),
                )
                .await
            }
            .map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    &source_id,
                    STORAGE_SOT,
                    storage_error_data(&error),
                    "inspect daemon STORAGE_MAINTENANCE_* or STORAGE_BOUNDED_READ_* admission/completion records and the named intelligence error",
                )
            })??;
            let summary = if let Some(weave) = &response.weave {
                format!(
                    "intelligence weave panel={} records_woven={} records_removed={} cross_terms={} agreement_edges={} between_record_graph_reference_sha256={} xterm_rows={} graph_rows={} dda_signal_yield={} blind_spot_pairs={}/{} blind_spot_records={} outside_window={}",
                    weave.panel_version,
                    weave.records_woven,
                    weave.records_removed,
                    weave.cross_terms_materialized,
                    weave.agreement_edges_persisted,
                    weave.between_record_graph_reference_sha256,
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
                    "intelligence bits panel={} anchor={} anchored_records={} distinct_outcomes={} measurable={} unmeasurable_reason={} total_bits={:.4} grounded={} domain_provisional={} domain_grounded_fraction={:.4} slots={} assay_rows={}",
                    bits.panel_version,
                    bits.anchor_kind,
                    bits.anchored_records,
                    bits.distinct_outcomes,
                    bits.measurable,
                    bits.unmeasurable_reason.as_deref().unwrap_or("none"),
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
                    "intelligence redundancy panel={} n_lenses={} effective_rank={:.4} over_lenses=[{}] pairs_possible={} pairs_evaluated={} pairs_skipped={} skipped=[{}] low_signal_lenses=[{}] redundant_pairs={} domain_provisional={} domain_grounded_fraction={:.4} assay_rows={}",
                    redundancy.panel_version,
                    redundancy.n_lenses,
                    redundancy.effective_rank,
                    redundancy
                        .effective_rank_slots
                        .iter()
                        .zip(&redundancy.effective_rank_lenses)
                        .map(|(slot, lens)| format!("{slot}:{lens}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    redundancy.pairs_possible,
                    redundancy.pairs_evaluated,
                    redundancy.pairs_skipped,
                    redundancy
                        .skipped_details
                        .iter()
                        .map(|skip| format!(
                            "{}({}) x {}({}) reason={} offending_slot={} n_paired={}",
                            skip.slot_a,
                            skip.lens_a,
                            skip.slot_b,
                            skip.lens_b,
                            skip.reason,
                            skip.offending_slot
                                .map_or_else(|| "none".to_owned(), |slot| slot.to_string()),
                            skip.n_paired
                        ))
                        .collect::<Vec<_>>()
                        .join(" | "),
                    redundancy
                        .low_signal_lenses
                        .iter()
                        .map(|lens| format!(
                            "{}({}) {} constant_value={} records={}",
                            lens.slot,
                            lens.lens,
                            lens.code,
                            lens.constant_value,
                            lens.records_observed
                        ))
                        .collect::<Vec<_>>()
                        .join(" | "),
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
            } else if let Some(causal_map) = &response.causal_map {
                let streams = causal_map.artifact["streams"]
                    .as_array()
                    .map_or(0, Vec::len);
                let pairs = causal_map.artifact["pairs"].as_array().map_or(0, Vec::len);
                format!(
                    "intelligence {} streams={} pairs={} evidence_class={} structural_identified={} graph_key={} sha256={} bytes={} pointer_key={} pointer_sha256={} pointer_bytes={} artifact_readback_match={} pointer_readback_match={} graph_rows={}",
                    response.operation.as_str(),
                    streams,
                    pairs,
                    causal_map.artifact["evidence_class"]
                        .as_str()
                        .unwrap_or("unknown"),
                    causal_map.artifact["structural_effect_identified"]
                        .as_bool()
                        .unwrap_or(false),
                    causal_map.graph_key_hex,
                    causal_map.graph_value_sha256,
                    causal_map.graph_value_bytes,
                    causal_map.pointer_key_hex,
                    causal_map.pointer_value_sha256,
                    causal_map.pointer_value_bytes,
                    causal_map.physical_readback_matches,
                    causal_map.pointer_readback_matches,
                    causal_map.graph_cf_rows_after,
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
                    "intelligence drift panel={} n_occurrences={} n_distinct_instants={} ties_collapsed={} max_multiplicity={} n_gaps={} cusum_change_detected={} direction={:?} mmd_p_value={:?} temporal_xterm_rows={}",
                    drift.panel_version,
                    drift.n_occurrences,
                    drift.n_distinct_instants,
                    drift.ties_collapsed,
                    drift.max_multiplicity,
                    drift.n_gaps,
                    drift.cusum_change_detected,
                    drift.cusum_direction,
                    drift.mmd_p_value,
                    drift.temporal_xterm_cf_rows_after,
                )
            } else if let Some(hazard) = &response.hazard {
                format!(
                    "intelligence hazard panel={} n_occurrences={} n_distinct_instants={} ties_collapsed={} max_multiplicity={} n_gaps={} survival={:.4} overdue={} expected_next_seconds={:.1} temporal_xterm_rows={}",
                    hazard.panel_version,
                    hazard.n_occurrences,
                    hazard.n_distinct_instants,
                    hazard.ties_collapsed,
                    hazard.max_multiplicity,
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
                out.intelligence = Some(Box::new(response));
            })))
        }
        StorageOperation::GcOnce => {
            let spec = params
                .0
                .gc_once
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "gc_once"))?;
            Box::pin(handle_gc_once(service, &request_context, spec)).await
        }
    }
}

async fn handle_corpus_histogram(
    service: &SynapseService,
    spec: crate::m3::storage::StorageCorpusHistogramParams,
) -> Result<Json<StorageResponse>, ErrorData> {
    let operation = StorageOperation::CorpusHistogram;
    service.require_m3_permissions(
        STORAGE_TOOL,
        &crate::m3::storage::required_permissions_corpus_histogram(&spec),
    )?;
    let db = service.m3_storage().map_err(|error| {
        facade_delegate_error(
            STORAGE_TOOL,
            operation.as_str(),
            "calyx_storage",
            STORAGE_SOT,
            error,
            "repair storage/Calyx initialization and retry storage operation=corpus_histogram",
        )
    })?;
    let response = synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
        "storage_corpus_histogram",
        move || crate::m3::storage::inspect_corpus_histogram(&db, &spec),
    )
    .await
    .map_err(|error| {
        facade_delegate_error(
            STORAGE_TOOL,
            operation.as_str(),
            "calyx_storage",
            STORAGE_SOT,
            storage_error_data(&error),
            "inspect daemon STORAGE_MAINTENANCE_* admission/completion records and the named corpus-histogram error",
        )
    })?
    .map_err(|error| {
        facade_delegate_error(
            STORAGE_TOOL,
            operation.as_str(),
            "calyx_storage",
            STORAGE_SOT,
            error,
            "name a supported source_cf and declared dimensions; a row that will not decode is counted, not skipped",
        )
    })?;
    Ok(Json(storage_response(
        operation,
        format!(
            "{} rows_scanned={} rows_decoded={} decode_failures={} complete={}",
            response.source_cf,
            response.rows_scanned,
            response.rows_decoded,
            response.decode_failures,
            response.complete
        ),
        |out| out.corpus_histogram = Some(response),
    )))
}

fn handle_snapshot(
    service: &SynapseService,
    mut params: StorageParams,
    operation: StorageOperation,
) -> Result<Json<StorageResponse>, ErrorData> {
    match operation {
        StorageOperation::SnapshotGcStatus => {
            let spec = params.snapshot_gc_status.take().unwrap_or_default();
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_inspect(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "snapshot_gc_status",
                    STORAGE_SOT,
                    error,
                    "repair storage initialization and read the live Calyx vault counters again",
                )
            })?;
            let response =
                crate::m3::storage::inspect_snapshot_gc_status(&db).map_err(|error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        "snapshot_gc_status",
                        STORAGE_SOT,
                        error,
                        "inspect the live Calyx vault lifecycle state and retry the counter read",
                    )
                })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "snapshot GC floor_seq={} current_seq={} versions_reclaimed_total={} bytes_reclaimed_total={}",
                    response.floor_seq,
                    response.current_seq,
                    response.versions_reclaimed_total,
                    response.bytes_reclaimed_total,
                ),
                |out| out.snapshot_gc_status = Some(response),
            )))
        }
        StorageOperation::SnapshotOpen => {
            let spec = params
                .snapshot_open
                .take()
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "snapshot_open"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_snapshot_open(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_mvcc_snapshot_leases",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=snapshot_open",
                )
            })?;
            let response =
                crate::m3::storage::open_storage_snapshot(&db, &spec).map_err(|error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        "calyx_mvcc_snapshot_leases",
                        STORAGE_SOT,
                        error,
                        "request max_age_ms within 100..=60000 and release prior leases before retrying",
                    )
                })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Aster reader lease id={} snapshot_seq={} expires_at_unix_ms={} active_leases={}",
                    response.lease_id,
                    response.snapshot_seq,
                    response.expires_at_unix_ms,
                    response.active_lease_count
                ),
                |out| out.snapshot_open = Some(response),
            )))
        }
        StorageOperation::SnapshotRead => {
            let spec = params
                .snapshot_read
                .take()
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "snapshot_read"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_snapshot_read(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_mvcc_snapshot_leases",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=snapshot_read",
                )
            })?;
            let response =
                crate::m3::storage::read_storage_snapshot(&db, &spec).map_err(|error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        &spec.cf_name,
                        STORAGE_SOT,
                        error,
                        "pass an active lease_id plus one exact logical CF/key before the lease expires",
                    )
                })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Aster snapshot lease={} seq={} current_seq={} logical_cf={} key={} physical_present={} logical_present={} payload_sha256={}",
                    response.lease_id,
                    response.snapshot_seq,
                    response.current_seq,
                    response.cf_name,
                    response.key_hex,
                    response.physical_present,
                    response.logical_present,
                    response.payload_sha256.as_deref().unwrap_or("absent")
                ),
                |out| out.snapshot_read = Some(response),
            )))
        }
        StorageOperation::SnapshotRelease => {
            let spec = params
                .snapshot_release
                .take()
                .ok_or_else(|| missing_spec(STORAGE_TOOL, "snapshot_release"))?;
            service.require_m3_permissions(
                STORAGE_TOOL,
                &crate::m3::storage::required_permissions_snapshot_release(&spec),
            )?;
            let db = service.m3_storage().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "calyx_mvcc_snapshot_leases",
                    STORAGE_SOT,
                    error,
                    "repair storage/Calyx initialization and retry storage operation=snapshot_release",
                )
            })?;
            let response =
                crate::m3::storage::release_storage_snapshot(&db, &spec).map_err(|error| {
                    facade_delegate_error(
                        STORAGE_TOOL,
                        operation.as_str(),
                        "calyx_mvcc_snapshot_leases",
                        STORAGE_SOT,
                        error,
                        "release the exact active lease_id once, before its reported expiry",
                    )
                })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "Aster reader lease id={} snapshot_seq={} released={} active_leases={} current_seq={}",
                    response.lease_id,
                    response.snapshot_seq,
                    response.released,
                    response.active_lease_count,
                    response.current_seq
                ),
                |out| out.snapshot_release = Some(response),
            )))
        }
        _ => Err(ErrorData::internal_error(
            "SYNAPSE_STORAGE_SNAPSHOT_DISPATCH_INVALID: non-snapshot operation reached the snapshot handler; remediation=inspect storage operation routing",
            None,
        )),
    }
}

async fn handle_gc_once(
    service: &SynapseService,
    request_context: &RequestContext<RoleServer>,
    spec: crate::m3::storage::StorageGcOnceParams,
) -> Result<Json<StorageResponse>, ErrorData> {
    let operation = StorageOperation::GcOnce;
    require_maintenance_profile(
        service,
        request_context,
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
    let source_id = spec.cf_name.clone();
    let response = synapse_storage::maintenance::run_admitted_maintenance_preserving_error(
        "storage_gc_once",
        move || crate::m3::storage::run_storage_gc_once(&db, &spec),
    )
    .await
    .map_err(|error| {
        facade_delegate_error(
            STORAGE_TOOL,
            operation.as_str(),
            &source_id,
            STORAGE_SOT,
            storage_error_data(&error),
            "inspect daemon STORAGE_MAINTENANCE_* admission/completion records and the named GC error",
        )
    })?
    .map_err(|error| {
        facade_delegate_error(
            STORAGE_TOOL,
            operation.as_str(),
            &source_id,
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

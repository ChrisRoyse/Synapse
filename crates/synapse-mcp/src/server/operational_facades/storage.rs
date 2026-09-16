use rmcp::{RoleServer, service::RequestContext};

use crate::server::{ErrorData, Json, Parameters, SynapseService};

use super::{
    STORAGE_SOT, STORAGE_TOOL,
    errors::{facade_delegate_error, missing_spec},
    policy::require_maintenance_profile,
    response::storage_response,
    types::{StorageOperation, StorageParams, StorageResponse},
    validation::validate_storage_params,
};
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
            let runtime = service.reflex_runtime().map_err(|error| {
                facade_delegate_error(
                    STORAGE_TOOL,
                    operation.as_str(),
                    "reflex_runtime",
                    STORAGE_SOT,
                    error,
                    "repair storage/reflex initialization and retry storage operation=inspect",
                )
            })?;
            let response =
                crate::m3::storage::inspect_storage(&runtime, &spec).map_err(|error| {
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
                    "RocksDB CF rows={} pressure={}",
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
                    "repair storage initialization and read RocksDB CF metadata again",
                )
            })?;
            Ok(Json(storage_response(
                operation,
                format!(
                    "RocksDB summary cf_count={} pressure={}",
                    response.cf_row_counts.len(),
                    response.pressure_level.name
                ),
                |out| out.summary = Some(response),
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
            let runtime = service.reflex_runtime()?;
            let response =
                crate::m3::storage::run_storage_gc_once(&runtime, &spec).map_err(|error| {
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

use rmcp::{RoleServer, service::RequestContext};

use crate::server::{
    ErrorData, SynapseService,
    tool_profiles::{
        MEASURE_INTELLIGENCE_CAPABILITY, MEASURE_INTELLIGENCE_GRANT_NAMES,
        STORAGE_OPERATION_CLASS_SOURCE_OF_TRUTH, StorageOperationClass, ToolProfileKind,
        measure_intelligence_grant,
    },
};

use super::errors::facade_policy_error;
pub(super) fn session_or_stdio(
    request_context: &RequestContext<RoleServer>,
) -> Result<String, ErrorData> {
    Ok(
        crate::server::context::mcp_session_id_from_request_context(request_context)?
            .unwrap_or_else(|| "stdio".to_owned()),
    )
}

/// The single admission gate for a state-changing storage/hygiene operation,
/// routed by its declared [`StorageOperationClass`].
///
/// * `Control` -> unchanged: an explicit maintenance profile, which can only be
///   entered while holding the foreground input lease with `confirm_break_glass`
///   and a reason.
/// * `Measurement` -> the [`MEASURE_INTELLIGENCE_CAPABILITY`] grant
///   (`READ_STORAGE` + `WRITE_STORAGE`), no profile requirement and no
///   foreground lease, so an unattended `normal_agent`/scheduled session can run
///   it. The admission is logged with the session, profile, grant, sub-operation
///   and the classification's declared rationale.
///
/// The class is never inferred here; callers pass what
/// [`crate::server::tool_profiles::INTELLIGENCE_OPERATION_CLASSES`] declares, and
/// an undeclared operation resolves to `Control` (#2077).
///
/// # Errors
///
/// Returns the structured facade policy error when a control-class operation is
/// attempted without a maintenance profile, or the M3 authorization error when a
/// measurement-class operation is attempted without the measurement grant.
pub(super) fn require_storage_operation_authority(
    service: &SynapseService,
    request_context: &RequestContext<RoleServer>,
    tool: &'static str,
    operation: &'static str,
    class: StorageOperationClass,
    rationale: &'static str,
    source_id: &str,
    source_of_truth: &'static str,
) -> Result<(), ErrorData> {
    match class {
        StorageOperationClass::Control => require_maintenance_profile(
            service,
            request_context,
            tool,
            operation,
            source_id,
            source_of_truth,
        ),
        StorageOperationClass::Measurement => {
            // Fail closed on the grant before anything reads the corpus. This is
            // checked here rather than relying on the per-operation permission
            // helper so the gate cannot be defeated by a later change to what
            // that helper happens to require.
            service.require_m3_permissions(tool, &measure_intelligence_grant())?;
            let session_id =
                crate::server::context::mcp_session_id_from_request_context(request_context)?;
            let profile = service
                .tool_profile_snapshot(session_id.as_deref())?
                .profile;
            tracing::info!(
                code = "MCP_STORAGE_MEASUREMENT_CLASS_ADMITTED",
                tool,
                operation,
                source_id = %source_id,
                classification = class.as_str(),
                classification_rationale = rationale,
                classification_source_of_truth = STORAGE_OPERATION_CLASS_SOURCE_OF_TRUTH,
                grant = MEASURE_INTELLIGENCE_CAPABILITY,
                granted_permissions = MEASURE_INTELLIGENCE_GRANT_NAMES,
                profile = profile.as_str(),
                session_id = session_id.as_deref().unwrap_or("stdio"),
                break_glass_required = false,
                foreground_input_lease_required = false,
                source_of_truth,
                "measurement-class storage operation admitted without break_glass or the foreground input lease (#2077); evidence gates are unchanged"
            );
            Ok(())
        }
    }
}

pub(crate) fn require_maintenance_profile(
    service: &SynapseService,
    request_context: &RequestContext<RoleServer>,
    tool: &'static str,
    operation: &'static str,
    source_id: &str,
    source_of_truth: &'static str,
) -> Result<(), ErrorData> {
    let session_id = crate::server::context::mcp_session_id_from_request_context(request_context)?;
    let snapshot = service.tool_profile_snapshot(session_id.as_deref())?;
    if snapshot.profile.allows_maintenance_mutation() {
        return Ok(());
    }
    let valid_target_profiles =
        ToolProfileKind::MAINTENANCE_AUTHORIZED.map(ToolProfileKind::as_str);
    let unmet_prerequisites = [
        "foreground_input_lease",
        "confirm_break_glass_true",
        "non_empty_reason",
    ];
    Err(facade_policy_error(
        tool,
        operation,
        source_id,
        snapshot.profile,
        source_of_truth,
        "storage_maintenance_mutation",
        &valid_target_profiles,
        &unmet_prerequisites,
        "call act operation=lease_acquire; then call profile operation=set with profile=break_glass, confirm_break_glass=true, and a non-empty reason; retry the mutating operation; finally restore profile=normal_agent and release the foreground lease",
    ))
}

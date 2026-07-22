use rmcp::{RoleServer, service::RequestContext};

use crate::server::{ErrorData, SynapseService, tool_profiles::ToolProfileKind};

use super::errors::facade_policy_error;
pub(super) fn session_or_stdio(
    request_context: &RequestContext<RoleServer>,
) -> Result<String, ErrorData> {
    Ok(
        crate::server::context::mcp_session_id_from_request_context(request_context)?
            .unwrap_or_else(|| "stdio".to_owned()),
    )
}

pub(super) fn require_maintenance_profile(
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

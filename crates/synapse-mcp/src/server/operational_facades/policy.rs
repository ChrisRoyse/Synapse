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
    if matches!(
        snapshot.profile,
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability
    ) {
        return Ok(());
    }
    Err(facade_policy_error(
        tool,
        operation,
        source_id,
        snapshot.profile,
        source_of_truth,
        "switch to an explicit maintenance profile with operator intent before running this mutating operation; normal_agent may use the read-only operation first",
    ))
}

use rmcp::model::ErrorCode;
use serde_json::{Value, json};
use synapse_core::error_codes;

use crate::server::{ErrorData, tool_profiles::ToolProfileKind};
pub(super) fn missing_spec(tool: &'static str, operation: &'static str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("{tool} operation={operation} missing operation payload"),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_of_truth": "MCP request parameters",
            "source_id": operation,
            "remediation": "pass the payload object matching operation",
        })),
    )
}

pub(super) fn facade_policy_error(
    tool: &'static str,
    operation: &'static str,
    source_id: &str,
    profile: ToolProfileKind,
    source_of_truth: &'static str,
    required_capability: &'static str,
    valid_target_profiles: &[&'static str],
    unmet_prerequisites: &[&'static str],
    remediation: &'static str,
) -> ErrorData {
    tracing::warn!(
        code = error_codes::TOOL_PROFILE_POLICY_DENIED,
        tool,
        operation,
        source_id,
        current_profile = profile.as_str(),
        required_capability,
        valid_target_profiles = ?valid_target_profiles,
        unmet_prerequisites = ?unmet_prerequisites,
        "facade operation denied by tool-profile capability policy"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} operation={operation} is not allowed for profile {}",
            profile.as_str()
        ),
        Some(json!({
            "code": error_codes::TOOL_PROFILE_POLICY_DENIED,
            "tool": tool,
            "operation": operation,
            "source_id": source_id,
            "profile": profile.as_str(),
            "current_profile": profile.as_str(),
            "required_capability": required_capability,
            "valid_target_profiles": valid_target_profiles,
            "unmet_prerequisites": unmet_prerequisites,
            "source_of_truth": source_of_truth,
            "remediation": remediation,
        })),
    )
}

pub(super) fn facade_conflict_error(
    tool: &'static str,
    operation: &'static str,
    source_id: &str,
    source_of_truth: &'static str,
    code: &'static str,
    message: String,
    remediation: &'static str,
) -> ErrorData {
    tracing::warn!(
        code,
        tool,
        operation,
        source_id,
        "facade operation refused: conflicting in-flight operation on the same source of truth"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("{tool} operation={operation} refused for {source_id}: {message}"),
        Some(json!({
            "code": code,
            "tool": tool,
            "operation": operation,
            "source_id": source_id,
            "source_of_truth": source_of_truth,
            "remediation": remediation,
        })),
    )
}

/// Wraps a delegated failure in this facade's envelope.
///
/// The delegate's **own** remediation wins when it has one (#1911). This
/// function already read the delegate's `code` off its data and fell back only
/// when absent; `remediation` is the adjacent field of the same
/// `{code, message, remediation}` contract and was being overwritten
/// unconditionally, so every delegated failure across the 40-tool surface
/// reported its facade's generic sentence regardless of what actually broke.
///
/// The facade's own string is not discarded — it answers a different question
/// ("what does this tool need from you") than the substrate's ("what is
/// broken"), so it is reported alongside as `facade_remediation` and remains the
/// fallback for delegates that carry no remediation of their own.
pub(super) fn facade_delegate_error(
    tool: &'static str,
    operation: &'static str,
    source_id: &str,
    source_of_truth: &'static str,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let delegate_remediation = remediation_from(&error);
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} operation={operation} failed for {source_id}: {}",
            error.message
        ),
        Some(json!({
            "code": error_code_from(&error),
            "tool": tool,
            "operation": operation,
            "source_id": source_id,
            "source_of_truth": source_of_truth,
            "remediation": delegate_remediation.as_deref().unwrap_or(remediation),
            "facade_remediation": remediation,
            "cause": {
                "message": error.message.to_string(),
                "data": error.data,
            },
        })),
    )
}

fn error_code_from(error: &ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned()
}

/// The delegate's own remediation, when it carried one.
///
/// An empty or whitespace-only string is treated as absent: it would satisfy
/// the contract's shape while telling the operator nothing, and silently
/// preferring it over the facade's real guidance would be a regression dressed
/// as a fix.
fn remediation_from(error: &ErrorData) -> Option<String> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("remediation"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

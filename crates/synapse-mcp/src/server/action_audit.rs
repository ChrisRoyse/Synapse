use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::{ErrorData, RoleServer, service::RequestContext};
use serde::Serialize;
use serde_json::{Value, json};

use super::SynapseService;
use crate::m1::mcp_error;
use crate::server::url_redaction::redact_url_fields_for_public_readback;

static ACTION_AUDIT_SEQ: AtomicU32 = AtomicU32::new(0);

impl SynapseService {
    pub(super) fn audit_action_started_for_request(
        &self,
        tool: &'static str,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let session_id = action_session_id_from_request_context(request_context)?;
        self.write_action_audit_row(tool, "started", None, &json!({}), session_id.as_deref())
    }

    pub(super) fn audit_action_started_with_details(
        &self,
        tool: &'static str,
        details: &Value,
    ) -> Result<(), ErrorData> {
        self.write_action_audit_row(tool, "started", None, details, None)
    }

    pub(super) fn audit_action_started_with_details_for_request(
        &self,
        tool: &'static str,
        details: &Value,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let session_id = action_session_id_from_request_context(request_context)?;
        self.write_action_audit_row(tool, "started", None, details, session_id.as_deref())
    }

    pub(super) fn audit_action_started_with_details_for_session(
        &self,
        tool: &'static str,
        details: &Value,
        session_id: &str,
    ) -> Result<(), ErrorData> {
        self.write_action_audit_row(tool, "started", None, details, Some(session_id))
    }

    pub(super) fn audit_action_denied(&self, tool: &'static str, error: &ErrorData) {
        self.audit_action_denied_with_details(tool, error, &json!({}));
    }

    pub(super) fn audit_action_denied_for_request(
        &self,
        tool: &'static str,
        error: &ErrorData,
        request_context: &RequestContext<RoleServer>,
    ) {
        self.audit_action_denied_with_details_for_request(tool, error, &json!({}), request_context);
    }

    pub(super) fn audit_action_denied_with_details(
        &self,
        tool: &'static str,
        error: &ErrorData,
        details: &Value,
    ) {
        if let Err(audit_error) = self.write_action_audit_row(
            tool,
            "denied",
            error_data_code(error),
            &json!({
                "message": error.message.to_string(),
                "data": error.data.clone(),
                "request": details,
            }),
            None,
        ) {
            tracing::warn!(
                code = "ACTION_AUDIT_WRITE_FAILED",
                tool,
                status = "denied",
                audit_error = %audit_error,
                "action audit write failed after denied action"
            );
        }
    }

    pub(super) fn audit_action_denied_with_details_for_session(
        &self,
        tool: &'static str,
        error: &ErrorData,
        details: &Value,
        session_id: &str,
    ) {
        if let Err(audit_error) = self.write_action_audit_row(
            tool,
            "denied",
            error_data_code(error),
            &json!({
                "message": error.message.to_string(),
                "data": error.data.clone(),
                "request": details,
            }),
            Some(session_id),
        ) {
            tracing::warn!(
                code = "ACTION_AUDIT_WRITE_FAILED",
                tool,
                status = "denied",
                audit_error = %audit_error,
                "action audit write failed after denied action"
            );
        }
    }

    pub(super) fn audit_action_denied_with_details_for_request(
        &self,
        tool: &'static str,
        error: &ErrorData,
        details: &Value,
        request_context: &RequestContext<RoleServer>,
    ) {
        let session_id = match action_session_id_from_request_context(request_context) {
            Ok(session_id) => session_id,
            Err(session_error) => {
                tracing::warn!(
                    code = "ACTION_AUDIT_SESSION_ID_READ_FAILED",
                    tool,
                    source_error = %session_error.message,
                    source_error_data = ?session_error.data,
                    "action audit could not read request MCP session id for denied action"
                );
                None
            }
        };
        if let Err(audit_error) = self.write_action_audit_row(
            tool,
            "denied",
            error_data_code(error),
            &json!({
                "message": error.message.to_string(),
                "data": error.data.clone(),
                "request": details,
            }),
            session_id.as_deref(),
        ) {
            tracing::warn!(
                code = "ACTION_AUDIT_WRITE_FAILED",
                tool,
                status = "denied",
                audit_error = %audit_error,
                "action audit write failed after denied action"
            );
        }
    }

    pub(super) fn audit_action_result<T: Serialize>(
        &self,
        tool: &'static str,
        result: &Result<T, ErrorData>,
    ) -> Result<(), ErrorData> {
        match result {
            Ok(response) => self.write_action_audit_row(
                tool,
                "ok",
                None,
                &json!({
                    "response": response,
                }),
                None,
            ),
            Err(error) => self.write_action_audit_row(
                tool,
                "error",
                error_data_code(error),
                &json!({
                    "message": error.message.to_string(),
                    "data": error.data.clone(),
                }),
                None,
            ),
        }
    }

    pub(super) fn audit_action_result_for_request<T: Serialize>(
        &self,
        tool: &'static str,
        result: &Result<T, ErrorData>,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let session_id = action_session_id_from_request_context(request_context)?;
        match session_id.as_deref() {
            Some(session_id) => self.audit_action_result_for_session(tool, result, session_id),
            None => self.audit_action_result(tool, result),
        }
    }

    pub(super) fn audit_action_result_for_session<T: Serialize>(
        &self,
        tool: &'static str,
        result: &Result<T, ErrorData>,
        session_id: &str,
    ) -> Result<(), ErrorData> {
        match result {
            Ok(response) => self.write_action_audit_row(
                tool,
                "ok",
                None,
                &json!({
                    "response": response,
                }),
                Some(session_id),
            ),
            Err(error) => self.write_action_audit_row(
                tool,
                "error",
                error_data_code(error),
                &json!({
                    "message": error.message.to_string(),
                    "data": error.data.clone(),
                }),
                Some(session_id),
            ),
        }
    }

    pub(super) fn audit_action_redacted_result_for_session(
        &self,
        tool: &'static str,
        status: &'static str,
        error_code: Option<&str>,
        details: &Value,
        session_id: &str,
    ) -> Result<(), ErrorData> {
        self.write_action_audit_row(tool, status, error_code, details, Some(session_id))
    }

    pub(super) fn audit_action_ok_with_details_for_session(
        &self,
        tool: &'static str,
        details: &Value,
        session_id: &str,
    ) -> Result<(), ErrorData> {
        self.write_action_audit_row(tool, "ok", None, details, Some(session_id))
    }

    pub(super) fn audit_action_ok_with_details_for_request(
        &self,
        tool: &'static str,
        details: &Value,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let session_id = action_session_id_from_request_context(request_context)?;
        self.write_action_audit_row(tool, "ok", None, details, session_id.as_deref())
    }

    pub(super) fn audit_action_error_with_details_for_request(
        &self,
        tool: &'static str,
        error: &ErrorData,
        details: &Value,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let session_id = action_session_id_from_request_context(request_context)?;
        self.write_action_audit_row(
            tool,
            "error",
            error_data_code(error),
            &json!({
                "message": error.message.to_string(),
                "data": error.data.clone(),
                "request": details,
            }),
            session_id.as_deref(),
        )
    }

    pub(super) fn audit_action_error_with_details_for_session(
        &self,
        tool: &'static str,
        error: &ErrorData,
        details: &Value,
        session_id: &str,
    ) -> Result<(), ErrorData> {
        self.write_action_audit_row(
            tool,
            "error",
            error_data_code(error),
            &json!({
                "message": error.message.to_string(),
                "data": error.data.clone(),
                "request": details,
            }),
            Some(session_id),
        )
    }

    pub(super) fn audit_action_result_for_request_best_effort<T: Serialize>(
        &self,
        tool: &'static str,
        result: &Result<T, ErrorData>,
        request_context: &RequestContext<RoleServer>,
    ) {
        if let Err(error) = self.audit_action_result_for_request(tool, result, request_context) {
            tracing::warn!(
                code = "ACTION_AUDIT_WRITE_FAILED",
                tool,
                audit_error = %error,
                "action audit write failed after request-scoped action result"
            );
        }
    }

    fn write_action_audit_row(
        &self,
        tool: &'static str,
        status: &'static str,
        error_code: Option<&str>,
        details: &Value,
        action_session_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        let (ts_ns, seq) = next_audit_key_parts();
        let active_profile = self.action_audit_active_profile();
        let mut audit_context = self.current_action_audit_context()?;
        let action_session_id = action_session_id
            .map(str::to_owned)
            .or_else(crate::http::current_mcp_session_id);
        if let Some(action_session_id) = action_session_id {
            audit_context.session_id = Some(action_session_id);
        }
        let session_id = audit_context.session_id.clone();
        let profile_id = audit_context.profile_id.clone();
        let profile_version = audit_context.profile_version.clone();
        let profile_schema_version = audit_context.profile_schema_version;
        let foreground_tier =
            self.action_audit_foreground_tier(tool, status, session_id.as_deref(), details);
        let human_os_foreground = self.action_audit_foreground();
        let mut details = details.clone();
        let url_redaction_count = redact_url_fields_for_public_readback(&mut details);
        let mut redactions = action_audit_detail_redactions(&details);
        if url_redaction_count > 0 {
            redactions.push("url_fields".to_owned());
        }
        redactions.sort();
        redactions.dedup();
        let redacted = !redactions.is_empty();
        let value = json!({
            "schema_version": 1,
            "audit_id": format!("{ts_ns:020}-{seq:010}"),
            "ts_ns": ts_ns,
            "seq": seq,
            "session_id": session_id,
            "profile_id": profile_id,
            "profile_version": profile_version,
            "profile_schema_version": profile_schema_version,
            "audit_context": audit_context,
            "tool": tool,
            "status": status,
            "error_code": error_code,
            "foreground": human_os_foreground,
            "human_os_foreground": human_os_foreground,
            "agent_logical_foreground": self.action_audit_agent_logical_foreground(session_id.as_deref()),
            "foreground_lane": self.action_audit_foreground_lane(session_id.as_deref()),
            "foreground_tier": foreground_tier,
            "active_profile_id": active_profile.profile_id,
            "active_profile_schema_version": active_profile.schema_version,
            "redacted": redacted,
            "redactions": redactions,
            "details": details,
        });
        let encoded = synapse_storage::encode_json(&value).map_err(|error| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                format!("action audit row encode failed: {error}"),
            )
        })?;
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "reflex runtime lock poisoned while writing action audit",
            )
        })?;
        runtime
            .storage_put_action_log_rows(vec![(action_audit_key(ts_ns, seq), encoded)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        drop(runtime);
        tracing::info!(
            code = "ACTION_AUDIT_RECORDED",
            tool,
            status,
            ts_ns,
            seq,
            "action audit row written"
        );
        Ok(())
    }

    fn action_audit_foreground(&self) -> Value {
        match self.current_audit_foreground() {
            Ok(foreground) => {
                let observed_profile = self.action_audit_observed_profile(&foreground);
                json!({
                    "hwnd": foreground.hwnd,
                    "pid": foreground.pid,
                    "process_name": foreground.process_name,
                    "process_path": foreground.process_path,
                    "window_title": foreground.window_title,
                    "profile_id": observed_profile.profile_id,
                    "profile_schema_version": observed_profile.schema_version,
                })
            }
            Err(error) => json!({
                "read_error_code": error_data_code(&error),
                "read_error_message": error.message.to_string(),
            }),
        }
    }

    fn action_audit_agent_logical_foreground(&self, session_id: Option<&str>) -> Value {
        let Some(session_id) = session_id else {
            return json!({
                "source_of_truth": "MCP session id + CF_SESSIONS session-target row + daemon session target registry",
                "status": "missing_session",
                "no_human_os_foreground_fallback": true,
                "missing_reason": "action audit row has no MCP session id",
            });
        };
        let persisted_row_key = format!("mcp/session-target/v1/{session_id}");
        match self.agent_logical_foreground(session_id) {
            Ok(Some(target)) => json!({
                "source_of_truth": format!("CF_SESSIONS row {persisted_row_key} + daemon session target registry; never human OS foreground fallback"),
                "session_id": session_id,
                "status": "set",
                "target": super::target_claims::target_wire(&target),
                "persisted_row_key": persisted_row_key,
                "no_human_os_foreground_fallback": true,
            }),
            Ok(None) => json!({
                "source_of_truth": format!("CF_SESSIONS row {persisted_row_key} + daemon session target registry; never human OS foreground fallback"),
                "session_id": session_id,
                "status": "missing",
                "persisted_row_key": persisted_row_key,
                "no_human_os_foreground_fallback": true,
                "missing_reason": "no session-owned logical foreground target is set",
            }),
            Err(error) => json!({
                "source_of_truth": format!("CF_SESSIONS row {persisted_row_key} + daemon session target registry; never human OS foreground fallback"),
                "session_id": session_id,
                "status": "read_error",
                "persisted_row_key": persisted_row_key,
                "no_human_os_foreground_fallback": true,
                "read_error_code": error_data_code(&error),
                "read_error_message": error.message.to_string(),
            }),
        }
    }

    fn action_audit_foreground_lane(&self, session_id: Option<&str>) -> Value {
        let Some(session_id) = session_id else {
            return json!({
                "source_of_truth": "MCP session id + CF_SESSIONS session-target row + daemon target-claim registry + synapse_action input lease",
                "status": "missing_session",
                "explicit_real_foreground_lease": false,
                "no_human_os_foreground_fallback": true,
                "missing_reason": "action audit row has no MCP session id",
            });
        };
        match self.agent_logical_foreground(session_id) {
            Ok(Some(target)) => {
                let target_key = super::target_claims::target_key(&target);
                let target_wire = super::target_claims::target_wire(&target);
                let target_claim = self
                    .target_claim_status_snapshot()
                    .ok()
                    .and_then(|snapshot| {
                        snapshot
                            .claims
                            .into_iter()
                            .find(|claim| claim.target_key == target_key)
                    });
                let owner_session_id = target_claim
                    .as_ref()
                    .map(|claim| claim.owner_session_id.clone())
                    .unwrap_or_else(|| session_id.to_owned());
                let status = match target_claim.as_ref() {
                    Some(claim) if claim.owner_session_id != session_id => "conflicting_owner",
                    Some(_) => "claimed_by_session",
                    None => "unclaimed_session_target",
                };
                let lane_kind = match target {
                    super::SessionTarget::Window { .. } => "owned_window_target",
                    super::SessionTarget::Cdp { .. } => "owned_chrome_tab_target",
                };
                json!({
                    "source_of_truth": "daemon session target registry + CF_SESSIONS session-target row + daemon target-claim registry + synapse_action input lease",
                    "session_id": session_id,
                    "status": status,
                    "lane_kind": lane_kind,
                    "target_key": target_key,
                    "target": target_wire,
                    "target_claim": target_claim,
                    "owner_session_id": owner_session_id,
                    "explicit_real_foreground_lease": false,
                    "no_human_os_foreground_fallback": true,
                })
            }
            Ok(None) => {
                let lease = synapse_action::lease::status();
                if lease.owner_session_id.as_deref() == Some(session_id) {
                    json!({
                        "source_of_truth": "synapse_action input lease; explicit real OS foreground lease only, no implicit fallback",
                        "session_id": session_id,
                        "status": "explicit_real_foreground_lease",
                        "lane_kind": "real_os_foreground_lease",
                        "owner_session_id": session_id,
                        "explicit_real_foreground_lease": true,
                        "no_human_os_foreground_fallback": true,
                    })
                } else {
                    json!({
                        "source_of_truth": "CF_SESSIONS session-target row + daemon session target registry + synapse_action input lease",
                        "session_id": session_id,
                        "status": "missing",
                        "explicit_real_foreground_lease": false,
                        "no_human_os_foreground_fallback": true,
                        "missing_reason": "no agent logical foreground target and no explicit real foreground lease",
                    })
                }
            }
            Err(error) => json!({
                "source_of_truth": "CF_SESSIONS session-target row + daemon session target registry + synapse_action input lease",
                "session_id": session_id,
                "status": "read_error",
                "explicit_real_foreground_lease": false,
                "no_human_os_foreground_fallback": true,
                "read_error_code": error_data_code(&error),
                "read_error_message": error.message.to_string(),
            }),
        }
    }

    /// Compute the per-row foreground-tier policy block (#1006): which backend
    /// tier the action used, whether it required the human foreground, the live
    /// foreground-input lease state, and the calling session's foreground
    /// policy. When a session whose profile is NOT allowed to reach the real
    /// human OS foreground tier nonetheless records `required_foreground=true`
    /// on a non-denied action, this is a policy violation and is surfaced as a
    /// high-severity audit marker + ERROR log (queryable via
    /// `audit_intelligence_query`). Foreground-equivalent agent lanes are not
    /// this shared human OS foreground tier.
    fn action_audit_foreground_tier(
        &self,
        tool: &'static str,
        status: &str,
        session_id: Option<&str>,
        details: &Value,
    ) -> Value {
        let required_foreground = detail_required_foreground(details);
        let backend_tier = detail_backend_tier(details);
        let lease = synapse_action::lease::status();
        let caller_is_owner =
            session_id.is_some() && lease.owner_session_id.as_deref() == session_id;
        let profile = self
            .tool_profile_snapshot(session_id)
            .ok()
            .map(|snapshot| snapshot.profile);
        let policy_label = profile.map_or("unknown", super::tool_profiles::ToolProfileKind::as_str);
        let policy_allows_foreground =
            profile.is_some_and(super::tool_profiles::ToolProfileKind::allows_foreground_tier);
        // A denied action never actually touched the foreground, so it is not a
        // violation even if it was a foreground-tier request.
        let foreground_policy_violation =
            required_foreground && !policy_allows_foreground && status != "denied";
        if foreground_policy_violation {
            tracing::error!(
                code = "ACTION_FOREGROUND_TIER_POLICY_VIOLATION",
                tool,
                status,
                session_id = session_id.unwrap_or("<none>"),
                profile = policy_label,
                backend_tier = backend_tier.as_deref().unwrap_or("<none>"),
                lease_owner = lease.owner_session_id.as_deref().unwrap_or("<none>"),
                "a session whose tool profile lacks real human OS foreground permission recorded a \
                 foreground-tier action without break-glass/full-capability proof (#1006/#1219)"
            );
        }
        json!({
            "required_foreground": required_foreground,
            "backend_tier": backend_tier,
            "foreground_input_lease": {
                "held": lease.held,
                "owner_session_id": lease.owner_session_id,
                "caller_is_owner": caller_is_owner,
                "expires_in_ms": lease.expires_in_ms,
            },
            "session_foreground_policy": policy_label,
            "policy_allows_foreground": policy_allows_foreground,
            "foreground_policy_violation": foreground_policy_violation,
            "allowed": !foreground_policy_violation,
        })
    }

    fn action_audit_observed_profile(
        &self,
        foreground: &synapse_core::ForegroundContext,
    ) -> ActionAuditProfileRef {
        let window = synapse_profiles::ForegroundWindow {
            exe: non_empty(&foreground.process_name),
            title: non_empty(&foreground.window_title),
            steam_appid: foreground.steam_appid,
            window_class: None,
        };
        let profile_id = self
            .profile_runtime()
            .ok()
            .and_then(|runtime| runtime.resolve_foreground(&window).ok().flatten())
            .map(|resolution| resolution.profile_id);
        ActionAuditProfileRef {
            schema_version: profile_id
                .as_deref()
                .and_then(|profile_id| self.action_audit_profile_schema_version(profile_id)),
            profile_id,
        }
    }

    fn action_audit_active_profile(&self) -> ActionAuditProfileRef {
        let profile_id = self
            .profile_runtime()
            .ok()
            .and_then(|runtime| runtime.active_profile_id().ok().flatten());
        ActionAuditProfileRef {
            schema_version: profile_id
                .as_deref()
                .and_then(|profile_id| self.action_audit_profile_schema_version(profile_id)),
            profile_id,
        }
    }

    fn action_audit_profile_schema_version(&self, profile_id: &str) -> Option<u32> {
        self.profile_runtime().ok().and_then(|runtime| {
            runtime
                .list(true)
                .ok()?
                .into_iter()
                .find(|profile| profile.id == profile_id)
                .map(|profile| profile.schema_version)
        })
    }
}

fn action_audit_detail_redactions(details: &Value) -> Vec<String> {
    let mut redactions = Vec::new();
    collect_action_audit_detail_redactions(details, &mut redactions);
    redactions.sort();
    redactions.dedup();
    redactions
}

fn collect_action_audit_detail_redactions(value: &Value, redactions: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            if let Some(Value::Array(items)) = fields.get("redacted_fields") {
                redactions.extend(
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|item| !item.trim().is_empty())
                        .map(str::to_owned),
                );
            }
            for child in fields.values() {
                collect_action_audit_detail_redactions(child, redactions);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_action_audit_detail_redactions(child, redactions);
            }
        }
        _ => {}
    }
}

fn action_session_id_from_request_context(
    request_context: &RequestContext<RoleServer>,
) -> Result<Option<String>, ErrorData> {
    super::context::mcp_session_id_from_request_context(request_context)
}

struct ActionAuditProfileRef {
    profile_id: Option<String>,
    schema_version: Option<u32>,
}

/// Extract the foreground requirement from a tool's audit `details`. A tool
/// either states it directly (`required_foreground`) or reports it per backend
/// attempt (`tier_attempts[].required_foreground`); any foreground-requiring
/// attempt makes the action foreground-tier.
fn detail_required_foreground(details: &Value) -> bool {
    if details.get("required_foreground").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    details
        .get("tier_attempts")
        .and_then(Value::as_array)
        .is_some_and(|attempts| {
            attempts.iter().any(|attempt| {
                attempt.get("required_foreground").and_then(Value::as_bool) == Some(true)
            })
        })
}

/// Extract the backend tier a tool used, from whichever field the tool records.
fn detail_backend_tier(details: &Value) -> Option<String> {
    for key in ["backend_tier_used", "backend_tier", "tier"] {
        if let Some(tier) = details.get(key).and_then(Value::as_str) {
            return Some(tier.to_owned());
        }
    }
    // Otherwise, the last delivered tier attempt, if any.
    details
        .get("tier_attempts")
        .and_then(Value::as_array)
        .and_then(|attempts| {
            attempts
                .iter()
                .rev()
                .find_map(|attempt| attempt.get("tier").and_then(Value::as_str))
        })
        .map(str::to_owned)
}

fn next_audit_key_parts() -> (u64, u32) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let ts_ns = u64::try_from(nanos).unwrap_or(u64::MAX);
    let seq = ACTION_AUDIT_SEQ.fetch_add(1, Ordering::Relaxed);
    (ts_ns, seq)
}

fn action_audit_key(ts_ns: u64, seq: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn error_data_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

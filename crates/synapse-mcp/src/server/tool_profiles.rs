use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::{ErrorData, RoleServer, model::ErrorCode, model::Tool, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_action::lease;
use synapse_core::error_codes;
use synapse_storage::cf;

use super::{Json, Parameters, SynapseService, empty_input_schema, mcp_error, tool, tool_router};

const TOOL_PROFILE_PREFIX: &str = "mcp/tool-profile/v1/";
const TOOL_PROFILE_SOURCE_OF_TRUTH: &str = "CF_SESSIONS mcp/tool-profile/v1/<session_id>";
const TOOL_PROFILE_ROW_KIND: &str = "mcp_tool_profile";
const TOOL_PROFILE_SCHEMA_VERSION: u32 = 1;
const MAX_PROFILE_REASON_CHARS: usize = 1024;

const NORMAL_ALLOWED_EXACT: &[&str] = &[
    "act_launch",
    "act_run_shell",
    "act_run_shell_cancel",
    "act_run_shell_start",
    "act_run_shell_status",
    "act_spawn_agent",
    "agent_cost",
    "agent_inbox",
    "agent_interrupt",
    "agent_kill",
    "agent_pause",
    "agent_query",
    "agent_receipts",
    "agent_respawn",
    "agent_resume",
    "agent_send",
    "agent_send_broadcast",
    "agent_stats",
    "agent_steer",
    "agent_wait",
    "approval_decide",
    "approval_gate",
    "approval_list",
    "approval_request",
    "audit_intelligence_query",
    "browser_console_messages",
    "browser_content",
    "browser_evaluate",
    "browser_inspect",
    "browser_locate",
    "browser_set_value",
    "capture_screenshot",
    "cdp_activate_tab",
    "cdp_bridge_reload",
    "cdp_close_tab",
    "cdp_navigate_tab",
    "cdp_open_tab",
    "cdp_target_info",
    "clear_target",
    "control_lease_acquire",
    "control_lease_handoff",
    "control_lease_release",
    "control_lease_status",
    "escalation_ack",
    "escalation_list",
    "find",
    "fleet_stop",
    "get_target",
    "health",
    "hygiene_flags",
    "hygiene_scan_storage",
    "hygiene_scan_text",
    "local_model_list",
    "local_model_probe",
    "local_model_register",
    "local_model_remove",
    "local_model_update",
    "observe",
    "observe_delta",
    "profile_list",
    "read_text",
    "reality_audit",
    "reality_baseline",
    "session_end",
    "session_list",
    "session_status",
    "set_capture_target",
    "set_perception_mode",
    "set_target",
    "storage_inspect",
    "target_act",
    "target_claim",
    "target_claim_adopt",
    "target_claim_status",
    "target_release",
    "timeline_digest",
    "timeline_get",
    "timeline_search",
    "timeline_stats",
    "tool_profile_set",
    "tool_profile_status",
    "window_list",
    "workspace_get",
    "workspace_list",
    "workspace_put",
    "workspace_subscribe",
];

const NORMAL_ALLOWED_PREFIXES: &[&str] = &["agent_template_", "task_"];

const BROWSER_CONTROL_ALLOWED_EXACT: &[&str] = &[
    "approval_list",
    "browser_console_messages",
    "browser_content",
    "browser_evaluate",
    "browser_inspect",
    "browser_locate",
    "browser_set_value",
    "capture_screenshot",
    "cdp_activate_tab",
    "cdp_bridge_reload",
    "cdp_close_tab",
    "cdp_navigate_tab",
    "cdp_open_tab",
    "cdp_target_info",
    "clear_target",
    "control_lease_acquire",
    "control_lease_release",
    "control_lease_status",
    "escalation_list",
    "find",
    "get_target",
    "health",
    "observe",
    "observe_delta",
    "read_text",
    "reality_audit",
    "reality_baseline",
    "session_end",
    "session_list",
    "session_status",
    "set_capture_target",
    "set_perception_mode",
    "set_target",
    "storage_inspect",
    "target_act",
    "target_claim",
    "target_claim_adopt",
    "target_claim_status",
    "target_release",
    "tool_profile_set",
    "tool_profile_status",
    "window_list",
    "workspace_get",
    "workspace_list",
    "workspace_put",
    "workspace_subscribe",
];

const BREAK_GLASS_HAZARDOUS_TOOLS: &[&str] = &[
    "act_click",
    "act_clipboard",
    "act_combo",
    "act_focus_window",
    "act_keymap",
    "act_pad",
    "act_press",
    "act_scroll",
    "act_set_field_text",
    "act_set_value",
    "act_stroke",
    "act_type",
    "action_diagnostic_queue_full_setup",
    "action_diagnostic_rate_limit_override",
    "hidden_desktop_pip_frame",
    "release_all",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolProfileKind {
    NormalAgent,
    BrowserControl,
    BreakGlass,
    /// Full Synapse tool surface for Synapse-spawned local-model agents
    /// (gemma/DeepSeek/etc., #1031). Identical visibility to `BreakGlass` (every
    /// real tool, including the foreground input primitives) but assigned
    /// automatically to the trusted local-model harness instead of requiring an
    /// explicit operator-held foreground lease. Local models operate the machine
    /// and must never be missing a tool; foreground contention is handled at
    /// action time by the per-action lease/target guards (#717/#999), not by
    /// hiding the tools from discovery.
    FullCapability,
}

impl ToolProfileKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NormalAgent => "normal_agent",
            Self::BrowserControl => "browser_control",
            Self::BreakGlass => "break_glass",
            Self::FullCapability => "full_capability",
        }
    }

    /// Whether this profile is permitted to reach the real human OS foreground
    /// input tier. Normal/task profiles still preserve foreground-equivalent
    /// capability through `agent_logical_foreground` / `foreground_lane` routes;
    /// only the serialized real-foreground tier needs break-glass/full-capability
    /// proof (#999/#1219).
    pub(crate) const fn allows_foreground_tier(self) -> bool {
        matches!(self, Self::BreakGlass | Self::FullCapability)
    }

    fn label(self) -> &'static str {
        match self {
            Self::NormalAgent => "normal_agent",
            Self::BrowserControl => "dashboard/browser-control task",
            Self::BreakGlass => "break-glass/admin",
            Self::FullCapability => "full-capability local-model agent",
        }
    }

    fn is_visible(self, tool_name: &str) -> bool {
        match self {
            Self::BreakGlass | Self::FullCapability => true,
            Self::NormalAgent => {
                NORMAL_ALLOWED_EXACT.contains(&tool_name)
                    || NORMAL_ALLOWED_PREFIXES
                        .iter()
                        .any(|prefix| tool_name.starts_with(prefix))
            }
            Self::BrowserControl => BROWSER_CONTROL_ALLOWED_EXACT.contains(&tool_name),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct PersistedToolProfile {
    schema_version: u32,
    row_kind: String,
    session_id: String,
    profile: ToolProfileKind,
    source: String,
    reason: Option<String>,
    set_by_session_id: Option<String>,
    stored_at_unix_ms: u64,
    allowed_tool_count: usize,
    allowed_tool_sha256: String,
    denied_break_glass_tools: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileAssignment {
    pub schema_version: u32,
    pub row_kind: String,
    pub session_id: String,
    pub profile: ToolProfileKind,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by_session_id: Option<String>,
    pub stored_at_unix_ms: u64,
    pub allowed_tool_count: usize,
    pub allowed_tool_sha256: String,
    pub denied_break_glass_tools: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileRowReadback {
    pub cf_name: &'static str,
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub record: ToolProfileAssignment,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileAuditReadback {
    pub cf_name: &'static str,
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileSnapshot {
    pub source_of_truth: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub profile: ToolProfileKind,
    pub profile_label: &'static str,
    pub source: String,
    pub implementation_tool_count: usize,
    pub visible_tool_count: usize,
    pub visible_tool_sha256: String,
    pub visible_tool_names: Vec<String>,
    pub denied_break_glass_tools: Vec<String>,
    pub foreground_capability: ToolProfileForegroundCapability,
    pub hidden_tool_routes: Vec<HiddenToolCapabilityRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_row: Option<ToolProfileRowReadback>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileForegroundCapability {
    pub source_of_truth: &'static str,
    pub profile_preserves_capability: bool,
    pub human_os_foreground: &'static str,
    pub agent_logical_foreground: &'static str,
    pub preferred_path: &'static str,
    pub real_os_foreground_path: &'static str,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HiddenToolCapabilityRoute {
    pub hidden_tool: String,
    pub status: &'static str,
    pub preferred_tools: Vec<String>,
    pub agent_logical_foreground_policy: &'static str,
    pub human_os_foreground_policy: &'static str,
    pub break_glass_policy: &'static str,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileStatusResponse {
    pub snapshot: ToolProfileSnapshot,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileSetParams {
    pub profile: ToolProfileKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub confirm_break_glass: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileLeaseProof {
    pub required: bool,
    pub held: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    pub caller_is_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileSetResponse {
    pub before: ToolProfileSnapshot,
    pub after: ToolProfileSnapshot,
    pub row_readback: ToolProfileRowReadback,
    pub intent_audit: ToolProfileAuditReadback,
    pub final_audit: ToolProfileAuditReadback,
    pub lease_proof: ToolProfileLeaseProof,
}

#[tool_router(router = tool_profile_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Read this MCP session's effective tool profile, visible tools/list names, durable CF_SESSIONS policy row, and capability-preserving routes for hidden raw foreground primitives. The readback distinguishes human_os_foreground from agent_logical_foreground: normal/task profiles prefer target_act/browser/CDP/session-lane tools, while real OS foreground primitives stay reachable only through an explicit lease + break_glass path.",
        input_schema = empty_input_schema()
    )]
    pub async fn tool_profile_status(
        &self,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ToolProfileStatusResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "tool_profile_status",
            "tool.invocation kind=tool_profile_status"
        );
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        Ok(Json(ToolProfileStatusResponse {
            snapshot: self.tool_profile_snapshot(session_id.as_deref())?,
        }))
    }

    #[tool(
        description = "Set this MCP session's durable tool profile. normal_agent and browser_control preserve capability through target_act/browser/CDP/session-lane routes while keeping raw human-OS-foreground primitives out of the default affordance. break_glass exposes the full raw surface only when confirm_break_glass=true, reason is non-empty, and this session currently owns the foreground input lease."
    )]
    pub async fn tool_profile_set(
        &self,
        params: Parameters<ToolProfileSetParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ToolProfileSetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "tool_profile_set",
            "tool.invocation kind=tool_profile_set"
        );
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::HTTP_SESSION_INVALID,
                    "tool_profile_set requires an MCP session id so the policy decision can be persisted",
                )
            })?;
        let params = params.0;
        let reason = normalize_reason(params.reason.as_deref())?;
        let before = self.tool_profile_snapshot(Some(&session_id))?;
        let lease_proof = break_glass_lease_proof(&session_id, params.profile);
        let command_payload = json!({
            "requested_profile": params.profile.as_str(),
            "reason": reason,
            "confirm_break_glass": params.confirm_break_glass,
        });
        let command_before = json!({
            "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
            "before_profile": before.profile.as_str(),
            "before_visible_tool_count": before.visible_tool_count,
            "lease_proof": lease_proof,
        });
        let intent_audit = audit_readback(self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "tool_profile_set",
                "profile_set",
                Some(session_id.clone()),
                Some(session_id.clone()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            ),
        )?);

        if let Err(error) = validate_profile_set_policy(
            &session_id,
            params.profile,
            reason.as_deref(),
            params.confirm_break_glass,
            &lease_proof,
        ) {
            let final_audit = self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "tool_profile_set",
                    "profile_set",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
                        "after_profile": before.profile.as_str(),
                        "lease_proof": lease_proof,
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&error),
                ),
            )?;
            let _final_audit = audit_readback(final_audit);
            return Err(error);
        }

        let row_readback = match self.write_tool_profile_assignment(
            &session_id,
            params.profile,
            "tool_profile_set",
            reason.clone(),
            Some(session_id.clone()),
        ) {
            Ok(row) => row,
            Err(error) => {
                let final_audit = self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "tool_profile_set",
                        "profile_set",
                        Some(session_id.clone()),
                        Some(session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
                            "after_profile": before.profile.as_str(),
                            "lease_proof": lease_proof,
                        }),
                        "error",
                    )
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                let _final_audit = audit_readback(final_audit);
                return Err(error);
            }
        };
        let after = self.tool_profile_snapshot(Some(&session_id))?;

        // #1020: the visible tool surface for this MCP session just changed.
        // Push a `notifications/tools/list_changed` so the client refetches
        // `tools/list` within the *same* session. Without this, newly-allowed
        // break_glass tools stay uncallable until a full MCP reconnect, and a
        // reconnect mints a new session id that drops the foreground input lease
        // and any target claims acquired for the privileged action — defeating
        // the entire break-glass workflow. The durable CF_SESSIONS row is the
        // source of truth and is already committed above; this notification is a
        // best-effort client cache invalidation, so a delivery failure is logged
        // loudly but does not fail the (already-persisted) profile change.
        if before.visible_tool_sha256 != after.visible_tool_sha256 {
            match request_context.peer.notify_tool_list_changed().await {
                Ok(()) => {
                    tracing::info!(
                        code = "MCP_TOOL_LIST_CHANGED_NOTIFIED",
                        session_id = %session_id,
                        before_profile = before.profile.as_str(),
                        after_profile = after.profile.as_str(),
                        before_visible_tool_count = before.visible_tool_count,
                        after_visible_tool_count = after.visible_tool_count,
                        "tool_profile_set pushed notifications/tools/list_changed after a visible tool-surface change"
                    );
                }
                Err(notify_err) => {
                    tracing::error!(
                        code = "MCP_TOOL_LIST_CHANGED_NOTIFY_FAILED",
                        session_id = %session_id,
                        before_profile = before.profile.as_str(),
                        after_profile = after.profile.as_str(),
                        error = %notify_err,
                        "tool_profile_set persisted the new profile but failed to push notifications/tools/list_changed; the client may need to reconnect to observe the updated tool surface"
                    );
                }
            }
        }

        let final_audit = audit_readback(self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "tool_profile_set",
                "profile_set",
                Some(session_id.clone()),
                Some(session_id),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
                    "after_profile": after.profile.as_str(),
                    "after_visible_tool_count": after.visible_tool_count,
                    "row_readback": row_readback,
                    "lease_proof": lease_proof,
                }),
                "ok",
            ),
        )?);

        Ok(Json(ToolProfileSetResponse {
            before,
            after,
            row_readback,
            intent_audit,
            final_audit,
            lease_proof,
        }))
    }
}

impl SynapseService {
    pub(crate) fn tools_for_session_profile(
        &self,
        session_id: Option<&str>,
    ) -> Result<Vec<Tool>, ErrorData> {
        let snapshot = self.tool_profile_snapshot(session_id)?;
        let mut tools = self.full_sanitized_tools();
        if session_id.is_some() {
            tools.retain(|tool| snapshot.profile.is_visible(tool.name.as_ref()));
        }
        sort_tools_for_profile(&mut tools, snapshot.profile);
        Ok(tools)
    }

    pub(crate) fn tool_profile_snapshot(
        &self,
        session_id: Option<&str>,
    ) -> Result<ToolProfileSnapshot, ErrorData> {
        let full_tool_names = self.full_tool_names();
        let implementation_tool_count = full_tool_names.len();
        let (profile, source, policy_row) = match session_id {
            Some(session_id) => {
                let row = self.ensure_tool_profile_assignment(session_id)?;
                (row.record.profile, row.record.source.clone(), Some(row))
            }
            None => (
                ToolProfileKind::BreakGlass,
                "unscoped_stdio_admin".to_owned(),
                None,
            ),
        };
        let visible_tool_names = if session_id.is_some() {
            visible_tool_names_for_profile(profile, &full_tool_names)
        } else {
            full_tool_names
        };
        let visible_tool_sha256 = sha256_json_hex(&visible_tool_names)?;
        let denied_break_glass_tools = denied_break_glass_tools(&visible_tool_names);
        let hidden_tool_routes = hidden_tool_capability_routes(&visible_tool_names);
        Ok(ToolProfileSnapshot {
            source_of_truth: TOOL_PROFILE_SOURCE_OF_TRUTH,
            session_id: session_id.map(ToOwned::to_owned),
            profile,
            profile_label: profile.label(),
            source,
            implementation_tool_count,
            visible_tool_count: visible_tool_names.len(),
            visible_tool_sha256,
            visible_tool_names,
            denied_break_glass_tools,
            foreground_capability: foreground_capability_policy(profile),
            hidden_tool_routes,
            policy_row,
        })
    }

    pub(crate) fn admit_tool_call_for_profile(
        &self,
        tool_name: &str,
        session_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        let Some(session_id) = session_id else {
            return Ok(());
        };
        let full_tool_names = self.full_tool_names();
        if !full_tool_names.iter().any(|name| name == tool_name) {
            return Ok(());
        }
        let row = self.ensure_tool_profile_assignment(session_id)?;
        if row.record.profile.is_visible(tool_name) {
            return Ok(());
        }
        let visible_tool_names =
            visible_tool_names_for_profile(row.record.profile, &full_tool_names);
        let capability_route = hidden_tool_capability_route(tool_name);
        let error = ErrorData::new(
            ErrorCode(-32099),
            format!(
                "tool {tool_name:?} is hidden by MCP tool profile {} for session {session_id}",
                row.record.profile.as_str()
            ),
            Some(json!({
                "code": error_codes::TOOL_PROFILE_POLICY_DENIED,
                "tool": tool_name,
                "session_id": session_id,
                "profile": row.record.profile.as_str(),
                "profile_label": row.record.profile.label(),
                "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
                "policy_row": row,
                "visible_tool_count": visible_tool_names.len(),
                "capability_route": capability_route,
                "resolution": "use the named capability_route preferred tools for agent logical foreground work, or explicitly acquire the foreground input lease and set profile=break_glass with confirm_break_glass=true plus a non-empty reason for real human OS foreground work",
            })),
        );
        let command_payload = json!({
            "requested_tool": tool_name,
            "profile": row.record.profile.as_str(),
        });
        let command_before = json!({
            "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
            "policy_row": row,
            "visible_tool_count": visible_tool_names.len(),
        });
        self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "tool_profile_policy",
                "tool_call_denied",
                Some(session_id.to_owned()),
                Some(session_id.to_owned()),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": "CF_ACTION_LOG command_audit row",
                    "denied_tool": tool_name,
                    "capability_route": hidden_tool_capability_route(tool_name),
                }),
                "error",
            )
            .with_error(super::command_audit::command_audit_error_from_error_data(
                &error,
            )),
        )?;
        Err(error)
    }

    fn ensure_tool_profile_assignment(
        &self,
        session_id: &str,
    ) -> Result<ToolProfileRowReadback, ErrorData> {
        let is_local_agent = self.session_is_local_model_agent(session_id);
        match self.read_tool_profile_assignment(session_id)? {
            Some(row) => {
                // Self-heal: if a session was first seen before its MCP
                // initialize client identity landed in the registry, it may have
                // been written the least-privilege `default_normal_agent` row.
                // Once the registry classifies it as the trusted local-model
                // harness, upgrade it to the full-capability surface so the
                // local model is never left without the input primitives (#1031).
                // Only the *default* normal-agent row self-heals; an explicit
                // operator profile choice is never silently widened.
                if is_local_agent && row.record.source == "default_normal_agent" {
                    return self.write_tool_profile_assignment(
                        session_id,
                        ToolProfileKind::FullCapability,
                        "default_full_capability_local_agent",
                        None,
                        None,
                    );
                }
                Ok(row)
            }
            None => {
                let (profile, source) = if is_local_agent {
                    (
                        ToolProfileKind::FullCapability,
                        "default_full_capability_local_agent",
                    )
                } else {
                    (ToolProfileKind::NormalAgent, "default_normal_agent")
                };
                self.write_tool_profile_assignment(session_id, profile, source, None, None)
            }
        }
    }

    /// True when `session_id` belongs to a Synapse-spawned local-model agent
    /// (the `synapse-local-model-agent` MCP client, classified `"local-model"`
    /// by [`super::session_registry::infer_agent_kind`]). Trust basis: Synapse is
    /// loopback-only + bearer-token + single-user, so the MCP client identity is
    /// a sound affordance signal here (it is NOT a cross-tenant security boundary;
    /// `clientInfo` is self-reported per MCP). Unknown / unclassified sessions
    /// return false and get the least-privilege default.
    fn session_is_local_model_agent(&self, session_id: &str) -> bool {
        self.session_registry_ref()
            .lock()
            .ok()
            .and_then(|registry| registry.agent_kind_for(session_id))
            .as_deref()
            == Some("local-model")
    }

    fn read_tool_profile_assignment(
        &self,
        session_id: &str,
    ) -> Result<Option<ToolProfileRowReadback>, ErrorData> {
        let db = self.m3_storage()?;
        let key = tool_profile_key(session_id);
        let rows = db
            .scan_cf_prefix(cf::CF_SESSIONS, &key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let Some((read_key, value)) = rows.into_iter().find(|(row_key, _)| row_key == &key) else {
            return Ok(None);
        };
        let persisted =
            synapse_storage::decode_json::<PersistedToolProfile>(&value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("decode tool profile row failed for {session_id}: {error}"),
                )
            })?;
        if persisted.schema_version != TOOL_PROFILE_SCHEMA_VERSION
            || persisted.row_kind != TOOL_PROFILE_ROW_KIND
            || persisted.session_id != session_id
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "tool profile row mismatch for {session_id}: schema_version={} row_kind={} row_session_id={}",
                    persisted.schema_version, persisted.row_kind, persisted.session_id
                ),
            ));
        }
        let record = ToolProfileAssignment {
            schema_version: persisted.schema_version,
            row_kind: persisted.row_kind,
            session_id: persisted.session_id,
            profile: persisted.profile,
            source: persisted.source,
            reason: persisted.reason,
            set_by_session_id: persisted.set_by_session_id,
            stored_at_unix_ms: persisted.stored_at_unix_ms,
            allowed_tool_count: persisted.allowed_tool_count,
            allowed_tool_sha256: persisted.allowed_tool_sha256,
            denied_break_glass_tools: persisted.denied_break_glass_tools,
        };
        Ok(Some(ToolProfileRowReadback {
            cf_name: cf::CF_SESSIONS,
            key_hex: hex_lower(&read_key),
            value_len_bytes: value.len() as u64,
            value_sha256: sha256_hex(&value),
            record,
        }))
    }

    fn write_tool_profile_assignment(
        &self,
        session_id: &str,
        profile: ToolProfileKind,
        source: impl Into<String>,
        reason: Option<String>,
        set_by_session_id: Option<String>,
    ) -> Result<ToolProfileRowReadback, ErrorData> {
        let full_tool_names = self.full_tool_names();
        let allowed_tool_names = visible_tool_names_for_profile(profile, &full_tool_names);
        let allowed_tool_sha256 = sha256_json_hex(&allowed_tool_names)?;
        let record = ToolProfileAssignment {
            schema_version: TOOL_PROFILE_SCHEMA_VERSION,
            row_kind: TOOL_PROFILE_ROW_KIND.to_owned(),
            session_id: session_id.to_owned(),
            profile,
            source: source.into(),
            reason,
            set_by_session_id,
            stored_at_unix_ms: unix_ms_now(),
            allowed_tool_count: allowed_tool_names.len(),
            allowed_tool_sha256,
            denied_break_glass_tools: denied_break_glass_tools(&allowed_tool_names),
        };
        let encoded = synapse_storage::encode_json(&record).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("encode tool profile row failed for {session_id}: {error}"),
            )
        })?;
        let db = self.m3_storage()?;
        let key = tool_profile_key(session_id);
        db.put_batch_pressure_bypass(cf::CF_SESSIONS, [(key.clone(), encoded.clone())])
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let readback = self
            .read_tool_profile_assignment(session_id)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("tool profile row missing after write for {session_id}"),
                )
            })?;
        if readback.value_sha256 != sha256_hex(&encoded) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("tool profile row readback hash mismatch for {session_id}"),
            ));
        }
        tracing::info!(
            code = "MCP_TOOL_PROFILE_PERSISTED",
            session_id,
            profile = profile.as_str(),
            visible_tool_count = readback.record.allowed_tool_count,
            key_hex = %readback.key_hex,
            "persisted MCP tool profile to CF_SESSIONS"
        );
        Ok(readback)
    }

    fn full_sanitized_tools(&self) -> Vec<Tool> {
        super::schema_sanitize::sanitize_tools(self.tool_router.list_all())
    }

    fn full_tool_names(&self) -> Vec<String> {
        let mut names = self
            .full_sanitized_tools()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

fn validate_profile_set_policy(
    session_id: &str,
    profile: ToolProfileKind,
    reason: Option<&str>,
    confirm_break_glass: bool,
    lease_proof: &ToolProfileLeaseProof,
) -> Result<(), ErrorData> {
    // Both the full raw surface (break_glass) and the local-agent full-capability
    // surface, when requested *explicitly* via tool_profile_set, require the same
    // operator-intent proof. This stops any agent from self-escalating to the
    // foreground primitives by hand. The frictionless path to full_capability is
    // the automatic, client-identity-keyed default for the trusted local-model
    // harness (see `ensure_tool_profile_assignment`), never this tool.
    if !matches!(
        profile,
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability
    ) {
        return Ok(());
    }
    let profile_label = profile.as_str();
    if !confirm_break_glass {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("explicit profile={profile_label} requires confirm_break_glass=true"),
        ));
    }
    if reason.is_none_or(str::is_empty) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("explicit profile={profile_label} requires a non-empty reason"),
        ));
    }
    if !lease_proof.caller_is_owner {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "explicit profile={profile_label} requires this MCP session to own the foreground input lease; current owner={:?}",
                lease_proof.owner_session_id
            ),
            Some(json!({
                "code": error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
                "session_id": session_id,
                "profile": profile.as_str(),
                "lease_proof": lease_proof,
                "resolution": "call control_lease_acquire first, then retry tool_profile_set with confirm_break_glass=true and a reason",
            })),
        ));
    }
    Ok(())
}

fn break_glass_lease_proof(session_id: &str, profile: ToolProfileKind) -> ToolProfileLeaseProof {
    let status = lease::status();
    ToolProfileLeaseProof {
        required: profile == ToolProfileKind::BreakGlass,
        // FullCapability is auto-assigned to the trusted local-model harness and
        // does not gate on the foreground lease; only an *explicit*
        // tool_profile_set escalation (handled in validate_profile_set_policy)
        // is lease-gated. See [`validate_profile_set_policy`].
        held: status.held,
        owner_session_id: status.owner_session_id.clone(),
        caller_is_owner: status.owner_session_id.as_deref() == Some(session_id),
        expires_in_ms: status.expires_in_ms,
    }
}

fn normalize_reason(raw: Option<&str>) -> Result<Option<String>, ErrorData> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.chars().count() > MAX_PROFILE_REASON_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("tool profile reason must be at most {MAX_PROFILE_REASON_CHARS} characters"),
        ));
    }
    Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
}

fn visible_tool_names_for_profile(
    profile: ToolProfileKind,
    full_tool_names: &[String],
) -> Vec<String> {
    let mut names = full_tool_names
        .iter()
        .filter(|name| profile.is_visible(name))
        .cloned()
        .collect::<Vec<_>>();
    names.sort_by(|left, right| {
        tool_rank(profile, left)
            .cmp(&tool_rank(profile, right))
            .then(left.cmp(right))
    });
    names
}

fn denied_break_glass_tools(visible_tool_names: &[String]) -> Vec<String> {
    let visible = visible_tool_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    BREAK_GLASS_HAZARDOUS_TOOLS
        .iter()
        .copied()
        .filter(|name| !visible.contains(name))
        .map(str::to_owned)
        .collect()
}

fn foreground_capability_policy(profile: ToolProfileKind) -> ToolProfileForegroundCapability {
    let (preferred_path, real_os_foreground_path) = match profile {
        ToolProfileKind::NormalAgent => (
            "target_act, browser_set_value, cdp_* and per-session target/claim tools are visible in the normal profile",
            "control_lease_acquire + tool_profile_set break_glass + raw foreground primitive; denied without lease/reason/confirm",
        ),
        ToolProfileKind::BrowserControl => (
            "browser/CDP/target_act tools plus lease controls are visible in the task profile; raw shell/spawn surfaces stay hidden",
            "control_lease_acquire + tool_profile_set break_glass + raw foreground primitive; denied without lease/reason/confirm",
        ),
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability => (
            "full raw surface is visible; prefer target_act/session-lane tools unless real OS foreground input is the intended lane",
            "raw foreground primitives may run only under their own lease/target guards and action-audit policy",
        ),
    };
    ToolProfileForegroundCapability {
        source_of_truth: TOOL_PROFILE_SOURCE_OF_TRUTH,
        profile_preserves_capability: true,
        human_os_foreground: "the physical foreground window used by the human; never an implicit fallback for hidden tools",
        agent_logical_foreground: "the per-session foreground-equivalent target/lane; preferred for valid local work",
        preferred_path,
        real_os_foreground_path,
    }
}

fn hidden_tool_capability_routes(visible_tool_names: &[String]) -> Vec<HiddenToolCapabilityRoute> {
    let visible = visible_tool_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    BREAK_GLASS_HAZARDOUS_TOOLS
        .iter()
        .copied()
        .filter(|name| !visible.contains(name))
        .map(hidden_tool_capability_route)
        .collect()
}

fn hidden_tool_capability_route(tool_name: &str) -> HiddenToolCapabilityRoute {
    let preferred_tools = match tool_name {
        "act_click" => vec![
            "target_act verb=click",
            "browser DOM action through target_act",
            "target_claim",
        ],
        "act_type" | "act_set_value" | "act_set_field_text" => {
            vec![
                "target_act verb=set_field",
                "browser_set_value",
                "browser_evaluate",
            ]
        }
        "act_press" | "act_keymap" | "act_combo" => {
            vec![
                "target_act verb=press",
                "browser DOM action through target_act",
            ]
        }
        "act_scroll" => vec![
            "browser_evaluate scrollIntoView/window.scrollBy",
            "capture_screenshot",
            "observe",
            "target_claim",
        ],
        "act_stroke" | "act_pad" => vec![
            "target_claim",
            "control_lease_acquire",
            "tool_profile_set break_glass",
        ],
        "act_focus_window" => vec![
            "set_target",
            "target_claim",
            "control_lease_acquire",
            "tool_profile_set break_glass",
            "target_act verb=focus_window",
            "session_status",
        ],
        "act_launch" => vec![
            "act_spawn_agent",
            "cdp_open_tab",
            "target_act verb=navigate",
        ],
        "act_clipboard" => vec![
            "workspace_put",
            "browser_set_value",
            "target_act verb=set_field",
        ],
        "release_all" => vec![
            "target_release",
            "control_lease_release",
            "clear_target",
            "session_end",
        ],
        "hidden_desktop_pip_frame" => {
            vec!["capture_screenshot", "window_list", "session_status"]
        }
        "action_diagnostic_queue_full_setup" | "action_diagnostic_rate_limit_override" => {
            vec!["health", "storage_inspect", "session_status"]
        }
        _ => vec!["target_act", "tool_profile_set break_glass"],
    };
    HiddenToolCapabilityRoute {
        hidden_tool: tool_name.to_owned(),
        status: "routed_or_break_glass",
        preferred_tools: preferred_tools.into_iter().map(str::to_owned).collect(),
        agent_logical_foreground_policy: "use the preferred tools against this session's agent_logical_foreground/foreground_lane",
        human_os_foreground_policy: "never use the human OS foreground as an implicit fallback",
        break_glass_policy: "for a real OS foreground primitive, first acquire the input lease, then set profile=break_glass with confirm_break_glass=true and a non-empty reason",
    }
}

fn sort_tools_for_profile(tools: &mut [Tool], profile: ToolProfileKind) {
    tools.sort_by(|left, right| {
        let left_name = left.name.as_ref();
        let right_name = right.name.as_ref();
        tool_rank(profile, left_name)
            .cmp(&tool_rank(profile, right_name))
            .then(left_name.cmp(right_name))
    });
}

fn tool_rank(profile: ToolProfileKind, tool_name: &str) -> usize {
    match profile {
        ToolProfileKind::NormalAgent => NORMAL_ALLOWED_EXACT
            .iter()
            .position(|name| *name == tool_name)
            .unwrap_or(usize::MAX),
        ToolProfileKind::BrowserControl => BROWSER_CONTROL_ALLOWED_EXACT
            .iter()
            .position(|name| *name == tool_name)
            .unwrap_or(usize::MAX),
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability => usize::MAX,
    }
}

fn tool_profile_key(session_id: &str) -> Vec<u8> {
    format!("{TOOL_PROFILE_PREFIX}{session_id}").into_bytes()
}

fn audit_readback(
    readback: super::command_audit::CommandAuditRowReadback,
) -> ToolProfileAuditReadback {
    ToolProfileAuditReadback {
        cf_name: readback.cf_name,
        key_hex: readback.key_hex,
        value_len_bytes: readback.value_len_bytes,
        value_sha256: readback.value_sha256,
    }
}

fn sha256_json_hex<T: Serialize>(value: &T) -> Result<String, ErrorData> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("serialize tool profile digest payload failed: {error}"),
        )
    })?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{num::NonZeroUsize, path::Path};

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};

    fn service_with_db(path: &Path) -> SynapseService {
        SynapseService::try_with_m2_shutdown_reason_and_m3_config(
            CancellationToken::new(),
            "test",
            CancellationToken::new(),
            &M2ServiceConfig::default(),
            M3ServiceConfig::from_cli_parts(
                Some(path.join("db")),
                Some(path.to_path_buf()),
                false,
                "127.0.0.1:0".to_owned(),
                NonZeroUsize::new(4).expect("nonzero"),
                false,
                true,
                None,
                false,
                None,
            ),
            M4ServiceConfig::default(),
        )
        .expect("construct service")
    }

    fn tool_names(tools: Vec<Tool>) -> Vec<String> {
        tools
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn names() -> Vec<String> {
        let mut names = BREAK_GLASS_HAZARDOUS_TOOLS
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        names.extend(
            [
                "act_run_shell",
                "act_launch",
                "cdp_open_tab",
                "health",
                "session_list",
                "target_act",
                "browser_evaluate",
                "browser_set_value",
                "control_lease_acquire",
                "control_lease_release",
                "tool_profile_set",
                "tool_profile_status",
            ]
            .iter()
            .map(|name| (*name).to_owned()),
        );
        names
    }

    #[test]
    fn normal_profile_routes_foreground_capability_without_raw_primitives() {
        let visible = visible_tool_names_for_profile(ToolProfileKind::NormalAgent, &names());
        assert!(visible.contains(&"act_run_shell".to_owned()));
        assert!(visible.contains(&"act_launch".to_owned()));
        assert!(visible.contains(&"cdp_open_tab".to_owned()));
        assert!(visible.contains(&"target_act".to_owned()));
        assert!(visible.contains(&"browser_set_value".to_owned()));
        assert!(visible.contains(&"control_lease_acquire".to_owned()));
        assert!(visible.contains(&"tool_profile_set".to_owned()));
        assert!(!visible.contains(&"act_click".to_owned()));
        assert!(!visible.contains(&"act_type".to_owned()));
        assert!(!visible.contains(&"release_all".to_owned()));

        let policy = foreground_capability_policy(ToolProfileKind::NormalAgent);
        assert!(policy.profile_preserves_capability);
        assert!(
            policy
                .agent_logical_foreground
                .contains("per-session foreground-equivalent")
        );
        assert!(
            policy
                .human_os_foreground
                .contains("never an implicit fallback")
        );

        let routes = hidden_tool_capability_routes(&visible);
        assert!(
            !routes.iter().any(|route| route.hidden_tool == "act_launch"),
            "act_launch is a launch/target-creation capability with its own policy checks, not a hidden foreground input primitive"
        );
        let act_type_route = routes
            .iter()
            .find(|route| route.hidden_tool == "act_type")
            .expect("act_type route");
        assert!(
            act_type_route
                .preferred_tools
                .contains(&"target_act verb=set_field".to_owned())
        );
        assert!(
            act_type_route
                .preferred_tools
                .contains(&"browser_set_value".to_owned())
        );
        assert!(
            act_type_route
                .agent_logical_foreground_policy
                .contains("agent_logical_foreground")
        );
        assert!(
            act_type_route
                .human_os_foreground_policy
                .contains("never use the human OS foreground")
        );
    }

    #[test]
    fn browser_profile_is_narrower_than_normal_agent() {
        let visible = visible_tool_names_for_profile(ToolProfileKind::BrowserControl, &names());
        assert!(visible.contains(&"cdp_open_tab".to_owned()));
        assert!(visible.contains(&"session_list".to_owned()));
        assert!(visible.contains(&"target_act".to_owned()));
        assert!(visible.contains(&"browser_set_value".to_owned()));
        assert!(visible.contains(&"control_lease_acquire".to_owned()));
        assert!(visible.contains(&"control_lease_release".to_owned()));
        assert!(!visible.contains(&"act_run_shell".to_owned()));
        assert!(!visible.contains(&"act_click".to_owned()));
    }

    #[test]
    fn break_glass_profile_exposes_full_surface() {
        let mut expected = names();
        expected.sort();
        let visible = visible_tool_names_for_profile(ToolProfileKind::BreakGlass, &names());
        assert_eq!(visible, expected);
        assert!(denied_break_glass_tools(&visible).is_empty());
    }

    #[test]
    fn break_glass_requires_confirm_reason_and_lease() {
        let proof = ToolProfileLeaseProof {
            required: true,
            held: false,
            owner_session_id: None,
            caller_is_owner: false,
            expires_in_ms: None,
        };
        assert!(
            validate_profile_set_policy(
                "s1",
                ToolProfileKind::BreakGlass,
                Some("need raw foreground click"),
                false,
                &proof,
            )
            .is_err()
        );
        assert!(
            validate_profile_set_policy("s1", ToolProfileKind::BreakGlass, None, true, &proof,)
                .is_err()
        );
        assert!(
            validate_profile_set_policy(
                "s1",
                ToolProfileKind::BreakGlass,
                Some("need raw foreground click"),
                true,
                &proof,
            )
            .is_err()
        );
    }

    #[test]
    fn break_glass_policy_accepts_owned_lease_proof() {
        let proof = ToolProfileLeaseProof {
            required: true,
            held: true,
            owner_session_id: Some("s1".to_owned()),
            caller_is_owner: true,
            expires_in_ms: Some(10_000),
        };
        validate_profile_set_policy(
            "s1",
            ToolProfileKind::BreakGlass,
            Some("need raw foreground click"),
            true,
            &proof,
        )
        .expect("owned lease proof should allow break-glass");
    }

    #[test]
    fn default_normal_profile_persists_policy_row_and_filters_tools() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1008-default-session";
        let before = service
            .read_tool_profile_assignment(session_id)
            .expect("read before");
        assert!(before.is_none());

        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("profile tools"),
        );
        assert!(tools.contains(&"health".to_owned()));
        assert!(tools.contains(&"cdp_open_tab".to_owned()));
        assert!(tools.contains(&"tool_profile_status".to_owned()));
        assert!(!tools.contains(&"act_click".to_owned()));
        assert!(!tools.contains(&"act_type".to_owned()));
        assert!(!tools.contains(&"release_all".to_owned()));

        let row = service
            .read_tool_profile_assignment(session_id)
            .expect("read after")
            .expect("row after tools/list profile resolution");
        assert_eq!(row.cf_name, cf::CF_SESSIONS);
        assert_eq!(row.record.profile, ToolProfileKind::NormalAgent);
        assert_eq!(row.record.source, "default_normal_agent");
        assert_eq!(row.record.allowed_tool_count, tools.len());
        assert!(row.value_sha256.starts_with("sha256:"));

        let db = service.m3_storage().expect("storage");
        let stored = db
            .scan_cf_prefix(cf::CF_SESSIONS, &tool_profile_key(session_id))
            .expect("scan policy rows");
        assert_eq!(stored.len(), 1);
        assert_eq!(hex_lower(&stored[0].0), row.key_hex);
        assert_eq!(sha256_hex(&stored[0].1), row.value_sha256);
    }

    #[test]
    fn browser_control_profile_excludes_shell_and_foreground_primitives() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1008-browser-session";
        let row = service
            .write_tool_profile_assignment(
                session_id,
                ToolProfileKind::BrowserControl,
                "test_browser_control",
                Some("dashboard inactive tab verification".to_owned()),
                Some(session_id.to_owned()),
            )
            .expect("write browser profile");
        assert_eq!(row.record.profile, ToolProfileKind::BrowserControl);

        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("browser profile tools"),
        );
        assert!(tools.contains(&"cdp_open_tab".to_owned()));
        assert!(tools.contains(&"cdp_target_info".to_owned()));
        assert!(tools.contains(&"target_act".to_owned()));
        assert!(tools.contains(&"control_lease_acquire".to_owned()));
        assert!(tools.contains(&"control_lease_release".to_owned()));
        assert!(tools.contains(&"tool_profile_set".to_owned()));
        assert!(!tools.contains(&"act_run_shell".to_owned()));
        assert!(!tools.contains(&"act_spawn_agent".to_owned()));
        assert!(!tools.contains(&"act_type".to_owned()));
    }

    #[test]
    fn hidden_tool_call_denial_writes_policy_audit_row() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1008-denied-session";
        let error = service
            .admit_tool_call_for_profile("act_type", Some(session_id))
            .expect_err("normal profile must deny hidden foreground typing tool");
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        assert_eq!(code, Some(error_codes::TOOL_PROFILE_POLICY_DENIED));
        let route = error
            .data
            .as_ref()
            .and_then(|data| data.get("capability_route"))
            .expect("capability route in denial");
        assert_eq!(route["hidden_tool"], "act_type");
        let preferred_tools = route["preferred_tools"]
            .as_array()
            .expect("preferred tools array");
        assert!(
            preferred_tools
                .iter()
                .any(|tool| tool.as_str() == Some("target_act verb=set_field"))
        );
        assert!(
            preferred_tools
                .iter()
                .any(|tool| tool.as_str() == Some("browser_set_value"))
        );

        let db = service.m3_storage().expect("storage");
        let audit_rows = db
            .scan_cf_prefix(cf::CF_ACTION_LOG, b"")
            .expect("scan command audit");
        let matching = audit_rows
            .iter()
            .filter(|(_, value)| {
                let text = String::from_utf8_lossy(value);
                text.contains("tool_profile_policy")
                    && text.contains("tool_call_denied")
                    && text.contains("act_type")
                    && text.contains(error_codes::TOOL_PROFILE_POLICY_DENIED)
            })
            .count();
        assert_eq!(matching, 1);
    }

    /// Seeds the cross-session registry with a real `record_initialized` entry
    /// carrying `client_name`, exactly as the HTTP initialize path does
    /// (`transport.rs` -> `record_registry_initialized`).
    fn seed_session_client(service: &SynapseService, session_id: &str, client_name: &str) {
        use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
        use rmcp::transport::streamable_http_server::session::SessionState;
        let state = SessionState::new(InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new(client_name, "0.0.0-test"),
        ));
        service
            .session_registry_handle()
            .lock()
            .expect("session registry lock")
            .record_initialized(session_id, &state, "http", 1_000);
    }

    // The four foreground input primitives that #1031 must restore for local
    // models. They are in BREAK_GLASS_HAZARDOUS_TOOLS and hidden from
    // normal_agent — the exact tools gemma/DeepSeek lacked when they "opened
    // Notepad and typed nothing".
    const LOCAL_AGENT_REQUIRED_TOOLS: [&str; 4] = [
        "act_type",
        "act_set_field_text",
        "act_click",
        "act_focus_window",
    ];

    #[test]
    fn local_model_agent_session_gets_full_capability_surface() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1031-local-session";
        seed_session_client(&service, session_id, "synapse-local-model-agent");

        // tools/list surface SoT: the input primitives are present.
        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("local-agent profile tools"),
        );
        for required in LOCAL_AGENT_REQUIRED_TOOLS {
            assert!(
                tools.contains(&required.to_owned()),
                "local-model agent must see {required}; visible tool count = {}",
                tools.len()
            );
        }

        // Durable policy row SoT: full_capability, auto-assigned source.
        let row = service
            .read_tool_profile_assignment(session_id)
            .expect("read profile row")
            .expect("row exists after resolution");
        assert_eq!(row.record.profile, ToolProfileKind::FullCapability);
        assert_eq!(row.record.source, "default_full_capability_local_agent");
        assert!(row.record.denied_break_glass_tools.is_empty());

        // Physical CF_SESSIONS readback proves the row is on disk.
        let db = service.m3_storage().expect("storage");
        let stored = db
            .scan_cf_prefix(cf::CF_SESSIONS, &tool_profile_key(session_id))
            .expect("scan policy rows");
        assert_eq!(stored.len(), 1);
        assert_eq!(sha256_hex(&stored[0].1), row.value_sha256);

        // Call-admission gate SoT: a hidden-for-normal foreground tool is admitted.
        service
            .admit_tool_call_for_profile("act_type", Some(session_id))
            .expect("full_capability must admit act_type for the local-model agent");
    }

    #[test]
    fn non_local_agent_session_stays_least_privilege() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1031-codex-session";
        seed_session_client(&service, session_id, "codex-cli");

        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("codex profile tools"),
        );
        assert!(tools.contains(&"health".to_owned()));
        for hidden in LOCAL_AGENT_REQUIRED_TOOLS {
            assert!(
                !tools.contains(&hidden.to_owned()),
                "non-local agent must NOT see foreground primitive {hidden}"
            );
        }
        let row = service
            .read_tool_profile_assignment(session_id)
            .expect("read profile row")
            .expect("row exists after resolution");
        assert_eq!(row.record.profile, ToolProfileKind::NormalAgent);
        assert_eq!(row.record.source, "default_normal_agent");
        service
            .admit_tool_call_for_profile("act_type", Some(session_id))
            .expect_err("normal_agent must deny act_type");
    }

    #[test]
    fn local_model_session_self_heals_stale_normal_default() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1031-selfheal-session";

        // Simulate a least-privilege row written before the MCP client identity
        // was known to the registry.
        service
            .write_tool_profile_assignment(
                session_id,
                ToolProfileKind::NormalAgent,
                "default_normal_agent",
                None,
                None,
            )
            .expect("seed stale normal-agent row");
        let before = service
            .read_tool_profile_assignment(session_id)
            .expect("read before")
            .expect("row before");
        assert_eq!(before.record.profile, ToolProfileKind::NormalAgent);

        // Identity now classifies the session as the local-model harness.
        seed_session_client(&service, session_id, "synapse-local-model-agent");

        // Resolving the surface upgrades the durable row in place.
        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("post-heal tools"),
        );
        assert!(tools.contains(&"act_type".to_owned()));
        let after = service
            .read_tool_profile_assignment(session_id)
            .expect("read after")
            .expect("row after");
        assert_eq!(after.record.profile, ToolProfileKind::FullCapability);
        assert_eq!(after.record.source, "default_full_capability_local_agent");
    }

    #[test]
    fn explicit_tool_profile_set_to_full_capability_requires_lease_proof() {
        // The frictionless path to full_capability is the auto default; an
        // explicit escalation by hand must carry the same proof as break_glass
        // so no agent can self-escalate to the foreground primitives.
        let proof = ToolProfileLeaseProof {
            required: true,
            held: false,
            owner_session_id: None,
            caller_is_owner: false,
            expires_in_ms: None,
        };
        assert!(
            validate_profile_set_policy(
                "s1",
                ToolProfileKind::FullCapability,
                Some("need full surface"),
                true,
                &proof,
            )
            .is_err()
        );
    }

    /// Measurement probe (not a regression gate): emit the EXACT FullCapability
    /// tool surface a Synapse-spawned local-model agent (gemma/DeepSeek) receives,
    /// as physical source of truth, plus the real byte size + token estimate of
    /// the OpenAI `tools` array that the local-agent harness puts on the
    /// chat-completion request body.
    ///
    /// Faithful reproduction of the production path:
    ///   1. a session whose MCP client identity is the local-model harness
    ///      (`synapse-local-model-agent`) resolves to `ToolProfileKind::FullCapability`,
    ///   2. `tools_for_session_profile` returns `full_sanitized_tools()` (=
    ///      `sanitize_tools(tool_router.list_all())`) sorted exactly as the
    ///      handler emits them — the same `Vec<Tool>` the agent's `tools/list`
    ///      receives,
    ///   3. each `Tool` is mapped through the SAME JSON shape as
    ///      `local_agent::openai_tool_from_mcp` (kept in sync below) to build the
    ///      `tools[]` field of the request body.
    ///
    /// Skipped unless `SYNAPSE_TOOL_SURFACE_OUT` is set to the absolute output
    /// path, so it never writes during a normal `cargo test` run.
    ///
    /// ```text
    /// SYNAPSE_TOOL_SURFACE_OUT=C:/code/synapse-subconscious/artifacts/tool_surface.json \
    ///   cargo test -p synapse-mcp --lib -- --ignored --exact \
    ///   server::tool_profiles::tests::emit_full_capability_tool_surface_artifact --nocapture
    /// ```
    #[test]
    #[ignore = "measurement probe; set SYNAPSE_TOOL_SURFACE_OUT to run"]
    fn emit_full_capability_tool_surface_artifact() {
        let Ok(out_path) = std::env::var("SYNAPSE_TOOL_SURFACE_OUT") else {
            eprintln!("SYNAPSE_TOOL_SURFACE_OUT not set; skipping artifact emission");
            return;
        };

        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "tool-surface-probe-local-session";
        // Production identity for the Synapse local-model harness; this is what
        // makes the session resolve to FullCapability automatically.
        seed_session_client(&service, session_id, "synapse-local-model-agent");

        // The exact Vec<Tool> the agent's tools/list receives for this session.
        let tools = service
            .tools_for_session_profile(Some(session_id))
            .expect("full-capability tool surface");

        // Hard SoT assertion: this session really is FullCapability.
        let row = service
            .read_tool_profile_assignment(session_id)
            .expect("read profile row")
            .expect("row exists");
        assert_eq!(row.record.profile, ToolProfileKind::FullCapability);

        // Map each Tool exactly as local_agent::openai_tool_from_mcp does.
        // (That fn lives in the binary's local_agent module; its body is a pure
        // JSON wrapper with no schema logic — the schema work already happened in
        // sanitize_tools above. Kept byte-identical here.)
        let openai_tool_from_mcp = |tool: &Tool| -> serde_json::Value {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name.as_ref(),
                    "description": tool
                        .description
                        .as_ref()
                        .map(|desc| desc.as_ref())
                        .unwrap_or("Synapse MCP tool"),
                    "parameters": serde_json::Value::Object((*tool.input_schema).clone()),
                }
            })
        };

        let openai_tools: Vec<serde_json::Value> = tools.iter().map(openai_tool_from_mcp).collect();

        // The actual `tools` field of the chat-completion request body.
        let openai_tools_json =
            serde_json::to_string(&openai_tools).expect("serialize openai tools array");
        let openai_tools_bytes = openai_tools_json.len();
        let openai_tools_chars = openai_tools_json.chars().count();

        // Per-tool detail (sanitized schema, exactly as emitted to the model) and
        // the longest descriptions by byte size.
        let mut tool_entries: Vec<serde_json::Value> = Vec::with_capacity(tools.len());
        let mut desc_sizes: Vec<(String, usize)> = Vec::with_capacity(tools.len());
        for tool in &tools {
            let name = tool.name.to_string();
            let description = tool
                .description
                .as_ref()
                .map(|desc| desc.as_ref())
                .unwrap_or("Synapse MCP tool")
                .to_owned();
            desc_sizes.push((name.clone(), description.len()));
            tool_entries.push(serde_json::json!({
                "name": name,
                "description": description,
                "input_schema": serde_json::Value::Object((*tool.input_schema).clone()),
            }));
        }
        desc_sizes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let longest_descriptions: Vec<serde_json::Value> = desc_sizes
            .iter()
            .take(10)
            .map(|(name, bytes)| serde_json::json!({ "name": name, "description_bytes": bytes }))
            .collect();

        // Token estimates over the actual openai_tools payload. No real
        // tokenizer crate is in the workspace, so report both common heuristics,
        // clearly labeled as estimates.
        let approx_tokens_chars_div_4 = (openai_tools_chars as f64 / 4.0).ceil() as u64;
        let approx_tokens_chars_div_3_5 = (openai_tools_chars as f64 / 3.5).ceil() as u64;

        let artifact = serde_json::json!({
            "_meta": {
                "description": "Exact FullCapability MCP tool surface a Synapse-spawned local-model agent (e.g. gemma4) receives, with the real openai tools[] payload size and token estimate.",
                "profile": row.record.profile.as_str(),
                "profile_source": row.record.source,
                "client_identity": "synapse-local-model-agent",
                "produced_by": "server::tool_profiles::tests::emit_full_capability_tool_surface_artifact",
                "env_gates": {
                    "SYNAPSE_DEBUG_TOOLS": std::env::var("SYNAPSE_DEBUG_TOOLS").ok(),
                    "SYNAPSE_ENABLE_EVERQUEST": std::env::var("SYNAPSE_ENABLE_EVERQUEST").ok(),
                },
            },
            "tool_count": tools.len(),
            "openai_tools_bytes": openai_tools_bytes,
            "openai_tools_chars": openai_tools_chars,
            "approx_tokens": {
                "note": "No real tokenizer crate is vendored in this workspace; both values are heuristic estimates over the openai_tools payload.",
                "chars_div_4": approx_tokens_chars_div_4,
                "chars_div_3_5": approx_tokens_chars_div_3_5,
            },
            "longest_descriptions_by_bytes": longest_descriptions,
            "tools": tool_entries,
        });

        let out = std::path::Path::new(&out_path);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).expect("create artifact parent dir");
        }
        let serialized = serde_json::to_string_pretty(&artifact).expect("serialize artifact");
        std::fs::write(out, serialized).expect("write artifact");

        eprintln!("TOOL_SURFACE tool_count={}", tools.len());
        eprintln!("TOOL_SURFACE openai_tools_bytes={openai_tools_bytes}");
        eprintln!("TOOL_SURFACE openai_tools_chars={openai_tools_chars}");
        eprintln!("TOOL_SURFACE approx_tokens_chars_div_4={approx_tokens_chars_div_4}");
        eprintln!("TOOL_SURFACE approx_tokens_chars_div_3_5={approx_tokens_chars_div_3_5}");
        for (name, bytes) in desc_sizes.iter().take(10) {
            eprintln!("TOOL_SURFACE longest_desc {name} {bytes}");
        }
        eprintln!("TOOL_SURFACE written_to={out_path}");
    }
}

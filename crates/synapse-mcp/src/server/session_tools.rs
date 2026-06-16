//! Cross-session registry MCP tools for multi-agent coordination (#794).
//!
//! The registry is a read model: HTTP lifecycle/heartbeat state is joined with
//! the existing active-target registry and input lease snapshot at read time.
//! It does not gate any action/perception path.

use std::collections::{BTreeMap, BTreeSet};

use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_action::lease;
use synapse_core::error_codes;

use super::{
    ErrorData, Json, Parameters, SessionTarget, SynapseService, TargetWire,
    agent_state::{AgentLifecycleState, AgentStateRead},
    mcp_error,
    session_registry::SessionRegistryRead,
    session_registry::unix_time_ms_now,
    target_claims::TargetClaimRead,
    tool, tool_router,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {
    /// Include explicitly closed sessions. Live and stale sessions are always
    /// included because stale peers are part of the crash/disconnect readback.
    #[serde(default)]
    #[schemars(default)]
    pub include_closed: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusParams {
    /// MCP Streamable HTTP session id to inspect.
    pub session_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionEndParams {
    /// Optional explicit session id. When supplied it must match the caller's
    /// current MCP session id; one session may not tear down another session.
    #[serde(default)]
    #[schemars(default)]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionLeaseReadback {
    pub held: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    pub is_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquired_at_ms_ago: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renewed_at_ms_ago: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    #[serde(flatten)]
    pub registry: SessionRegistryRead,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_target: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_claims: Vec<TargetClaimRead>,
    pub lease: SessionLeaseReadback,
    /// #898 lifecycle state machine read for this session's agent: state,
    /// reason code, heartbeat, waiting_for detail, runaway flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_state: Option<AgentStateRead>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListResponse {
    pub now_unix_ms: u64,
    pub stale_after_ms: u64,
    pub registry_entry_count: usize,
    pub target_session_count: usize,
    pub returned_count: usize,
    pub input_lease_held: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_lease_owner_session_id: Option<String>,
    pub sessions: Vec<SessionSummary>,
    /// #898: agents tracked by the state machine that have no MCP session
    /// (in-flight spawns and active attention rows before registration).
    /// Terminal/dead history is split out below so default consumers do not
    /// page on already-ended agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unbound_agent_states: Vec<AgentStateRead>,
    /// Terminal unbound history retained for diagnostics. These rows are not
    /// actionable attention and must not be counted as stuck/live work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal_unbound_agent_states: Vec<AgentStateRead>,
    pub unbound_agent_filter: SessionUnboundAgentFilterReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUnboundAgentFilterReadback {
    pub source_of_truth: &'static str,
    pub active_unbound_agent_count: usize,
    pub terminal_unbound_agent_count: usize,
    pub terminal_states: Vec<&'static str>,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusResponse {
    pub now_unix_ms: u64,
    pub stale_after_ms: u64,
    pub found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionSummary>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionEndResponse {
    pub report: crate::server::session_lifecycle::SessionTeardownReport,
}

#[tool_router(router = session_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "List all known MCP sessions as a non-blocking cross-session read model: session id, client kind, liveness, heartbeat, active target, input-lease ownership, and last JSON-RPC tool action. Stale sessions are reported rather than hidden."
    )]
    pub async fn session_list(
        &self,
        params: Parameters<SessionListParams>,
    ) -> Result<Json<SessionListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "session_list",
            "tool.invocation kind=session_list"
        );
        self.session_list_impl(params.0.include_closed).map(Json)
    }

    #[tool(
        description = "Return one MCP session's registry row joined with active target and input-lease state. Unknown sessions return found=false instead of blocking or scanning external state."
    )]
    pub async fn session_status(
        &self,
        params: Parameters<SessionStatusParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<SessionStatusResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "session_status",
            "tool.invocation kind=session_status"
        );
        validate_session_id(&params.0.session_id)?;
        self.session_status_impl(&params.0.session_id).map(Json)
    }

    #[tool(
        description = "Explicitly end this MCP session and atomically reclaim all resources owned by it: held inputs, input lease, active target, virtual clipboard buffer, CDP targets, durable shell jobs, launched process resources, event subscriptions, persisted session row, and registry lifecycle. The optional session_id must equal the current caller session."
    )]
    pub async fn session_end(
        &self,
        params: Parameters<SessionEndParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<SessionEndResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "session_end",
            "tool.invocation kind=session_end"
        );
        let current_session_id = super::context::mcp_session_id_from_request_context(
            &request_context,
        )?
        .ok_or_else(|| {
            mcp_error(
                error_codes::HTTP_SESSION_INVALID,
                "session_end requires an MCP session id",
            )
        })?;
        let params = params.0;
        let requested_session_id = params.session_id.clone();
        let target_session_id = match requested_session_id.clone() {
            Some(session_id) => {
                validate_session_id(&session_id)?;
                if session_id != current_session_id {
                    return Err(ErrorData::new(
                        ErrorCode(-32099),
                        "session_end can only end the current MCP session",
                        Some(json!({
                            "code": error_codes::TOOL_PARAMS_INVALID,
                            "current_session_id": current_session_id,
                            "requested_session_id": session_id,
                        })),
                    ));
                }
                session_id
            }
            None => current_session_id.clone(),
        };
        let command_payload = json!({
            "requested_session_id": &requested_session_id,
            "target_session_id": &target_session_id,
        });
        let command_before = json!({
            "source_of_truth": "session lifecycle registry, input lease, target/session-owned resources",
            "target_session_id": &target_session_id,
            "session_status": self.session_status_impl(&target_session_id).ok(),
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "session_end",
            "kill",
            Some(current_session_id.clone()),
            Some(target_session_id.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let lifecycle = self.session_lifecycle_state()?;
        let report = match lifecycle
            .teardown_session(&target_session_id, "explicit_session_end")
            .await
        {
            Ok(report) => report,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "session_end",
                        "kill",
                        Some(current_session_id.clone()),
                        Some(target_session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": "session lifecycle registry, input lease, target/session-owned resources",
                            "session_status": self.session_status_impl(&target_session_id).ok(),
                        }),
                        "error",
                    )
                    .with_error(super::command_audit::command_audit_error_from_error_data(
                        &error,
                    )),
                )?;
                return Err(error);
            }
        };
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "session_end",
            "kill",
            Some(current_session_id.clone()),
            Some(target_session_id.clone()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": "session lifecycle registry, input lease, target/session-owned resources",
                "report": &report,
                "session_status": self.session_status_impl(&target_session_id).ok(),
            }),
            "ok",
        ))?;
        Ok(Json(SessionEndResponse { report }))
    }
}

impl SynapseService {
    pub(crate) fn session_list_impl(
        &self,
        include_closed: bool,
    ) -> Result<SessionListResponse, ErrorData> {
        let now_unix_ms = unix_time_ms_now();
        let (registry_reads, stale_after_ms, registry_entry_count) =
            self.session_registry_reads(now_unix_ms)?;
        let targets = self.session_target_wires()?;
        let target_claims_by_owner = self.target_claim_reads_by_owner()?;
        let lease_status = lease::status();
        let mut session_ids = registry_reads
            .keys()
            .chain(targets.keys())
            .chain(target_claims_by_owner.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Some(owner) = lease_status.owner_session_id.as_ref() {
            session_ids.insert(owner.clone());
        }
        let mut sessions = Vec::new();
        for session_id in session_ids {
            let Some(summary) = build_session_summary(
                &session_id,
                registry_reads.get(&session_id).cloned(),
                targets.get(&session_id).cloned(),
                target_claims_by_owner
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default(),
                &lease_status,
                now_unix_ms,
                stale_after_ms,
            ) else {
                continue;
            };
            if !include_closed && summary.registry.lifecycle == "closed" {
                continue;
            }
            sessions.push(summary);
        }
        sessions.sort_by(|a, b| a.registry.session_id.cmp(&b.registry.session_id));
        let returned_count = sessions.len();
        let raw_unbound_agent_states = super::agent_state::unbound_reads(now_unix_ms);
        let (unbound_agent_states, terminal_unbound_agent_states, unbound_agent_filter) =
            split_unbound_agent_states(raw_unbound_agent_states);
        Ok(SessionListResponse {
            now_unix_ms,
            stale_after_ms,
            registry_entry_count,
            target_session_count: targets.len(),
            returned_count,
            input_lease_held: lease_status.held,
            input_lease_owner_session_id: lease_status.owner_session_id.clone(),
            sessions,
            unbound_agent_states,
            terminal_unbound_agent_states,
            unbound_agent_filter,
        })
    }

    pub(crate) fn session_status_impl(
        &self,
        session_id: &str,
    ) -> Result<SessionStatusResponse, ErrorData> {
        let now_unix_ms = unix_time_ms_now();
        let (registry_reads, stale_after_ms, _registry_entry_count) =
            self.session_registry_reads(now_unix_ms)?;
        let active_target = self
            .session_target(Some(session_id))?
            .as_ref()
            .map(session_target_wire);
        let target_claims = self
            .target_claim_reads_by_owner()?
            .remove(session_id)
            .unwrap_or_default();
        let lease_status = lease::status();
        let session = build_session_summary(
            session_id,
            registry_reads.get(session_id).cloned(),
            active_target,
            target_claims,
            &lease_status,
            now_unix_ms,
            stale_after_ms,
        );
        Ok(SessionStatusResponse {
            now_unix_ms,
            stale_after_ms,
            found: session.is_some(),
            session,
        })
    }

    fn session_registry_reads(
        &self,
        now_unix_ms: u64,
    ) -> Result<(BTreeMap<String, SessionRegistryRead>, u64, usize), ErrorData> {
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned",
            )
        })?;
        let stale_after_ms = guard.stale_after_ms();
        let reads = guard
            .reads(now_unix_ms)
            .into_iter()
            .map(|entry| (entry.session_id.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let count = reads.len();
        drop(guard);
        Ok((reads, stale_after_ms, count))
    }

    fn session_target_wires(&self) -> Result<BTreeMap<String, TargetWire>, ErrorData> {
        let guard = self.session_targets_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session target registry lock poisoned",
            )
        })?;
        let targets = guard
            .iter()
            .map(|(session_id, target)| (session_id.clone(), session_target_wire(target)))
            .collect::<BTreeMap<_, _>>();
        drop(guard);
        Ok(targets)
    }
}

fn split_unbound_agent_states(
    rows: Vec<AgentStateRead>,
) -> (
    Vec<AgentStateRead>,
    Vec<AgentStateRead>,
    SessionUnboundAgentFilterReadback,
) {
    let mut active_rows = Vec::new();
    let mut terminal_rows = Vec::new();
    for row in rows {
        if unbound_agent_row_is_terminal(&row) {
            terminal_rows.push(row);
        } else {
            active_rows.push(row);
        }
    }
    let filter = SessionUnboundAgentFilterReadback {
        source_of_truth: "agent_state::unbound_reads split by lifecycle state",
        active_unbound_agent_count: active_rows.len(),
        terminal_unbound_agent_count: terminal_rows.len(),
        terminal_states: vec!["dead"],
        reason: "terminal unbound history is diagnostic history, not actionable attention",
    };
    (active_rows, terminal_rows, filter)
}

fn unbound_agent_row_is_terminal(row: &AgentStateRead) -> bool {
    matches!(row.state, AgentLifecycleState::Dead)
}

fn build_session_summary(
    session_id: &str,
    registry: Option<SessionRegistryRead>,
    active_target: Option<TargetWire>,
    target_claims: Vec<TargetClaimRead>,
    lease_status: &synapse_action::LeaseStatus,
    now_unix_ms: u64,
    stale_after_ms: u64,
) -> Option<SessionSummary> {
    let registry = registry.or_else(|| {
        (active_target.is_some()
            || !target_claims.is_empty()
            || lease_status.owner_session_id.as_deref() == Some(session_id))
        .then(|| synthetic_registry_read(session_id, now_unix_ms, stale_after_ms))
    })?;
    Some(SessionSummary {
        registry,
        active_target,
        target_claims,
        lease: SessionLeaseReadback {
            held: lease_status.held,
            owner_session_id: lease_status.owner_session_id.clone(),
            is_owner: lease_status.owner_session_id.as_deref() == Some(session_id),
            acquired_at_ms_ago: lease_status.acquired_at_ms_ago,
            renewed_at_ms_ago: lease_status.renewed_at_ms_ago,
            ttl_ms: lease_status.ttl_ms,
            expires_in_ms: lease_status.expires_in_ms,
        },
        agent_state: super::agent_state::read_for_session(session_id, now_unix_ms),
    })
}

fn synthetic_registry_read(
    session_id: &str,
    now_unix_ms: u64,
    stale_after_ms: u64,
) -> SessionRegistryRead {
    SessionRegistryRead {
        session_id: session_id.to_owned(),
        transport: "unknown".to_owned(),
        client_name: None,
        client_version: None,
        protocol_version: None,
        agent_kind: "unknown".to_owned(),
        lifecycle: "unregistered".to_owned(),
        started_at_unix_ms: now_unix_ms,
        last_seen_unix_ms: now_unix_ms,
        last_seen_ms_ago: 0,
        stale_after_ms,
        closed_at_unix_ms: None,
        last_action: None,
        last_reason_code: None,
        last_tools_list_unix_ms: None,
        last_tools_list_visible_tool_count: None,
        last_tools_list_visible_tool_sha256: None,
        spawned_agent: None,
    }
}

fn session_target_wire(target: &SessionTarget) -> TargetWire {
    match target {
        SessionTarget::Window { hwnd } => TargetWire::Window { window_hwnd: *hwnd },
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => TargetWire::Cdp {
            window_hwnd: *window_hwnd,
            cdp_target_id: cdp_target_id.clone(),
        },
    }
}

pub(crate) fn validate_session_id(session_id: &str) -> Result<(), ErrorData> {
    if session_id.trim().is_empty() {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_id must not be empty",
            Some(json!({"code": error_codes::TOOL_PARAMS_INVALID})),
        ));
    }
    if session_id.chars().count() > 512 {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_id must be at most 512 Unicode scalar values",
            Some(json!({"code": error_codes::TOOL_PARAMS_INVALID})),
        ));
    }
    if !session_id.chars().all(|ch| ('!'..='~').contains(&ch)) {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_id must contain only visible ASCII characters",
            Some(json!({"code": error_codes::TOOL_PARAMS_INVALID})),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use synapse_core::AgentEventKind;

    #[test]
    fn session_status_rejects_empty_or_non_visible_ascii_ids() {
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("abc def").is_err());
        assert!(validate_session_id("abc\n").is_err());
        assert!(validate_session_id("session-1").is_ok());
    }

    #[test]
    fn synthetic_entries_cover_target_or_lease_only_sessions() {
        let session_id = "lease-only";
        let lease_status = synapse_action::LeaseStatus {
            held: true,
            owner_session_id: Some(session_id.to_owned()),
            acquired_at_ms_ago: Some(1),
            renewed_at_ms_ago: Some(1),
            ttl_ms: Some(30_000),
            expires_in_ms: Some(29_999),
        };
        let summary = build_session_summary(
            session_id,
            None,
            None,
            Vec::new(),
            &lease_status,
            1_000,
            500,
        )
        .unwrap();
        assert_eq!(summary.registry.lifecycle, "unregistered");
        assert!(summary.lease.is_owner);
    }

    #[test]
    fn terminal_unbound_agents_are_split_from_actionable_session_list_rows() {
        let rows = vec![
            agent_read("active-working", AgentLifecycleState::Working, None),
            agent_read(
                "active-stuck",
                AgentLifecycleState::Stuck,
                Some("silent_timeout"),
            ),
            agent_read(
                "dead-local-model",
                AgentLifecycleState::Dead,
                Some("local_model_registry_row_missing"),
            ),
            agent_read(
                "active-needs-input",
                AgentLifecycleState::NeedsInput,
                Some("permission_prompt"),
            ),
        ];
        let (active, terminal, filter) = split_unbound_agent_states(rows);

        assert_eq!(
            active
                .iter()
                .map(|row| row.anchor.as_str())
                .collect::<Vec<_>>(),
            vec!["active-working", "active-stuck", "active-needs-input"]
        );
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].anchor, "dead-local-model");
        assert_eq!(terminal[0].state, AgentLifecycleState::Dead);
        assert_eq!(filter.active_unbound_agent_count, 3);
        assert_eq!(filter.terminal_unbound_agent_count, 1);
        assert!(filter.reason.contains("not actionable attention"));
    }

    fn agent_read(
        anchor: &str,
        state: AgentLifecycleState,
        reason_code: Option<&str>,
    ) -> AgentStateRead {
        AgentStateRead {
            anchor: anchor.to_owned(),
            spawn_id: Some(anchor.to_owned()),
            session_id: None,
            agent_kind: Some("test-agent".to_owned()),
            state,
            reason_code: reason_code.map(str::to_owned),
            since_unix_ms: 1_000,
            last_event_unix_ms: 1_000,
            last_event_kind: AgentEventKind::StateChanged,
            silent_ms: 0,
            waiting_for: None,
            runaway: false,
            consecutive_identical_tool_calls: 0,
            last_tool_name: None,
            launcher_process_id: None,
            agent_process_id: None,
            log_dir: None,
        }
    }
}

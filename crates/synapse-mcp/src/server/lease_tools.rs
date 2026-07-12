//! MCP tools for the multi-agent input lease (epic #719, issue #733).
//!
//! The lease is the coordination primitive that lets many agents share the
//! single real foreground/cursor without interleaving: an agent acquires it
//! before a leased-foreground action and releases it after. Background tiers
//! (CDP/UIA/PostMessage) never touch it. These tools expose the lease over MCP
//! so an agent (or operator) can explicitly acquire/release/inspect it, and the
//! lease state is also surfaced under `health.subsystems.action`.
//!
//! The lease is keyed by `Mcp-Session-Id`, so every lease tool requires a
//! session. In the shared-daemon HTTP deployment each agent terminal has its
//! own session id; a missing session id is a fail-loud `TOOL_PARAMS_INVALID`.

use super::{
    ErrorData, Json, Parameters, SynapseService, empty_input_schema, mcp_error,
    session_registry::{SessionRegistryRead, unix_time_ms_now},
    session_tools::validate_session_id,
    tool, tool_router,
};
use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_action::{LeaseError, LeaseOutcome, LeaseStatus, lease};
use synapse_core::error_codes;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlLeaseAcquireParams {
    /// Lease lifetime in milliseconds. Must be in [100, 30000]. The lease is
    /// renewed on every leased action and on a repeat acquire by the holder, so
    /// a short TTL is the safety floor against a crashed holder, not a hard cap
    /// on how long real work can take.
    #[serde(default = "default_lease_ttl_ms")]
    #[schemars(default = "default_lease_ttl_ms", range(min = 100, max = 30000))]
    pub ttl_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlLeaseHandoffParams {
    /// Live MCP session id that should receive the foreground input lease.
    pub to_session: String,
    /// Fresh lease lifetime in milliseconds for the recipient. Must be in
    /// [100, 30000] like `control_lease_acquire`.
    #[serde(default = "default_lease_ttl_ms")]
    #[schemars(default = "default_lease_ttl_ms", range(min = 100, max = 30000))]
    pub ttl_ms: u64,
}

const fn default_lease_ttl_ms() -> u64 {
    synapse_action::DEFAULT_LEASE_TTL_MS
}

/// Flattened lease snapshot returned by every lease tool. `LeaseStatus` lives in
/// `synapse-action` (no `schemars` dep there), so its fields are flattened here
/// rather than embedded, keeping the action crate schema-free.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlLeaseResponse {
    /// One of: `acquired`, `renewed`, `released`, `not_held`, `status`.
    /// `not_held` is an idempotent release of a lease this session did not
    /// hold (already expired or never acquired) — a success, not a failure.
    pub outcome: String,
    /// Whether the lease is currently held by anyone.
    pub held: bool,
    /// The session id of the current holder, if any.
    pub owner_session_id: Option<String>,
    /// The session id that issued this tool call.
    pub this_session_id: String,
    /// Whether the calling session is the current holder.
    pub is_owner: bool,
    /// Release outcomes only (#1556): whether this call actually released a
    /// lease. `false` on an idempotent no-op release of an unheld lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released: Option<bool>,
    /// Release outcomes only (#1556): whether a lease owned by this session was
    /// held at release time. `false` when the lease had already expired or was
    /// never acquired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was_held: Option<bool>,
    pub acquired_at_ms_ago: Option<u64>,
    pub renewed_at_ms_ago: Option<u64>,
    pub ttl_ms: Option<u64>,
    pub expires_in_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DashboardControlLeaseForceReleaseResponse {
    pub requested_owner_session_id: String,
    pub confirmed: bool,
    pub released: bool,
    pub before: ControlLeaseResponse,
    pub after: ControlLeaseResponse,
    pub persisted_row_deleted: bool,
    pub source_of_truth: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DashboardControlLeaseHandoffResponse {
    pub from_session_id: String,
    pub to_session_id: String,
    pub ttl_ms: u64,
    pub before_from: ControlLeaseResponse,
    pub before_to: ControlLeaseResponse,
    pub response: ControlLeaseResponse,
    pub after_from: ControlLeaseResponse,
    pub after_to: ControlLeaseResponse,
    pub source_of_truth: &'static str,
}

impl ControlLeaseResponse {
    fn from_status(outcome: &str, this_session_id: String, status: &LeaseStatus) -> Self {
        let is_owner = status.owner_session_id.as_deref() == Some(this_session_id.as_str());
        Self {
            outcome: outcome.to_owned(),
            held: status.held,
            owner_session_id: status.owner_session_id.clone(),
            this_session_id,
            is_owner,
            released: None,
            was_held: None,
            acquired_at_ms_ago: status.acquired_at_ms_ago,
            renewed_at_ms_ago: status.renewed_at_ms_ago,
            ttl_ms: status.ttl_ms,
            expires_in_ms: status.expires_in_ms,
        }
    }

    /// Readback for a release that actually freed a lease this session held.
    fn released(this_session_id: String, status: &LeaseStatus) -> Self {
        let mut response = Self::from_status("released", this_session_id, status);
        response.released = Some(true);
        response.was_held = Some(true);
        response
    }

    /// Readback for an idempotent no-op release (#1556): the session held no
    /// lease (already expired or never acquired), so nothing was freed. The
    /// physical truth — lease unheld, nobody owns it — is stated explicitly.
    fn release_noop(this_session_id: String) -> Self {
        let mut response = Self::from_status("not_held", this_session_id, &LeaseStatus::unheld());
        response.released = Some(false);
        response.was_held = Some(false);
        response
    }
}

const DASHBOARD_LEASE_SOURCE_OF_TRUTH: &str =
    "synapse_action::lease + CF_KV MCP session lease rows + CF_AGENT_EVENTS + CF_ACTION_LOG";
pub(super) const LEASE_TTL_SOURCE_OF_TRUTH: &str =
    "request ttl_ms before synapse_action::lease mutation";

#[tool_router(router = lease_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Acquire (or renew) the process-global input lease for this MCP session. The lease serializes real foreground/cursor/keyboard/clipboard actions across agents; background tiers (CDP/UIA/PostMessage) never need it. Refuse-not-block: if another live session holds it, returns ACTION_FOREGROUND_LEASE_BUSY with the current holder and a retry hint instead of waiting."
    )]
    pub async fn control_lease_acquire(
        &self,
        params: Parameters<ControlLeaseAcquireParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ControlLeaseResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "control_lease_acquire",
            "tool.invocation kind=control_lease_acquire"
        );
        let params = params.0;
        validate_lease_ttl_ms("control_lease_acquire", params.ttl_ms)?;
        let session_id = require_lease_session_id(&request_context)?;
        self.restore_session_lease_if_needed(&session_id)?;
        let command_payload = json!({ "ttl_ms": params.ttl_ms });
        let command_before = json!({
            "source_of_truth": "synapse_action::lease",
            "caller": lease_status_for_session(&session_id),
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "control_lease_acquire",
            "lease_acquire",
            Some(session_id.clone()),
            Some(session_id.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let response = match acquire_lease_for_session(&session_id, params.ttl_ms) {
            Ok(response) => response,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "control_lease_acquire",
                        "lease_acquire",
                        Some(session_id.clone()),
                        Some(session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": "synapse_action::lease",
                            "caller": lease_status_for_session(&session_id),
                        }),
                        "error",
                    )
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };
        let status = lease::status();
        if let Err(error) = self.persist_session_lease(&session_id, &status) {
            let released = lease::release_if_owner(&session_id);
            tracing::error!(
                code = error_codes::TOOL_INTERNAL_ERROR,
                session_id,
                released_after_persist_failure = released,
                error = ?error,
                "input lease acquire failed durability write; released in-memory lease before returning error"
            );
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "control_lease_acquire",
                    "lease_acquire",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "synapse_action::lease",
                        "released_after_persist_failure": released,
                        "caller": lease_status_for_session(&session_id),
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&error),
                ),
            )?;
            return Err(error);
        }
        // Journal first-time acquisitions (#897); renewals happen on every
        // leased action and would flood the journal without adding signal.
        // An acquisition that cannot be journaled is rolled back the same
        // way as a persist failure: control-plane state must stay auditable.
        if response.outcome == "acquired"
            && let Err(journal_error) = self.journal_lease_event(
                synapse_core::AgentEventKind::LeaseAcquired,
                &session_id,
                None,
                status.ttl_ms,
            )
        {
            let released = lease::release_if_owner(&session_id);
            let persisted_row_deleted = self.delete_persisted_session_lease(&session_id).is_ok();
            tracing::error!(
                code = error_codes::TOOL_INTERNAL_ERROR,
                session_id,
                released_after_journal_failure = released,
                persisted_row_deleted,
                error = ?journal_error,
                "input lease acquire could not journal its agent event; rolled the acquisition back"
            );
            let tool_error = super::agent_events::agent_event_tool_error(
                "control_lease_acquire",
                &journal_error,
                false,
            );
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "control_lease_acquire",
                    "lease_acquire",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "synapse_action::lease",
                        "released_after_journal_failure": released,
                        "persisted_row_deleted": persisted_row_deleted,
                        "caller": lease_status_for_session(&session_id),
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&tool_error),
                ),
            )?;
            return Err(tool_error);
        }
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "control_lease_acquire",
            "lease_acquire",
            Some(session_id.clone()),
            Some(session_id.clone()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": "synapse_action::lease",
                "response": &response,
                "caller": lease_status_for_session(&session_id),
            }),
            "ok",
        ))?;
        Ok(Json(response))
    }

    #[tool(
        description = "Release the input lease held by this MCP session. Idempotent: releasing a lease this session does not hold (already expired or never acquired) succeeds with outcome=not_held (released=false, was_held=false), not an error. Errors with ACTION_FOREGROUND_LEASE_NOT_HELD only when the lease is currently held by a DIFFERENT session.",
        input_schema = empty_input_schema()
    )]
    pub async fn control_lease_release(
        &self,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ControlLeaseResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "control_lease_release",
            "tool.invocation kind=control_lease_release"
        );
        let session_id = require_lease_session_id(&request_context)?;
        self.restore_session_lease_if_needed(&session_id)?;
        let command_payload = json!({});
        let command_before = json!({
            "source_of_truth": "synapse_action::lease",
            "caller": lease_status_for_session(&session_id),
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "control_lease_release",
            "lease_release",
            Some(session_id.clone()),
            Some(session_id.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let response = match release_lease_for_session(&session_id) {
            Ok(response) => response,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "control_lease_release",
                        "lease_release",
                        Some(session_id.clone()),
                        Some(session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": "synapse_action::lease",
                            "caller": lease_status_for_session(&session_id),
                        }),
                        "error",
                    )
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };
        if let Err(error) = self.delete_persisted_session_lease(&session_id) {
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "control_lease_release",
                    "lease_release",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "synapse_action::lease",
                        "response": &response,
                        "caller": lease_status_for_session(&session_id),
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&error),
                ),
            )?;
            return Err(error);
        }
        // The release is already committed; a journal failure is surfaced
        // with that context instead of pretending the release failed.
        if let Err(error) = self.journal_lease_event(
            synapse_core::AgentEventKind::LeaseReleased,
            &session_id,
            None,
            None,
        ) {
            let tool_error =
                super::agent_events::agent_event_tool_error("control_lease_release", &error, true);
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "control_lease_release",
                    "lease_release",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "synapse_action::lease",
                        "response": &response,
                        "caller": lease_status_for_session(&session_id),
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&tool_error),
                ),
            )?;
            return Err(tool_error);
        }
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "control_lease_release",
            "lease_release",
            Some(session_id.clone()),
            Some(session_id.clone()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": "synapse_action::lease",
                "response": &response,
                "caller": lease_status_for_session(&session_id),
            }),
            "ok",
        ))?;
        Ok(Json(response))
    }

    #[tool(
        description = "Atomically hand off the input lease from this MCP session to a named live peer without releasing it into a race. The caller must be the current holder; the recipient must be a live registered MCP session. Stale, closed, unknown, malformed, or self recipients fail closed."
    )]
    pub async fn control_lease_handoff(
        &self,
        params: Parameters<ControlLeaseHandoffParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ControlLeaseResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "control_lease_handoff",
            "tool.invocation kind=control_lease_handoff"
        );
        let params = params.0;
        validate_lease_ttl_ms("control_lease_handoff", params.ttl_ms)?;
        let session_id = require_lease_session_id(&request_context)?;
        self.restore_session_lease_if_needed(&session_id)?;
        let to_session = params.to_session;
        self.ensure_handoff_recipient_live(&session_id, &to_session)?;
        let command_payload = json!({
            "to_session": &to_session,
            "ttl_ms": params.ttl_ms,
        });
        let command_before = json!({
            "source_of_truth": "synapse_action::lease",
            "from": lease_status_for_session(&session_id),
            "to": lease_status_for_session(&to_session),
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "control_lease_handoff",
            "lease_handoff",
            Some(session_id.clone()),
            Some(to_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let handoff = match handoff_lease_for_session(&session_id, &to_session, params.ttl_ms) {
            Ok(handoff) => handoff,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "control_lease_handoff",
                        "lease_handoff",
                        Some(session_id.clone()),
                        Some(to_session.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": "synapse_action::lease",
                            "from": lease_status_for_session(&session_id),
                            "to": lease_status_for_session(&to_session),
                        }),
                        "error",
                    )
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };
        let response =
            ControlLeaseResponse::from_status("handed_off", session_id.clone(), &handoff.current);
        let persist_readback =
            match self.persist_session_lease_handoff(&session_id, &to_session, &handoff.current) {
                Ok(readback) => readback,
                Err(error) => {
                    self.rollback_handoff_after_persist_failure(
                        &session_id,
                        &to_session,
                        &handoff.prior,
                        &error,
                    );
                    self.command_audit_final(
                        super::command_audit::CommandAuditInput::mcp(
                            "control_lease_handoff",
                            "lease_handoff",
                            Some(session_id.clone()),
                            Some(to_session.clone()),
                            command_payload,
                            command_before,
                            json!({
                                "source_of_truth": "synapse_action::lease",
                                "from": lease_status_for_session(&session_id),
                                "to": lease_status_for_session(&to_session),
                            }),
                            "error",
                        )
                        .with_error(
                            super::command_audit::command_audit_error_from_error_data(&error),
                        ),
                    )?;
                    return Err(error);
                }
            };
        tracing::info!(
            code = "INPUT_LEASE_HANDOFF_COMMITTED",
            from_session_id = session_id,
            to_session_id = to_session,
            from_row_existed_before = persist_readback.from_row_existed_before,
            from_row_exists_after = persist_readback.from_row_exists_after,
            from_row_deleted = persist_readback.from_row_deleted,
            to_row_exists_after = persist_readback.to_row_exists_after,
            to_row_session_id = ?persist_readback.to_row_session_id,
            "readback=input_lease edge=handoff_committed"
        );
        // Handoff = one released + one acquired event, both tagged with the
        // handoff reason so the journal reconstructs the transfer (#897).
        // The handoff is already committed; journal failure surfaces as such.
        if let Err(error) =
            self.journal_lease_handoff_events(&session_id, &to_session, handoff.current.ttl_ms)
        {
            let tool_error =
                super::agent_events::agent_event_tool_error("control_lease_handoff", &error, true);
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "control_lease_handoff",
                    "lease_handoff",
                    Some(session_id.clone()),
                    Some(to_session.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "synapse_action::lease",
                        "response": &response,
                        "from": lease_status_for_session(&session_id),
                        "to": lease_status_for_session(&to_session),
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&tool_error),
                ),
            )?;
            return Err(tool_error);
        }
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "control_lease_handoff",
            "lease_handoff",
            Some(session_id.clone()),
            Some(to_session.clone()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": "synapse_action::lease",
                "response": &response,
                "from": lease_status_for_session(&session_id),
                "to": lease_status_for_session(&to_session),
            }),
            "ok",
        ))?;
        Ok(Json(response))
    }

    #[tool(
        description = "Read the current input lease state (holder, age, TTL, expiry). Never blocks; safe to poll. Reports whether the calling session is the holder.",
        input_schema = empty_input_schema()
    )]
    pub async fn control_lease_status(
        &self,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ControlLeaseResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "control_lease_status",
            "tool.invocation kind=control_lease_status"
        );
        let session_id = require_lease_session_id(&request_context)?;
        self.restore_session_lease_if_needed(&session_id)?;
        Ok(Json(lease_status_for_session(&session_id)))
    }
}

pub(super) fn validate_lease_ttl_ms(tool: &'static str, ttl_ms: u64) -> Result<(), ErrorData> {
    if (synapse_action::MIN_LEASE_TTL_MS..=synapse_action::MAX_LEASE_TTL_MS).contains(&ttl_ms) {
        return Ok(());
    }
    tracing::warn!(
        code = error_codes::TOOL_PARAMS_INVALID,
        detail_code = "LEASE_TTL_OUT_OF_RANGE",
        tool,
        ttl_ms,
        min_ttl_ms = synapse_action::MIN_LEASE_TTL_MS,
        max_ttl_ms = synapse_action::MAX_LEASE_TTL_MS,
        source_of_truth = LEASE_TTL_SOURCE_OF_TRUTH,
        "lease ttl_ms rejected before lease mutation"
    );
    Err(ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} ttl_ms must be between {} and {}; got {ttl_ms}",
            synapse_action::MIN_LEASE_TTL_MS,
            synapse_action::MAX_LEASE_TTL_MS
        ),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "detail_code": "LEASE_TTL_OUT_OF_RANGE",
            "tool": tool,
            "source_id": "ttl_ms",
            "source_of_truth": LEASE_TTL_SOURCE_OF_TRUTH,
            "min_ttl_ms": synapse_action::MIN_LEASE_TTL_MS,
            "max_ttl_ms": synapse_action::MAX_LEASE_TTL_MS,
            "ttl_ms": ttl_ms,
            "remediation": "pass ttl_ms in the advertised lease range or omit it for the default",
        })),
    ))
}

/// Acquire/renew the lease for `session_id`. Contended → `ACTION_FOREGROUND_LEASE_BUSY`.
/// Split out from the `#[tool]` method so the full outcome logic is unit-testable
/// without constructing an MCP `RequestContext`.
fn acquire_lease_for_session(
    session_id: &str,
    ttl_ms: u64,
) -> Result<ControlLeaseResponse, ErrorData> {
    let ttl = lease::ttl_from_ms(ttl_ms);
    match lease::try_acquire(session_id, ttl) {
        LeaseOutcome::Acquired(status) => {
            tracing::info!(
                code = "INPUT_LEASE_ACQUIRED",
                session_id = %session_id,
                ttl_ms = status.ttl_ms,
                "readback=input_lease outcome=acquired"
            );
            Ok(ControlLeaseResponse::from_status(
                "acquired",
                session_id.to_owned(),
                &status,
            ))
        }
        LeaseOutcome::Renewed(status) => {
            tracing::info!(
                code = "INPUT_LEASE_RENEWED",
                session_id = %session_id,
                ttl_ms = status.ttl_ms,
                "readback=input_lease outcome=renewed"
            );
            Ok(ControlLeaseResponse::from_status(
                "renewed",
                session_id.to_owned(),
                &status,
            ))
        }
        LeaseOutcome::Busy {
            holder,
            retry_after_ms,
        } => {
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_LEASE_BUSY,
                session_id = %session_id,
                holder = ?holder.owner_session_id,
                retry_after_ms,
                "readback=input_lease outcome=busy"
            );
            Err(lease_busy_error(session_id, &holder, retry_after_ms))
        }
        LeaseOutcome::CleanupPending {
            expired,
            retry_after_ms,
        } => {
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_LEASE_BUSY,
                session_id = %session_id,
                expired_owner = ?expired.owner_session_id,
                retry_after_ms,
                "readback=input_lease outcome=cleanup_pending"
            );
            Err(lease_cleanup_pending_error(
                session_id,
                &expired,
                retry_after_ms,
            ))
        }
    }
}

fn release_lease_for_session(session_id: &str) -> Result<ControlLeaseResponse, ErrorData> {
    match lease::release(session_id) {
        Ok(status) => {
            tracing::info!(
                code = "INPUT_LEASE_RELEASED",
                session_id = %session_id,
                "readback=input_lease outcome=released"
            );
            Ok(ControlLeaseResponse::released(session_id.to_owned(), &status))
        }
        Err(error) => match &error {
            // #1556: releasing an unheld/expired lease is the *expected* end
            // state of a correct short-TTL client whose happy path outlived the
            // lease (TTL max 30s). Make it idempotent: success with a physically
            // honest readback (released=false, was_held=false, holder=null). The
            // caller still audits it. Erroring here trained callers to swallow
            // release errors, which masked the one case that matters below.
            LeaseError::NotHeld { holder: None, .. } => {
                tracing::info!(
                    code = "INPUT_LEASE_RELEASE_NOOP",
                    session_id = %session_id,
                    "readback=input_lease outcome=not_held released=false was_held=false"
                );
                Ok(ControlLeaseResponse::release_noop(session_id.to_owned()))
            }
            // Held by a DIFFERENT live session — a real caller bug (releasing
            // someone else's lease). Keep the hard fail-closed error.
            LeaseError::NotHeld { holder: Some(_), .. } => {
                Err(lease_not_held_error(session_id, &error))
            }
        },
    }
}

fn handoff_lease_for_session(
    from_session_id: &str,
    to_session_id: &str,
    ttl_ms: u64,
) -> Result<synapse_action::LeaseHandoff, ErrorData> {
    match lease::handoff(from_session_id, to_session_id, lease::ttl_from_ms(ttl_ms)) {
        Ok(handoff) => {
            tracing::info!(
                code = "INPUT_LEASE_HANDED_OFF",
                from_session_id,
                to_session_id,
                prior = ?handoff.prior,
                current = ?handoff.current,
                "readback=input_lease outcome=handed_off"
            );
            Ok(handoff)
        }
        Err(error) => Err(lease_not_held_error(from_session_id, &error)),
    }
}

fn lease_status_for_session(session_id: &str) -> ControlLeaseResponse {
    let status = lease::status();
    ControlLeaseResponse::from_status("status", session_id.to_owned(), &status)
}

impl SynapseService {
    /// Journals one input-lease lifecycle event (#897).
    fn journal_lease_event(
        &self,
        kind: synapse_core::AgentEventKind,
        session_id: &str,
        reason_code: Option<&str>,
        ttl_ms: Option<u64>,
    ) -> Result<(), synapse_storage::StorageError> {
        let db = self
            .m3_storage()
            .map_err(|error| synapse_storage::StorageError::WriteFailed {
                cf_name: synapse_storage::cf::CF_AGENT_EVENTS.to_owned(),
                detail: format!("open storage for lease agent event: {}", error.message),
            })?;
        let mut record =
            synapse_core::AgentEventRecord::new(super::agent_events::unix_time_ns_now(), kind);
        record.session_id = Some(session_id.to_owned());
        record.reason_code = reason_code.map(ToOwned::to_owned);
        record.attributes.conversation_id = Some(session_id.to_owned());
        record.payload = json!({ "ttl_ms": ttl_ms });
        super::agent_events::record_agent_event(&db, &record).map(|_readback| ())
    }

    /// Journals the released/acquired event pair for a committed handoff in
    /// one storage batch.
    fn journal_lease_handoff_events(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), synapse_storage::StorageError> {
        let db = self
            .m3_storage()
            .map_err(|error| synapse_storage::StorageError::WriteFailed {
                cf_name: synapse_storage::cf::CF_AGENT_EVENTS.to_owned(),
                detail: format!(
                    "open storage for lease handoff agent events: {}",
                    error.message
                ),
            })?;
        let ts_ns = super::agent_events::unix_time_ns_now();
        let mut released =
            synapse_core::AgentEventRecord::new(ts_ns, synapse_core::AgentEventKind::LeaseReleased);
        released.session_id = Some(from_session_id.to_owned());
        released.reason_code = Some("handoff".to_owned());
        released.attributes.conversation_id = Some(from_session_id.to_owned());
        released.payload = json!({ "to_session": to_session_id });
        let mut acquired =
            synapse_core::AgentEventRecord::new(ts_ns, synapse_core::AgentEventKind::LeaseAcquired);
        acquired.session_id = Some(to_session_id.to_owned());
        acquired.reason_code = Some("handoff".to_owned());
        acquired.attributes.conversation_id = Some(to_session_id.to_owned());
        acquired.payload = json!({ "from_session": from_session_id, "ttl_ms": ttl_ms });
        super::agent_events::record_agent_events(&db, &[released, acquired]).map(|_readbacks| ())
    }

    fn ensure_handoff_recipient_live(
        &self,
        from_session_id: &str,
        to_session_id: &str,
    ) -> Result<(), ErrorData> {
        validate_session_id(to_session_id)?;
        if from_session_id == to_session_id {
            return Err(ErrorData::new(
                ErrorCode(-32099),
                "control_lease_handoff requires a different recipient session",
                Some(json!({
                    "code": error_codes::TOOL_PARAMS_INVALID,
                    "from_session_id": from_session_id,
                    "to_session_id": to_session_id,
                })),
            ));
        }
        let now_unix_ms = unix_time_ms_now();
        let recipient = {
            let guard = self.session_registry_ref().lock().map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "session registry lock poisoned while validating lease handoff recipient",
                )
            })?;
            guard
                .reads(now_unix_ms)
                .into_iter()
                .find(|entry| entry.session_id == to_session_id)
        };
        match recipient.as_ref() {
            Some(read) if read.lifecycle == "live" => Ok(()),
            _ => Err(recipient_unknown_error(
                from_session_id,
                to_session_id,
                recipient.as_ref(),
            )),
        }
    }

    fn rollback_handoff_after_persist_failure(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        prior: &LeaseStatus,
        error: &ErrorData,
    ) {
        let rollback_ttl_ms = prior
            .expires_in_ms
            .or(prior.ttl_ms)
            .unwrap_or(synapse_action::DEFAULT_LEASE_TTL_MS);
        match lease::handoff(
            to_session_id,
            from_session_id,
            lease::ttl_from_ms(rollback_ttl_ms),
        ) {
            Ok(rollback) => {
                let rollback_persist = self.persist_session_lease_handoff(
                    to_session_id,
                    from_session_id,
                    &rollback.current,
                );
                tracing::error!(
                    code = error_codes::TOOL_INTERNAL_ERROR,
                    from_session_id,
                    to_session_id,
                    rollback_persisted = rollback_persist.is_ok(),
                    rollback_persist_error = ?rollback_persist.as_ref().err(),
                    original_error = ?error,
                    rollback = ?rollback,
                    "input lease handoff failed durability write; rolled memory back to prior holder"
                );
            }
            Err(rollback_error) => {
                let recipient_released = lease::release_if_owner(to_session_id);
                tracing::error!(
                    code = error_codes::TOOL_INTERNAL_ERROR,
                    from_session_id,
                    to_session_id,
                    recipient_released,
                    rollback_error = ?rollback_error,
                    original_error = ?error,
                    "input lease handoff failed durability write and memory rollback could not restore prior holder"
                );
            }
        }
    }

    pub(crate) fn dashboard_control_lease_force_release(
        &self,
        owner_session_id: String,
        confirmed: bool,
    ) -> Result<DashboardControlLeaseForceReleaseResponse, ErrorData> {
        validate_session_id(&owner_session_id)?;
        let before = lease_status_for_session(&owner_session_id);
        let command_payload = json!({
            "owner_session_id": &owner_session_id,
            "confirmed": confirmed,
        });
        let command_before = json!({
            "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
            "owner": &before,
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "control_lease_force_release",
                "lease_force_release",
                None,
                Some(owner_session_id.clone()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_channel("dashboard"),
        )?;
        if !confirmed {
            let error = mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "dashboard lease force-release requires confirmation",
            );
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "control_lease_force_release",
                    "lease_force_release",
                    None,
                    Some(owner_session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
                        "owner": lease_status_for_session(&owner_session_id),
                    }),
                    "error",
                )
                .with_channel("dashboard")
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&error),
                ),
            )?;
            return Err(error);
        }
        let released_prior =
            lease::force_clear_if_owner(&owner_session_id, "dashboard_force_release");
        let released = released_prior.is_some();
        let mut persisted_row_deleted = false;
        if released {
            if let Err(error) = self.delete_persisted_session_lease(&owner_session_id) {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "control_lease_force_release",
                        "lease_force_release",
                        None,
                        Some(owner_session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
                            "released": true,
                            "owner": lease_status_for_session(&owner_session_id),
                        }),
                        "error",
                    )
                    .with_channel("dashboard")
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
            persisted_row_deleted = true;
            if let Err(error) = self.journal_lease_event(
                synapse_core::AgentEventKind::LeaseReleased,
                &owner_session_id,
                Some("dashboard_force_release"),
                None,
            ) {
                let tool_error = super::agent_events::agent_event_tool_error(
                    "control_lease_force_release",
                    &error,
                    true,
                );
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "control_lease_force_release",
                        "lease_force_release",
                        None,
                        Some(owner_session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
                            "released": true,
                            "owner": lease_status_for_session(&owner_session_id),
                        }),
                        "error",
                    )
                    .with_channel("dashboard")
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&tool_error),
                    ),
                )?;
                return Err(tool_error);
            }
        }
        let response = DashboardControlLeaseForceReleaseResponse {
            requested_owner_session_id: owner_session_id.clone(),
            confirmed,
            released,
            before,
            after: lease_status_for_session(&owner_session_id),
            persisted_row_deleted,
            source_of_truth: DASHBOARD_LEASE_SOURCE_OF_TRUTH,
        };
        self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "control_lease_force_release",
                "lease_force_release",
                None,
                Some(owner_session_id),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
                    "response": &response,
                }),
                "ok",
            )
            .with_channel("dashboard"),
        )?;
        Ok(response)
    }

    pub(crate) fn dashboard_control_lease_handoff(
        &self,
        from_session_id: String,
        to_session_id: String,
        ttl_ms: u64,
    ) -> Result<DashboardControlLeaseHandoffResponse, ErrorData> {
        validate_session_id(&from_session_id)?;
        self.ensure_handoff_recipient_live(&from_session_id, &to_session_id)?;
        self.restore_session_lease_if_needed(&from_session_id)?;
        let before_from = lease_status_for_session(&from_session_id);
        let before_to = lease_status_for_session(&to_session_id);
        let command_payload = json!({
            "from_session_id": &from_session_id,
            "to_session_id": &to_session_id,
            "ttl_ms": ttl_ms,
        });
        let command_before = json!({
            "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
            "from": &before_from,
            "to": &before_to,
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "control_lease_handoff",
                "lease_handoff",
                Some(from_session_id.clone()),
                Some(to_session_id.clone()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_channel("dashboard"),
        )?;
        let handoff = match handoff_lease_for_session(&from_session_id, &to_session_id, ttl_ms) {
            Ok(handoff) => handoff,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "control_lease_handoff",
                        "lease_handoff",
                        Some(from_session_id.clone()),
                        Some(to_session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
                            "from": lease_status_for_session(&from_session_id),
                            "to": lease_status_for_session(&to_session_id),
                        }),
                        "error",
                    )
                    .with_channel("dashboard")
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };
        let response = ControlLeaseResponse::from_status(
            "handed_off",
            from_session_id.clone(),
            &handoff.current,
        );
        let persist_readback = match self.persist_session_lease_handoff(
            &from_session_id,
            &to_session_id,
            &handoff.current,
        ) {
            Ok(readback) => readback,
            Err(error) => {
                self.rollback_handoff_after_persist_failure(
                    &from_session_id,
                    &to_session_id,
                    &handoff.prior,
                    &error,
                );
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "control_lease_handoff",
                        "lease_handoff",
                        Some(from_session_id.clone()),
                        Some(to_session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
                            "from": lease_status_for_session(&from_session_id),
                            "to": lease_status_for_session(&to_session_id),
                        }),
                        "error",
                    )
                    .with_channel("dashboard")
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };
        tracing::info!(
            code = "DASHBOARD_INPUT_LEASE_HANDOFF_COMMITTED",
            from_session_id,
            to_session_id,
            from_row_existed_before = persist_readback.from_row_existed_before,
            from_row_exists_after = persist_readback.from_row_exists_after,
            to_row_exists_after = persist_readback.to_row_exists_after,
            "readback=input_lease edge=dashboard_handoff_committed"
        );
        if let Err(error) = self.journal_lease_handoff_events(
            &from_session_id,
            &to_session_id,
            handoff.current.ttl_ms,
        ) {
            let tool_error =
                super::agent_events::agent_event_tool_error("control_lease_handoff", &error, true);
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "control_lease_handoff",
                    "lease_handoff",
                    Some(from_session_id.clone()),
                    Some(to_session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
                        "from": lease_status_for_session(&from_session_id),
                        "to": lease_status_for_session(&to_session_id),
                    }),
                    "error",
                )
                .with_channel("dashboard")
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&tool_error),
                ),
            )?;
            return Err(tool_error);
        }
        let dashboard_response = DashboardControlLeaseHandoffResponse {
            from_session_id: from_session_id.clone(),
            to_session_id: to_session_id.clone(),
            ttl_ms,
            before_from,
            before_to,
            response,
            after_from: lease_status_for_session(&from_session_id),
            after_to: lease_status_for_session(&to_session_id),
            source_of_truth: DASHBOARD_LEASE_SOURCE_OF_TRUTH,
        };
        self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "control_lease_handoff",
                "lease_handoff",
                Some(from_session_id),
                Some(to_session_id),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": DASHBOARD_LEASE_SOURCE_OF_TRUTH,
                    "response": &dashboard_response,
                }),
                "ok",
            )
            .with_channel("dashboard"),
        )?;
        Ok(dashboard_response)
    }
}

/// Resolves the calling session id, failing loud when absent. The lease is
/// per-session, so an unidentified caller cannot meaningfully own it.
fn require_lease_session_id(
    request_context: &RequestContext<RoleServer>,
) -> Result<String, ErrorData> {
    super::context::mcp_session_id_from_request_context(request_context)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "input lease tools require an MCP session id (run the daemon in HTTP mode so each agent has its own Mcp-Session-Id)",
        )
    })
}

fn lease_busy_error(session_id: &str, holder: &LeaseStatus, retry_after_ms: u64) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "input lease is held by session {:?}; retry after {retry_after_ms}ms",
            holder.owner_session_id
        ),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_LEASE_BUSY,
            "requesting_session_id": session_id,
            "holder_session_id": holder.owner_session_id,
            "retry_after_ms": retry_after_ms,
            "holder": holder,
        })),
    )
}

fn lease_cleanup_pending_error(
    session_id: &str,
    expired: &LeaseStatus,
    retry_after_ms: u64,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "input lease expired for session {:?}; held-input cleanup is pending, retry after {retry_after_ms}ms",
            expired.owner_session_id
        ),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_LEASE_BUSY,
            "requesting_session_id": session_id,
            "holder_session_id": expired.owner_session_id,
            "retry_after_ms": retry_after_ms,
            "expired": expired,
            "cleanup_pending": true,
        })),
    )
}

fn recipient_unknown_error(
    from_session_id: &str,
    to_session_id: &str,
    recipient: Option<&SessionRegistryRead>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("lease handoff recipient session {to_session_id:?} is not live"),
        Some(json!({
            "code": error_codes::RECIPIENT_UNKNOWN,
            "from_session_id": from_session_id,
            "to_session_id": to_session_id,
            "recipient": recipient,
            "resolution": "start or reconnect the recipient agent so it registers a live MCP session, then retry handoff",
        })),
    )
}

fn lease_not_held_error(session_id: &str, error: &synapse_action::LeaseError) -> ErrorData {
    let holder = match error {
        synapse_action::LeaseError::NotHeld { holder, .. } => holder.clone(),
    };
    ErrorData::new(
        ErrorCode(-32099),
        error.to_string(),
        Some(json!({
            "code": error.code(),
            "requesting_session_id": session_id,
            "holder_session_id": holder,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_lease_for_session, handoff_lease_for_session, lease_status_for_session,
        release_lease_for_session, validate_lease_ttl_ms,
    };
    use crate::test_support;
    use synapse_core::error_codes;

    const TEST_RESET_REASON: &str = "lease_tools_test_reset";

    fn error_code(error: &rmcp::ErrorData) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    }

    fn error_u64(error: &rmcp::ErrorData, field: &str) -> Option<u64> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get(field))
            .and_then(serde_json::Value::as_u64)
    }

    #[test]
    fn lease_ttl_validator_rejects_out_of_range_with_structured_bounds() {
        let below = validate_lease_ttl_ms("control_lease_acquire", 99)
            .expect_err("below-min ttl must fail closed");
        assert_eq!(
            error_code(&below).as_deref(),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert_eq!(
            error_u64(&below, "min_ttl_ms"),
            Some(synapse_action::MIN_LEASE_TTL_MS)
        );
        assert_eq!(
            error_u64(&below, "max_ttl_ms"),
            Some(synapse_action::MAX_LEASE_TTL_MS)
        );
        assert_eq!(error_u64(&below, "ttl_ms"), Some(99));

        let above = validate_lease_ttl_ms("control_lease_handoff", 30_001)
            .expect_err("above-max ttl must fail closed");
        assert_eq!(
            error_code(&above).as_deref(),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert_eq!(error_u64(&above, "ttl_ms"), Some(30_001));

        validate_lease_ttl_ms("control_lease_acquire", synapse_action::MIN_LEASE_TTL_MS)
            .expect("minimum boundary ttl should be accepted");
        validate_lease_ttl_ms("control_lease_acquire", synapse_action::MAX_LEASE_TTL_MS)
            .expect("maximum boundary ttl should be accepted");
    }

    #[test]
    fn acquire_then_status_then_release_round_trip() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let session = "fsv-tool-acquire";
        let acquired = acquire_lease_for_session(session, 5_000)
            .map_err(|error| anyhow::anyhow!("acquire failed: {error:?}"))?;
        assert_eq!(acquired.outcome, "acquired");
        assert!(acquired.held);
        assert!(acquired.is_owner);
        assert_eq!(acquired.owner_session_id.as_deref(), Some(session));

        // Source of truth: a separate status read reflects the holder.
        let status = lease_status_for_session(session);
        assert!(status.held);
        assert!(status.is_owner);
        assert_eq!(status.owner_session_id.as_deref(), Some(session));
        println!(
            "readback=input_lease step=after_acquire held={} owner={:?} expires_in_ms={:?}",
            status.held, status.owner_session_id, status.expires_in_ms
        );

        let released = release_lease_for_session(session)
            .map_err(|error| anyhow::anyhow!("release failed: {error:?}"))?;
        assert_eq!(released.outcome, "released");
        assert!(!released.held);

        let after = lease_status_for_session(session);
        assert!(!after.held);
        assert_eq!(after.owner_session_id, None);
        println!(
            "readback=input_lease step=after_release held={} owner={:?}",
            after.held, after.owner_session_id
        );
        test_support::reset_lease(TEST_RESET_REASON);
        Ok(())
    }

    #[test]
    fn second_session_is_refused_busy_with_holder() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let owner = "fsv-tool-busy-owner";
        let contender = "fsv-tool-busy-contender";
        let _held = acquire_lease_for_session(owner, 5_000)
            .map_err(|error| anyhow::anyhow!("owner acquire failed: {error:?}"))?;

        let error = match acquire_lease_for_session(contender, 5_000) {
            Ok(response) => anyhow::bail!("contender unexpectedly acquired: {response:?}"),
            Err(error) => error,
        };
        assert_eq!(
            error_code(&error).as_deref(),
            Some(error_codes::ACTION_FOREGROUND_LEASE_BUSY)
        );
        let holder = error
            .data
            .as_ref()
            .and_then(|data| data.get("holder_session_id"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(holder, Some(owner));

        // Source of truth: owner still holds; contender did not block.
        let status = lease_status_for_session(owner);
        assert_eq!(status.owner_session_id.as_deref(), Some(owner));
        println!(
            "readback=input_lease step=busy requesting={contender} holder={:?}",
            status.owner_session_id
        );
        test_support::reset_lease(TEST_RESET_REASON);
        Ok(())
    }

    #[test]
    fn owner_handoff_transfers_to_recipient_and_prior_owner_is_busy() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let owner = "fsv-tool-handoff-owner";
        let recipient = "fsv-tool-handoff-recipient";
        let _held = acquire_lease_for_session(owner, 5_000)
            .map_err(|error| anyhow::anyhow!("owner acquire failed: {error:?}"))?;

        let handoff = handoff_lease_for_session(owner, recipient, 6_000)
            .map_err(|error| anyhow::anyhow!("handoff failed: {error:?}"))?;
        assert_eq!(handoff.prior.owner_session_id.as_deref(), Some(owner));
        assert_eq!(handoff.current.owner_session_id.as_deref(), Some(recipient));

        let recipient_status = lease_status_for_session(recipient);
        assert!(recipient_status.is_owner);
        assert_eq!(
            recipient_status.owner_session_id.as_deref(),
            Some(recipient)
        );
        let owner_error = match acquire_lease_for_session(owner, 5_000) {
            Ok(response) => anyhow::bail!("prior owner unexpectedly reacquired: {response:?}"),
            Err(error) => error,
        };
        assert_eq!(
            error_code(&owner_error).as_deref(),
            Some(error_codes::ACTION_FOREGROUND_LEASE_BUSY)
        );
        println!(
            "readback=input_lease step=handoff owner_before={:?} owner_after={:?}",
            handoff.prior.owner_session_id, recipient_status.owner_session_id
        );
        test_support::reset_lease(TEST_RESET_REASON);
        Ok(())
    }

    #[test]
    fn non_owner_release_errors_not_held() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let owner = "fsv-tool-nonowner-owner";
        let intruder = "fsv-tool-nonowner-intruder";
        let _held = acquire_lease_for_session(owner, 5_000)
            .map_err(|error| anyhow::anyhow!("owner acquire failed: {error:?}"))?;

        let error = match release_lease_for_session(intruder) {
            Ok(response) => anyhow::bail!("intruder unexpectedly released: {response:?}"),
            Err(error) => error,
        };
        assert_eq!(
            error_code(&error).as_deref(),
            Some(error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD)
        );
        // Owner's lease survives the intruder's failed release.
        assert!(lease_status_for_session(owner).is_owner);
        test_support::reset_lease(TEST_RESET_REASON);
        Ok(())
    }

    #[test]
    fn release_when_unheld_is_idempotent_success() -> anyhow::Result<()> {
        // #1556: no lease is held; releasing must succeed as a no-op with a
        // physically honest readback, not ACTION_FOREGROUND_LEASE_NOT_HELD.
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        test_support::reset_lease(TEST_RESET_REASON);
        let session = "fsv-tool-idempotent-release";

        let response = release_lease_for_session(session)
            .map_err(|error| anyhow::anyhow!("unheld release must succeed: {error:?}"))?;

        assert_eq!(response.outcome, "not_held");
        assert_eq!(response.released, Some(false));
        assert_eq!(response.was_held, Some(false));
        assert!(!response.held);
        assert_eq!(response.owner_session_id, None);
        println!(
            "readback=input_lease step=idempotent_release outcome={} released={:?} was_held={:?} held={}",
            response.outcome, response.released, response.was_held, response.held
        );
        test_support::reset_lease(TEST_RESET_REASON);
        Ok(())
    }

    #[test]
    fn release_after_own_lease_expired_is_idempotent_success() -> anyhow::Result<()> {
        // #1556: the common real case — a short-TTL client whose happy path
        // outlived its lease. `ttl_from_ms` clamps the requested TTL up to
        // MIN_LEASE_TTL_MS (100ms), so the sleep must exceed that floor for the
        // lease to *genuinely* lapse; a shorter sleep leaves it live and would
        // silently test the wrong path (release of a still-owned lease).
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        test_support::reset_lease(TEST_RESET_REASON);
        let session = "fsv-tool-expired-release";
        let _held = acquire_lease_for_session(session, 1)
            .map_err(|error| anyhow::anyhow!("acquire failed: {error:?}"))?;
        std::thread::sleep(std::time::Duration::from_millis(
            synapse_action::MIN_LEASE_TTL_MS + 50,
        ));

        let response = release_lease_for_session(session)
            .map_err(|error| anyhow::anyhow!("expired-lease release must succeed: {error:?}"))?;

        // release() reaps the lapsed grant (expire_if_lapsed) *before* the owner
        // check, so nothing remains to free: the honest readback is a no-op
        // (not_held / released=false / was_held=false / no owner) — and crucially
        // still a SUCCESS, never the ACTION_FOREGROUND_LEASE_NOT_HELD error that
        // #1556 measured on ~45% of real release calls.
        assert_eq!(response.outcome, "not_held");
        assert_eq!(response.released, Some(false));
        assert_eq!(response.was_held, Some(false));
        assert!(!response.held);
        assert_eq!(response.owner_session_id, None);
        println!(
            "readback=input_lease step=expired_owner_release outcome={} released={:?} was_held={:?} held={}",
            response.outcome, response.released, response.was_held, response.held
        );
        test_support::reset_lease(TEST_RESET_REASON);
        Ok(())
    }

    #[test]
    fn repeat_acquire_by_owner_renews() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let session = "fsv-tool-renew";
        let _first = acquire_lease_for_session(session, 5_000)
            .map_err(|error| anyhow::anyhow!("first acquire failed: {error:?}"))?;
        let second = acquire_lease_for_session(session, 5_000)
            .map_err(|error| anyhow::anyhow!("renew failed: {error:?}"))?;
        assert_eq!(second.outcome, "renewed");
        assert!(second.is_owner);
        test_support::reset_lease(TEST_RESET_REASON);
        Ok(())
    }
}

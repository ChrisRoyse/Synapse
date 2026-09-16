//! Advisory target ownership claims for multi-agent coordination (#797).
//!
//! Claims are daemon-local read-model state keyed by target identity. They do
//! not replace per-tool validation or capability checks; they make same-target
//! mutation conflicts explicit before another session silently clobbers a
//! claimed window/tab.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;

use super::{
    ErrorData, Json, Parameters, SessionTarget, SynapseService, TargetWire, mcp_error,
    session_registry::unix_time_ms_now, tool, tool_router,
};

pub(crate) const DEFAULT_TARGET_CLAIM_TTL_MS: u64 = 120_000;
const MIN_TARGET_CLAIM_TTL_MS: u64 = 1_000;
const MAX_TARGET_CLAIM_TTL_MS: u64 = 600_000;

pub(crate) type SharedTargetClaims = Arc<Mutex<TargetClaimRegistry>>;

#[derive(Clone, Debug, Default)]
pub(crate) struct TargetClaimRegistry {
    claims: BTreeMap<String, TargetClaimEntry>,
    next_generation: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TargetClaimEntry {
    target_key: String,
    target: SessionTarget,
    owner_session_id: String,
    claimed_at_unix_ms: u64,
    renewed_at_unix_ms: u64,
    ttl_ms: u64,
    expires_at_unix_ms: u64,
    generation: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetClaimTargetParam {
    Window {
        #[schemars(range(min = 1, max = 4_294_967_295_u64))]
        window_hwnd: i64,
    },
    Cdp {
        #[schemars(range(min = 1, max = 4_294_967_295_u64))]
        window_hwnd: i64,
        cdp_target_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetClaimParams {
    #[serde(default)]
    #[schemars(default)]
    pub target: Option<TargetClaimTargetParam>,
    #[serde(default = "default_target_claim_ttl_ms")]
    #[schemars(
        default = "default_target_claim_ttl_ms",
        range(min = 1000, max = 600000)
    )]
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetClaimAdoptParams {
    /// Existing owner session id read from target_claim_status/session_list.
    pub owner_session_id: String,
    #[serde(default)]
    #[schemars(default)]
    pub target: Option<TargetClaimTargetParam>,
    #[serde(default = "default_target_claim_ttl_ms")]
    #[schemars(
        default = "default_target_claim_ttl_ms",
        range(min = 1000, max = 600000)
    )]
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetReleaseParams {
    #[serde(default)]
    #[schemars(default)]
    pub target: Option<TargetClaimTargetParam>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetClaimStatusParams {
    #[serde(default)]
    #[schemars(default)]
    pub target: Option<TargetClaimTargetParam>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetClaimRead {
    pub target_key: String,
    pub target: TargetWire,
    pub owner_session_id: String,
    pub claimed_at_unix_ms: u64,
    pub renewed_at_unix_ms: u64,
    pub ttl_ms: u64,
    pub expires_at_unix_ms: u64,
    pub expires_in_ms: u64,
    pub generation: u64,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetClaimResponse {
    pub session_id: String,
    pub outcome: String,
    pub claim: TargetClaimRead,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetClaimAdoptResponse {
    pub session_id: String,
    pub adopted_from_session_id: String,
    pub outcome: String,
    pub prior_claim: TargetClaimRead,
    pub claim: TargetClaimRead,
    pub owner_in_flight_before: Vec<crate::daemon_lifecycle::InFlightToolCallRead>,
    pub owner_teardown_report: crate::server::session_lifecycle::SessionTeardownReport,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetReleaseResponse {
    pub session_id: String,
    pub target_key: String,
    pub target: TargetWire,
    pub released: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_claim: Option<TargetClaimRead>,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetClaimStatusResponse {
    pub session_id: String,
    pub now_unix_ms: u64,
    pub claim_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_claim: Option<TargetClaimRead>,
    pub claims: Vec<TargetClaimRead>,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DashboardTargetClaimPruneResponse {
    pub before_count: usize,
    pub after_count: usize,
    pub pruned_count: usize,
    pub before_claims: Vec<TargetClaimRead>,
    pub after: TargetClaimStatusResponse,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetClaimCleanupReport {
    pub owned_before: usize,
    pub released: usize,
    pub target_keys: Vec<String>,
    pub failed: bool,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct TargetClaimConflict {
    entry: TargetClaimEntry,
    requester_session_id: String,
    tool: &'static str,
}

#[tool_router(router = target_claim_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Claim this MCP session's active target, or an explicit window/CDP target, as an advisory ownership lease. A live claim causes other sessions' mutating actions against the same target to fail closed with TARGET_CO_OWNED while read-only observe remains allowed."
    )]
    pub async fn target_claim(
        &self,
        params: Parameters<TargetClaimParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetClaimResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "target_claim",
            "tool.invocation kind=target_claim"
        );
        let session_id = require_claim_session_id(&request_context)?;
        let _authority_gate = self
            .lock_session_authority_for_tool("target_claim", &session_id)
            .await?;
        validate_target_claim_ttl(params.0.ttl_ms)?;
        let target = self.resolve_target_claim_target(&session_id, params.0.target)?;
        validate_claim_target(&target)?;
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, Some(&session_id))?;
        super::operator_panic_boundary::ensure_mcp_mutation(
            "target_claim_before_registry_mutation",
        )?;
        let mut guard = self.lock_target_claims()?;
        let prior_registry = guard.clone();
        guard.prune_inactive(now, &live_sessions);
        let target_key = target_key(&target);
        let target_wire = target_wire(&target);
        let command_payload = json!({
            "target_key": &target_key,
            "target": &target_wire,
            "ttl_ms": params.0.ttl_ms,
        });
        let command_before = json!({
            "source_of_truth": source_of_truth(),
            "claims": guard.reads(now),
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "target_claim",
                "target_claim",
                Some(session_id.clone()),
                Some(session_id.clone()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_target(json!({
                "target_key": &target_key,
                "target": &target_wire,
            })),
        )?;
        let claim_result = guard.claim(&session_id, target, params.0.ttl_ms, now, &live_sessions);
        if let Err(panic_error) = super::operator_panic_boundary::ensure_mcp_mutation(
            "target_claim_after_registry_mutation",
        ) {
            *guard = prior_registry;
            return Err(panic_error);
        }
        match claim_result {
            Ok((entry, outcome)) => {
                let response = TargetClaimResponse {
                    session_id: session_id.clone(),
                    outcome: outcome.to_owned(),
                    claim: entry.read(now),
                };
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "target_claim",
                        "target_claim",
                        Some(session_id.clone()),
                        Some(session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": source_of_truth(),
                            "response": &response,
                            "claims": guard.reads(now),
                        }),
                        "ok",
                    )
                    .with_target(json!({
                        "target_key": &target_key,
                        "target": &target_wire,
                    })),
                )?;
                tracing::info!(
                    code = "TARGET_CLAIM_SET",
                    session_id = %session_id,
                    target_key = %entry.target_key,
                    outcome,
                    expires_at_unix_ms = entry.expires_at_unix_ms,
                    "readback=target_claim outcome={outcome}"
                );
                Ok(Json(response))
            }
            Err(conflict) => {
                let error = conflict_error("target_claim", &session_id, &conflict, "target_claim");
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "target_claim",
                        "target_claim",
                        Some(session_id.clone()),
                        Some(session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": source_of_truth(),
                            "claims": guard.reads(now),
                        }),
                        "error",
                    )
                    .with_target(json!({
                        "target_key": &target_key,
                        "target": &target_wire,
                    }))
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                Err(error)
            }
        }
    }

    #[tool(
        description = "Explicitly recover a target claim from an older live same-agent MCP session after client churn/compaction. The caller must provide the current owner_session_id from target_claim_status/session_list; adoption fails closed unless both sessions have the same client identity, the caller is newer, and the old owner has no in-flight tool call. On success the old session is terminated through session lifecycle cleanup before the caller receives the target claim."
    )]
    pub async fn target_claim_adopt(
        &self,
        params: Parameters<TargetClaimAdoptParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetClaimAdoptResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "target_claim_adopt",
            "tool.invocation kind=target_claim_adopt"
        );
        let session_id = require_claim_session_id(&request_context)?;
        let TargetClaimAdoptParams {
            owner_session_id,
            target,
            ttl_ms,
        } = params.0;
        super::session_tools::validate_session_id(&owner_session_id)?;
        if owner_session_id == session_id {
            return Err(adopt_refused_error(
                &session_id,
                &owner_session_id,
                "target_claim_adopt requires a different owner_session_id",
                None,
                None,
            ));
        }
        // Adoption mutates both authority domains. Lexical multi-lock ordering
        // prevents reverse-adoption deadlocks and keeps the owner stable through
        // its lifecycle teardown and the requester's replacement claim.
        let _authority_gates = self
            .lock_session_authorities(&[session_id.as_str(), owner_session_id.as_str()])
            .await?;
        self.reject_terminated_session_tool_call("target_claim_adopt", &session_id)?;
        self.reject_terminated_session_tool_call("target_claim_adopt", &owner_session_id)?;
        validate_target_claim_ttl(ttl_ms)?;
        let target = self.resolve_target_claim_target(&session_id, target)?;
        validate_claim_target(&target)?;
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, Some(&session_id))?;
        super::operator_panic_boundary::ensure_mcp_mutation(
            "target_claim_adopt_before_initial_registry_prune",
        )?;
        let prior_entry = {
            let mut guard = self.lock_target_claims()?;
            let prior_registry = guard.clone();
            guard.prune_inactive(now, &live_sessions);
            let prior_entry = guard.get(&target_key(&target));
            if let Err(panic_error) = super::operator_panic_boundary::ensure_mcp_mutation(
                "target_claim_adopt_after_initial_registry_prune",
            ) {
                *guard = prior_registry;
                return Err(panic_error);
            }
            prior_entry
        }
        .ok_or_else(|| target_claim_not_found_error("target_claim_adopt", &session_id, &target))?;
        if prior_entry.owner_session_id != owner_session_id {
            return Err(adopt_refused_error(
                &session_id,
                &owner_session_id,
                "target_claim_adopt owner_session_id does not match the live target claim owner",
                None,
                None,
            ));
        }
        let prior_claim = prior_entry.read(now);
        let (_requester, _owner, owner_in_flight_before) =
            self.ensure_same_agent_adoption_allowed(&session_id, &owner_session_id, now)?;
        let command_payload = json!({
            "owner_session_id": &owner_session_id,
            "target_key": &prior_claim.target_key,
            "target": &prior_claim.target,
            "ttl_ms": ttl_ms,
        });
        let command_before = json!({
            "source_of_truth": source_of_truth(),
            "prior_claim": &prior_claim,
            "owner_in_flight_before": owner_in_flight_before,
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "target_claim_adopt",
                "target_claim_adopt",
                Some(session_id.clone()),
                Some(owner_session_id.clone()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_target(json!({
                "target_key": &prior_claim.target_key,
                "target": &prior_claim.target,
            })),
        )?;

        let lifecycle = self.session_lifecycle_state()?;
        super::operator_panic_boundary::ensure_mcp_mutation(
            "target_claim_adopt_before_owner_teardown",
        )?;
        let owner_teardown_report = match lifecycle
            .teardown_session_authority_locked(&owner_session_id, "target_claim_adopt")
            .await
        {
            Ok(report) => report,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "target_claim_adopt",
                        "target_claim_adopt",
                        Some(session_id.clone()),
                        Some(owner_session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": source_of_truth(),
                            "claims": self.lock_target_claims()?.reads(unix_time_ms_now()),
                        }),
                        "error",
                    )
                    .with_target(json!({
                        "target_key": &prior_claim.target_key,
                        "target": &prior_claim.target,
                    }))
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };
        if let Err(panic_error) = super::operator_panic_boundary::ensure_mcp_mutation(
            "target_claim_adopt_after_owner_teardown",
        ) {
            synapse_action::record_operator_panic_safety_incident();
            tracing::error!(
                code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                session_id = %session_id,
                owner_session_id = %owner_session_id,
                target_key = %prior_claim.target_key,
                "operator panic crossed target-claim adoption after owner teardown; old authority remains terminated and no replacement claim will be installed"
            );
            return Err(panic_error);
        }

        let now_after = unix_time_ms_now();
        let live_sessions_after = self.live_target_claim_sessions(now_after, Some(&session_id))?;
        super::operator_panic_boundary::ensure_mcp_mutation(
            "target_claim_adopt_before_replacement_claim",
        )?;
        let mut guard = self.lock_target_claims()?;
        let prior_registry = guard.clone();
        guard.prune_inactive(now_after, &live_sessions_after);
        let claim_result =
            guard.claim(&session_id, target, ttl_ms, now_after, &live_sessions_after);
        if let Err(panic_error) = super::operator_panic_boundary::ensure_mcp_mutation(
            "target_claim_adopt_after_replacement_claim",
        ) {
            *guard = prior_registry;
            return Err(panic_error);
        }
        let (entry, _claim_outcome) = match claim_result {
            Ok(result) => result,
            Err(conflict) => {
                let error = conflict_error(
                    "target_claim_adopt",
                    &session_id,
                    &conflict,
                    "adopt_after_owner_teardown",
                );
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "target_claim_adopt",
                        "target_claim_adopt",
                        Some(session_id.clone()),
                        Some(owner_session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": source_of_truth(),
                            "owner_teardown_report": &owner_teardown_report,
                            "claims": guard.reads(now_after),
                        }),
                        "error",
                    )
                    .with_target(json!({
                        "target_key": &prior_claim.target_key,
                        "target": &prior_claim.target,
                    }))
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };
        let response = TargetClaimAdoptResponse {
            session_id: session_id.clone(),
            adopted_from_session_id: owner_session_id.clone(),
            outcome: "adopted".to_owned(),
            prior_claim,
            claim: entry.read(now_after),
            owner_in_flight_before,
            owner_teardown_report,
            source_of_truth: source_of_truth(),
        };
        self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "target_claim_adopt",
                "target_claim_adopt",
                Some(session_id.clone()),
                Some(owner_session_id.clone()),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": source_of_truth(),
                    "response": &response,
                    "claims": guard.reads(now_after),
                }),
                "ok",
            )
            .with_target(json!({
                "target_key": &response.claim.target_key,
                "target": &response.claim.target,
            })),
        )?;
        tracing::info!(
            code = "TARGET_CLAIM_ADOPTED",
            session_id = %session_id,
            adopted_from_session_id = %owner_session_id,
            target_key = %entry.target_key,
            generation = entry.generation,
            prior_generation = prior_entry.generation,
            "readback=target_claim_adopt outcome=adopted"
        );
        Ok(Json(response))
    }

    #[tool(
        description = "Release this MCP session's advisory target ownership claim for the active target, or an explicit window/CDP target. A session cannot release another session's claim."
    )]
    pub async fn target_release(
        &self,
        params: Parameters<TargetReleaseParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetReleaseResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "target_release",
            "tool.invocation kind=target_release"
        );
        let session_id = require_claim_session_id(&request_context)?;
        let _authority_gate = self
            .lock_session_authority_for_tool("target_release", &session_id)
            .await?;
        let target = self.resolve_target_claim_target(&session_id, params.0.target)?;
        validate_claim_target_shape(&target)?;
        let target_key = target_key(&target);
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, Some(&session_id))?;
        super::operator_panic_boundary::ensure_mcp_mutation(
            "target_release_before_registry_mutation",
        )?;
        let mut guard = self.lock_target_claims()?;
        let prior_registry = guard.clone();
        guard.prune_inactive(now, &live_sessions);
        let target_wire = target_wire(&target);
        let command_payload = json!({
            "target_key": &target_key,
            "target": &target_wire,
        });
        let command_before = json!({
            "source_of_truth": source_of_truth(),
            "claims": guard.reads(now),
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "target_release",
                "target_release",
                Some(session_id.clone()),
                Some(session_id.clone()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_target(json!({
                "target_key": &target_key,
                "target": &target_wire,
            })),
        )?;
        let release_result = guard.release(&session_id, &target_key);
        if let Err(panic_error) = super::operator_panic_boundary::ensure_mcp_mutation(
            "target_release_after_registry_mutation",
        ) {
            *guard = prior_registry;
            return Err(panic_error);
        }
        match release_result {
            Ok(released) => {
                let released_claim = released.as_ref().map(|entry| entry.read(now));
                let response = TargetReleaseResponse {
                    session_id: session_id.clone(),
                    target_key: target_key.clone(),
                    target: target_wire.clone(),
                    released: released.is_some(),
                    released_claim,
                    source_of_truth: source_of_truth(),
                };
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "target_release",
                        "target_release",
                        Some(session_id.clone()),
                        Some(session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": source_of_truth(),
                            "response": &response,
                            "claims": guard.reads(now),
                        }),
                        "ok",
                    )
                    .with_target(json!({
                        "target_key": &target_key,
                        "target": &target_wire,
                    })),
                )?;
                tracing::info!(
                    code = "TARGET_CLAIM_RELEASED",
                    session_id = %session_id,
                    target_key = %target_key,
                    released = released.is_some(),
                    "readback=target_release"
                );
                Ok(Json(response))
            }
            Err(conflict) => {
                let error =
                    conflict_error("target_release", &session_id, &conflict, "target_release");
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "target_release",
                        "target_release",
                        Some(session_id.clone()),
                        Some(session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": source_of_truth(),
                            "claims": guard.reads(now),
                        }),
                        "error",
                    )
                    .with_target(json!({
                        "target_key": &target_key,
                        "target": &target_wire,
                    }))
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                Err(error)
            }
        }
    }

    #[tool(
        description = "Read the live advisory target ownership claims. With target omitted, returns all live claims; with target supplied or active, also returns the claim for that target."
    )]
    pub async fn target_claim_status(
        &self,
        params: Parameters<TargetClaimStatusParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetClaimStatusResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "target_claim_status",
            "tool.invocation kind=target_claim_status"
        );
        let session_id = require_claim_session_id(&request_context)?;
        let _authority_gate = self
            .lock_session_authority_for_tool("target_claim_status", &session_id)
            .await?;
        let target = match params.0.target {
            Some(target) => Some(target_param_to_session_target(target)?),
            None => self.session_target(Some(&session_id))?,
        };
        if let Some(target) = &target {
            validate_claim_target(target)?;
        }
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, Some(&session_id))?;
        let mut guard = self.lock_target_claims()?;
        guard.prune_inactive(now, &live_sessions);
        let target_key = target.as_ref().map(target_key);
        let claims = guard.reads(now);
        let target_claim = target_key
            .as_ref()
            .and_then(|key| claims.iter().find(|claim| &claim.target_key == key))
            .cloned();
        Ok(Json(TargetClaimStatusResponse {
            session_id,
            now_unix_ms: now,
            claim_count: claims.len(),
            target_key,
            target_claim,
            claims,
            source_of_truth: source_of_truth(),
        }))
    }
}

impl SynapseService {
    pub(crate) fn lock_target_claims(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, TargetClaimRegistry>, ErrorData> {
        self.target_claims.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "target claim registry lock poisoned",
            )
        })
    }

    pub(crate) fn target_claim_reads_by_owner(
        &self,
    ) -> Result<BTreeMap<String, Vec<TargetClaimRead>>, ErrorData> {
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, None)?;
        let mut guard = self.lock_target_claims()?;
        guard.prune_inactive(now, &live_sessions);
        let mut by_owner: BTreeMap<String, Vec<TargetClaimRead>> = BTreeMap::new();
        for claim in guard.reads(now) {
            by_owner
                .entry(claim.owner_session_id.clone())
                .or_default()
                .push(claim);
        }
        Ok(by_owner)
    }

    pub(crate) fn target_claim_status_snapshot(
        &self,
    ) -> Result<TargetClaimStatusResponse, ErrorData> {
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, None)?;
        let mut guard = self.lock_target_claims()?;
        guard.prune_inactive(now, &live_sessions);
        let claims = guard.reads(now);
        Ok(TargetClaimStatusResponse {
            session_id: "dashboard".to_owned(),
            now_unix_ms: now,
            claim_count: claims.len(),
            target_key: None,
            target_claim: None,
            claims,
            source_of_truth: source_of_truth(),
        })
    }

    pub(crate) fn dashboard_target_claim_prune(
        &self,
    ) -> Result<DashboardTargetClaimPruneResponse, ErrorData> {
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, None)?;
        let before_claims = {
            let guard = self.lock_target_claims()?;
            guard.reads(now)
        };
        let command_payload = json!({
            "live_session_count": live_sessions.len(),
        });
        let command_before = json!({
            "source_of_truth": source_of_truth(),
            "claim_count": before_claims.len(),
            "claims": &before_claims,
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "target_claim_status",
                "target_claim_prune",
                None,
                None,
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_channel("dashboard"),
        )?;
        let after_claims = {
            let mut guard = self.lock_target_claims()?;
            guard.prune_inactive(now, &live_sessions);
            guard.reads(now)
        };
        let after_keys = after_claims
            .iter()
            .map(|claim| claim.target_key.clone())
            .collect::<BTreeSet<_>>();
        let pruned_count = before_claims
            .iter()
            .filter(|claim| !after_keys.contains(&claim.target_key))
            .count();
        let after = TargetClaimStatusResponse {
            session_id: "dashboard".to_owned(),
            now_unix_ms: now,
            claim_count: after_claims.len(),
            target_key: None,
            target_claim: None,
            claims: after_claims,
            source_of_truth: source_of_truth(),
        };
        let response = DashboardTargetClaimPruneResponse {
            before_count: before_claims.len(),
            after_count: after.claim_count,
            pruned_count,
            before_claims,
            after,
            source_of_truth: source_of_truth(),
        };
        self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "target_claim_status",
                "target_claim_prune",
                None,
                None,
                command_payload,
                command_before,
                json!({
                    "source_of_truth": source_of_truth(),
                    "response": &response,
                }),
                "ok",
            )
            .with_channel("dashboard"),
        )?;
        Ok(response)
    }

    pub(crate) fn ensure_target_claim_allows_action(
        &self,
        tool: &'static str,
        explicit_target: Option<SessionTarget>,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let Some(session_id) =
            super::context::mcp_session_id_from_request_context(request_context)?
        else {
            return Ok(());
        };
        let target = match explicit_target {
            Some(target) => Some(target),
            None => self.session_target(Some(&session_id))?,
        };
        let target = match target {
            Some(target) => target,
            None => current_foreground_session_target()?,
        };
        self.ensure_target_claim_allows_session(tool, &session_id, &target)
    }

    pub(crate) fn ensure_target_claim_allows_session(
        &self,
        tool: &'static str,
        session_id: &str,
        target: &SessionTarget,
    ) -> Result<(), ErrorData> {
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, Some(session_id))?;
        let mut guard = self.lock_target_claims()?;
        guard.prune_inactive(now, &live_sessions);
        if let Some(entry) = guard.conflict(session_id, target) {
            return Err(conflict_error(
                tool,
                session_id,
                &TargetClaimConflict {
                    entry,
                    requester_session_id: session_id.to_owned(),
                    tool,
                },
                "mutating_action_claim_check",
            ));
        }
        Ok(())
    }

    pub(crate) fn target_claim_for_session(
        &self,
        session_id: &str,
        target: &SessionTarget,
    ) -> Result<Option<TargetClaimRead>, ErrorData> {
        let now = unix_time_ms_now();
        let live_sessions = self.live_target_claim_sessions(now, Some(session_id))?;
        let mut guard = self.lock_target_claims()?;
        guard.prune_inactive(now, &live_sessions);
        let claim = guard
            .get(&target_key(target))
            .filter(|entry| entry.owner_session_id == session_id)
            .map(|entry| entry.read(now));
        Ok(claim)
    }

    fn resolve_target_claim_target(
        &self,
        session_id: &str,
        target: Option<TargetClaimTargetParam>,
    ) -> Result<SessionTarget, ErrorData> {
        match target {
            Some(target) => target_param_to_session_target(target),
            None => self.session_target(Some(session_id))?.ok_or_else(|| {
                mcp_error(
                    error_codes::TARGET_NOT_SET,
                    "target_claim requires an explicit target or this session's active target",
                )
            }),
        }
    }

    fn live_target_claim_sessions(
        &self,
        now_unix_ms: u64,
        include_session_id: Option<&str>,
    ) -> Result<BTreeSet<String>, ErrorData> {
        let mut live = BTreeSet::new();
        if let Some(session_id) = include_session_id {
            live.insert(session_id.to_owned());
        }
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while reading target claim owner liveness",
            )
        })?;
        for read in guard.reads(now_unix_ms) {
            if read.lifecycle == "live" {
                live.insert(read.session_id);
            }
        }
        Ok(live)
    }

    pub(crate) fn ensure_same_agent_adoption_allowed(
        &self,
        requester_session_id: &str,
        owner_session_id: &str,
        now_unix_ms: u64,
    ) -> Result<
        (
            super::session_registry::SessionRegistryRead,
            super::session_registry::SessionRegistryRead,
            Vec<crate::daemon_lifecycle::InFlightToolCallRead>,
        ),
        ErrorData,
    > {
        let (requester, owner) = {
            let guard = self.session_registry_ref().lock().map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "session registry lock poisoned while validating target claim adoption",
                )
            })?;
            let reads = guard.reads(now_unix_ms);
            let requester = reads
                .iter()
                .find(|read| read.session_id == requester_session_id)
                .cloned()
                .ok_or_else(|| {
                    adopt_refused_error(
                        requester_session_id,
                        owner_session_id,
                        "requesting session is missing from session registry",
                        None,
                        None,
                    )
                })?;
            let owner = reads
                .iter()
                .find(|read| read.session_id == owner_session_id)
                .cloned()
                .ok_or_else(|| {
                    adopt_refused_error(
                        requester_session_id,
                        owner_session_id,
                        "owner session is missing from session registry",
                        Some(&requester),
                        None,
                    )
                })?;
            (requester, owner)
        };
        if requester.lifecycle != "live" || owner.lifecycle != "live" {
            return Err(adopt_refused_error(
                requester_session_id,
                owner_session_id,
                "target_claim_adopt requires both sessions to be live before explicit takeover",
                Some(&requester),
                Some(&owner),
            ));
        }
        if requester.agent_kind == "unknown" || requester.agent_kind != owner.agent_kind {
            return Err(adopt_refused_error(
                requester_session_id,
                owner_session_id,
                "target_claim_adopt requires matching known agent_kind",
                Some(&requester),
                Some(&owner),
            ));
        }
        if requester.client_name.is_none() || requester.client_name != owner.client_name {
            return Err(adopt_refused_error(
                requester_session_id,
                owner_session_id,
                "target_claim_adopt requires matching client_name",
                Some(&requester),
                Some(&owner),
            ));
        }
        if requester.started_at_unix_ms <= owner.started_at_unix_ms {
            return Err(adopt_refused_error(
                requester_session_id,
                owner_session_id,
                "target_claim_adopt requires the adopting session to be newer than the owner",
                Some(&requester),
                Some(&owner),
            ));
        }
        let owner_in_flight =
            match crate::daemon_lifecycle::in_flight_tool_calls_for_session(owner_session_id) {
                Ok(calls) => calls,
                #[cfg(test)]
                Err(error) if error.to_string().contains("ledger is not configured") => Vec::new(),
                Err(error) => {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("read daemon lifecycle in-flight tool calls: {error:#}"),
                    ));
                }
            };
        if !owner_in_flight.is_empty() {
            return Err(owner_active_error(
                requester_session_id,
                owner_session_id,
                owner_in_flight,
                "owner session has in-flight tool calls",
            ));
        }
        let lease_status = synapse_action::lease::status();
        if lease_status.owner_session_id.as_deref() == Some(owner_session_id) {
            return Err(owner_active_error(
                requester_session_id,
                owner_session_id,
                Vec::new(),
                "owner session holds the foreground input lease",
            ));
        }
        Ok((requester, owner, owner_in_flight))
    }
}

pub(crate) fn cleanup_claims_for_session(
    claims: &SharedTargetClaims,
    session_id: &str,
) -> TargetClaimCleanupReport {
    match claims.lock() {
        Ok(mut claims) => {
            let owned_before = claims
                .claims
                .values()
                .filter(|entry| entry.owner_session_id == session_id)
                .count();
            let released = claims.release_owner(session_id);
            TargetClaimCleanupReport {
                owned_before,
                released: released.len(),
                target_keys: released
                    .iter()
                    .map(|entry| entry.target_key.clone())
                    .collect(),
                failed: false,
                error_message: None,
            }
        }
        Err(_error) => TargetClaimCleanupReport {
            failed: true,
            error_message: Some("target claim registry lock poisoned".to_owned()),
            ..TargetClaimCleanupReport::default()
        },
    }
}

impl TargetClaimRegistry {
    pub(crate) fn claim(
        &mut self,
        session_id: &str,
        target: SessionTarget,
        ttl_ms: u64,
        now_unix_ms: u64,
        live_sessions: &BTreeSet<String>,
    ) -> Result<(TargetClaimEntry, &'static str), TargetClaimConflict> {
        let target_key = target_key(&target);
        if let Some(existing) = self.claims.get(&target_key)
            && existing.owner_session_id != session_id
            && live_sessions.contains(&existing.owner_session_id)
        {
            return Err(TargetClaimConflict {
                entry: existing.clone(),
                requester_session_id: session_id.to_owned(),
                tool: "target_claim",
            });
        }
        let existing = self.claims.get(&target_key).cloned();
        let same_owner = existing
            .as_ref()
            .is_some_and(|entry| entry.owner_session_id == session_id);
        let outcome = if same_owner { "renewed" } else { "claimed" };
        let generation = if same_owner {
            existing.as_ref().map_or(0, |entry| entry.generation)
        } else {
            self.allocate_generation()
        };
        let claimed_at_unix_ms = if same_owner {
            existing
                .as_ref()
                .map_or(now_unix_ms, |entry| entry.claimed_at_unix_ms)
        } else {
            now_unix_ms
        };
        let entry = TargetClaimEntry {
            target_key: target_key.clone(),
            target,
            owner_session_id: session_id.to_owned(),
            claimed_at_unix_ms,
            renewed_at_unix_ms: now_unix_ms,
            ttl_ms,
            expires_at_unix_ms: now_unix_ms.saturating_add(ttl_ms),
            generation,
        };
        self.claims.insert(target_key, entry.clone());
        Ok((entry, outcome))
    }

    pub(crate) fn release(
        &mut self,
        session_id: &str,
        target_key: &str,
    ) -> Result<Option<TargetClaimEntry>, TargetClaimConflict> {
        let Some(existing) = self.claims.get(target_key).cloned() else {
            return Ok(None);
        };
        if existing.owner_session_id != session_id {
            return Err(TargetClaimConflict {
                entry: existing,
                requester_session_id: session_id.to_owned(),
                tool: "target_release",
            });
        }
        Ok(self.claims.remove(target_key))
    }

    fn release_owner(&mut self, session_id: &str) -> Vec<TargetClaimEntry> {
        let target_keys = self
            .claims
            .iter()
            .filter_map(|(key, entry)| (entry.owner_session_id == session_id).then(|| key.clone()))
            .collect::<Vec<_>>();
        target_keys
            .into_iter()
            .filter_map(|key| self.claims.remove(&key))
            .collect()
    }

    fn conflict(&self, session_id: &str, target: &SessionTarget) -> Option<TargetClaimEntry> {
        self.claims
            .get(&target_key(target))
            .filter(|entry| entry.owner_session_id != session_id)
            .cloned()
    }

    fn get(&self, target_key: &str) -> Option<TargetClaimEntry> {
        self.claims.get(target_key).cloned()
    }

    pub(crate) fn reads(&self, now_unix_ms: u64) -> Vec<TargetClaimRead> {
        self.claims
            .values()
            .map(|entry| entry.read(now_unix_ms))
            .collect()
    }

    fn prune_inactive(&mut self, now_unix_ms: u64, live_sessions: &BTreeSet<String>) {
        self.claims.retain(|_key, entry| {
            entry.expires_at_unix_ms > now_unix_ms
                && live_sessions.contains(&entry.owner_session_id)
        });
    }

    fn allocate_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.saturating_add(1);
        self.next_generation
    }
}

impl TargetClaimEntry {
    fn read(&self, now_unix_ms: u64) -> TargetClaimRead {
        TargetClaimRead {
            target_key: self.target_key.clone(),
            target: target_wire(&self.target),
            owner_session_id: self.owner_session_id.clone(),
            claimed_at_unix_ms: self.claimed_at_unix_ms,
            renewed_at_unix_ms: self.renewed_at_unix_ms,
            ttl_ms: self.ttl_ms,
            expires_at_unix_ms: self.expires_at_unix_ms,
            expires_in_ms: self.expires_at_unix_ms.saturating_sub(now_unix_ms),
            generation: self.generation,
            source_of_truth: source_of_truth(),
        }
    }
}

pub(crate) fn target_key(target: &SessionTarget) -> String {
    match target {
        SessionTarget::Window { hwnd } => format!("window:0x{hwnd:x}"),
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => format!("cdp:0x{window_hwnd:x}:{cdp_target_id}"),
    }
}

pub(crate) fn target_wire(target: &SessionTarget) -> TargetWire {
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

pub(crate) fn window_session_target(hwnd: i64) -> SessionTarget {
    SessionTarget::Window { hwnd }
}

fn current_foreground_session_target() -> Result<SessionTarget, ErrorData> {
    let foreground = synapse_a11y::current_foreground_context().map_err(|error| {
        mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!("target claim foreground read failed before mutating action: {error}"),
        )
    })?;
    validate_claim_target(&SessionTarget::Window {
        hwnd: foreground.hwnd,
    })?;
    Ok(SessionTarget::Window {
        hwnd: foreground.hwnd,
    })
}

fn target_param_to_session_target(
    target: TargetClaimTargetParam,
) -> Result<SessionTarget, ErrorData> {
    match target {
        TargetClaimTargetParam::Window { window_hwnd } => {
            Ok(SessionTarget::Window { hwnd: window_hwnd })
        }
        TargetClaimTargetParam::Cdp {
            window_hwnd,
            cdp_target_id,
        } => {
            validate_cdp_target_id(&cdp_target_id)?;
            Ok(SessionTarget::Cdp {
                window_hwnd,
                cdp_target_id,
            })
        }
    }
}

fn validate_claim_target(target: &SessionTarget) -> Result<(), ErrorData> {
    validate_claim_target_shape(target)?;
    match target {
        SessionTarget::Window { hwnd }
        | SessionTarget::Cdp {
            window_hwnd: hwnd, ..
        } => {
            synapse_a11y::foreground_context(*hwnd).map_err(|error| {
                mcp_error(
                    error_codes::TARGET_WINDOW_NOT_FOUND,
                    format!("target claim window_hwnd 0x{hwnd:x} is not live: {error}"),
                )
            })?;
        }
    }
    Ok(())
}

fn validate_claim_target_shape(target: &SessionTarget) -> Result<(), ErrorData> {
    match target {
        SessionTarget::Window { hwnd } => {
            crate::m1::validate_window_hwnd_shape("target", *hwnd)?;
        }
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => {
            crate::m1::validate_window_hwnd_shape("target", *window_hwnd)?;
            validate_cdp_target_id(cdp_target_id)?;
        }
    }
    Ok(())
}

fn validate_cdp_target_id(cdp_target_id: &str) -> Result<(), ErrorData> {
    if cdp_target_id.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target claim cdp_target_id must not be empty",
        ));
    }
    if cdp_target_id.chars().count() > 256 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target claim cdp_target_id must be at most 256 Unicode scalar values",
        ));
    }
    if !cdp_target_id.chars().all(|ch| ('!'..='~').contains(&ch)) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target claim cdp_target_id must contain only visible ASCII characters",
        ));
    }
    Ok(())
}

fn validate_target_claim_ttl(ttl_ms: u64) -> Result<(), ErrorData> {
    if !(MIN_TARGET_CLAIM_TTL_MS..=MAX_TARGET_CLAIM_TTL_MS).contains(&ttl_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_claim ttl_ms must be in {MIN_TARGET_CLAIM_TTL_MS}..={MAX_TARGET_CLAIM_TTL_MS}, got {ttl_ms}"
            ),
        ));
    }
    Ok(())
}

fn conflict_error(
    tool: &'static str,
    requester_session_id: &str,
    conflict: &TargetClaimConflict,
    operation: &'static str,
) -> ErrorData {
    let now = unix_time_ms_now();
    let holder = conflict.entry.owner_session_id.clone();
    let read = conflict.entry.read(now);
    tracing::warn!(
        code = error_codes::TARGET_CO_OWNED,
        tool,
        operation,
        requester_session_id,
        holder_session_id = %holder,
        target_key = %conflict.entry.target_key,
        conflict_tool = conflict.tool,
        conflict_requester_session_id = %conflict.requester_session_id,
        "target claim conflict"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} refused target {} because it is claimed by live MCP session {holder}",
            conflict.entry.target_key
        ),
        Some(json!({
            "code": error_codes::TARGET_CO_OWNED,
            "tool": tool,
            "operation": operation,
            "source_of_truth": source_of_truth(),
            "target_key": conflict.entry.target_key,
            "target": read.target,
            "requester_session_id": requester_session_id,
            "holder_session_id": holder,
            "claim": read,
            "read_only_observe_allowed": true,
            "mutation_allowed": false,
            "resolution": "release the claim, wait for claim expiry, use a different target, or call target_claim_adopt only for an older idle same-agent owner read from the Source of Truth",
        })),
    )
}

fn target_claim_not_found_error(
    tool: &'static str,
    requester_session_id: &str,
    target: &SessionTarget,
) -> ErrorData {
    let key = target_key(target);
    tracing::warn!(
        code = error_codes::TARGET_CLAIM_NOT_FOUND,
        tool,
        requester_session_id,
        target_key = %key,
        "target claim adoption found no live claim for target"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("{tool} found no live claim for target {key}"),
        Some(json!({
            "code": error_codes::TARGET_CLAIM_NOT_FOUND,
            "tool": tool,
            "requester_session_id": requester_session_id,
            "target_key": key,
            "target": target_wire(target),
            "source_of_truth": source_of_truth(),
            "resolution": "read target_claim_status/session_list and call target_claim for unowned or expired targets",
        })),
    )
}

fn adopt_refused_error(
    requester_session_id: &str,
    owner_session_id: &str,
    reason: &str,
    requester: Option<&super::session_registry::SessionRegistryRead>,
    owner: Option<&super::session_registry::SessionRegistryRead>,
) -> ErrorData {
    tracing::warn!(
        code = error_codes::TARGET_CLAIM_ADOPT_REFUSED,
        requester_session_id,
        owner_session_id,
        reason,
        "target claim adoption refused"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("target_claim_adopt refused: {reason}"),
        Some(json!({
            "code": error_codes::TARGET_CLAIM_ADOPT_REFUSED,
            "tool": "target_claim_adopt",
            "requester_session_id": requester_session_id,
            "owner_session_id": owner_session_id,
            "reason": reason,
            "requester": requester,
            "owner": owner,
            "source_of_truth": "session registry + daemon target claim registry",
            "mutation_allowed": false,
        })),
    )
}

fn owner_active_error(
    requester_session_id: &str,
    owner_session_id: &str,
    owner_in_flight: Vec<crate::daemon_lifecycle::InFlightToolCallRead>,
    reason: &str,
) -> ErrorData {
    let lease_status = synapse_action::lease::status();
    tracing::warn!(
        code = error_codes::TARGET_CLAIM_OWNER_ACTIVE,
        requester_session_id,
        owner_session_id,
        reason,
        in_flight_count = owner_in_flight.len(),
        lease_owner_session_id = ?lease_status.owner_session_id,
        "target claim adoption refused because owner is active"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("target_claim_adopt refused: {reason}"),
        Some(json!({
            "code": error_codes::TARGET_CLAIM_OWNER_ACTIVE,
            "tool": "target_claim_adopt",
            "requester_session_id": requester_session_id,
            "owner_session_id": owner_session_id,
            "reason": reason,
            "owner_in_flight": owner_in_flight,
            "lease": lease_status,
            "source_of_truth": "daemon lifecycle in-flight ledger + input lease registry",
            "mutation_allowed": false,
        })),
    )
}

fn require_claim_session_id(
    request_context: &RequestContext<RoleServer>,
) -> Result<String, ErrorData> {
    super::context::mcp_session_id_from_request_context(request_context)?.ok_or_else(|| {
        mcp_error(
            error_codes::HTTP_SESSION_INVALID,
            "target claim tools require an MCP session id",
        )
    })
}

const fn default_target_claim_ttl_ms() -> u64 {
    DEFAULT_TARGET_CLAIM_TTL_MS
}

fn source_of_truth() -> String {
    "daemon target claim registry".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::schema_for;

    fn collect_window_hwnd_ranges(value: &serde_json::Value, ranges: &mut Vec<(u64, u64)>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(window_hwnd) = object.get("window_hwnd")
                    && let (Some(minimum), Some(maximum)) = (
                        window_hwnd
                            .get("minimum")
                            .and_then(serde_json::Value::as_u64),
                        window_hwnd
                            .get("maximum")
                            .and_then(serde_json::Value::as_u64),
                    )
                {
                    ranges.push((minimum, maximum));
                }
                for child in object.values() {
                    collect_window_hwnd_ranges(child, ranges);
                }
            }
            serde_json::Value::Array(array) => {
                for child in array {
                    collect_window_hwnd_ranges(child, ranges);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn target_claim_target_schema_enforces_canonical_window_handle_range() {
        let schema = serde_json::to_value(schema_for!(TargetClaimTargetParam))
            .expect("target claim target schema should serialize");
        let mut ranges = Vec::new();
        collect_window_hwnd_ranges(&schema, &mut ranges);

        assert_eq!(
            ranges,
            vec![(1, u64::from(u32::MAX)), (1, u64::from(u32::MAX))],
            "schema={schema}"
        );
    }

    #[test]
    fn claim_conflict_is_reported_for_live_other_owner() {
        let mut registry = TargetClaimRegistry::default();
        let target = SessionTarget::Window { hwnd: 0x1234 };
        let live = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        let first = registry
            .claim("a", target.clone(), 10_000, 1_000, &live)
            .expect("first claim should succeed");
        println!(
            "readback=target_claim first key={} owner={}",
            first.0.target_key, first.0.owner_session_id
        );

        let conflict = registry
            .claim("b", target, 10_000, 1_001, &live)
            .expect_err("second live owner must conflict");

        assert_eq!(conflict.entry.owner_session_id, "a");
        assert_eq!(conflict.entry.target_key, "window:0x1234");
    }

    #[test]
    fn expired_or_stale_claim_does_not_block_new_owner() {
        let mut registry = TargetClaimRegistry::default();
        let target = SessionTarget::Window { hwnd: 0x5678 };
        let live_a = BTreeSet::from(["a".to_owned()]);
        registry
            .claim("a", target.clone(), 1_000, 1_000, &live_a)
            .expect("initial claim should succeed");
        let live_b = BTreeSet::from(["b".to_owned()]);
        registry.prune_inactive(2_001, &live_b);
        let second = registry
            .claim("b", target, 10_000, 2_001, &live_b)
            .expect("expired stale claim should be pruned");

        println!(
            "readback=target_claim edge=expired owner_after={}",
            second.0.owner_session_id
        );
        assert_eq!(second.0.owner_session_id, "b");
    }

    #[test]
    fn claim_generation_increments_only_on_new_owner() {
        let mut registry = TargetClaimRegistry::default();
        let target = SessionTarget::Window { hwnd: 0x9012 };
        let live_a = BTreeSet::from(["a".to_owned()]);
        let first = registry
            .claim("a", target.clone(), 10_000, 1_000, &live_a)
            .expect("first claim should succeed")
            .0;
        let renewed = registry
            .claim("a", target.clone(), 10_000, 1_500, &live_a)
            .expect("same owner renew should succeed")
            .0;
        let live_b = BTreeSet::from(["b".to_owned()]);
        registry.prune_inactive(2_001, &live_b);
        let reclaimed = registry
            .claim("b", target, 10_000, 2_001, &live_b)
            .expect("new owner should claim after stale prune")
            .0;

        println!(
            "readback=target_claim_generation first={} renewed={} reclaimed={}",
            first.generation, renewed.generation, reclaimed.generation
        );
        assert_eq!(first.generation, 1);
        assert_eq!(renewed.generation, first.generation);
        assert!(reclaimed.generation > renewed.generation);
        assert_eq!(reclaimed.owner_session_id, "b");
    }

    #[test]
    fn target_release_shape_validation_accepts_dead_nonzero_window_key() {
        let target = SessionTarget::Window { hwnd: 0xd17ee };

        validate_claim_target_shape(&target)
            .expect("release cleanup should not require HWND liveness for a non-zero claim key");
    }

    #[test]
    fn target_release_shape_validation_rejects_noncanonical_window_keys() {
        for hwnd in [-1, 0, i64::from(u32::MAX) + 1, i64::MAX] {
            let target = SessionTarget::Window { hwnd };
            let error = validate_claim_target_shape(&target)
                .expect_err("noncanonical HWND has no valid claim key shape");
            let data = error.data.expect("shape error must be structured");

            assert_eq!(
                data.get("code").and_then(|code| code.as_str()),
                Some(error_codes::TOOL_PARAMS_INVALID)
            );
            assert_eq!(
                data.get("actual_value").and_then(|value| value.as_i64()),
                Some(hwnd)
            );
        }
    }
}

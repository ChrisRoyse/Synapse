use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::{ErrorData, model::ErrorCode, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use synapse_core::{Backend, ProfileId, ProfileUseScope, error_codes, types::RoutineRecord};
use synapse_profiles::{ProfileError, ProfileRuntime, ProfileStatus};
use synapse_reflex::ReflexRuntime;
use synapse_storage::{Db, cf, decode_json, encode_json};

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, normalize_replay_path, replay_root, required},
    plan::{PlanDocument, RoutineCompilePlanParams, compile_routine_plan},
    routines::{load_routine_record, validate_routine_id_param},
};

const AUTHORING_PREFIX: &str = "profile_authoring/v1/";
const CANDIDATE_PREFIX: &str = "profile_authoring/v1/candidate/";
const ROUTINE_AUTOMATION_PREFIX: &str = "routine_automation/v1/";
const AUTHORING_SCHEMA_VERSION: u32 = 1;
const DEFAULT_MAX_AUDIT_ROWS: u32 = 500;
const DEFAULT_MAX_REPLAY_ROWS: u32 = 500;
const DEFAULT_LIST_LIMIT: u32 = 100;
const MAX_AUTHORING_ROWS: u32 = 10_000;
const CANDIDATE_ID_HASH_CHARS: usize = 16;

type RawStorageRow = (Vec<u8>, Vec<u8>);
type RawStorageRows = Vec<RawStorageRow>;

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringGenerateParams {
    pub profile_id: ProfileId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_path: Option<String>,
    #[serde(default = "default_max_audit_rows")]
    #[schemars(default = "default_max_audit_rows", range(min = 0, max = 10000))]
    pub max_audit_rows: u32,
    #[serde(default = "default_max_replay_rows")]
    #[schemars(default = "default_max_replay_rows", range(min = 0, max = 10000))]
    pub max_replay_rows: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<ProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default = "default_list_limit")]
    #[schemars(default = "default_list_limit", range(min = 1, max = 1000))]
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringInspectParams {
    pub candidate_id: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub enum ProfileAuthoringDecision {
    Accept,
    Reject,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringDecideParams {
    pub candidate_id: String,
    pub decision: ProfileAuthoringDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringExportParams {
    pub candidate_id: String,
    pub output_path: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineAutomateParams {
    pub routine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<ProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub store_plan: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringCandidate {
    pub schema_version: u32,
    pub row_kind: String,
    pub candidate_id: String,
    pub profile_id: ProfileId,
    pub state: String,
    pub generated_at_ns: u64,
    pub updated_at_ns: u64,
    pub accepted_at_ns: Option<u64>,
    pub rejected_at_ns: Option<u64>,
    pub operator_note: Option<String>,
    pub rejection_reason: Option<String>,
    pub source: ProfileAuthoringSource,
    pub evidence_summary: Value,
    pub expected_improvement: Vec<String>,
    pub patch: Value,
    pub safety_review: ProfileAuthoringSafetyReview,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringSource {
    pub audit_cf_name: String,
    pub profile_cf_name: String,
    pub replay_path: Option<String>,
    pub audit_rows_scanned: u64,
    pub audit_rows_relevant: u64,
    pub replay_rows_scanned: u64,
    pub replay_rows_relevant: u64,
    pub source_row_keys: Vec<String>,
    pub source_audit_ids: Vec<String>,
    pub evidence_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringSafetyReview {
    pub activation: ProfileAuthoringActivationReview,
    pub contribution: ProfileAuthoringContributionReview,
    pub unsafe_permission_escalation: bool,
    pub rejected_reasons: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringActivationReview {
    pub required: bool,
    pub active_on_accept: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringContributionReview {
    pub registry_allowed: bool,
    pub external_sharing_allowed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringCandidateSummary {
    pub cf_name: String,
    pub row_key: String,
    pub candidate_id: String,
    pub profile_id: ProfileId,
    pub state: String,
    pub generated_at_ns: u64,
    pub updated_at_ns: u64,
    pub evidence_hash: String,
    pub expected_improvement: Vec<String>,
    pub value_len_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringGenerateResponse {
    pub cf_name: String,
    pub row_key: String,
    pub wrote_row: bool,
    pub active_profile_id: Option<ProfileId>,
    pub candidate: ProfileAuthoringCandidate,
    pub summary: ProfileAuthoringCandidateSummary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringListResponse {
    pub cf_name: String,
    pub prefix: String,
    pub profile_id: Option<ProfileId>,
    pub state: Option<String>,
    pub limit: u32,
    pub total_matched: u64,
    pub candidates: Vec<ProfileAuthoringCandidateSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringInspectResponse {
    pub cf_name: String,
    pub row_key: String,
    pub found: bool,
    pub candidate: Option<ProfileAuthoringCandidate>,
    pub summary: Option<ProfileAuthoringCandidateSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringDecideResponse {
    pub cf_name: String,
    pub row_key: String,
    pub candidate_id: String,
    pub profile_id: ProfileId,
    pub previous_state: String,
    pub state: String,
    pub decision: ProfileAuthoringDecision,
    pub wrote_row: bool,
    pub activated: bool,
    pub active_profile_id: Option<ProfileId>,
    pub candidate: ProfileAuthoringCandidate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAuthoringExportResponse {
    pub output_path: String,
    pub bytes_written: u64,
    pub cf_name: String,
    pub row_key: String,
    pub candidate_id: String,
    pub profile_id: ProfileId,
    pub state: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineAutomationRecord {
    pub schema_version: u32,
    pub row_kind: String,
    pub routine_id: String,
    pub profile_id: ProfileId,
    pub candidate_id: String,
    pub candidate_row_key: String,
    pub plan_ref: String,
    pub state: String,
    pub generated_at_ns: u64,
    pub updated_at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_at_ns: Option<u64>,
    pub plan_fully_deterministic: bool,
    pub total_steps: u32,
    pub deterministic_steps: u32,
    pub agent_task_steps: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineAutomateResponse {
    pub cf_name: String,
    pub row_key: String,
    pub wrote_row: bool,
    pub active_profile_id: Option<ProfileId>,
    pub candidate: ProfileAuthoringCandidate,
    pub summary: ProfileAuthoringCandidateSummary,
    pub plan: PlanDocument,
    pub automation: RoutineAutomationRecord,
}

#[must_use]
pub const fn profile_authoring_generate() -> M3ToolStub {
    M3ToolStub::new("profile_authoring_generate")
}

#[must_use]
pub const fn profile_authoring_list() -> M3ToolStub {
    M3ToolStub::new("profile_authoring_list")
}

#[must_use]
pub const fn profile_authoring_inspect() -> M3ToolStub {
    M3ToolStub::new("profile_authoring_inspect")
}

#[must_use]
pub const fn profile_authoring_decide() -> M3ToolStub {
    M3ToolStub::new("profile_authoring_decide")
}

#[must_use]
pub const fn profile_authoring_export() -> M3ToolStub {
    M3ToolStub::new("profile_authoring_export")
}

#[must_use]
pub const fn routine_automate() -> M3ToolStub {
    M3ToolStub::new("routine_automate")
}

#[must_use]
pub fn required_permissions_generate(
    _params: &ProfileAuthoringGenerateParams,
) -> RequiredPermissions {
    required([
        Permission::ReadProfile,
        Permission::ReadStorage,
        Permission::WriteStorage,
    ])
}

#[must_use]
pub fn required_permissions_list(_params: &ProfileAuthoringListParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_inspect(
    _params: &ProfileAuthoringInspectParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_decide(params: &ProfileAuthoringDecideParams) -> RequiredPermissions {
    match params.decision {
        ProfileAuthoringDecision::Accept => required([
            Permission::ReadProfile,
            Permission::ReadStorage,
            Permission::WriteStorage,
        ]),
        ProfileAuthoringDecision::Reject => {
            required([Permission::ReadStorage, Permission::WriteStorage])
        }
    }
}

#[must_use]
pub fn required_permissions_export(_params: &ProfileAuthoringExportParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_routine_automate(
    _params: &RoutineAutomateParams,
) -> RequiredPermissions {
    required([
        Permission::ReadProfile,
        Permission::ReadStorage,
        Permission::WriteStorage,
    ])
}

pub fn generate_profile_authoring_candidate(
    profile_runtime: &ProfileRuntime,
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileAuthoringGenerateParams,
) -> Result<ProfileAuthoringGenerateResponse, ErrorData> {
    validate_generation_limits(params.max_audit_rows, params.max_replay_rows)?;
    let profile = profile_status(profile_runtime, &params.profile_id)?;
    let audit_rows = audit_rows(reflex_runtime, params.max_audit_rows)?;
    let replay_records = replay_records(params.replay_path.as_deref(), params.max_replay_rows)?;

    let mut builder = EvidenceBuilder::new(&profile);
    for (key, value) in audit_rows {
        builder.record_audit_row(&key, &value)?;
    }
    for record in replay_records.records {
        builder.record_replay_record(&record);
    }

    let now_ns = now_ns();
    let built = builder.finish(replay_records.path, now_ns)?;
    let candidate_id = params
        .candidate_id
        .as_deref()
        .map(normalized_candidate_id)
        .transpose()?
        .unwrap_or_else(|| generated_candidate_id(&profile.id, &built.source.evidence_hash));
    let row_key = candidate_key(&candidate_id);
    let candidate = ProfileAuthoringCandidate {
        schema_version: AUTHORING_SCHEMA_VERSION,
        row_kind: "profile_authoring_candidate".to_owned(),
        candidate_id,
        profile_id: profile.id,
        state: "candidate".to_owned(),
        generated_at_ns: now_ns,
        updated_at_ns: now_ns,
        accepted_at_ns: None,
        rejected_at_ns: None,
        operator_note: None,
        rejection_reason: None,
        source: built.source,
        evidence_summary: built.evidence_summary,
        expected_improvement: built.expected_improvement,
        patch: built.patch,
        safety_review: ProfileAuthoringSafetyReview {
            activation: ProfileAuthoringActivationReview {
                required: true,
                active_on_accept: false,
            },
            contribution: ProfileAuthoringContributionReview {
                registry_allowed: false,
                external_sharing_allowed: false,
            },
            unsafe_permission_escalation: false,
            rejected_reasons: Vec::new(),
        },
    };
    let encoded = encode_candidate(&candidate)?;
    let runtime = lock_runtime(reflex_runtime, "writing profile authoring candidate")?;
    runtime
        .storage_put_profile_rows(vec![(row_key.as_bytes().to_vec(), encoded)])
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let readback = runtime
        .storage_profile_row(row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                "profile authoring candidate was not readable after write",
            )
        })?;
    drop(runtime);
    let candidate = decode_candidate(&readback)?;
    let summary = candidate_summary(&row_key, &candidate, readback.len());
    let active_profile_id = profile_runtime
        .active_profile_id()
        .map_err(|error| profile_error(&error))?;
    Ok(ProfileAuthoringGenerateResponse {
        cf_name: cf::CF_PROFILES.to_owned(),
        row_key,
        wrote_row: true,
        active_profile_id,
        candidate,
        summary,
    })
}

pub fn generate_routine_automation_candidate(
    profile_runtime: &ProfileRuntime,
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    db: &Arc<Db>,
    params: &RoutineAutomateParams,
) -> Result<RoutineAutomateResponse, ErrorData> {
    validate_routine_id_param("routine_automate", &params.routine_id)?;
    let profile_id = resolve_routine_automation_profile_id(profile_runtime, params)?;
    let profile = profile_status(profile_runtime, &profile_id)?;
    let Some(routine) = load_routine_record(db, &params.routine_id)? else {
        return Err(authoring_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "ROUTINE_NOT_MINED: routine_id {} is not in CF_ROUTINES; nothing to automate",
                params.routine_id
            ),
            json!({
                "routine_id": params.routine_id,
                "cf_name": cf::CF_ROUTINES,
            }),
        ));
    };
    if !db.pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "routine_automate refused under disk pressure: cf_name={} pressure_level={:?}",
                cf::CF_KV,
                db.pressure_level()
            ),
        ));
    }

    let plan_response = compile_routine_plan(
        db,
        &RoutineCompilePlanParams {
            routine_id: params.routine_id.clone(),
            store: params.store_plan,
        },
    )?;
    let plan = plan_response.plan;
    let now_ns = now_ns();
    let evidence_hash = routine_automation_evidence_hash(&profile.id, &routine, &plan)?;
    let candidate_id = params
        .candidate_id
        .as_deref()
        .map(normalized_candidate_id)
        .transpose()?
        .unwrap_or_else(|| routine_automation_candidate_id(&routine.routine_id, &evidence_hash));
    let row_key = candidate_key(&candidate_id);
    let plan_ref = format!("plan/v1/{}", routine.routine_id);
    let patch = routine_automation_patch(&profile, &routine, &plan, &candidate_id, &plan_ref);
    let evidence_summary =
        routine_automation_evidence_summary(&routine, &plan, plan_response.stored);
    let expected_improvement = vec![
        format!(
            "Promote routine {} into a reviewable, armable automation candidate",
            routine.routine_id
        ),
        format!(
            "Preserve {} compiled setup steps, including {} judgment-required agent task stubs",
            plan.total_steps, plan.agent_task_steps
        ),
        "Reuse profile_authoring_inspect/decide/export for operator review and installation state"
            .to_owned(),
    ];
    let candidate = ProfileAuthoringCandidate {
        schema_version: AUTHORING_SCHEMA_VERSION,
        row_kind: "profile_authoring_candidate".to_owned(),
        candidate_id,
        profile_id: profile.id.clone(),
        state: "candidate".to_owned(),
        generated_at_ns: now_ns,
        updated_at_ns: now_ns,
        accepted_at_ns: None,
        rejected_at_ns: None,
        operator_note: None,
        rejection_reason: None,
        source: ProfileAuthoringSource {
            audit_cf_name: cf::CF_ROUTINES.to_owned(),
            profile_cf_name: cf::CF_PROFILES.to_owned(),
            replay_path: None,
            audit_rows_scanned: 1,
            audit_rows_relevant: 1,
            replay_rows_scanned: 0,
            replay_rows_relevant: 0,
            source_row_keys: vec![hex_encode(routine.routine_id.as_bytes())],
            source_audit_ids: routine
                .evidence
                .iter()
                .flat_map(|evidence| evidence.episode_ids.iter().cloned())
                .collect(),
            evidence_hash,
        },
        evidence_summary,
        expected_improvement,
        patch,
        safety_review: ProfileAuthoringSafetyReview {
            activation: ProfileAuthoringActivationReview {
                required: true,
                active_on_accept: false,
            },
            contribution: ProfileAuthoringContributionReview {
                registry_allowed: false,
                external_sharing_allowed: false,
            },
            unsafe_permission_escalation: false,
            rejected_reasons: Vec::new(),
        },
    };
    let encoded = encode_candidate(&candidate)?;
    let runtime = lock_runtime(reflex_runtime, "writing routine automation candidate")?;
    runtime
        .storage_put_profile_rows(vec![(row_key.as_bytes().to_vec(), encoded)])
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let readback = runtime
        .storage_profile_row(row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                "routine automation candidate was not readable after write",
            )
        })?;
    drop(runtime);
    let candidate = decode_candidate(&readback)?;
    let summary = candidate_summary(&row_key, &candidate, readback.len());
    let automation = RoutineAutomationRecord {
        schema_version: AUTHORING_SCHEMA_VERSION,
        row_kind: "routine_automation".to_owned(),
        routine_id: routine.routine_id.clone(),
        profile_id: profile.id,
        candidate_id: candidate.candidate_id.clone(),
        candidate_row_key: row_key.clone(),
        plan_ref,
        state: "candidate".to_owned(),
        generated_at_ns: now_ns,
        updated_at_ns: now_ns,
        installed_at_ns: None,
        rejected_at_ns: None,
        plan_fully_deterministic: plan.fully_deterministic,
        total_steps: plan.total_steps,
        deterministic_steps: plan.deterministic_steps,
        agent_task_steps: plan.agent_task_steps,
    };
    write_routine_automation_record(db, &automation)?;
    let automation = load_routine_automation_record(db, &routine.routine_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            "routine automation row was not readable after write",
        )
    })?;
    let active_profile_id = profile_runtime
        .active_profile_id()
        .map_err(|error| profile_error(&error))?;
    Ok(RoutineAutomateResponse {
        cf_name: cf::CF_PROFILES.to_owned(),
        row_key,
        wrote_row: true,
        active_profile_id,
        candidate,
        summary,
        plan,
        automation,
    })
}

pub fn list_profile_authoring_candidates(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileAuthoringListParams,
) -> Result<ProfileAuthoringListResponse, ErrorData> {
    let limit = validate_list_limit(params.limit)?;
    let runtime = lock_runtime(reflex_runtime, "listing profile authoring candidates")?;
    let rows = runtime
        .storage_cf_prefix_rows(cf::CF_PROFILES, CANDIDATE_PREFIX.as_bytes(), usize::MAX)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    drop(runtime);

    let state = params.state.as_deref().map(normalized_state).transpose()?;
    let mut candidates = Vec::new();
    let mut total_matched = 0_u64;
    for (key, value) in rows {
        let candidate = decode_candidate(&value)?;
        if params
            .profile_id
            .as_deref()
            .is_some_and(|profile_id| profile_id != candidate.profile_id)
        {
            continue;
        }
        if state
            .as_deref()
            .is_some_and(|state| state != candidate.state)
        {
            continue;
        }
        total_matched = total_matched.saturating_add(1);
        if candidates.len() < limit {
            candidates.push(candidate_summary(
                &String::from_utf8_lossy(&key),
                &candidate,
                value.len(),
            ));
        }
    }
    Ok(ProfileAuthoringListResponse {
        cf_name: cf::CF_PROFILES.to_owned(),
        prefix: CANDIDATE_PREFIX.to_owned(),
        profile_id: params.profile_id.clone(),
        state,
        limit: params.limit,
        total_matched,
        candidates,
    })
}

pub fn inspect_profile_authoring_candidate(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileAuthoringInspectParams,
) -> Result<ProfileAuthoringInspectResponse, ErrorData> {
    let candidate_id = normalized_candidate_id(&params.candidate_id)?;
    let row_key = candidate_key(&candidate_id);
    let runtime = lock_runtime(reflex_runtime, "inspecting profile authoring candidate")?;
    let row = runtime
        .storage_profile_row(row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    drop(runtime);
    let Some(row) = row else {
        return Ok(ProfileAuthoringInspectResponse {
            cf_name: cf::CF_PROFILES.to_owned(),
            row_key,
            found: false,
            candidate: None,
            summary: None,
        });
    };
    let candidate = decode_candidate(&row)?;
    let summary = candidate_summary(&row_key, &candidate, row.len());
    Ok(ProfileAuthoringInspectResponse {
        cf_name: cf::CF_PROFILES.to_owned(),
        row_key,
        found: true,
        candidate: Some(candidate),
        summary: Some(summary),
    })
}

pub fn decide_profile_authoring_candidate(
    profile_runtime: &ProfileRuntime,
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    db: &Arc<Db>,
    params: &ProfileAuthoringDecideParams,
) -> Result<ProfileAuthoringDecideResponse, ErrorData> {
    validate_decide_fields(params)?;
    match params.decision {
        ProfileAuthoringDecision::Accept => {
            decide_accept_candidate(profile_runtime, reflex_runtime, db, params)
        }
        ProfileAuthoringDecision::Reject => decide_reject_candidate(reflex_runtime, db, params),
    }
}

fn validate_decide_fields(params: &ProfileAuthoringDecideParams) -> Result<(), ErrorData> {
    match params.decision {
        ProfileAuthoringDecision::Accept if params.reason.is_some() => Err(authoring_error(
            error_codes::TOOL_PARAMS_INVALID,
            "profile_authoring_decide reason is only valid for decision=reject",
            json!({
                "candidate_id": &params.candidate_id,
                "decision": "accept",
                "invalid_field": "reason",
            }),
        )),
        ProfileAuthoringDecision::Reject if params.operator_note.is_some() => Err(authoring_error(
            error_codes::TOOL_PARAMS_INVALID,
            "profile_authoring_decide operator_note is only valid for decision=accept",
            json!({
                "candidate_id": &params.candidate_id,
                "decision": "reject",
                "invalid_field": "operator_note",
            }),
        )),
        _ => Ok(()),
    }
}

fn decide_accept_candidate(
    profile_runtime: &ProfileRuntime,
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    db: &Arc<Db>,
    params: &ProfileAuthoringDecideParams,
) -> Result<ProfileAuthoringDecideResponse, ErrorData> {
    let candidate_id = normalized_candidate_id(&params.candidate_id)?;
    let row_key = candidate_key(&candidate_id);
    let (mut candidate, previous_state) = read_candidate_required(reflex_runtime, &row_key)?;
    let wrote_row = if candidate.state == "accepted" {
        false
    } else if candidate.state == "candidate" {
        "accepted".clone_into(&mut candidate.state);
        candidate.updated_at_ns = now_ns();
        candidate.accepted_at_ns = Some(candidate.updated_at_ns);
        candidate.operator_note.clone_from(&params.operator_note);
        write_candidate(reflex_runtime, &row_key, &candidate)?;
        true
    } else {
        return Err(authoring_error(
            error_codes::PROFILE_AUTHORING_INVALID_STATE,
            format!(
                "candidate {} cannot be accepted from state {}",
                candidate.candidate_id, candidate.state
            ),
            json!({
                "candidate_id": candidate.candidate_id,
                "state": candidate.state,
            }),
        ));
    };
    let candidate = read_candidate_required(reflex_runtime, &row_key)?.0;
    maybe_mark_routine_automation_decision(db, &candidate, "installed", candidate.accepted_at_ns)?;
    let active_profile_id = profile_runtime
        .active_profile_id()
        .map_err(|error| profile_error(&error))?;
    Ok(ProfileAuthoringDecideResponse {
        cf_name: cf::CF_PROFILES.to_owned(),
        row_key,
        candidate_id: candidate.candidate_id.clone(),
        profile_id: candidate.profile_id.clone(),
        previous_state,
        state: candidate.state.clone(),
        decision: ProfileAuthoringDecision::Accept,
        wrote_row,
        activated: false,
        active_profile_id,
        candidate,
    })
}

fn decide_reject_candidate(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    db: &Arc<Db>,
    params: &ProfileAuthoringDecideParams,
) -> Result<ProfileAuthoringDecideResponse, ErrorData> {
    let candidate_id = normalized_candidate_id(&params.candidate_id)?;
    let row_key = candidate_key(&candidate_id);
    let (mut candidate, previous_state) = read_candidate_required(reflex_runtime, &row_key)?;
    let wrote_row = if candidate.state == "rejected" {
        false
    } else if candidate.state == "candidate" {
        "rejected".clone_into(&mut candidate.state);
        candidate.updated_at_ns = now_ns();
        candidate.rejected_at_ns = Some(candidate.updated_at_ns);
        candidate.rejection_reason.clone_from(&params.reason);
        write_candidate(reflex_runtime, &row_key, &candidate)?;
        true
    } else {
        return Err(authoring_error(
            error_codes::PROFILE_AUTHORING_INVALID_STATE,
            format!(
                "candidate {} cannot be rejected from state {}",
                candidate.candidate_id, candidate.state
            ),
            json!({
                "candidate_id": candidate.candidate_id,
                "state": candidate.state,
            }),
        ));
    };
    let candidate = read_candidate_required(reflex_runtime, &row_key)?.0;
    maybe_mark_routine_automation_decision(db, &candidate, "rejected", candidate.rejected_at_ns)?;
    Ok(ProfileAuthoringDecideResponse {
        cf_name: cf::CF_PROFILES.to_owned(),
        row_key,
        candidate_id: candidate.candidate_id.clone(),
        profile_id: candidate.profile_id.clone(),
        previous_state,
        state: candidate.state.clone(),
        decision: ProfileAuthoringDecision::Reject,
        wrote_row,
        activated: false,
        active_profile_id: None,
        candidate,
    })
}

pub fn export_profile_authoring_candidate(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileAuthoringExportParams,
) -> Result<ProfileAuthoringExportResponse, ErrorData> {
    let candidate_id = normalized_candidate_id(&params.candidate_id)?;
    let row_key = candidate_key(&candidate_id);
    let (candidate, _previous_state) = read_candidate_required(reflex_runtime, &row_key)?;
    let output_path = PathBuf::from(params.output_path.trim());
    if output_path.as_os_str().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "profile_authoring_export output_path must not be empty",
        ));
    }
    if let Some(parent) = output_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "profile_authoring_export could not create parent directory {}: {error}",
                    parent.display()
                ),
            )
        })?;
    }
    let bundle = json!({
        "schema_version": AUTHORING_SCHEMA_VERSION,
        "exported_at_ns": now_ns(),
        "cf_name": cf::CF_PROFILES,
        "row_key": row_key,
        "candidate": candidate,
    });
    let bytes = serde_json::to_vec_pretty(&bundle).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("profile_authoring_export could not encode bundle: {error}"),
        )
    })?;
    fs::write(&output_path, &bytes).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "profile_authoring_export could not write {}: {error}",
                output_path.display()
            ),
        )
    })?;
    let written = fs::read(&output_path).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "profile_authoring_export could not read back {}: {error}",
                output_path.display()
            ),
        )
    })?;
    let readback = serde_json::from_slice::<Value>(&written).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "profile_authoring_export could not parse readback {}: {error}",
                output_path.display()
            ),
        )
    })?;
    let candidate = readback
        .get("candidate")
        .cloned()
        .and_then(|value| serde_json::from_value::<ProfileAuthoringCandidate>(value).ok())
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "profile_authoring_export encoded candidate could not be read back",
            )
        })?;
    Ok(ProfileAuthoringExportResponse {
        output_path: output_path.display().to_string(),
        bytes_written: written.len() as u64,
        cf_name: cf::CF_PROFILES.to_owned(),
        row_key,
        candidate_id: candidate.candidate_id,
        profile_id: candidate.profile_id,
        state: candidate.state,
    })
}

pub(crate) fn load_routine_automation_record(
    db: &Arc<Db>,
    routine_id: &str,
) -> Result<Option<RoutineAutomationRecord>, ErrorData> {
    validate_routine_id_param("routine_inspect", routine_id)?;
    let key = routine_automation_key(routine_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, key.as_bytes())
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!("routine automation storage read failed: {error}"),
            )
        })?;
    match rows
        .into_iter()
        .find(|(row_key, _)| row_key == key.as_bytes())
    {
        Some((_key, value)) => decode_json::<RoutineAutomationRecord>(&value)
            .map(Some)
            .map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("ROUTINE_AUTOMATION_ROW_DECODE_FAILED for {routine_id}: {error}"),
                )
            }),
        None => Ok(None),
    }
}

fn resolve_routine_automation_profile_id(
    runtime: &ProfileRuntime,
    params: &RoutineAutomateParams,
) -> Result<ProfileId, ErrorData> {
    if let Some(profile_id) = params.profile_id.as_ref() {
        return Ok(profile_id.clone());
    }
    runtime
        .active_profile_id()
        .map_err(|error| profile_error(&error))?
        .ok_or_else(|| {
            authoring_error(
                error_codes::TOOL_PARAMS_INVALID,
                "routine_automate requires profile_id when no active profile is set",
                json!({
                    "routine_id": params.routine_id,
                    "missing_field": "profile_id",
                }),
            )
        })
}

fn routine_automation_key(routine_id: &str) -> String {
    format!("{ROUTINE_AUTOMATION_PREFIX}{routine_id}")
}

fn write_routine_automation_record(
    db: &Arc<Db>,
    record: &RoutineAutomationRecord,
) -> Result<(), ErrorData> {
    let key = routine_automation_key(&record.routine_id);
    let value = encode_json(record).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to encode routine automation row for {}: {error}",
                record.routine_id
            ),
        )
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(key.into_bytes(), value)])
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "failed to persist routine automation row for {}: {error}",
                    record.routine_id
                ),
            )
        })
}

fn maybe_mark_routine_automation_decision(
    db: &Arc<Db>,
    candidate: &ProfileAuthoringCandidate,
    state: &str,
    decided_at_ns: Option<u64>,
) -> Result<(), ErrorData> {
    let Some(routine_id) = routine_id_from_candidate(candidate) else {
        return Ok(());
    };
    let Some(mut record) = load_routine_automation_record(db, &routine_id)? else {
        return Ok(());
    };
    if record.candidate_id != candidate.candidate_id {
        return Ok(());
    }
    record.state = state.to_owned();
    record.updated_at_ns = decided_at_ns.unwrap_or_else(now_ns);
    match state {
        "installed" => {
            record.installed_at_ns = Some(record.updated_at_ns);
            record.rejected_at_ns = None;
        }
        "rejected" => {
            record.rejected_at_ns = Some(record.updated_at_ns);
        }
        _ => {}
    }
    write_routine_automation_record(db, &record)
}

fn routine_id_from_candidate(candidate: &ProfileAuthoringCandidate) -> Option<String> {
    candidate
        .patch
        .pointer("/routine_automation/routine_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn routine_automation_candidate_id(routine_id: &str, evidence_hash: &str) -> String {
    let suffix = evidence_hash
        .strip_prefix("sha256:")
        .unwrap_or(evidence_hash)
        .chars()
        .take(CANDIDATE_ID_HASH_CHARS)
        .collect::<String>();
    format!("routine-auto.{routine_id}.{suffix}")
}

fn routine_automation_evidence_hash(
    profile_id: &str,
    routine: &RoutineRecord,
    plan: &PlanDocument,
) -> Result<String, ErrorData> {
    let payload = json!({
        "kind": "routine_automation",
        "profile_id": profile_id,
        "routine": routine,
        "plan": {
            "record_version": plan.record_version,
            "routine_id": &plan.routine_id,
            "granularity": &plan.granularity,
            "schedule_label": &plan.schedule_label,
            "total_steps": plan.total_steps,
            "deterministic_steps": plan.deterministic_steps,
            "agent_task_steps": plan.agent_task_steps,
            "fully_deterministic": plan.fully_deterministic,
            "steps": &plan.steps,
        },
    });
    let bytes = serde_json::to_vec(&payload).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("routine automation evidence hash encode failed: {error}"),
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("sha256:{}", hex_encode(&hasher.finalize())))
}

fn routine_automation_evidence_summary(
    routine: &RoutineRecord,
    plan: &PlanDocument,
    plan_stored: bool,
) -> Value {
    json!({
        "source": "routine_automate",
        "routine_id": &routine.routine_id,
        "granularity": &routine.granularity,
        "schedule_label": &routine.schedule_label,
        "support_days": routine.support_days,
        "occurrence_count": routine.occurrence_count,
        "confidence": routine.confidence,
        "evidence_episode_ids": routine
            .evidence
            .iter()
            .flat_map(|evidence| evidence.episode_ids.iter())
            .collect::<Vec<_>>(),
        "plan": {
            "stored": plan_stored,
            "total_steps": plan.total_steps,
            "deterministic_steps": plan.deterministic_steps,
            "agent_task_steps": plan.agent_task_steps,
            "fully_deterministic": plan.fully_deterministic,
        },
    })
}

fn routine_automation_patch(
    profile: &ProfileStatus,
    routine: &RoutineRecord,
    plan: &PlanDocument,
    candidate_id: &str,
    plan_ref: &str,
) -> Value {
    json!({
        "schema_version": AUTHORING_SCHEMA_VERSION,
        "kind": "routine_automation_candidate",
        "profile_id": &profile.id,
        "routine_automation": {
            "routine_id": &routine.routine_id,
            "candidate_id": candidate_id,
            "candidate_row_key": candidate_key(candidate_id),
            "profile_id": &profile.id,
            "plan_ref": plan_ref,
            "install_state_key": routine_automation_key(&routine.routine_id),
            "source_routine": {
                "granularity": &routine.granularity,
                "schedule_label": &routine.schedule_label,
                "mean_minute_of_day": routine.mean_minute_of_day,
                "tolerance_minutes": routine.tolerance_minutes,
                "support_days": routine.support_days,
                "occurrence_count": routine.occurrence_count,
                "confidence": routine.confidence,
            },
            "task": {
                "title": routine_automation_title(routine),
                "instructions": routine_automation_instructions(routine, plan),
                "acceptance": [
                    "Run the compiled setup plan steps in order.",
                    "Verify every deterministic postcondition against physical source of truth.",
                    "For agent_task steps, use the routine evidence and report success/failure explicitly.",
                    "Do not arm unattended execution until the operator reviews and accepts this candidate."
                ],
            },
            "compiled_plan": plan,
        },
        "safety": {
            "activation_required": true,
            "operator_review_required": true,
            "auto_arm_on_accept": false,
        },
    })
}

fn routine_automation_title(routine: &RoutineRecord) -> String {
    format!(
        "Automate routine {} ({})",
        routine.routine_id, routine.schedule_label
    )
}

fn routine_automation_instructions(routine: &RoutineRecord, plan: &PlanDocument) -> Vec<String> {
    let mut instructions = vec![
        format!(
            "Use source routine {} mined from {} occurrence(s) over {} support day(s).",
            routine.routine_id, routine.occurrence_count, routine.support_days
        ),
        format!(
            "Schedule signature: {}; confidence {:.3}.",
            routine.schedule_label, routine.confidence
        ),
        "Execute setup steps through Synapse target-aware/background-first tools when possible; do not silently skip judgment-required steps.".to_owned(),
    ];
    instructions.extend(plan.steps.iter().map(|step| {
        format!(
            "Step {}: {}. Backend={:?}. Postcondition={:?}.",
            step.index, step.action, step.backend, step.postcondition
        )
    }));
    instructions
}

struct ReplayRecords {
    path: Option<String>,
    records: Vec<Value>,
}

struct BuiltCandidate {
    source: ProfileAuthoringSource,
    evidence_summary: Value,
    expected_improvement: Vec<String>,
    patch: Value,
}

struct EvidenceBuilder {
    profile_id: ProfileId,
    profile_schema_version: u32,
    profile_use_scope: ProfileUseScope,
    existing_exes: BTreeSet<String>,
    existing_hud_names: BTreeSet<String>,
    existing_detection_classes: BTreeSet<String>,
    existing_backends: BTreeMap<String, String>,
    audit_rows_scanned: u64,
    audit_rows_relevant: u64,
    replay_rows_scanned: u64,
    replay_rows_relevant: u64,
    source_row_keys: Vec<String>,
    source_audit_ids: Vec<String>,
    observed_process_names: BTreeMap<String, u64>,
    observed_backends: BTreeMap<String, u64>,
    observed_hud_fields: BTreeMap<String, u64>,
    observed_detection_classes: BTreeMap<String, u64>,
    tool_counts: BTreeMap<String, u64>,
    match_hints: BTreeSet<String>,
    hud_hints: BTreeMap<String, Value>,
    keymap_hints: BTreeMap<String, String>,
    backend_hints: BTreeMap<String, String>,
    detection_hints: BTreeSet<String>,
    reflex_hints: Vec<Value>,
    use_scope_hint: Option<String>,
    metadata_hints: BTreeMap<String, String>,
    conflict_reasons: Vec<String>,
    unsafe_reasons: Vec<String>,
}

impl EvidenceBuilder {
    fn new(profile: &ProfileStatus) -> Self {
        Self {
            profile_id: profile.id.clone(),
            profile_schema_version: profile.schema_version,
            profile_use_scope: profile.use_scope,
            existing_exes: profile
                .matches
                .iter()
                .filter_map(|value| value.exe.as_deref())
                .map(normalize_key)
                .collect(),
            existing_hud_names: profile
                .hud_fields
                .iter()
                .map(std::borrow::ToOwned::to_owned)
                .collect(),
            existing_detection_classes: profile
                .detection_classes
                .iter()
                .map(std::borrow::ToOwned::to_owned)
                .collect(),
            existing_backends: backends_map(profile),
            audit_rows_scanned: 0,
            audit_rows_relevant: 0,
            replay_rows_scanned: 0,
            replay_rows_relevant: 0,
            source_row_keys: Vec::new(),
            source_audit_ids: Vec::new(),
            observed_process_names: BTreeMap::new(),
            observed_backends: BTreeMap::new(),
            observed_hud_fields: BTreeMap::new(),
            observed_detection_classes: BTreeMap::new(),
            tool_counts: BTreeMap::new(),
            match_hints: BTreeSet::new(),
            hud_hints: BTreeMap::new(),
            keymap_hints: BTreeMap::new(),
            backend_hints: BTreeMap::new(),
            detection_hints: BTreeSet::new(),
            reflex_hints: Vec::new(),
            use_scope_hint: None,
            metadata_hints: BTreeMap::new(),
            conflict_reasons: Vec::new(),
            unsafe_reasons: Vec::new(),
        }
    }

    fn record_audit_row(&mut self, key: &[u8], value: &[u8]) -> Result<(), ErrorData> {
        self.audit_rows_scanned = self.audit_rows_scanned.saturating_add(1);
        let row = decode_json::<Value>(value).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("profile authoring could not decode audit row: {error}"),
            )
        })?;
        if !row_matches_profile(&row, &self.profile_id) {
            return Ok(());
        }
        self.audit_rows_relevant = self.audit_rows_relevant.saturating_add(1);
        self.source_row_keys.push(hex_encode(key));
        if let Some(audit_id) = optional_string(&row, "audit_id") {
            self.source_audit_ids.push(audit_id);
        }
        if let Some(process_name) = row
            .pointer("/foreground/process_name")
            .and_then(Value::as_str)
        {
            self.record_process_name(process_name);
        }
        if let Some(tool) = row.get("tool").and_then(Value::as_str) {
            increment(&mut self.tool_counts, tool);
        }
        if let Some(backend) = row
            .pointer("/details/response/backend_used")
            .or_else(|| row.pointer("/details/response/backend"))
            .or_else(|| row.get("backend"))
            .and_then(Value::as_str)
        {
            self.record_backend(backend);
        }
        if let Some(hints) = row
            .get("profile_authoring")
            .or_else(|| row.pointer("/details/profile_authoring"))
        {
            self.record_hints(hints);
        }
        Ok(())
    }

    fn record_replay_record(&mut self, record: &Value) {
        self.replay_rows_scanned = self.replay_rows_scanned.saturating_add(1);
        let normalized = replay_payload(record);
        if !row_matches_profile(&normalized, &self.profile_id) {
            return;
        }
        self.replay_rows_relevant = self.replay_rows_relevant.saturating_add(1);
        if let Some(process_name) = normalized
            .pointer("/foreground/process_name")
            .and_then(Value::as_str)
        {
            self.record_process_name(process_name);
        }
        if let Some(hud) = normalized
            .pointer("/hud/by_name")
            .and_then(Value::as_object)
        {
            for name in hud.keys() {
                increment(&mut self.observed_hud_fields, name);
            }
        }
        if let Some(entities) = normalized.get("entities").and_then(Value::as_array) {
            for entity in entities {
                if let Some(class_label) = entity
                    .get("class_label")
                    .or_else(|| entity.get("kind"))
                    .and_then(Value::as_str)
                {
                    increment(&mut self.observed_detection_classes, class_label);
                }
            }
        }
        if let Some(hints) = normalized.get("profile_authoring") {
            self.record_hints(hints);
        }
    }

    fn record_process_name(&mut self, value: &str) {
        let process_name = value.trim();
        if !process_name.is_empty() {
            increment(&mut self.observed_process_names, process_name);
        }
    }

    fn record_backend(&mut self, value: &str) {
        let backend = value.trim().to_ascii_lowercase();
        if backend.is_empty() {
            return;
        }
        if is_unsafe_backend(&backend) {
            self.unsafe_reasons.push(format!(
                "backend default {backend:?} requires explicit operator policy"
            ));
        }
        increment(&mut self.observed_backends, &backend);
    }

    fn record_hints(&mut self, hints: &Value) {
        self.record_match_hints(hints);
        self.record_hud_hints(hints);
        self.record_keymap_hints(hints);
        self.record_backend_hints(hints);
        self.record_detection_hints(hints);
        self.record_reflex_hints(hints);
        self.record_scope_hint(hints);
        self.record_metadata_hints(hints);
    }

    fn record_match_hints(&mut self, hints: &Value) {
        for exe in string_array(hints.pointer("/matches/exe")) {
            self.match_hints.insert(exe);
        }
    }

    fn record_hud_hints(&mut self, hints: &Value) {
        let Some(fields) = hints.get("hud_fields").and_then(Value::as_array) else {
            return;
        };
        for field in fields {
            let Some(name) = field.get("name").and_then(Value::as_str) else {
                continue;
            };
            if let Some(existing) = self.hud_hints.get(name) {
                if existing != field {
                    self.conflict_reasons
                        .push(format!("hud field {name:?} has conflicting definitions"));
                }
            } else {
                self.hud_hints.insert(name.to_owned(), field.clone());
            }
        }
    }

    fn record_keymap_hints(&mut self, hints: &Value) {
        let Some(map) = hints.get("keymap").and_then(Value::as_object) else {
            return;
        };
        for (action, key) in map {
            let Some(key) = key
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            if let Some(existing) = self.keymap_hints.get(action) {
                if existing != key {
                    self.conflict_reasons.push(format!(
                        "keymap action {action:?} has conflicting keys {existing:?} and {key:?}"
                    ));
                }
            } else {
                self.keymap_hints.insert(action.clone(), key.to_owned());
            }
        }
    }

    fn record_backend_hints(&mut self, hints: &Value) {
        let Some(map) = hints.get("backend_defaults").and_then(Value::as_object) else {
            return;
        };
        for (field, backend) in map {
            let Some(backend) = backend
                .as_str()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            if is_unsafe_backend(&backend) {
                self.unsafe_reasons.push(format!(
                    "backend default {field}={backend:?} requires hardware/driver escalation"
                ));
                continue;
            }
            if let Some(existing) = self.backend_hints.get(field) {
                if existing != &backend {
                    self.conflict_reasons.push(format!(
                        "backend field {field:?} has conflicting values {existing:?} and {backend:?}"
                    ));
                }
            } else {
                self.backend_hints.insert(field.clone(), backend);
            }
        }
    }

    fn record_detection_hints(&mut self, hints: &Value) {
        for class_label in string_array(hints.get("detection_classes")) {
            self.detection_hints.insert(class_label);
        }
    }

    fn record_reflex_hints(&mut self, hints: &Value) {
        let Some(combos) = hints.get("reflex_combos").and_then(Value::as_array) else {
            return;
        };
        self.reflex_hints.extend(combos.iter().cloned());
    }

    fn record_scope_hint(&mut self, hints: &Value) {
        let Some(scope) = hints
            .get("use_scope")
            .and_then(Value::as_str)
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
        else {
            return;
        };
        if is_unsafe_scope_change(self.profile_use_scope, &scope) {
            self.unsafe_reasons.push(format!(
                "use_scope change to {scope:?} is an unsafe escalation"
            ));
            return;
        }
        if let Some(existing) = &self.use_scope_hint {
            if existing != &scope {
                self.conflict_reasons.push(format!(
                    "use_scope has conflicting values {existing:?} and {scope:?}"
                ));
            }
        } else {
            self.use_scope_hint = Some(scope);
        }
    }

    fn record_metadata_hints(&mut self, hints: &Value) {
        let Some(map) = hints.get("metadata").and_then(Value::as_object) else {
            return;
        };
        for (key, value) in map {
            let Some(value) = value.as_str().map(str::trim) else {
                continue;
            };
            if metadata_escalates(key, value) {
                self.unsafe_reasons.push(format!(
                    "metadata {key}={value:?} would escalate supported-use permissions"
                ));
                continue;
            }
            if let Some(existing) = self.metadata_hints.get(key) {
                if existing != value {
                    self.conflict_reasons.push(format!(
                        "metadata {key:?} has conflicting values {existing:?} and {value:?}"
                    ));
                }
            } else {
                self.metadata_hints.insert(key.clone(), value.to_owned());
            }
        }
    }

    fn finish(
        self,
        replay_path: Option<String>,
        _generated_at_ns: u64,
    ) -> Result<BuiltCandidate, ErrorData> {
        if self.audit_rows_relevant == 0 && self.replay_rows_relevant == 0 {
            return Err(authoring_error(
                error_codes::PROFILE_AUTHORING_INSUFFICIENT_EVIDENCE,
                "profile authoring found no relevant audit or replay evidence",
                json!({
                    "profile_id": self.profile_id,
                    "audit_rows_scanned": self.audit_rows_scanned,
                    "replay_rows_scanned": self.replay_rows_scanned,
                }),
            ));
        }
        if !self.conflict_reasons.is_empty() {
            return Err(authoring_error(
                error_codes::PROFILE_AUTHORING_CONFLICTING_EVIDENCE,
                "profile authoring evidence contains conflicting profile changes",
                json!({
                    "profile_id": self.profile_id,
                    "conflicts": self.conflict_reasons,
                }),
            ));
        }
        if !self.unsafe_reasons.is_empty() {
            return Err(authoring_error(
                error_codes::PROFILE_AUTHORING_UNSAFE_ESCALATION,
                "profile authoring evidence requests unsafe permission escalation",
                json!({
                    "profile_id": self.profile_id,
                    "unsafe_reasons": self.unsafe_reasons,
                }),
            ));
        }

        let patch = self.profile_patch();
        if patch_is_empty(&patch) {
            return Err(authoring_error(
                error_codes::PROFILE_AUTHORING_INSUFFICIENT_EVIDENCE,
                "profile authoring evidence produced no profile patch",
                json!({
                    "profile_id": self.profile_id,
                    "audit_rows_relevant": self.audit_rows_relevant,
                    "replay_rows_relevant": self.replay_rows_relevant,
                }),
            ));
        }
        let evidence_summary = self.evidence_summary();
        let expected_improvement = expected_improvement(&patch);
        let evidence_hash = evidence_hash(
            &self.profile_id,
            &self.source_row_keys,
            &self.source_audit_ids,
            &patch,
            &evidence_summary,
        )?;
        Ok(BuiltCandidate {
            source: ProfileAuthoringSource {
                audit_cf_name: cf::CF_ACTION_LOG.to_owned(),
                profile_cf_name: cf::CF_PROFILES.to_owned(),
                replay_path,
                audit_rows_scanned: self.audit_rows_scanned,
                audit_rows_relevant: self.audit_rows_relevant,
                replay_rows_scanned: self.replay_rows_scanned,
                replay_rows_relevant: self.replay_rows_relevant,
                source_row_keys: self.source_row_keys,
                source_audit_ids: self.source_audit_ids,
                evidence_hash,
            },
            evidence_summary,
            expected_improvement,
            patch,
        })
    }

    fn profile_patch(&self) -> Value {
        json!({
            "schema_version": AUTHORING_SCHEMA_VERSION,
            "profile_id": self.profile_id,
            "base_profile_schema_version": self.profile_schema_version,
            "matches": {
                "add_exe": self.patch_exes(),
            },
            "hud_fields": {
                "add": self.patch_hud_fields(),
            },
            "keymap": {
                "set": self.keymap_hints,
            },
            "backends": {
                "set": self.patch_backends(),
            },
            "detection": {
                "add_classes": self.patch_detection_classes(),
            },
            "reflex_combos": {
                "propose": self.reflex_hints,
            },
            "safety": {
                "use_scope": self.use_scope_hint,
                "metadata": self.metadata_hints,
            },
        })
    }

    fn patch_exes(&self) -> Vec<String> {
        self.observed_process_names
            .keys()
            .chain(self.match_hints.iter())
            .filter(|exe| !self.existing_exes.contains(&normalize_key(exe)))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn patch_hud_fields(&self) -> Vec<Value> {
        let mut fields = Vec::new();
        for (name, count) in &self.observed_hud_fields {
            if !self.existing_hud_names.contains(name) {
                fields.push(json!({
                    "name": name,
                    "source": "replay_hud_by_name",
                    "evidence_count": count,
                }));
            }
        }
        for (name, value) in &self.hud_hints {
            if !self.existing_hud_names.contains(name) {
                fields.push(value.clone());
            }
        }
        fields
    }

    fn patch_detection_classes(&self) -> Vec<String> {
        self.observed_detection_classes
            .keys()
            .chain(self.detection_hints.iter())
            .filter(|class_label| !self.existing_detection_classes.contains(*class_label))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn patch_backends(&self) -> BTreeMap<String, String> {
        self.backend_hints
            .iter()
            .filter(|(field, value)| self.existing_backends.get(*field) != Some(*value))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect()
    }

    fn evidence_summary(&self) -> Value {
        json!({
            "audit_rows_scanned": self.audit_rows_scanned,
            "audit_rows_relevant": self.audit_rows_relevant,
            "replay_rows_scanned": self.replay_rows_scanned,
            "replay_rows_relevant": self.replay_rows_relevant,
            "observed_process_names": self.observed_process_names,
            "observed_backends": self.observed_backends,
            "observed_hud_fields": self.observed_hud_fields,
            "observed_detection_classes": self.observed_detection_classes,
            "tool_counts": self.tool_counts,
            "hint_counts": {
                "matches": self.match_hints.len(),
                "hud_fields": self.hud_hints.len(),
                "keymap": self.keymap_hints.len(),
                "backend_defaults": self.backend_hints.len(),
                "detection_classes": self.detection_hints.len(),
                "reflex_combos": self.reflex_hints.len(),
                "metadata": self.metadata_hints.len(),
            },
        })
    }
}

const fn default_max_audit_rows() -> u32 {
    DEFAULT_MAX_AUDIT_ROWS
}

const fn default_max_replay_rows() -> u32 {
    DEFAULT_MAX_REPLAY_ROWS
}

const fn default_list_limit() -> u32 {
    DEFAULT_LIST_LIMIT
}

fn validate_generation_limits(max_audit_rows: u32, max_replay_rows: u32) -> Result<(), ErrorData> {
    if max_audit_rows > MAX_AUTHORING_ROWS || max_replay_rows > MAX_AUTHORING_ROWS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "profile_authoring_generate row limits must be <= {MAX_AUTHORING_ROWS}; got audit={max_audit_rows} replay={max_replay_rows}"
            ),
        ));
    }
    Ok(())
}

fn validate_list_limit(limit: u32) -> Result<usize, ErrorData> {
    if limit == 0 || limit > 1000 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "profile_authoring_list limit must be between 1 and 1000",
        ));
    }
    Ok(limit as usize)
}

fn profile_status(runtime: &ProfileRuntime, profile_id: &str) -> Result<ProfileStatus, ErrorData> {
    runtime
        .list(true)
        .map_err(|error| profile_error(&error))?
        .into_iter()
        .find(|profile| profile.id == profile_id)
        .ok_or_else(|| {
            mcp_error(
                error_codes::PROFILE_NOT_FOUND,
                format!("profile {profile_id} was not found"),
            )
        })
}

fn audit_rows(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    max_audit_rows: u32,
) -> Result<RawStorageRows, ErrorData> {
    if max_audit_rows == 0 {
        return Ok(Vec::new());
    }
    let runtime = lock_runtime(reflex_runtime, "reading profile authoring audit evidence")?;
    runtime
        .storage_cf_tail_rows(cf::CF_ACTION_LOG, max_audit_rows as usize)
        .map_err(|error| mcp_error(error.code(), error.to_string()))
}

fn replay_records(path: Option<&str>, max_replay_rows: u32) -> Result<ReplayRecords, ErrorData> {
    let Some(path) = path else {
        return Ok(ReplayRecords {
            path: None,
            records: Vec::new(),
        });
    };
    if max_replay_rows == 0 {
        return Ok(ReplayRecords {
            path: Some(path.to_owned()),
            records: Vec::new(),
        });
    }
    let normalized = normalize_replay_path(&replay_root(), Some(path))?;
    let text = fs::read_to_string(&normalized).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "profile_authoring_generate could not read replay {}: {error}",
                normalized.display()
            ),
        )
    })?;
    let mut records = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if records.len() >= max_replay_rows as usize {
            break;
        }
        let record = serde_json::from_str::<Value>(line).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "profile_authoring_generate could not parse replay line in {}: {error}",
                    normalized.display()
                ),
            )
        })?;
        records.push(record);
    }
    Ok(ReplayRecords {
        path: Some(normalized.display().to_string()),
        records,
    })
}

fn row_matches_profile(row: &Value, profile_id: &str) -> bool {
    [
        row.get("profile_id"),
        row.get("active_profile_id"),
        row.pointer("/foreground/profile_id"),
        row.pointer("/audit_context/profile_id"),
        row.pointer("/app_context/profile_id"),
        row.pointer("/record/foreground/profile_id"),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.as_str() == Some(profile_id))
}

fn replay_payload(record: &Value) -> Value {
    record
        .get("record")
        .cloned()
        .unwrap_or_else(|| record.clone())
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect(),
        Some(Value::String(value)) if !value.trim().is_empty() => vec![value.trim().to_owned()],
        _ => Vec::new(),
    }
}

fn optional_string(row: &Value, field: &str) -> Option<String> {
    row.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn increment(counts: &mut BTreeMap<String, u64>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        *counts.entry(value.to_owned()).or_default() += 1;
    }
}

fn backends_map(profile: &ProfileStatus) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "default".to_owned(),
            backend_name(profile.backends.default).to_owned(),
        ),
        (
            "keyboard_default".to_owned(),
            backend_name(profile.backends.keyboard_default).to_owned(),
        ),
        (
            "mouse_default".to_owned(),
            backend_name(profile.backends.mouse_default).to_owned(),
        ),
        (
            "pad_default".to_owned(),
            backend_name(profile.backends.pad_default).to_owned(),
        ),
    ])
}

const fn backend_name(value: Backend) -> &'static str {
    match value {
        Backend::Auto => "auto",
        Backend::Software => "software",
        Backend::Vigem => "vigem",
        Backend::Hardware => "hardware",
    }
}

fn is_unsafe_backend(value: &str) -> bool {
    matches!(value, "hardware" | "vigem")
}

fn is_unsafe_scope_change(current: ProfileUseScope, requested: &str) -> bool {
    matches!(requested, "sanctioned_research")
        || (matches!(
            current,
            ProfileUseScope::Unknown | ProfileUseScope::OperatorOwnedTest
        ) && matches!(requested, "productivity"))
}

fn metadata_escalates(key: &str, value: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    value == "true"
        && (key.contains("remote_server_allowed")
            || key.contains("allow_shell")
            || key.contains("hardware_hid")
            || key.contains("input_hardware"))
}

fn patch_is_empty(patch: &Value) -> bool {
    let empty_array = Value::Array(Vec::new());
    let empty_object = Value::Object(Map::new());
    patch.pointer("/matches/add_exe") == Some(&empty_array)
        && patch.pointer("/hud_fields/add") == Some(&empty_array)
        && patch.pointer("/keymap/set") == Some(&empty_object)
        && patch.pointer("/backends/set") == Some(&empty_object)
        && patch.pointer("/detection/add_classes") == Some(&empty_array)
        && patch.pointer("/reflex_combos/propose") == Some(&empty_array)
        && patch
            .pointer("/safety/use_scope")
            .is_some_and(Value::is_null)
        && patch.pointer("/safety/metadata") == Some(&empty_object)
}

fn expected_improvement(patch: &Value) -> Vec<String> {
    let mut improvements = Vec::new();
    if let Some(values) = patch.pointer("/matches/add_exe").and_then(Value::as_array)
        && !values.is_empty()
    {
        improvements
            .push("improve foreground profile matching from observed process evidence".to_owned());
    }
    if let Some(values) = patch.pointer("/hud_fields/add").and_then(Value::as_array)
        && !values.is_empty()
    {
        improvements.push("add HUD extraction targets observed in replay evidence".to_owned());
    }
    if let Some(values) = patch.pointer("/keymap/set").and_then(Value::as_object)
        && !values.is_empty()
    {
        improvements.push("fill keymap actions from explicit replay/audit hints".to_owned());
    }
    if let Some(values) = patch.pointer("/backends/set").and_then(Value::as_object)
        && !values.is_empty()
    {
        improvements.push("align backend defaults with successful local evidence".to_owned());
    }
    if let Some(values) = patch
        .pointer("/detection/add_classes")
        .and_then(Value::as_array)
        && !values.is_empty()
    {
        improvements.push("add detection classes observed in replay entities".to_owned());
    }
    if let Some(values) = patch
        .pointer("/reflex_combos/propose")
        .and_then(Value::as_array)
        && !values.is_empty()
    {
        improvements
            .push("propose reflex combo candidates from explicit evidence hints".to_owned());
    }
    if patch
        .pointer("/safety/use_scope")
        .is_some_and(|value| !value.is_null())
        || patch
            .pointer("/safety/metadata")
            .and_then(Value::as_object)
            .is_some_and(|values| !values.is_empty())
    {
        improvements.push("capture safety/use-scope metadata as local candidate state".to_owned());
    }
    improvements
}

fn evidence_hash(
    profile_id: &str,
    row_keys: &[String],
    audit_ids: &[String],
    patch: &Value,
    evidence_summary: &Value,
) -> Result<String, ErrorData> {
    let mut hasher = Sha256::new();
    hasher.update(profile_id.as_bytes());
    for key in row_keys {
        hasher.update(key.as_bytes());
        hasher.update([0]);
    }
    for audit_id in audit_ids {
        hasher.update(audit_id.as_bytes());
        hasher.update([0]);
    }
    let patch_bytes = serde_json::to_vec(patch).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("profile authoring could not encode patch for evidence hash: {error}"),
        )
    })?;
    let summary_bytes = serde_json::to_vec(evidence_summary).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "profile authoring could not encode evidence summary for evidence hash: {error}"
            ),
        )
    })?;
    hasher.update(patch_bytes);
    hasher.update(summary_bytes);
    Ok(format!("sha256:{}", hex_encode(&hasher.finalize())))
}

fn generated_candidate_id(profile_id: &str, evidence_hash: &str) -> String {
    let suffix = evidence_hash
        .trim_start_matches("sha256:")
        .chars()
        .take(CANDIDATE_ID_HASH_CHARS)
        .collect::<String>();
    format!("{profile_id}.{suffix}")
}

fn normalized_candidate_id(candidate_id: &str) -> Result<String, ErrorData> {
    let candidate_id = candidate_id.trim();
    if candidate_id.is_empty()
        || candidate_id.len() > 160
        || !candidate_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "profile authoring candidate_id must be 1..=160 ASCII letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(candidate_id.to_owned())
}

fn normalized_state(state: &str) -> Result<String, ErrorData> {
    let state = state.trim().to_ascii_lowercase();
    if matches!(state.as_str(), "candidate" | "accepted" | "rejected") {
        Ok(state)
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "profile authoring state must be candidate, accepted, or rejected",
        ))
    }
}

fn candidate_key(candidate_id: &str) -> String {
    format!("{CANDIDATE_PREFIX}{candidate_id}")
}

fn read_candidate_required(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    row_key: &str,
) -> Result<(ProfileAuthoringCandidate, String), ErrorData> {
    let runtime = lock_runtime(reflex_runtime, "reading profile authoring candidate")?;
    let row = runtime
        .storage_profile_row(row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    drop(runtime);
    let Some(row) = row else {
        return Err(authoring_error(
            error_codes::PROFILE_AUTHORING_CANDIDATE_NOT_FOUND,
            format!("profile authoring candidate row {row_key} was not found"),
            json!({ "row_key": row_key }),
        ));
    };
    let candidate = decode_candidate(&row)?;
    let previous_state = candidate.state.clone();
    Ok((candidate, previous_state))
}

fn write_candidate(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    row_key: &str,
    candidate: &ProfileAuthoringCandidate,
) -> Result<(), ErrorData> {
    let encoded = encode_candidate(candidate)?;
    let runtime = lock_runtime(reflex_runtime, "writing profile authoring candidate state")?;
    runtime
        .storage_put_profile_rows(vec![(row_key.as_bytes().to_vec(), encoded)])
        .map_err(|error| mcp_error(error.code(), error.to_string()))
}

fn encode_candidate(candidate: &ProfileAuthoringCandidate) -> Result<Vec<u8>, ErrorData> {
    encode_json(candidate).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("profile authoring candidate encode failed: {error}"),
        )
    })
}

fn decode_candidate(value: &[u8]) -> Result<ProfileAuthoringCandidate, ErrorData> {
    decode_json::<ProfileAuthoringCandidate>(value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("profile authoring candidate decode failed: {error}"),
        )
    })
}

fn candidate_summary(
    row_key: &str,
    candidate: &ProfileAuthoringCandidate,
    value_len: usize,
) -> ProfileAuthoringCandidateSummary {
    ProfileAuthoringCandidateSummary {
        cf_name: cf::CF_PROFILES.to_owned(),
        row_key: row_key.to_owned(),
        candidate_id: candidate.candidate_id.clone(),
        profile_id: candidate.profile_id.clone(),
        state: candidate.state.clone(),
        generated_at_ns: candidate.generated_at_ns,
        updated_at_ns: candidate.updated_at_ns,
        evidence_hash: candidate.source.evidence_hash.clone(),
        expected_improvement: candidate.expected_improvement.clone(),
        value_len_bytes: value_len as u64,
    }
}

fn profile_error(error: &ProfileError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

fn authoring_error(code: &'static str, message: impl Into<String>, data: Value) -> ErrorData {
    let mut payload = Map::new();
    payload.insert("code".to_owned(), Value::String(code.to_owned()));
    payload.insert(
        "row_prefix".to_owned(),
        Value::String(AUTHORING_PREFIX.to_owned()),
    );
    payload.insert("details".to_owned(), data);
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(Value::Object(payload)),
    )
}

fn lock_runtime<'a>(
    runtime: &'a Arc<Mutex<ReflexRuntime>>,
    operation: &str,
) -> Result<MutexGuard<'a, ReflexRuntime>, ErrorData> {
    runtime.lock().map_err(|_error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("reflex runtime lock poisoned while {operation}"),
        )
    })
}

fn normalize_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use synapse_core::{
        Backend, PerceptionMode, ProfileBackends, ProfileUseScope, SCHEMA_VERSION,
        types::{RoutineDowClass, RoutineGranularity, RoutineStep},
    };
    use synapse_storage::Db;

    fn temp_db() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Db::open(dir.path(), SCHEMA_VERSION).expect("open db"));
        (dir, db)
    }

    fn test_profile_status(profile_id: &str) -> ProfileStatus {
        ProfileStatus {
            id: profile_id.to_owned(),
            label: "Test profile".to_owned(),
            use_scope: ProfileUseScope::OperatorOwnedTest,
            mode: PerceptionMode::A11yOnly,
            detection_model_id: None,
            detection_classes: Vec::new(),
            hud_fields: Vec::new(),
            keymap_actions: Vec::new(),
            backends: ProfileBackends {
                default: Backend::Auto,
                keyboard_default: Backend::Auto,
                mouse_default: Backend::Auto,
                pad_default: Backend::Auto,
            },
            event_extensions: Vec::new(),
            active: false,
            schema_version: 2,
            matches: Vec::new(),
            metadata: BTreeMap::new(),
            source_path: PathBuf::from("notepad.toml"),
        }
    }

    fn routine_step(app: &str, document: Option<&str>) -> RoutineStep {
        RoutineStep {
            app: app.to_owned(),
            document: document.map(str::to_owned),
        }
    }

    fn test_routine() -> RoutineRecord {
        RoutineRecord {
            record_version: 1,
            ts_ns: 100,
            routine_id: "rt1-0123456789abcdef".to_owned(),
            granularity: RoutineGranularity::AppDocument,
            steps: vec![
                routine_step("chrome.exe", Some("mail.google.com")),
                routine_step("excel.exe", Some("q3 report")),
            ],
            dow_class: RoutineDowClass::Weekdays,
            mean_minute_of_day: 525,
            tolerance_minutes: 20,
            schedule_label: "weekdays 08:45+/-20m".to_owned(),
            support_days: 5,
            occurrence_count: 7,
            opportunity_days: 8,
            confidence: 0.61,
            window_start_ns: 0,
            window_end_ns: 10,
            active_days_in_window: 8,
            first_seen_day_start_ns: 1,
            last_seen_day_start_ns: 9,
            evidence: vec![synapse_core::types::RoutineEvidence {
                day_start_ns: 1,
                minute_of_day: 525,
                episode_ids: vec!["ep1-alpha".to_owned(), "ep1-beta".to_owned()],
            }],
        }
    }

    fn params(decision: ProfileAuthoringDecision) -> ProfileAuthoringDecideParams {
        ProfileAuthoringDecideParams {
            candidate_id: "candidate.alpha".to_owned(),
            decision,
            operator_note: None,
            reason: None,
        }
    }

    fn error_code(error: &ErrorData) -> Option<&str> {
        error.data.as_ref()?.get("code")?.as_str()
    }

    fn invalid_field(error: &ErrorData) -> Option<&str> {
        error
            .data
            .as_ref()?
            .get("details")?
            .get("invalid_field")?
            .as_str()
    }

    #[test]
    fn decide_accept_allows_operator_note_only() {
        let mut params = params(ProfileAuthoringDecision::Accept);
        params.operator_note = Some("reviewed by operator".to_owned());

        validate_decide_fields(&params).expect("operator note is valid for accept");

        params.reason = Some("not relevant".to_owned());
        let error = validate_decide_fields(&params)
            .expect_err("reason must not be accepted with decision=accept");
        assert_eq!(error_code(&error), Some(error_codes::TOOL_PARAMS_INVALID));
        assert_eq!(invalid_field(&error), Some("reason"));
    }

    #[test]
    fn decide_reject_allows_reason_only() {
        let mut params = params(ProfileAuthoringDecision::Reject);
        params.reason = Some("insufficient evidence".to_owned());

        validate_decide_fields(&params).expect("reason is valid for reject");

        params.operator_note = Some("reviewed by operator".to_owned());
        let error = validate_decide_fields(&params)
            .expect_err("operator_note must not be accepted with decision=reject");
        assert_eq!(error_code(&error), Some(error_codes::TOOL_PARAMS_INVALID));
        assert_eq!(invalid_field(&error), Some("operator_note"));
    }

    #[test]
    fn demo_record_replay_rows_generate_authoring_patch() {
        let profile = test_profile_status("notepad");
        let mut builder = EvidenceBuilder::new(&profile);
        builder.record_replay_record(&json!({
            "record": {
                "type": "demo_record",
                "demo_id": "demo.notepad.test",
                "profile_id": "notepad",
                "foreground": {
                    "profile_id": "notepad",
                    "process_name": "notepad.exe"
                },
                "uia": {
                    "event_kind": "value_changed",
                    "element_id": "cdcd:42:0000002a",
                    "name": "Document",
                    "value": "hello",
                    "previous_value": ""
                },
                "profile_authoring": {
                    "matches": {
                        "exe": ["notepad.exe"]
                    },
                    "metadata": {
                        "demo_recording": "true",
                        "demo_id": "demo.notepad.test"
                    }
                }
            }
        }));

        let built = builder
            .finish(Some("demo-recordings/notepad.jsonl".to_owned()), 123)
            .unwrap_or_else(|error| panic!("demo replay evidence should build a patch: {error:?}"));

        println!(
            "readback=profile_authoring source=demo_record replay_rows_relevant={} patch={}",
            built.source.replay_rows_relevant, built.patch
        );
        assert_eq!(built.source.replay_rows_scanned, 1);
        assert_eq!(built.source.replay_rows_relevant, 1);
        assert_eq!(built.patch["matches"]["add_exe"], json!(["notepad.exe"]));
        assert_eq!(
            built.patch["safety"]["metadata"]["demo_recording"],
            json!("true")
        );
    }

    #[test]
    fn routine_automation_patch_packages_plan_and_agent_task_steps() {
        let profile = test_profile_status("excel");
        let routine = test_routine();
        let plan = crate::m3::plan::compile_plan(&routine, 123);
        let patch = routine_automation_patch(
            &profile,
            &routine,
            &plan,
            "routine-auto.rt1-0123456789abcdef.feedfacefeedface",
            "plan/v1/rt1-0123456789abcdef",
        );

        println!("readback=routine_automate_patch patch={patch}");
        assert_eq!(patch["kind"], "routine_automation_candidate");
        assert_eq!(
            patch["routine_automation"]["routine_id"],
            "rt1-0123456789abcdef"
        );
        assert_eq!(
            patch["routine_automation"]["compiled_plan"]["agent_task_steps"],
            1
        );
        assert!(
            patch["routine_automation"]["task"]["instructions"]
                .as_array()
                .expect("instructions")
                .iter()
                .any(|value| value
                    .as_str()
                    .is_some_and(|line| line.contains("judgment-required")))
        );
    }

    #[test]
    fn routine_automation_decision_updates_durable_status_row() {
        let (_dir, db) = temp_db();
        let routine = test_routine();
        let plan = crate::m3::plan::compile_plan(&routine, 123);
        let profile = test_profile_status("excel");
        let candidate_id = "routine-auto.rt1-0123456789abcdef.feedfacefeedface";
        let row_key = candidate_key(candidate_id);
        let record = RoutineAutomationRecord {
            schema_version: AUTHORING_SCHEMA_VERSION,
            row_kind: "routine_automation".to_owned(),
            routine_id: routine.routine_id.clone(),
            profile_id: profile.id.clone(),
            candidate_id: candidate_id.to_owned(),
            candidate_row_key: row_key.clone(),
            plan_ref: "plan/v1/rt1-0123456789abcdef".to_owned(),
            state: "candidate".to_owned(),
            generated_at_ns: 123,
            updated_at_ns: 123,
            installed_at_ns: None,
            rejected_at_ns: None,
            plan_fully_deterministic: plan.fully_deterministic,
            total_steps: plan.total_steps,
            deterministic_steps: plan.deterministic_steps,
            agent_task_steps: plan.agent_task_steps,
        };
        write_routine_automation_record(&db, &record).expect("write automation row");
        let candidate = ProfileAuthoringCandidate {
            schema_version: AUTHORING_SCHEMA_VERSION,
            row_kind: "profile_authoring_candidate".to_owned(),
            candidate_id: candidate_id.to_owned(),
            profile_id: profile.id,
            state: "accepted".to_owned(),
            generated_at_ns: 123,
            updated_at_ns: 456,
            accepted_at_ns: Some(456),
            rejected_at_ns: None,
            operator_note: None,
            rejection_reason: None,
            source: ProfileAuthoringSource {
                audit_cf_name: cf::CF_ROUTINES.to_owned(),
                profile_cf_name: cf::CF_PROFILES.to_owned(),
                replay_path: None,
                audit_rows_scanned: 1,
                audit_rows_relevant: 1,
                replay_rows_scanned: 0,
                replay_rows_relevant: 0,
                source_row_keys: vec![],
                source_audit_ids: vec![],
                evidence_hash: "sha256:feedfacefeedface".to_owned(),
            },
            evidence_summary: json!({}),
            expected_improvement: vec![],
            patch: routine_automation_patch(
                &test_profile_status("excel"),
                &routine,
                &plan,
                candidate_id,
                "plan/v1/rt1-0123456789abcdef",
            ),
            safety_review: ProfileAuthoringSafetyReview {
                activation: ProfileAuthoringActivationReview {
                    required: true,
                    active_on_accept: false,
                },
                contribution: ProfileAuthoringContributionReview {
                    registry_allowed: false,
                    external_sharing_allowed: false,
                },
                unsafe_permission_escalation: false,
                rejected_reasons: vec![],
            },
        };

        maybe_mark_routine_automation_decision(&db, &candidate, "installed", Some(456))
            .expect("mark installed");
        let readback = load_routine_automation_record(&db, &routine.routine_id)
            .expect("load automation")
            .expect("automation row");
        println!(
            "readback=routine_automation state={} candidate_id={} routine_id={}",
            readback.state, readback.candidate_id, readback.routine_id
        );
        assert_eq!(readback.state, "installed");
        assert_eq!(readback.installed_at_ns, Some(456));
        assert_eq!(readback.candidate_id, candidate_id);
    }
}

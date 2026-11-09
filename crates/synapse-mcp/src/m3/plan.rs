//! Routine → setup-plan compiler (#859, epic #832/#828).
//!
//! Turns a mined routine template into an inspectable, executable SETUP plan:
//! each [`RoutineStep`] compiles to a [`PlanStep`] carrying the action backend
//! (`act_launch` for apps, `cdp_open_tab` for background browser tabs, shell
//! association for documents) and an explicit POSTCONDITION the executor (#860)
//! must verify against the physical SoT before moving on — the no-silent-success
//! doctrine. Steps that need judgment (a non-browser app remembered only by a
//! window title, where the concrete document path is unknown) compile to an
//! `agent_task` stub instead of being silently dropped.
//!
//! This module owns only COMPILATION (pure) plus persistence of the plan
//! document in `CF_KV` (`plan/v1/<routine_id>`). Execution + postcondition
//! verification is #860.

use std::sync::Arc;

use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use synapse_core::error_codes;
use synapse_core::types::{RoutineGranularity, RoutineRecord, RoutineStep};
use synapse_storage::{Db, cf, decode_json, encode_json};

use crate::m1::mcp_error;

use super::episodes::now_ts_ns;
use super::permissions::{Permission, RequiredPermissions, required};
use super::routines::{load_routine_record, validate_routine_id_param};

const PLAN_PREFIX: &str = "plan/v1/";
const PLAN_RECORD_VERSION: u32 = 1;

/// Process names treated as browsers (open documents as background tabs/URLs).
const BROWSER_APPS: [&str; 6] = [
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
];

/// The action backend the executor (#860) will drive for a step.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanBackend {
    /// Launch an application without stealing the foreground (`act_launch`).
    ActLaunch,
    /// Open a URL in a background browser tab (`cdp_open_tab`).
    CdpOpenTab,
    /// Open a document via its shell association.
    ShellOpen,
    /// Hand off to a scoped agent task (judgment required).
    AgentTask,
}

/// The postcondition the executor verifies against the physical SoT before the
/// next step runs. Failure aborts the plan with a precise reason (#860).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Postcondition {
    /// A top-level window owned by a process of this name exists.
    WindowForProcessExists { process: String },
    /// A browser tab whose URL host matches exists.
    BrowserTabAtHost { host: String },
    /// A window for `app` showing `document` is open.
    DocumentWindowOpen { app: String, document: String },
    /// The handed-off agent task reported success with its own evidence.
    AgentReported,
}

/// One compiled step.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub index: u32,
    /// The routine template step this came from.
    pub source_app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_document: Option<String>,
    pub backend: PlanBackend,
    /// Whether the step compiled to a deterministic action (`true`) or degraded
    /// to an `agent_task` stub (`false`). Never silently dropped either way.
    pub deterministic: bool,
    /// Human/agent-readable action description.
    pub action: String,
    pub postcondition: Postcondition,
    /// Why this step degraded to an agent task (only set when not deterministic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_task_reason: Option<String>,
}

/// The full compiled plan, stored with the routine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanDocument {
    pub record_version: u32,
    pub routine_id: String,
    pub compiled_ts_ns: u64,
    pub granularity: RoutineGranularity,
    pub schedule_label: String,
    pub total_steps: u32,
    pub deterministic_steps: u32,
    pub agent_task_steps: u32,
    /// True when every step compiled deterministically (no judgment needed).
    pub fully_deterministic: bool,
    pub steps: Vec<PlanStep>,
}

fn is_browser(app: &str) -> bool {
    BROWSER_APPS.contains(&app)
}

/// True when a browser document identity looks like a URL host we can open
/// (mined browser docs are normalized URL hosts, e.g. `mail.google.com`).
fn looks_like_host(document: &str) -> bool {
    let doc = document.trim();
    !doc.is_empty()
        && !doc.contains(' ')
        && doc.contains('.')
        && doc
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '/'))
}

/// Compiles one routine step. Pure — the heart of the compiler, unit-tested
/// over synthetic routines.
#[must_use]
pub fn compile_step(index: u32, step: &RoutineStep) -> PlanStep {
    let app = step.app.clone();
    match &step.document {
        // App-granularity (or no remembered document): just launch the app.
        None => PlanStep {
            index,
            source_app: app.clone(),
            source_document: None,
            backend: PlanBackend::ActLaunch,
            deterministic: true,
            action: format!("launch {app} (no-activate)"),
            postcondition: Postcondition::WindowForProcessExists { process: app },
            agent_task_reason: None,
        },
        Some(document) if is_browser(&app) && looks_like_host(document) => {
            let host = document.trim().to_owned();
            PlanStep {
                index,
                source_app: app,
                source_document: Some(document.clone()),
                backend: PlanBackend::CdpOpenTab,
                deterministic: true,
                action: format!("open https://{host} in a background tab"),
                postcondition: Postcondition::BrowserTabAtHost { host },
                agent_task_reason: None,
            }
        }
        // Browser step whose document is not a clean host (a page title, say):
        // we cannot deterministically reconstruct the URL — hand to an agent.
        Some(document) if is_browser(&app) => PlanStep {
            index,
            source_app: app,
            source_document: Some(document.clone()),
            backend: PlanBackend::AgentTask,
            deterministic: false,
            action: format!("open the browser page titled {document:?}"),
            postcondition: Postcondition::AgentReported,
            agent_task_reason: Some(
                "browser document is a page title, not a resolvable URL host".to_owned(),
            ),
        },
        // Non-browser app + document: mined documents here are normalized window
        // titles, not file paths, so the concrete document to open requires
        // judgment. Degrade to an agent-task stub (never dropped).
        Some(document) => PlanStep {
            index,
            source_app: app.clone(),
            source_document: Some(document.clone()),
            backend: PlanBackend::AgentTask,
            deterministic: false,
            action: format!("open {document:?} in {app}"),
            postcondition: Postcondition::DocumentWindowOpen {
                app,
                document: document.clone(),
            },
            agent_task_reason: Some(
                "document identity is a normalized window title; the concrete file path is \
                 unknown and needs an agent to locate/open it"
                    .to_owned(),
            ),
        },
    }
}

/// Compiles a whole routine into a setup plan (pure).
#[must_use]
pub fn compile_plan(routine: &RoutineRecord, now_ns: u64) -> PlanDocument {
    let steps: Vec<PlanStep> = routine
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| compile_step(u32::try_from(index).unwrap_or(u32::MAX), step))
        .collect();
    let deterministic_steps =
        u32::try_from(steps.iter().filter(|s| s.deterministic).count()).unwrap_or(u32::MAX);
    let agent_task_steps =
        u32::try_from(steps.iter().filter(|s| !s.deterministic).count()).unwrap_or(u32::MAX);
    PlanDocument {
        record_version: PLAN_RECORD_VERSION,
        routine_id: routine.routine_id.clone(),
        compiled_ts_ns: now_ns,
        granularity: routine.granularity,
        schedule_label: routine.schedule_label.clone(),
        total_steps: u32::try_from(steps.len()).unwrap_or(u32::MAX),
        deterministic_steps,
        agent_task_steps,
        fully_deterministic: agent_task_steps == 0,
        steps,
    }
}

fn plan_key(routine_id: &str) -> Vec<u8> {
    format!("{PLAN_PREFIX}{routine_id}").into_bytes()
}

fn storage_error(error: impl std::fmt::Display) -> ErrorData {
    mcp_error(
        error_codes::STORAGE_READ_FAILED,
        format!("plan storage failure: {error}"),
    )
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineCompilePlanParams {
    pub routine_id: String,
    /// Persist the plan to CF_KV (default true). false = compile-and-return only.
    #[serde(default = "default_true")]
    pub store: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineCompilePlanResponse {
    pub plan: PlanDocument,
    pub stored: bool,
}

pub fn required_permissions_compile(_params: &RoutineCompilePlanParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

pub fn compile_routine_plan(
    db: &Arc<Db>,
    params: &RoutineCompilePlanParams,
) -> Result<RoutineCompilePlanResponse, ErrorData> {
    validate_routine_id_param("routine_compile_plan", &params.routine_id)?;
    let Some(routine) = load_routine_record(db, &params.routine_id)? else {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "ROUTINE_NOT_MINED: routine_id {} is not in CF_ROUTINES; nothing to compile. Run \
                 routine_mine, or routine_list to see what exists",
                params.routine_id
            ),
        ));
    };
    let plan = compile_plan(&routine, now_ts_ns());

    let stored = if params.store {
        if !db.pressure_permits_write(cf::CF_KV) {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "routine_compile_plan refused under disk pressure: pressure_level={:?}",
                    db.pressure_level()
                ),
            ));
        }
        let key = plan_key(&params.routine_id);
        let value = encode_json(&plan).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!("failed to encode plan for {}: {error}", params.routine_id),
            )
        })?;
        db.put_batch_pressure_bypass(cf::CF_KV, [(key, value)])
            .map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!("failed to persist plan for {}: {error}", params.routine_id),
                )
            })?;
        // Read-your-write.
        let readback = load_plan(db, &params.routine_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "PLAN_READBACK_MISSING: plan for {} vanished immediately after write",
                    params.routine_id
                ),
            )
        })?;
        if readback != plan {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "PLAN_READBACK_MISMATCH: persisted plan for {} != value just written",
                    params.routine_id
                ),
            ));
        }
        true
    } else {
        false
    };

    Ok(RoutineCompilePlanResponse { plan, stored })
}

pub fn load_plan(db: &Arc<Db>, routine_id: &str) -> Result<Option<PlanDocument>, ErrorData> {
    let key = plan_key(routine_id);
    let rows = db.scan_cf_prefix(cf::CF_KV, &key).map_err(storage_error)?;
    match rows.into_iter().find(|(k, _)| k == &key) {
        Some((_, value)) => {
            let plan: PlanDocument = decode_json(&value).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("PLAN_ROW_DECODE_FAILED for {routine_id}: {error}"),
                )
            })?;
            Ok(Some(plan))
        }
        None => Ok(None),
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanGetParams {
    pub routine_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanGetResponse {
    pub found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanDocument>,
}

pub fn required_permissions_get(_params: &PlanGetParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

pub fn get_plan(db: &Arc<Db>, params: &PlanGetParams) -> Result<PlanGetResponse, ErrorData> {
    validate_routine_id_param("plan_get", &params.routine_id)?;
    let plan = load_plan(db, &params.routine_id)?;
    Ok(PlanGetResponse {
        found: plan.is_some(),
        plan,
    })
}

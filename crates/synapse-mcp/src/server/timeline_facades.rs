use super::{
    ErrorData, Json, Parameters, SynapseService, tool, tool_profiles::ToolProfileKind, tool_router,
};

use crate::m3::{
    episodes::{EpisodeGetParams, EpisodeGetResponse, EpisodeListParams, EpisodeListResponse},
    hygiene::{HygieneRedactParams, HygieneRedactResponse},
    timeline::{
        TimelineGetParams, TimelineGetResponse, TimelinePurgeParams, TimelinePurgeResponse,
        TimelineSearchParams, TimelineSearchResponse, TimelineStatsParams, TimelineStatsResponse,
    },
    timeline_control::{
        TimelineExclusionsParams, TimelineExclusionsResponse, TimelinePauseParams,
        TimelinePauseResponse, TimelineResumeParams, TimelineResumeResponse,
    },
};
use rmcp::{RoleServer, model::ErrorCode, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;

const TIMELINE_TOOL: &str = "timeline";
const EPISODE_TOOL: &str = "episode";
const PRIVACY_TOOL: &str = "privacy";
const TIMELINE_SOURCE_OF_TRUTH: &str = "CF_TIMELINE rows + live recorder control gate";
const EPISODE_SOURCE_OF_TRUTH: &str = "CF_EPISODES rows + CF_TIMELINE evidence refs";
const PRIVACY_SOURCE_OF_TRUTH: &str =
    "CF_KV timeline/control/v1 + CF_TIMELINE rows/audit rows + hygiene flag/taint rows";

/// #1516: default recent window (7 days, in ns) applied to a `timeline stats`
/// call that supplies no time bounds. An unwindowed scan starts at the OLDEST
/// row and is budget-capped, so on a large timeline it silently returns stale
/// stats for the oldest slice; scoping the default to recent activity serves the
/// common intent and lets the scan complete within budget. Callers wanting
/// lifetime stats pass an explicit `start_ts_ns` (e.g. 0).
const TIMELINE_STATS_DEFAULT_WINDOW_NS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;

/// Lower bound `now - window_ns`, saturating at 0. Uses the wall clock; a clock
/// at or before the epoch yields 0 (full range) rather than erroring.
fn recent_window_start_ns(window_ns: u64) -> u64 {
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    now_ns.saturating_sub(window_ns)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineOperation {
    Get,
    Search,
    Stats,
}

impl TimelineOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Search => "search",
            Self::Stats => "stats",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineParams {
    pub operation: TimelineOperation,
    #[serde(default)]
    pub get: Option<TimelineGetParams>,
    #[serde(default)]
    pub search: Option<TimelineSearchParams>,
    #[serde(default)]
    pub stats: Option<TimelineStatsParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineResponse {
    pub operation: TimelineOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub get: Option<TimelineGetResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<TimelineSearchResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<TimelineStatsResponse>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOperation {
    List,
    Get,
}

impl EpisodeOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Get => "get",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeParams {
    pub operation: EpisodeOperation,
    #[serde(default)]
    pub list: Option<EpisodeListParams>,
    #[serde(default)]
    pub get: Option<EpisodeGetParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeResponse {
    pub operation: EpisodeOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<EpisodeListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub get: Option<EpisodeGetResponse>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyOperation {
    Pause,
    Resume,
    Exclusions,
    Redact,
    Purge,
}

impl PrivacyOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Exclusions => "exclusions",
            Self::Redact => "redact",
            Self::Purge => "purge",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrivacyParams {
    pub operation: PrivacyOperation,
    #[serde(default)]
    pub pause: Option<TimelinePauseParams>,
    #[serde(default)]
    pub resume: Option<TimelineResumeParams>,
    #[serde(default)]
    pub exclusions: Option<TimelineExclusionsParams>,
    #[serde(default)]
    pub redact: Option<HygieneRedactParams>,
    #[serde(default)]
    pub purge: Option<TimelinePurgeParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrivacyResponse {
    pub operation: PrivacyOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<TimelinePauseResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<TimelineResumeResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusions: Option<TimelineExclusionsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redact: Option<HygieneRedactResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge: Option<TimelinePurgeResponse>,
}

#[tool_router(router = timeline_facade_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Facade for timeline get/search/stats in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Delegates to the real timeline_get/timeline_search/timeline_stats paths and returns CF_TIMELINE/readback metadata."
    )]
    pub async fn timeline(
        &self,
        params: Parameters<TimelineParams>,
    ) -> Result<Json<TimelineResponse>, ErrorData> {
        validate_timeline_facade_params(&params.0)?;
        let operation = params.0.operation;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TIMELINE_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=timeline"
        );
        match operation {
            TimelineOperation::Get => {
                let spec = params.0.get.ok_or_else(|| missing_timeline_spec("get"))?;
                let source_id = timeline_range_source_id(spec.start_ts_ns, spec.end_ts_ns);
                let response = self
                    .timeline_get(Parameters(spec))
                    .await
                    .map_err(|error| {
                        timeline_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix timeline get bounds/kinds/actor/cursor and inspect CF_TIMELINE rows",
                        )
                    })?
                    .0;
                Ok(Json(timeline_response(
                    operation,
                    format!(
                        "CF_TIMELINE get rows={} scanned={} invalid={} next_cursor={}",
                        response.rows.len(),
                        response.scanned_rows,
                        response.invalid_rows,
                        cursor_state(&response.next_cursor)
                    ),
                    |out| out.get = Some(response),
                )))
            }
            TimelineOperation::Search => {
                let spec = params
                    .0
                    .search
                    .ok_or_else(|| missing_timeline_spec("search"))?;
                let source_id = timeline_range_source_id(spec.start_ts_ns, spec.end_ts_ns);
                let response = self
                    .timeline_search(Parameters(spec))
                    .await
                    .map_err(|error| {
                        timeline_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix timeline search filters/cursor and inspect CF_TIMELINE rows",
                        )
                    })?
                    .0;
                Ok(Json(timeline_response(
                    operation,
                    format!(
                        "CF_TIMELINE search matches={} scanned={} invalid={} next_cursor={}",
                        response.matches.len(),
                        response.scanned_rows,
                        response.invalid_rows,
                        cursor_state(&response.next_cursor)
                    ),
                    |out| out.search = Some(response),
                )))
            }
            TimelineOperation::Stats => {
                let mut spec = params
                    .0
                    .stats
                    .ok_or_else(|| missing_timeline_spec("stats"))?;
                // #1516: when no time bounds are supplied, seek to a recent window
                // instead of scanning from the oldest row, so the default serves
                // "recent activity" and completes within the scan budget. The applied
                // default is reported in the readback so it is never silent.
                let applied_default_window_start_ns =
                    if spec.start_ts_ns.is_none() && spec.end_ts_ns.is_none() {
                        let start = recent_window_start_ns(TIMELINE_STATS_DEFAULT_WINDOW_NS);
                        spec.start_ts_ns = Some(start);
                        Some(start)
                    } else {
                        None
                    };
                let source_id = timeline_range_source_id(spec.start_ts_ns, spec.end_ts_ns);
                let response = self
                    .timeline_stats(Parameters(spec))
                    .await
                    .map_err(|error| {
                        timeline_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix timeline stats bounds and inspect CF_TIMELINE plus recorder control state",
                        )
                    })?
                    .0;
                let default_window_note = match applied_default_window_start_ns {
                    Some(start) => format!(
                        " applied_default_recent_window_start_ns={start} (no start_ts_ns/end_ts_ns supplied; pass start_ts_ns=0 for lifetime stats)"
                    ),
                    None => String::new(),
                };
                Ok(Json(timeline_response(
                    operation,
                    format!(
                        "CF_TIMELINE stats total_rows={} scanned={} invalid={} scan_complete={} recorder_paused={}{}",
                        response.total_rows,
                        response.scanned_rows,
                        response.invalid_rows,
                        response.scan_complete,
                        response.recorder.paused,
                        default_window_note,
                    ),
                    |out| out.stats = Some(response),
                )))
            }
        }
    }

    #[tool(
        description = "Facade for episode reads in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Delegates to the real episode_list/episode_get paths and returns CF_EPISODES plus CF_TIMELINE evidence readback metadata."
    )]
    pub async fn episode(
        &self,
        params: Parameters<EpisodeParams>,
    ) -> Result<Json<EpisodeResponse>, ErrorData> {
        validate_episode_facade_params(&params.0)?;
        let operation = params.0.operation;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = EPISODE_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=episode"
        );
        match operation {
            EpisodeOperation::List => {
                let spec = params.0.list.ok_or_else(|| missing_episode_spec("list"))?;
                let source_id = timeline_range_source_id(spec.start_ts_ns, spec.end_ts_ns);
                let response = self
                    .episode_list(Parameters(spec))
                    .await
                    .map_err(|error| {
                        episode_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix episode list bounds/apps/actor/cursor and inspect CF_EPISODES rows",
                        )
                    })?
                    .0;
                Ok(Json(episode_response(
                    operation,
                    format!(
                        "CF_EPISODES list episodes={} scanned={} next_cursor={}",
                        response.episodes.len(),
                        response.scanned_rows,
                        cursor_state(&response.next_cursor)
                    ),
                    |out| out.list = Some(response),
                )))
            }
            EpisodeOperation::Get => {
                let spec = params.0.get.ok_or_else(|| missing_episode_spec("get"))?;
                let source_id = spec.episode_id.clone();
                let response = self
                    .episode_get(Parameters(spec))
                    .await
                    .map_err(|error| {
                        episode_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an existing episode_id and inspect CF_EPISODES plus CF_TIMELINE refs",
                        )
                    })?
                    .0;
                Ok(Json(episode_response(
                    operation,
                    format!(
                        "CF_EPISODES episode_id={} timeline_refs={} refs_scanned={} refs_invalid={} next_refs_cursor={}",
                        response.episode.episode_id,
                        response.timeline_refs.len(),
                        response.refs_scanned_rows,
                        response.refs_invalid_rows,
                        cursor_state(&response.next_refs_cursor)
                    ),
                    |out| out.get = Some(response),
                )))
            }
        }
    }

    #[tool(
        description = "Profile-gated privacy facade for timeline recorder pause/resume/exclusions plus timeline redaction/purge in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Mutations require an explicit break_glass/full_capability profile and return physical CF_KV/CF_TIMELINE/hygiene readback metadata."
    )]
    pub async fn privacy(
        &self,
        params: Parameters<PrivacyParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<PrivacyResponse>, ErrorData> {
        validate_privacy_facade_params(&params.0)?;
        let operation = params.0.operation;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = PRIVACY_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=privacy"
        );
        match operation {
            PrivacyOperation::Pause => {
                let spec = params
                    .0
                    .pause
                    .ok_or_else(|| missing_privacy_spec("pause"))?;
                self.require_privacy_mutation_profile(
                    operation,
                    "timeline/control/v1",
                    &request_context,
                )?;
                let response = self
                    .timeline_pause(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        privacy_delegate_error(
                            operation,
                            "timeline/control/v1",
                            error,
                            "escalate to an explicit privacy/admin profile, then inspect CF_KV timeline control and boundary rows",
                        )
                    })?
                    .0;
                Ok(Json(privacy_response(
                    operation,
                    format!(
                        "CF_KV timeline/control/v1 paused={} persisted={} boundary_row_written={} changed_at_ns={}",
                        response.paused,
                        response.persisted,
                        response.boundary_row_written,
                        response.changed_at_ns
                    ),
                    |out| out.pause = Some(response),
                )))
            }
            PrivacyOperation::Resume => {
                let spec = params
                    .0
                    .resume
                    .ok_or_else(|| missing_privacy_spec("resume"))?;
                self.require_privacy_mutation_profile(
                    operation,
                    "timeline/control/v1",
                    &request_context,
                )?;
                let response = self
                    .timeline_resume(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        privacy_delegate_error(
                            operation,
                            "timeline/control/v1",
                            error,
                            "escalate to an explicit privacy/admin profile, then inspect CF_KV timeline control and boundary rows",
                        )
                    })?
                    .0;
                Ok(Json(privacy_response(
                    operation,
                    format!(
                        "CF_KV timeline/control/v1 paused={} persisted={} boundary_row_written={} changed_at_ns={}",
                        response.paused,
                        response.persisted,
                        response.boundary_row_written,
                        response.changed_at_ns
                    ),
                    |out| out.resume = Some(response),
                )))
            }
            PrivacyOperation::Exclusions => {
                let spec = params
                    .0
                    .exclusions
                    .ok_or_else(|| missing_privacy_spec("exclusions"))?;
                if exclusions_mutates(&spec) {
                    self.require_privacy_mutation_profile(
                        operation,
                        "timeline/control/v1/exclusions",
                        &request_context,
                    )?;
                }
                let response = self
                    .timeline_exclusions(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        privacy_delegate_error(
                            operation,
                            "timeline/control/v1/exclusions",
                            error,
                            "fix exclusion add/remove entries, escalate for mutations, and inspect recorder control state",
                        )
                    })?
                    .0;
                Ok(Json(privacy_response(
                    operation,
                    format!(
                        "timeline exclusions runtime={} effective={} persisted={} added={} removed={}",
                        response.runtime_exclusions.len(),
                        response.effective_exclusions.len(),
                        response.persisted,
                        response.added.len(),
                        response.removed.len()
                    ),
                    |out| out.exclusions = Some(response),
                )))
            }
            PrivacyOperation::Redact => {
                let spec = params
                    .0
                    .redact
                    .ok_or_else(|| missing_privacy_spec("redact"))?;
                let source_id = redact_source_id(&spec);
                self.require_privacy_mutation_profile(operation, &source_id, &request_context)?;
                let response = self
                    .timeline_redact(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        privacy_delegate_error(
                            operation,
                            source_id,
                            error,
                            "escalate to an explicit privacy/admin profile, fix the flag selector, and inspect source rows plus hygiene taint/audit rows",
                        )
                    })?
                    .0;
                Ok(Json(privacy_response(
                    operation,
                    format!(
                        "hygiene redact matched_flags={} redacted_rows={} dry_run={} audit_key={}",
                        response.matched_flags,
                        response.redacted_rows,
                        response.dry_run,
                        response.audit_key_hex.as_deref().unwrap_or("<none>")
                    ),
                    |out| out.redact = Some(response),
                )))
            }
            PrivacyOperation::Purge => {
                let spec = params
                    .0
                    .purge
                    .ok_or_else(|| missing_privacy_spec("purge"))?;
                let source_id = purge_source_id(&spec);
                self.require_privacy_mutation_profile(operation, &source_id, &request_context)?;
                let response = self
                    .timeline_purge(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        privacy_delegate_error(
                            operation,
                            source_id,
                            error,
                            "escalate to an explicit privacy/admin profile, fix purge filters, and inspect CF_TIMELINE rows plus purge audit row",
                        )
                    })?
                    .0;
                Ok(Json(privacy_response(
                    operation,
                    format!(
                        "CF_TIMELINE purge matched={} deleted={} dry_run={} audit_key={} next_cursor={}",
                        response.matched_rows,
                        response.deleted_rows,
                        response.dry_run,
                        response.audit_key_hex.as_deref().unwrap_or("<none>"),
                        cursor_state(&response.next_cursor)
                    ),
                    |out| out.purge = Some(response),
                )))
            }
        }
    }
}

impl SynapseService {
    fn require_privacy_mutation_profile(
        &self,
        operation: PrivacyOperation,
        source_id: &str,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?;
        let snapshot = self.tool_profile_snapshot(session_id.as_deref())?;
        if matches!(
            snapshot.profile,
            ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability
        ) {
            return Ok(());
        }
        Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "privacy operation={} requires an explicit privacy/admin tool profile",
                operation.as_str()
            ),
            Some(json!({
                "code": error_codes::TOOL_PROFILE_POLICY_DENIED,
                "tool": PRIVACY_TOOL,
                "operation": operation.as_str(),
                "source_id": source_id,
                "source_of_truth": "CF_SESSIONS mcp/tool-profile/v1/<session_id>",
                "session_id": session_id,
                "profile": snapshot.profile.as_str(),
                "profile_label": snapshot.profile_label,
                "policy_row": snapshot.policy_row,
                "remediation": "call profile operation=set profile=break_glass confirm_break_glass=true reason=<privacy mutation reason> after acquiring any required lease, or run from the trusted full_capability local-agent/admin profile",
            })),
        ))
    }
}

fn validate_timeline_facade_params(params: &TimelineParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        TIMELINE_TOOL,
        params.operation.as_str(),
        &[
            ("get", params.get.is_some()),
            ("search", params.search.is_some()),
            ("stats", params.stats.is_some()),
        ],
    )
}

fn validate_episode_facade_params(params: &EpisodeParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        EPISODE_TOOL,
        params.operation.as_str(),
        &[
            ("list", params.list.is_some()),
            ("get", params.get.is_some()),
        ],
    )
}

fn validate_privacy_facade_params(params: &PrivacyParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        PRIVACY_TOOL,
        params.operation.as_str(),
        &[
            ("pause", params.pause.is_some()),
            ("resume", params.resume.is_some()),
            ("exclusions", params.exclusions.is_some()),
            ("redact", params.redact.is_some()),
            ("purge", params.purge.is_some()),
        ],
    )
}

fn validate_exact_operation_spec(
    tool: &'static str,
    operation: &'static str,
    specs: &[(&'static str, bool)],
) -> Result<(), ErrorData> {
    let present = specs
        .iter()
        .filter_map(|(name, is_present)| is_present.then_some(*name))
        .collect::<Vec<_>>();
    if !present.contains(&operation) {
        return Err(facade_params_error(
            tool,
            operation,
            format!("{tool} operation={operation} requires a matching {operation} spec"),
            format!("pass {operation}={{...}} and no other operation spec"),
        ));
    }
    if present.len() != 1 {
        return Err(facade_params_error(
            tool,
            operation,
            format!("{tool} operation={operation} received invalid operation specs {present:?}"),
            format!("pass exactly one operation-specific spec matching {operation}"),
        ));
    }
    Ok(())
}

fn missing_timeline_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        TIMELINE_TOOL,
        operation,
        format!("timeline operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn missing_episode_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        EPISODE_TOOL,
        operation,
        format!("episode operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn missing_privacy_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        PRIVACY_TOOL,
        operation,
        format!("privacy operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn facade_params_error(
    tool: &'static str,
    operation: &'static str,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_of_truth": "typed facade params before delegated operation",
            "remediation": remediation.into(),
        })),
    )
}

fn timeline_delegate_error(
    operation: TimelineOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        TIMELINE_TOOL,
        operation.as_str(),
        TIMELINE_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn episode_delegate_error(
    operation: EpisodeOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        EPISODE_TOOL,
        operation.as_str(),
        EPISODE_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn privacy_delegate_error(
    operation: PrivacyOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        PRIVACY_TOOL,
        operation.as_str(),
        PRIVACY_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn delegate_error(
    tool: &'static str,
    operation: &'static str,
    source_of_truth: &'static str,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause_data = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "tool": tool,
            "operation": operation,
            "source_of_truth": source_of_truth,
            "source_id": source_id.into(),
            "remediation": remediation,
            "cause": cause_data,
        })),
    )
}

fn timeline_response(
    operation: TimelineOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut TimelineResponse),
) -> TimelineResponse {
    let mut response = TimelineResponse {
        operation,
        source_of_truth: format!(
            "{TIMELINE_SOURCE_OF_TRUTH} + delegated timeline operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        get: None,
        search: None,
        stats: None,
    };
    populate(&mut response);
    response
}

fn episode_response(
    operation: EpisodeOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut EpisodeResponse),
) -> EpisodeResponse {
    let mut response = EpisodeResponse {
        operation,
        source_of_truth: format!(
            "{EPISODE_SOURCE_OF_TRUTH} + delegated episode operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        list: None,
        get: None,
    };
    populate(&mut response);
    response
}

fn privacy_response(
    operation: PrivacyOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut PrivacyResponse),
) -> PrivacyResponse {
    let mut response = PrivacyResponse {
        operation,
        source_of_truth: format!(
            "{PRIVACY_SOURCE_OF_TRUTH} + delegated privacy operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        pause: None,
        resume: None,
        exclusions: None,
        redact: None,
        purge: None,
    };
    populate(&mut response);
    response
}

fn cursor_state(cursor: &Option<String>) -> &'static str {
    if cursor.is_some() {
        "present"
    } else {
        "absent"
    }
}

fn timeline_range_source_id(start_ts_ns: Option<u64>, end_ts_ns: Option<u64>) -> String {
    format!(
        "range:{}..{}",
        start_ts_ns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unbounded".to_owned()),
        end_ts_ns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unbounded".to_owned())
    )
}

fn exclusions_mutates(params: &TimelineExclusionsParams) -> bool {
    !params.add.as_deref().unwrap_or_default().is_empty()
        || !params.remove.as_deref().unwrap_or_default().is_empty()
}

fn redact_source_id(params: &HygieneRedactParams) -> String {
    if let Some(ids) = params.flag_ids.as_ref() {
        return format!("flag_ids:{}", ids.len());
    }
    match (&params.source_cf, &params.source_key_hex, params.min_score) {
        (Some(source_cf), Some(source_key_hex), min_score) => {
            format!("query:{source_cf}:{source_key_hex}:min_score={min_score:?}")
        }
        (Some(source_cf), None, min_score) => {
            format!("query:{source_cf}:min_score={min_score:?}")
        }
        (None, Some(source_key_hex), min_score) => {
            format!("query:<missing_cf>:{source_key_hex}:min_score={min_score:?}")
        }
        (None, None, min_score) => format!("query:min_score={min_score:?}"),
    }
}

fn purge_source_id(params: &TimelinePurgeParams) -> String {
    if let Some(ids) = params.flag_ids.as_ref() {
        return format!("flag_ids:{}", ids.len());
    }
    if params.all {
        return "all".to_owned();
    }
    timeline_range_source_id(params.start_ts_ns, params.end_ts_ns)
}

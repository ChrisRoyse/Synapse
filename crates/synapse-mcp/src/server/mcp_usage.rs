use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;
use synapse_storage::{
    CalyxAnchorValueReadback, CalyxAnchorWriteReport, ConstellationPutReport, Db, GroundingAnchor,
    cf,
};

use crate::daemon_lifecycle::FinishedToolCallReadback;
use crate::m3::grounding;

use super::{ErrorData, SynapseService, mcp_error};

pub(crate) const MCP_USAGE_SOURCE_OF_TRUTH: &str =
    "CF_KV mcp-usage/v1 rows + Calyx syn-mcp-usage-v1 constellations + physical Anchors CF rows";

const USAGE_SCHEMA_VERSION: u32 = 1;
const USAGE_CALL_PREFIX: &str = "mcp-usage/v1/call/";
const USAGE_POLICY_PREFIX: &str = "mcp-usage/v1/policy/";
const USAGE_PROMOTION_PREFIX: &str = "mcp-usage/v1/promotion/";
const USAGE_SESSION_SEQ_PREFIX: &str = "mcp-usage/v1/session-seq/";
const STEERING_HINT_SIZE_LIMIT_BYTES: usize = 4 * 1024;
const PROMOTED_VALUE_SIZE_LIMIT_BYTES: usize = 1024;
const PROMOTED_VALUE_DEPTH_LIMIT: u32 = 8;
const STEERING_EVIDENCE_FLOOR: u64 = 1;
const KNOWN_BAD_COST_HINT_ID: &str = "cost_summarize_unbounded_scan";
const KNOWN_BAD_COST_ROUTE_ID: &str = "cost.summarize";
const KNOWN_BAD_COST_ERROR_TYPE: &str = "AGENT_COST_FLEET_ROLLUP_UNAVAILABLE";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageGuideParams {
    pub route_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageGuidePolicyParams {
    pub route_id: String,
    pub hint_id: String,
    pub enabled: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageGuidePromoteParams {
    pub route_id: String,
    pub parameter_path: String,
    pub promoted_value: McpUsagePromotedValue,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageGuideRollbackParams {
    pub promotion_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageGuideResponse {
    pub source_of_truth: String,
    pub route_id: String,
    pub evidence_rows: u64,
    pub success_rows: u64,
    pub error_rows: u64,
    pub latest_usage_row_key: Option<String>,
    pub latest_usage_value_sha256: Option<String>,
    pub pattern: McpUsagePattern,
    pub steering: Option<McpSteeringBlock>,
    pub active_policy: Option<McpUsagePolicySnapshot>,
    pub active_promotions: Vec<McpUsagePromotionSnapshot>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsagePolicyResponse {
    pub ok: bool,
    pub source_of_truth: String,
    pub policy: McpUsagePolicySnapshot,
    pub storage_readback: McpUsageStorageReadback,
    pub constellation_readback: McpUsageConstellationReadback,
    pub anchor_readback: McpUsageAnchorReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsagePromotionResponse {
    pub ok: bool,
    pub source_of_truth: String,
    pub promotion_id: String,
    pub route_id: String,
    pub parameter_path: String,
    pub promoted_value: McpUsagePromotedValue,
    pub state: String,
    pub ledger_rows: Vec<McpUsagePromotionSnapshot>,
    pub storage_readbacks: Vec<McpUsageStorageReadback>,
    pub constellation_readbacks: Vec<McpUsageConstellationReadback>,
    pub anchor_readbacks: Vec<McpUsageAnchorReadback>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsagePattern {
    pub kernel_basis: String,
    pub recommended_tool: String,
    pub recommended_operation: Option<String>,
    pub recommended_parameterizations: Vec<String>,
    pub misuse_warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpSteeringBlock {
    pub schema_version: u32,
    pub hint_id: String,
    pub route_id: String,
    pub evidence_floor: u64,
    pub evidence_rows: u64,
    pub latest_evidence_row_key: String,
    pub policy_row_key: String,
    pub source_of_truth: String,
    pub warning: String,
    pub suggested_next_tool: String,
    pub suggested_next_operation: String,
    pub suggested_parameters: Vec<String>,
    pub remediation: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsagePolicySnapshot {
    pub schema_version: u32,
    pub row_key: String,
    pub route_id: String,
    pub hint_id: String,
    pub enabled: bool,
    pub reason: String,
    pub observed_at_unix_ms: u64,
    pub evidence_floor: u64,
    pub evidence_rows_at_write: u64,
    pub latest_evidence_row_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsagePromotionSnapshot {
    pub schema_version: u32,
    pub row_key: String,
    pub promotion_id: String,
    pub route_id: String,
    pub parameter_path: String,
    pub promoted_value: McpUsagePromotedValue,
    pub state: String,
    pub stage_index: u64,
    pub reason: String,
    pub observed_at_unix_ms: u64,
    pub evidence_floor: u64,
    pub evidence_rows_at_write: u64,
    pub latest_evidence_row_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum McpUsagePromotedValue {
    Null(()),
    Bool(bool),
    Integer(i64),
    Number(f64),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpArgumentShape {
    pub top_level_keys: Vec<String>,
    pub nested_paths: Vec<String>,
    pub param_count: u64,
    pub nested_path_count: u64,
    pub shape_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageStorageReadback {
    pub cf_name: String,
    pub row_key: String,
    pub row_key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub exact_value_match: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageConstellationReadback {
    pub panel_name: String,
    pub panel_version: u32,
    pub source_cf: String,
    pub source_key_hex: String,
    pub raw_sha256: String,
    pub cx_id: String,
    pub disposition: String,
    pub latest_seq: u64,
    pub slot_count: u64,
    pub scalar_count: u64,
    pub duration_us: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageAnchorReadback {
    pub source_cf: String,
    pub source_key_hex: String,
    pub source_value_sha256: String,
    pub panel_name: String,
    pub panel_version: u32,
    pub cx_id: String,
    pub anchor_kind: String,
    pub anchor_value: McpUsageAnchorValueReadback,
    pub anchor_source: String,
    pub confidence: f32,
    pub ledger_seq: u64,
    pub ledger_hash: String,
    pub latest_seq: u64,
    pub readback_anchor_count: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpUsageAnchorValueReadback {
    pub value_type: String,
    pub bool_value: Option<bool>,
    pub text_value: Option<String>,
    pub number_value: Option<f64>,
    pub one_hot_values: Vec<String>,
    pub vector_len: Option<u64>,
    pub vector_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpUsageRecord {
    schema_version: u32,
    row_kind: String,
    run_id: String,
    pid: u32,
    seq: u64,
    session_sequence_position: u64,
    tool: String,
    operation: Option<String>,
    route_id: String,
    profile: Option<String>,
    tool_surface_sha256: Option<String>,
    mcp_session_id_sha256: Option<String>,
    argument_top_level_keys: Vec<String>,
    argument_nested_paths: Vec<String>,
    argument_top_level_key_count: u64,
    argument_nested_path_count: u64,
    argument_shape_sha256: String,
    status: String,
    error_type: Option<String>,
    started_at_unix_ms: u64,
    finished_at_unix_ms: u64,
    duration_ms: u64,
    response_size_bytes: u64,
    response_content_count: u64,
    steering_emitted: bool,
    steering_hint_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionSequenceCounter {
    schema_version: u32,
    row_kind: String,
    mcp_session_id_sha256: String,
    last_sequence_position: u64,
    updated_at_unix_ms: u64,
}

#[derive(Clone, Debug)]
struct PersistedUsageRecord {
    row_key: String,
    value_sha256: String,
    record: McpUsageRecord,
}

struct UsagePersistReadback {
    storage: McpUsageStorageReadback,
    constellation: McpUsageConstellationReadback,
    anchor: McpUsageAnchorReadback,
}

struct UsageEvidence {
    rows: Vec<PersistedUsageRecord>,
    success_rows: u64,
    error_rows: u64,
    latest_row_key: Option<String>,
    latest_value_sha256: Option<String>,
}

type UsageCounterRow = Option<(Vec<u8>, Vec<u8>)>;
type UsageSequenceReadback = (u64, UsageCounterRow);

pub(crate) fn argument_shape_from_arguments(
    arguments: Option<&Map<String, Value>>,
) -> McpArgumentShape {
    let Some(arguments) = arguments else {
        return finalize_argument_shape(Vec::new(), Vec::new());
    };
    let mut top_level_keys = arguments.keys().cloned().collect::<Vec<_>>();
    top_level_keys.sort();
    let mut nested_paths = Vec::new();
    for (key, value) in arguments {
        collect_argument_paths(key, value, &mut nested_paths);
    }
    nested_paths.sort();
    finalize_argument_shape(top_level_keys, nested_paths)
}

pub(crate) fn record_success_and_attach_steering(
    service: &SynapseService,
    finished: FinishedToolCallReadback,
    argument_shape: McpArgumentShape,
    result: &mut CallToolResult,
) -> Result<(), ErrorData> {
    let (response_size_bytes, response_content_count) = success_response_measurements(result)?;
    let db = service_mcp_usage_db(service)?;
    let steering = steering_for_finished_call(&db, &finished, None, &argument_shape)?;
    let readback = persist_finished_call(
        &db,
        finished,
        argument_shape,
        response_size_bytes,
        response_content_count,
        steering.as_ref().map(|block| block.hint_id.clone()),
    )?;
    tracing::debug!(
        code = "MCP_USAGE_CALL_PERSISTED",
        row_key = %readback.storage.row_key,
        cx_id = %readback.constellation.cx_id,
        anchor_kind = %readback.anchor.anchor_kind,
        "MCP tool-call usage row, Calyx constellation, and grounded anchor read back"
    );
    if let Some(block) = steering {
        attach_steering_to_success(result, block)?;
    }
    Ok(())
}

pub(crate) fn record_error_and_attach_steering(
    service: &SynapseService,
    finished: FinishedToolCallReadback,
    argument_shape: McpArgumentShape,
    mut error: ErrorData,
) -> Result<ErrorData, ErrorData> {
    let error_type = error_type_from_snapshot(finished.error.as_ref(), finished.panic.as_ref());
    let (response_size_bytes, response_content_count) = error_response_measurements(&error)?;
    let db = service_mcp_usage_db(service)?;
    let steering =
        steering_for_finished_call(&db, &finished, error_type.as_deref(), &argument_shape)?;
    let readback = persist_finished_call(
        &db,
        finished,
        argument_shape,
        response_size_bytes,
        response_content_count,
        steering.as_ref().map(|block| block.hint_id.clone()),
    )?;
    tracing::debug!(
        code = "MCP_USAGE_CALL_PERSISTED",
        row_key = %readback.storage.row_key,
        cx_id = %readback.constellation.cx_id,
        anchor_kind = %readback.anchor.anchor_kind,
        "MCP tool-call usage row, Calyx constellation, and grounded anchor read back"
    );
    if let Some(block) = steering {
        attach_steering_to_error(&mut error, block)?;
    }
    Ok(error)
}

pub(crate) fn guide(
    service: &SynapseService,
    params: McpUsageGuideParams,
) -> Result<McpUsageGuideResponse, ErrorData> {
    let route_id = validated_route_id(&params.route_id)?;
    let db = service_mcp_usage_db(service)?;
    let evidence = usage_evidence_for_route(&db, &route_id)?;
    let active_policy = latest_policy_for_route_hint(&db, &route_id, KNOWN_BAD_COST_HINT_ID)?;
    let steering = guidance_steering_for_route(&db, &route_id)?;
    let active_promotions = active_promotions_for_route(&db, &route_id)?;
    let pattern = pattern_for_route(
        &route_id,
        &evidence,
        active_policy.as_ref(),
        &active_promotions,
    )?;
    Ok(McpUsageGuideResponse {
        source_of_truth: MCP_USAGE_SOURCE_OF_TRUTH.to_owned(),
        route_id,
        evidence_rows: u64::try_from(evidence.rows.len()).unwrap_or(u64::MAX),
        success_rows: evidence.success_rows,
        error_rows: evidence.error_rows,
        latest_usage_row_key: evidence.latest_row_key,
        latest_usage_value_sha256: evidence.latest_value_sha256,
        pattern,
        steering,
        active_policy,
        active_promotions,
    })
}

pub(crate) fn write_policy(
    service: &SynapseService,
    params: McpUsageGuidePolicyParams,
) -> Result<McpUsagePolicyResponse, ErrorData> {
    let route_id = validated_route_id(&params.route_id)?;
    let hint_id = validated_hint_id(&params.hint_id)?;
    let reason = validated_reason(&params.reason)?;
    let db = service_mcp_usage_db(service)?;
    let evidence = usage_evidence_for_route(&db, &route_id)?;
    if params.enabled
        && hint_id == KNOWN_BAD_COST_HINT_ID
        && evidence.error_rows < STEERING_EVIDENCE_FLOOR
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "MCP_USAGE_POLICY_EVIDENCE_FLOOR_UNMET: route_id={route_id} hint_id={hint_id} evidence_rows={} floor={STEERING_EVIDENCE_FLOOR}; trigger the real MCP tool call that creates evidence first",
                evidence.error_rows
            ),
        ));
    }
    let observed_at_unix_ms = now_unix_ms()?;
    let row_key = format!(
        "{USAGE_POLICY_PREFIX}{}/{}/{observed_at_unix_ms:020}",
        safe_key_component(&route_id),
        safe_key_component(&hint_id),
    );
    let policy = McpUsagePolicySnapshot {
        schema_version: USAGE_SCHEMA_VERSION,
        row_key: row_key.clone(),
        route_id,
        hint_id,
        enabled: params.enabled,
        reason,
        observed_at_unix_ms,
        evidence_floor: STEERING_EVIDENCE_FLOOR,
        evidence_rows_at_write: evidence.error_rows,
        latest_evidence_row_key: evidence.latest_row_key,
    };
    let record = serde_json::to_value(&policy).map_err(serialize_mcp_usage_error)?;
    let encoded = serde_json::to_vec(&record).map_err(serialize_mcp_usage_error)?;
    let storage_readback = put_usage_kv_and_readback(&db, &row_key, encoded.clone(), None)?;
    let constellation_readback =
        put_usage_constellation_readback(&db, &row_key, &encoded, &record)?;
    let anchor = grounding::bool_anchor(
        "synapse:mcp_steering_enabled",
        policy.enabled,
        grounding::SOURCE_MCP_USAGE,
        observed_at_unix_ms,
    );
    let anchor_readback = write_usage_anchor_readback(&db, &row_key, &encoded, anchor)?;
    Ok(McpUsagePolicyResponse {
        ok: true,
        source_of_truth: MCP_USAGE_SOURCE_OF_TRUTH.to_owned(),
        policy,
        storage_readback,
        constellation_readback,
        anchor_readback,
    })
}

pub(crate) fn promote(
    service: &SynapseService,
    params: McpUsageGuidePromoteParams,
) -> Result<McpUsagePromotionResponse, ErrorData> {
    let route_id = validated_route_id(&params.route_id)?;
    let parameter_path = validated_parameter_path(&params.parameter_path)?;
    let promoted_value = validated_promoted_value(&params.promoted_value)?;
    let reason = validated_reason(&params.reason)?;
    let db = service_mcp_usage_db(service)?;
    let evidence = usage_evidence_for_route(&db, &route_id)?;
    if u64::try_from(evidence.rows.len()).unwrap_or(u64::MAX) < STEERING_EVIDENCE_FLOOR {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "MCP_USAGE_PROMOTION_EVIDENCE_FLOOR_UNMET: route_id={route_id} evidence_rows={} floor={STEERING_EVIDENCE_FLOOR}; trigger real MCP calls before promoting defaults",
                evidence.rows.len()
            ),
        ));
    }
    let observed_at_unix_ms = now_unix_ms()?;
    let promotion_id = promotion_id(&route_id, &parameter_path, &promoted_value)?;
    let stages = [("shadow", 0_u64), ("gates", 1_u64), ("promoted", 2_u64)];
    let mut ledger_rows = Vec::with_capacity(stages.len());
    let mut storage_readbacks = Vec::with_capacity(stages.len());
    let mut constellation_readbacks = Vec::with_capacity(stages.len());
    let mut anchor_readbacks = Vec::with_capacity(stages.len());
    for (state, stage_index) in stages {
        let row_key = format!(
            "{USAGE_PROMOTION_PREFIX}{}/{stage_index:03}-{state}",
            safe_key_component(&promotion_id),
        );
        let snapshot = McpUsagePromotionSnapshot {
            schema_version: USAGE_SCHEMA_VERSION,
            row_key: row_key.clone(),
            promotion_id: promotion_id.clone(),
            route_id: route_id.clone(),
            parameter_path: parameter_path.clone(),
            promoted_value: promoted_value.clone(),
            state: state.to_owned(),
            stage_index,
            reason: reason.clone(),
            observed_at_unix_ms,
            evidence_floor: STEERING_EVIDENCE_FLOOR,
            evidence_rows_at_write: u64::try_from(evidence.rows.len()).unwrap_or(u64::MAX),
            latest_evidence_row_key: evidence.latest_row_key.clone(),
        };
        let record = serde_json::to_value(&snapshot).map_err(serialize_mcp_usage_error)?;
        let encoded = serde_json::to_vec(&record).map_err(serialize_mcp_usage_error)?;
        storage_readbacks.push(put_usage_kv_and_readback(
            &db,
            &row_key,
            encoded.clone(),
            None,
        )?);
        constellation_readbacks.push(put_usage_constellation_readback(
            &db, &row_key, &encoded, &record,
        )?);
        let anchor = grounding::enum_anchor(
            "synapse:mcp_default_promotion_state",
            state,
            grounding::SOURCE_MCP_USAGE,
            observed_at_unix_ms,
        );
        anchor_readbacks.push(write_usage_anchor_readback(
            &db, &row_key, &encoded, anchor,
        )?);
        ledger_rows.push(snapshot);
    }
    Ok(McpUsagePromotionResponse {
        ok: true,
        source_of_truth: MCP_USAGE_SOURCE_OF_TRUTH.to_owned(),
        promotion_id,
        route_id,
        parameter_path,
        promoted_value,
        state: "promoted".to_owned(),
        ledger_rows,
        storage_readbacks,
        constellation_readbacks,
        anchor_readbacks,
    })
}

pub(crate) fn rollback(
    service: &SynapseService,
    params: McpUsageGuideRollbackParams,
) -> Result<McpUsagePromotionResponse, ErrorData> {
    let promotion_id = validated_component("promotion_id", &params.promotion_id)?;
    let reason = validated_reason(&params.reason)?;
    let db = service_mcp_usage_db(service)?;
    let existing = promotion_rows_for_id(&db, &promotion_id)?;
    let Some(promoted) = existing.iter().find(|row| row.state == "promoted") else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "MCP_USAGE_PROMOTION_ROLLBACK_TARGET_MISSING: promotion_id={promotion_id} has no promoted ledger row in {MCP_USAGE_SOURCE_OF_TRUTH}"
            ),
        ));
    };
    let existing_rollback = existing.iter().find(|row| row.state == "rolled_back");
    if let Some(rollback) = existing_rollback {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "MCP_USAGE_PROMOTION_ALREADY_ROLLED_BACK: promotion_id={promotion_id} rollback_row={}",
                rollback.row_key
            ),
        ));
    }
    let observed_at_unix_ms = now_unix_ms()?;
    let row_key = format!(
        "{USAGE_PROMOTION_PREFIX}{}/{:03}-rolled_back",
        safe_key_component(&promotion_id),
        promoted.stage_index.saturating_add(1),
    );
    let snapshot = McpUsagePromotionSnapshot {
        schema_version: USAGE_SCHEMA_VERSION,
        row_key: row_key.clone(),
        promotion_id: promotion_id.clone(),
        route_id: promoted.route_id.clone(),
        parameter_path: promoted.parameter_path.clone(),
        promoted_value: promoted.promoted_value.clone(),
        state: "rolled_back".to_owned(),
        stage_index: promoted.stage_index.saturating_add(1),
        reason,
        observed_at_unix_ms,
        evidence_floor: promoted.evidence_floor,
        evidence_rows_at_write: promoted.evidence_rows_at_write,
        latest_evidence_row_key: promoted.latest_evidence_row_key.clone(),
    };
    let record = serde_json::to_value(&snapshot).map_err(serialize_mcp_usage_error)?;
    let encoded = serde_json::to_vec(&record).map_err(serialize_mcp_usage_error)?;
    let storage_readback = put_usage_kv_and_readback(&db, &row_key, encoded.clone(), None)?;
    let constellation_readback =
        put_usage_constellation_readback(&db, &row_key, &encoded, &record)?;
    let anchor = grounding::enum_anchor(
        "synapse:mcp_default_promotion_state",
        "rolled_back",
        grounding::SOURCE_MCP_USAGE,
        observed_at_unix_ms,
    );
    let anchor_readback = write_usage_anchor_readback(&db, &row_key, &encoded, anchor)?;
    Ok(McpUsagePromotionResponse {
        ok: true,
        source_of_truth: MCP_USAGE_SOURCE_OF_TRUTH.to_owned(),
        promotion_id,
        route_id: snapshot.route_id.clone(),
        parameter_path: snapshot.parameter_path.clone(),
        promoted_value: snapshot.promoted_value.clone(),
        state: snapshot.state.clone(),
        ledger_rows: vec![snapshot],
        storage_readbacks: vec![storage_readback],
        constellation_readbacks: vec![constellation_readback],
        anchor_readbacks: vec![anchor_readback],
    })
}

fn finalize_argument_shape(
    top_level_keys: Vec<String>,
    nested_paths: Vec<String>,
) -> McpArgumentShape {
    let param_count = u64::try_from(top_level_keys.len()).unwrap_or(u64::MAX);
    let nested_path_count = u64::try_from(nested_paths.len()).unwrap_or(u64::MAX);
    let shape_sha256 = sha256_shape(&top_level_keys, &nested_paths);
    McpArgumentShape {
        top_level_keys,
        nested_paths,
        param_count,
        nested_path_count,
        shape_sha256,
    }
}

fn collect_argument_paths(prefix: &str, value: &Value, out: &mut Vec<String>) {
    out.push(format!("{prefix}:{}", value_kind(value)));
    match value {
        Value::Array(values) => {
            out.push(format!("{prefix}[]:array_len_{}", values.len()));
            for value in values.iter().take(8) {
                collect_argument_paths(&format!("{prefix}[]"), value, out);
            }
        }
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if let Some(value) = object.get(key) {
                    collect_argument_paths(&format!("{prefix}.{key}"), value, out);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn persist_finished_call(
    db: &Db,
    finished: FinishedToolCallReadback,
    argument_shape: McpArgumentShape,
    response_size_bytes: u64,
    response_content_count: u64,
    steering_hint_id: Option<String>,
) -> Result<UsagePersistReadback, ErrorData> {
    let session_hash = finished.mcp_session_id.as_deref().map(sha256_text);
    let observed_at_unix_ms = finished.finished_at_unix_ms;
    let (session_sequence_position, counter_row) =
        next_session_sequence_row(db, session_hash.as_deref(), observed_at_unix_ms)?;
    let route_id = finished
        .route_id
        .clone()
        .unwrap_or_else(|| finished.tool.clone());
    let error_type = error_type_from_snapshot(finished.error.as_ref(), finished.panic.as_ref());
    let steering_emitted = steering_hint_id.is_some();
    let record = McpUsageRecord {
        schema_version: USAGE_SCHEMA_VERSION,
        row_kind: "call".to_owned(),
        run_id: finished.run_id,
        pid: finished.pid,
        seq: finished.seq,
        session_sequence_position,
        tool: finished.tool,
        operation: finished.operation,
        route_id,
        profile: finished.profile,
        tool_surface_sha256: finished.tool_surface_sha256,
        mcp_session_id_sha256: session_hash,
        argument_top_level_keys: argument_shape.top_level_keys,
        argument_nested_paths: argument_shape.nested_paths,
        argument_top_level_key_count: argument_shape.param_count,
        argument_nested_path_count: argument_shape.nested_path_count,
        argument_shape_sha256: argument_shape.shape_sha256,
        status: finished.status,
        error_type,
        started_at_unix_ms: finished.started_at_unix_ms,
        finished_at_unix_ms: finished.finished_at_unix_ms,
        duration_ms: finished.duration_ms,
        response_size_bytes,
        response_content_count,
        steering_emitted,
        steering_hint_id,
    };
    let row_key = format!(
        "{USAGE_CALL_PREFIX}{}/{:020}",
        safe_key_component(&record.run_id),
        record.seq
    );
    let record_value = serde_json::to_value(&record).map_err(serialize_mcp_usage_error)?;
    let encoded = serde_json::to_vec(&record_value).map_err(serialize_mcp_usage_error)?;
    let storage = put_usage_kv_and_readback(db, &row_key, encoded.clone(), counter_row)?;
    let constellation = put_usage_constellation_readback(db, &row_key, &encoded, &record_value)?;
    let anchor = grounding::enum_anchor(
        "synapse:mcp_tool_call_outcome",
        &record.status,
        grounding::SOURCE_MCP_USAGE,
        observed_at_unix_ms,
    );
    let anchor = write_usage_anchor_readback(db, &row_key, &encoded, anchor)?;
    Ok(UsagePersistReadback {
        storage,
        constellation,
        anchor,
    })
}

fn next_session_sequence_row(
    db: &Db,
    session_hash: Option<&str>,
    observed_at_unix_ms: u64,
) -> Result<UsageSequenceReadback, ErrorData> {
    let Some(session_hash) = session_hash else {
        return Ok((0, None));
    };
    let key = format!("{USAGE_SESSION_SEQ_PREFIX}{session_hash}");
    let previous = db
        .get_cf(cf::CF_KV, key.as_bytes())
        .map_err(|error| storage_mcp_error("read MCP usage session sequence counter", error))?;
    let next = match previous {
        Some(bytes) => {
            let counter: SessionSequenceCounter =
                serde_json::from_slice(&bytes).map_err(|error| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "MCP_USAGE_SESSION_COUNTER_CORRUPT: key={key} decode failed: {error}"
                        ),
                    )
                })?;
            if counter.mcp_session_id_sha256 != session_hash {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "MCP_USAGE_SESSION_COUNTER_ID_MISMATCH: key={key} stored_hash={} expected_hash={session_hash}",
                        counter.mcp_session_id_sha256
                    ),
                ));
            }
            counter.last_sequence_position.saturating_add(1)
        }
        None => 1,
    };
    let counter = SessionSequenceCounter {
        schema_version: USAGE_SCHEMA_VERSION,
        row_kind: "session_sequence_counter".to_owned(),
        mcp_session_id_sha256: session_hash.to_owned(),
        last_sequence_position: next,
        updated_at_unix_ms: observed_at_unix_ms,
    };
    let encoded = serde_json::to_vec(&counter).map_err(serialize_mcp_usage_error)?;
    Ok((next, Some((key.into_bytes(), encoded))))
}

fn put_usage_kv_and_readback(
    db: &Db,
    row_key: &str,
    encoded: Vec<u8>,
    counter_row: Option<(Vec<u8>, Vec<u8>)>,
) -> Result<McpUsageStorageReadback, ErrorData> {
    let mut rows = Vec::with_capacity(if counter_row.is_some() { 2 } else { 1 });
    rows.push((row_key.as_bytes().to_vec(), encoded.clone()));
    if let Some(counter_row) = counter_row {
        rows.push(counter_row);
    }
    db.put_batch_pressure_bypass(cf::CF_KV, rows)
        .map_err(|error| storage_mcp_error("write MCP usage CF_KV row", error))?;
    let readback = db
        .get_cf(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| storage_mcp_error("read back MCP usage CF_KV row", error))?;
    let Some(readback) = readback else {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "MCP_USAGE_ROW_READBACK_MISSING: cf={} row_key={row_key}",
                cf::CF_KV
            ),
        ));
    };
    let exact_value_match = readback == encoded;
    if !exact_value_match {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "MCP_USAGE_ROW_READBACK_MISMATCH: cf={} row_key={row_key} expected_sha256={} actual_sha256={}",
                cf::CF_KV,
                sha256_hex(&encoded),
                sha256_hex(&readback)
            ),
        ));
    }
    Ok(McpUsageStorageReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        row_key_hex: hex_encode(row_key.as_bytes()),
        value_len_bytes: u64::try_from(readback.len()).unwrap_or(u64::MAX),
        value_sha256: sha256_hex(&readback),
        exact_value_match,
    })
}

fn put_usage_constellation_readback(
    db: &Db,
    row_key: &str,
    encoded: &[u8],
    record: &Value,
) -> Result<McpUsageConstellationReadback, ErrorData> {
    let report = db
        .put_mcp_usage_constellation(row_key.as_bytes(), encoded, record)
        .map_err(|error| storage_mcp_error("put MCP usage Calyx constellation", error))?;
    Ok(constellation_readback(report))
}

fn write_usage_anchor_readback(
    db: &Db,
    row_key: &str,
    encoded: &[u8],
    anchor: GroundingAnchor,
) -> Result<McpUsageAnchorReadback, ErrorData> {
    let report = grounding::write_anchor_for_existing_constellation(
        db,
        cf::CF_KV,
        row_key.as_bytes(),
        encoded,
        anchor,
        "MCP usage grounding",
    )?;
    Ok(anchor_readback(report))
}

fn steering_for_finished_call(
    db: &Db,
    finished: &FinishedToolCallReadback,
    error_type: Option<&str>,
    argument_shape: &McpArgumentShape,
) -> Result<Option<McpSteeringBlock>, ErrorData> {
    let route_id = finished
        .route_id
        .as_deref()
        .unwrap_or(finished.tool.as_str());
    if route_id != KNOWN_BAD_COST_ROUTE_ID || error_type != Some(KNOWN_BAD_COST_ERROR_TYPE) {
        return Ok(None);
    }
    if !argument_shape
        .nested_paths
        .iter()
        .any(|path| path == "summarize.all_history:bool" || path == "all_history:bool")
    {
        return Ok(None);
    }
    guidance_steering_for_route(db, route_id)
}

fn guidance_steering_for_route(
    db: &Db,
    route_id: &str,
) -> Result<Option<McpSteeringBlock>, ErrorData> {
    if route_id != KNOWN_BAD_COST_ROUTE_ID {
        return Ok(None);
    }
    let Some(policy) = latest_policy_for_route_hint(db, route_id, KNOWN_BAD_COST_HINT_ID)? else {
        return Ok(None);
    };
    if !policy.enabled {
        return Ok(None);
    }
    let evidence = usage_evidence_for_route(db, route_id)?;
    let matching_bad = evidence
        .rows
        .iter()
        .filter(|row| row.record.error_type.as_deref() == Some(KNOWN_BAD_COST_ERROR_TYPE))
        .collect::<Vec<_>>();
    let evidence_rows = u64::try_from(matching_bad.len()).unwrap_or(u64::MAX);
    if evidence_rows < policy.evidence_floor {
        return Ok(None);
    }
    let latest = matching_bad.last().ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "MCP_USAGE_STEERING_EVIDENCE_EMPTY_AFTER_COUNT",
        )
    })?;
    let block = McpSteeringBlock {
        schema_version: USAGE_SCHEMA_VERSION,
        hint_id: KNOWN_BAD_COST_HINT_ID.to_owned(),
        route_id: route_id.to_owned(),
        evidence_floor: policy.evidence_floor,
        evidence_rows,
        latest_evidence_row_key: latest.row_key.clone(),
        policy_row_key: policy.row_key,
        source_of_truth: MCP_USAGE_SOURCE_OF_TRUTH.to_owned(),
        warning: "agent_cost summarize without spawn_id is a known-bad unbounded fleet scan".to_owned(),
        suggested_next_tool: "cost".to_owned(),
        suggested_next_operation: "summarize".to_owned(),
        suggested_parameters: vec![
            "summarize.spawn_id=<existing-agent-spawn-id>".to_owned(),
            "summarize.all_history=false".to_owned(),
            "summarize.include_per_turn=true only when per-turn rows are needed".to_owned(),
        ],
        remediation: "scope the cost rollup to a spawn_id or wait for the TimeSeries/OLAP fleet rollup tracked by #1688".to_owned(),
    };
    enforce_steering_size_bound(&block)?;
    Ok(Some(block))
}

fn usage_evidence_for_route(db: &Db, route_id: &str) -> Result<UsageEvidence, ErrorData> {
    let rows = scan_usage_call_rows(db)?;
    let mut matched = rows
        .into_iter()
        .filter(|row| row.record.route_id == route_id)
        .collect::<Vec<_>>();
    matched.sort_by(|left, right| {
        left.record
            .finished_at_unix_ms
            .cmp(&right.record.finished_at_unix_ms)
            .then_with(|| left.row_key.cmp(&right.row_key))
    });
    let success_rows = matched
        .iter()
        .filter(|row| row.record.status == "ok")
        .count() as u64;
    let error_rows = matched
        .iter()
        .filter(|row| row.record.status != "ok")
        .count() as u64;
    let latest_row_key = matched.last().map(|row| row.row_key.clone());
    let latest_value_sha256 = matched.last().map(|row| row.value_sha256.clone());
    Ok(UsageEvidence {
        rows: matched,
        success_rows,
        error_rows,
        latest_row_key,
        latest_value_sha256,
    })
}

fn scan_usage_call_rows(db: &Db) -> Result<Vec<PersistedUsageRecord>, ErrorData> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, USAGE_CALL_PREFIX.as_bytes())
        .map_err(|error| storage_mcp_error("scan MCP usage call rows", error))?;
    let mut out = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let row_key = String::from_utf8(key.clone()).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "MCP_USAGE_ROW_KEY_NOT_UTF8: key_hex={} error={error}",
                    hex_encode(&key)
                ),
            )
        })?;
        let record: McpUsageRecord = serde_json::from_slice(&value).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("MCP_USAGE_ROW_CORRUPT: row_key={row_key} decode failed: {error}"),
            )
        })?;
        out.push(PersistedUsageRecord {
            row_key,
            value_sha256: sha256_hex(&value),
            record,
        });
    }
    Ok(out)
}

fn latest_policy_for_route_hint(
    db: &Db,
    route_id: &str,
    hint_id: &str,
) -> Result<Option<McpUsagePolicySnapshot>, ErrorData> {
    let prefix = format!(
        "{USAGE_POLICY_PREFIX}{}/{}/",
        safe_key_component(route_id),
        safe_key_component(hint_id)
    );
    let mut policies = Vec::new();
    for (key, value) in db
        .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
        .map_err(|error| storage_mcp_error("scan MCP usage policy rows", error))?
    {
        let row_key = String::from_utf8(key.clone()).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "MCP_USAGE_POLICY_KEY_NOT_UTF8: key_hex={} error={error}",
                    hex_encode(&key)
                ),
            )
        })?;
        let mut policy: McpUsagePolicySnapshot =
            serde_json::from_slice(&value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "MCP_USAGE_POLICY_ROW_CORRUPT: row_key={row_key} decode failed: {error}"
                    ),
                )
            })?;
        policy.row_key = row_key;
        policies.push(policy);
    }
    policies.sort_by(|left, right| {
        left.observed_at_unix_ms
            .cmp(&right.observed_at_unix_ms)
            .then_with(|| left.row_key.cmp(&right.row_key))
    });
    Ok(policies.pop())
}

fn active_promotions_for_route(
    db: &Db,
    route_id: &str,
) -> Result<Vec<McpUsagePromotionSnapshot>, ErrorData> {
    let mut by_id: BTreeMap<String, Vec<McpUsagePromotionSnapshot>> = BTreeMap::new();
    for row in scan_promotion_rows(db)? {
        if row.route_id == route_id {
            by_id.entry(row.promotion_id.clone()).or_default().push(row);
        }
    }
    let mut active = Vec::new();
    for (_promotion_id, mut rows) in by_id {
        rows.sort_by_key(|row| row.stage_index);
        if rows.iter().any(|row| row.state == "rolled_back") {
            continue;
        }
        if let Some(row) = rows.into_iter().find(|row| row.state == "promoted") {
            active.push(row);
        }
    }
    Ok(active)
}

fn promotion_rows_for_id(
    db: &Db,
    promotion_id: &str,
) -> Result<Vec<McpUsagePromotionSnapshot>, ErrorData> {
    let prefix = format!(
        "{USAGE_PROMOTION_PREFIX}{}/",
        safe_key_component(promotion_id)
    );
    let mut rows = Vec::new();
    for (key, value) in db
        .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
        .map_err(|error| storage_mcp_error("scan MCP usage promotion rows", error))?
    {
        let row_key = String::from_utf8(key.clone()).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "MCP_USAGE_PROMOTION_KEY_NOT_UTF8: key_hex={} error={error}",
                    hex_encode(&key)
                ),
            )
        })?;
        let mut row: McpUsagePromotionSnapshot =
            serde_json::from_slice(&value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "MCP_USAGE_PROMOTION_ROW_CORRUPT: row_key={row_key} decode failed: {error}"
                    ),
                )
            })?;
        row.row_key = row_key;
        rows.push(row);
    }
    rows.sort_by_key(|row| row.stage_index);
    Ok(rows)
}

fn scan_promotion_rows(db: &Db) -> Result<Vec<McpUsagePromotionSnapshot>, ErrorData> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, USAGE_PROMOTION_PREFIX.as_bytes())
        .map_err(|error| storage_mcp_error("scan MCP usage promotion ledger", error))?;
    let mut out = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let row_key = String::from_utf8(key.clone()).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "MCP_USAGE_PROMOTION_KEY_NOT_UTF8: key_hex={} error={error}",
                    hex_encode(&key)
                ),
            )
        })?;
        let mut row: McpUsagePromotionSnapshot =
            serde_json::from_slice(&value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "MCP_USAGE_PROMOTION_ROW_CORRUPT: row_key={row_key} decode failed: {error}"
                    ),
                )
            })?;
        row.row_key = row_key;
        out.push(row);
    }
    Ok(out)
}

fn pattern_for_route(
    route_id: &str,
    evidence: &UsageEvidence,
    active_policy: Option<&McpUsagePolicySnapshot>,
    active_promotions: &[McpUsagePromotionSnapshot],
) -> Result<McpUsagePattern, ErrorData> {
    let (tool, operation) = route_id
        .split_once('.')
        .map_or((route_id.to_owned(), None), |(tool, operation)| {
            (tool.to_owned(), Some(operation.to_owned()))
        });
    let mut recommended_parameterizations = Vec::with_capacity(active_promotions.len() + 2);
    for row in active_promotions {
        recommended_parameterizations.push(format!(
            "{}={}",
            row.parameter_path,
            promoted_value_text(&row.promoted_value)?
        ));
    }
    let mut misuse_warnings = Vec::new();
    if route_id == KNOWN_BAD_COST_ROUTE_ID {
        recommended_parameterizations
            .push("summarize.spawn_id=<existing-agent-spawn-id>".to_owned());
        recommended_parameterizations.push("summarize.all_history=false".to_owned());
        if active_policy.is_some_and(|policy| policy.enabled) {
            misuse_warnings.push(
                "unbounded cost summarize is grounded as a known-bad call pattern".to_owned(),
            );
        }
    }
    recommended_parameterizations.sort();
    recommended_parameterizations.dedup();
    Ok(McpUsagePattern {
        kernel_basis: format!(
            "syn-mcp-usage-v1 constellations over {} usage rows with {} grounded errors and {} grounded successes",
            evidence.rows.len(),
            evidence.error_rows,
            evidence.success_rows
        ),
        recommended_tool: tool,
        recommended_operation: operation,
        recommended_parameterizations,
        misuse_warnings,
    })
}

fn attach_steering_to_success(
    result: &mut CallToolResult,
    block: McpSteeringBlock,
) -> Result<(), ErrorData> {
    let block_value = serde_json::to_value(&block).map_err(serialize_mcp_usage_error)?;
    let Some(structured) = result.structured_content.as_mut() else {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "MCP_STEERING_RESPONSE_SHAPE_INVALID: tool result has no structured_content object to receive steering",
        ));
    };
    let Some(object) = structured.as_object_mut() else {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "MCP_STEERING_RESPONSE_SHAPE_INVALID: structured_content is not an object",
        ));
    };
    object.insert("steering".to_owned(), block_value);
    result.content = vec![Content::text(structured.to_string())];
    Ok(())
}

fn attach_steering_to_error(
    error: &mut ErrorData,
    block: McpSteeringBlock,
) -> Result<(), ErrorData> {
    let block_value = serde_json::to_value(&block).map_err(serialize_mcp_usage_error)?;
    match error.data.as_mut() {
        Some(data) => {
            let Some(object) = data.as_object_mut() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "MCP_STEERING_ERROR_DATA_SHAPE_INVALID: error data is not an object",
                ));
            };
            object.insert("steering".to_owned(), block_value);
        }
        None => {
            error.data = Some(json!({
                "steering": block_value,
                "source_of_truth": MCP_USAGE_SOURCE_OF_TRUTH,
            }));
        }
    }
    Ok(())
}

fn enforce_steering_size_bound(block: &McpSteeringBlock) -> Result<(), ErrorData> {
    let encoded = serde_json::to_vec(block).map_err(serialize_mcp_usage_error)?;
    if encoded.len() > STEERING_HINT_SIZE_LIMIT_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "MCP_STEERING_BLOCK_OVERSIZE: {} bytes exceeds limit {STEERING_HINT_SIZE_LIMIT_BYTES}",
                encoded.len()
            ),
        ));
    }
    Ok(())
}

fn success_response_measurements(result: &CallToolResult) -> Result<(u64, u64), ErrorData> {
    let encoded = serde_json::to_vec(result).map_err(serialize_mcp_usage_error)?;
    Ok((
        u64::try_from(encoded.len()).unwrap_or(u64::MAX),
        u64::try_from(result.content.len()).unwrap_or(u64::MAX),
    ))
}

fn error_response_measurements(error: &ErrorData) -> Result<(u64, u64), ErrorData> {
    let encoded = serde_json::to_vec(error).map_err(serialize_mcp_usage_error)?;
    Ok((u64::try_from(encoded.len()).unwrap_or(u64::MAX), 1))
}

fn error_type_from_snapshot(error: Option<&Value>, panic: Option<&Value>) -> Option<String> {
    if panic.is_some() {
        return Some("panic".to_owned());
    }
    let error = error?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if message.contains(KNOWN_BAD_COST_ERROR_TYPE) {
        return Some(KNOWN_BAD_COST_ERROR_TYPE.to_owned());
    }
    error
        .pointer("/data/detail_code")
        .and_then(Value::as_str)
        .or_else(|| error.pointer("/data/code").and_then(Value::as_str))
        .or_else(|| error.get("synapse_code").and_then(Value::as_str))
        .or_else(|| {
            error
                .get("rmcp_code")
                .and_then(Value::as_i64)
                .map(|_| "json_rpc_error")
        })
        .map(str::to_owned)
}

fn service_mcp_usage_db(service: &SynapseService) -> Result<std::sync::Arc<Db>, ErrorData> {
    let state = service.m3_state_handle();
    let mut guard = state.lock().map_err(|_error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "MCP_USAGE_STATE_LOCK_POISONED: M3 service state lock poisoned while opening usage storage",
        )
    })?;
    guard.ensure_storage().map_err(|error| {
        mcp_error(
            error.code(),
            format!("MCP_USAGE_STORAGE_OPEN_FAILED: {error}"),
        )
    })
}

fn serialize_mcp_usage_error(error: serde_json::Error) -> ErrorData {
    mcp_error(
        error_codes::TOOL_INTERNAL_ERROR,
        format!("MCP_USAGE_JSON_SERIALIZE_FAILED: {error}"),
    )
}

fn storage_mcp_error(action: &'static str, error: synapse_storage::StorageError) -> ErrorData {
    mcp_error(
        error.code(),
        format!("MCP_USAGE_STORAGE_FAILED: {action}: {error}"),
    )
}

fn constellation_readback(report: ConstellationPutReport) -> McpUsageConstellationReadback {
    McpUsageConstellationReadback {
        panel_name: report.panel_name.to_owned(),
        panel_version: report.panel_version,
        source_cf: report.source_cf.to_owned(),
        source_key_hex: report.source_key_hex,
        raw_sha256: report.raw_sha256,
        cx_id: report.cx_id,
        disposition: report.disposition.as_str().to_owned(),
        latest_seq: report.latest_seq,
        slot_count: report.slot_count,
        scalar_count: report.scalar_count,
        duration_us: report.duration_us,
    }
}

fn anchor_readback(report: CalyxAnchorWriteReport) -> McpUsageAnchorReadback {
    McpUsageAnchorReadback {
        source_cf: report.source_cf,
        source_key_hex: report.source_key_hex,
        source_value_sha256: report.source_value_sha256,
        panel_name: report.panel_name,
        panel_version: report.panel_version,
        cx_id: report.cx_id,
        anchor_kind: report.anchor_kind,
        anchor_value: anchor_value_readback(report.anchor_value),
        anchor_source: report.anchor_source,
        confidence: report.confidence,
        ledger_seq: report.ledger_seq,
        ledger_hash: report.ledger_hash,
        latest_seq: report.latest_seq,
        readback_anchor_count: report.readback_anchor_count,
    }
}

fn anchor_value_readback(value: CalyxAnchorValueReadback) -> McpUsageAnchorValueReadback {
    McpUsageAnchorValueReadback {
        value_type: value.value_type,
        bool_value: value.bool_value,
        text_value: value.text_value,
        number_value: value.number_value,
        one_hot_values: value.one_hot_values,
        vector_len: value.vector_len,
        vector_sha256: value.vector_sha256,
    }
}

fn validated_route_id(route_id: &str) -> Result<String, ErrorData> {
    validated_component("route_id", route_id)
}

fn validated_hint_id(hint_id: &str) -> Result<String, ErrorData> {
    validated_component("hint_id", hint_id)
}

fn validated_parameter_path(parameter_path: &str) -> Result<String, ErrorData> {
    validated_component("parameter_path", parameter_path)
}

fn validated_promoted_value(
    promoted_value: &McpUsagePromotedValue,
) -> Result<McpUsagePromotedValue, ErrorData> {
    let depth = promoted_value_depth(promoted_value);
    if depth > PROMOTED_VALUE_DEPTH_LIMIT {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "MCP_USAGE_PROMOTED_VALUE_INVALID: promoted_value nesting depth {depth} exceeds limit {PROMOTED_VALUE_DEPTH_LIMIT}"
            ),
        ));
    }
    let encoded = serde_json::to_vec(promoted_value).map_err(serialize_mcp_usage_error)?;
    if encoded.len() > PROMOTED_VALUE_SIZE_LIMIT_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "MCP_USAGE_PROMOTED_VALUE_INVALID: promoted_value serialized length {} exceeds {PROMOTED_VALUE_SIZE_LIMIT_BYTES} bytes",
                encoded.len()
            ),
        ));
    }
    Ok(promoted_value.clone())
}

fn validated_reason(reason: &str) -> Result<String, ErrorData> {
    let reason = reason.trim();
    if reason.len() < 8 || reason.len() > 512 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "MCP_USAGE_REASON_INVALID: reason must be 8..=512 trimmed bytes",
        ));
    }
    Ok(reason.to_owned())
}

fn validated_component(field: &'static str, value: &str) -> Result<String, ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("MCP_USAGE_{field}_INVALID: {field} must be 1..=128 trimmed bytes"),
        ));
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "MCP_USAGE_{field}_INVALID: {field} may contain only ASCII alphanumeric, '.', '_', '-', or '/'"
            ),
        ));
    }
    Ok(trimmed.to_owned())
}

fn promotion_id(
    route_id: &str,
    parameter_path: &str,
    promoted_value: &McpUsagePromotedValue,
) -> Result<String, ErrorData> {
    let promoted_value = promoted_value_text(promoted_value)?;
    let material = format!("{route_id}\0{parameter_path}\0{promoted_value}");
    Ok(format!(
        "{}-{}",
        safe_key_component(route_id),
        &sha256_text(&material).trim_start_matches("sha256:")[..16]
    ))
}

fn promoted_value_text(promoted_value: &McpUsagePromotedValue) -> Result<String, ErrorData> {
    serde_json::to_string(promoted_value).map_err(serialize_mcp_usage_error)
}

fn promoted_value_depth(promoted_value: &McpUsagePromotedValue) -> u32 {
    match promoted_value {
        McpUsagePromotedValue::Array(values) => values
            .iter()
            .map(promoted_value_depth)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
        McpUsagePromotedValue::Object(values) => values
            .values()
            .map(promoted_value_depth)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
        McpUsagePromotedValue::Null(_)
        | McpUsagePromotedValue::Bool(_)
        | McpUsagePromotedValue::Integer(_)
        | McpUsagePromotedValue::Number(_)
        | McpUsagePromotedValue::String(_) => 1,
    }
}

fn safe_key_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
                char::from(byte)
            } else {
                '_'
            }
        })
        .collect()
}

fn now_unix_ms() -> Result<u64, ErrorData> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("MCP_USAGE_CLOCK_BEFORE_UNIX_EPOCH: {error}"),
            )
        })?;
    Ok(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

fn sha256_shape(top_level_keys: &[String], nested_paths: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-mcp-argument-shape-v1\0");
    for key in top_level_keys {
        hasher.update((key.len() as u64).to_be_bytes());
        hasher.update(key.as_bytes());
    }
    hasher.update(b"\0nested\0");
    for path in nested_paths {
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
    }
    format!("sha256:{}", hex_encode(hasher.finalize().as_ref()))
}

fn sha256_text(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_encode(digest.as_ref()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[(byte >> 4) as usize]));
        out.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    out
}

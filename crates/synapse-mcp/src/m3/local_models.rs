use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, Url, header};
use rmcp::{ErrorData, model::ErrorCode, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use synapse_core::{SCHEMA_VERSION, error_codes};
use synapse_storage::{Db, cf};
use uuid::Uuid;

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

const REGISTRY_PREFIX: &str = "local_model_registry/v1/model/name_hex/";
const PROBE_EVIDENCE_PREFIX: &str = "local_model_registry/v1/probe/name_hex/";
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
const MAX_NAME_CHARS: usize = 100;
const MAX_BASE_URL_CHARS: usize = 2048;
const MAX_MODEL_ID_CHARS: usize = 256;
const MAX_NOTES_CHARS: usize = 2000;
const MAX_ENV_VAR_CHARS: usize = 128;
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 10_000;
const MAX_PROBE_TIMEOUT_MS: u64 = 120_000;
const RAW_RESPONSE_EXCERPT_CHARS: usize = 2048;
const PROBE_TOOL_NAME: &str = "synapse_probe";

/// A raw `(key, value)` row as returned by a column-family scan.
type RawRow = (Vec<u8>, Vec<u8>);

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelApiShape {
    #[default]
    OpenAiChatCompletions,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelRuntimePreset {
    #[default]
    OpenAiCompatible,
    DeepSeekV4FlashNonThinking,
    DeepSeekV4Reasoning,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelRegisterParams {
    pub name: String,
    pub base_url: String,
    pub model_id: String,
    #[serde(default)]
    pub api_shape: LocalModelApiShape,
    #[serde(default)]
    pub runtime_preset: LocalModelRuntimePreset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub context_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_tools: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_non_loopback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env_var: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 120000))]
    pub probe_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_true")]
    pub include_disabled: bool,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 1000))]
    pub limit: usize,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelUpdateParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_shape: Option<LocalModelApiShape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_preset: Option<LocalModelRuntimePreset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub clear_context_length: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_tools: Option<u32>,
    #[serde(default)]
    pub clear_max_tools: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default)]
    pub clear_notes: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_non_loopback: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env_var: Option<String>,
    #[serde(default)]
    pub clear_api_key_env_var: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 120000))]
    pub probe_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelRemoveParams {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelProbeParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 120000))]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LocalModelRegistryRow {
    pub schema_version: u32,
    pub row_key: String,
    pub name: String,
    pub base_url: String,
    pub model_id: String,
    pub api_shape: LocalModelApiShape,
    #[serde(default)]
    pub runtime_preset: LocalModelRuntimePreset,
    pub context_length: Option<u32>,
    pub max_tools: Option<u32>,
    pub notes: Option<String>,
    pub enabled: bool,
    pub allow_non_loopback: bool,
    pub api_key_env_var: Option<String>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub created_by_session: String,
    pub updated_by_session: String,
    pub last_probe: Option<LocalModelProbeReport>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LocalModelProbeReport {
    pub schema_version: u32,
    pub observed_at_unix_ms: u64,
    pub endpoint_url: String,
    pub healthy: bool,
    pub status: String,
    pub latency_ms: u64,
    pub tokens_per_second: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub raw_response_sha256: Option<String>,
    pub raw_response_excerpt: Option<String>,
    pub raw_response_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalModelStorageReadback {
    pub cf_name: String,
    pub row_key: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelCorruptRow {
    pub row_key: String,
    pub value_len_bytes: u64,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelRegisterResponse {
    pub row: LocalModelRegistryRow,
    pub probe: LocalModelProbeReport,
    pub storage_readback: LocalModelStorageReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelListResponse {
    pub schema_version: u32,
    pub source_of_truth: String,
    pub scanned_rows: usize,
    pub rows: Vec<LocalModelRegistryRow>,
    pub corrupt_rows: Vec<LocalModelCorruptRow>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelUpdateResponse {
    pub before_row: LocalModelRegistryRow,
    pub row: LocalModelRegistryRow,
    pub probe: Option<LocalModelProbeReport>,
    pub storage_readback: LocalModelStorageReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelRemoveResponse {
    pub removed_row: LocalModelRegistryRow,
    pub removed_readback: LocalModelStorageReadback,
    pub after_row_present: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalModelProbeResponse {
    pub row: LocalModelRegistryRow,
    pub probe: LocalModelProbeReport,
    pub storage_readback: LocalModelStorageReadback,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalModelProbeEvidenceRow {
    schema_version: u32,
    row_key: String,
    model_row_key: String,
    name: String,
    action: String,
    by_session: String,
    created_at_unix_ms: u64,
    probe: LocalModelProbeReport,
}

#[must_use]
pub const fn local_model_register() -> M3ToolStub {
    M3ToolStub::new("local_model_register")
}

#[must_use]
pub const fn local_model_list() -> M3ToolStub {
    M3ToolStub::new("local_model_list")
}

#[must_use]
pub const fn local_model_update() -> M3ToolStub {
    M3ToolStub::new("local_model_update")
}

#[must_use]
pub const fn local_model_remove() -> M3ToolStub {
    M3ToolStub::new("local_model_remove")
}

#[must_use]
pub const fn local_model_probe() -> M3ToolStub {
    M3ToolStub::new("local_model_probe")
}

#[must_use]
pub fn required_permissions_register(_params: &LocalModelRegisterParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_list(_params: &LocalModelListParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_update(_params: &LocalModelUpdateParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_remove(_params: &LocalModelRemoveParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_probe(_params: &LocalModelProbeParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

pub async fn register_local_model(
    db: &Arc<Db>,
    params: LocalModelRegisterParams,
    by_session: &str,
) -> Result<LocalModelRegisterResponse, ErrorData> {
    validate_register_params(&params)?;
    let now = unix_time_ms_now();
    let row_key = registry_row_key(&params.name)?;
    if read_model_row_optional(db, &row_key)?.is_some() {
        return Err(model_registry_error(
            error_codes::MODEL_REGISTRY_CONFLICT,
            format!("local model {:?} already exists", params.name),
            json!({
                "row_key": row_key,
                "source_of_truth": cf::CF_KV,
            }),
        ));
    }

    let mut row = LocalModelRegistryRow {
        schema_version: SCHEMA_VERSION,
        row_key: row_key.clone(),
        name: normalize_name(&params.name)?,
        base_url: normalize_base_url(&params.base_url)?,
        model_id: normalize_model_id(&params.model_id)?,
        api_shape: params.api_shape,
        runtime_preset: params.runtime_preset,
        context_length: params.context_length,
        max_tools: params.max_tools,
        notes: normalize_optional_text(params.notes, "notes", MAX_NOTES_CHARS)?,
        enabled: params.enabled,
        allow_non_loopback: params.allow_non_loopback,
        api_key_env_var: normalize_optional_env_var(params.api_key_env_var)?,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        created_by_session: by_session.to_owned(),
        updated_by_session: by_session.to_owned(),
        last_probe: None,
    };

    let probe = probe_row(&row, params.probe_timeout_ms).await;
    row.last_probe = Some(probe.clone());
    if !probe.healthy {
        let evidence = write_probe_evidence(db, &row, &probe, "register_rejected", by_session)?;
        return Err(probe_error_with_evidence(&probe, Some(&evidence)));
    }

    let encoded = encode_json_row(&row)?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
        .map_err(storage_error)?;
    let storage_readback = readback_exact_row(db, &row_key)?;
    Ok(LocalModelRegisterResponse {
        row,
        probe,
        storage_readback,
    })
}

pub fn list_local_models(
    db: &Arc<Db>,
    params: &LocalModelListParams,
) -> Result<LocalModelListResponse, ErrorData> {
    validate_list_params(params)?;
    let (rows, corrupt_rows) = if let Some(name) = params.name.as_deref() {
        let row_key = registry_row_key(name)?;
        match scan_exact_row(db, &row_key)? {
            Some((key, encoded)) => match decode_model_row(&key, &encoded) {
                Ok(row) => (vec![row], Vec::new()),
                Err(error) => (
                    Vec::new(),
                    vec![LocalModelCorruptRow {
                        row_key,
                        value_len_bytes: encoded.len() as u64,
                        error,
                    }],
                ),
            },
            None => (Vec::new(), Vec::new()),
        }
    } else {
        scan_model_rows(db)?
    };

    let scanned_rows = rows.len() + corrupt_rows.len();
    let rows = rows
        .into_iter()
        .filter(|row| params.include_disabled || row.enabled)
        .take(params.limit)
        .collect::<Vec<_>>();
    Ok(LocalModelListResponse {
        schema_version: SCHEMA_VERSION,
        source_of_truth: format!("{} prefix {REGISTRY_PREFIX}", cf::CF_KV),
        scanned_rows,
        rows,
        corrupt_rows,
    })
}

pub async fn update_local_model(
    db: &Arc<Db>,
    params: LocalModelUpdateParams,
    by_session: &str,
) -> Result<LocalModelUpdateResponse, ErrorData> {
    validate_update_params(&params)?;
    let old_row_key = registry_row_key(&params.name)?;
    let before_row = read_model_row_required(db, &old_row_key)?;
    let mut row = before_row.clone();
    let mut probe_required = false;

    if let Some(new_name) = params.new_name.as_deref() {
        row.name = normalize_name(new_name)?;
        row.row_key = registry_row_key(new_name)?;
        if row.row_key != old_row_key && read_model_row_optional(db, &row.row_key)?.is_some() {
            return Err(model_registry_error(
                error_codes::MODEL_REGISTRY_CONFLICT,
                format!("local model {new_name:?} already exists"),
                json!({
                    "row_key": row.row_key,
                    "source_of_truth": cf::CF_KV,
                }),
            ));
        }
    }
    if let Some(base_url) = params.base_url {
        row.base_url = normalize_base_url(&base_url)?;
        probe_required = true;
    }
    if let Some(model_id) = params.model_id {
        row.model_id = normalize_model_id(&model_id)?;
        probe_required = true;
    }
    if let Some(api_shape) = params.api_shape {
        row.api_shape = api_shape;
        probe_required = true;
    }
    if let Some(runtime_preset) = params.runtime_preset {
        row.runtime_preset = runtime_preset;
        probe_required = true;
    }
    if params.clear_context_length {
        row.context_length = None;
    } else if params.context_length.is_some() {
        row.context_length = params.context_length;
    }
    if params.clear_max_tools {
        row.max_tools = None;
    } else if params.max_tools.is_some() {
        row.max_tools = params.max_tools;
    }
    if params.clear_notes {
        row.notes = None;
    } else if params.notes.is_some() {
        row.notes = normalize_optional_text(params.notes, "notes", MAX_NOTES_CHARS)?;
    }
    if let Some(enabled) = params.enabled {
        row.enabled = enabled;
        if enabled {
            probe_required = true;
        }
    }
    if let Some(allow_non_loopback) = params.allow_non_loopback {
        row.allow_non_loopback = allow_non_loopback;
        probe_required = true;
    }
    if params.clear_api_key_env_var {
        row.api_key_env_var = None;
        probe_required = true;
    } else if params.api_key_env_var.is_some() {
        row.api_key_env_var = normalize_optional_env_var(params.api_key_env_var)?;
        probe_required = true;
    }

    row.updated_at_unix_ms = unix_time_ms_now();
    row.updated_by_session = by_session.to_owned();

    let probe = if probe_required {
        let probe = probe_row(&row, params.probe_timeout_ms).await;
        row.last_probe = Some(probe.clone());
        if !probe.healthy {
            let evidence = write_probe_evidence(db, &row, &probe, "update_rejected", by_session)?;
            return Err(probe_error_with_evidence(&probe, Some(&evidence)));
        }
        Some(probe)
    } else {
        None
    };

    let encoded = encode_json_row(&row)?;
    let deletes = if old_row_key == row.row_key {
        Vec::new()
    } else {
        vec![old_row_key.as_bytes().to_vec()]
    };
    db.mutate_batch_pressure_bypass(
        cf::CF_KV,
        deletes,
        [(row.row_key.as_bytes().to_vec(), encoded)],
    )
    .map_err(storage_error)?;
    let storage_readback = readback_exact_row(db, &row.row_key)?;
    Ok(LocalModelUpdateResponse {
        before_row,
        row,
        probe,
        storage_readback,
    })
}

pub fn remove_local_model(
    db: &Arc<Db>,
    params: &LocalModelRemoveParams,
) -> Result<LocalModelRemoveResponse, ErrorData> {
    validate_remove_params(params)?;
    let row_key = registry_row_key(&params.name)?;
    let removed_row = read_model_row_required(db, &row_key)?;
    let removed_readback = readback_exact_row(db, &row_key)?;
    db.delete_batch(cf::CF_KV, [row_key.as_bytes().to_vec()])
        .map_err(storage_error)?;
    let after_row_present = scan_exact_row(db, &row_key)?.is_some();
    Ok(LocalModelRemoveResponse {
        removed_row,
        removed_readback,
        after_row_present,
    })
}

pub async fn probe_local_model(
    db: &Arc<Db>,
    params: &LocalModelProbeParams,
    by_session: &str,
) -> Result<LocalModelProbeResponse, ErrorData> {
    validate_probe_params(params)?;
    let row_key = registry_row_key(&params.name)?;
    let mut row = read_model_row_required(db, &row_key)?;
    let probe = probe_row(&row, params.timeout_ms).await;
    row.last_probe = Some(probe.clone());
    row.updated_at_unix_ms = unix_time_ms_now();
    row.updated_by_session = by_session.to_owned();
    let encoded = encode_json_row(&row)?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(row.row_key.as_bytes().to_vec(), encoded)])
        .map_err(storage_error)?;
    let storage_readback = readback_exact_row(db, &row.row_key)?;
    Ok(LocalModelProbeResponse {
        row,
        probe,
        storage_readback,
    })
}

pub fn local_model_snapshot(db: &Arc<Db>) -> Result<Vec<LocalModelRegistryRow>, ErrorData> {
    let (rows, _corrupt_rows) = scan_model_rows(db)?;
    Ok(rows)
}

async fn probe_row(row: &LocalModelRegistryRow, timeout_ms: Option<u64>) -> LocalModelProbeReport {
    let observed_at_unix_ms = unix_time_ms_now();
    let started = std::time::Instant::now();
    let endpoint = match chat_completions_endpoint(row) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            return probe_report_error(
                observed_at_unix_ms,
                "",
                started.elapsed(),
                error_codes::TOOL_PARAMS_INVALID,
                error.message.to_string(),
                None,
            );
        }
    };

    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_PROBE_TIMEOUT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_PROBE_TIMEOUT_MS {
        return probe_report_error(
            observed_at_unix_ms,
            endpoint.as_str(),
            started.elapsed(),
            error_codes::TOOL_PARAMS_INVALID,
            format!("probe timeout_ms must be between 1 and {MAX_PROBE_TIMEOUT_MS}"),
            None,
        );
    }

    let client = match Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return probe_report_error(
                observed_at_unix_ms,
                endpoint.as_str(),
                started.elapsed(),
                error_codes::MODEL_ENDPOINT_UNREACHABLE,
                format!("failed to build HTTP client: {error}"),
                None,
            );
        }
    };

    let nonce = format!("probe-{}", Uuid::now_v7().simple());
    let mut body = probe_request(&row.model_id, &nonce);
    apply_runtime_preset(row, &mut body);
    let mut request = client.post(endpoint.clone()).json(&body);
    if let Some(env_var) = row.api_key_env_var.as_deref() {
        match std::env::var(env_var) {
            Ok(token) if !token.trim().is_empty() => {
                request = request.bearer_auth(token);
            }
            Ok(_) | Err(_) => {
                return probe_report_error(
                    observed_at_unix_ms,
                    endpoint.as_str(),
                    started.elapsed(),
                    error_codes::MODEL_API_KEY_MISSING,
                    format!("api_key_env_var {env_var:?} is not set to a non-empty value"),
                    None,
                );
            }
        }
    }
    request = request.header(header::ACCEPT, "application/json");

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return probe_report_error(
                observed_at_unix_ms,
                endpoint.as_str(),
                started.elapsed(),
                error_codes::MODEL_ENDPOINT_UNREACHABLE,
                format!("local model endpoint was unreachable: {error}"),
                None,
            );
        }
    };
    let status = response.status();
    let raw_text = match response.text().await {
        Ok(text) => text,
        Err(error) => {
            return probe_report_error(
                observed_at_unix_ms,
                endpoint.as_str(),
                started.elapsed(),
                error_codes::MODEL_ENDPOINT_UNREACHABLE,
                format!("local model endpoint response body could not be read: {error}"),
                None,
            );
        }
    };

    if !status.is_success() {
        return probe_report_error(
            observed_at_unix_ms,
            endpoint.as_str(),
            started.elapsed(),
            error_codes::MODEL_TOOLS_UNSUPPORTED,
            format!(
                "tool-call probe request returned HTTP status {}",
                status.as_u16()
            ),
            Some(&raw_text),
        );
    }

    match validate_tool_call_response(&raw_text, &nonce) {
        Ok(usage) => probe_report_success(
            observed_at_unix_ms,
            endpoint.as_str(),
            started.elapsed(),
            usage,
            &raw_text,
        ),
        Err(detail) => probe_report_error(
            observed_at_unix_ms,
            endpoint.as_str(),
            started.elapsed(),
            error_codes::MODEL_TOOLS_UNSUPPORTED,
            detail,
            Some(&raw_text),
        ),
    }
}

fn probe_request(model_id: &str, nonce: &str) -> serde_json::Value {
    json!({
        "model": model_id,
        "messages": [
            {
                "role": "system",
                "content": "You are validating local model tool calling. Return no prose. Call the requested tool exactly once."
            },
            {
                "role": "user",
                "content": format!("Call {PROBE_TOOL_NAME} with nonce {nonce:?}.")
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": PROBE_TOOL_NAME,
                    "description": "Echo the provided nonce to prove structured tool calling works.",
                    "parameters": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "nonce": {
                                "type": "string",
                                "description": "The nonce copied from the user message."
                            }
                        },
                        "required": ["nonce"]
                    }
                }
            }
        ],
        "tool_choice": {
            "type": "function",
            "function": {
                "name": PROBE_TOOL_NAME
            }
        },
        "stream": false,
        "temperature": 0,
        "max_tokens": 128
    })
}

fn apply_runtime_preset(row: &LocalModelRegistryRow, body: &mut serde_json::Value) {
    match row.runtime_preset {
        LocalModelRuntimePreset::OpenAiCompatible => {}
        LocalModelRuntimePreset::DeepSeekV4FlashNonThinking => {
            body["thinking"] = json!({ "type": "disabled" });
        }
        LocalModelRuntimePreset::DeepSeekV4Reasoning => {
            body["thinking"] = json!({ "type": "enabled" });
            body["reasoning_effort"] = json!("max");
            if let Some(object) = body.as_object_mut() {
                object.remove("tool_choice");
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ProbeUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

fn validate_tool_call_response(raw_text: &str, nonce: &str) -> Result<ProbeUsage, String> {
    let value: serde_json::Value = serde_json::from_str(raw_text)
        .map_err(|error| format!("probe response is not valid JSON: {error}"))?;
    let choice = value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "probe response missing choices[0]".to_owned())?;
    let message = choice
        .get("message")
        .ok_or_else(|| "probe response missing choices[0].message".to_owned())?;
    let tool_calls = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "probe response missing message.tool_calls array".to_owned())?;
    let call = tool_calls
        .iter()
        .find(|call| {
            call.get("function")
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
                == Some(PROBE_TOOL_NAME)
        })
        .ok_or_else(|| format!("probe response did not call required tool {PROBE_TOOL_NAME:?}"))?;
    let function = call
        .get("function")
        .ok_or_else(|| "probe tool call missing function object".to_owned())?;
    let arguments = function
        .get("arguments")
        .ok_or_else(|| "probe tool call missing function.arguments".to_owned())?;
    let argument_value = match arguments {
        serde_json::Value::String(raw) => serde_json::from_str::<serde_json::Value>(raw)
            .map_err(|error| format!("probe tool arguments string is not valid JSON: {error}"))?,
        value if value.is_object() => value.clone(),
        _ => {
            return Err(
                "probe tool arguments must be a JSON object or JSON object string".to_owned(),
            );
        }
    };
    let actual_nonce = argument_value
        .get("nonce")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "probe tool arguments missing nonce string".to_owned())?;
    if actual_nonce != nonce {
        return Err(format!(
            "probe tool nonce mismatch: expected {nonce:?}, got {actual_nonce:?}"
        ));
    }
    let usage = value.get("usage");
    Ok(ProbeUsage {
        prompt_tokens: usage
            .and_then(|usage| usage.get("prompt_tokens"))
            .and_then(serde_json::Value::as_u64),
        completion_tokens: usage
            .and_then(|usage| usage.get("completion_tokens"))
            .and_then(serde_json::Value::as_u64),
        total_tokens: usage
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(serde_json::Value::as_u64),
    })
}

fn probe_report_success(
    observed_at_unix_ms: u64,
    endpoint_url: &str,
    elapsed: Duration,
    usage: ProbeUsage,
    raw_text: &str,
) -> LocalModelProbeReport {
    let latency_ms = duration_ms(elapsed);
    let token_count = usage.completion_tokens.or(usage.total_tokens);
    let (raw_response_excerpt, raw_response_truncated) =
        truncate_chars(raw_text, RAW_RESPONSE_EXCERPT_CHARS);
    LocalModelProbeReport {
        schema_version: SCHEMA_VERSION,
        observed_at_unix_ms,
        endpoint_url: endpoint_url.to_owned(),
        healthy: true,
        status: "healthy".to_owned(),
        latency_ms,
        tokens_per_second: tokens_per_second(token_count, latency_ms),
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
        error_code: None,
        error_detail: None,
        raw_response_sha256: Some(sha256_bytes(raw_text.as_bytes())),
        raw_response_excerpt: Some(raw_response_excerpt),
        raw_response_truncated,
    }
}

fn probe_report_error(
    observed_at_unix_ms: u64,
    endpoint_url: &str,
    elapsed: Duration,
    error_code: &'static str,
    detail: String,
    raw_text: Option<&str>,
) -> LocalModelProbeReport {
    let (excerpt, truncated) = raw_text.map_or((None, false), |raw| {
        let (excerpt, truncated) = truncate_chars(raw, RAW_RESPONSE_EXCERPT_CHARS);
        (Some(excerpt), truncated)
    });
    LocalModelProbeReport {
        schema_version: SCHEMA_VERSION,
        observed_at_unix_ms,
        endpoint_url: endpoint_url.to_owned(),
        healthy: false,
        status: "unhealthy".to_owned(),
        latency_ms: duration_ms(elapsed),
        tokens_per_second: None,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        error_code: Some(error_code.to_owned()),
        error_detail: Some(detail),
        raw_response_sha256: raw_text.map(|raw| sha256_bytes(raw.as_bytes())),
        raw_response_excerpt: excerpt,
        raw_response_truncated: truncated,
    }
}

fn write_probe_evidence(
    db: &Arc<Db>,
    row: &LocalModelRegistryRow,
    probe: &LocalModelProbeReport,
    action: &str,
    by_session: &str,
) -> Result<LocalModelStorageReadback, ErrorData> {
    let name_hex = hex_lower(row.name.as_bytes());
    let evidence_key = format!(
        "{PROBE_EVIDENCE_PREFIX}{name_hex}/{:020}-{}",
        probe.observed_at_unix_ms,
        Uuid::now_v7().simple()
    );
    let evidence = LocalModelProbeEvidenceRow {
        schema_version: SCHEMA_VERSION,
        row_key: evidence_key.clone(),
        model_row_key: row.row_key.clone(),
        name: row.name.clone(),
        action: action.to_owned(),
        by_session: by_session.to_owned(),
        created_at_unix_ms: unix_time_ms_now(),
        probe: probe.clone(),
    };
    let encoded = encode_json_row(&evidence)?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(evidence_key.as_bytes().to_vec(), encoded)])
        .map_err(storage_error)?;
    readback_exact_row(db, &evidence_key)
}

fn chat_completions_endpoint(row: &LocalModelRegistryRow) -> Result<Url, ErrorData> {
    match row.api_shape {
        LocalModelApiShape::OpenAiChatCompletions => {}
    }
    let base_url = normalize_base_url(&row.base_url)?;
    let parsed = Url::parse(&base_url).map_err(|error| {
        invalid(format!(
            "base_url must be an absolute http(s) URL parseable by reqwest: {error}"
        ))
    })?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(invalid("base_url must not include query or fragment"));
    }
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(invalid(format!(
                "base_url scheme {scheme:?} is not supported"
            )));
        }
    }
    if !row.allow_non_loopback && !is_loopback_url(&parsed) {
        return Err(invalid(
            "base_url host must be loopback unless allow_non_loopback=true",
        ));
    }
    let trimmed = base_url.trim_end_matches('/');
    let path = parsed.path().trim_end_matches('/');
    let endpoint = if path.ends_with("/chat/completions") {
        trimmed.to_owned()
    } else if path.is_empty() || path == "/" {
        format!("{trimmed}/v1/chat/completions")
    } else {
        format!("{trimmed}/chat/completions")
    };
    Url::parse(&endpoint).map_err(|error| {
        invalid(format!(
            "derived chat completions endpoint is invalid: {error}"
        ))
    })
}

fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|addr| addr.is_loopback())
}

fn scan_model_rows(
    db: &Arc<Db>,
) -> Result<(Vec<LocalModelRegistryRow>, Vec<LocalModelCorruptRow>), ErrorData> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, REGISTRY_PREFIX.as_bytes())
        .map_err(storage_error)?;
    let mut decoded = Vec::new();
    let mut corrupt = Vec::new();
    for (key, value) in rows {
        match decode_model_row(&key, &value) {
            Ok(row) => decoded.push(row),
            Err(error) => corrupt.push(LocalModelCorruptRow {
                row_key: String::from_utf8_lossy(&key).into_owned(),
                value_len_bytes: value.len() as u64,
                error,
            }),
        }
    }
    decoded.sort_by(|left, right| left.name.cmp(&right.name));
    Ok((decoded, corrupt))
}

fn read_model_row_required(
    db: &Arc<Db>,
    row_key: &str,
) -> Result<LocalModelRegistryRow, ErrorData> {
    read_model_row_optional(db, row_key)?.ok_or_else(|| {
        model_registry_error(
            error_codes::MODEL_REGISTRY_NOT_FOUND,
            format!("local model row not found: {row_key}"),
            json!({
                "row_key": row_key,
                "source_of_truth": cf::CF_KV,
            }),
        )
    })
}

fn read_model_row_optional(
    db: &Arc<Db>,
    row_key: &str,
) -> Result<Option<LocalModelRegistryRow>, ErrorData> {
    let Some((key, value)) = scan_exact_row(db, row_key)? else {
        return Ok(None);
    };
    decode_model_row(&key, &value).map(Some).map_err(|error| {
        model_registry_error(
            error_codes::STORAGE_CORRUPTED,
            format!("local model registry row {row_key:?} is corrupt: {error}"),
            json!({
                "row_key": row_key,
                "source_of_truth": cf::CF_KV,
            }),
        )
    })
}

fn scan_exact_row(db: &Arc<Db>, row_key: &str) -> Result<Option<RawRow>, ErrorData> {
    let key_bytes = row_key.as_bytes();
    let rows = db
        .scan_cf_prefix(cf::CF_KV, key_bytes)
        .map_err(storage_error)?;
    Ok(rows.into_iter().find(|(key, _value)| key == key_bytes))
}

fn decode_model_row(key: &[u8], value: &[u8]) -> Result<LocalModelRegistryRow, String> {
    let row: LocalModelRegistryRow = serde_json::from_slice(value)
        .map_err(|error| format!("decode local model row failed: {error}"))?;
    let row_key = String::from_utf8_lossy(key).into_owned();
    if row.row_key != row_key {
        return Err(format!(
            "row_key field mismatch: stored key {row_key:?}, value row_key {:?}",
            row.row_key
        ));
    }
    Ok(row)
}

fn readback_exact_row(db: &Arc<Db>, row_key: &str) -> Result<LocalModelStorageReadback, ErrorData> {
    let Some((_key, value)) = scan_exact_row(db, row_key)? else {
        return Err(model_registry_error(
            error_codes::STORAGE_READ_FAILED,
            format!("required CF_KV row missing after write: {row_key}"),
            json!({
                "row_key": row_key,
                "source_of_truth": cf::CF_KV,
            }),
        ));
    };
    Ok(LocalModelStorageReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        value_len_bytes: value.len() as u64,
        value_sha256: sha256_bytes(&value),
    })
}

fn registry_row_key(name: &str) -> Result<String, ErrorData> {
    let name = normalize_name(name)?;
    Ok(format!("{REGISTRY_PREFIX}{}", hex_lower(name.as_bytes())))
}

fn validate_register_params(params: &LocalModelRegisterParams) -> Result<(), ErrorData> {
    normalize_name(&params.name)?;
    normalize_base_url(&params.base_url)?;
    normalize_model_id(&params.model_id)?;
    validate_optional_u32(params.context_length, "context_length")?;
    validate_optional_u32(params.max_tools, "max_tools")?;
    normalize_optional_text(params.notes.clone(), "notes", MAX_NOTES_CHARS)?;
    normalize_optional_env_var(params.api_key_env_var.clone())?;
    validate_timeout(params.probe_timeout_ms)?;
    Ok(())
}

fn validate_list_params(params: &LocalModelListParams) -> Result<(), ErrorData> {
    if let Some(name) = params.name.as_deref() {
        normalize_name(name)?;
    }
    validate_limit(params.limit)?;
    Ok(())
}

fn validate_update_params(params: &LocalModelUpdateParams) -> Result<(), ErrorData> {
    normalize_name(&params.name)?;
    if let Some(name) = params.new_name.as_deref() {
        normalize_name(name)?;
    }
    if let Some(base_url) = params.base_url.as_deref() {
        normalize_base_url(base_url)?;
    }
    if let Some(model_id) = params.model_id.as_deref() {
        normalize_model_id(model_id)?;
    }
    validate_optional_u32(params.context_length, "context_length")?;
    validate_optional_u32(params.max_tools, "max_tools")?;
    normalize_optional_text(params.notes.clone(), "notes", MAX_NOTES_CHARS)?;
    normalize_optional_env_var(params.api_key_env_var.clone())?;
    validate_timeout(params.probe_timeout_ms)?;
    Ok(())
}

fn validate_remove_params(params: &LocalModelRemoveParams) -> Result<(), ErrorData> {
    normalize_name(&params.name)?;
    Ok(())
}

fn validate_probe_params(params: &LocalModelProbeParams) -> Result<(), ErrorData> {
    normalize_name(&params.name)?;
    validate_timeout(params.timeout_ms)?;
    Ok(())
}

fn normalize_name(value: &str) -> Result<String, ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid("local model name must not be empty"));
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(invalid(format!(
            "local model name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err(invalid(
            "local model name must not contain control characters",
        ));
    }
    Ok(trimmed.to_owned())
}

fn normalize_base_url(value: &str) -> Result<String, ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid("base_url must not be empty"));
    }
    if trimmed.chars().count() > MAX_BASE_URL_CHARS {
        return Err(invalid(format!(
            "base_url must be at most {MAX_BASE_URL_CHARS} characters"
        )));
    }
    Ok(trimmed.to_owned())
}

fn normalize_model_id(value: &str) -> Result<String, ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid("model_id must not be empty"));
    }
    if trimmed.chars().count() > MAX_MODEL_ID_CHARS {
        return Err(invalid(format!(
            "model_id must be at most {MAX_MODEL_ID_CHARS} characters"
        )));
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err(invalid("model_id must not contain control characters"));
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional_text(
    value: Option<String>,
    field: &str,
    max_chars: usize,
) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > max_chars {
        return Err(invalid(format!(
            "{field} must be at most {max_chars} characters"
        )));
    }
    Ok(Some(trimmed.to_owned()))
}

fn normalize_optional_env_var(value: Option<String>) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_ENV_VAR_CHARS {
        return Err(invalid(format!(
            "api_key_env_var must be at most {MAX_ENV_VAR_CHARS} characters"
        )));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(invalid(
            "api_key_env_var must contain only ASCII letters, digits, or underscore",
        ));
    }
    Ok(Some(trimmed.to_owned()))
}

fn validate_optional_u32(value: Option<u32>, field: &str) -> Result<(), ErrorData> {
    if value == Some(0) {
        return Err(invalid(format!("{field} must be positive when provided")));
    }
    Ok(())
}

fn validate_timeout(timeout_ms: Option<u64>) -> Result<(), ErrorData> {
    if let Some(timeout_ms) = timeout_ms
        && (timeout_ms == 0 || timeout_ms > MAX_PROBE_TIMEOUT_MS)
    {
        return Err(invalid(format!(
            "probe timeout_ms must be between 1 and {MAX_PROBE_TIMEOUT_MS}"
        )));
    }
    Ok(())
}

fn validate_limit(limit: usize) -> Result<(), ErrorData> {
    if limit == 0 || limit > MAX_LIMIT {
        return Err(invalid(format!(
            "local_model_list limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    Ok(())
}

fn probe_error_with_evidence(
    probe: &LocalModelProbeReport,
    evidence: Option<&LocalModelStorageReadback>,
) -> ErrorData {
    let code = probe
        .error_code
        .as_deref()
        .unwrap_or(error_codes::MODEL_TOOLS_UNSUPPORTED);
    let detail = probe
        .error_detail
        .clone()
        .unwrap_or_else(|| "local model tool-call probe failed".to_owned());
    let mut data = json!({
        "code": code,
        "probe": probe,
    });
    if let Some(evidence) = evidence
        && let Some(object) = data.as_object_mut()
    {
        object.insert("evidence_readback".to_owned(), json!(evidence));
    }
    ErrorData::new(ErrorCode(-32099), detail, Some(data))
}

fn model_registry_error(
    code: &'static str,
    message: impl Into<String>,
    extra: serde_json::Value,
) -> ErrorData {
    let mut data = json!({ "code": code });
    if let (Some(data), Some(extra)) = (data.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            data.insert(key.clone(), value.clone());
        }
    }
    ErrorData::new(ErrorCode(-32099), message.into(), Some(data))
}

fn storage_error(error: synapse_storage::StorageError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

fn invalid(message: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, message.into())
}

fn encode_json_row<T: Serialize>(value: &T) -> Result<Vec<u8>, ErrorData> {
    serde_json::to_vec(value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("encode local model registry row failed: {error}"),
        )
    })
}

fn default_enabled() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

fn unix_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn tokens_per_second(tokens: Option<u64>, latency_ms: u64) -> Option<f64> {
    let tokens = tokens?;
    if tokens == 0 || latency_ms == 0 {
        return None;
    }
    Some(tokens as f64 / (latency_ms as f64 / 1000.0))
}

fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut iter = value.chars();
    let excerpt = iter.by_ref().take(max_chars).collect::<String>();
    let truncated = iter.next().is_some();
    (excerpt, truncated)
}

fn sha256_bytes(value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    let digest = hasher.finalize();
    hex_lower(&digest)
}

fn hex_lower(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_code(error: &ErrorData) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str)
    }

    fn temp_db() -> anyhow::Result<(tempfile::TempDir, Arc<Db>)> {
        let dir = tempfile::tempdir()?;
        let db = Arc::new(Db::open(dir.path(), SCHEMA_VERSION)?);
        Ok((dir, db))
    }

    fn healthy_response(nonce: &str) -> String {
        json!({
            "choices": [
                {
                    "message": {
                        "tool_calls": [
                            {
                                "type": "function",
                                "function": {
                                    "name": PROBE_TOOL_NAME,
                                    "arguments": json!({"nonce": nonce}).to_string()
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 3,
                "total_tokens": 12
            }
        })
        .to_string()
    }

    fn env_probe_row(prefix: &str) -> Option<LocalModelRegistryRow> {
        let base_url = match std::env::var(format!("{prefix}_BASE_URL")) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                println!("skip={prefix}_BASE_URL not set; no real local endpoint configured");
                return None;
            }
        };
        let model_id = match std::env::var(format!("{prefix}_MODEL_ID")) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                println!("skip={prefix}_MODEL_ID not set; no real local endpoint configured");
                return None;
            }
        };
        let api_key_env_var = match std::env::var(format!("{prefix}_API_KEY_ENV_VAR")) {
            Ok(value) => {
                normalize_optional_env_var(Some(value)).expect("valid api key env var name")
            }
            Err(_) => None,
        };
        let allow_non_loopback = std::env::var(format!("{prefix}_ALLOW_NON_LOOPBACK"))
            .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);
        Some(LocalModelRegistryRow {
            schema_version: SCHEMA_VERSION,
            row_key: "env-probe".to_owned(),
            name: prefix.to_ascii_lowercase(),
            base_url,
            model_id,
            api_shape: LocalModelApiShape::OpenAiChatCompletions,
            runtime_preset: LocalModelRuntimePreset::OpenAiCompatible,
            context_length: None,
            max_tools: None,
            notes: None,
            enabled: true,
            allow_non_loopback,
            api_key_env_var,
            created_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            created_by_session: "test".to_owned(),
            updated_by_session: "test".to_owned(),
            last_probe: None,
        })
    }

    #[test]
    fn tool_call_probe_parser_accepts_forced_structured_call() -> anyhow::Result<()> {
        let nonce = "probe-known";
        println!("readback=local_model_probe_parser before=nonce:{nonce}");
        let usage = validate_tool_call_response(&healthy_response(nonce), nonce)
            .map_err(|error| anyhow::anyhow!(error))?;
        println!(
            "readback=local_model_probe_parser after=prompt:{:?} completion:{:?} total:{:?}",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        );
        assert_eq!(usage.prompt_tokens, Some(9));
        assert_eq!(usage.completion_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(12));
        Ok(())
    }

    #[test]
    fn tool_call_probe_parser_rejects_plain_text_response() {
        println!("readback=local_model_probe_parser_plain before=raw:plain-text");
        let result = validate_tool_call_response(
            r#"{"choices":[{"message":{"content":"hello"}}]}"#,
            "probe-known",
        );
        println!("readback=local_model_probe_parser_plain after={result:?}");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn probe_reports_missing_api_key_before_http_request() -> anyhow::Result<()> {
        let env_var = "SYNAPSE_TEST_LOCAL_MODEL_PROBE_MISSING_KEY";
        unsafe { std::env::remove_var(env_var) };
        let row_key = registry_row_key("deepseek-missing-key")?;
        let row = LocalModelRegistryRow {
            schema_version: SCHEMA_VERSION,
            row_key,
            name: "deepseek-missing-key".to_owned(),
            base_url: "https://api.deepseek.com".to_owned(),
            model_id: "deepseek-v4-flash".to_owned(),
            api_shape: LocalModelApiShape::OpenAiChatCompletions,
            runtime_preset: LocalModelRuntimePreset::DeepSeekV4FlashNonThinking,
            context_length: Some(1_000_000),
            max_tools: Some(128),
            notes: None,
            enabled: true,
            allow_non_loopback: true,
            api_key_env_var: Some(env_var.to_owned()),
            created_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            created_by_session: "test".to_owned(),
            updated_by_session: "test".to_owned(),
            last_probe: None,
        };
        println!("readback=local_model_probe_missing_key before=env:{env_var}:missing");
        let probe = probe_row(&row, Some(1000)).await;
        println!(
            "readback=local_model_probe_missing_key after=healthy:{} code:{:?} detail:{:?}",
            probe.healthy, probe.error_code, probe.error_detail
        );
        assert!(!probe.healthy);
        assert_eq!(
            probe.error_code.as_deref(),
            Some(error_codes::MODEL_API_KEY_MISSING)
        );
        assert!(
            probe
                .error_detail
                .as_deref()
                .unwrap_or_default()
                .contains(env_var)
        );
        assert!(probe.raw_response_sha256.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn real_tool_endpoint_probe_from_env_or_loud_skip() {
        let Some(row) = env_probe_row("SYNAPSE_LOCAL_MODEL_TOOL_PROBE") else {
            return;
        };
        let timeout_ms = std::env::var("SYNAPSE_LOCAL_MODEL_TOOL_PROBE_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PROBE_TIMEOUT_MS);
        println!(
            "readback=local_model_tool_env_probe before=endpoint:{} model:{}",
            row.base_url, row.model_id
        );
        let probe = probe_row(&row, Some(timeout_ms)).await;
        println!(
            "readback=local_model_tool_env_probe after=healthy:{} code:{:?} detail:{:?}",
            probe.healthy, probe.error_code, probe.error_detail
        );
        assert!(
            probe.healthy,
            "real tool-capable endpoint probe failed: {probe:?}"
        );
    }

    #[tokio::test]
    async fn real_non_tool_endpoint_probe_from_env_or_loud_skip() {
        let Some(row) = env_probe_row("SYNAPSE_LOCAL_MODEL_NON_TOOL_PROBE") else {
            return;
        };
        let timeout_ms = std::env::var("SYNAPSE_LOCAL_MODEL_NON_TOOL_PROBE_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PROBE_TIMEOUT_MS);
        println!(
            "readback=local_model_non_tool_env_probe before=endpoint:{} model:{}",
            row.base_url, row.model_id
        );
        let probe = probe_row(&row, Some(timeout_ms)).await;
        println!(
            "readback=local_model_non_tool_env_probe after=healthy:{} code:{:?} detail:{:?}",
            probe.healthy, probe.error_code, probe.error_detail
        );
        assert!(
            !probe.healthy,
            "non-tool endpoint unexpectedly passed: {probe:?}"
        );
        assert_eq!(
            probe.error_code.as_deref(),
            Some(error_codes::MODEL_TOOLS_UNSUPPORTED)
        );
    }

    #[test]
    fn registry_rows_round_trip_and_remove_from_physical_cf() -> anyhow::Result<()> {
        let (_dir, db) = temp_db()?;
        let row_key = registry_row_key("issue932-local")?;
        println!("readback=local_model_registry before=row_key:{row_key}");
        let row = LocalModelRegistryRow {
            schema_version: SCHEMA_VERSION,
            row_key: row_key.clone(),
            name: "issue932-local".to_owned(),
            base_url: "http://127.0.0.1:1234/v1".to_owned(),
            model_id: "tool-model".to_owned(),
            api_shape: LocalModelApiShape::OpenAiChatCompletions,
            runtime_preset: LocalModelRuntimePreset::OpenAiCompatible,
            context_length: Some(8192),
            max_tools: Some(32),
            notes: Some("known synthetic row".to_owned()),
            enabled: true,
            allow_non_loopback: false,
            api_key_env_var: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            created_by_session: "test".to_owned(),
            updated_by_session: "test".to_owned(),
            last_probe: Some(probe_report_success(
                1,
                "http://127.0.0.1:1234/v1/chat/completions",
                Duration::from_millis(25),
                ProbeUsage {
                    prompt_tokens: Some(1),
                    completion_tokens: Some(2),
                    total_tokens: Some(3),
                },
                &healthy_response("nonce"),
            )),
        };
        let encoded = encode_json_row(&row)?;
        db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])?;
        let listed = list_local_models(
            &db,
            &LocalModelListParams {
                name: None,
                include_disabled: true,
                limit: DEFAULT_LIMIT,
            },
        )?;
        println!(
            "readback=local_model_registry after_list=count:{} first_key:{}",
            listed.rows.len(),
            listed.rows[0].row_key
        );
        assert_eq!(listed.rows.len(), 1);
        assert_eq!(listed.rows[0].name, "issue932-local");

        let removed = remove_local_model(
            &db,
            &LocalModelRemoveParams {
                name: "issue932-local".to_owned(),
            },
        )?;
        println!(
            "readback=local_model_registry after_remove=after_present:{} removed_key:{}",
            removed.after_row_present, removed.removed_readback.row_key
        );
        assert!(!removed.after_row_present);
        Ok(())
    }

    #[test]
    fn registry_validates_loopback_and_name_edges() {
        let blank = registry_row_key("   ");
        println!("readback=local_model_registry_blank after={blank:?}");
        assert_eq!(
            error_code(&blank.unwrap_err()),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );

        let row = LocalModelRegistryRow {
            schema_version: SCHEMA_VERSION,
            row_key: "unused".to_owned(),
            name: "remote".to_owned(),
            base_url: "http://192.0.2.10:1234/v1".to_owned(),
            model_id: "m".to_owned(),
            api_shape: LocalModelApiShape::OpenAiChatCompletions,
            runtime_preset: LocalModelRuntimePreset::OpenAiCompatible,
            context_length: None,
            max_tools: None,
            notes: None,
            enabled: true,
            allow_non_loopback: false,
            api_key_env_var: None,
            created_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            created_by_session: "test".to_owned(),
            updated_by_session: "test".to_owned(),
            last_probe: None,
        };
        let remote = chat_completions_endpoint(&row);
        println!("readback=local_model_registry_remote after={remote:?}");
        assert_eq!(
            error_code(&remote.unwrap_err()),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }
}

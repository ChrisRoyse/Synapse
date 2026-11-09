use std::{
    error::Error as StdError,
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
/// CF_KV prefix for DPAPI-encrypted API-key secrets, keyed by model name. The
/// value is ciphertext only — the plaintext key is never persisted.
const SECRET_PREFIX: &str = "local_model_secret/v1/name_hex/";
const MAX_API_KEY_CHARS: usize = 8192;
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
const ACT_CALL_TOOL_NAME: &str = "act_call";

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
    #[serde(
        rename = "deepseek_v4_flash_non_thinking",
        alias = "deep_seek_v4_flash_non_thinking"
    )]
    DeepSeekV4FlashNonThinking,
    #[serde(rename = "deepseek_v4_reasoning", alias = "deep_seek_v4_reasoning")]
    DeepSeekV4Reasoning,
    /// The model has the full Synapse tool surface internalized in its weights
    /// (a tool-internalization LoRA): it emits structured tool calls from a
    /// near-empty prompt. The harness must inject NO tool catalog (that ~48k
    /// token catalog is the context poison internalization exists to remove),
    /// and the registration probe must not depend on a tool definition being
    /// sent. Tool-call routing of the response is unchanged.
    #[serde(rename = "internalized_no_catalog")]
    InternalizedNoCatalog,
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
    /// Plaintext API key. When present and non-empty it is encrypted with
    /// Windows DPAPI (CurrentUser) and stored at rest; the plaintext is never
    /// persisted or echoed back. Requires `api_key_env_var` to name the
    /// environment variable the key is injected under at spawn/probe time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
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
    /// Plaintext API key to (re)store, DPAPI-encrypted at rest. Empty/whitespace
    /// is rejected; use `clear_api_key` to remove a stored secret instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Delete the stored encrypted API key for this model.
    #[serde(default)]
    pub clear_api_key: bool,
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
    /// Whether an encrypted API key is stored for this model in the DPAPI
    /// secret store. Computed at read time from the secret CF_KV rows — the
    /// secret store is the source of truth, not this persisted snapshot (the
    /// stored row always serializes `false`). Never carries the key itself.
    #[serde(default)]
    pub has_api_key_secret: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
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
        has_api_key_secret: false,
    };

    // Persist the encrypted key (if supplied) BEFORE probing so the probe
    // authenticates with the real credential. On probe failure we remove the
    // orphan secret so a rejected registration leaves no stored key behind.
    if let Some(api_key) = params.api_key.as_deref() {
        if row.api_key_env_var.is_none() {
            return Err(invalid(
                "api_key requires api_key_env_var to name the environment variable the key is injected under at spawn/probe time",
            ));
        }
        put_model_secret(db, &row.name, api_key, by_session)?;
    }

    let probe = probe_row(db, &row, params.probe_timeout_ms).await;
    row.last_probe = Some(probe.clone());
    if !probe.healthy {
        if params.api_key.is_some() {
            let _ = delete_model_secret(db, &row.name);
        }
        let evidence = write_probe_evidence(db, &row, &probe, "register_rejected", by_session)?;
        return Err(probe_error_with_evidence(&probe, Some(&evidence)));
    }

    let encoded = encode_json_row(&row)?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
        .map_err(storage_error)?;
    let storage_readback = readback_exact_row(db, &row_key)?;
    row.has_api_key_secret = model_secret_present(db, &row.name)?;
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
    let mut rows = rows
        .into_iter()
        .filter(|row| params.include_disabled || row.enabled)
        .take(params.limit)
        .collect::<Vec<_>>();
    for row in &mut rows {
        row.has_api_key_secret = model_secret_present(db, &row.name)?;
    }
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

    // Secret-store mutation. The secret is keyed by model name, so a rename
    // carries it to the new key. We snapshot the affected secret rows so a
    // failed probe can be rolled back to leave no partial credential state.
    let old_name = before_row.name.clone();
    let new_name = row.name.clone();
    let old_secret_key = secret_row_key(&old_name)?;
    let new_secret_key = secret_row_key(&new_name)?;
    let snap_old = scan_exact_row(db, &old_secret_key)?.map(|(_, value)| value);
    let snap_new = if new_secret_key == old_secret_key {
        snap_old.clone()
    } else {
        scan_exact_row(db, &new_secret_key)?.map(|(_, value)| value)
    };

    if params.api_key.is_some() && params.clear_api_key {
        return Err(invalid("api_key and clear_api_key are mutually exclusive"));
    }
    if params.api_key.is_some() && row.api_key_env_var.is_none() {
        return Err(invalid(
            "api_key requires api_key_env_var to name the environment variable the key is injected under",
        ));
    }
    let mut secret_mutated = false;
    if params.clear_api_key {
        delete_model_secret(db, &old_name)?;
        if new_secret_key != old_secret_key {
            delete_model_secret(db, &new_name)?;
        }
        secret_mutated = true;
        probe_required = true;
    } else if let Some(api_key) = params.api_key.as_deref() {
        put_model_secret(db, &new_name, api_key, by_session)?;
        if new_secret_key != old_secret_key {
            delete_model_secret(db, &old_name)?;
        }
        secret_mutated = true;
        probe_required = true;
    } else if new_secret_key != old_secret_key {
        // Pure rename: move any existing secret to the new name key.
        if let Some(secret) = read_model_secret(db, &old_name)? {
            put_model_secret(db, &new_name, &secret, by_session)?;
            delete_model_secret(db, &old_name)?;
            secret_mutated = true;
        }
    }

    let rollback_secret = |db: &Arc<Db>| -> Result<(), ErrorData> {
        if secret_mutated {
            restore_secret_raw(db, &old_secret_key, snap_old.clone())?;
            if new_secret_key != old_secret_key {
                restore_secret_raw(db, &new_secret_key, snap_new.clone())?;
            }
        }
        Ok(())
    };

    let probe = if probe_required {
        let probe = probe_row(db, &row, params.probe_timeout_ms).await;
        row.last_probe = Some(probe.clone());
        if !probe.healthy {
            rollback_secret(db)?;
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
    if let Err(error) = db.mutate_batch_pressure_bypass(
        cf::CF_KV,
        deletes,
        [(row.row_key.as_bytes().to_vec(), encoded)],
    ) {
        // The row write failed after the secret store was already mutated; undo
        // the secret change so the two stay consistent.
        rollback_secret(db)?;
        return Err(storage_error(error));
    }
    let storage_readback = readback_exact_row(db, &row.row_key)?;
    row.has_api_key_secret = model_secret_present(db, &row.name)?;
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
    // Remove any stored encrypted API key so a removed model leaves no orphan
    // credential at rest.
    delete_model_secret(db, &params.name)?;
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
    let probe = probe_row(db, &row, params.timeout_ms).await;
    row.last_probe = Some(probe.clone());
    row.updated_at_unix_ms = unix_time_ms_now();
    row.updated_by_session = by_session.to_owned();
    let encoded = encode_json_row(&row)?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(row.row_key.as_bytes().to_vec(), encoded)])
        .map_err(storage_error)?;
    let storage_readback = readback_exact_row(db, &row.row_key)?;
    row.has_api_key_secret = model_secret_present(db, &row.name)?;
    Ok(LocalModelProbeResponse {
        row,
        probe,
        storage_readback,
    })
}

pub fn local_model_snapshot(db: &Arc<Db>) -> Result<Vec<LocalModelRegistryRow>, ErrorData> {
    let (mut rows, _corrupt_rows) = scan_model_rows(db)?;
    for row in &mut rows {
        row.has_api_key_secret = model_secret_present(db, &row.name)?;
    }
    Ok(rows)
}

/// CF_KV row holding the DPAPI-encrypted API key for one model. The ciphertext
/// is opaque and bound to the current Windows user; the plaintext never lands
/// here.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalModelSecretRow {
    schema_version: u32,
    row_key: String,
    name: String,
    /// Hex-encoded DPAPI (CurrentUser) ciphertext of the API key.
    ciphertext_hex: String,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    updated_by_session: String,
}

/// How an API key was resolved for a probe or spawn.
pub enum ResolvedApiKey {
    /// The model has no `api_key_env_var`; no bearer auth is used.
    NotRequired,
    /// A non-empty key was found. `source` records where (audit-friendly).
    Resolved {
        env_var: String,
        value: String,
        source: &'static str,
    },
}

fn secret_row_key(name: &str) -> Result<String, ErrorData> {
    let name = normalize_name(name)?;
    Ok(format!("{SECRET_PREFIX}{}", hex_lower(name.as_bytes())))
}

fn read_secret_row_optional(
    db: &Arc<Db>,
    row_key: &str,
) -> Result<Option<LocalModelSecretRow>, ErrorData> {
    match scan_exact_row(db, row_key)? {
        Some((_key, value)) => serde_json::from_slice(&value)
            .map(Some)
            .map_err(|error| invalid(format!("decode local model secret row failed: {error}"))),
        None => Ok(None),
    }
}

/// Returns whether an encrypted API key is stored for `name` without decrypting
/// it. Used to surface `has_api_key_secret` to the dashboard.
pub fn model_secret_present(db: &Arc<Db>, name: &str) -> Result<bool, ErrorData> {
    Ok(read_secret_row_optional(db, &secret_row_key(name)?)?.is_some())
}

/// Restores a secret row to an exact prior state (the raw stored bytes, or
/// absent). Used to roll back the secret store if an update's probe fails after
/// the secret was already mutated, so a rejected update leaves no partial state.
fn restore_secret_raw(
    db: &Arc<Db>,
    row_key: &str,
    snapshot: Option<Vec<u8>>,
) -> Result<(), ErrorData> {
    match snapshot {
        Some(bytes) => db
            .put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), bytes)])
            .map_err(storage_error)?,
        None => db
            .delete_batch(cf::CF_KV, [row_key.as_bytes().to_vec()])
            .map_err(storage_error)?,
    }
    db.flush().map_err(storage_error)
}

/// Encrypts `plaintext` with DPAPI and stores it keyed by model name,
/// overwriting any prior secret. Flushes so the credential is durable before
/// returning (read-after-write correctness for an immediate spawn/probe).
pub fn put_model_secret(
    db: &Arc<Db>,
    name: &str,
    plaintext: &str,
    by_session: &str,
) -> Result<(), ErrorData> {
    let trimmed = plaintext.trim();
    if trimmed.is_empty() {
        return Err(invalid("api_key must not be empty or whitespace"));
    }
    if plaintext.chars().count() > MAX_API_KEY_CHARS {
        return Err(invalid(format!(
            "api_key exceeds {MAX_API_KEY_CHARS} characters"
        )));
    }
    let row_key = secret_row_key(name)?;
    let ciphertext = crate::secret_crypto::protect(plaintext.as_bytes()).map_err(|error| {
        model_registry_error(
            error_codes::MODEL_API_KEY_STORE_FAILED,
            format!("failed to DPAPI-encrypt API key for {name:?}: {error}"),
            json!({ "model": name }),
        )
    })?;
    let now = unix_time_ms_now();
    let created_at_unix_ms =
        read_secret_row_optional(db, &row_key)?.map_or(now, |existing| existing.created_at_unix_ms);
    let row = LocalModelSecretRow {
        schema_version: SCHEMA_VERSION,
        row_key: row_key.clone(),
        name: normalize_name(name)?,
        ciphertext_hex: hex_lower(&ciphertext),
        created_at_unix_ms,
        updated_at_unix_ms: now,
        updated_by_session: by_session.to_owned(),
    };
    let encoded = encode_json_row(&row)?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
        .map_err(storage_error)?;
    db.flush().map_err(storage_error)?;
    Ok(())
}

/// Deletes the stored encrypted API key for `name`. Returns whether a secret
/// was present. Flushes so the deletion is durable.
pub fn delete_model_secret(db: &Arc<Db>, name: &str) -> Result<bool, ErrorData> {
    let row_key = secret_row_key(name)?;
    let present = read_secret_row_optional(db, &row_key)?.is_some();
    if present {
        db.delete_batch(cf::CF_KV, [row_key.as_bytes().to_vec()])
            .map_err(storage_error)?;
        db.flush().map_err(storage_error)?;
    }
    Ok(present)
}

/// Decrypts and returns the stored API key for `name`, or `None` if no secret
/// is stored. A decrypt failure is surfaced loudly (wrong user / copied DB /
/// tampered bytes), never swallowed.
pub fn read_model_secret(db: &Arc<Db>, name: &str) -> Result<Option<String>, ErrorData> {
    let Some(row) = read_secret_row_optional(db, &secret_row_key(name)?)? else {
        return Ok(None);
    };
    let ciphertext = hex_decode(&row.ciphertext_hex)?;
    let plaintext = crate::secret_crypto::unprotect(&ciphertext).map_err(|error| {
        model_registry_error(
            error_codes::MODEL_API_KEY_DECRYPT_FAILED,
            format!("failed to DPAPI-decrypt stored API key for {name:?}: {error}"),
            json!({
                "model": name,
                "remediation": "re-enter the API key on this Windows user account; DPAPI ciphertext is bound to the user that stored it and cannot move between accounts or machines",
            }),
        )
    })?;
    let value = String::from_utf8(plaintext)
        .map_err(|error| invalid(format!("decrypted API key was not valid UTF-8: {error}")))?;
    Ok(Some(value))
}

/// Resolves the API key for `row`: the encrypted secret store first, then the
/// process environment, else a loud `MODEL_API_KEY_MISSING` naming both sources
/// checked. This is the single resolution point shared by probe and spawn.
pub fn resolve_local_model_api_key(
    db: &Arc<Db>,
    row: &LocalModelRegistryRow,
) -> Result<ResolvedApiKey, ErrorData> {
    let Some(env_var) = row.api_key_env_var.as_deref() else {
        return Ok(ResolvedApiKey::NotRequired);
    };
    if let Some(secret) = read_model_secret(db, &row.name)? {
        if !secret.trim().is_empty() {
            return Ok(ResolvedApiKey::Resolved {
                env_var: env_var.to_owned(),
                value: secret,
                source: "dpapi_secret_store",
            });
        }
    }
    if let Ok(value) = std::env::var(env_var) {
        if !value.trim().is_empty() {
            return Ok(ResolvedApiKey::Resolved {
                env_var: env_var.to_owned(),
                value,
                source: "process_env",
            });
        }
    }
    Err(model_registry_error(
        error_codes::MODEL_API_KEY_MISSING,
        format!(
            "no API key available for local model {:?}: neither the encrypted secret store nor the process environment variable {env_var:?} held a non-empty value",
            row.name
        ),
        json!({
            "model": row.name,
            "api_key_env_var": env_var,
            "sources_checked": ["dpapi_secret_store", "process_env"],
            "remediation": "store a key via local_model_update { name, api_key } or the dashboard Add/Edit API Model form, or set the environment variable before launching the daemon",
            "source_of_truth": cf::CF_KV,
        }),
    ))
}

fn reqwest_probe_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_builder() {
        "client_builder"
    } else if error.is_request() {
        "request"
    } else if error.is_redirect() {
        "redirect"
    } else if error.status().is_some() {
        "http_status"
    } else {
        "reqwest"
    }
}

fn reqwest_probe_error_detail(
    phase: &'static str,
    endpoint: &Url,
    timeout_ms: u64,
    error: &reqwest::Error,
) -> String {
    let kind = reqwest_probe_error_kind(error);
    let mut source_chain = vec![error.to_string()];
    let mut source: Option<&(dyn StdError + 'static)> = error.source();
    while let Some(next) = source {
        source_chain.push(next.to_string());
        source = next.source();
    }
    format!(
        "local model probe HTTP failure: phase={phase} kind={kind} endpoint={} timeout_ms={} is_connect={} is_timeout={} is_body={} source_chain={}",
        endpoint.as_str(),
        timeout_ms,
        error.is_connect(),
        error.is_timeout(),
        error.is_body(),
        source_chain.join(" -> ")
    )
}

async fn probe_row(
    db: &Arc<Db>,
    row: &LocalModelRegistryRow,
    timeout_ms: Option<u64>,
) -> LocalModelProbeReport {
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
                "build_endpoint",
                "invalid_url",
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
            "validate_timeout",
            "out_of_range",
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
                "build_client",
                reqwest_probe_error_kind(&error),
                reqwest_probe_error_detail("build_client", &endpoint, timeout_ms, &error),
                None,
            );
        }
    };

    let nonce = format!("probe-{}", Uuid::now_v7().simple());
    let internalized = matches!(
        row.runtime_preset,
        LocalModelRuntimePreset::InternalizedNoCatalog
    );
    let mut body = if internalized {
        internalized_probe_request(&row.model_id, &nonce)
    } else {
        probe_request(&row.model_id, &nonce)
    };
    apply_runtime_preset(row, &mut body);
    let mut request = client.post(endpoint.clone()).json(&body);
    match resolve_local_model_api_key(db, row) {
        Ok(ResolvedApiKey::NotRequired) => {}
        Ok(ResolvedApiKey::Resolved { value, .. }) => {
            request = request.bearer_auth(value);
        }
        Err(error) => {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str);
            let code = match code {
                Some(error_codes::MODEL_API_KEY_DECRYPT_FAILED) => {
                    error_codes::MODEL_API_KEY_DECRYPT_FAILED
                }
                _ => error_codes::MODEL_API_KEY_MISSING,
            };
            return probe_report_error(
                observed_at_unix_ms,
                endpoint.as_str(),
                started.elapsed(),
                code,
                "resolve_api_key",
                "missing_or_unreadable_secret",
                error.message.to_string(),
                None,
            );
        }
    }
    request = request.header(header::ACCEPT, "application/json");

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let kind = reqwest_probe_error_kind(&error);
            let detail = reqwest_probe_error_detail("send", &endpoint, timeout_ms, &error);
            tracing::warn!(
                code = "LOCAL_MODEL_PROBE_HTTP_SEND_FAILED",
                model = %row.name,
                endpoint = %endpoint,
                error_kind = kind,
                timeout_ms,
                error = %error,
                "local model probe HTTP send failed"
            );
            return probe_report_error(
                observed_at_unix_ms,
                endpoint.as_str(),
                started.elapsed(),
                error_codes::MODEL_ENDPOINT_UNREACHABLE,
                "send",
                kind,
                detail,
                None,
            );
        }
    };
    let status = response.status();
    let raw_text = match response.text().await {
        Ok(text) => text,
        Err(error) => {
            let kind = reqwest_probe_error_kind(&error);
            let detail = reqwest_probe_error_detail("read_body", &endpoint, timeout_ms, &error);
            tracing::warn!(
                code = "LOCAL_MODEL_PROBE_HTTP_BODY_READ_FAILED",
                model = %row.name,
                endpoint = %endpoint,
                error_kind = kind,
                timeout_ms,
                error = %error,
                "local model probe HTTP response body read failed"
            );
            return probe_report_error(
                observed_at_unix_ms,
                endpoint.as_str(),
                started.elapsed(),
                error_codes::MODEL_ENDPOINT_UNREACHABLE,
                "read_body",
                kind,
                detail,
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
            "http_status",
            "non_success_status",
            format!(
                "tool-call probe request returned HTTP status {}",
                status.as_u16()
            ),
            Some(&raw_text),
        );
    }

    let validation = if internalized {
        validate_internalized_probe_response(&raw_text, &nonce)
    } else {
        validate_tool_call_response(&raw_text, &nonce)
    };
    match validation {
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
            "validate_tool_call",
            "schema_or_nonce_mismatch",
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
        LocalModelRuntimePreset::InternalizedNoCatalog => {
            // The model has the tool surface in its weights; do not send a tool
            // definition or forced choice. It emits the synapse_probe tool call
            // from the instruction alone, proving structured tool calling works
            // without any catalog. Validation (nonce echo) is unchanged.
            if let Some(object) = body.as_object_mut() {
                object.remove("tools");
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
    let argument_value = required_probe_tool_arguments(tool_calls, PROBE_TOOL_NAME)?;
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

fn required_probe_tool_arguments(
    tool_calls: &[serde_json::Value],
    required_tool_name: &str,
) -> Result<serde_json::Value, String> {
    let mut errors = Vec::new();
    for call in tool_calls {
        match probe_tool_call_arguments(call, required_tool_name) {
            Ok(Some(arguments)) => return Ok(arguments),
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
    }
    let suffix = if errors.is_empty() {
        String::new()
    } else {
        format!("; rejected candidate(s): {}", errors.join("; "))
    };
    Err(format!(
        "probe response did not call required tool {required_tool_name:?}{suffix}"
    ))
}

fn probe_tool_call_arguments(
    call: &serde_json::Value,
    required_tool_name: &str,
) -> Result<Option<serde_json::Value>, String> {
    let function = call
        .get("function")
        .ok_or_else(|| "probe tool call missing function object".to_owned())?;
    let name = function
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "probe tool call missing function.name string".to_owned())?;
    let arguments = function
        .get("arguments")
        .ok_or_else(|| "probe tool call missing function.arguments".to_owned())?;

    if name == required_tool_name {
        return Ok(Some(serde_json::Value::Object(
            parse_probe_argument_object(arguments, "probe tool arguments")?,
        )));
    }

    if name != ACT_CALL_TOOL_NAME {
        return Ok(None);
    }

    let mut wrapper = parse_probe_argument_object(arguments, "act_call arguments")?;
    let nested_name = wrapper
        .remove("tool_name")
        .and_then(|value| value.as_str().map(str::trim).map(ToOwned::to_owned))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "act_call arguments require non-empty tool_name".to_owned())?;
    if nested_name != required_tool_name {
        return Ok(None);
    }
    let delegated = match wrapper
        .remove("arguments")
        .or_else(|| wrapper.remove("args"))
    {
        Some(value) => {
            if !wrapper.is_empty() {
                return Err(
                    "act_call arguments must not mix args/arguments with flattened delegated keys"
                        .to_owned(),
                );
            }
            parse_probe_argument_object(&value, "act_call delegated arguments")?
        }
        None => wrapper,
    };
    Ok(Some(serde_json::Value::Object(delegated)))
}

fn parse_probe_argument_object(
    value: &serde_json::Value,
    context: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match value {
        serde_json::Value::String(raw) => {
            let decoded = serde_json::from_str::<serde_json::Value>(raw)
                .map_err(|error| format!("{context} string is not valid JSON: {error}"))?;
            decoded
                .as_object()
                .cloned()
                .ok_or_else(|| format!("{context} string must decode to a JSON object"))
        }
        serde_json::Value::Object(map) => Ok(map.clone()),
        serde_json::Value::Array(_) => Err(format!(
            "{context} must be a JSON object; positional arrays are ambiguous and rejected fail-closed"
        )),
        other => Err(format!("{context} must be a JSON object, got {other}")),
    }
}

/// The real, read-only Synapse tool an internalized model is asked to call
/// during probing. It carries the full surface in its weights but does NOT know
/// the synthetic `synapse_probe` tool, so we exercise a tool it actually learned
/// and pass the nonce as the key it must echo back.
const INTERNALIZED_PROBE_TOOL: &str = "workspace_get";

/// Probe request for an internalized model: NO tool catalog is sent. The model
/// must emit a structured call to a real read-only tool with the nonce as its
/// key, proving (a) structured tool calling, (b) correct real-tool selection
/// from weights, and (c) nonce echo / argument fidelity.
fn internalized_probe_request(model_id: &str, nonce: &str) -> serde_json::Value {
    json!({
        "model": model_id,
        "messages": [
            {
                "role": "system",
                "content": "Return no prose. Emit exactly one tool call."
            },
            {
                "role": "user",
                "content": format!("Read the workspace blackboard entry whose key is {nonce:?}.")
            }
        ],
        "stream": false,
        "temperature": 0,
        "max_tokens": 128
    })
}

/// Validates an internalized model's probe response: it must call the real
/// `workspace_get` tool with `key` echoing the nonce. No catalog/`synapse_probe`
/// is involved.
fn validate_internalized_probe_response(raw_text: &str, nonce: &str) -> Result<ProbeUsage, String> {
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
        .ok_or_else(|| {
            "internalized probe response missing message.tool_calls array (model emitted no tool call from its weights)".to_owned()
        })?;
    let argument_value = required_probe_tool_arguments(tool_calls, INTERNALIZED_PROBE_TOOL)
        .map_err(|error| {
            format!(
                "internalized probe did not call the real tool {INTERNALIZED_PROBE_TOOL:?}: {error}"
            )
        })?;
    let actual_key = argument_value
        .get("key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "internalized probe tool arguments missing key string".to_owned())?;
    if actual_key != nonce {
        return Err(format!(
            "internalized probe nonce mismatch: expected key {nonce:?}, got {actual_key:?}"
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
        error_phase: None,
        error_kind: None,
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
    error_phase: &'static str,
    error_kind: &'static str,
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
        error_phase: Some(error_phase.to_owned()),
        error_kind: Some(error_kind.to_owned()),
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

fn hex_decode(value: &str) -> Result<Vec<u8>, ErrorData> {
    if !value.len().is_multiple_of(2) {
        return Err(invalid("stored ciphertext hex has an odd length"));
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|error| invalid(format!("stored ciphertext hex is invalid: {error}")))
        })
        .collect()
}

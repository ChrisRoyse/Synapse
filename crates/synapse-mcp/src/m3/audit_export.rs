use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::Utc;
use regex::Regex;
use rmcp::{ErrorData, model::ErrorCode, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use synapse_core::{ProfileId, SCHEMA_VERSION, error_codes};
use synapse_reflex::ReflexRuntime;
use synapse_storage::{cf, decode_json, encode_json};

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

const CONSENT_PREFIX: &str = "audit_export/v1/consent/";
const DEFAULT_MAX_EXPORT_ROWS: u32 = 100;
const MAX_EXPORT_ROWS: u32 = 1_000;
const DEFAULT_MAX_ROW_BYTES: u64 = 64 * 1024;
const MAX_ROW_BYTES: u64 = 512 * 1024;
const STRICT_POLICY: &str = "strict";
const BUNDLE_KIND: &str = "audit_export_bundle";
const RETAINED_FIELD_PATHS: &[&str] = &[
    "schema_version",
    "row_kind",
    "profile_id",
    "profile_version",
    "profile_schema_version",
    "active_profile_id",
    "active_profile_schema_version",
    "tool",
    "verb",
    "phase",
    "channel",
    "status",
    "outcome",
    "error_code",
    "redacted",
    "audit_context/profile_id",
    "audit_context/profile_version",
    "audit_context/profile_schema_version",
    "actor/channel",
    "actor/tool",
    "actor/profile_id",
    "actor/profile_version",
    "actor/profile_schema_version",
    "foreground/profile_id",
    "foreground/profile_schema_version",
    "foreground/process_name",
    "human_os_foreground/profile_id",
    "human_os_foreground/profile_schema_version",
    "human_os_foreground/process_name",
    "foreground_tier/required_foreground",
    "foreground_tier/backend_tier",
    "foreground_tier/session_foreground_policy",
    "foreground_tier/policy_allows_foreground",
    "foreground_tier/foreground_policy_violation",
    "foreground_tier/allowed",
    "source_of_truth/cf_name",
    "source_of_truth/row_kind",
    "source_of_truth/retention",
    "details/response/backend",
];
const TRAVERSABLE_CONTAINER_PATHS: &[&str] = &[
    "actor",
    "audit_context",
    "audit_context/app_context",
    "audit_context/backend_policy",
    "before",
    "after",
    "details",
    "details/response",
    "error",
    "foreground",
    "foreground_tier",
    "human_os_foreground",
    "payload_bounded",
    "source_of_truth",
    "target",
];

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditExportConsentParams {
    pub enabled: bool,
    #[serde(default = "default_redaction_policy")]
    #[schemars(default = "default_redaction_policy")]
    pub redaction_policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditExportBundleParams {
    pub profile_id: ProfileId,
    pub output_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent: Option<AuditExportConsentParams>,
    #[serde(default = "default_max_export_rows")]
    #[schemars(default = "default_max_export_rows", range(min = 1, max = 1000))]
    pub max_rows: u32,
    #[serde(default = "default_max_row_bytes")]
    #[schemars(default = "default_max_row_bytes", range(min = 1, max = 524_288))]
    pub max_row_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditExportBundleResponse {
    pub profile_id: ProfileId,
    pub output_dir: String,
    pub manifest_path: String,
    pub rows_path: String,
    pub redaction_report_path: String,
    pub consent_key: String,
    pub redaction_policy: String,
    pub rows_scanned: u64,
    pub rows_exported: u64,
    pub redacted_fields: u64,
    pub manifest_sha256: String,
    pub rows_sha256: String,
    pub redaction_report_sha256: String,
    pub consent_row: AuditExportStoredRow,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditExportStoredRow {
    pub cf_name: String,
    pub key: String,
    pub key_hex: String,
    pub value: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct ConsentRow {
    schema_version: u32,
    row_kind: &'static str,
    row_id: String,
    created_at: String,
    updated_at: String,
    profile_id: String,
    state: &'static str,
    enabled: bool,
    redaction_policy: String,
    allowed_redaction_policies: Vec<String>,
    external_sharing_allowed: bool,
    operator_note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditExportManifest {
    schema_version: u32,
    kind: &'static str,
    exported_at: String,
    profile_id: String,
    source_cf_name: &'static str,
    consent_key: String,
    consent_sha256: String,
    redaction_policy: String,
    rows_scanned: u64,
    rows_exported: u64,
    external_sharing_allowed: bool,
    files: AuditExportBundleFiles,
    row_hashes: Vec<AuditExportRowHash>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditExportBundleFiles {
    rows_json: String,
    rows_sha256: String,
    redaction_report_json: String,
    redaction_report_sha256: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditExportRowHash {
    key_hex: String,
    redacted_value_sha256: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditExportRow {
    cf_name: &'static str,
    key_hex: String,
    value: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditExportRedactionReport {
    schema_version: u32,
    policy: String,
    rows_scanned: u64,
    rows_exported: u64,
    rows_redacted: u64,
    fields_redacted: u64,
    redacted_by_class: BTreeMap<String, u64>,
    retained_fields: Vec<String>,
    fail_closed_rules: Vec<String>,
}

#[derive(Default)]
struct RedactionStats {
    rows_redacted: u64,
    fields_redacted: u64,
    redacted_by_class: BTreeMap<String, u64>,
}

#[must_use]
pub const fn audit_export_bundle() -> M3ToolStub {
    M3ToolStub::new("audit_export_bundle")
}

#[must_use]
pub fn required_permissions_bundle(_params: &AuditExportBundleParams) -> RequiredPermissions {
    required([
        Permission::ReadProfile,
        Permission::ReadStorage,
        Permission::WriteStorage,
    ])
}

#[expect(
    clippy::too_many_lines,
    reason = "bundle export keeps consent validation, redaction, hashing, and file write order visible"
)]
pub fn export_audit_bundle(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &AuditExportBundleParams,
) -> Result<AuditExportBundleResponse, ErrorData> {
    validate_export_params(params)?;
    let (explicit_consent, redaction_policy) = explicit_export_consent(params)?;

    let output_dir = required_output_dir(&params.output_path)?;
    let consent_key = consent_key(&params.profile_id);
    let runtime = lock_runtime(reflex_runtime, "exporting redacted audit bundle")?;
    let consent_bytes = write_audit_export_consent_row(
        &runtime,
        &params.profile_id,
        explicit_consent,
        &redaction_policy,
        &consent_key,
    )?;
    let consent = decode_json::<Value>(&consent_bytes).map_err(decode_error)?;
    validate_consent(&consent, &redaction_policy)?;
    let rows = runtime
        .storage_cf_tail_rows(cf::CF_ACTION_LOG, params.max_rows as usize)
        .map_err(storage_error)?;
    drop(runtime);

    let rows_scanned = rows.len() as u64;
    let mut exported_rows = Vec::new();
    let mut row_hashes = Vec::new();
    let mut stats = RedactionStats::default();
    for (key, value) in rows {
        let row = decode_json::<Value>(&value).map_err(decode_error)?;
        if !value_mentions_profile(&row, &params.profile_id) {
            continue;
        }
        if value.len() as u64 > params.max_row_bytes {
            return Err(payload_too_large_error(
                value.len() as u64,
                params.max_row_bytes,
            ));
        }
        let mut row_stats = RedactionStats::default();
        let redacted = redact_value(row, &mut row_stats, "");
        if row_stats.fields_redacted > 0 {
            stats.rows_redacted += 1;
            stats.fields_redacted += row_stats.fields_redacted;
            for (class, count) in row_stats.redacted_by_class {
                *stats.redacted_by_class.entry(class).or_default() += count;
            }
        }
        let key_hex = hex_encode(&key);
        let encoded_redacted = encode_json(&redacted).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("redacted audit row encode failed: {error}"),
            )
        })?;
        row_hashes.push(AuditExportRowHash {
            key_hex: key_hex.clone(),
            redacted_value_sha256: sha256_hex(&encoded_redacted),
        });
        exported_rows.push(AuditExportRow {
            cf_name: cf::CF_ACTION_LOG,
            key_hex,
            value: redacted,
        });
    }

    let report = AuditExportRedactionReport {
        schema_version: SCHEMA_VERSION,
        policy: redaction_policy.to_owned(),
        rows_scanned,
        rows_exported: exported_rows.len() as u64,
        rows_redacted: stats.rows_redacted,
        fields_redacted: stats.fields_redacted,
        redacted_by_class: stats.redacted_by_class,
        retained_fields: retained_fields(),
        fail_closed_rules: fail_closed_rules(),
    };
    let rows_bytes = serde_json::to_vec_pretty(&exported_rows).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("audit export rows encode failed: {error}"),
        )
    })?;
    let report_bytes = serde_json::to_vec_pretty(&report).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("audit export redaction report encode failed: {error}"),
        )
    })?;
    let rows_sha256 = sha256_hex(&rows_bytes);
    let report_sha256 = sha256_hex(&report_bytes);
    let manifest = AuditExportManifest {
        schema_version: SCHEMA_VERSION,
        kind: BUNDLE_KIND,
        exported_at: Utc::now().to_rfc3339(),
        profile_id: params.profile_id.clone(),
        source_cf_name: cf::CF_ACTION_LOG,
        consent_key: consent_key.clone(),
        consent_sha256: sha256_hex(&consent_bytes),
        redaction_policy: redaction_policy.clone(),
        rows_scanned,
        rows_exported: exported_rows.len() as u64,
        external_sharing_allowed: false,
        files: AuditExportBundleFiles {
            rows_json: "rows.json".to_owned(),
            rows_sha256: rows_sha256.clone(),
            redaction_report_json: "redaction_report.json".to_owned(),
            redaction_report_sha256: report_sha256.clone(),
        },
        row_hashes,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("audit export manifest encode failed: {error}"),
        )
    })?;
    let manifest_sha256 = sha256_hex(&manifest_bytes);

    write_bundle_files(&output_dir, &manifest_bytes, &rows_bytes, &report_bytes)?;
    Ok(AuditExportBundleResponse {
        profile_id: params.profile_id.clone(),
        output_dir: output_dir.display().to_string(),
        manifest_path: output_dir.join("manifest.json").display().to_string(),
        rows_path: output_dir.join("rows.json").display().to_string(),
        redaction_report_path: output_dir
            .join("redaction_report.json")
            .display()
            .to_string(),
        consent_key: consent_key.clone(),
        redaction_policy,
        rows_scanned,
        rows_exported: exported_rows.len() as u64,
        redacted_fields: report.fields_redacted,
        manifest_sha256,
        rows_sha256,
        redaction_report_sha256: report_sha256,
        consent_row: stored_row(cf::CF_KV, &consent_key, &consent_bytes)?,
    })
}

fn validate_export_params(params: &AuditExportBundleParams) -> Result<(), ErrorData> {
    if params.max_rows == 0 || params.max_rows > MAX_EXPORT_ROWS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("audit_export_bundle max_rows must be 1..={MAX_EXPORT_ROWS}"),
        ));
    }
    if params.max_row_bytes == 0 || params.max_row_bytes > MAX_ROW_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("audit_export_bundle max_row_bytes must be 1..={MAX_ROW_BYTES}"),
        ));
    }
    Ok(())
}

fn explicit_export_consent(
    params: &AuditExportBundleParams,
) -> Result<(&AuditExportConsentParams, String), ErrorData> {
    let consent = params
        .consent
        .as_ref()
        .ok_or_else(|| consent_required_error("consent_arg_missing"))?;
    if !consent.enabled {
        return Err(consent_required_error("consent_disabled"));
    }
    let consent_policy = consent.redaction_policy.trim();
    validate_redaction_policy(consent_policy)?;
    let redaction_policy = params
        .redaction_policy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(consent_policy);
    validate_redaction_policy(redaction_policy)?;
    if redaction_policy != consent_policy {
        return Err(redaction_required_error("redaction_policy_not_consented"));
    }
    Ok((consent, redaction_policy.to_owned()))
}

fn write_audit_export_consent_row(
    runtime: &ReflexRuntime,
    profile_id: &str,
    consent: &AuditExportConsentParams,
    redaction_policy: &str,
    key: &str,
) -> Result<Vec<u8>, ErrorData> {
    let now = Utc::now().to_rfc3339();
    let created_at = runtime
        .storage_kv_row(key.as_bytes())
        .map_err(storage_error)?
        .and_then(|value| decode_json::<Value>(&value).ok())
        .and_then(|value| string_field(&value, "created_at"))
        .unwrap_or_else(|| now.clone());
    let row = ConsentRow {
        schema_version: SCHEMA_VERSION,
        row_kind: "audit_export_consent",
        row_id: profile_id.to_owned(),
        created_at,
        updated_at: now,
        profile_id: profile_id.to_owned(),
        state: "enabled",
        enabled: true,
        redaction_policy: redaction_policy.to_owned(),
        allowed_redaction_policies: vec![redaction_policy.to_owned()],
        external_sharing_allowed: false,
        operator_note: consent.operator_note.clone(),
    };
    let encoded = encode_json(&row).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("audit export consent row encode failed: {error}"),
        )
    })?;
    runtime
        .storage_put_kv_rows(vec![(key.as_bytes().to_vec(), encoded)])
        .map_err(storage_error)?;
    runtime
        .storage_kv_row(key.as_bytes())
        .map_err(storage_error)?
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "audit export consent row did not persist after write",
            )
        })
}

fn validate_redaction_policy(policy: &str) -> Result<(), ErrorData> {
    if policy == STRICT_POLICY {
        return Ok(());
    }
    Err(redaction_required_error("redaction_policy_unsupported"))
}

fn validate_consent(consent: &Value, policy: &str) -> Result<(), ErrorData> {
    if consent.get("schema_version").and_then(Value::as_u64) != Some(u64::from(SCHEMA_VERSION))
        || consent.get("row_kind").and_then(Value::as_str) != Some("audit_export_consent")
    {
        return Err(consent_required_error("consent_row_invalid"));
    }
    if consent.get("enabled").and_then(Value::as_bool) != Some(true)
        || consent.get("state").and_then(Value::as_str) != Some("enabled")
    {
        return Err(consent_required_error("consent_disabled"));
    }
    let allowed = consent
        .get("allowed_redaction_policies")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(policy)));
    if !allowed {
        return Err(redaction_required_error("redaction_policy_not_consented"));
    }
    if consent
        .get("external_sharing_allowed")
        .and_then(Value::as_bool)
        != Some(false)
    {
        return Err(consent_required_error("external_sharing_not_local_first"));
    }
    Ok(())
}

fn redact_value(value: Value, stats: &mut RedactionStats, path: &str) -> Value {
    match value {
        Value::Object(object) => redact_object(object, stats, path),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .enumerate()
                .map(|(index, item)| redact_value(item, stats, &format!("{path}/{index}")))
                .collect(),
        ),
        Value::String(text) if path_like_string(&text) => {
            redact_leaf(stats, "path_like_string");
            json!("[redacted:path_like_string]")
        }
        other => other,
    }
}

fn redact_object(object: Map<String, Value>, stats: &mut RedactionStats, path: &str) -> Value {
    let mut redacted = Map::new();
    for (key, value) in object {
        let child_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}/{key}")
        };
        if let Some(class) = sensitive_field_class(&key) {
            redact_leaf(stats, class);
            redacted.insert(key, json!(format!("[redacted:{class}]")));
        } else if retained_field_path(&child_path) {
            redacted.insert(key, redact_retained_value(value, stats));
        } else if traversable_container_path(&child_path) {
            redacted.insert(key, redact_value(value, stats, &child_path));
        } else {
            redact_leaf(stats, "unknown_field");
            redacted.insert(key, json!("[redacted:unknown_field]"));
        }
    }
    Value::Object(redacted)
}

fn retained_field_path(path: &str) -> bool {
    RETAINED_FIELD_PATHS.contains(&path)
}

fn traversable_container_path(path: &str) -> bool {
    TRAVERSABLE_CONTAINER_PATHS.contains(&path)
}

fn redact_retained_value(value: Value, stats: &mut RedactionStats) -> Value {
    match value {
        Value::String(text) if path_like_string(&text) => {
            redact_leaf(stats, "path_like_string");
            json!("[redacted:path_like_string]")
        }
        Value::Object(_) | Value::Array(_) => {
            redact_leaf(stats, "unknown_field");
            json!("[redacted:unknown_field]")
        }
        other => other,
    }
}

fn sensitive_field_class(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("cookie")
        || lower.contains("credential")
        || lower.contains("authorization")
    {
        return Some("credential");
    }
    if matches!(
        lower.as_str(),
        "audit_id"
            | "session_id"
            | "request_id"
            | "trace_id"
            | "span_id"
            | "correlation_id"
            | "idempotency_key"
            | "element_id"
            | "entity_id"
            | "hwnd"
            | "pid"
            | "seq"
            | "track_id"
            | "window_id"
    ) {
        return Some("high_cardinality_id");
    }
    if matches!(
        lower.as_str(),
        "ts_ns"
            | "timestamp"
            | "timestamp_ns"
            | "time_ns"
            | "created_at"
            | "updated_at"
            | "started_at"
            | "ended_at"
            | "activated_at"
            | "launched_at"
    ) || lower.ends_with("_at")
        || lower.ends_with("_ns")
    {
        return Some("timing");
    }
    if matches!(
        lower.as_str(),
        "user"
            | "user_id"
            | "username"
            | "user_name"
            | "account_id"
            | "operator_id"
            | "principal"
            | "email"
            | "player_name"
    ) || lower.ends_with("_user")
    {
        return Some("user_identifier");
    }
    if lower.contains("window_title") || lower == "title" || lower.ends_with("_title") {
        return Some("window_title");
    }
    if lower.contains("process_path")
        || lower.contains("command_line")
        || lower.ends_with("_path")
        || lower.contains("filepath")
        || lower.contains("file_path")
        || lower.contains("directory")
    {
        return Some("path");
    }
    if lower.contains("ocr")
        || lower == "text"
        || lower == "raw_text"
        || lower.ends_with("_text")
        || lower.contains("clipboard")
        || lower.contains("transcript")
    {
        return Some("text");
    }
    if lower.contains("screenshot")
        || lower.contains("image")
        || lower.contains("bitmap")
        || lower.contains("pixels")
    {
        return Some("image");
    }
    None
}

fn redact_leaf(stats: &mut RedactionStats, class: &'static str) {
    stats.fields_redacted += 1;
    *stats.redacted_by_class.entry(class.to_owned()).or_default() += 1;
}

fn path_like_string(value: &str) -> bool {
    looks_like_windows_path(value) || looks_like_unix_path(value)
}

fn looks_like_windows_path(value: &str) -> bool {
    Regex::new(r"(?i)[a-z]:\\|\\\\|\\users\\").is_ok_and(|regex| regex.is_match(value))
}

fn looks_like_unix_path(value: &str) -> bool {
    Regex::new(r"(^|\s)/(home|users|tmp|var|opt|mnt|volume)/")
        .is_ok_and(|regex| regex.is_match(value))
}

fn write_bundle_files(
    output_dir: &Path,
    manifest_bytes: &[u8],
    rows_bytes: &[u8],
    report_bytes: &[u8],
) -> Result<(), ErrorData> {
    fs::create_dir_all(output_dir).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "audit_export_bundle could not create output dir {}: {error}",
                output_dir.display()
            ),
        )
    })?;
    write_bundle_file(&output_dir.join("rows.json"), rows_bytes)?;
    write_bundle_file(&output_dir.join("redaction_report.json"), report_bytes)?;
    write_bundle_file(&output_dir.join("manifest.json"), manifest_bytes)?;
    Ok(())
}

fn write_bundle_file(path: &Path, bytes: &[u8]) -> Result<(), ErrorData> {
    fs::write(path, bytes).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "audit_export_bundle could not write {}: {error}",
                path.display()
            ),
        )
    })
}

fn required_output_dir(value: &str) -> Result<PathBuf, ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "audit_export_bundle output_path must not be empty",
        ));
    }
    Ok(PathBuf::from(trimmed))
}

fn value_mentions_profile(value: &Value, profile_id: &str) -> bool {
    string_field(value, "profile_id").as_deref() == Some(profile_id)
        || string_field(value, "active_profile_id").as_deref() == Some(profile_id)
        || value
            .pointer("/audit_context/profile_id")
            .and_then(Value::as_str)
            == Some(profile_id)
        || value
            .pointer("/foreground/profile_id")
            .and_then(Value::as_str)
            == Some(profile_id)
}

fn stored_row(cf_name: &str, key: &str, bytes: &[u8]) -> Result<AuditExportStoredRow, ErrorData> {
    Ok(AuditExportStoredRow {
        cf_name: cf_name.to_owned(),
        key: key.to_owned(),
        key_hex: hex_encode(key.as_bytes()),
        value: decode_json::<Value>(bytes).map_err(decode_error)?,
    })
}

fn consent_key(profile_id: &str) -> String {
    format!("{CONSENT_PREFIX}{profile_id}")
}

fn retained_fields() -> Vec<String> {
    RETAINED_FIELD_PATHS
        .iter()
        .map(|path| path.replace('/', "."))
        .collect()
}

fn fail_closed_rules() -> Vec<String> {
    [
        "explicit consent argument must be present and enabled",
        "consent row is written only from the consented export call",
        "redaction policy must be selected and match explicit consent",
        "external sharing remains false in local bundles",
        "matching rows larger than max_row_bytes abort the export before files are written",
        "only retained_fields are exported in plaintext; every unknown field redacts as unknown_field",
        "credential-like keys redact before retained-field checks",
        "path-like string values redact even when a retained key is unexpectedly path-shaped",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

const fn default_max_export_rows() -> u32 {
    DEFAULT_MAX_EXPORT_ROWS
}

const fn default_max_row_bytes() -> u64 {
    DEFAULT_MAX_ROW_BYTES
}

fn default_redaction_policy() -> String {
    STRICT_POLICY.to_owned()
}

fn lock_runtime<'a>(
    reflex_runtime: &'a Arc<Mutex<ReflexRuntime>>,
    context: &str,
) -> Result<MutexGuard<'a, ReflexRuntime>, ErrorData> {
    reflex_runtime.lock().map_err(|_error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("reflex runtime lock poisoned while {context}"),
        )
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err receives owned errors and this adapter preserves the simple call sites"
)]
fn storage_error(error: synapse_storage::StorageError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err receives owned errors and this adapter preserves the simple call sites"
)]
fn decode_error(error: synapse_storage::StorageError) -> ErrorData {
    mcp_error(
        error_codes::STORAGE_CORRUPTED,
        format!("audit export row decode failed: {error}"),
    )
}

fn consent_required_error(reason: &'static str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        "audit export requires explicit enabled local consent",
        Some(json!({
            "code": error_codes::AUDIT_EXPORT_CONSENT_REQUIRED,
            "reason": reason,
        })),
    )
}

fn redaction_required_error(reason: &'static str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        "audit export requires a supported redaction policy",
        Some(json!({
            "code": error_codes::AUDIT_EXPORT_REDACTION_REQUIRED,
            "reason": reason,
        })),
    )
}

fn payload_too_large_error(actual: u64, limit: u64) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("audit export row size {actual} exceeds max_row_bytes {limit}"),
        Some(json!({
            "code": error_codes::AUDIT_EXPORT_PAYLOAD_TOO_LARGE,
            "reason": "row_too_large",
            "actual_row_bytes": actual,
            "max_row_bytes": limit,
        })),
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests keep redaction failures close to the assertion"
)]
mod tests {
    use super::*;

    fn bundle_params(
        consent: Option<AuditExportConsentParams>,
        redaction_policy: Option<&str>,
    ) -> AuditExportBundleParams {
        AuditExportBundleParams {
            profile_id: "luanti.minetest".to_owned(),
            output_path: "target/audit-export-test".to_owned(),
            redaction_policy: redaction_policy.map(str::to_owned),
            consent,
            max_rows: default_max_export_rows(),
            max_row_bytes: default_max_row_bytes(),
        }
    }

    fn enabled_consent(redaction_policy: &str) -> AuditExportConsentParams {
        AuditExportConsentParams {
            enabled: true,
            redaction_policy: redaction_policy.to_owned(),
            operator_note: Some("operator consented local redacted export".to_owned()),
        }
    }

    fn error_code(error: &ErrorData) -> Option<&str> {
        error.data.as_ref()?.get("code")?.as_str()
    }

    fn error_reason(error: &ErrorData) -> Option<&str> {
        error.data.as_ref()?.get("reason")?.as_str()
    }

    #[test]
    fn export_consent_requires_explicit_consent_argument() {
        let error = explicit_export_consent(&bundle_params(None, Some("strict")))
            .expect_err("missing consent must fail closed");

        assert_eq!(
            error_code(&error),
            Some(error_codes::AUDIT_EXPORT_CONSENT_REQUIRED)
        );
        assert_eq!(error_reason(&error), Some("consent_arg_missing"));
    }

    #[test]
    fn export_consent_rejects_disabled_argument() {
        let mut consent = enabled_consent("strict");
        consent.enabled = false;

        let error = explicit_export_consent(&bundle_params(Some(consent), Some("strict")))
            .expect_err("disabled consent must fail closed");

        assert_eq!(
            error_code(&error),
            Some(error_codes::AUDIT_EXPORT_CONSENT_REQUIRED)
        );
        assert_eq!(error_reason(&error), Some("consent_disabled"));
    }

    #[test]
    fn export_consent_rejects_unsupported_policy_override() {
        let error = explicit_export_consent(&bundle_params(
            Some(enabled_consent("strict")),
            Some("operator_override"),
        ))
        .expect_err("top-level redaction policy must be supported and consented");

        assert_eq!(
            error_code(&error),
            Some(error_codes::AUDIT_EXPORT_REDACTION_REQUIRED)
        );
        assert_eq!(error_reason(&error), Some("redaction_policy_unsupported"));
    }

    #[test]
    fn export_consent_accepts_nested_consent_policy_as_effective_policy() {
        let params = bundle_params(Some(enabled_consent("strict")), None);

        let (consent, policy) =
            explicit_export_consent(&params).expect("strict enabled consent should be accepted");

        assert!(consent.enabled);
        assert_eq!(policy, "strict");
    }

    #[test]
    fn strict_redaction_keeps_audit_context_profile_signal() {
        let row = json!({
            "schema_version": 1,
            "audit_id": "01KDXAUDIT",
            "profile_id": "luanti.minetest",
            "audit_context": {
                "profile_id": "luanti.minetest",
                "session_id": "01KDXSESSION",
                "activated_at": "2026-05-27T12:00:00Z"
            },
            "seq": 77,
            "ts_ns": 1_779_886_057_659_440_500_u64,
            "foreground": {
                "process_name": "luanti.exe",
                "process_path": "C:\\Users\\hotra\\AppData\\Local\\synapse\\benchmarks\\luanti\\bin\\luanti.exe",
                "window_title": "Luanti 5.16.1 - C:\\Users\\hotra\\private-world",
                "pid": 54548
            },
            "details": {
                "raw_text": "typed private note",
                "ocr_words": ["secret"],
                "screenshot_path": "C:\\Users\\hotra\\Pictures\\screen.png",
                "user_id": "operator@example.invalid",
                "command_line": "luanti.exe --world C:\\Users\\hotra\\worlds\\private"
            }
        });
        let mut stats = RedactionStats::default();

        let redacted = redact_value(row, &mut stats, "");

        assert_eq!(redacted["profile_id"], "luanti.minetest");
        assert_eq!(redacted["audit_context"]["profile_id"], "luanti.minetest");
        assert_eq!(
            redacted["audit_context"]["session_id"],
            "[redacted:high_cardinality_id]"
        );
        assert_eq!(
            redacted["audit_context"]["activated_at"],
            "[redacted:timing]"
        );
        assert_eq!(redacted["seq"], "[redacted:high_cardinality_id]");
        assert_eq!(redacted["ts_ns"], "[redacted:timing]");
        assert_eq!(
            redacted["foreground"]["process_name"], "luanti.exe",
            "useful process signal should remain"
        );
        assert_eq!(redacted["foreground"]["process_path"], "[redacted:path]");
        assert_eq!(
            redacted["foreground"]["window_title"],
            "[redacted:window_title]"
        );
        assert_eq!(redacted["details"]["raw_text"], "[redacted:text]");
        assert_eq!(redacted["details"]["ocr_words"], "[redacted:text]");
        assert_eq!(redacted["details"]["user_id"], "[redacted:user_identifier]");
        assert_eq!(redacted["details"]["command_line"], "[redacted:path]");
        assert!(stats.fields_redacted >= 8);
    }

    #[test]
    fn strict_redaction_redacts_unknown_payload_field_names() {
        let row = json!({
            "schema_version": 1,
            "profile_id": "luanti.minetest",
            "actor": {
                "channel": "mcp",
                "tool": {
                    "token": "SYNAPSE_SECRET_RETAINED_SHAPE_1540"
                }
            },
            "tool": "approval_activate",
            "status": "ok",
            "details": {
                "value": "SYNAPSE_SECRET_VALUE_1540",
                "content": "SYNAPSE_SECRET_CONTENT_1540",
                "args": ["SYNAPSE_SECRET_ARG_1540"],
                "note": "SYNAPSE_SECRET_NOTE_1540",
                "body": "SYNAPSE_SECRET_BODY_1540",
                "message": "SYNAPSE_SECRET_MESSAGE_1540",
                "token": "synapse-activation-v1-plaintext-token"
            }
        });
        let mut stats = RedactionStats::default();

        let redacted = redact_value(row, &mut stats, "");
        let encoded = serde_json::to_string(&redacted).expect("encode redacted row");

        assert_eq!(redacted["profile_id"], "luanti.minetest");
        assert_eq!(redacted["actor"]["channel"], "mcp");
        assert_eq!(redacted["actor"]["tool"], "[redacted:unknown_field]");
        assert_eq!(redacted["tool"], "approval_activate");
        assert_eq!(redacted["status"], "ok");
        assert_eq!(redacted["details"]["value"], "[redacted:unknown_field]");
        assert_eq!(redacted["details"]["content"], "[redacted:unknown_field]");
        assert_eq!(redacted["details"]["args"], "[redacted:unknown_field]");
        assert_eq!(redacted["details"]["note"], "[redacted:unknown_field]");
        assert_eq!(redacted["details"]["body"], "[redacted:unknown_field]");
        assert_eq!(redacted["details"]["message"], "[redacted:unknown_field]");
        assert_eq!(redacted["details"]["token"], "[redacted:credential]");
        assert!(!encoded.contains("SYNAPSE_SECRET_"));
        assert!(!encoded.contains("synapse-activation-v1-plaintext-token"));
        assert_eq!(
            stats
                .redacted_by_class
                .get("unknown_field")
                .copied()
                .unwrap_or_default(),
            7
        );
        assert_eq!(
            stats
                .redacted_by_class
                .get("credential")
                .copied()
                .unwrap_or_default(),
            1
        );
    }

    #[test]
    fn path_like_string_recognizes_unix_paths_after_whitespace() {
        assert!(path_like_string("opened /home/operator/private.txt"));
    }
}

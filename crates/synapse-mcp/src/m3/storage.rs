use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use synapse_core::error_codes;
use synapse_reflex::ReflexRuntime;
use synapse_storage::{
    CalyxAnchorScanReport as BackendCalyxAnchorScanReport,
    CalyxAnchorValueReadback as BackendCalyxAnchorValueReadback,
    CalyxVaultCollectionInspect as BackendCalyxVaultCollectionInspect,
    CalyxVaultInspect as BackendCalyxVaultInspect, DiskPressureLevel, GcReport, PressureReport,
    STORAGE_METADATA_ONLY_REDACTION_POLICY, cf,
};

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    audit_retention::{
        AUDIT_RETENTION_MODE, AuditRetentionPolicy, AuditRetentionReport, AuditRetentionRunConfig,
        audit_retention_policies, run_audit_retention, validate_audit_retention_config,
    },
    permissions::{Permission, RequiredPermissions, required},
};

const MAX_PROBE_ROWS: u32 = 10_000;
const MAX_PROBE_VALUE_BYTES: u32 = 65_536;
const MAX_KEY_PREFIX_BYTES: usize = 128;
const MAX_ROW_CAP: u64 = 1_000_000;
const MAX_INSPECT_SAMPLE_ROWS_PER_CF: usize = 3;
const PROBE_WRITABLE_CFS: [&str; cf::ALL_COLUMN_FAMILIES.len()] = cf::ALL_COLUMN_FAMILIES;
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageInspectParams {}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageAnchorsParams {
    pub cf_name: String,
    /// Hex-encoded exact source-row key in `cf_name`.
    pub key_hex: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePutProbeRowsParams {
    pub cf_name: String,
    pub key_prefix: String,
    #[schemars(range(min = 0, max = 10000))]
    pub rows: u32,
    #[schemars(range(min = 0, max = 65536))]
    pub value_bytes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ns_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ns_step: Option<u64>,
    /// Key layout: `prefix_index` (default) writes `{prefix}:{index}` string
    /// keys; `timeline_ts` writes the binary `CF_TIMELINE` codec keys
    /// (`ts_ns BE || seq BE`, requires `ts_ns_start`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_mode: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageGcOnceParams {
    pub cf_name: String,
    #[schemars(range(min = 1, max = 1_000_000))]
    pub soft_cap_rows: u64,
    #[schemars(range(min = 1, max = 1_000_000))]
    pub hard_cap_rows: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_window_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePressureSampleParams {
    pub free_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageInspectResponse {
    pub schema_version: u32,
    pub storage_backend: String,
    pub pressure_level: StoragePressureLevel,
    pub pressure_transition_codes: Vec<String>,
    pub audit_retention_policies: Vec<AuditRetentionPolicy>,
    pub cf_sizes: BTreeMap<String, u64>,
    pub cf_row_counts: BTreeMap<String, u64>,
    pub cf_row_samples: BTreeMap<String, Vec<StorageRowSample>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault: Option<StorageCalyxVaultInspect>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageAnchorsResponse {
    pub source_cf: String,
    pub source_key_hex: String,
    pub source_value_len_bytes: u64,
    pub source_value_sha256: String,
    pub panel_name: String,
    pub panel_version: u32,
    pub cx_id: String,
    pub anchor_count: u64,
    pub anchors: Vec<StorageAnchorRow>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageAnchorRow {
    pub key_hex: String,
    pub cx_id: String,
    pub kind: String,
    pub value: StorageAnchorValue,
    pub source: String,
    pub observed_at_ms: u64,
    pub confidence: f32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageAnchorValue {
    pub value_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bool_value: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub one_hot_values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_sha256: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageSummaryResponse {
    pub schema_version: u32,
    pub storage_backend: String,
    pub pressure_level: StoragePressureLevel,
    pub pressure_transition_codes: Vec<String>,
    pub audit_retention_policy_count: usize,
    pub metrics_mode: String,
    pub cf_sizes: BTreeMap<String, u64>,
    pub cf_row_counts: BTreeMap<String, u64>,
    pub missing_cf_size_estimates: Vec<String>,
    pub missing_cf_row_count_estimates: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageRowSample {
    pub key_len_bytes: u64,
    pub key_sha256: String,
    pub key_material_omitted: bool,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub value_encoding: String,
    pub value_content_omitted: bool,
    pub redaction_policy: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageCalyxVaultInspect {
    pub schema_version: u32,
    pub vault_id: String,
    pub latest_seq: u64,
    pub inspected_at_unix_ms: u64,
    pub collection_count: u64,
    pub raw_row_count: u64,
    pub live_row_count: u64,
    pub expired_row_count: u64,
    pub user_key_bytes: u64,
    pub payload_bytes: u64,
    pub stored_value_bytes: u64,
    pub total_logical_bytes: u64,
    pub collections: BTreeMap<String, StorageCalyxVaultCollectionInspect>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageCalyxVaultCollectionInspect {
    pub collection_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_name: Option<String>,
    pub collection_id_hex: String,
    pub namespace: u64,
    pub raw_row_count: u64,
    pub live_row_count: u64,
    pub expired_row_count: u64,
    pub user_key_bytes: u64,
    pub payload_bytes: u64,
    pub stored_value_bytes: u64,
    pub total_logical_bytes: u64,
    pub expires_at_ms_histogram: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePutProbeRowsResponse {
    pub cf_name: String,
    pub key_prefix: String,
    pub requested_rows: u32,
    pub value_bytes: u32,
    pub before_rows: u64,
    pub after_rows: u64,
    pub rows_added: u64,
    pub after_cf_size_bytes: u64,
    pub pressure_level: StoragePressureLevel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageGcOnceResponse {
    pub cf_name: String,
    pub before_rows: u64,
    pub after_rows: u64,
    pub total_evicted_rows: u64,
    pub cache_evictions_total_delta: u64,
    pub cf_reports: Vec<StorageGcCfReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_retention_report_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_retention: Option<AuditRetentionReport>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageGcCfReport {
    pub cf_name: String,
    pub before_value: u64,
    pub after_value: u64,
    pub before_estimated_num_keys: Option<u64>,
    pub after_estimated_num_keys: Option<u64>,
    pub examined_rows: u64,
    pub scan_limited: bool,
    pub evicted_rows: u64,
    pub hard_cap_reached: bool,
    pub hard_cap_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eviction_skipped_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePressureSampleResponse {
    pub report: StoragePressureReport,
    pub pressure_transition_codes: Vec<String>,
    pub cf_row_counts: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePressureReport {
    pub free_bytes: u64,
    pub previous_level: StoragePressureLevel,
    pub current_level: StoragePressureLevel,
    pub emitted_code: Option<String>,
    pub compacted_cfs: Vec<String>,
    pub gc_advised: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePressureLevel {
    pub name: String,
    pub value: u8,
}

#[must_use]
pub const fn storage_inspect() -> M3ToolStub {
    M3ToolStub::new("storage_inspect")
}

#[must_use]
pub const fn storage_put_probe_rows() -> M3ToolStub {
    M3ToolStub::new("storage_put_probe_rows")
}

#[must_use]
pub const fn storage_gc_once() -> M3ToolStub {
    M3ToolStub::new("storage_gc_once")
}

#[must_use]
pub const fn storage_pressure_sample() -> M3ToolStub {
    M3ToolStub::new("storage_pressure_sample")
}

#[must_use]
pub fn required_permissions_inspect(_params: &StorageInspectParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_anchors(_params: &StorageAnchorsParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_put(_params: &StoragePutProbeRowsParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_gc(_params: &StorageGcOnceParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_pressure(_params: &StoragePressureSampleParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

pub fn inspect_storage(
    db: &synapse_storage::Db,
    _params: &StorageInspectParams,
) -> Result<StorageInspectResponse, ErrorData> {
    inspect_db(db)
}

pub fn inspect_storage_anchors(
    db: &synapse_storage::Db,
    params: &StorageAnchorsParams,
) -> Result<StorageAnchorsResponse, ErrorData> {
    let key = hex_decode(params.key_hex.trim()).map_err(|detail| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage operation=anchors key_hex invalid: {detail}"),
        )
    })?;
    let cf_name = known_anchor_source_cf_for_key(&params.cf_name, &key)?;
    let source_value = db
        .get_cf(cf_name, &key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let Some(source_value) = source_value else {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "storage operation=anchors source row not found: cf_name={cf_name} key_hex={}",
                params.key_hex.trim()
            ),
        ));
    };
    let source_value_len_bytes = u64::try_from(source_value.len()).unwrap_or(u64::MAX);
    let report = db
        .calyx_anchor_scan_for_source(cf_name, &key, &source_value)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(storage_anchors_response(report, source_value_len_bytes))
}

pub fn inspect_storage_summary(
    db: &synapse_storage::Db,
) -> Result<StorageSummaryResponse, ErrorData> {
    let (cf_sizes, missing_cf_size_estimates) = db
        .cf_live_data_size_estimates()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let (cf_row_counts, missing_cf_row_count_estimates) = db
        .cf_estimated_row_counts()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageSummaryResponse {
        schema_version: db.schema_version,
        storage_backend: db.backend_name().to_owned(),
        pressure_level: pressure_level(db.pressure_level()),
        pressure_transition_codes: db
            .pressure_transition_codes()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .into_iter()
            .map(str::to_owned)
            .collect(),
        audit_retention_policy_count: audit_retention_policies().len(),
        metrics_mode: storage_metrics_mode(),
        cf_sizes,
        cf_row_counts,
        missing_cf_size_estimates,
        missing_cf_row_count_estimates,
    })
}

fn storage_metrics_mode() -> String {
    "calyx_exact_scan_sizes_counts".to_owned()
}

pub fn put_probe_rows(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &StoragePutProbeRowsParams,
) -> Result<StoragePutProbeRowsResponse, ErrorData> {
    validate_probe_params(params)?;
    let cf_name = probe_writable_cf(&params.cf_name)?;
    let rows = build_probe_rows(params);
    let runtime = lock_runtime(runtime)?;
    let pressure = runtime.storage_pressure_level();
    if params.rows > 0 && !runtime.storage_pressure_permits_write(cf_name) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "storage diagnostic write refused under disk pressure: cf_name={cf_name} pressure_level={pressure:?}"
            ),
        ));
    }
    let before = cf_count(
        &runtime
            .storage_cf_row_counts()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?,
        cf_name,
    );
    runtime
        .storage_put_probe_rows(cf_name, rows)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let after_counts = runtime
        .storage_cf_row_counts()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let after_sizes = runtime
        .storage_cf_sizes()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let after = cf_count(&after_counts, cf_name);
    Ok(StoragePutProbeRowsResponse {
        cf_name: cf_name.to_owned(),
        key_prefix: params.key_prefix.trim().to_owned(),
        requested_rows: params.rows,
        value_bytes: params.value_bytes,
        before_rows: before,
        after_rows: after,
        rows_added: after.saturating_sub(before),
        after_cf_size_bytes: cf_count(&after_sizes, cf_name),
        pressure_level: pressure_level(runtime.storage_pressure_level()),
    })
}

pub fn run_storage_gc_once(
    db: &synapse_storage::Db,
    params: &StorageGcOnceParams,
) -> Result<StorageGcOnceResponse, ErrorData> {
    validate_gc_params(params)?;
    if params.cf_name.trim() == AUDIT_RETENTION_MODE {
        let before_counts = db
            .cf_row_counts()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let before = audit_rows_total(&before_counts);
        let result = run_audit_retention(
            db,
            &AuditRetentionRunConfig {
                run_id: params.run_id.clone(),
                now_ns: params.now_ns,
                max_age_ns: params.max_age_ns,
                dedupe_window_ns: params.dedupe_window_ns,
                profile_id: params.profile_id.clone(),
                soft_cap_rows: params.soft_cap_rows,
                hard_cap_rows: params.hard_cap_rows,
            },
        )?;
        let after_counts = db
            .cf_row_counts()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let after = audit_rows_total(&after_counts);
        return Ok(StorageGcOnceResponse {
            cf_name: AUDIT_RETENTION_MODE.to_owned(),
            before_rows: before,
            after_rows: after,
            total_evicted_rows: result.readback_report.total_deleted_rows,
            cache_evictions_total_delta: result.readback_report.total_deleted_rows,
            cf_reports: Vec::new(),
            audit_retention_report_key: Some(result.report_key),
            audit_retention: Some(result.readback_report),
        });
    }
    reject_audit_retention_fields(params)?;
    let cf_name = probe_writable_cf(&params.cf_name)?;
    let report = db
        .run_gc_once_with_row_caps(cf_name, params.soft_cap_rows, params.hard_cap_rows)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let (before, after) = report
        .cf(cf_name)
        .map(|cf_report| (cf_report.before_value, cf_report.after_value))
        .unwrap_or((0, 0));
    Ok(gc_response(cf_name, before, after, report))
}

pub fn apply_storage_pressure_sample(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &StoragePressureSampleParams,
) -> Result<StoragePressureSampleResponse, ErrorData> {
    let runtime = lock_runtime(runtime)?;
    let report = runtime
        .storage_run_pressure_sample(params.free_bytes)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let pressure_transition_codes = runtime
        .storage_pressure_transition_codes()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .into_iter()
        .map(str::to_owned)
        .collect();
    let (cf_row_counts, _missing_cf_row_count_estimates) = runtime
        .storage_cf_estimated_row_counts()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    drop(runtime);
    Ok(StoragePressureSampleResponse {
        report: pressure_report(report),
        pressure_transition_codes,
        cf_row_counts,
    })
}

fn inspect_db(db: &synapse_storage::Db) -> Result<StorageInspectResponse, ErrorData> {
    Ok(StorageInspectResponse {
        schema_version: db.schema_version,
        storage_backend: db.backend_name().to_owned(),
        pressure_level: pressure_level(db.pressure_level()),
        pressure_transition_codes: db
            .pressure_transition_codes()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .into_iter()
            .map(str::to_owned)
            .collect(),
        audit_retention_policies: audit_retention_policies(),
        cf_sizes: db
            .cf_sizes()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?,
        cf_row_counts: db
            .cf_row_counts()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?,
        cf_row_samples: cf_row_samples(db)?,
        calyx_vault: db
            .calyx_vault_inspect()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .map(storage_calyx_vault_inspect),
    })
}

fn cf_row_samples(
    db: &synapse_storage::Db,
) -> Result<BTreeMap<String, Vec<StorageRowSample>>, ErrorData> {
    let mut samples = BTreeMap::new();
    for cf_name in cf::ALL_COLUMN_FAMILIES {
        let rows = db
            .scan_cf_tail(cf_name, MAX_INSPECT_SAMPLE_ROWS_PER_CF)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        samples.insert(
            cf_name.to_owned(),
            rows.into_iter()
                .map(|(key, value)| storage_row_sample(&key, &value))
                .collect(),
        );
    }
    Ok(samples)
}

fn storage_row_sample(key: &[u8], value: &[u8]) -> StorageRowSample {
    StorageRowSample {
        key_len_bytes: key.len() as u64,
        key_sha256: sha256_hex(key),
        key_material_omitted: true,
        value_len_bytes: value.len() as u64,
        value_sha256: sha256_hex(value),
        value_encoding: classify_value_encoding(value),
        value_content_omitted: true,
        redaction_policy: STORAGE_METADATA_ONLY_REDACTION_POLICY.to_owned(),
    }
}

fn storage_calyx_vault_inspect(report: BackendCalyxVaultInspect) -> StorageCalyxVaultInspect {
    StorageCalyxVaultInspect {
        schema_version: report.schema_version,
        vault_id: report.vault_id,
        latest_seq: report.latest_seq,
        inspected_at_unix_ms: report.inspected_at_unix_ms,
        collection_count: report.collection_count,
        raw_row_count: report.raw_row_count,
        live_row_count: report.live_row_count,
        expired_row_count: report.expired_row_count,
        user_key_bytes: report.user_key_bytes,
        payload_bytes: report.payload_bytes,
        stored_value_bytes: report.stored_value_bytes,
        total_logical_bytes: report.total_logical_bytes,
        collections: report
            .collections
            .into_iter()
            .map(|(name, collection)| (name, storage_calyx_vault_collection(collection)))
            .collect(),
    }
}

fn storage_calyx_vault_collection(
    collection: BackendCalyxVaultCollectionInspect,
) -> StorageCalyxVaultCollectionInspect {
    StorageCalyxVaultCollectionInspect {
        collection_name: collection.collection_name,
        cf_name: collection.cf_name,
        collection_id_hex: collection.collection_id_hex,
        namespace: collection.namespace,
        raw_row_count: collection.raw_row_count,
        live_row_count: collection.live_row_count,
        expired_row_count: collection.expired_row_count,
        user_key_bytes: collection.user_key_bytes,
        payload_bytes: collection.payload_bytes,
        stored_value_bytes: collection.stored_value_bytes,
        total_logical_bytes: collection.total_logical_bytes,
        expires_at_ms_histogram: collection.expires_at_ms_histogram,
    }
}

fn storage_anchors_response(
    report: BackendCalyxAnchorScanReport,
    source_value_len_bytes: u64,
) -> StorageAnchorsResponse {
    StorageAnchorsResponse {
        source_cf: report.source_cf,
        source_key_hex: report.source_key_hex,
        source_value_len_bytes,
        source_value_sha256: report.source_value_sha256,
        panel_name: report.panel_name,
        panel_version: report.panel_version,
        cx_id: report.cx_id,
        anchor_count: u64::try_from(report.anchors.len()).unwrap_or(u64::MAX),
        anchors: report
            .anchors
            .into_iter()
            .map(|row| StorageAnchorRow {
                key_hex: row.key_hex,
                cx_id: row.cx_id,
                kind: row.kind,
                value: storage_anchor_value(row.value),
                source: row.source,
                observed_at_ms: row.observed_at_ms,
                confidence: row.confidence,
            })
            .collect(),
    }
}

fn storage_anchor_value(value: BackendCalyxAnchorValueReadback) -> StorageAnchorValue {
    StorageAnchorValue {
        value_type: value.value_type,
        bool_value: value.bool_value,
        text_value: value.text_value,
        number_value: value.number_value,
        one_hot_values: value.one_hot_values,
        vector_len: value.vector_len,
        vector_sha256: value.vector_sha256,
    }
}

fn classify_value_encoding(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "empty".to_owned();
    }
    if std::str::from_utf8(bytes).is_err() {
        return "binary_or_invalid_utf8".to_owned();
    }
    if serde_json::from_slice::<Value>(bytes).is_ok() {
        return "json".to_owned();
    }
    "utf8_non_json".to_owned()
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

fn hex_decode(value: &str) -> Result<Vec<u8>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("empty hex string".to_owned());
    }
    if !value.len().is_multiple_of(2) {
        return Err(format!("hex length must be even; got {}", value.len()));
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    for index in (0..bytes.len()).step_by(2) {
        let hi = hex_digit(bytes[index])
            .ok_or_else(|| format!("invalid hex digit at byte offset {index}"))?;
        let lo = hex_digit(bytes[index + 1])
            .ok_or_else(|| format!("invalid hex digit at byte offset {}", index + 1))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn known_anchor_source_cf_for_key(raw: &str, key: &[u8]) -> Result<&'static str, ErrorData> {
    let trimmed = raw.trim();
    let Some(cf_name) = cf::ALL_COLUMN_FAMILIES
        .iter()
        .copied()
        .find(|cf_name| *cf_name == trimmed)
    else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage operation=anchors cf_name is not known: {trimmed:?}"),
        ));
    };
    synapse_storage::constellations::anchor_panel_for_source_row(cf_name, key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(cf_name)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_encode(digest.as_ref()))
}

fn validate_probe_params(params: &StoragePutProbeRowsParams) -> Result<(), ErrorData> {
    if params.rows > MAX_PROBE_ROWS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_put_probe_rows rows must be <= {MAX_PROBE_ROWS}"),
        ));
    }
    if params.value_bytes > MAX_PROBE_VALUE_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_put_probe_rows value_bytes must be <= {MAX_PROBE_VALUE_BYTES}"),
        ));
    }
    let key_prefix = params.key_prefix.trim();
    if key_prefix.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "storage_put_probe_rows key_prefix must not be empty",
        ));
    }
    if key_prefix.len() > MAX_KEY_PREFIX_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_put_probe_rows key_prefix must be <= {MAX_KEY_PREFIX_BYTES} bytes"),
        ));
    }
    if let Some(value_json) = &params.value_json
        && !value_json.is_object()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "storage_put_probe_rows value_json must be a JSON object",
        ));
    }
    match params.key_mode.as_deref().map(str::trim) {
        None | Some("prefix_index") => {}
        Some("timeline_ts") => {
            if params.ts_ns_start.is_none() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "storage_put_probe_rows key_mode=timeline_ts requires ts_ns_start",
                ));
            }
            if params.cf_name.trim() != cf::CF_TIMELINE {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "storage_put_probe_rows key_mode=timeline_ts is only valid for CF_TIMELINE",
                ));
            }
            // Timeline-keyed probes must be valid TimelineRecord envelopes:
            // this mode exists to seed realistic timeline rows, and the
            // envelope rejects unknown fields, so the generic probe_id/seq
            // diagnostics are not injected. Validate the merged row-0 value
            // up front so a bad template fails closed instead of writing
            // rows that every consumer counts as invalid.
            let Some(template) = &params.value_json else {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "storage_put_probe_rows key_mode=timeline_ts requires value_json",
                ));
            };
            let merged = timeline_record_value(template, params.ts_ns_start.unwrap_or_default());
            if let Err(error) =
                serde_json::from_value::<synapse_core::types::TimelineRecord>(merged)
            {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "storage_put_probe_rows key_mode=timeline_ts value_json is not a valid TimelineRecord: {error}"
                    ),
                ));
            }
        }
        Some(other) => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "storage_put_probe_rows key_mode must be \"prefix_index\" or \"timeline_ts\"; got {other:?}"
                ),
            ));
        }
    }
    Ok(())
}

fn validate_gc_params(params: &StorageGcOnceParams) -> Result<(), ErrorData> {
    if params.soft_cap_rows == 0 || params.soft_cap_rows > MAX_ROW_CAP {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_gc_once soft_cap_rows must be between 1 and {MAX_ROW_CAP}"),
        ));
    }
    if params.hard_cap_rows == 0 || params.hard_cap_rows > MAX_ROW_CAP {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_gc_once hard_cap_rows must be between 1 and {MAX_ROW_CAP}"),
        ));
    }
    if params.hard_cap_rows < params.soft_cap_rows {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "storage_gc_once hard_cap_rows must be >= soft_cap_rows",
        ));
    }
    if params.cf_name.trim() == AUDIT_RETENTION_MODE {
        validate_audit_retention_config(&AuditRetentionRunConfig {
            run_id: params.run_id.clone(),
            now_ns: params.now_ns,
            max_age_ns: params.max_age_ns,
            dedupe_window_ns: params.dedupe_window_ns,
            profile_id: params.profile_id.clone(),
            soft_cap_rows: params.soft_cap_rows,
            hard_cap_rows: params.hard_cap_rows,
        })?;
    }
    Ok(())
}

fn reject_audit_retention_fields(params: &StorageGcOnceParams) -> Result<(), ErrorData> {
    if params.run_id.is_some()
        || params.now_ns.is_some()
        || params.max_age_ns.is_some()
        || params.dedupe_window_ns.is_some()
        || params.profile_id.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "storage_gc_once audit retention fields require cf_name=\"AUDIT_RETENTION\"",
        ));
    }
    Ok(())
}

fn probe_writable_cf(raw: &str) -> Result<&'static str, ErrorData> {
    let trimmed = raw.trim();
    PROBE_WRITABLE_CFS
        .into_iter()
        .find(|name| *name == trimmed)
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "storage diagnostic writes support only {}; got {trimmed:?}",
                    PROBE_WRITABLE_CFS.join(", ")
                ),
            )
        })
}

fn build_probe_rows(params: &StoragePutProbeRowsParams) -> Vec<(Vec<u8>, Vec<u8>)> {
    let prefix = params.key_prefix.trim();
    let timeline_keys = params.key_mode.as_deref().map(str::trim) == Some("timeline_ts");
    (0..params.rows)
        .map(|index| {
            let key = if timeline_keys {
                let ts_ns = params.ts_ns_start.unwrap_or_default().saturating_add(
                    params
                        .ts_ns_step
                        .unwrap_or_default()
                        .saturating_mul(u64::from(index)),
                );
                synapse_storage::timeline::timeline_key(ts_ns, index)
            } else {
                format!("{prefix}:{index:020}").into_bytes()
            };
            let value = probe_value(params, prefix, index);
            (key, value)
        })
        .collect()
}

fn probe_value(params: &StoragePutProbeRowsParams, prefix: &str, index: u32) -> Vec<u8> {
    if let Some(template) = &params.value_json {
        if params.key_mode.as_deref().map(str::trim) == Some("timeline_ts") {
            let ts_ns = params.ts_ns_start.unwrap_or_default().saturating_add(
                params
                    .ts_ns_step
                    .unwrap_or_default()
                    .saturating_mul(u64::from(index)),
            );
            let merged = timeline_record_value(template, ts_ns);
            return synapse_storage::encode_json(&merged)
                .unwrap_or_else(|_error| byte_probe_value(prefix, index, 0));
        }
        return json_probe_value(params, template, prefix, index);
    }
    byte_probe_value(prefix, index, params.value_bytes as usize)
}

/// Merges the per-row timestamp into a `TimelineRecord` template without the
/// generic probe diagnostics (the envelope rejects unknown fields).
fn timeline_record_value(template: &Value, ts_ns: u64) -> Value {
    let mut value = template.clone();
    if let Some(object) = value.as_object_mut() {
        object.insert("ts_ns".to_owned(), Value::from(ts_ns));
    }
    value
}

fn byte_probe_value(prefix: &str, index: u32, len: usize) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    let seed = format!("synapse-storage-probe:{prefix}:{index}:").into_bytes();
    let mut value = Vec::with_capacity(len);
    while value.len() < len {
        value.extend_from_slice(&seed);
    }
    value.truncate(len);
    value
}

fn json_probe_value(
    params: &StoragePutProbeRowsParams,
    template: &Value,
    prefix: &str,
    index: u32,
) -> Vec<u8> {
    let mut value = template.clone();
    if let Some(object) = value.as_object_mut() {
        object
            .entry("probe_id")
            .or_insert_with(|| Value::String(format!("{prefix}:{index:020}")));
        object
            .entry("seq")
            .or_insert_with(|| Value::from(u64::from(index)));
        if let Some(start) = params.ts_ns_start {
            let ts_ns = start.saturating_add(
                params
                    .ts_ns_step
                    .unwrap_or_default()
                    .saturating_mul(u64::from(index)),
            );
            object.entry("ts_ns").or_insert_with(|| Value::from(ts_ns));
            object
                .entry("audit_id")
                .or_insert_with(|| Value::String(format!("{ts_ns:020}-{index:010}")));
        }
    }
    synapse_storage::encode_json(&value)
        .unwrap_or_else(|_error| byte_probe_value(prefix, index, params.value_bytes as usize))
}

fn gc_response(
    cf_name: &str,
    before_rows: u64,
    after_rows: u64,
    report: GcReport,
) -> StorageGcOnceResponse {
    let total_evicted_rows = report.total_evicted_rows();
    StorageGcOnceResponse {
        cf_name: cf_name.to_owned(),
        before_rows,
        after_rows,
        total_evicted_rows,
        cache_evictions_total_delta: total_evicted_rows,
        cf_reports: report
            .cf_reports
            .into_iter()
            .map(|report| StorageGcCfReport {
                cf_name: report.cf_name,
                before_value: report.before_value,
                after_value: report.after_value,
                before_estimated_num_keys: report.before_estimated_num_keys,
                after_estimated_num_keys: report.after_estimated_num_keys,
                examined_rows: report.examined_rows,
                scan_limited: report.scan_limited,
                evicted_rows: report.evicted_rows,
                hard_cap_reached: report.hard_cap_reached,
                hard_cap_code: report.hard_cap_code.map(str::to_owned),
                eviction_skipped_reason: report.eviction_skipped_reason.map(str::to_owned),
            })
            .collect(),
        audit_retention_report_key: None,
        audit_retention: None,
    }
}

fn audit_rows_total(counts: &BTreeMap<String, u64>) -> u64 {
    [
        cf::CF_ACTION_LOG,
        cf::CF_REFLEX_AUDIT,
        cf::CF_EVENTS,
        cf::CF_OBSERVATIONS,
        cf::CF_SESSIONS,
        cf::CF_PROFILES,
        cf::CF_KV,
    ]
    .into_iter()
    .map(|cf_name| cf_count(counts, cf_name))
    .sum()
}

fn pressure_report(report: PressureReport) -> StoragePressureReport {
    StoragePressureReport {
        free_bytes: report.free_bytes,
        previous_level: pressure_level(report.previous_level),
        current_level: pressure_level(report.current_level),
        emitted_code: report.emitted_code.map(str::to_owned),
        compacted_cfs: report
            .compacted_cfs
            .into_iter()
            .map(str::to_owned)
            .collect(),
        gc_advised: report.gc_advised,
    }
}

fn pressure_level(level: DiskPressureLevel) -> StoragePressureLevel {
    StoragePressureLevel {
        name: format!("{level:?}"),
        value: level as u8,
    }
}

fn cf_count(counts: &BTreeMap<String, u64>, cf_name: &str) -> u64 {
    counts.get(cf_name).copied().unwrap_or_default()
}

fn lock_runtime(
    runtime: &Arc<Mutex<ReflexRuntime>>,
) -> Result<MutexGuard<'_, ReflexRuntime>, ErrorData> {
    runtime.lock().map_err(|_err| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "reflex runtime lock poisoned",
        )
    })
}

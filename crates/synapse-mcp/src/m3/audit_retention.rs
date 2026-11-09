use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use synapse_core::{error_codes, retention::RetentionTtl};
use synapse_reflex::ReflexRuntime;
use synapse_storage::{cf, decode_json, encode_json};

use crate::m1::mcp_error;

pub const AUDIT_RETENTION_MODE: &str = "AUDIT_RETENTION";

const RETENTION_SCHEMA_VERSION: u32 = 1;
const MAX_RUN_ID_BYTES: usize = 80;
const MAX_SCAN_ROWS_PER_CF: usize = 100_000;
const NANOS_PER_SECOND: u64 = 1_000_000_000;
const SECONDS_PER_HOUR: u64 = 60 * 60;
const HOURS_PER_DAY: u64 = 24;

type RawRow = (Vec<u8>, Vec<u8>);
type RawRows = Vec<RawRow>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditRetentionPolicy {
    pub audit_class: String,
    pub cf_name: String,
    pub key_prefix: Option<String>,
    pub ttl: String,
    pub ttl_ns: Option<u64>,
    pub dedupe_key_fields: Vec<String>,
    pub pressure_preserve: bool,
    pub strategic: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRetentionRunConfig {
    pub run_id: Option<String>,
    pub now_ns: Option<u64>,
    pub max_age_ns: Option<u64>,
    pub dedupe_window_ns: Option<u64>,
    pub profile_id: Option<String>,
    pub soft_cap_rows: u64,
    pub hard_cap_rows: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditRetentionRunResult {
    pub report_key: String,
    pub report: AuditRetentionReport,
    pub readback_report: AuditRetentionReport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditRetentionReport {
    pub schema_version: u32,
    pub row_kind: String,
    pub run_id: String,
    pub generated_at_ns: u64,
    pub profile_id: Option<String>,
    pub pressure_level: String,
    pub soft_cap_rows: u64,
    pub hard_cap_rows: u64,
    pub max_age_ns: Option<u64>,
    pub dedupe_window_ns: u64,
    pub policies: Vec<AuditRetentionPolicySnapshot>,
    pub cf_reports: Vec<AuditRetentionCfReport>,
    pub before_row_counts: BTreeMap<String, u64>,
    pub after_row_counts: BTreeMap<String, u64>,
    pub total_scanned_rows: u64,
    pub total_deleted_rows: u64,
    pub total_expired_rows_deleted: u64,
    pub total_duplicate_rows_deleted: u64,
    pub total_cap_rows_deleted: u64,
    pub total_backfilled_rows: u64,
    pub total_unknown_schema_rows: u64,
    pub preserved_strategic_rows: u64,
    pub dedupe_keys: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditRetentionPolicySnapshot {
    pub audit_class: String,
    pub cf_name: String,
    pub key_prefix: Option<String>,
    pub ttl_ns: Option<u64>,
    pub pressure_preserve: bool,
    pub strategic: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditRetentionCfReport {
    pub audit_class: String,
    pub cf_name: String,
    pub key_prefix: Option<String>,
    pub before_rows: u64,
    pub after_rows: u64,
    pub scanned_rows: u64,
    pub retained_rows: u64,
    pub profile_filtered_rows: u64,
    pub decode_failed_rows: u64,
    pub unknown_schema_rows: u64,
    pub expired_rows_deleted: u64,
    pub duplicate_rows_deleted: u64,
    pub cap_rows_deleted: u64,
    pub backfilled_rows: u64,
    pub preserved_strategic_rows: u64,
    pub deletion_keys_hex: Vec<String>,
    pub backfilled_keys_hex: Vec<String>,
}

#[must_use]
pub fn audit_retention_policies() -> Vec<AuditRetentionPolicy> {
    policy_specs()
        .into_iter()
        .map(|spec| AuditRetentionPolicy {
            audit_class: spec.audit_class.to_owned(),
            cf_name: spec.cf_name.to_owned(),
            key_prefix: spec.key_prefix.map(str::to_owned),
            ttl: spec.ttl_label.to_owned(),
            ttl_ns: spec.ttl_ns,
            dedupe_key_fields: spec
                .dedupe_key_fields
                .iter()
                .map(|field| (*field).to_owned())
                .collect(),
            pressure_preserve: spec.pressure_preserve,
            strategic: spec.strategic,
        })
        .collect()
}

pub fn validate_audit_retention_config(params: &AuditRetentionRunConfig) -> Result<(), ErrorData> {
    if params.soft_cap_rows == 0 {
        return Err(invalid("AUDIT_RETENTION soft_cap_rows must be >= 1"));
    }
    if params.hard_cap_rows < params.soft_cap_rows {
        return Err(invalid(
            "AUDIT_RETENTION hard_cap_rows must be >= soft_cap_rows",
        ));
    }
    if params.dedupe_window_ns == Some(0) {
        return Err(invalid("AUDIT_RETENTION dedupe_window_ns must be >= 1"));
    }
    if params.max_age_ns == Some(0) {
        return Err(invalid("AUDIT_RETENTION max_age_ns must be >= 1"));
    }
    if let Some(run_id) = &params.run_id {
        validate_run_id(run_id)?;
    }
    if let Some(profile_id) = &params.profile_id {
        let trimmed = profile_id.trim();
        if trimmed.is_empty() || trimmed.len() > 128 {
            return Err(invalid(
                "AUDIT_RETENTION profile_id must be non-empty and <= 128 bytes",
            ));
        }
    }
    Ok(())
}

pub fn run_audit_retention(
    runtime: &ReflexRuntime,
    params: &AuditRetentionRunConfig,
) -> Result<AuditRetentionRunResult, ErrorData> {
    validate_audit_retention_config(params)?;
    let run_id = params
        .run_id
        .as_deref()
        .map_or_else(|| format!("run-{}", current_time_ns()), str::to_owned);
    let now_ns = params.now_ns.unwrap_or_else(current_time_ns);
    let before_row_counts = runtime.storage_cf_row_counts().map_err(storage_error)?;
    let mut report = AuditRetentionReport {
        schema_version: RETENTION_SCHEMA_VERSION,
        row_kind: "audit_retention_report".to_owned(),
        run_id: run_id.clone(),
        generated_at_ns: now_ns,
        profile_id: params
            .profile_id
            .as_ref()
            .map(|profile| profile.trim().to_owned()),
        pressure_level: format!("{:?}", runtime.storage_pressure_level()),
        soft_cap_rows: params.soft_cap_rows,
        hard_cap_rows: params.hard_cap_rows,
        max_age_ns: params.max_age_ns,
        dedupe_window_ns: params.dedupe_window_ns.unwrap_or(NANOS_PER_SECOND),
        policies: policy_specs()
            .iter()
            .map(|spec| AuditRetentionPolicySnapshot {
                audit_class: spec.audit_class.to_owned(),
                cf_name: spec.cf_name.to_owned(),
                key_prefix: spec.key_prefix.map(str::to_owned),
                ttl_ns: params.max_age_ns.or(spec.ttl_ns),
                pressure_preserve: spec.pressure_preserve,
                strategic: spec.strategic,
            })
            .collect(),
        cf_reports: Vec::new(),
        before_row_counts,
        after_row_counts: BTreeMap::new(),
        total_scanned_rows: 0,
        total_deleted_rows: 0,
        total_expired_rows_deleted: 0,
        total_duplicate_rows_deleted: 0,
        total_cap_rows_deleted: 0,
        total_backfilled_rows: 0,
        total_unknown_schema_rows: 0,
        preserved_strategic_rows: 0,
        dedupe_keys: Vec::new(),
    };

    let mut dedupe_keys = BTreeSet::new();
    for spec in policy_specs() {
        let cf_report = run_policy(runtime, spec, params, now_ns, &mut dedupe_keys)?;
        accumulate_report(&mut report, &cf_report);
        report.cf_reports.push(cf_report);
    }
    report.after_row_counts = runtime.storage_cf_row_counts().map_err(storage_error)?;
    report.dedupe_keys = dedupe_keys.into_iter().collect();

    let report_key = report_key(&run_id);
    let encoded = encode_json(&report).map_err(encode_error)?;
    runtime
        .storage_put_rows_pressure_bypass(
            cf::CF_KV,
            vec![(report_key.as_bytes().to_vec(), encoded)],
        )
        .map_err(storage_error)?;
    let readback_report = runtime
        .storage_kv_row(report_key.as_bytes())
        .map_err(storage_error)?
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "audit retention report readback missing after write",
            )
        })
        .and_then(|value| decode_json::<AuditRetentionReport>(&value).map_err(decode_error))?;

    Ok(AuditRetentionRunResult {
        report_key,
        report,
        readback_report,
    })
}

fn run_policy(
    runtime: &ReflexRuntime,
    spec: PolicySpec,
    params: &AuditRetentionRunConfig,
    now_ns: u64,
    dedupe_keys: &mut BTreeSet<String>,
) -> Result<AuditRetentionCfReport, ErrorData> {
    let rows = rows_for_policy(runtime, spec)?;
    let before_rows = rows.len() as u64;
    let ttl_ns = params.max_age_ns.or(spec.ttl_ns);
    let profile_filter = params.profile_id.as_deref().map(str::trim);
    let mut report = AuditRetentionCfReport {
        audit_class: spec.audit_class.to_owned(),
        cf_name: spec.cf_name.to_owned(),
        key_prefix: spec.key_prefix.map(str::to_owned),
        before_rows,
        after_rows: before_rows,
        scanned_rows: before_rows,
        retained_rows: 0,
        profile_filtered_rows: 0,
        decode_failed_rows: 0,
        unknown_schema_rows: 0,
        expired_rows_deleted: 0,
        duplicate_rows_deleted: 0,
        cap_rows_deleted: 0,
        backfilled_rows: 0,
        preserved_strategic_rows: 0,
        deletion_keys_hex: Vec::new(),
        backfilled_keys_hex: Vec::new(),
    };
    if spec.strategic {
        report.preserved_strategic_rows = before_rows;
        report.retained_rows = before_rows;
        return Ok(report);
    }

    let mut state = PolicyRunState::default();
    let mut collect = PolicyCollectContext {
        spec,
        params,
        now_ns,
        ttl_ns,
        profile_filter,
        dedupe_keys,
    };
    collect_policy_changes(rows, &mut report, &mut state, &mut collect)?;
    apply_row_cap(params.soft_cap_rows, &mut state);
    apply_policy_changes(runtime, spec, &mut report, state)?;
    Ok(report)
}

#[derive(Default)]
struct PolicyRunState {
    retained: Vec<RetainedRow>,
    delete_reasons: BTreeMap<Vec<u8>, DeleteReason>,
    backfilled_rows: RawRows,
    first_by_dedupe: BTreeMap<String, (Vec<u8>, Option<u64>)>,
}

struct PolicyCollectContext<'a> {
    spec: PolicySpec,
    params: &'a AuditRetentionRunConfig,
    now_ns: u64,
    ttl_ns: Option<u64>,
    profile_filter: Option<&'a str>,
    dedupe_keys: &'a mut BTreeSet<String>,
}

fn collect_policy_changes(
    rows: RawRows,
    report: &mut AuditRetentionCfReport,
    state: &mut PolicyRunState,
    context: &mut PolicyCollectContext<'_>,
) -> Result<(), ErrorData> {
    for (key, value) in rows {
        let parsed = parse_row(&value, context.spec);
        let ParsedRow::Known(mut row) = parsed else {
            match parsed {
                ParsedRow::DecodeFailed => report.decode_failed_rows += 1,
                ParsedRow::UnknownSchema => report.unknown_schema_rows += 1,
                ParsedRow::Known(_) => unreachable!(),
            }
            state.retained.push(RetainedRow { key, ts_ns: None });
            continue;
        };

        let row_profile_id = row.profile_id();
        if context
            .profile_filter
            .is_some_and(|profile| row_profile_id.as_deref() != Some(profile))
        {
            report.profile_filtered_rows += 1;
            state.retained.push(RetainedRow {
                key,
                ts_ns: row.ts_ns,
            });
            continue;
        }

        if let Some(ttl_ns) = context.ttl_ns
            && row
                .ts_ns
                .is_some_and(|ts_ns| context.now_ns.saturating_sub(ts_ns) > ttl_ns)
        {
            state
                .delete_reasons
                .insert(key.clone(), DeleteReason::Expired);
            continue;
        }

        if row.backfill(context.now_ns) {
            let encoded = encode_json(&row.value).map_err(encode_error)?;
            state.backfilled_rows.push((key.clone(), encoded));
        }

        let dedupe_key = row.dedupe_key(context.spec);
        if let Some(dedupe_key) = dedupe_key {
            context.dedupe_keys.insert(dedupe_key.clone());
            if let Some((_first_key, first_ts_ns)) = state.first_by_dedupe.get(&dedupe_key)
                && within_dedupe_window(
                    *first_ts_ns,
                    row.ts_ns,
                    report_dedupe_window(context.params),
                )
            {
                state
                    .delete_reasons
                    .insert(key.clone(), DeleteReason::Duplicate);
                continue;
            }
            state
                .first_by_dedupe
                .insert(dedupe_key, (key.clone(), row.ts_ns));
        }
        state.retained.push(RetainedRow {
            key,
            ts_ns: row.ts_ns,
        });
    }
    Ok(())
}

fn apply_row_cap(soft_cap_rows: u64, state: &mut PolicyRunState) {
    if u64::try_from(state.retained.len()).unwrap_or(u64::MAX) > soft_cap_rows {
        let mut deletable = state
            .retained
            .iter()
            .filter(|row| !state.delete_reasons.contains_key(&row.key))
            .collect::<Vec<_>>();
        deletable.sort_by_key(|row| (row.ts_ns.unwrap_or(u64::MAX), row.key.clone()));
        let cap = usize::try_from(soft_cap_rows).unwrap_or(usize::MAX);
        let excess = state.retained.len().saturating_sub(cap);
        for row in deletable.into_iter().take(excess) {
            state
                .delete_reasons
                .insert(row.key.clone(), DeleteReason::Cap);
        }
    }
}

fn apply_policy_changes(
    runtime: &ReflexRuntime,
    spec: PolicySpec,
    report: &mut AuditRetentionCfReport,
    state: PolicyRunState,
) -> Result<(), ErrorData> {
    let mut delete_keys = Vec::new();
    for (key, reason) in &state.delete_reasons {
        match reason {
            DeleteReason::Expired => report.expired_rows_deleted += 1,
            DeleteReason::Duplicate => report.duplicate_rows_deleted += 1,
            DeleteReason::Cap => report.cap_rows_deleted += 1,
        }
        report.deletion_keys_hex.push(hex_encode(key));
        delete_keys.push(key.clone());
    }
    report.backfilled_rows = state.backfilled_rows.len() as u64;
    report.backfilled_keys_hex = state
        .backfilled_rows
        .iter()
        .map(|(key, _value)| hex_encode(key))
        .collect();

    if !state.backfilled_rows.is_empty() {
        runtime
            .storage_put_rows_pressure_bypass(spec.cf_name, state.backfilled_rows)
            .map_err(storage_error)?;
    }
    if !delete_keys.is_empty() {
        runtime
            .storage_delete_rows(spec.cf_name, delete_keys)
            .map_err(storage_error)?;
    }
    report.after_rows = rows_for_policy(runtime, spec)?.len() as u64;
    report.retained_rows = report.after_rows;
    Ok(())
}

const fn report_dedupe_window(params: &AuditRetentionRunConfig) -> u64 {
    match params.dedupe_window_ns {
        Some(value) => value,
        None => NANOS_PER_SECOND,
    }
}

const fn within_dedupe_window(
    first_ts_ns: Option<u64>,
    next_ts_ns: Option<u64>,
    window_ns: u64,
) -> bool {
    match (first_ts_ns, next_ts_ns) {
        (Some(first), Some(next)) => first.abs_diff(next) <= window_ns,
        _ => true,
    }
}

fn rows_for_policy(runtime: &ReflexRuntime, spec: PolicySpec) -> Result<RawRows, ErrorData> {
    spec.key_prefix.map_or_else(
        || {
            runtime
                .storage_cf_prefix_rows(spec.cf_name, &[], MAX_SCAN_ROWS_PER_CF)
                .map_err(storage_error)
        },
        |prefix| {
            runtime
                .storage_cf_prefix_rows(spec.cf_name, prefix.as_bytes(), MAX_SCAN_ROWS_PER_CF)
                .map_err(storage_error)
        },
    )
}

const fn accumulate_report(report: &mut AuditRetentionReport, cf_report: &AuditRetentionCfReport) {
    report.total_scanned_rows += cf_report.scanned_rows;
    report.total_expired_rows_deleted += cf_report.expired_rows_deleted;
    report.total_duplicate_rows_deleted += cf_report.duplicate_rows_deleted;
    report.total_cap_rows_deleted += cf_report.cap_rows_deleted;
    report.total_backfilled_rows += cf_report.backfilled_rows;
    report.total_unknown_schema_rows += cf_report.unknown_schema_rows;
    report.preserved_strategic_rows += cf_report.preserved_strategic_rows;
    report.total_deleted_rows += cf_report
        .expired_rows_deleted
        .saturating_add(cf_report.duplicate_rows_deleted)
        .saturating_add(cf_report.cap_rows_deleted);
}

enum ParsedRow {
    Known(AuditRow),
    DecodeFailed,
    UnknownSchema,
}

struct AuditRow {
    value: Value,
    ts_ns: Option<u64>,
}

impl AuditRow {
    fn profile_id(&self) -> Option<String> {
        string_at(&self.value, &["profile_id"])
            .or_else(|| string_at(&self.value, &["audit_context", "profile_id"]))
            .or_else(|| string_at(&self.value, &["foreground", "profile_id"]))
            .or_else(|| string_at(&self.value, &["active_profile_id"]))
            .or_else(|| string_at(&self.value, &["active_profile"]))
    }

    fn profile_schema_version(&self) -> Option<u32> {
        u32_at(&self.value, &["profile_schema_version"])
            .or_else(|| u32_at(&self.value, &["audit_context", "profile_schema_version"]))
            .or_else(|| u32_at(&self.value, &["foreground", "profile_schema_version"]))
            .or_else(|| u32_at(&self.value, &["active_profile_schema_version"]))
    }

    fn backfill(&mut self, now_ns: u64) -> bool {
        let profile_changed = self.backfill_profile_id();
        let version_changed = self.backfill_profile_schema_version();
        let changed = profile_changed || version_changed;
        if changed {
            set_field(
                &mut self.value,
                "audit_retention",
                json!({
                    "schema_version": RETENTION_SCHEMA_VERSION,
                    "backfilled_at_ns": now_ns,
                    "source": "storage_gc_once:AUDIT_RETENTION",
                }),
            );
        }
        changed
    }

    fn backfill_profile_id(&mut self) -> bool {
        if self.value.get("profile_id").is_some() {
            return false;
        }
        let Some(profile_id) = self.profile_id() else {
            return false;
        };
        set_field(&mut self.value, "profile_id", json!(profile_id));
        true
    }

    fn backfill_profile_schema_version(&mut self) -> bool {
        if self.value.get("profile_schema_version").is_some() {
            return false;
        }
        let Some(schema_version) = self.profile_schema_version() else {
            return false;
        };
        set_field(
            &mut self.value,
            "profile_schema_version",
            json!(schema_version),
        );
        true
    }

    fn dedupe_key(&self, spec: PolicySpec) -> Option<String> {
        match spec.audit_class {
            "actions" => {
                let status = string_at(&self.value, &["status"])?;
                if !matches!(status.as_str(), "error" | "denied" | "ok" | "started") {
                    return None;
                }
                Some(format!(
                    "actions|profile={}|tool={}|status={}|error={}|process={}|backend={}",
                    self.profile_id().unwrap_or_default(),
                    string_at(&self.value, &["tool"]).unwrap_or_default(),
                    status,
                    string_at(&self.value, &["error_code"]).unwrap_or_default(),
                    string_at(&self.value, &["foreground", "process_name"]).unwrap_or_default(),
                    string_at(&self.value, &["details", "response", "backend"])
                        .or_else(|| string_at(
                            &self.value,
                            &["details", "response", "backend_used"]
                        ))
                        .unwrap_or_default(),
                ))
            }
            "reflex_audit" => Some(format!(
                "reflex|profile={}|reflex={}|status={}|error={}",
                self.profile_id().unwrap_or_default(),
                string_at(&self.value, &["reflex_id"]).unwrap_or_default(),
                string_at(&self.value, &["status"]).unwrap_or_default(),
                string_at(&self.value, &["error_code"]).unwrap_or_default(),
            )),
            "events" => Some(format!(
                "event|profile={}|kind={}|source={}",
                self.profile_id().unwrap_or_default(),
                string_at(&self.value, &["kind"]).unwrap_or_default(),
                string_at(&self.value, &["source"]).unwrap_or_default(),
            )),
            "observations" => Some(format!(
                "observation|profile={}|reason={}|process={}|mode={}",
                self.profile_id().unwrap_or_default(),
                string_at(&self.value, &["reason"]).unwrap_or_default(),
                string_at(&self.value, &["foreground", "process_name"]).unwrap_or_default(),
                string_at(&self.value, &["mode"]).unwrap_or_default(),
            )),
            "sessions" => Some(format!(
                "session|profile={}|session={}|transport={}",
                self.profile_id().unwrap_or_default(),
                string_at(&self.value, &["session_id"]).unwrap_or_default(),
                string_at(&self.value, &["transport"]).unwrap_or_default(),
            )),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct RetainedRow {
    key: Vec<u8>,
    ts_ns: Option<u64>,
}

#[derive(Clone, Copy)]
enum DeleteReason {
    Expired,
    Duplicate,
    Cap,
}

#[derive(Clone, Copy)]
struct PolicySpec {
    audit_class: &'static str,
    cf_name: &'static str,
    key_prefix: Option<&'static str>,
    ttl_label: &'static str,
    ttl_ns: Option<u64>,
    dedupe_key_fields: &'static [&'static str],
    pressure_preserve: bool,
    strategic: bool,
}

fn policy_specs() -> Vec<PolicySpec> {
    let mut specs = vec![
        PolicySpec {
            audit_class: "actions",
            cf_name: cf::CF_ACTION_LOG,
            key_prefix: None,
            ttl_label: "24h",
            ttl_ns: ttl_to_ns(RetentionTtl::Hours(24)),
            dedupe_key_fields: &[
                "profile_id",
                "tool",
                "status",
                "error_code",
                "foreground.process_name",
                "details.response.backend",
            ],
            pressure_preserve: false,
            strategic: false,
        },
        PolicySpec {
            audit_class: "process_history",
            cf_name: cf::CF_PROCESS_HISTORY,
            key_prefix: None,
            ttl_label: "6h",
            ttl_ns: ttl_to_ns(RetentionTtl::Hours(6)),
            dedupe_key_fields: &["tool", "status", "target", "pid"],
            pressure_preserve: false,
            strategic: false,
        },
        PolicySpec {
            audit_class: "reflex_audit",
            cf_name: cf::CF_REFLEX_AUDIT,
            key_prefix: None,
            ttl_label: "7d",
            ttl_ns: ttl_to_ns(RetentionTtl::Days(7)),
            dedupe_key_fields: &["profile_id", "reflex_id", "status", "error_code"],
            pressure_preserve: true,
            strategic: false,
        },
        PolicySpec {
            audit_class: "events",
            cf_name: cf::CF_EVENTS,
            key_prefix: None,
            ttl_label: "24h",
            ttl_ns: ttl_to_ns(RetentionTtl::Hours(24)),
            dedupe_key_fields: &["profile_id", "kind", "source"],
            pressure_preserve: false,
            strategic: false,
        },
        PolicySpec {
            audit_class: "observations",
            cf_name: cf::CF_OBSERVATIONS,
            key_prefix: None,
            ttl_label: "6h",
            ttl_ns: ttl_to_ns(RetentionTtl::Hours(6)),
            dedupe_key_fields: &["profile_id", "reason", "foreground.process_name", "mode"],
            pressure_preserve: false,
            strategic: false,
        },
        PolicySpec {
            audit_class: "sessions",
            cf_name: cf::CF_SESSIONS,
            key_prefix: None,
            ttl_label: "30d",
            ttl_ns: ttl_to_ns(RetentionTtl::Days(30)),
            dedupe_key_fields: &["profile_id", "session_id", "transport"],
            pressure_preserve: true,
            strategic: false,
        },
        PolicySpec {
            audit_class: "profile_quality_snapshots",
            cf_name: cf::CF_PROFILES,
            key_prefix: Some("profile_quality/v1/"),
            ttl_label: "none",
            ttl_ns: None,
            dedupe_key_fields: &[],
            pressure_preserve: true,
            strategic: true,
        },
        PolicySpec {
            audit_class: "audit_export_consent",
            cf_name: cf::CF_KV,
            key_prefix: Some("audit_export/v1/consent/"),
            ttl_label: "none",
            ttl_ns: None,
            dedupe_key_fields: &[],
            pressure_preserve: true,
            strategic: true,
        },
    ];
    specs.extend(reality_policy_specs());
    specs
}

const fn reality_policy_specs() -> [PolicySpec; 4] {
    [
        PolicySpec {
            audit_class: "reality_baselines",
            cf_name: cf::CF_KV,
            key_prefix: Some("reality/baseline/v1/"),
            ttl_label: "none",
            ttl_ns: None,
            dedupe_key_fields: &[],
            pressure_preserve: true,
            strategic: true,
        },
        PolicySpec {
            audit_class: "reality_heads",
            cf_name: cf::CF_KV,
            key_prefix: Some("reality/head/v1/"),
            ttl_label: "none",
            ttl_ns: None,
            dedupe_key_fields: &[],
            pressure_preserve: true,
            strategic: true,
        },
        PolicySpec {
            audit_class: "reality_audits",
            cf_name: cf::CF_KV,
            key_prefix: Some("reality/audit/v1/"),
            ttl_label: "none",
            ttl_ns: None,
            dedupe_key_fields: &[],
            pressure_preserve: true,
            strategic: true,
        },
        PolicySpec {
            audit_class: "reality_delta_journal",
            cf_name: cf::CF_KV,
            key_prefix: Some("reality/delta/v1/"),
            ttl_label: "lru",
            ttl_ns: None,
            dedupe_key_fields: &[],
            pressure_preserve: false,
            strategic: false,
        },
    ]
}

fn parse_row(value: &[u8], _spec: PolicySpec) -> ParsedRow {
    let Ok(row) = decode_json::<Value>(value) else {
        return ParsedRow::DecodeFailed;
    };
    if row.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return ParsedRow::UnknownSchema;
    }
    let ts_ns = row.get("ts_ns").and_then(Value::as_u64);
    ParsedRow::Known(AuditRow { value: row, ts_ns })
}

fn report_key(run_id: &str) -> String {
    format!("audit_retention/v1/report/{run_id}")
}

fn validate_run_id(run_id: &str) -> Result<(), ErrorData> {
    let trimmed = run_id.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_RUN_ID_BYTES {
        return Err(invalid(
            "AUDIT_RETENTION run_id must be non-empty and <= 80 bytes",
        ));
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid(
            "AUDIT_RETENTION run_id may contain only ASCII letters, digits, '.', '_' and '-'",
        ));
    }
    Ok(())
}

fn ttl_to_ns(ttl: RetentionTtl) -> Option<u64> {
    match ttl {
        RetentionTtl::None | RetentionTtl::LruOnly => None,
        RetentionTtl::Hours(hours) => hours
            .checked_mul(SECONDS_PER_HOUR)?
            .checked_mul(NANOS_PER_SECOND),
        RetentionTtl::Days(days) => days
            .checked_mul(HOURS_PER_DAY)?
            .checked_mul(SECONDS_PER_HOUR)?
            .checked_mul(NANOS_PER_SECOND),
    }
}

fn current_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
        })
}

fn string_at(row: &Value, path: &[&str]) -> Option<String> {
    path.iter()
        .try_fold(row, |value, field| value.get(*field))?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn u32_at(row: &Value, path: &[&str]) -> Option<u32> {
    path.iter()
        .try_fold(row, |value, field| value.get(*field))?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
}

fn set_field(row: &mut Value, field: &str, value: Value) {
    if !row.is_object() {
        *row = Value::Object(Map::new());
    }
    if let Some(object) = row.as_object_mut() {
        object.insert(field.to_owned(), value);
    }
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

fn storage_error(error: synapse_storage::StorageError) -> ErrorData {
    let code = error.code();
    let message = error.to_string();
    drop(error);
    mcp_error(code, message)
}

fn encode_error(error: synapse_storage::StorageError) -> ErrorData {
    let message = error.to_string();
    drop(error);
    mcp_error(
        error_codes::TOOL_INTERNAL_ERROR,
        format!("audit retention report encode failed: {message}"),
    )
}

fn decode_error(error: synapse_storage::StorageError) -> ErrorData {
    let message = error.to_string();
    drop(error);
    mcp_error(
        error_codes::TOOL_INTERNAL_ERROR,
        format!("audit retention row decode failed: {message}"),
    )
}

fn invalid(message: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, message.into())
}

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest, Sha256};
use synapse_core::error_codes;
use synapse_profiles::ProfileStatus;
use synapse_storage::{cf, decode_json};

use super::{
    ProfileCompatibilitySummary, ProfileQualityContribution, ProfileQualityCounts,
    ProfileQualityRates, ProfileQualityRealityEvidence, ProfileQualityRedaction,
    ProfileQualityRefreshParams, ProfileQualityRuntimeEvidence, ProfileQualityScore,
    ProfileQualitySnapshot, ProfileQualitySource, ProfileQualityVersionSummary, hex_encode,
};

const QUALITY_SCHEMA_VERSION: u32 = 3;
const FUTURE_SKEW_NS: u64 = 60 * 1_000_000_000;
const WILSON_Z_95: f64 = 1.959_963_984_540_054;
const CONFIDENCE_FULL_SAMPLE: f64 = 20.0;

pub(super) struct ProfileQualityInputRows {
    pub action: Vec<(Vec<u8>, Vec<u8>)>,
    pub observations: Vec<(Vec<u8>, Vec<u8>)>,
    pub events: Vec<(Vec<u8>, Vec<u8>)>,
    pub reality: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug)]
struct ParsedAuditRow {
    audit_id: Option<String>,
    ts_ns: u64,
    tool: String,
    status: String,
    error_code: Option<String>,
    active_profile_id: Option<String>,
    active_profile_schema_version: Option<u32>,
    foreground_profile_id: Option<String>,
    foreground_profile_schema_version: Option<u32>,
    foreground_process_name: Option<String>,
    response_backend: Option<String>,
}

#[derive(Debug)]
struct ParsedObservationRow {
    observation_id: Option<String>,
    ts_ns: u64,
    foreground_profile_id: Option<String>,
    foreground_process_name: Option<String>,
    target_id: Option<String>,
    recent_event_kinds: Vec<String>,
}

#[derive(Debug)]
struct ParsedEventRow {
    event_id: Option<String>,
    ts_ns: u64,
    profile_id: Option<String>,
    kind: String,
    process_name: Option<String>,
    target_id: Option<String>,
}

#[derive(Debug)]
struct ParsedRealityRow {
    profile_key: String,
    kind: ParsedRealityRowKind,
}

#[derive(Debug)]
enum ParsedRealityRowKind {
    Baseline {
        epoch_id: Option<String>,
        source_surfaces: Vec<String>,
    },
    Head {
        epoch_id: Option<String>,
        head_seq: Option<u64>,
    },
    Delta {
        epoch_id: Option<String>,
        seq: Option<u64>,
        kind: Option<String>,
        path: Option<String>,
        source_surfaces: Vec<String>,
    },
    Audit {
        audit_id: Option<String>,
        epoch_id: Option<String>,
        compared_seq_end: Option<u64>,
        drift_status: Option<String>,
        rebase_required: bool,
        source_surfaces: Vec<String>,
    },
}

#[derive(Debug)]
struct RealityParseError {
    profile_key: Option<String>,
}

pub(super) fn build_snapshot(
    profile: &ProfileStatus,
    rows: ProfileQualityInputRows,
    params: &ProfileQualityRefreshParams,
    generated_at_ns: u64,
) -> ProfileQualitySnapshot {
    let mut builder = SnapshotBuilder::new(
        profile,
        params,
        rows.action.len() as u64,
        rows.observations.len() as u64,
        rows.events.len() as u64,
        rows.reality.len() as u64,
        generated_at_ns,
    );
    for (_key, value) in rows.action {
        match parse_audit_row(&value) {
            Ok(row) => builder.record_action_row(&row),
            Err(()) => builder.source.audit_rows_decode_failed += 1,
        }
    }
    for (_key, value) in rows.observations {
        match parse_observation_row(&value) {
            Ok(row) => builder.record_observation_row(&row),
            Err(()) => builder.runtime_evidence.observation_rows_decode_failed += 1,
        }
    }
    for (_key, value) in rows.events {
        match parse_event_row(&value) {
            Ok(row) => builder.record_event_row(&row),
            Err(()) => builder.runtime_evidence.event_rows_decode_failed += 1,
        }
    }
    for (key, value) in rows.reality {
        match parse_reality_row(&key, &value) {
            Ok(Some(row)) => builder.record_reality_row(&row),
            Ok(None) => {}
            Err(error) => builder.record_reality_decode_failure(error.profile_key.as_deref()),
        }
    }
    builder.finish()
}

fn parse_audit_row(value: &[u8]) -> Result<ParsedAuditRow, ()> {
    let row = decode_json::<Value>(value).map_err(|_error| ())?;
    let ts_ns = row.get("ts_ns").and_then(Value::as_u64).ok_or(())?;
    let tool = row
        .get("tool")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(())?
        .to_owned();
    let status = row
        .get("status")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(())?
        .to_owned();
    Ok(ParsedAuditRow {
        audit_id: optional_string(&row, "audit_id"),
        ts_ns,
        tool,
        status,
        error_code: optional_string(&row, "error_code"),
        active_profile_id: optional_string(&row, "active_profile_id"),
        active_profile_schema_version: optional_u32(&row, "active_profile_schema_version")
            .or_else(|| optional_u32(&row, "profile_schema_version")),
        foreground_profile_id: pointer_string(&row, "/foreground/profile_id"),
        foreground_profile_schema_version: row
            .pointer("/foreground/profile_schema_version")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        foreground_process_name: pointer_string(&row, "/foreground/process_name"),
        response_backend: pointer_string(&row, "/details/response/backend")
            .or_else(|| pointer_string(&row, "/details/response/backend_used")),
    })
}

fn parse_observation_row(value: &[u8]) -> Result<ParsedObservationRow, ()> {
    let row = decode_json::<Value>(value).map_err(|_error| ())?;
    let recent_event_kinds = row
        .get("recent_events")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| pointer_string(item, "/kind"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(ParsedObservationRow {
        observation_id: optional_string(&row, "observation_id"),
        ts_ns: row.get("ts_ns").and_then(Value::as_u64).ok_or(())?,
        foreground_profile_id: pointer_string(&row, "/foreground/profile_id"),
        foreground_process_name: pointer_string(&row, "/foreground/process_name"),
        target_id: pointer_identifier_string(&row, "/foreground/steam_appid")
            .or_else(|| pointer_string(&row, "/foreground/target_id")),
        recent_event_kinds,
    })
}

fn parse_event_row(value: &[u8]) -> Result<ParsedEventRow, ()> {
    let row = decode_json::<Value>(value).map_err(|_error| ())?;
    let kind = optional_string(&row, "kind").ok_or(())?;
    Ok(ParsedEventRow {
        event_id: optional_string(&row, "event_id"),
        ts_ns: row.get("ts_ns").and_then(Value::as_u64).ok_or(())?,
        profile_id: optional_string(&row, "profile_id")
            .or_else(|| pointer_string(&row, "/audit_context/profile_id"))
            .or_else(|| pointer_string(&row, "/data/profile_id"))
            .or_else(|| pointer_string(&row, "/foreground/profile_id")),
        kind,
        process_name: pointer_string(&row, "/data/process_name")
            .or_else(|| pointer_string(&row, "/audit_context/app_context/process_name"))
            .or_else(|| pointer_string(&row, "/foreground/process_name")),
        target_id: pointer_string(&row, "/audit_context/app_context/target_id")
            .or_else(|| pointer_string(&row, "/data/target_id"))
            .or_else(|| pointer_identifier_string(&row, "/foreground/steam_appid")),
    })
}

fn parse_reality_row(
    key: &[u8],
    value: &[u8],
) -> Result<Option<ParsedRealityRow>, RealityParseError> {
    let Ok(key) = std::str::from_utf8(key) else {
        return Ok(None);
    };
    let Some(key_parts) = reality_key_parts(key) else {
        return Ok(None);
    };
    let row = decode_json::<Value>(value).map_err(|_error| RealityParseError {
        profile_key: Some(key_parts.profile_key.clone()),
    })?;
    let kind = match key_parts.row_kind.as_str() {
        "baseline" => ParsedRealityRowKind::Baseline {
            epoch_id: optional_string(&row, "epoch_id")
                .or_else(|| Some(key_parts.epoch_or_id.clone())),
            source_surfaces: source_surfaces(&row, "source_refs"),
        },
        "head" => ParsedRealityRowKind::Head {
            epoch_id: optional_string(&row, "epoch_id"),
            head_seq: row.get("head_seq").and_then(Value::as_u64),
        },
        "delta" => ParsedRealityRowKind::Delta {
            epoch_id: optional_string(&row, "epoch_id")
                .or_else(|| Some(key_parts.epoch_or_id.clone())),
            seq: row.get("seq").and_then(Value::as_u64).or(key_parts.seq),
            kind: optional_string(&row, "kind"),
            path: optional_string(&row, "path"),
            source_surfaces: source_surfaces(&row, "source_refs"),
        },
        "audit" => ParsedRealityRowKind::Audit {
            audit_id: optional_string(&row, "audit_id")
                .or_else(|| Some(key_parts.epoch_or_id.clone())),
            epoch_id: optional_string(&row, "epoch_id"),
            compared_seq_end: row.get("compared_seq_end").and_then(Value::as_u64),
            drift_status: optional_string(&row, "drift_status"),
            rebase_required: row
                .get("rebase_required")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            source_surfaces: audit_source_surfaces(&row),
        },
        _ => return Ok(None),
    };
    Ok(Some(ParsedRealityRow {
        profile_key: key_parts.profile_key,
        kind,
    }))
}

#[derive(Debug)]
struct RealityKeyParts {
    row_kind: String,
    profile_key: String,
    epoch_or_id: String,
    seq: Option<u64>,
}

fn reality_key_parts(key: &str) -> Option<RealityKeyParts> {
    let parts = key.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["reality", "baseline", "v1", profile_key, epoch_id] => Some(RealityKeyParts {
            row_kind: "baseline".to_owned(),
            profile_key: (*profile_key).to_owned(),
            epoch_or_id: (*epoch_id).to_owned(),
            seq: None,
        }),
        ["reality", "head", "v1", profile_key] => Some(RealityKeyParts {
            row_kind: "head".to_owned(),
            profile_key: (*profile_key).to_owned(),
            epoch_or_id: String::new(),
            seq: None,
        }),
        ["reality", "delta", "v1", profile_key, epoch_id, seq] => Some(RealityKeyParts {
            row_kind: "delta".to_owned(),
            profile_key: (*profile_key).to_owned(),
            epoch_or_id: (*epoch_id).to_owned(),
            seq: seq.parse::<u64>().ok(),
        }),
        ["reality", "audit", "v1", profile_key, audit_id] => Some(RealityKeyParts {
            row_kind: "audit".to_owned(),
            profile_key: (*profile_key).to_owned(),
            epoch_or_id: (*audit_id).to_owned(),
            seq: None,
        }),
        _ => None,
    }
}

fn audit_source_surfaces(row: &Value) -> Vec<String> {
    let mut surfaces = source_surfaces(row, "physical_source_refs");
    if let Some(items) = row.get("drift_items").and_then(Value::as_array) {
        for item in items {
            surfaces.extend(source_surfaces(item, "source_refs"));
        }
    }
    surfaces.sort();
    surfaces.dedup();
    surfaces
}

fn source_surfaces(row: &Value, field: &str) -> Vec<String> {
    let mut surfaces = Vec::new();
    if let Some(items) = row.get("source_surfaces").and_then(Value::as_array) {
        surfaces.extend(items.iter().filter_map(Value::as_str).map(bounded_label));
    }
    if let Some(items) = row.get(field).and_then(Value::as_array) {
        surfaces.extend(
            items
                .iter()
                .filter_map(|item| item.get("surface").and_then(Value::as_str))
                .map(bounded_label),
        );
    }
    surfaces.sort();
    surfaces.dedup();
    surfaces
}

fn optional_string(row: &Value, field: &str) -> Option<String> {
    row.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_u32(row: &Value, field: &str) -> Option<u32> {
    row.get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn pointer_string(row: &Value, pointer: &str) -> Option<String> {
    row.pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn pointer_identifier_string(row: &Value, pointer: &str) -> Option<String> {
    let value = row.pointer(pointer)?;
    if let Some(value) = value.as_str().filter(|value| !value.is_empty()) {
        return Some(value.to_owned());
    }
    value
        .as_u64()
        .map(|value| value.to_string())
        .or_else(|| value.as_i64().map(|value| value.to_string()))
}

struct SnapshotBuilder {
    profile_id: String,
    source: ProfileQualitySource,
    counts: ProfileQualityCounts,
    compatibility: ProfileCompatibilitySummary,
    versioning: ProfileQualityVersionSummary,
    runtime_evidence: ProfileQualityRuntimeEvidence,
    reality_evidence: ProfileQualityRealityEvidence,
    reality_delta_seqs_by_epoch: BTreeMap<String, BTreeSet<u64>>,
    reality_audit_max_seq_by_epoch: BTreeMap<String, u64>,
    generated_at_ns: u64,
    evidence_parts: Vec<String>,
    profile_label: String,
    profile_schema_version: u32,
    quality_signal: Option<String>,
    manual_fsv_evidence_ref: Option<String>,
    profile_target_id: Option<String>,
}

impl SnapshotBuilder {
    fn new(
        profile: &ProfileStatus,
        params: &ProfileQualityRefreshParams,
        audit_rows_scanned: u64,
        observation_rows_scanned: u64,
        event_rows_scanned: u64,
        reality_rows_scanned: u64,
        generated_at_ns: u64,
    ) -> Self {
        Self {
            profile_id: profile.id.clone(),
            source: ProfileQualitySource {
                audit_cf_name: cf::CF_ACTION_LOG.to_owned(),
                profile_cf_name: cf::CF_PROFILES.to_owned(),
                audit_rows_scanned,
                audit_rows_decode_failed: 0,
                audit_rows_stale: 0,
                audit_rows_future: 0,
                audit_rows_other_profile: 0,
                audit_rows_profile_relevant: 0,
                first_relevant_audit_id: None,
                last_relevant_audit_id: None,
                first_relevant_ts_ns: None,
                last_relevant_ts_ns: None,
                max_audit_rows: params.max_audit_rows,
                stale_after_ns: params.stale_after_ns,
            },
            counts: ProfileQualityCounts {
                started_rows: 0,
                ok_rows: 0,
                error_rows: 0,
                denied_rows: 0,
                unknown_status_rows: 0,
                quality_eligible_ok_rows: 0,
                quality_eligible_error_rows: 0,
                backend_unavailable_rows: 0,
                release_all_rows: 0,
                launch_ok_rows: 0,
                launch_error_rows: 0,
                tool_counts: BTreeMap::new(),
                error_code_counts: BTreeMap::new(),
            },
            compatibility: ProfileCompatibilitySummary {
                foreground_match_rows: 0,
                active_profile_only_rows: 0,
                profile_mismatch_rows: 0,
                target_denied_rows: 0,
                observed_process_names: BTreeMap::new(),
                observed_backends: BTreeMap::new(),
            },
            versioning: ProfileQualityVersionSummary {
                current_profile_schema_version: profile.schema_version,
                rows_with_profile_schema_version: 0,
                current_version_rows: 0,
                older_version_rows: 0,
                newer_version_rows: 0,
                unknown_version_rows: 0,
                mixed_profile_schema_versions: false,
                observed_profile_schema_versions: BTreeMap::new(),
            },
            runtime_evidence: ProfileQualityRuntimeEvidence {
                observation_cf_name: cf::CF_OBSERVATIONS.to_owned(),
                event_cf_name: cf::CF_EVENTS.to_owned(),
                observation_rows_scanned,
                event_rows_scanned,
                ..ProfileQualityRuntimeEvidence::default()
            },
            reality_evidence: ProfileQualityRealityEvidence {
                kv_cf_name: cf::CF_KV.to_owned(),
                reality_rows_scanned,
                ..ProfileQualityRealityEvidence::default()
            },
            reality_delta_seqs_by_epoch: BTreeMap::new(),
            reality_audit_max_seq_by_epoch: BTreeMap::new(),
            generated_at_ns,
            evidence_parts: Vec::new(),
            profile_label: profile.label.clone(),
            profile_schema_version: profile.schema_version,
            quality_signal: profile.metadata.get("registry.quality_signal").cloned(),
            manual_fsv_evidence_ref: params
                .manual_fsv_evidence_ref
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            profile_target_id: profile
                .metadata
                .get("registry.compatibility_target")
                .or_else(|| profile.metadata.get("benchmark_id"))
                .cloned(),
        }
    }

    fn record_action_row(&mut self, row: &ParsedAuditRow) {
        if self.is_action_stale_or_future(row.ts_ns) {
            return;
        }
        let foreground_match = row.foreground_profile_id.as_deref() == Some(&self.profile_id);
        let active_match = row.active_profile_id.as_deref() == Some(&self.profile_id);
        if !(foreground_match || active_match) {
            self.source.audit_rows_other_profile += 1;
            return;
        }

        self.source.audit_rows_profile_relevant += 1;
        self.record_action_range(row);
        self.record_compatibility(row, foreground_match, active_match);
        *self.counts.tool_counts.entry(row.tool.clone()).or_default() += 1;
        if let Some(error_code) = &row.error_code {
            *self
                .counts
                .error_code_counts
                .entry(error_code.clone())
                .or_default() += 1;
            if error_code == error_codes::ACTION_BACKEND_UNAVAILABLE {
                self.counts.backend_unavailable_rows += 1;
            }
        }
        if let Some(backend) = &row.response_backend {
            *self
                .compatibility
                .observed_backends
                .entry(backend.clone())
                .or_default() += 1;
        }
        self.record_status(row, foreground_match);
        self.record_version(row, foreground_match);
        self.evidence_parts.push(action_evidence_part(row));
    }

    fn record_observation_row(&mut self, row: &ParsedObservationRow) {
        if self.is_runtime_stale_or_future(row.ts_ns, RuntimeRowKind::Observation) {
            return;
        }
        if row.foreground_profile_id.as_deref() != Some(&self.profile_id) {
            self.runtime_evidence.observation_rows_other_profile += 1;
            return;
        }

        self.runtime_evidence.observation_rows_profile_relevant += 1;
        if self
            .runtime_evidence
            .first_relevant_observation_id
            .is_none()
        {
            self.runtime_evidence
                .first_relevant_observation_id
                .clone_from(&row.observation_id);
        }
        self.runtime_evidence
            .last_relevant_observation_id
            .clone_from(&row.observation_id);
        self.record_runtime_ts(row.ts_ns);
        if let Some(process_name) = &row.foreground_process_name {
            increment(
                &mut self.runtime_evidence.observed_process_names,
                process_name.clone(),
            );
        }
        let target_id = row.target_id.as_ref().or(self.profile_target_id.as_ref());
        if let Some(target_id) = target_id {
            increment(
                &mut self.runtime_evidence.observed_target_ids,
                target_id.clone(),
            );
        }
        for kind in &row.recent_event_kinds {
            increment(
                &mut self.runtime_evidence.observed_event_kinds,
                kind.clone(),
            );
            if is_log_event_kind(kind) {
                increment(
                    &mut self.runtime_evidence.observed_log_event_kinds,
                    kind.clone(),
                );
            }
        }
        self.evidence_parts.push(observation_evidence_part(row));
    }

    fn record_event_row(&mut self, row: &ParsedEventRow) {
        if self.is_runtime_stale_or_future(row.ts_ns, RuntimeRowKind::Event) {
            return;
        }
        if row.profile_id.as_deref() != Some(&self.profile_id) {
            self.runtime_evidence.event_rows_other_profile += 1;
            return;
        }

        self.runtime_evidence.event_rows_profile_relevant += 1;
        if self.runtime_evidence.first_relevant_event_id.is_none() {
            self.runtime_evidence
                .first_relevant_event_id
                .clone_from(&row.event_id);
        }
        self.runtime_evidence
            .last_relevant_event_id
            .clone_from(&row.event_id);
        self.record_runtime_ts(row.ts_ns);
        if let Some(process_name) = &row.process_name {
            increment(
                &mut self.runtime_evidence.observed_process_names,
                process_name.clone(),
            );
        }
        let target_id = row.target_id.as_ref().or(self.profile_target_id.as_ref());
        if let Some(target_id) = target_id {
            increment(
                &mut self.runtime_evidence.observed_target_ids,
                target_id.clone(),
            );
        }
        increment(
            &mut self.runtime_evidence.observed_event_kinds,
            row.kind.clone(),
        );
        if is_log_event_kind(&row.kind) {
            increment(
                &mut self.runtime_evidence.observed_log_event_kinds,
                row.kind.clone(),
            );
        }
        self.evidence_parts.push(event_evidence_part(row));
    }

    fn record_reality_decode_failure(&mut self, profile_key: Option<&str>) {
        if profile_key == Some(&self.profile_id) {
            self.reality_evidence.reality_rows_decode_failed += 1;
        } else if profile_key.is_some() {
            self.reality_evidence.reality_rows_other_profile += 1;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn record_reality_row(&mut self, row: &ParsedRealityRow) {
        if row.profile_key != self.profile_id {
            self.reality_evidence.reality_rows_other_profile += 1;
            return;
        }
        match &row.kind {
            ParsedRealityRowKind::Baseline {
                epoch_id,
                source_surfaces,
            } => {
                self.reality_evidence.baseline_rows += 1;
                self.reality_evidence
                    .latest_baseline_epoch_id
                    .clone_from(epoch_id);
                self.record_reality_surfaces(source_surfaces);
                self.evidence_parts.push(format!(
                    "reality_baseline|{}|{}",
                    row.profile_key,
                    epoch_id.as_deref().unwrap_or("")
                ));
            }
            ParsedRealityRowKind::Head { epoch_id, head_seq } => {
                self.reality_evidence.head_rows += 1;
                self.reality_evidence
                    .latest_head_epoch_id
                    .clone_from(epoch_id);
                self.reality_evidence.latest_head_seq = *head_seq;
                self.evidence_parts.push(format!(
                    "reality_head|{}|{}|{}",
                    row.profile_key,
                    epoch_id.as_deref().unwrap_or(""),
                    head_seq.map_or_else(String::new, |value| value.to_string())
                ));
            }
            ParsedRealityRowKind::Delta {
                epoch_id,
                seq,
                kind,
                path,
                source_surfaces,
            } => {
                self.reality_evidence.delta_rows += 1;
                if let (Some(epoch_id), Some(seq)) = (epoch_id, seq) {
                    self.reality_delta_seqs_by_epoch
                        .entry(epoch_id.clone())
                        .or_default()
                        .insert(*seq);
                }
                if let Some(kind) = kind {
                    increment(
                        &mut self.reality_evidence.delta_kind_counts,
                        bounded_label(kind),
                    );
                }
                if let Some(path) = path {
                    increment(
                        &mut self.reality_evidence.delta_path_counts,
                        bounded_label(path),
                    );
                }
                self.record_reality_surfaces(source_surfaces);
                self.evidence_parts.push(format!(
                    "reality_delta|{}|{}|{}|{}|{}",
                    row.profile_key,
                    epoch_id.as_deref().unwrap_or(""),
                    seq.map_or_else(String::new, |value| value.to_string()),
                    kind.as_deref().unwrap_or(""),
                    path.as_deref().unwrap_or("")
                ));
            }
            ParsedRealityRowKind::Audit {
                audit_id,
                epoch_id,
                compared_seq_end,
                drift_status,
                rebase_required,
                source_surfaces,
            } => {
                self.reality_evidence.audit_rows += 1;
                self.reality_evidence.latest_audit_id.clone_from(audit_id);
                self.reality_evidence
                    .latest_audit_status
                    .clone_from(drift_status);
                self.reality_evidence.latest_audit_compared_seq_end = *compared_seq_end;
                if let (Some(epoch_id), Some(compared_seq_end)) = (epoch_id, compared_seq_end) {
                    let current = self
                        .reality_audit_max_seq_by_epoch
                        .entry(epoch_id.clone())
                        .or_default();
                    *current = (*current).max(*compared_seq_end);
                }
                if let Some(status) = drift_status {
                    let status = bounded_label(status);
                    increment(
                        &mut self.reality_evidence.audit_drift_status_counts,
                        status.clone(),
                    );
                    match status.as_str() {
                        "in_sync" => self.reality_evidence.in_sync_audit_rows += 1,
                        "source_unavailable" => {
                            self.reality_evidence.source_unavailable_audit_rows += 1;
                            self.reality_evidence.drift_audit_rows += 1;
                        }
                        _ => self.reality_evidence.drift_audit_rows += 1,
                    }
                }
                if *rebase_required {
                    self.reality_evidence.rebase_required_rows += 1;
                }
                self.record_reality_surfaces(source_surfaces);
                self.evidence_parts.push(format!(
                    "reality_audit|{}|{}|{}|{}|{}|{}",
                    row.profile_key,
                    audit_id.as_deref().unwrap_or(""),
                    epoch_id.as_deref().unwrap_or(""),
                    compared_seq_end.map_or_else(String::new, |value| value.to_string()),
                    drift_status.as_deref().unwrap_or(""),
                    rebase_required
                ));
            }
        }
    }

    fn record_reality_surfaces(&mut self, surfaces: &[String]) {
        for surface in surfaces {
            increment(
                &mut self.reality_evidence.source_surface_counts,
                bounded_label(surface),
            );
        }
    }

    const fn is_action_stale_or_future(&mut self, ts_ns: u64) -> bool {
        if ts_ns > self.generated_at_ns.saturating_add(FUTURE_SKEW_NS) {
            self.source.audit_rows_future += 1;
            return true;
        }
        if self.generated_at_ns.saturating_sub(ts_ns) > self.source.stale_after_ns {
            self.source.audit_rows_stale += 1;
            return true;
        }
        false
    }

    const fn is_runtime_stale_or_future(&mut self, ts_ns: u64, kind: RuntimeRowKind) -> bool {
        if ts_ns > self.generated_at_ns.saturating_add(FUTURE_SKEW_NS) {
            match kind {
                RuntimeRowKind::Observation => self.runtime_evidence.observation_rows_future += 1,
                RuntimeRowKind::Event => self.runtime_evidence.event_rows_future += 1,
            }
            return true;
        }
        if self.generated_at_ns.saturating_sub(ts_ns) > self.source.stale_after_ns {
            match kind {
                RuntimeRowKind::Observation => self.runtime_evidence.observation_rows_stale += 1,
                RuntimeRowKind::Event => self.runtime_evidence.event_rows_stale += 1,
            }
            return true;
        }
        false
    }

    fn record_action_range(&mut self, row: &ParsedAuditRow) {
        if self.source.first_relevant_ts_ns.is_none() {
            self.source.first_relevant_ts_ns = Some(row.ts_ns);
            self.source
                .first_relevant_audit_id
                .clone_from(&row.audit_id);
        }
        self.source.last_relevant_ts_ns = Some(row.ts_ns);
        self.source.last_relevant_audit_id.clone_from(&row.audit_id);
    }

    fn record_runtime_ts(&mut self, ts_ns: u64) {
        self.runtime_evidence.last_relevant_ts_ns = Some(
            self.runtime_evidence
                .last_relevant_ts_ns
                .map_or(ts_ns, |previous| previous.max(ts_ns)),
        );
    }

    fn record_compatibility(
        &mut self,
        row: &ParsedAuditRow,
        foreground_match: bool,
        active_match: bool,
    ) {
        if foreground_match {
            self.compatibility.foreground_match_rows += 1;
        } else if active_match {
            self.compatibility.active_profile_only_rows += 1;
            self.compatibility.profile_mismatch_rows += 1;
        }
        if let Some(process_name) = &row.foreground_process_name {
            *self
                .compatibility
                .observed_process_names
                .entry(process_name.clone())
                .or_default() += 1;
        }
    }

    fn record_status(&mut self, row: &ParsedAuditRow, foreground_match: bool) {
        match row.status.as_str() {
            "started" => self.counts.started_rows += 1,
            "ok" => {
                self.counts.ok_rows += 1;
                if foreground_match {
                    self.counts.quality_eligible_ok_rows += 1;
                }
                if row.tool == "act_launch" {
                    self.counts.launch_ok_rows += 1;
                }
                if row.tool == "release_all" {
                    self.counts.release_all_rows += 1;
                }
            }
            "error" => {
                self.counts.error_rows += 1;
                if foreground_match {
                    self.counts.quality_eligible_error_rows += 1;
                }
                if row.tool == "act_launch" {
                    self.counts.launch_error_rows += 1;
                }
            }
            "denied" => {
                self.counts.denied_rows += 1;
                self.compatibility.target_denied_rows += 1;
            }
            _ => self.counts.unknown_status_rows += 1,
        }
    }

    fn record_version(&mut self, row: &ParsedAuditRow, foreground_match: bool) {
        let row_version = if foreground_match {
            row.foreground_profile_schema_version
        } else {
            row.active_profile_schema_version
        };
        let Some(row_version) = row_version else {
            self.versioning.unknown_version_rows += 1;
            return;
        };

        self.versioning.rows_with_profile_schema_version += 1;
        *self
            .versioning
            .observed_profile_schema_versions
            .entry(row_version.to_string())
            .or_default() += 1;

        match row_version.cmp(&self.profile_schema_version) {
            std::cmp::Ordering::Equal => self.versioning.current_version_rows += 1,
            std::cmp::Ordering::Less => self.versioning.older_version_rows += 1,
            std::cmp::Ordering::Greater => self.versioning.newer_version_rows += 1,
        }
    }

    fn finish(self) -> ProfileQualitySnapshot {
        let score = score_from_counts(&self.counts);
        let rates = rates_from_counts(&self.counts);
        let mut reality_evidence = self.reality_evidence;
        finalize_reality_evidence(
            &mut reality_evidence,
            &self.reality_delta_seqs_by_epoch,
            &self.reality_audit_max_seq_by_epoch,
        );
        let evidence_hash = evidence_hash(
            &self.profile_id,
            &self.evidence_parts,
            &self.source,
            &self.runtime_evidence,
            &reality_evidence,
            self.manual_fsv_evidence_ref.as_deref(),
        );
        let mut versioning = self.versioning;
        versioning.mixed_profile_schema_versions =
            versioning.observed_profile_schema_versions.len() > 1;
        ProfileQualitySnapshot {
            schema_version: QUALITY_SCHEMA_VERSION,
            profile_id: self.profile_id,
            profile_label: self.profile_label,
            profile_schema_version: self.profile_schema_version,
            quality_signal: self.quality_signal,
            manual_fsv_evidence_ref: self.manual_fsv_evidence_ref,
            generated_at_ns: self.generated_at_ns,
            evidence_hash,
            source: self.source,
            counts: self.counts,
            rates,
            score,
            compatibility: self.compatibility,
            versioning,
            runtime_evidence: self.runtime_evidence,
            reality_evidence,
            redaction: ProfileQualityRedaction {
                local_only: true,
                snapshot_redacts_process_path: true,
                snapshot_redacts_window_title: true,
                retained_identifiers: vec![
                    "profile_id".to_owned(),
                    "manual_fsv_evidence_ref".to_owned(),
                    "tool".to_owned(),
                    "status".to_owned(),
                    "error_code".to_owned(),
                    "process_name".to_owned(),
                    "target_id".to_owned(),
                    "observation_id".to_owned(),
                    "event_id".to_owned(),
                    "event_kind".to_owned(),
                    "reality_epoch_id".to_owned(),
                    "reality_delta_kind".to_owned(),
                    "reality_delta_path".to_owned(),
                    "reality_audit_status".to_owned(),
                    "reality_source_surface".to_owned(),
                ],
            },
            contribution: ProfileQualityContribution {
                export_allowed: false,
                operator_consent_required: true,
                future_bundle_shape: "operator_approved_profile_quality_v2".to_owned(),
            },
        }
    }
}

#[derive(Clone, Copy)]
enum RuntimeRowKind {
    Observation,
    Event,
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn score_from_counts(counts: &ProfileQualityCounts) -> ProfileQualityScore {
    let successes = counts.quality_eligible_ok_rows;
    let failures = counts.quality_eligible_error_rows;
    let sample_size = successes.saturating_add(failures);
    let wilson = wilson_lower_bound(successes, sample_size);
    ProfileQualityScore {
        score_0_100: (wilson * 100.0).round() as u32,
        confidence_0_1: (sample_size as f64 / CONFIDENCE_FULL_SAMPLE).min(1.0),
        wilson_success_lower_95: wilson,
        sample_size,
        method: "wilson_lower_95_on_foreground_ok_vs_error; denied rows tracked separately"
            .to_owned(),
    }
}

fn rates_from_counts(counts: &ProfileQualityCounts) -> ProfileQualityRates {
    let terminal = counts
        .ok_rows
        .saturating_add(counts.error_rows)
        .saturating_add(counts.denied_rows);
    ProfileQualityRates {
        success_rate: ratio(counts.ok_rows, terminal),
        error_rate: ratio(counts.error_rows, terminal),
        denied_rate: ratio(counts.denied_rows, terminal),
        backend_unavailable_rate: ratio(counts.backend_unavailable_rows, terminal),
    }
}

#[allow(clippy::cast_precision_loss)]
fn finalize_reality_evidence(
    evidence: &mut ProfileQualityRealityEvidence,
    delta_seqs_by_epoch: &BTreeMap<String, BTreeSet<u64>>,
    audit_max_seq_by_epoch: &BTreeMap<String, u64>,
) {
    let mut audited_delta_rows = 0_u64;
    for (epoch_id, delta_seqs) in delta_seqs_by_epoch {
        if let Some(compared_seq_end) = audit_max_seq_by_epoch.get(epoch_id) {
            audited_delta_rows += delta_seqs
                .iter()
                .filter(|seq| *seq <= compared_seq_end)
                .count() as u64;
        }
    }
    evidence.audited_delta_rows = audited_delta_rows.min(evidence.delta_rows);
    evidence.unaudited_delta_rows = evidence.delta_rows.saturating_sub(audited_delta_rows);
    evidence.drift_rate = ratio(evidence.drift_audit_rows, evidence.audit_rows);
    evidence.rebase_rate = ratio(evidence.rebase_required_rows, evidence.audit_rows);
    evidence.audited_delta_rate = ratio(evidence.audited_delta_rows, evidence.delta_rows);
    evidence.calibration_source = if evidence.audit_rows > 0 {
        "reality_audit".to_owned()
    } else {
        "none".to_owned()
    };
    evidence.delta_first_supported = evidence.baseline_rows > 0
        && evidence.head_rows > 0
        && evidence.audit_rows > 0
        && evidence.source_unavailable_audit_rows == 0;
    evidence.full_snapshot_required = evidence.audit_rows == 0
        || evidence.rebase_required_rows > 0
        || evidence.source_unavailable_audit_rows > 0
        || evidence.unaudited_delta_rows > 0;
}

#[allow(clippy::cast_precision_loss)]
fn wilson_lower_bound(successes: u64, sample_size: u64) -> f64 {
    if sample_size == 0 {
        return 0.0;
    }
    let n = sample_size as f64;
    let p = successes as f64 / n;
    let z2 = WILSON_Z_95 * WILSON_Z_95;
    let center = p + z2 / (2.0 * n);
    let margin = WILSON_Z_95 * (p.mul_add(1.0 - p, z2 / (4.0 * n)) / n).sqrt();
    ((center - margin) / (1.0 + z2 / n)).clamp(0.0, 1.0)
}

#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

fn action_evidence_part(row: &ParsedAuditRow) -> String {
    format!(
        "action|{}|{}|{}|{}|{}|{}|{}|{}",
        row.audit_id.as_deref().unwrap_or(""),
        row.ts_ns,
        row.tool,
        row.status,
        row.error_code.as_deref().unwrap_or(""),
        row.foreground_profile_id.as_deref().unwrap_or(""),
        row.foreground_profile_schema_version
            .map(|value| value.to_string())
            .unwrap_or_default(),
        row.active_profile_schema_version
            .map(|value| value.to_string())
            .unwrap_or_default()
    )
}

fn observation_evidence_part(row: &ParsedObservationRow) -> String {
    format!(
        "observation|{}|{}|{}|{}|{}",
        row.observation_id.as_deref().unwrap_or(""),
        row.ts_ns,
        row.foreground_profile_id.as_deref().unwrap_or(""),
        row.foreground_process_name.as_deref().unwrap_or(""),
        row.recent_event_kinds.join(",")
    )
}

fn event_evidence_part(row: &ParsedEventRow) -> String {
    format!(
        "event|{}|{}|{}|{}|{}|{}",
        row.event_id.as_deref().unwrap_or(""),
        row.ts_ns,
        row.profile_id.as_deref().unwrap_or(""),
        row.kind.as_str(),
        row.process_name.as_deref().unwrap_or(""),
        row.target_id.as_deref().unwrap_or("")
    )
}

fn evidence_hash(
    profile_id: &str,
    parts: &[String],
    source: &ProfileQualitySource,
    runtime: &ProfileQualityRuntimeEvidence,
    reality: &ProfileQualityRealityEvidence,
    manual_fsv_evidence_ref: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(profile_id.as_bytes());
    hasher.update(source.stale_after_ns.to_be_bytes());
    hasher.update(source.audit_rows_decode_failed.to_be_bytes());
    hasher.update(source.audit_rows_stale.to_be_bytes());
    hasher.update(source.audit_rows_future.to_be_bytes());
    hasher.update(runtime.observation_rows_decode_failed.to_be_bytes());
    hasher.update(runtime.observation_rows_stale.to_be_bytes());
    hasher.update(runtime.observation_rows_future.to_be_bytes());
    hasher.update(runtime.event_rows_decode_failed.to_be_bytes());
    hasher.update(runtime.event_rows_stale.to_be_bytes());
    hasher.update(runtime.event_rows_future.to_be_bytes());
    hasher.update(reality.reality_rows_decode_failed.to_be_bytes());
    hasher.update(reality.baseline_rows.to_be_bytes());
    hasher.update(reality.delta_rows.to_be_bytes());
    hasher.update(reality.audit_rows.to_be_bytes());
    hasher.update(reality.audited_delta_rows.to_be_bytes());
    hasher.update(reality.rebase_required_rows.to_be_bytes());
    if let Some(value) = manual_fsv_evidence_ref {
        hasher.update(value.as_bytes());
    }
    hasher.update([0]);
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("sha256:{}", hex_encode(&hasher.finalize()))
}

fn increment(counts: &mut BTreeMap<String, u64>, key: String) {
    *counts.entry(key).or_default() += 1;
}

fn bounded_label(value: &str) -> String {
    value
        .chars()
        .filter(|value| !value.is_control())
        .take(96)
        .collect()
}

fn is_log_event_kind(kind: &str) -> bool {
    kind.starts_with("runtime.log.") || kind.contains(".log.")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use serde_json::json;
    use synapse_core::{Backend, PerceptionMode, ProfileBackends, ProfileUseScope};
    use synapse_profiles::ProfileStatus;

    use super::{
        ProfileQualityInputRows, ProfileQualityRefreshParams, build_snapshot, parse_reality_row,
    };

    #[test]
    fn reality_rows_become_calibrated_profile_quality_signals() {
        let snapshot = build_snapshot(
            &profile(),
            ProfileQualityInputRows {
                action: Vec::new(),
                observations: Vec::new(),
                events: Vec::new(),
                reality: vec![
                    row(
                        "reality/baseline/v1/demo.profile/epoch-a",
                        json!({
                            "epoch_id": "epoch-a",
                            "source_surfaces": ["window", "process", "game_log"]
                        }),
                    ),
                    row(
                        "reality/head/v1/demo.profile",
                        json!({"epoch_id": "epoch-a", "head_seq": 2}),
                    ),
                    row(
                        "reality/delta/v1/demo.profile/epoch-a/00000000000000000001",
                        json!({
                            "epoch_id": "epoch-a",
                            "seq": 1,
                            "kind": "log_cursor_changed",
                            "path": "/events/log_cursor",
                            "source_refs": [{"surface": "game_log"}]
                        }),
                    ),
                    row(
                        "reality/delta/v1/demo.profile/epoch-a/00000000000000000002",
                        json!({
                            "epoch_id": "epoch-a",
                            "seq": 2,
                            "kind": "runtime_event_changed",
                            "path": "/events/runtime",
                            "source_refs": [{"surface": "game_log"}]
                        }),
                    ),
                    row(
                        "reality/audit/v1/demo.profile/audit-a",
                        json!({
                            "audit_id": "audit-a",
                            "epoch_id": "epoch-a",
                            "compared_seq_end": 2,
                            "drift_status": "in_sync",
                            "rebase_required": false,
                            "physical_source_refs": [
                                {"surface": "window"},
                                {"surface": "game_log"}
                            ]
                        }),
                    ),
                ],
            },
            &params(),
            1_780_000_000_000,
        );

        let reality = snapshot.reality_evidence;
        assert_eq!(reality.baseline_rows, 1);
        assert_eq!(reality.delta_rows, 2);
        assert_eq!(reality.audit_rows, 1);
        assert_eq!(reality.audited_delta_rows, 2);
        assert_eq!(reality.unaudited_delta_rows, 0);
        assert_eq!(reality.in_sync_audit_rows, 1);
        assert!(reality.delta_first_supported);
        assert!(!reality.full_snapshot_required);
        assert_eq!(reality.calibration_source, "reality_audit");
        assert_eq!(
            reality.delta_kind_counts.get("runtime_event_changed"),
            Some(&1)
        );
        assert_eq!(reality.source_surface_counts.get("game_log"), Some(&4));
    }

    #[test]
    fn unaudited_deltas_do_not_make_delta_first_supported() {
        let snapshot = build_snapshot(
            &profile(),
            ProfileQualityInputRows {
                action: Vec::new(),
                observations: Vec::new(),
                events: Vec::new(),
                reality: vec![
                    row(
                        "reality/baseline/v1/demo.profile/epoch-b",
                        json!({"epoch_id": "epoch-b"}),
                    ),
                    row(
                        "reality/head/v1/demo.profile",
                        json!({"epoch_id": "epoch-b", "head_seq": 1}),
                    ),
                    row(
                        "reality/delta/v1/demo.profile/epoch-b/00000000000000000001",
                        json!({
                            "epoch_id": "epoch-b",
                            "seq": 1,
                            "kind": "foreground_changed",
                            "path": "/foreground"
                        }),
                    ),
                ],
            },
            &params(),
            1_780_000_000_000,
        );

        let reality = snapshot.reality_evidence;
        assert_eq!(reality.audit_rows, 0);
        assert_eq!(reality.audited_delta_rows, 0);
        assert_eq!(reality.unaudited_delta_rows, 1);
        assert!(!reality.delta_first_supported);
        assert!(reality.full_snapshot_required);
        assert_eq!(reality.calibration_source, "none");
    }

    #[test]
    fn rebase_required_audits_force_full_snapshot_required() {
        let snapshot = build_snapshot(
            &profile(),
            ProfileQualityInputRows {
                action: Vec::new(),
                observations: Vec::new(),
                events: Vec::new(),
                reality: vec![row(
                    "reality/audit/v1/demo.profile/audit-drift",
                    json!({
                        "audit_id": "audit-drift",
                        "epoch_id": "epoch-c",
                        "compared_seq_end": 1,
                        "drift_status": "rebase_required",
                        "rebase_required": true,
                        "drift_items": [{
                            "source_refs": [{"surface": "window"}]
                        }]
                    }),
                )],
            },
            &params(),
            1_780_000_000_000,
        );

        let reality = snapshot.reality_evidence;
        assert_eq!(reality.audit_rows, 1);
        assert_eq!(reality.drift_audit_rows, 1);
        assert_eq!(reality.rebase_required_rows, 1);
        assert!(reality.full_snapshot_required);
        assert_eq!(
            reality.audit_drift_status_counts.get("rebase_required"),
            Some(&1)
        );
    }

    #[test]
    fn reality_parser_ignores_non_reality_keys() {
        assert!(
            parse_reality_row(b"profile_quality/v1/demo.profile", br#"{}"#)
                .unwrap()
                .is_none()
        );
    }

    fn params() -> ProfileQualityRefreshParams {
        ProfileQualityRefreshParams {
            profile_id: "demo.profile".to_owned(),
            max_audit_rows: 100,
            stale_after_ns: 86_400_000_000_000,
            manual_fsv_evidence_ref: Some("issue-543-test".to_owned()),
        }
    }

    fn profile() -> ProfileStatus {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "registry.quality_signal".to_owned(),
            "profile_quality.demo.profile".to_owned(),
        );
        metadata.insert(
            "registry.compatibility_target".to_owned(),
            "demo.profile.level2".to_owned(),
        );
        ProfileStatus {
            id: "demo.profile".to_owned(),
            label: "Demo Profile".to_owned(),
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
            active: true,
            schema_version: 2,
            matches: Vec::new(),
            metadata,
            source_path: PathBuf::from("profiles/demo.profile.toml"),
        }
    }

    fn row(key: &str, value: serde_json::Value) -> (Vec<u8>, Vec<u8>) {
        (key.as_bytes().to_vec(), serde_json::to_vec(&value).unwrap())
    }
}

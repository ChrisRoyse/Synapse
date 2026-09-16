use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::Utc;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rmcp::{ErrorData, model::ErrorCode, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::{ProfileId, SCHEMA_VERSION, error_codes};
use synapse_profiles::{
    PackageSignature, ProfileError, ProfilePackageManifest, package_manifest_digest,
    package_signature_payload, package_signature_payload_digest, parse_package_manifest_bytes,
    parse_package_manifest_bytes_with_digest, parse_profile_file,
};
use synapse_reflex::ReflexRuntime;
use synapse_storage::{cf, decode_json, encode_json};

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

const REGISTRY_PREFIX: &str = "profile_registry/v1/";
const SOURCE_PREFIX: &str = "profile_registry/v1/source/";
const PACKAGE_PREFIX: &str = "profile_registry/v1/package/";
const PROFILE_PREFIX: &str = "profile_registry/v1/profile/";
const INSTALLED_PREFIX: &str = "profile_registry/v1/installed/";
const COMPAT_PREFIX: &str = "profile_registry/v1/compat/";
const CURATED_TARGET_PREFIX: &str = "profile_registry/v1/curated_target/";
const QUALITY_LINK_PREFIX: &str = "profile_registry/v1/quality_link/";
const TRUST_ROOT_PREFIX: &str = "profile_registry/v1/trust_root/";
const QUARANTINE_PREFIX: &str = "profile_registry/v1/quarantine/";
const ROLLBACK_PREFIX: &str = "profile_registry/v1/rollback/";
const CONTRIBUTION_PREFIX: &str = "profile_registry/v1/contribution/";
const HEAD_PREFIX: &str = "profile_registry/v1/head/";
const QUALITY_PREFIX: &str = "profile_quality/v1/";
const AUDIT_EXPORT_CONSENT_PREFIX: &str = "audit_export/v1/consent/";
const DEFAULT_SOURCE_ID: &str = "registry.local";
const DEFAULT_INSTALL_TRUST_POLICY: &str = "local_first";
const DEFAULT_BUNDLE_KIND: &str = "registry";
const REGISTRY_BUNDLE_KIND: &str = "registry";
const CONTRIBUTION_BUNDLE_KIND: &str = "contribution";
const DEFAULT_SEARCH_LIMIT: u32 = 100;
const MAX_SEARCH_LIMIT: u32 = 1000;
const DEFAULT_REPORT_AUDIT_ROWS: u32 = 100;
const MAX_REPORT_AUDIT_ROWS: u32 = 1000;
const DEFAULT_CONTRIBUTION_AUDIT_ROWS: u32 = 100;
const MAX_CONTRIBUTION_AUDIT_ROWS: u32 = 1000;
const MAX_CONTRIBUTION_IMPORT_ROWS: usize = 1000;
const HIGH_QUALITY_SCORE_THRESHOLD: u64 = 90;
const MIN_HIGH_QUALITY_EVIDENCE_ROWS: u64 = 10;
const VALUE_PREFIX_CHARS: usize = 1024;
const CONTRIBUTION_POISON_MARKERS: &[&str] = &[
    "ignore previous instructions",
    "ignore all previous instructions",
    "disregard previous instructions",
    "system prompt",
    "developer message",
    "tool call",
    "exfiltrate",
    "reveal secrets",
    "jailbreak",
    "prompt injection",
    "run powershell",
    "execute shell",
    "download and run",
    "act_run_shell",
];
const SYNTHETIC_FIXTURE_SIGNER_ID: &str = "synapse.fixture.signer";
const SYNTHETIC_FIXTURE_PUBLIC_KEY_HEX: &str =
    "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";
const SYNTHETIC_FIXTURE_KEY_ID: &str =
    "sha256:56475aa75463474c0285df5dbf2bcab73da651358839e9b77481b2eab107708c";

type EncodedRow = (Vec<u8>, Vec<u8>);
type EncodedRows = Vec<EncodedRow>;

struct RegistryExportInputs {
    profile_rows: EncodedRows,
    kv_rows: EncodedRows,
    audit_rows: EncodedRows,
    quality_row: Option<EncodedRow>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProfileRegistryQueryView {
    Search,
    Inspect,
    Report,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryQueryParams {
    pub view: ProfileRegistryQueryView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_kind: Option<String>,
    #[serde(default)]
    pub include_disabled: bool,
    #[serde(default = "default_search_limit")]
    #[schemars(default = "default_search_limit", range(min = 1, max = 1000))]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<ProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_profile_id: Option<ProfileId>,
    #[serde(default = "default_report_audit_rows")]
    #[schemars(default = "default_report_audit_rows", range(min = 1, max = 1000))]
    pub max_audit_rows: u32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryInstallParams {
    pub manifest_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_manifest_digest: Option<String>,
    #[serde(default = "default_source_id")]
    #[schemars(default = "default_source_id")]
    pub source_id: String,
    #[serde(default = "default_install_trust_policy")]
    #[schemars(default = "default_install_trust_policy")]
    pub trust_policy: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryDisableParams {
    pub profile_id: ProfileId,
    #[serde(default = "default_disabled_state")]
    #[schemars(default = "default_disabled_state")]
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryExportParams {
    pub output_path: String,
    #[serde(default = "default_bundle_kind")]
    #[schemars(default = "default_bundle_kind")]
    pub bundle_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<ProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_kind: Option<String>,
    #[serde(default)]
    pub include_disabled: bool,
    #[serde(default = "default_include_audit_evidence")]
    #[schemars(default = "default_include_audit_evidence")]
    pub include_audit_evidence: bool,
    #[serde(default = "default_include_quality_summary")]
    #[schemars(default = "default_include_quality_summary")]
    pub include_quality_summary: bool,
    #[serde(default = "default_contribution_audit_rows")]
    #[schemars(
        default = "default_contribution_audit_rows",
        range(min = 1, max = 1000)
    )]
    pub max_audit_rows: u32,
    #[serde(default = "default_search_limit")]
    #[schemars(default = "default_search_limit", range(min = 1, max = 1000))]
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryImportParams {
    pub bundle_path: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryRollbackParams {
    pub profile_id: ProfileId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_package_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_package_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditIntelligenceQueryParams {
    pub profile_id: ProfileId,
    #[serde(default = "default_search_limit")]
    #[schemars(default = "default_search_limit", range(min = 1, max = 1000))]
    pub max_rows: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryRowSummary {
    pub cf_name: String,
    pub key: String,
    pub key_hex: String,
    pub row_kind: Option<String>,
    pub row_id: Option<String>,
    pub source_id: Option<String>,
    pub state: Option<String>,
    pub profile_id: Option<ProfileId>,
    pub profile_version: Option<String>,
    pub package_id: Option<String>,
    pub package_version: Option<String>,
    pub updated_at: Option<String>,
    pub value_len_bytes: u64,
    pub value_utf8_prefix: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryStoredRow {
    pub summary: ProfileRegistryRowSummary,
    pub value: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistrySearchResponse {
    pub cf_name: String,
    pub prefix: String,
    pub query: Option<String>,
    pub row_kind: Option<String>,
    pub include_disabled: bool,
    pub limit: u32,
    pub total_matched: u64,
    pub rows: Vec<ProfileRegistryRowSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryInspectResponse {
    pub cf_name: String,
    pub row_key: String,
    pub found: bool,
    pub row: Option<ProfileRegistryStoredRow>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportResponse {
    pub schema_version: u32,
    pub generated_at: String,
    pub storage_path: String,
    pub profile_id: Option<ProfileId>,
    pub limit: u32,
    pub max_audit_rows: u32,
    pub row_counts: BTreeMap<String, u64>,
    pub source_of_truth: Vec<ProfileRegistryReportPointer>,
    pub registry_rows_scanned: u64,
    pub registry_heads: Vec<ProfileRegistryRowSummary>,
    pub installed_profiles: Vec<ProfileRegistryReportInstalledProfile>,
    pub packages: Vec<ProfileRegistryReportPackage>,
    pub curated_targets: Vec<ProfileRegistryReportCuratedTarget>,
    pub quarantined_packages: Vec<ProfileRegistryReportQuarantine>,
    pub rollback_rows: Vec<ProfileRegistryReportRollback>,
    pub quality_snapshots: Vec<ProfileRegistryReportQualitySnapshot>,
    pub consent: Vec<ProfileRegistryReportConsent>,
    pub recent_audit: ProfileRegistryReportAudit,
    pub explicit_controls: Vec<ProfileRegistryReportControl>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryQueryResponse {
    pub view: ProfileRegistryQueryView,
    pub search: Option<ProfileRegistrySearchResponse>,
    pub inspect: Option<ProfileRegistryInspectResponse>,
    pub report: Option<ProfileRegistryReportResponse>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportPointer {
    pub surface: String,
    pub cf_name: Option<String>,
    pub key: Option<String>,
    pub key_hex: Option<String>,
    pub path: Option<String>,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportInstalledProfile {
    pub row_key: String,
    pub profile_id: ProfileId,
    pub active_profile_version: Option<String>,
    pub installed_package_id: Option<String>,
    pub installed_package_version: Option<String>,
    pub activation_state: Option<String>,
    pub state: Option<String>,
    pub trust_status: Option<String>,
    pub signature_status: Option<String>,
    pub signer_id: Option<String>,
    pub trust_root_key: Option<String>,
    pub installed_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportPackage {
    pub row_key: String,
    pub package_id: Option<String>,
    pub package_version: Option<String>,
    pub profile_id: Option<ProfileId>,
    pub profile_version: Option<String>,
    pub state: Option<String>,
    pub trust_status: Option<String>,
    pub signature_status: Option<String>,
    pub moderation_status: Option<String>,
    pub signer_id: Option<String>,
    pub trust_root_key: Option<String>,
    pub manifest_digest: Option<String>,
    pub target_ids: Vec<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportCuratedTarget {
    pub row_key: String,
    pub seed_set_id: Option<String>,
    pub target_id: Option<String>,
    pub tier: Option<String>,
    pub priority: Option<String>,
    pub status: Option<String>,
    pub backlog_issue: Option<String>,
    pub profile_id: Option<ProfileId>,
    pub package_id: Option<String>,
    pub package_version: Option<String>,
    pub profile_quality_key: Option<String>,
    pub minimum_manual_fsv: Vec<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportQuarantine {
    pub row_key: String,
    pub package_id: Option<String>,
    pub package_version: Option<String>,
    pub profile_id: Option<ProfileId>,
    pub profile_version: Option<String>,
    pub quarantine_reason: Option<String>,
    pub error_code: Option<String>,
    pub signature_status: Option<String>,
    pub signer_id: Option<String>,
    pub state: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportRollback {
    pub row_key: String,
    pub profile_id: Option<ProfileId>,
    pub from_package_id: Option<String>,
    pub from_package_version: Option<String>,
    pub to_package_id: Option<String>,
    pub to_package_version: Option<String>,
    pub reason: Option<String>,
    pub trust_status: Option<String>,
    pub signature_status: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportQualitySnapshot {
    pub row_key: String,
    pub key_hex: String,
    pub profile_id: Option<ProfileId>,
    pub generated_at_ns: Option<u64>,
    pub evidence_hash: Option<String>,
    pub manual_fsv_evidence_ref: Option<String>,
    pub score_0_100: Option<u64>,
    pub sample_size: Option<u64>,
    pub audit_rows_scanned: Option<u64>,
    pub audit_rows_profile_relevant: Option<u64>,
    pub audit_rows_stale: Option<u64>,
    pub observation_rows_scanned: Option<u64>,
    pub observation_rows_profile_relevant: Option<u64>,
    pub event_rows_scanned: Option<u64>,
    pub event_rows_profile_relevant: Option<u64>,
    pub stale_after_ns: Option<u64>,
    pub stale_evidence_present: bool,
    pub quality_signal: Option<String>,
    pub observed_target_ids: BTreeMap<String, u64>,
    pub observed_event_kinds: BTreeMap<String, u64>,
    pub observed_log_event_kinds: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportConsent {
    pub row_key: String,
    pub key_hex: String,
    pub profile_id: ProfileId,
    pub enabled: bool,
    pub state: Option<String>,
    pub redaction_policy: Option<String>,
    pub external_sharing_allowed: bool,
    pub matching_audit_rows: u64,
    pub audit_export_status: String,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportAudit {
    pub cf_name: String,
    pub rows_scanned: u64,
    pub matching_rows: u64,
    pub by_status: BTreeMap<String, u64>,
    pub by_tool: BTreeMap<String, u64>,
    pub by_error_code: BTreeMap<String, u64>,
    pub latest_rows: Vec<ProfileRegistryReportAuditRow>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportAuditRow {
    pub key_hex: String,
    pub profile_id: Option<ProfileId>,
    pub foreground_profile_id: Option<ProfileId>,
    pub tool: Option<String>,
    pub status: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryReportControl {
    pub tool: String,
    pub purpose: String,
    pub writes: String,
    pub requires_explicit_operator_trigger: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryInstallResponse {
    pub operation: String,
    pub source_id: String,
    pub package_id: String,
    pub package_version: String,
    pub profile_id: ProfileId,
    pub profile_version: String,
    pub manifest_path: String,
    pub manifest_digest: String,
    pub profile_toml_path: String,
    pub wrote_rows: bool,
    pub idempotent: bool,
    pub trust_status: String,
    pub signature_status: String,
    pub signer_id: Option<String>,
    pub trust_root_key: Option<String>,
    pub signature_payload_digest: Option<String>,
    pub cf_profile_row_keys: Vec<String>,
    pub cf_kv_row_keys: Vec<String>,
    pub row_summaries: Vec<ProfileRegistryRowSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryDisableResponse {
    pub profile_id: ProfileId,
    pub row_key: String,
    pub previous_state: Option<String>,
    pub state: String,
    pub wrote_row: bool,
    pub row: ProfileRegistryStoredRow,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryExportResponse {
    pub output_path: String,
    pub bundle_kind: String,
    pub bytes_written: u64,
    pub rows_exported: u64,
    pub audit_evidence_rows: u64,
    pub quality_summary_rows: u64,
    pub deterministic_bundle_sha256: String,
    pub registry_rows_sha256: String,
    pub audit_evidence_sha256: String,
    pub quality_summary_sha256: String,
    pub rows: Vec<ProfileRegistryRowSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryImportResponse {
    pub bundle_path: String,
    pub bundle_kind: String,
    pub rows_read: u64,
    pub cf_profile_rows_written: u64,
    pub cf_kv_rows_written: u64,
    pub duplicate_rows: u64,
    pub contribution_row_key: Option<String>,
    pub deterministic_bundle_sha256: Option<String>,
    pub rows: Vec<ProfileRegistryRowSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRegistryRollbackResponse {
    pub profile_id: ProfileId,
    pub previous_package_id: String,
    pub previous_package_version: String,
    pub rolled_back_package_id: String,
    pub rolled_back_package_version: String,
    pub row_key: String,
    pub rollback_row_key: String,
    pub wrote_row: bool,
    pub installed_row: ProfileRegistryStoredRow,
    pub rollback_row: ProfileRegistryStoredRow,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditIntelligenceQueryResponse {
    pub profile_id: ProfileId,
    pub max_rows: u32,
    pub action: AuditBucketSummary,
    pub events: AuditBucketSummary,
    pub reflexes: AuditBucketSummary,
    pub sessions: AuditSessionSummary,
    pub quality_snapshot_key: String,
    pub quality_snapshot: Option<Value>,
    pub learning_candidates: Vec<AuditLearningCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditBucketSummary {
    pub cf_name: String,
    pub rows_scanned: u64,
    pub matching_rows: u64,
    pub by_status: BTreeMap<String, u64>,
    pub by_kind_or_tool: BTreeMap<String, u64>,
    pub by_error_code: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditSessionSummary {
    pub cf_name: String,
    pub rows_scanned: u64,
    pub matching_rows: u64,
    pub session_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditLearningCandidate {
    pub kind: String,
    pub evidence_count: u64,
    pub rationale: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileRegistryExportBundle {
    schema_version: u32,
    #[serde(default = "default_bundle_kind")]
    bundle_kind: String,
    exported_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_id: Option<ProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deterministic_bundle_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registry_rows_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audit_evidence_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quality_summary_sha256: Option<String>,
    #[serde(default)]
    merge_rules: Vec<String>,
    rows: Vec<ProfileRegistryBundleRow>,
    #[serde(default)]
    audit_evidence: Vec<ProfileRegistryAuditEvidenceRow>,
    #[serde(default)]
    quality_summaries: Vec<ProfileRegistryQualitySummaryRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileRegistryBundleRow {
    cf_name: String,
    key: String,
    value: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileRegistryAuditEvidenceRow {
    cf_name: String,
    key_hex: String,
    value_sha256: String,
    profile_id: Option<ProfileId>,
    foreground_profile_id: Option<ProfileId>,
    foreground_process_name: Option<String>,
    tool: Option<String>,
    status: Option<String>,
    error_code: Option<String>,
    backend_used: Option<String>,
    profile_schema_version: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileRegistryQualitySummaryRow {
    cf_name: String,
    key: String,
    key_hex: String,
    value_sha256: String,
    profile_id: Option<ProfileId>,
    evidence_hash: Option<String>,
    score_0_100: Option<u64>,
    sample_size: Option<u64>,
    quality_signal: Option<String>,
    reality_delta_rows: Option<u64>,
    reality_audit_rows: Option<u64>,
    reality_audited_delta_rows: Option<u64>,
    reality_rebase_required_rows: Option<u64>,
    reality_drift_rate: Option<f64>,
    delta_first_supported: Option<bool>,
    full_snapshot_required: Option<bool>,
}

#[derive(Clone, Debug)]
struct TrustRoot {
    signer_id: &'static str,
    key_id: &'static str,
    public_key_hex: &'static str,
    trust_domain: &'static str,
}

#[derive(Clone, Debug)]
struct TrustVerification {
    trust_policy_id: String,
    trust_status: String,
    signature_status: String,
    signer_id: Option<String>,
    key_id: Option<String>,
    trust_root_key: Option<String>,
    signature_payload_digest: Option<String>,
    required: bool,
}

#[derive(Clone, Debug)]
struct ContributionReview {
    review_state: &'static str,
    risk_flags: Vec<String>,
    trust_downgrade_reasons: Vec<String>,
    rank_eligible: bool,
    quality_weight: u64,
    registry_rows_accepted: u64,
    registry_rows_rejected: u64,
    external_success_evidence_rows: u64,
    local_success_evidence_rows: u64,
}

impl ContributionReview {
    fn staged(registry_rows_read: u64, external_success_evidence_rows: u64) -> Self {
        Self {
            review_state: "staged",
            risk_flags: Vec::new(),
            trust_downgrade_reasons: vec![
                "external_contribution_untrusted_until_local_review".to_owned(),
                "external_quality_claims_do_not_count_as_local_success".to_owned(),
            ],
            rank_eligible: false,
            quality_weight: 0,
            registry_rows_accepted: registry_rows_read,
            registry_rows_rejected: 0,
            external_success_evidence_rows,
            local_success_evidence_rows: 0,
        }
    }

    const fn quarantined(
        registry_rows_read: u64,
        external_success_evidence_rows: u64,
        risk_flags: Vec<String>,
        trust_downgrade_reasons: Vec<String>,
    ) -> Self {
        Self {
            review_state: "quarantined",
            risk_flags,
            trust_downgrade_reasons,
            rank_eligible: false,
            quality_weight: 0,
            registry_rows_accepted: 0,
            registry_rows_rejected: registry_rows_read,
            external_success_evidence_rows,
            local_success_evidence_rows: 0,
        }
    }

    fn is_quarantined(&self) -> bool {
        matches!(self.review_state, "quarantined")
    }
}

#[derive(Clone, Debug)]
struct TrustFailure {
    reason: &'static str,
    message: String,
    trust_policy_id: String,
    signature_status: String,
    signer_id: Option<String>,
    key_id: Option<String>,
    signature_payload_digest: Option<String>,
}

const BUILTIN_TRUST_ROOTS: &[TrustRoot] = &[TrustRoot {
    signer_id: SYNTHETIC_FIXTURE_SIGNER_ID,
    key_id: SYNTHETIC_FIXTURE_KEY_ID,
    public_key_hex: SYNTHETIC_FIXTURE_PUBLIC_KEY_HEX,
    trust_domain: "local-fixture",
}];

#[must_use]
pub const fn profile_registry_query() -> M3ToolStub {
    M3ToolStub::new("profile_registry_query")
}

#[must_use]
pub const fn profile_registry_install() -> M3ToolStub {
    M3ToolStub::new("profile_registry_install")
}

#[must_use]
pub const fn profile_registry_disable() -> M3ToolStub {
    M3ToolStub::new("profile_registry_disable")
}

#[must_use]
pub const fn profile_registry_export() -> M3ToolStub {
    M3ToolStub::new("profile_registry_export")
}

#[must_use]
pub const fn profile_registry_import() -> M3ToolStub {
    M3ToolStub::new("profile_registry_import")
}

#[must_use]
pub const fn profile_registry_rollback() -> M3ToolStub {
    M3ToolStub::new("profile_registry_rollback")
}

#[must_use]
pub const fn audit_intelligence_query() -> M3ToolStub {
    M3ToolStub::new("audit_intelligence_query")
}

#[must_use]
pub fn required_permissions_query(_params: &ProfileRegistryQueryParams) -> RequiredPermissions {
    required([Permission::ReadProfile, Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_install(_params: &ProfileRegistryInstallParams) -> RequiredPermissions {
    required([
        Permission::ReadProfile,
        Permission::ReadStorage,
        Permission::WriteStorage,
    ])
}

#[must_use]
pub fn required_permissions_disable(_params: &ProfileRegistryDisableParams) -> RequiredPermissions {
    required([
        Permission::ReadProfile,
        Permission::ReadStorage,
        Permission::WriteStorage,
    ])
}

#[must_use]
pub fn required_permissions_export(_params: &ProfileRegistryExportParams) -> RequiredPermissions {
    required([Permission::ReadProfile, Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_import(_params: &ProfileRegistryImportParams) -> RequiredPermissions {
    required([
        Permission::ReadProfile,
        Permission::ReadStorage,
        Permission::WriteStorage,
    ])
}

#[must_use]
pub fn required_permissions_rollback(
    _params: &ProfileRegistryRollbackParams,
) -> RequiredPermissions {
    required([
        Permission::ReadProfile,
        Permission::ReadStorage,
        Permission::WriteStorage,
    ])
}

#[must_use]
pub fn required_permissions_audit(_params: &AuditIntelligenceQueryParams) -> RequiredPermissions {
    required([Permission::ReadProfile, Permission::ReadStorage])
}

pub fn query_registry(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryQueryParams,
) -> Result<ProfileRegistryQueryResponse, ErrorData> {
    match params.view {
        ProfileRegistryQueryView::Search => {
            let search = search_registry(reflex_runtime, params)?;
            Ok(ProfileRegistryQueryResponse {
                view: ProfileRegistryQueryView::Search,
                search: Some(search),
                inspect: None,
                report: None,
            })
        }
        ProfileRegistryQueryView::Inspect => {
            let inspect = inspect_registry(reflex_runtime, params)?;
            Ok(ProfileRegistryQueryResponse {
                view: ProfileRegistryQueryView::Inspect,
                search: None,
                inspect: Some(inspect),
                report: None,
            })
        }
        ProfileRegistryQueryView::Report => {
            let report = report_profile_registry(reflex_runtime, params)?;
            Ok(ProfileRegistryQueryResponse {
                view: ProfileRegistryQueryView::Report,
                search: None,
                inspect: None,
                report: Some(report),
            })
        }
    }
}

fn search_registry(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryQueryParams,
) -> Result<ProfileRegistrySearchResponse, ErrorData> {
    validate_limit(params.limit)?;
    if let Some(kind) = &params.row_kind {
        validate_non_empty("row_kind", kind)?;
    }
    let runtime = lock_runtime(reflex_runtime, "searching profile registry")?;
    let rows = runtime
        .storage_cf_prefix_rows(cf::CF_PROFILES, REGISTRY_PREFIX.as_bytes(), usize::MAX)
        .map_err(storage_error)?;
    drop(runtime);
    let query = normalized_query(params.query.as_deref());
    let mut matched = Vec::new();
    let mut total_matched = 0_u64;
    for (key, value) in rows {
        let summary = row_summary(cf::CF_PROFILES, &key, &value);
        if !row_filter_matches(
            &summary,
            &value,
            query.as_deref(),
            params.row_kind.as_deref(),
            params.include_disabled,
        ) {
            continue;
        }
        total_matched += 1;
        if matched.len() < params.limit as usize {
            matched.push(summary);
        }
    }
    Ok(ProfileRegistrySearchResponse {
        cf_name: cf::CF_PROFILES.to_owned(),
        prefix: REGISTRY_PREFIX.to_owned(),
        query,
        row_kind: params.row_kind.clone(),
        include_disabled: params.include_disabled,
        limit: params.limit,
        total_matched,
        rows: matched,
    })
}

fn inspect_registry(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryQueryParams,
) -> Result<ProfileRegistryInspectResponse, ErrorData> {
    let (cf_name, key) = inspect_key(params)?;
    let runtime = lock_runtime(reflex_runtime, "inspecting profile registry")?;
    let value = if cf_name == cf::CF_KV {
        runtime.storage_kv_row(key.as_bytes())
    } else {
        runtime.storage_profile_row(key.as_bytes())
    }
    .map_err(storage_error)?;
    drop(runtime);
    let row = value
        .as_ref()
        .map(|value| stored_row(cf_name, key.as_bytes(), value))
        .transpose()?;
    Ok(ProfileRegistryInspectResponse {
        cf_name: cf_name.to_owned(),
        row_key: key,
        found: row.is_some(),
        row,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "operator report intentionally assembles all registry, audit, quality, and consent surfaces in one read-only MCP operation"
)]
fn report_profile_registry(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryQueryParams,
) -> Result<ProfileRegistryReportResponse, ErrorData> {
    validate_report_params(params)?;
    let runtime = lock_runtime(reflex_runtime, "reporting profile registry state")?;
    let storage_path = runtime.storage_path().display().to_string();
    let row_counts = runtime.storage_cf_row_counts().map_err(storage_error)?;
    let registry_rows = runtime
        .storage_cf_prefix_rows(cf::CF_PROFILES, REGISTRY_PREFIX.as_bytes(), usize::MAX)
        .map_err(storage_error)?;
    let head_rows = runtime
        .storage_cf_prefix_rows(cf::CF_KV, HEAD_PREFIX.as_bytes(), usize::MAX)
        .map_err(storage_error)?;
    let quality_rows = runtime
        .storage_cf_prefix_rows(cf::CF_PROFILES, QUALITY_PREFIX.as_bytes(), usize::MAX)
        .map_err(storage_error)?;
    let consent_rows = runtime
        .storage_cf_prefix_rows(
            cf::CF_KV,
            AUDIT_EXPORT_CONSENT_PREFIX.as_bytes(),
            usize::MAX,
        )
        .map_err(storage_error)?;
    let audit_rows = runtime
        .storage_cf_tail_rows(cf::CF_ACTION_LOG, params.max_audit_rows as usize)
        .map_err(storage_error)?;
    drop(runtime);

    let profile_filter = params.profile_id.as_deref();
    let limit = params.limit as usize;
    let mut source_of_truth = report_base_pointers(&storage_path);
    let mut registry_heads = Vec::new();
    for (key, value) in head_rows {
        let row = stored_row(cf::CF_KV, &key, &value)?;
        if registry_heads.len() < limit {
            push_sot_row(
                &mut source_of_truth,
                "registry_head",
                cf::CF_KV,
                &key,
                &storage_path,
                "registry source head pointer",
            );
            registry_heads.push(row.summary);
        }
    }

    let registry_rows_scanned = registry_rows.len() as u64;
    let mut installed_profiles = Vec::new();
    let mut packages = Vec::new();
    let mut curated_targets = Vec::new();
    let mut quarantined_packages = Vec::new();
    let mut rollback_rows = Vec::new();
    for (key, value) in registry_rows {
        let row = stored_row(cf::CF_PROFILES, &key, &value)?;
        if !report_row_matches_profile(&row, profile_filter) {
            continue;
        }
        match row.value.get("row_kind").and_then(Value::as_str) {
            Some("installed_profile") if installed_profiles.len() < limit => {
                if let Some(item) = report_installed_profile(&row) {
                    push_sot_row(
                        &mut source_of_truth,
                        "installed_profile",
                        cf::CF_PROFILES,
                        &key,
                        &storage_path,
                        "installed profile registry row",
                    );
                    installed_profiles.push(item);
                }
            }
            Some("profile_package") if packages.len() < limit => {
                push_sot_row(
                    &mut source_of_truth,
                    "profile_package",
                    cf::CF_PROFILES,
                    &key,
                    &storage_path,
                    "profile package registry row",
                );
                packages.push(report_package(&row));
            }
            Some("curated_profile_target") if curated_targets.len() < limit => {
                push_sot_row(
                    &mut source_of_truth,
                    "curated_profile_target",
                    cf::CF_PROFILES,
                    &key,
                    &storage_path,
                    "curated starter target row",
                );
                curated_targets.push(report_curated_target(&row));
            }
            Some("profile_package_quarantine") if quarantined_packages.len() < limit => {
                push_sot_row(
                    &mut source_of_truth,
                    "profile_package_quarantine",
                    cf::CF_PROFILES,
                    &key,
                    &storage_path,
                    "quarantined package row",
                );
                quarantined_packages.push(report_quarantine(&row));
            }
            Some("profile_registry_rollback") if rollback_rows.len() < limit => {
                push_sot_row(
                    &mut source_of_truth,
                    "profile_registry_rollback",
                    cf::CF_PROFILES,
                    &key,
                    &storage_path,
                    "rollback audit row",
                );
                rollback_rows.push(report_rollback(&row));
            }
            _ => {}
        }
    }

    let recent_audit = summarize_report_audit(&audit_rows, profile_filter, limit)?;
    let mut quality_snapshots = Vec::new();
    for (key, value) in quality_rows {
        let quality_profile_id = quality_row_profile_id(&key, &value)?;
        if profile_filter
            .is_some_and(|profile_id| quality_profile_id.as_deref() != Some(profile_id))
        {
            continue;
        }
        if quality_snapshots.len() < limit {
            push_sot_row(
                &mut source_of_truth,
                "profile_quality_snapshot",
                cf::CF_PROFILES,
                &key,
                &storage_path,
                "profile quality snapshot row",
            );
            quality_snapshots.push(report_quality_snapshot(&key, &value)?);
        }
    }

    let mut consent = Vec::new();
    let mut matched_consent = false;
    for (key, value) in consent_rows {
        let consent_profile_id = consent_row_profile_id(&key, &value)?;
        if profile_filter.is_some_and(|profile_id| consent_profile_id != profile_id) {
            continue;
        }
        matched_consent = true;
        if consent.len() < limit {
            let matching_audit_rows = count_matching_audit_rows(&audit_rows, &consent_profile_id)?;
            push_sot_row(
                &mut source_of_truth,
                "audit_export_consent",
                cf::CF_KV,
                &key,
                &storage_path,
                "audit export consent row",
            );
            consent.push(report_consent_row(
                &key,
                &value,
                consent_profile_id,
                matching_audit_rows,
            )?);
        }
    }
    if let Some(profile_id) = &params.profile_id
        && !matched_consent
        && consent.len() < limit
    {
        let key = format!("{AUDIT_EXPORT_CONSENT_PREFIX}{profile_id}");
        push_sot_row(
            &mut source_of_truth,
            "audit_export_consent_missing",
            cf::CF_KV,
            key.as_bytes(),
            &storage_path,
            "expected audit export consent row is absent",
        );
        consent.push(missing_consent_report(
            profile_id,
            &key,
            count_matching_audit_rows(&audit_rows, profile_id)?,
        ));
    }

    Ok(ProfileRegistryReportResponse {
        schema_version: SCHEMA_VERSION,
        generated_at: Utc::now().to_rfc3339(),
        storage_path,
        profile_id: params.profile_id.clone(),
        limit: params.limit,
        max_audit_rows: params.max_audit_rows,
        row_counts,
        source_of_truth,
        registry_rows_scanned,
        registry_heads,
        installed_profiles,
        packages,
        curated_targets,
        quarantined_packages,
        rollback_rows,
        quality_snapshots,
        consent,
        recent_audit,
        explicit_controls: registry_report_controls(),
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "single MCP operation keeps manifest validation, duplicate handling, row write, and readback together"
)]
pub fn install_registry_package(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryInstallParams,
) -> Result<ProfileRegistryInstallResponse, ErrorData> {
    validate_registry_id("source_id", &params.source_id)?;
    validate_install_trust_policy(&params.trust_policy)?;
    let manifest_path = required_path("manifest_path", &params.manifest_path)?;
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        mcp_error(
            error_codes::PROFILE_PARSE_ERROR,
            format!(
                "profile_registry_install could not read manifest {}: {error}",
                manifest_path.display()
            ),
        )
    })?;
    let manifest_digest = package_manifest_digest(&manifest_bytes);
    let manifest = parse_manifest(&manifest_path, &manifest_bytes, params)?;
    let trust = match verify_manifest_trust(&manifest, params) {
        Ok(trust) => trust,
        Err(failure) => {
            let updated_at = Utc::now().to_rfc3339();
            let quarantine_key = quarantine_key(
                &manifest.package_id,
                &manifest.package_version,
                &manifest_digest,
            );
            let row = quarantine_row(
                &manifest,
                &manifest_path,
                &manifest_digest,
                &params.source_id,
                &updated_at,
                &failure,
            );
            let runtime = lock_runtime(reflex_runtime, "quarantining failed profile package")?;
            runtime
                .storage_put_profile_rows(vec![encoded_row(quarantine_key.clone(), &row)?])
                .map_err(storage_error)?;
            let stored = runtime
                .storage_profile_row(quarantine_key.as_bytes())
                .map_err(storage_error)?;
            drop(runtime);
            let quarantine_readback = stored
                .as_ref()
                .map(|value| row_summary(cf::CF_PROFILES, quarantine_key.as_bytes(), value));
            return Err(trust_error(
                &failure,
                &quarantine_key,
                quarantine_readback.as_ref(),
            ));
        }
    };
    let profile_toml_path = resolve_package_file(&manifest_path, &manifest.files.profile_toml);
    let loaded_profile = parse_profile_file(&profile_toml_path).map_err(profile_error)?;
    if loaded_profile.profile.id != manifest.profile_id {
        return Err(registry_error(
            "profile_toml_id_mismatch",
            format!(
                "manifest profile_id {} does not match profile TOML id {}",
                manifest.profile_id, loaded_profile.profile.id
            ),
        ));
    }
    let package_key = package_key(&manifest.package_id, &manifest.package_version);
    let updated_at = Utc::now().to_rfc3339();
    let runtime = lock_runtime(reflex_runtime, "installing profile registry package")?;
    if let Some(existing) = runtime
        .storage_profile_row(package_key.as_bytes())
        .map_err(storage_error)?
    {
        let existing_value = decode_json::<Value>(&existing).map_err(decode_error)?;
        let existing_digest = existing_value
            .get("manifest_digest")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if existing_digest == manifest_digest {
            let row = row_summary(cf::CF_PROFILES, package_key.as_bytes(), &existing);
            return Ok(ProfileRegistryInstallResponse {
                operation: "install_or_update".to_owned(),
                source_id: params.source_id.clone(),
                package_id: manifest.package_id,
                package_version: manifest.package_version,
                profile_id: manifest.profile_id,
                profile_version: manifest.profile_version,
                manifest_path: manifest_path.display().to_string(),
                manifest_digest,
                profile_toml_path: profile_toml_path.display().to_string(),
                wrote_rows: false,
                idempotent: true,
                trust_status: existing_value
                    .get("trust_status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                signature_status: existing_value
                    .get("signature_status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                signer_id: existing_value
                    .get("signer_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                trust_root_key: existing_value
                    .get("trust_root_key")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                signature_payload_digest: existing_value
                    .get("signature_payload_digest")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                cf_profile_row_keys: vec![package_key],
                cf_kv_row_keys: Vec::new(),
                row_summaries: vec![row],
            });
        }
        return Err(registry_error(
            "duplicate_package_version_conflict",
            format!(
                "package {}@{} already exists with manifest_digest {}; new digest is {}",
                manifest.package_id, manifest.package_version, existing_digest, manifest_digest
            ),
        ));
    }

    let quality_row = runtime
        .storage_profile_row(quality_key(&manifest.profile_id).as_bytes())
        .map_err(storage_error)?;
    let mut profile_rows = registry_rows(
        &manifest,
        &manifest_path,
        &manifest_digest,
        &profile_toml_path,
        &params.source_id,
        &updated_at,
        &trust,
        quality_row.as_deref(),
    )?;
    let kv_rows = vec![head_row(
        &manifest,
        &manifest_digest,
        &params.source_id,
        &updated_at,
        &trust,
    )?];
    let profile_row_keys = profile_rows
        .iter()
        .map(|(key, _value)| String::from_utf8_lossy(key).into_owned())
        .collect::<Vec<_>>();
    let kv_row_keys = kv_rows
        .iter()
        .map(|(key, _value)| String::from_utf8_lossy(key).into_owned())
        .collect::<Vec<_>>();
    runtime
        .storage_put_profile_rows(std::mem::take(&mut profile_rows))
        .map_err(storage_error)?;
    runtime
        .storage_put_kv_rows(kv_rows)
        .map_err(storage_error)?;
    let mut summaries = Vec::new();
    for key in &profile_row_keys {
        if let Some(value) = runtime
            .storage_profile_row(key.as_bytes())
            .map_err(storage_error)?
        {
            summaries.push(row_summary(cf::CF_PROFILES, key.as_bytes(), &value));
        }
    }
    for key in &kv_row_keys {
        if let Some(value) = runtime
            .storage_kv_row(key.as_bytes())
            .map_err(storage_error)?
        {
            summaries.push(row_summary(cf::CF_KV, key.as_bytes(), &value));
        }
    }
    drop(runtime);
    Ok(ProfileRegistryInstallResponse {
        operation: "install_or_update".to_owned(),
        source_id: params.source_id.clone(),
        package_id: manifest.package_id,
        package_version: manifest.package_version,
        profile_id: manifest.profile_id,
        profile_version: manifest.profile_version,
        manifest_path: manifest_path.display().to_string(),
        manifest_digest,
        profile_toml_path: profile_toml_path.display().to_string(),
        wrote_rows: true,
        idempotent: false,
        trust_status: trust.trust_status,
        signature_status: trust.signature_status,
        signer_id: trust.signer_id,
        trust_root_key: trust.trust_root_key,
        signature_payload_digest: trust.signature_payload_digest,
        cf_profile_row_keys: profile_row_keys,
        cf_kv_row_keys: kv_row_keys,
        row_summaries: summaries,
    })
}

pub fn disable_registry_profile(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryDisableParams,
) -> Result<ProfileRegistryDisableResponse, ErrorData> {
    validate_disabled_state(&params.state)?;
    let key = installed_key(&params.profile_id);
    let runtime = lock_runtime(reflex_runtime, "disabling profile registry package")?;
    let existing = runtime
        .storage_profile_row(key.as_bytes())
        .map_err(storage_error)?
        .ok_or_else(|| {
            registry_error(
                "installed_profile_missing",
                format!(
                    "installed profile row for {} was not found",
                    params.profile_id
                ),
            )
        })?;
    let mut value = decode_json::<Value>(&existing).map_err(decode_error)?;
    let previous_state = value
        .get("state")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let updated_at = Utc::now().to_rfc3339();
    set_object_field(&mut value, "state", json!(params.state));
    set_object_field(&mut value, "activation_state", json!(params.state));
    set_object_field(&mut value, "updated_at", json!(updated_at));
    set_object_field(&mut value, "disable_reason", json!(params.reason));
    if params.state == "removed" {
        set_object_field(&mut value, "removed_at", json!(updated_at));
    } else {
        set_object_field(&mut value, "disabled_at", json!(updated_at));
    }
    let encoded = encode_json(&value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("installed registry row encode failed: {error}"),
        )
    })?;
    runtime
        .storage_put_profile_rows(vec![(key.clone().into_bytes(), encoded)])
        .map_err(storage_error)?;
    let stored = runtime
        .storage_profile_row(key.as_bytes())
        .map_err(storage_error)?
        .ok_or_else(|| {
            registry_error(
                "installed_profile_write_missing",
                "installed profile row did not persist",
            )
        })?;
    drop(runtime);
    let row = stored_row(cf::CF_PROFILES, key.as_bytes(), &stored)?;
    Ok(ProfileRegistryDisableResponse {
        profile_id: params.profile_id.clone(),
        row_key: key,
        previous_state,
        state: params.state.clone(),
        wrote_row: true,
        row,
    })
}

pub fn export_registry(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryExportParams,
) -> Result<ProfileRegistryExportResponse, ErrorData> {
    validate_limit(params.limit)?;
    validate_contribution_audit_rows(params.max_audit_rows)?;
    let bundle_kind = normalized_bundle_kind(&params.bundle_kind)?;
    if bundle_kind == CONTRIBUTION_BUNDLE_KIND && params.profile_id.is_none() {
        return Err(registry_error(
            "contribution_profile_id_missing",
            "profile_registry_export bundle_kind=contribution requires profile_id",
        ));
    }
    let output_path = required_path("output_path", &params.output_path)?;
    let runtime = lock_runtime(reflex_runtime, "exporting profile registry")?;
    let inputs = registry_export_inputs(&runtime, params, bundle_kind.as_str())?;
    drop(runtime);
    let (mut bundle_rows, summaries) = collect_export_bundle_rows(
        params,
        bundle_kind.as_str(),
        inputs.profile_rows,
        inputs.kv_rows,
    )?;
    sort_bundle_rows(&mut bundle_rows);
    let audit_evidence =
        collect_contribution_audit_evidence(params, bundle_kind.as_str(), inputs.audit_rows)?;
    let quality_summaries = collect_contribution_quality_summaries(inputs.quality_row)?;
    let merge_rules = merge_rules();
    let registry_rows_sha256 = hash_json(&bundle_rows)?;
    let audit_evidence_sha256 = hash_json(&audit_evidence)?;
    let quality_summary_sha256 = hash_json(&quality_summaries)?;
    let deterministic_bundle_sha256 = contribution_content_hash(
        bundle_kind.as_str(),
        params.profile_id.as_deref(),
        &merge_rules,
        &bundle_rows,
        &audit_evidence,
        &quality_summaries,
    )?;
    let bundle = ProfileRegistryExportBundle {
        schema_version: SCHEMA_VERSION,
        bundle_kind: bundle_kind.clone(),
        exported_at: Utc::now().to_rfc3339(),
        profile_id: params.profile_id.clone(),
        deterministic_bundle_sha256: Some(deterministic_bundle_sha256.clone()),
        registry_rows_sha256: Some(registry_rows_sha256.clone()),
        audit_evidence_sha256: Some(audit_evidence_sha256.clone()),
        quality_summary_sha256: Some(quality_summary_sha256.clone()),
        merge_rules,
        rows: bundle_rows,
        audit_evidence,
        quality_summaries,
    };
    let bytes = write_registry_export_bundle(&output_path, &bundle)?;
    Ok(ProfileRegistryExportResponse {
        output_path: output_path.display().to_string(),
        bundle_kind,
        bytes_written: bytes.len() as u64,
        rows_exported: summaries.len() as u64,
        audit_evidence_rows: bundle.audit_evidence.len() as u64,
        quality_summary_rows: bundle.quality_summaries.len() as u64,
        deterministic_bundle_sha256,
        registry_rows_sha256,
        audit_evidence_sha256,
        quality_summary_sha256,
        rows: summaries,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "single MCP operation keeps bundle validation, risk review, duplicate handling, row write, and response assembly together"
)]
pub fn import_registry(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryImportParams,
) -> Result<ProfileRegistryImportResponse, ErrorData> {
    let bundle_path = required_path("bundle_path", &params.bundle_path)?;
    let bytes = fs::read(&bundle_path).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "profile_registry_import could not read bundle {}: {error}",
                bundle_path.display()
            ),
        )
    })?;
    let mut bundle =
        serde_json::from_slice::<ProfileRegistryExportBundle>(&bytes).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("profile registry import bundle decode failed: {error}"),
            )
        })?;
    if bundle.schema_version != SCHEMA_VERSION {
        return Err(registry_error(
            "registry_bundle_schema_unsupported",
            format!(
                "registry bundle schema_version must be {SCHEMA_VERSION}; got {}",
                bundle.schema_version
            ),
        ));
    }
    let bundle_kind = normalized_bundle_kind(&bundle.bundle_kind)?;
    bundle.bundle_kind.clone_from(&bundle_kind);
    validate_bundle_hashes(&bundle)?;
    let mut profile_rows = Vec::new();
    let mut kv_rows = Vec::new();
    let mut summaries = Vec::new();
    for row in &bundle.rows {
        validate_bundle_row(row)?;
        let encoded = encode_json(&row.value).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("profile registry import row encode failed: {error}"),
            )
        })?;
        summaries.push(row_summary(
            row.cf_name.as_str(),
            row.key.as_bytes(),
            &encoded,
        ));
        if row.cf_name == cf::CF_PROFILES {
            profile_rows.push((row.key.clone().into_bytes(), encoded));
        } else {
            kv_rows.push((row.key.clone().into_bytes(), encoded));
        }
    }
    let contribution_review = if bundle_kind == CONTRIBUTION_BUNDLE_KIND {
        let review = review_contribution_bundle(&bundle, summaries.len() as u64);
        if review.is_quarantined() {
            profile_rows.clear();
            kv_rows.clear();
        }
        Some(review)
    } else {
        None
    };
    let contribution_row_key = if bundle_kind == CONTRIBUTION_BUNDLE_KIND {
        Some(contribution_key(
            bundle.profile_id.as_deref().unwrap_or("unknown-profile"),
            bundle.deterministic_bundle_sha256.as_deref(),
        ))
    } else {
        None
    };
    if let Some(key) = &contribution_row_key {
        let value = contribution_import_row(
            &bundle_path,
            &bundle,
            summaries.len() as u64,
            contribution_review.as_ref(),
        )?;
        let encoded = encode_json(&value).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("profile registry contribution row encode failed: {error}"),
            )
        })?;
        summaries.push(row_summary(cf::CF_PROFILES, key.as_bytes(), &encoded));
        profile_rows.push((key.clone().into_bytes(), encoded));
    }
    let runtime = lock_runtime(reflex_runtime, "importing profile registry")?;
    let duplicate_rows =
        filter_duplicate_or_conflicting_rows(&runtime, &mut profile_rows, &mut kv_rows)?;
    let profile_count = profile_rows.len() as u64;
    let kv_count = kv_rows.len() as u64;
    runtime
        .storage_put_profile_rows(profile_rows)
        .map_err(storage_error)?;
    runtime
        .storage_put_kv_rows(kv_rows)
        .map_err(storage_error)?;
    drop(runtime);
    Ok(ProfileRegistryImportResponse {
        bundle_path: bundle_path.display().to_string(),
        bundle_kind,
        rows_read: summaries.len() as u64,
        cf_profile_rows_written: profile_count,
        cf_kv_rows_written: kv_count,
        duplicate_rows,
        contribution_row_key,
        deterministic_bundle_sha256: bundle.deterministic_bundle_sha256.clone(),
        rows: summaries,
    })
}

fn registry_export_inputs(
    runtime: &MutexGuard<'_, ReflexRuntime>,
    params: &ProfileRegistryExportParams,
    bundle_kind: &str,
) -> Result<RegistryExportInputs, ErrorData> {
    let profile_rows = runtime
        .storage_cf_prefix_rows(cf::CF_PROFILES, REGISTRY_PREFIX.as_bytes(), usize::MAX)
        .map_err(storage_error)?;
    let kv_rows = runtime
        .storage_cf_prefix_rows(cf::CF_KV, REGISTRY_PREFIX.as_bytes(), usize::MAX)
        .map_err(storage_error)?;
    let audit_rows = if bundle_kind == CONTRIBUTION_BUNDLE_KIND && params.include_audit_evidence {
        runtime
            .storage_cf_tail_rows(cf::CF_ACTION_LOG, params.max_audit_rows as usize)
            .map_err(storage_error)?
    } else {
        Vec::new()
    };
    let quality_row = if bundle_kind == CONTRIBUTION_BUNDLE_KIND
        && params.include_quality_summary
        && let Some(profile_id) = &params.profile_id
    {
        runtime
            .storage_profile_row(quality_key(profile_id).as_bytes())
            .map_err(storage_error)?
            .map(|value| (quality_key(profile_id).into_bytes(), value))
    } else {
        None
    };
    Ok(RegistryExportInputs {
        profile_rows,
        kv_rows,
        audit_rows,
        quality_row,
    })
}

fn collect_export_bundle_rows(
    params: &ProfileRegistryExportParams,
    bundle_kind: &str,
    profile_rows: EncodedRows,
    kv_rows: EncodedRows,
) -> Result<
    (
        Vec<ProfileRegistryBundleRow>,
        Vec<ProfileRegistryRowSummary>,
    ),
    ErrorData,
> {
    let query = normalized_query(params.query.as_deref());
    let mut bundle_rows = Vec::new();
    let mut summaries = Vec::new();
    'rows: for (cf_name, rows) in [(cf::CF_PROFILES, profile_rows), (cf::CF_KV, kv_rows)] {
        for (key, value) in rows {
            let summary = row_summary(cf_name, &key, &value);
            if !row_filter_matches(
                &summary,
                &value,
                query.as_deref(),
                params.row_kind.as_deref(),
                params.include_disabled,
            ) {
                continue;
            }
            if bundle_kind == CONTRIBUTION_BUNDLE_KIND
                && let Some(profile_id) = &params.profile_id
                && !registry_row_contributes_to_profile(&summary, &value, profile_id)
            {
                continue;
            }
            if summaries.len() >= params.limit as usize {
                break 'rows;
            }
            let mut decoded = decode_json::<Value>(&value).map_err(decode_error)?;
            if bundle_kind == CONTRIBUTION_BUNDLE_KIND {
                redact_contribution_registry_value(&mut decoded);
            }
            bundle_rows.push(ProfileRegistryBundleRow {
                cf_name: cf_name.to_owned(),
                key: String::from_utf8_lossy(&key).into_owned(),
                value: decoded,
            });
            summaries.push(summary);
        }
    }
    Ok((bundle_rows, summaries))
}

fn collect_contribution_audit_evidence(
    params: &ProfileRegistryExportParams,
    bundle_kind: &str,
    audit_rows: EncodedRows,
) -> Result<Vec<ProfileRegistryAuditEvidenceRow>, ErrorData> {
    if bundle_kind != CONTRIBUTION_BUNDLE_KIND {
        return Ok(Vec::new());
    }
    let mut evidence = Vec::new();
    for (key, value) in audit_rows {
        if let Some(row) = contribution_audit_row(params.profile_id.as_deref(), &key, &value)? {
            evidence.push(row);
        }
    }
    Ok(evidence)
}

fn collect_contribution_quality_summaries(
    quality_row: Option<EncodedRow>,
) -> Result<Vec<ProfileRegistryQualitySummaryRow>, ErrorData> {
    quality_row
        .map(|(key, value)| contribution_quality_summary(&key, &value))
        .transpose()
        .map(|row| row.into_iter().collect())
}

fn write_registry_export_bundle(
    output_path: &Path,
    bundle: &ProfileRegistryExportBundle,
) -> Result<Vec<u8>, ErrorData> {
    let bytes = serde_json::to_vec_pretty(bundle).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("profile registry export encode failed: {error}"),
        )
    })?;
    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "profile registry export could not create {}: {error}",
                    parent.display()
                ),
            )
        })?;
    }
    fs::write(output_path, &bytes).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "profile registry export could not write {}: {error}",
                output_path.display()
            ),
        )
    })?;
    Ok(bytes)
}

#[expect(
    clippy::too_many_lines,
    reason = "rollback performs one atomic read/validate/write/readback operation"
)]
pub fn rollback_registry_profile(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &ProfileRegistryRollbackParams,
) -> Result<ProfileRegistryRollbackResponse, ErrorData> {
    if params.target_package_id.is_some() != params.target_package_version.is_some() {
        return Err(registry_error(
            "rollback_target_incomplete",
            "target_package_id and target_package_version must be provided together",
        ));
    }
    let installed_key = installed_key(&params.profile_id);
    let updated_at = Utc::now().to_rfc3339();
    let runtime = lock_runtime(reflex_runtime, "rolling back profile registry package")?;
    let installed_bytes = runtime
        .storage_profile_row(installed_key.as_bytes())
        .map_err(storage_error)?
        .ok_or_else(|| {
            rollback_unavailable_error(
                "installed_profile_missing",
                format!(
                    "installed profile row for {} was not found",
                    params.profile_id
                ),
            )
        })?;
    let mut installed = decode_json::<Value>(&installed_bytes).map_err(decode_error)?;
    let previous_package_id =
        required_string_from_value(&installed, "installed_package_id", "installed row")?;
    let previous_package_version =
        required_string_from_value(&installed, "installed_package_version", "installed row")?;
    let previous_installed_at = installed
        .get("installed_at")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let target_key = if let (Some(package_id), Some(package_version)) =
        (&params.target_package_id, &params.target_package_version)
    {
        package_key(package_id, package_version)
    } else {
        select_prior_package_key(
            &runtime,
            &params.profile_id,
            &previous_package_id,
            &previous_package_version,
        )?
    };
    let target_bytes = runtime
        .storage_profile_row(target_key.as_bytes())
        .map_err(storage_error)?
        .ok_or_else(|| {
            rollback_unavailable_error(
                "rollback_target_missing",
                format!("rollback target package row {target_key} was not found"),
            )
        })?;
    let target = decode_json::<Value>(&target_bytes).map_err(decode_error)?;
    validate_rollback_target(&target, &params.profile_id)?;
    let target_package_id = required_string_from_value(&target, "package_id", "target package")?;
    let target_package_version =
        required_string_from_value(&target, "package_version", "target package")?;
    let target_profile_version =
        required_string_from_value(&target, "profile_version", "target package")?;
    if target_package_id == previous_package_id
        && target_package_version == previous_package_version
    {
        return Err(rollback_unavailable_error(
            "rollback_target_is_current",
            "rollback target is already the installed package",
        ));
    }
    set_object_field(
        &mut installed,
        "previous_installed_package_id",
        json!(previous_package_id),
    );
    set_object_field(
        &mut installed,
        "previous_installed_package_version",
        json!(previous_package_version),
    );
    if let Some(previous_installed_at) = previous_installed_at {
        set_object_field(
            &mut installed,
            "previous_installed_at",
            json!(previous_installed_at),
        );
    }
    set_object_field(
        &mut installed,
        "installed_package_id",
        json!(target_package_id),
    );
    set_object_field(
        &mut installed,
        "installed_package_version",
        json!(target_package_version),
    );
    set_object_field(
        &mut installed,
        "active_profile_version",
        json!(target_profile_version),
    );
    set_object_field(&mut installed, "installed_at", json!(updated_at));
    set_object_field(&mut installed, "updated_at", json!(updated_at));
    set_object_field(&mut installed, "rollback_at", json!(updated_at));
    set_object_field(
        &mut installed,
        "rollback_reason",
        json!(params.reason.as_deref()),
    );
    set_object_field(&mut installed, "state", json!("active"));
    set_object_field(&mut installed, "activation_state", json!("installed"));
    set_object_field(
        &mut installed,
        "trust_status",
        target
            .get("trust_status")
            .cloned()
            .unwrap_or_else(|| json!("unknown")),
    );
    set_object_field(
        &mut installed,
        "signature_status",
        target
            .get("signature_status")
            .cloned()
            .unwrap_or_else(|| json!("unknown")),
    );
    set_object_field(
        &mut installed,
        "trust_root_key",
        target.get("trust_root_key").cloned().unwrap_or(Value::Null),
    );
    set_object_field(
        &mut installed,
        "signature_payload_digest",
        target
            .get("signature_payload_digest")
            .cloned()
            .unwrap_or(Value::Null),
    );
    set_object_field(
        &mut installed,
        "signer_id",
        target.get("signer_id").cloned().unwrap_or(Value::Null),
    );
    set_object_field(
        &mut installed,
        "key_id",
        target.get("key_id").cloned().unwrap_or(Value::Null),
    );
    set_object_field(
        &mut installed,
        "trust_policy_id",
        target
            .get("trust_policy_id")
            .cloned()
            .unwrap_or(Value::Null),
    );
    set_object_field(
        &mut installed,
        "trust_required",
        target.get("trust_required").cloned().unwrap_or(Value::Null),
    );
    let rollback_key = rollback_key(&params.profile_id, &updated_at);
    let rollback = json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "profile_registry_rollback",
        "row_id": format!("{}@{}", params.profile_id, updated_at),
        "created_at": updated_at,
        "updated_at": updated_at,
        "source_id": target.get("source_id").and_then(Value::as_str).unwrap_or(DEFAULT_SOURCE_ID),
        "state": "active",
        "profile_id": params.profile_id,
        "from_package_id": previous_package_id,
        "from_package_version": previous_package_version,
        "to_package_id": target_package_id,
        "to_package_version": target_package_version,
        "to_profile_version": target_profile_version,
        "target_package_key": target_key,
        "reason": params.reason.as_deref(),
        "trust_status": target.get("trust_status").cloned().unwrap_or_else(|| json!("unknown")),
        "signature_status": target.get("signature_status").cloned().unwrap_or_else(|| json!("unknown")),
        "trust_root_key": target.get("trust_root_key").cloned().unwrap_or(Value::Null),
        "signature_payload_digest": target.get("signature_payload_digest").cloned().unwrap_or(Value::Null),
        "signer_id": target.get("signer_id").cloned().unwrap_or(Value::Null),
        "key_id": target.get("key_id").cloned().unwrap_or(Value::Null),
        "trust_policy_id": target.get("trust_policy_id").cloned().unwrap_or(Value::Null),
        "trust_required": target.get("trust_required").cloned().unwrap_or(Value::Null),
    });
    let installed_encoded = encode_json(&installed).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("rollback installed row encode failed: {error}"),
        )
    })?;
    let rollback_encoded = encode_json(&rollback).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("rollback row encode failed: {error}"),
        )
    })?;
    runtime
        .storage_put_profile_rows(vec![
            (installed_key.clone().into_bytes(), installed_encoded),
            (rollback_key.clone().into_bytes(), rollback_encoded),
        ])
        .map_err(storage_error)?;
    let installed_readback = runtime
        .storage_profile_row(installed_key.as_bytes())
        .map_err(storage_error)?
        .ok_or_else(|| {
            registry_error(
                "rollback_installed_write_missing",
                "installed profile row did not persist after rollback",
            )
        })?;
    let rollback_readback = runtime
        .storage_profile_row(rollback_key.as_bytes())
        .map_err(storage_error)?
        .ok_or_else(|| {
            registry_error("rollback_row_write_missing", "rollback row did not persist")
        })?;
    drop(runtime);
    Ok(ProfileRegistryRollbackResponse {
        profile_id: params.profile_id.clone(),
        previous_package_id,
        previous_package_version,
        rolled_back_package_id: target_package_id,
        rolled_back_package_version: target_package_version,
        row_key: installed_key.clone(),
        rollback_row_key: rollback_key.clone(),
        wrote_row: true,
        installed_row: stored_row(
            cf::CF_PROFILES,
            installed_key.as_bytes(),
            &installed_readback,
        )?,
        rollback_row: stored_row(cf::CF_PROFILES, rollback_key.as_bytes(), &rollback_readback)?,
    })
}

pub fn query_audit_intelligence(
    reflex_runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &AuditIntelligenceQueryParams,
) -> Result<AuditIntelligenceQueryResponse, ErrorData> {
    validate_limit(params.max_rows)?;
    let runtime = lock_runtime(reflex_runtime, "querying audit intelligence")?;
    let action_rows = runtime
        .storage_cf_tail_rows(cf::CF_ACTION_LOG, params.max_rows as usize)
        .map_err(storage_error)?;
    let event_rows = runtime
        .storage_cf_tail_rows(cf::CF_EVENTS, params.max_rows as usize)
        .map_err(storage_error)?;
    let reflex_rows = runtime
        .storage_cf_tail_rows(cf::CF_REFLEX_AUDIT, params.max_rows as usize)
        .map_err(storage_error)?;
    let session_rows = runtime
        .storage_cf_tail_rows(cf::CF_SESSIONS, params.max_rows as usize)
        .map_err(storage_error)?;
    let quality_key = quality_key(&params.profile_id);
    let quality_snapshot = runtime
        .storage_profile_row(quality_key.as_bytes())
        .map_err(storage_error)?
        .map(|value| decode_json::<Value>(&value).map_err(decode_error))
        .transpose()?;
    drop(runtime);
    let action = summarize_bucket(cf::CF_ACTION_LOG, &params.profile_id, action_rows, "tool")?;
    let events = summarize_bucket(cf::CF_EVENTS, &params.profile_id, event_rows, "kind")?;
    let reflexes = summarize_bucket(
        cf::CF_REFLEX_AUDIT,
        &params.profile_id,
        reflex_rows,
        "status",
    )?;
    let sessions = summarize_sessions(&params.profile_id, session_rows)?;
    let learning_candidates =
        learning_candidates(&action, &events, &reflexes, quality_snapshot.is_some());
    Ok(AuditIntelligenceQueryResponse {
        profile_id: params.profile_id.clone(),
        max_rows: params.max_rows,
        action,
        events,
        reflexes,
        sessions,
        quality_snapshot_key: quality_key,
        quality_snapshot,
        learning_candidates,
    })
}

fn parse_manifest(
    path: &Path,
    bytes: &[u8],
    params: &ProfileRegistryInstallParams,
) -> Result<ProfilePackageManifest, ErrorData> {
    params.expected_manifest_digest.as_ref().map_or_else(
        || parse_package_manifest_bytes(path, bytes).map_err(profile_error),
        |expected| {
            parse_package_manifest_bytes_with_digest(path, bytes, expected).map_err(profile_error)
        },
    )
}

#[expect(
    clippy::result_large_err,
    reason = "trust failure is returned once to write an explicit quarantine row"
)]
#[expect(
    clippy::too_many_lines,
    reason = "trust verification keeps fail-closed decision order visible"
)]
fn verify_manifest_trust(
    manifest: &ProfilePackageManifest,
    params: &ProfileRegistryInstallParams,
) -> Result<TrustVerification, TrustFailure> {
    let required =
        params.trust_policy == "signed_required" || manifest.trust.policy == "signed_required";
    let trust_policy_id = if required {
        "signed-required".to_owned()
    } else {
        "local-first".to_owned()
    };
    let payload_digest = package_signature_payload_digest(manifest);
    if manifest.signatures.is_empty() {
        return if required {
            Err(TrustFailure {
                reason: "signature_required_missing",
                message: "profile package policy requires a trusted Ed25519 signature".to_owned(),
                trust_policy_id,
                signature_status: "missing".to_owned(),
                signer_id: None,
                key_id: None,
                signature_payload_digest: Some(payload_digest),
            })
        } else {
            Ok(TrustVerification {
                trust_policy_id,
                trust_status: "local_validated".to_owned(),
                signature_status: "unsigned_allowed".to_owned(),
                signer_id: None,
                key_id: None,
                trust_root_key: None,
                signature_payload_digest: Some(payload_digest),
                required,
            })
        };
    }
    let required_signers = manifest
        .trust
        .required_signers
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut unknown = None;
    let mut invalid = None;
    for signature in &manifest.signatures {
        if !required_signers.is_empty() && !required_signers.contains(&signature.signer_id.as_str())
        {
            unknown = Some(signature);
            continue;
        }
        let Some(root) = find_trust_root(&signature.signer_id, &signature.key_id) else {
            unknown = Some(signature);
            continue;
        };
        match verify_ed25519_signature(manifest, root, signature) {
            Ok(()) => {
                let trust_root_key = trust_root_key(root);
                return Ok(TrustVerification {
                    trust_policy_id,
                    trust_status: "trusted".to_owned(),
                    signature_status: "verified".to_owned(),
                    signer_id: Some(signature.signer_id.clone()),
                    key_id: Some(signature.key_id.clone()),
                    trust_root_key: Some(trust_root_key),
                    signature_payload_digest: Some(payload_digest),
                    required,
                });
            }
            Err(message) => {
                invalid = Some((signature, message));
            }
        }
    }
    if let Some((signature, message)) = invalid {
        return Err(TrustFailure {
            reason: "signature_invalid",
            message,
            trust_policy_id,
            signature_status: "invalid".to_owned(),
            signer_id: Some(signature.signer_id.clone()),
            key_id: Some(signature.key_id.clone()),
            signature_payload_digest: Some(payload_digest),
        });
    }
    let Some(signature) = unknown.or_else(|| manifest.signatures.first()) else {
        return Err(TrustFailure {
            reason: "signature_required_missing",
            message: "profile package policy requires a trusted Ed25519 signature".to_owned(),
            trust_policy_id,
            signature_status: "missing".to_owned(),
            signer_id: None,
            key_id: None,
            signature_payload_digest: Some(payload_digest),
        });
    };
    Err(TrustFailure {
        reason: "signer_unknown",
        message: format!(
            "profile package signer {} with key {} is not trusted by the local registry",
            signature.signer_id, signature.key_id
        ),
        trust_policy_id,
        signature_status: "unknown_signer".to_owned(),
        signer_id: Some(signature.signer_id.clone()),
        key_id: Some(signature.key_id.clone()),
        signature_payload_digest: Some(payload_digest),
    })
}

fn verify_ed25519_signature(
    manifest: &ProfilePackageManifest,
    root: &TrustRoot,
    signature: &PackageSignature,
) -> Result<(), String> {
    let public_key_bytes = decode_hex_array::<32>("trust root public key", root.public_key_hex)?;
    let signature_bytes =
        decode_prefixed_hex_array::<64>("package signature", &signature.signature, "ed25519:")?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|error| format!("trust root public key is invalid: {error}"))?;
    let signature = Signature::from_bytes(&signature_bytes);
    verifying_key
        .verify(&package_signature_payload(manifest), &signature)
        .map_err(|error| format!("profile package signature did not verify: {error}"))
}

fn find_trust_root(signer_id: &str, key_id: &str) -> Option<&'static TrustRoot> {
    BUILTIN_TRUST_ROOTS
        .iter()
        .find(|root| root.signer_id == signer_id && root.key_id == key_id)
}

fn find_trust_root_by_signer(signer_id: &str) -> Option<&'static TrustRoot> {
    BUILTIN_TRUST_ROOTS
        .iter()
        .find(|root| root.signer_id == signer_id)
}

fn trust_root_row(root: &TrustRoot, source_id: &str, updated_at: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "registry_trust_root",
        "row_id": format!("{}:{}", root.signer_id, root.key_id),
        "created_at": updated_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "signer_id": root.signer_id,
        "key_id": root.key_id,
        "algorithm": "ed25519",
        "public_key_sha256": root.key_id,
        "public_key_hex": root.public_key_hex,
        "trust_domain": root.trust_domain,
        "origin": "bundled_fixture_root",
        "operator_owned": true,
    })
}

fn quarantine_row(
    manifest: &ProfilePackageManifest,
    manifest_path: &Path,
    manifest_digest: &str,
    source_id: &str,
    updated_at: &str,
    failure: &TrustFailure,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "profile_package_quarantine",
        "row_id": format!("{}@{}", manifest.package_id, manifest.package_version),
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "quarantined",
        "activation_state": "quarantined",
        "package_id": manifest.package_id,
        "package_version": manifest.package_version,
        "profile_id": manifest.profile_id,
        "profile_version": manifest.profile_version,
        "manifest_path": manifest_path.display().to_string(),
        "manifest_digest": manifest_digest,
        "trust_policy_id": failure.trust_policy_id,
        "signature_status": failure.signature_status,
        "signer_id": failure.signer_id.as_deref(),
        "key_id": failure.key_id.as_deref(),
        "signature_payload_digest": failure.signature_payload_digest.as_deref(),
        "quarantine_reason": failure.reason,
        "error_code": error_codes::PROFILE_TRUST_VERIFICATION_FAILED,
        "error_message": failure.message,
    })
}

fn trust_error(
    failure: &TrustFailure,
    quarantine_row_key: &str,
    quarantine_readback: Option<&ProfileRegistryRowSummary>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "profile package trust verification failed and package was quarantined: {}",
            failure.message
        ),
        Some(json!({
            "code": error_codes::PROFILE_TRUST_VERIFICATION_FAILED,
            "reason": failure.reason,
            "quarantine_row_key": quarantine_row_key,
            "signature_status": failure.signature_status.as_str(),
            "signer_id": failure.signer_id.as_deref(),
            "key_id": failure.key_id.as_deref(),
            "signature_payload_digest": failure.signature_payload_digest.as_deref(),
            "quarantine_readback": quarantine_readback,
        })),
    )
}

fn decode_prefixed_hex_array<const N: usize>(
    field: &str,
    value: &str,
    prefix: &str,
) -> Result<[u8; N], String> {
    let hex = value
        .strip_prefix(prefix)
        .ok_or_else(|| format!("{field} must start with {prefix}"))?;
    decode_hex_array(field, hex)
}

fn decode_hex_array<const N: usize>(field: &str, hex: &str) -> Result<[u8; N], String> {
    if hex.len() != N * 2 {
        return Err(format!("{field} must contain {} hex characters", N * 2));
    }
    let mut output = [0_u8; N];
    let bytes = hex.as_bytes();
    for index in 0..N {
        let high =
            hex_nibble(bytes[index * 2]).ok_or_else(|| format!("{field} contains non-hex data"))?;
        let low = hex_nibble(bytes[index * 2 + 1])
            .ok_or_else(|| format!("{field} contains non-hex data"))?;
        output[index] = (high << 4) | low;
    }
    Ok(output)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "row builder takes explicit SoT ingredients from install readback"
)]
fn registry_rows(
    manifest: &ProfilePackageManifest,
    manifest_path: &Path,
    manifest_digest: &str,
    profile_toml_path: &Path,
    source_id: &str,
    updated_at: &str,
    trust: &TrustVerification,
    quality_row: Option<&[u8]>,
) -> Result<EncodedRows, ErrorData> {
    let mut rows = vec![
        encoded_row(
            source_key(source_id),
            &source_row(manifest, source_id, updated_at, trust),
        )?,
        encoded_row(
            package_key(&manifest.package_id, &manifest.package_version),
            &package_row(
                manifest,
                manifest_path,
                manifest_digest,
                source_id,
                updated_at,
                trust,
            ),
        )?,
        encoded_row(
            profile_key(&manifest.profile_id, &manifest.profile_version),
            &profile_row(manifest, profile_toml_path, source_id, updated_at, trust),
        )?,
        encoded_row(
            installed_key(&manifest.profile_id),
            &installed_row(manifest, source_id, updated_at, trust),
        )?,
    ];
    if let Some(root_key) = &trust.trust_root_key
        && let Some(root) = trust
            .signer_id
            .as_deref()
            .and_then(find_trust_root_by_signer)
    {
        rows.push(encoded_row(
            root_key.clone(),
            &trust_root_row(root, source_id, updated_at),
        )?);
    }
    for target in &manifest.targets {
        rows.push(encoded_row(
            compat_key(
                &target.target_id,
                &manifest.profile_id,
                &manifest.profile_version,
            ),
            &compat_row(
                manifest,
                &target.target_id,
                &target.target_kind,
                source_id,
                updated_at,
                trust,
            ),
        )?);
    }
    if let Some((key, value)) = curated_target_row(manifest, source_id, updated_at)? {
        rows.push(encoded_row(key, &value)?);
    }
    if let Some(quality_row) = quality_row {
        let quality = decode_json::<Value>(quality_row).map_err(decode_error)?;
        rows.push(encoded_row(
            quality_link_key(&manifest.profile_id, &manifest.profile_version),
            &quality_link_row(manifest, &quality, source_id, updated_at),
        )?);
    }
    Ok(rows)
}

fn encoded_row(key: String, value: &Value) -> Result<EncodedRow, ErrorData> {
    let encoded = encode_json(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("profile registry row encode failed: {error}"),
        )
    })?;
    Ok((key.into_bytes(), encoded))
}

fn source_row(
    manifest: &ProfilePackageManifest,
    source_id: &str,
    updated_at: &str,
    trust: &TrustVerification,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "registry_source",
        "row_id": source_id,
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "source_kind": manifest.source.kind,
        "base_url": manifest.source.uri,
        "root_path": null,
        "auth_mode": "none",
        "trust_policy_id": trust.trust_policy_id.as_str(),
        "trust_required": trust.required,
        "offline_usable": true,
        "last_health_status": "ok",
    })
}

fn package_row(
    manifest: &ProfilePackageManifest,
    manifest_path: &Path,
    manifest_digest: &str,
    source_id: &str,
    updated_at: &str,
    trust: &TrustVerification,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "profile_package",
        "row_id": format!("{}@{}", manifest.package_id, manifest.package_version),
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "package_id": manifest.package_id,
        "package_version": manifest.package_version,
        "manifest_path": manifest_path.display().to_string(),
        "manifest_digest": manifest_digest,
        "package_digest": manifest.hashes.package_sha256,
        "profile_id": manifest.profile_id,
        "profile_version": manifest.profile_version,
        "target_ids": manifest.targets.iter().map(|target| target.target_id.clone()).collect::<Vec<_>>(),
        "license_spdx": manifest.permissions.license_spdx,
        "governance_manifest_key": format!("profile_registry/v1/package/{}/{}", manifest.package_id, manifest.package_version),
        "trust_status": trust.trust_status.as_str(),
        "signature_status": trust.signature_status.as_str(),
        "signer_id": trust.signer_id.as_deref(),
        "key_id": trust.key_id.as_deref(),
        "trust_root_key": trust.trust_root_key.as_deref(),
        "signature_payload_digest": trust.signature_payload_digest.as_deref(),
        "trust_policy_id": trust.trust_policy_id.as_str(),
        "trust_required": trust.required,
        "moderation_status": "local_only",
        "revoked": false,
        "profile_versions": [manifest.profile_version.clone()],
        "provenance": {
            "source_kind": manifest.source.kind,
            "source_uri": manifest.source.uri,
            "source_revision": manifest.source.revision,
            "built_by": manifest.source.built_by,
            "generated_by": manifest.source.generated_by,
        },
    })
}

fn profile_row(
    manifest: &ProfilePackageManifest,
    profile_toml_path: &Path,
    source_id: &str,
    updated_at: &str,
    trust: &TrustVerification,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "profile_version",
        "row_id": format!("{}@{}", manifest.profile_id, manifest.profile_version),
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "profile_id": manifest.profile_id,
        "profile_version": manifest.profile_version,
        "package_id": manifest.package_id,
        "package_version": manifest.package_version,
        "profile_toml_path": profile_toml_path.display().to_string(),
        "profile_toml_digest": manifest.hashes.profile_toml_sha256,
        "use_scope": manifest.permissions.use_scope,
        "schema_version_supported": true,
        "trust_status": trust.trust_status.as_str(),
        "signature_status": trust.signature_status.as_str(),
        "trust_root_key": trust.trust_root_key.as_deref(),
    })
}

fn installed_row(
    manifest: &ProfilePackageManifest,
    source_id: &str,
    updated_at: &str,
    trust: &TrustVerification,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "installed_profile",
        "row_id": manifest.profile_id,
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "profile_id": manifest.profile_id,
        "active_profile_version": manifest.profile_version,
        "installed_package_id": manifest.package_id,
        "installed_package_version": manifest.package_version,
        "installed_at": updated_at,
        "activation_state": "installed",
        "trust_status": trust.trust_status.as_str(),
        "signature_status": trust.signature_status.as_str(),
        "signer_id": trust.signer_id.as_deref(),
        "key_id": trust.key_id.as_deref(),
        "trust_root_key": trust.trust_root_key.as_deref(),
        "signature_payload_digest": trust.signature_payload_digest.as_deref(),
        "trust_policy_id": trust.trust_policy_id.as_str(),
        "trust_required": trust.required,
        "operator_overrides_path": null,
    })
}

fn compat_row(
    manifest: &ProfilePackageManifest,
    target_id: &str,
    target_kind: &str,
    source_id: &str,
    updated_at: &str,
    trust: &TrustVerification,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "compatibility_target",
        "row_id": format!("{target_id}:{}@{}", manifest.profile_id, manifest.profile_version),
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "target_id": target_id,
        "target_kind": target_kind,
        "profile_id": manifest.profile_id,
        "profile_version": manifest.profile_version,
        "compatibility_status": "declared",
        "trust_status": trust.trust_status.as_str(),
        "source_quality_snapshot_key": quality_key(&manifest.profile_id),
        "evidence_hash": manifest.hashes.package_sha256,
    })
}

fn quality_link_row(
    manifest: &ProfilePackageManifest,
    quality: &Value,
    source_id: &str,
    updated_at: &str,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "quality_link",
        "row_id": format!("{}@{}", manifest.profile_id, manifest.profile_version),
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "profile_id": manifest.profile_id,
        "profile_version": manifest.profile_version,
        "profile_quality_key": quality_key(&manifest.profile_id),
        "source_cf_ranges": {
            "audit_cf_name": quality.pointer("/source/audit_cf_name").cloned().unwrap_or_else(|| json!(cf::CF_ACTION_LOG)),
            "audit_rows_scanned": quality.pointer("/source/audit_rows_scanned").cloned().unwrap_or_else(|| json!(0)),
            "observation_cf_name": quality.pointer("/runtime_evidence/observation_cf_name").cloned().unwrap_or_else(|| json!(cf::CF_OBSERVATIONS)),
            "observation_rows_scanned": quality.pointer("/runtime_evidence/observation_rows_scanned").cloned().unwrap_or_else(|| json!(0)),
            "event_cf_name": quality.pointer("/runtime_evidence/event_cf_name").cloned().unwrap_or_else(|| json!(cf::CF_EVENTS)),
            "event_rows_scanned": quality.pointer("/runtime_evidence/event_rows_scanned").cloned().unwrap_or_else(|| json!(0)),
        },
        "manual_fsv_evidence_ref": quality.get("manual_fsv_evidence_ref").cloned().unwrap_or(Value::Null),
        "quality_score": quality.pointer("/score/score_0_100").cloned().unwrap_or_else(|| json!(0)),
        "sample_count": quality.pointer("/score/sample_size").cloned().unwrap_or_else(|| json!(0)),
        "evidence_hash": quality.get("evidence_hash").cloned().unwrap_or_else(|| json!("")),
    })
}

fn curated_target_row(
    manifest: &ProfilePackageManifest,
    source_id: &str,
    updated_at: &str,
) -> Result<Option<(String, Value)>, ErrorData> {
    if !manifest
        .metadata
        .keys()
        .any(|key| key.starts_with("curated."))
    {
        return Ok(None);
    }

    let seed_set_id = required_curated_metadata(manifest, "curated.seed_set_id")?;
    let target_id = required_curated_metadata(manifest, "curated.target_id")?;
    let tier = required_curated_metadata(manifest, "curated.tier")?;
    let priority = required_curated_metadata(manifest, "curated.priority")?;
    let status = required_curated_metadata(manifest, "curated.status")?;
    let backlog_issue = required_curated_metadata(manifest, "curated.backlog_issue")?;
    let minimum_manual_fsv = required_curated_metadata(manifest, "curated.minimum_manual_fsv")?;
    validate_registry_id("metadata.curated.seed_set_id", seed_set_id)?;
    validate_registry_id("metadata.curated.target_id", target_id)?;
    validate_registry_id("metadata.curated.tier", tier)?;
    validate_registry_id("metadata.curated.priority", priority)?;
    validate_registry_id("metadata.curated.status", status)?;
    validate_non_empty("metadata.curated.backlog_issue", backlog_issue)?;
    validate_non_empty("metadata.curated.minimum_manual_fsv", minimum_manual_fsv)?;
    let Some(target) = manifest
        .targets
        .iter()
        .find(|target| target.target_id == target_id)
    else {
        return Err(registry_error(
            "curated_target_missing_compatibility",
            format!(
                "metadata.curated.target_id {target_id:?} must match a manifest compatibility target"
            ),
        ));
    };
    let fsv_steps = split_metadata_list(minimum_manual_fsv);
    if fsv_steps.is_empty() {
        return Err(registry_error(
            "curated_target_fsv_missing",
            "metadata.curated.minimum_manual_fsv must name at least one manual FSV surface",
        ));
    }
    let key = curated_target_key(seed_set_id, target_id);
    let value = json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "curated_profile_target",
        "row_id": format!("{seed_set_id}:{target_id}"),
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "seed_set_id": seed_set_id,
        "target_id": target_id,
        "tier": tier,
        "priority": priority,
        "status": status,
        "backlog_issue": backlog_issue,
        "profile_id": manifest.profile_id,
        "profile_version": manifest.profile_version,
        "package_id": manifest.package_id,
        "package_version": manifest.package_version,
        "target_kind": target.target_kind,
        "app_id": target.app_id,
        "process_name": target.process_name,
        "title_regex": target.title_regex,
        "steam_appid": target.steam_appid,
        "app_version": target.app_version,
        "use_scope": manifest.permissions.use_scope,
        "compatibility_status": "declared",
        "safe_default_backend_policy": manifest.metadata.get("curated.safe_default_backend_policy"),
        "input_backends": manifest.input.backends,
        "minimum_manual_fsv": fsv_steps,
        "quality_signal": manifest.metadata.get("quality_signal")
            .or_else(|| manifest.metadata.get("curated.quality_signal")),
        "profile_quality_key": quality_key(&manifest.profile_id),
        "package_key": package_key(&manifest.package_id, &manifest.package_version),
        "installed_key": installed_key(&manifest.profile_id),
        "compatibility_key": compat_key(target_id, &manifest.profile_id, &manifest.profile_version),
        "provenance": {
            "source_kind": manifest.source.kind,
            "source_uri": manifest.source.uri,
            "source_revision": manifest.source.revision,
            "built_by": manifest.source.built_by,
            "generated_by": manifest.source.generated_by,
        },
    });
    Ok(Some((key, value)))
}

fn required_curated_metadata<'a>(
    manifest: &'a ProfilePackageManifest,
    key: &'static str,
) -> Result<&'a str, ErrorData> {
    manifest
        .metadata
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            registry_error(
                "curated_target_metadata_missing",
                format!("{key} is required when any curated.* metadata is present"),
            )
        })
}

fn split_metadata_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

fn head_row(
    manifest: &ProfilePackageManifest,
    manifest_digest: &str,
    source_id: &str,
    updated_at: &str,
    trust: &TrustVerification,
) -> Result<EncodedRow, ErrorData> {
    let value = json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "registry_head",
        "row_id": source_id,
        "created_at": manifest.created_at,
        "updated_at": updated_at,
        "source_id": source_id,
        "state": "active",
        "package_id": manifest.package_id,
        "package_version": manifest.package_version,
        "package_key": package_key(&manifest.package_id, &manifest.package_version),
        "manifest_digest": manifest_digest,
        "trust_status": trust.trust_status.as_str(),
        "signature_status": trust.signature_status.as_str(),
        "trust_root_key": trust.trust_root_key.as_deref(),
    });
    encoded_row(head_key(source_id), &value)
}

fn stored_row(
    cf_name: &str,
    key: &[u8],
    value: &[u8],
) -> Result<ProfileRegistryStoredRow, ErrorData> {
    Ok(ProfileRegistryStoredRow {
        summary: row_summary(cf_name, key, value),
        value: decode_json::<Value>(value).map_err(decode_error)?,
    })
}

fn row_summary(cf_name: &str, key: &[u8], value: &[u8]) -> ProfileRegistryRowSummary {
    let decoded = decode_json::<Value>(value).ok();
    ProfileRegistryRowSummary {
        cf_name: cf_name.to_owned(),
        key: String::from_utf8_lossy(key).into_owned(),
        key_hex: hex_encode(key),
        row_kind: decoded
            .as_ref()
            .and_then(|value| string_field(value, "row_kind")),
        row_id: decoded
            .as_ref()
            .and_then(|value| string_field(value, "row_id")),
        source_id: decoded
            .as_ref()
            .and_then(|value| string_field(value, "source_id")),
        state: decoded
            .as_ref()
            .and_then(|value| string_field(value, "state")),
        profile_id: decoded
            .as_ref()
            .and_then(|value| string_field(value, "profile_id")),
        profile_version: decoded.as_ref().and_then(|value| {
            string_field(value, "profile_version")
                .or_else(|| string_field(value, "active_profile_version"))
        }),
        package_id: decoded.as_ref().and_then(|value| {
            string_field(value, "package_id")
                .or_else(|| string_field(value, "installed_package_id"))
        }),
        package_version: decoded.as_ref().and_then(|value| {
            string_field(value, "package_version")
                .or_else(|| string_field(value, "installed_package_version"))
        }),
        updated_at: decoded
            .as_ref()
            .and_then(|value| string_field(value, "updated_at")),
        value_len_bytes: value.len() as u64,
        value_utf8_prefix: utf8_prefix(value, VALUE_PREFIX_CHARS),
    }
}

fn summarize_bucket(
    cf_name: &str,
    profile_id: &str,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    kind_field: &str,
) -> Result<AuditBucketSummary, ErrorData> {
    let rows_scanned = rows.len() as u64;
    let mut summary = AuditBucketSummary {
        cf_name: cf_name.to_owned(),
        rows_scanned,
        matching_rows: 0,
        by_status: BTreeMap::new(),
        by_kind_or_tool: BTreeMap::new(),
        by_error_code: BTreeMap::new(),
    };
    for (_key, value) in rows {
        let row = decode_json::<Value>(&value).map_err(decode_error)?;
        if !value_mentions_profile(&row, profile_id) {
            continue;
        }
        summary.matching_rows += 1;
        if let Some(status) = string_field(&row, "status") {
            increment(&mut summary.by_status, status);
        }
        if let Some(kind) = string_field(&row, kind_field)
            .or_else(|| string_field(&row, "kind"))
            .or_else(|| string_field(&row, "tool"))
        {
            increment(&mut summary.by_kind_or_tool, kind);
        }
        if let Some(error_code) = string_field(&row, "error_code").or_else(|| {
            row.pointer("/data/error_code")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }) {
            increment(&mut summary.by_error_code, error_code);
        }
    }
    Ok(summary)
}

fn summarize_sessions(
    profile_id: &str,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<AuditSessionSummary, ErrorData> {
    let rows_scanned = rows.len() as u64;
    let mut session_ids = Vec::new();
    let mut matching_rows = 0;
    for (_key, value) in rows {
        let row = decode_json::<Value>(&value).map_err(decode_error)?;
        if !value_mentions_profile(&row, profile_id) {
            continue;
        }
        matching_rows += 1;
        if let Some(session_id) = string_field(&row, "session_id") {
            session_ids.push(session_id);
        }
    }
    session_ids.sort();
    session_ids.dedup();
    Ok(AuditSessionSummary {
        cf_name: cf::CF_SESSIONS.to_owned(),
        rows_scanned,
        matching_rows,
        session_ids,
    })
}

fn learning_candidates(
    action: &AuditBucketSummary,
    events: &AuditBucketSummary,
    reflexes: &AuditBucketSummary,
    has_quality_snapshot: bool,
) -> Vec<AuditLearningCandidate> {
    let mut candidates = Vec::new();
    let action_errors = action.by_error_code.values().sum();
    if action_errors > 0 {
        candidates.push(AuditLearningCandidate {
            kind: "action_error_cluster".to_owned(),
            evidence_count: action_errors,
            rationale:
                "Profile has action error rows; inspect keymap/backend policy for repeat failures."
                    .to_owned(),
        });
    }
    let activation_denied = events
        .by_kind_or_tool
        .get("profile.activation_denied")
        .copied()
        .unwrap_or_default();
    if activation_denied > 0 {
        candidates.push(AuditLearningCandidate {
            kind: "activation_denied".to_owned(),
            evidence_count: activation_denied,
            rationale: "Activation denial rows exist; profile registry should surface missing or disabled package state.".to_owned(),
        });
    }
    let reflex_errors = reflexes.by_error_code.values().sum();
    if reflex_errors > 0 {
        candidates.push(AuditLearningCandidate {
            kind: "reflex_error_cluster".to_owned(),
            evidence_count: reflex_errors,
            rationale: "Reflex audit error rows exist; inspect trigger lifetime and action sequence stability.".to_owned(),
        });
    }
    if !has_quality_snapshot {
        candidates.push(AuditLearningCandidate {
            kind: "quality_snapshot_missing".to_owned(),
            evidence_count: 1,
            rationale: "No profile quality snapshot row exists yet; run profile_quality_refresh after collecting action evidence.".to_owned(),
        });
    }
    candidates
}

fn value_mentions_profile(value: &Value, profile_id: &str) -> bool {
    string_field(value, "profile_id").as_deref() == Some(profile_id)
        || string_field(value, "active_profile_id").as_deref() == Some(profile_id)
        || string_field(value, "active_profile").as_deref() == Some(profile_id)
        || value
            .pointer("/audit_context/profile_id")
            .and_then(Value::as_str)
            == Some(profile_id)
        || value
            .pointer("/foreground/profile_id")
            .and_then(Value::as_str)
            == Some(profile_id)
        || value.pointer("/data/profile_id").and_then(Value::as_str) == Some(profile_id)
        || value
            .get("profile_history")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("profile_id").and_then(Value::as_str) == Some(profile_id))
            })
}

fn validate_report_params(params: &ProfileRegistryQueryParams) -> Result<(), ErrorData> {
    validate_limit(params.limit)?;
    if params.max_audit_rows == 0 || params.max_audit_rows > MAX_REPORT_AUDIT_ROWS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "profile_registry_query max_audit_rows must be 1..={MAX_REPORT_AUDIT_ROWS}; got {}",
                params.max_audit_rows
            ),
        ));
    }
    if let Some(profile_id) = &params.profile_id {
        validate_non_empty("profile_id", profile_id)?;
    }
    Ok(())
}

fn report_base_pointers(storage_path: &str) -> Vec<ProfileRegistryReportPointer> {
    [
        (
            "storage_path",
            None,
            None,
            "RocksDB root path backing this report",
        ),
        (
            "registry_rows_prefix",
            Some(cf::CF_PROFILES),
            Some(REGISTRY_PREFIX),
            "registry package, installed, compatibility, quarantine, rollback, and curated rows",
        ),
        (
            "registry_head_prefix",
            Some(cf::CF_KV),
            Some(HEAD_PREFIX),
            "registry source head pointers",
        ),
        (
            "quality_snapshot_prefix",
            Some(cf::CF_PROFILES),
            Some(QUALITY_PREFIX),
            "profile quality snapshot rows",
        ),
        (
            "audit_export_consent_prefix",
            Some(cf::CF_KV),
            Some(AUDIT_EXPORT_CONSENT_PREFIX),
            "audit export consent rows",
        ),
        (
            "recent_audit_tail",
            Some(cf::CF_ACTION_LOG),
            None,
            "tail rows scanned for recent profile-linked audit evidence",
        ),
        (
            "recent_observation_tail",
            Some(cf::CF_OBSERVATIONS),
            None,
            "tail rows scanned for profile-linked observation evidence in quality snapshots",
        ),
        (
            "recent_event_tail",
            Some(cf::CF_EVENTS),
            None,
            "tail rows scanned for profile-linked event and log evidence in quality snapshots",
        ),
    ]
    .into_iter()
    .map(
        |(surface, cf_name, key, description)| ProfileRegistryReportPointer {
            surface: surface.to_owned(),
            cf_name: cf_name.map(str::to_owned),
            key: key.map(str::to_owned),
            key_hex: key.map(|value| hex_encode(value.as_bytes())),
            path: Some(storage_path.to_owned()),
            description: description.to_owned(),
        },
    )
    .collect()
}

fn push_sot_row(
    pointers: &mut Vec<ProfileRegistryReportPointer>,
    surface: &str,
    cf_name: &str,
    key: &[u8],
    storage_path: &str,
    description: &str,
) {
    pointers.push(ProfileRegistryReportPointer {
        surface: surface.to_owned(),
        cf_name: Some(cf_name.to_owned()),
        key: Some(String::from_utf8_lossy(key).into_owned()),
        key_hex: Some(hex_encode(key)),
        path: Some(storage_path.to_owned()),
        description: description.to_owned(),
    });
}

fn report_row_matches_profile(
    row: &ProfileRegistryStoredRow,
    profile_filter: Option<&str>,
) -> bool {
    profile_filter.is_none_or(|profile_id| {
        row.summary.profile_id.as_deref() == Some(profile_id)
            || value_mentions_profile(&row.value, profile_id)
    })
}

fn report_installed_profile(
    row: &ProfileRegistryStoredRow,
) -> Option<ProfileRegistryReportInstalledProfile> {
    Some(ProfileRegistryReportInstalledProfile {
        row_key: row.summary.key.clone(),
        profile_id: string_field(&row.value, "profile_id")?,
        active_profile_version: string_field(&row.value, "active_profile_version"),
        installed_package_id: string_field(&row.value, "installed_package_id"),
        installed_package_version: string_field(&row.value, "installed_package_version"),
        activation_state: string_field(&row.value, "activation_state"),
        state: string_field(&row.value, "state"),
        trust_status: string_field(&row.value, "trust_status"),
        signature_status: string_field(&row.value, "signature_status"),
        signer_id: string_field(&row.value, "signer_id"),
        trust_root_key: string_field(&row.value, "trust_root_key"),
        installed_at: string_field(&row.value, "installed_at"),
        updated_at: string_field(&row.value, "updated_at"),
    })
}

fn report_package(row: &ProfileRegistryStoredRow) -> ProfileRegistryReportPackage {
    ProfileRegistryReportPackage {
        row_key: row.summary.key.clone(),
        package_id: string_field(&row.value, "package_id"),
        package_version: string_field(&row.value, "package_version"),
        profile_id: string_field(&row.value, "profile_id"),
        profile_version: string_field(&row.value, "profile_version"),
        state: string_field(&row.value, "state"),
        trust_status: string_field(&row.value, "trust_status"),
        signature_status: string_field(&row.value, "signature_status"),
        moderation_status: string_field(&row.value, "moderation_status"),
        signer_id: string_field(&row.value, "signer_id"),
        trust_root_key: string_field(&row.value, "trust_root_key"),
        manifest_digest: string_field(&row.value, "manifest_digest"),
        target_ids: string_array_field(&row.value, "target_ids"),
        updated_at: string_field(&row.value, "updated_at"),
    }
}

fn report_curated_target(row: &ProfileRegistryStoredRow) -> ProfileRegistryReportCuratedTarget {
    ProfileRegistryReportCuratedTarget {
        row_key: row.summary.key.clone(),
        seed_set_id: string_field(&row.value, "seed_set_id"),
        target_id: string_field(&row.value, "target_id"),
        tier: string_field(&row.value, "tier"),
        priority: string_field(&row.value, "priority"),
        status: string_field(&row.value, "status"),
        backlog_issue: string_field(&row.value, "backlog_issue"),
        profile_id: string_field(&row.value, "profile_id"),
        package_id: string_field(&row.value, "package_id"),
        package_version: string_field(&row.value, "package_version"),
        profile_quality_key: string_field(&row.value, "profile_quality_key"),
        minimum_manual_fsv: string_array_field(&row.value, "minimum_manual_fsv"),
        updated_at: string_field(&row.value, "updated_at"),
    }
}

fn report_quarantine(row: &ProfileRegistryStoredRow) -> ProfileRegistryReportQuarantine {
    ProfileRegistryReportQuarantine {
        row_key: row.summary.key.clone(),
        package_id: string_field(&row.value, "package_id"),
        package_version: string_field(&row.value, "package_version"),
        profile_id: string_field(&row.value, "profile_id"),
        profile_version: string_field(&row.value, "profile_version"),
        quarantine_reason: string_field(&row.value, "quarantine_reason"),
        error_code: string_field(&row.value, "error_code"),
        signature_status: string_field(&row.value, "signature_status"),
        signer_id: string_field(&row.value, "signer_id"),
        state: string_field(&row.value, "state"),
        updated_at: string_field(&row.value, "updated_at"),
    }
}

fn report_rollback(row: &ProfileRegistryStoredRow) -> ProfileRegistryReportRollback {
    ProfileRegistryReportRollback {
        row_key: row.summary.key.clone(),
        profile_id: string_field(&row.value, "profile_id"),
        from_package_id: string_field(&row.value, "from_package_id"),
        from_package_version: string_field(&row.value, "from_package_version"),
        to_package_id: string_field(&row.value, "to_package_id"),
        to_package_version: string_field(&row.value, "to_package_version"),
        reason: string_field(&row.value, "reason"),
        trust_status: string_field(&row.value, "trust_status"),
        signature_status: string_field(&row.value, "signature_status"),
        updated_at: string_field(&row.value, "updated_at"),
    }
}

fn summarize_report_audit(
    rows: &[(Vec<u8>, Vec<u8>)],
    profile_filter: Option<&str>,
    limit: usize,
) -> Result<ProfileRegistryReportAudit, ErrorData> {
    let mut report = ProfileRegistryReportAudit {
        cf_name: cf::CF_ACTION_LOG.to_owned(),
        rows_scanned: rows.len() as u64,
        matching_rows: 0,
        by_status: BTreeMap::new(),
        by_tool: BTreeMap::new(),
        by_error_code: BTreeMap::new(),
        latest_rows: Vec::new(),
    };
    for (key, value) in rows {
        let decoded = decode_json::<Value>(value).map_err(decode_error)?;
        if profile_filter.is_some_and(|profile_id| !value_mentions_profile(&decoded, profile_id)) {
            continue;
        }
        report.matching_rows += 1;
        if let Some(status) = string_field(&decoded, "status") {
            increment(&mut report.by_status, status);
        }
        if let Some(tool) = string_field(&decoded, "tool") {
            increment(&mut report.by_tool, tool);
        }
        if let Some(error_code) = string_field(&decoded, "error_code") {
            increment(&mut report.by_error_code, error_code);
        }
        if report.latest_rows.len() < limit {
            report.latest_rows.push(ProfileRegistryReportAuditRow {
                key_hex: hex_encode(key),
                profile_id: string_field(&decoded, "profile_id")
                    .or_else(|| string_field(&decoded, "active_profile_id")),
                foreground_profile_id: decoded
                    .pointer("/foreground/profile_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                tool: string_field(&decoded, "tool"),
                status: string_field(&decoded, "status"),
                error_code: string_field(&decoded, "error_code"),
            });
        }
    }
    Ok(report)
}

fn quality_row_profile_id(key: &[u8], value: &[u8]) -> Result<Option<ProfileId>, ErrorData> {
    let decoded = decode_json::<Value>(value).map_err(decode_error)?;
    Ok(string_field(&decoded, "profile_id").or_else(|| {
        String::from_utf8_lossy(key)
            .strip_prefix(QUALITY_PREFIX)
            .map(str::to_owned)
    }))
}

fn report_quality_snapshot(
    key: &[u8],
    value: &[u8],
) -> Result<ProfileRegistryReportQualitySnapshot, ErrorData> {
    let decoded = decode_json::<Value>(value).map_err(decode_error)?;
    let audit_rows_stale = decoded
        .pointer("/source/audit_rows_stale")
        .and_then(Value::as_u64);
    Ok(ProfileRegistryReportQualitySnapshot {
        row_key: String::from_utf8_lossy(key).into_owned(),
        key_hex: hex_encode(key),
        profile_id: string_field(&decoded, "profile_id"),
        generated_at_ns: decoded.get("generated_at_ns").and_then(Value::as_u64),
        evidence_hash: string_field(&decoded, "evidence_hash"),
        manual_fsv_evidence_ref: string_field(&decoded, "manual_fsv_evidence_ref"),
        score_0_100: decoded
            .pointer("/score/score_0_100")
            .and_then(Value::as_u64),
        sample_size: decoded
            .pointer("/score/sample_size")
            .and_then(Value::as_u64),
        audit_rows_scanned: decoded
            .pointer("/source/audit_rows_scanned")
            .and_then(Value::as_u64),
        audit_rows_profile_relevant: decoded
            .pointer("/source/audit_rows_profile_relevant")
            .and_then(Value::as_u64),
        audit_rows_stale,
        observation_rows_scanned: decoded
            .pointer("/runtime_evidence/observation_rows_scanned")
            .and_then(Value::as_u64),
        observation_rows_profile_relevant: decoded
            .pointer("/runtime_evidence/observation_rows_profile_relevant")
            .and_then(Value::as_u64),
        event_rows_scanned: decoded
            .pointer("/runtime_evidence/event_rows_scanned")
            .and_then(Value::as_u64),
        event_rows_profile_relevant: decoded
            .pointer("/runtime_evidence/event_rows_profile_relevant")
            .and_then(Value::as_u64),
        stale_after_ns: decoded
            .pointer("/source/stale_after_ns")
            .and_then(Value::as_u64),
        stale_evidence_present: audit_rows_stale.unwrap_or_default() > 0,
        quality_signal: string_field(&decoded, "quality_signal"),
        observed_target_ids: u64_map_field(&decoded, "/runtime_evidence/observed_target_ids"),
        observed_event_kinds: u64_map_field(&decoded, "/runtime_evidence/observed_event_kinds"),
        observed_log_event_kinds: u64_map_field(
            &decoded,
            "/runtime_evidence/observed_log_event_kinds",
        ),
    })
}

fn consent_row_profile_id(key: &[u8], value: &[u8]) -> Result<ProfileId, ErrorData> {
    let decoded = decode_json::<Value>(value).map_err(decode_error)?;
    string_field(&decoded, "profile_id")
        .or_else(|| {
            String::from_utf8_lossy(key)
                .strip_prefix(AUDIT_EXPORT_CONSENT_PREFIX)
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            registry_error(
                "audit_export_consent_profile_missing",
                "consent row missing profile_id",
            )
        })
}

fn report_consent_row(
    key: &[u8],
    value: &[u8],
    profile_id: ProfileId,
    matching_audit_rows: u64,
) -> Result<ProfileRegistryReportConsent, ErrorData> {
    let decoded = decode_json::<Value>(value).map_err(decode_error)?;
    let enabled = decoded
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let external_sharing_allowed = decoded
        .get("external_sharing_allowed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let redaction_policy = string_field(&decoded, "redaction_policy");
    Ok(ProfileRegistryReportConsent {
        row_key: String::from_utf8_lossy(key).into_owned(),
        key_hex: hex_encode(key),
        profile_id,
        enabled,
        state: string_field(&decoded, "state"),
        redaction_policy: redaction_policy.clone(),
        external_sharing_allowed,
        matching_audit_rows,
        audit_export_status: audit_export_status(
            enabled,
            redaction_policy.as_deref(),
            matching_audit_rows,
        ),
        updated_at: string_field(&decoded, "updated_at"),
    })
}

fn missing_consent_report(
    profile_id: &str,
    key: &str,
    matching_audit_rows: u64,
) -> ProfileRegistryReportConsent {
    ProfileRegistryReportConsent {
        row_key: key.to_owned(),
        key_hex: hex_encode(key.as_bytes()),
        profile_id: profile_id.to_owned(),
        enabled: false,
        state: Some("missing".to_owned()),
        redaction_policy: None,
        external_sharing_allowed: false,
        matching_audit_rows,
        audit_export_status: "consent_missing".to_owned(),
        updated_at: None,
    }
}

fn audit_export_status(
    enabled: bool,
    redaction_policy: Option<&str>,
    matching_audit_rows: u64,
) -> String {
    if !enabled {
        return "consent_disabled".to_owned();
    }
    if redaction_policy != Some("strict") {
        return "redaction_policy_not_strict".to_owned();
    }
    if matching_audit_rows == 0 {
        return "no_matching_audit_rows".to_owned();
    }
    "local_export_ready".to_owned()
}

fn count_matching_audit_rows(
    rows: &[(Vec<u8>, Vec<u8>)],
    profile_id: &str,
) -> Result<u64, ErrorData> {
    let mut count = 0_u64;
    for (_key, value) in rows {
        let decoded = decode_json::<Value>(value).map_err(decode_error)?;
        if value_mentions_profile(&decoded, profile_id) {
            count += 1;
        }
    }
    Ok(count)
}

fn registry_report_controls() -> Vec<ProfileRegistryReportControl> {
    vec![
        ProfileRegistryReportControl {
            tool: "profile_registry_install".to_owned(),
            purpose: "install or update a local registry package manifest".to_owned(),
            writes: "CF_PROFILES profile_registry/v1/* and CF_KV profile_registry/v1/head/*"
                .to_owned(),
            requires_explicit_operator_trigger: true,
        },
        ProfileRegistryReportControl {
            tool: "profile_registry_rollback".to_owned(),
            purpose: "move an installed profile back to a prior trusted package".to_owned(),
            writes: "CF_PROFILES installed profile and profile_registry/v1/rollback/*".to_owned(),
            requires_explicit_operator_trigger: true,
        },
        ProfileRegistryReportControl {
            tool: "profile_registry_export".to_owned(),
            purpose: "write an explicit local registry or contribution JSON bundle".to_owned(),
            writes: "operator-selected local output file".to_owned(),
            requires_explicit_operator_trigger: true,
        },
        ProfileRegistryReportControl {
            tool: "profile_registry_import".to_owned(),
            purpose: "stage or import an explicit local registry JSON bundle".to_owned(),
            writes: "CF_PROFILES/CF_KV registry rows after local validation".to_owned(),
            requires_explicit_operator_trigger: true,
        },
        ProfileRegistryReportControl {
            tool: "profile_quality_refresh".to_owned(),
            purpose: "refresh local quality scoring from stored action audit evidence".to_owned(),
            writes: "CF_PROFILES profile_quality/v1/<profile_id>".to_owned(),
            requires_explicit_operator_trigger: true,
        },
        ProfileRegistryReportControl {
            tool: "audit_export_bundle".to_owned(),
            purpose: "write a local redacted audit bundle after explicit consent".to_owned(),
            writes: "CF_KV audit_export/v1/consent/<profile_id> and operator-selected local output directory".to_owned(),
            requires_explicit_operator_trigger: true,
        },
    ]
}

fn select_prior_package_key(
    runtime: &MutexGuard<'_, ReflexRuntime>,
    profile_id: &str,
    current_package_id: &str,
    current_package_version: &str,
) -> Result<String, ErrorData> {
    let rows = runtime
        .storage_cf_prefix_rows(cf::CF_PROFILES, PACKAGE_PREFIX.as_bytes(), usize::MAX)
        .map_err(storage_error)?;
    let mut candidates = Vec::new();
    for (key, value) in rows {
        let row = decode_json::<Value>(&value).map_err(decode_error)?;
        if row.get("profile_id").and_then(Value::as_str) != Some(profile_id) {
            continue;
        }
        let package_id = row
            .get("package_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let package_version = row
            .get("package_version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if package_id == current_package_id && package_version == current_package_version {
            continue;
        }
        if !is_known_good_package(&row) {
            continue;
        }
        candidates.push((
            semver_sort_key(package_version),
            String::from_utf8_lossy(&key).into_owned(),
        ));
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    candidates.pop().map(|(_version, key)| key).ok_or_else(|| {
        rollback_unavailable_error(
            "rollback_prior_package_missing",
            format!("no prior known-good package row exists for profile {profile_id}"),
        )
    })
}

fn validate_rollback_target(target: &Value, profile_id: &str) -> Result<(), ErrorData> {
    if target.get("row_kind").and_then(Value::as_str) != Some("profile_package") {
        return Err(rollback_unavailable_error(
            "rollback_target_kind_invalid",
            "rollback target row must be a profile_package row",
        ));
    }
    if target.get("profile_id").and_then(Value::as_str) != Some(profile_id) {
        return Err(rollback_unavailable_error(
            "rollback_target_profile_mismatch",
            format!("rollback target package does not belong to profile {profile_id}"),
        ));
    }
    if !is_known_good_package(target) {
        return Err(rollback_unavailable_error(
            "rollback_target_not_known_good",
            "rollback target is not an active trusted/local-validated package",
        ));
    }
    Ok(())
}

fn is_known_good_package(value: &Value) -> bool {
    value.get("state").and_then(Value::as_str) == Some("active")
        && matches!(
            value.get("trust_status").and_then(Value::as_str),
            Some("trusted" | "local_validated")
        )
        && value.get("revoked").and_then(Value::as_bool) != Some(true)
}

fn semver_sort_key(version: &str) -> (u64, u64, u64, String) {
    let mut parts = version.splitn(2, ['-', '+']);
    let core = parts.next().unwrap_or_default();
    let suffix = parts.next().unwrap_or_default().to_owned();
    let mut numbers = core.split('.');
    let major = numbers
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0);
    let minor = numbers
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0);
    let patch = numbers
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0);
    (major, minor, patch, suffix)
}

fn required_string_from_value(
    value: &Value,
    field: &str,
    context: &str,
) -> Result<String, ErrorData> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            registry_error(
                "registry_required_field_missing",
                format!("{context} missing required string field {field}"),
            )
        })
}

fn inspect_key(params: &ProfileRegistryQueryParams) -> Result<(&'static str, String), ErrorData> {
    if let Some(row_key) = params
        .row_key
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        if row_key.starts_with(HEAD_PREFIX) {
            return Ok((cf::CF_KV, row_key.to_owned()));
        }
        if row_key.starts_with(REGISTRY_PREFIX) {
            return Ok((cf::CF_PROFILES, row_key.to_owned()));
        }
        return Err(registry_error(
            "registry_row_key_invalid",
            "row_key must start with profile_registry/v1/",
        ));
    }
    if let Some(source_id) = &params.source_id {
        return Ok((cf::CF_PROFILES, source_key(source_id)));
    }
    if let (Some(package_id), Some(package_version)) = (&params.package_id, &params.package_version)
    {
        return Ok((cf::CF_PROFILES, package_key(package_id, package_version)));
    }
    if let (Some(profile_id), Some(profile_version)) = (&params.profile_id, &params.profile_version)
    {
        return Ok((cf::CF_PROFILES, profile_key(profile_id, profile_version)));
    }
    if let Some(profile_id) = &params.installed_profile_id {
        return Ok((cf::CF_PROFILES, installed_key(profile_id)));
    }
    Err(registry_error(
        "registry_inspect_target_missing",
        "provide row_key, source_id, package_id+package_version, profile_id+profile_version, or installed_profile_id",
    ))
}

fn row_filter_matches(
    summary: &ProfileRegistryRowSummary,
    value: &[u8],
    query: Option<&str>,
    row_kind: Option<&str>,
    include_disabled: bool,
) -> bool {
    if !include_disabled
        && matches!(
            summary.state.as_deref(),
            Some("disabled" | "removed" | "staged" | "quarantined" | "rejected")
        )
    {
        return false;
    }
    if let Some(row_kind) = row_kind
        && summary.row_kind.as_deref() != Some(row_kind)
    {
        return false;
    }
    query.is_none_or(|query| {
        summary.key.to_ascii_lowercase().contains(query)
            || summary
                .value_utf8_prefix
                .to_ascii_lowercase()
                .contains(query)
            || String::from_utf8_lossy(value)
                .to_ascii_lowercase()
                .contains(query)
    })
}

fn registry_row_contributes_to_profile(
    summary: &ProfileRegistryRowSummary,
    value: &[u8],
    profile_id: &str,
) -> bool {
    if summary.profile_id.as_deref() == Some(profile_id) {
        return true;
    }
    if decode_json::<Value>(value)
        .ok()
        .is_some_and(|decoded| value_mentions_profile(&decoded, profile_id))
    {
        return true;
    }
    matches!(
        summary.row_kind.as_deref(),
        Some("registry_source" | "registry_head" | "trust_root")
    )
}

fn sort_bundle_rows(rows: &mut [ProfileRegistryBundleRow]) {
    rows.sort_by(|left, right| {
        left.cf_name
            .cmp(&right.cf_name)
            .then_with(|| left.key.cmp(&right.key))
    });
}

fn contribution_audit_row(
    profile_id_filter: Option<&str>,
    key: &[u8],
    value: &[u8],
) -> Result<Option<ProfileRegistryAuditEvidenceRow>, ErrorData> {
    let decoded = decode_json::<Value>(value).map_err(decode_error)?;
    if let Some(profile_id) = profile_id_filter
        && !value_mentions_profile(&decoded, profile_id)
    {
        return Ok(None);
    }
    Ok(Some(ProfileRegistryAuditEvidenceRow {
        cf_name: cf::CF_ACTION_LOG.to_owned(),
        key_hex: hex_encode(key),
        value_sha256: sha256_hex(value),
        profile_id: string_field(&decoded, "profile_id"),
        foreground_profile_id: decoded
            .pointer("/foreground/profile_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        foreground_process_name: decoded
            .pointer("/foreground/process_name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tool: string_field(&decoded, "tool"),
        status: string_field(&decoded, "status"),
        error_code: string_field(&decoded, "error_code"),
        backend_used: decoded
            .pointer("/details/response/backend_used")
            .or_else(|| decoded.pointer("/details/response/backend"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        profile_schema_version: decoded
            .pointer("/foreground/profile_schema_version")
            .or_else(|| decoded.get("profile_schema_version"))
            .and_then(Value::as_u64),
    }))
}

fn contribution_quality_summary(
    key: &[u8],
    value: &[u8],
) -> Result<ProfileRegistryQualitySummaryRow, ErrorData> {
    let decoded = decode_json::<Value>(value).map_err(decode_error)?;
    Ok(ProfileRegistryQualitySummaryRow {
        cf_name: cf::CF_PROFILES.to_owned(),
        key: String::from_utf8_lossy(key).into_owned(),
        key_hex: hex_encode(key),
        value_sha256: sha256_hex(value),
        profile_id: string_field(&decoded, "profile_id"),
        evidence_hash: string_field(&decoded, "evidence_hash"),
        score_0_100: decoded
            .pointer("/score/score_0_100")
            .and_then(Value::as_u64),
        sample_size: decoded
            .pointer("/score/sample_size")
            .and_then(Value::as_u64),
        quality_signal: string_field(&decoded, "quality_signal"),
        reality_delta_rows: decoded
            .pointer("/reality_evidence/delta_rows")
            .and_then(Value::as_u64),
        reality_audit_rows: decoded
            .pointer("/reality_evidence/audit_rows")
            .and_then(Value::as_u64),
        reality_audited_delta_rows: decoded
            .pointer("/reality_evidence/audited_delta_rows")
            .and_then(Value::as_u64),
        reality_rebase_required_rows: decoded
            .pointer("/reality_evidence/rebase_required_rows")
            .and_then(Value::as_u64),
        reality_drift_rate: decoded
            .pointer("/reality_evidence/drift_rate")
            .and_then(Value::as_f64),
        delta_first_supported: decoded
            .pointer("/reality_evidence/delta_first_supported")
            .and_then(Value::as_bool),
        full_snapshot_required: decoded
            .pointer("/reality_evidence/full_snapshot_required")
            .and_then(Value::as_bool),
    })
}

fn redact_contribution_registry_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let path_keys = object
                .keys()
                .filter(|key| {
                    key.as_str() == "path"
                        || key.ends_with("_path")
                        || key.ends_with("_paths")
                        || key.ends_with("Path")
                })
                .cloned()
                .collect::<Vec<_>>();
            for key in path_keys {
                object.remove(&key);
            }
            for value in object.values_mut() {
                redact_contribution_registry_value(value);
            }
        }
        Value::Array(items) => {
            for value in items {
                redact_contribution_registry_value(value);
            }
        }
        _ => {}
    }
}

fn validate_bundle_hashes(bundle: &ProfileRegistryExportBundle) -> Result<(), ErrorData> {
    let registry_rows_sha256 = hash_json(&bundle.rows)?;
    verify_optional_hash(
        "registry_rows_sha256",
        bundle.registry_rows_sha256.as_deref(),
        &registry_rows_sha256,
    )?;
    let audit_evidence_sha256 = hash_json(&bundle.audit_evidence)?;
    verify_optional_hash(
        "audit_evidence_sha256",
        bundle.audit_evidence_sha256.as_deref(),
        &audit_evidence_sha256,
    )?;
    let quality_summary_sha256 = hash_json(&bundle.quality_summaries)?;
    verify_optional_hash(
        "quality_summary_sha256",
        bundle.quality_summary_sha256.as_deref(),
        &quality_summary_sha256,
    )?;
    let content_hash = contribution_content_hash(
        bundle.bundle_kind.as_str(),
        bundle.profile_id.as_deref(),
        &bundle.merge_rules,
        &bundle.rows,
        &bundle.audit_evidence,
        &bundle.quality_summaries,
    )?;
    verify_optional_hash(
        "deterministic_bundle_sha256",
        bundle.deterministic_bundle_sha256.as_deref(),
        &content_hash,
    )?;
    if bundle.bundle_kind == CONTRIBUTION_BUNDLE_KIND
        && bundle.deterministic_bundle_sha256.is_none()
    {
        return Err(registry_error(
            "contribution_bundle_hash_missing",
            "contribution bundle must carry deterministic_bundle_sha256",
        ));
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "review rules are kept linear so each quarantine/de-rank reason is audit-readable"
)]
fn review_contribution_bundle(
    bundle: &ProfileRegistryExportBundle,
    registry_rows_read: u64,
) -> ContributionReview {
    let external_success_evidence_rows = bundle
        .audit_evidence
        .iter()
        .filter(|row| row.status.as_deref() == Some("ok"))
        .count() as u64;
    let mut terminal_risk_flags = Vec::new();
    let mut advisory_risk_flags = Vec::new();
    let mut trust_downgrade_reasons = Vec::new();

    if bundle.profile_id.as_deref().is_none_or(str::is_empty) {
        push_unique(&mut terminal_risk_flags, "contribution_profile_id_missing");
        push_unique(
            &mut trust_downgrade_reasons,
            "missing_profile_id_prevents_safe_review",
        );
    }
    if bundle.rows.is_empty() {
        push_unique(&mut terminal_risk_flags, "contribution_registry_rows_empty");
        push_unique(
            &mut trust_downgrade_reasons,
            "empty_contribution_cannot_improve_registry",
        );
    }
    if bundle.rows.len() > MAX_CONTRIBUTION_IMPORT_ROWS {
        push_unique(
            &mut terminal_risk_flags,
            "contribution_registry_rows_limit_exceeded",
        );
        push_unique(
            &mut trust_downgrade_reasons,
            "row_volume_exceeds_local_review_limit",
        );
    }
    if bundle.audit_evidence.len() > MAX_CONTRIBUTION_AUDIT_ROWS as usize {
        push_unique(
            &mut terminal_risk_flags,
            "contribution_audit_evidence_limit_exceeded",
        );
        push_unique(
            &mut trust_downgrade_reasons,
            "audit_evidence_volume_exceeds_local_review_limit",
        );
    }

    for row in &bundle.rows {
        if contribution_value_has_poison_marker(&row.value) {
            push_unique(&mut terminal_risk_flags, "metadata_prompt_injection_text");
            push_unique(
                &mut trust_downgrade_reasons,
                "untrusted_metadata_contains_agent_instruction_marker",
            );
        }
        if contribution_value_requests_unsafe_permission(&row.value) {
            push_unique(
                &mut terminal_risk_flags,
                "unsafe_action_or_permission_metadata",
            );
            push_unique(
                &mut trust_downgrade_reasons,
                "contribution_requests_unsafe_runtime_permission",
            );
        }
        if let Some(profile_id) = bundle.profile_id.as_deref()
            && !contribution_row_is_global(row)
            && !value_mentions_profile(&row.value, profile_id)
        {
            push_unique(
                &mut terminal_risk_flags,
                "contribution_registry_row_profile_mismatch",
            );
            push_unique(
                &mut trust_downgrade_reasons,
                "registry_row_does_not_match_bundle_profile_id",
            );
        }
    }

    for evidence in &bundle.audit_evidence {
        if let Some(profile_id) = bundle.profile_id.as_deref() {
            let has_profile_field =
                evidence.profile_id.is_some() || evidence.foreground_profile_id.is_some();
            let mentions_bundle_profile = evidence.profile_id.as_deref() == Some(profile_id)
                || evidence.foreground_profile_id.as_deref() == Some(profile_id);
            if has_profile_field && !mentions_bundle_profile {
                push_unique(&mut terminal_risk_flags, "audit_evidence_profile_mismatch");
                push_unique(
                    &mut trust_downgrade_reasons,
                    "audit_evidence_does_not_match_bundle_profile_id",
                );
            }
        }
    }

    for summary in &bundle.quality_summaries {
        if let Some(profile_id) = bundle.profile_id.as_deref()
            && summary.profile_id.as_deref() != Some(profile_id)
        {
            push_unique(&mut terminal_risk_flags, "quality_summary_profile_mismatch");
            push_unique(
                &mut trust_downgrade_reasons,
                "quality_summary_does_not_match_bundle_profile_id",
            );
        }
        let score = summary.score_0_100.unwrap_or_default();
        let sample_size = summary.sample_size.unwrap_or_default();
        if score > 100 {
            push_unique(&mut terminal_risk_flags, "quality_score_out_of_range");
            push_unique(
                &mut trust_downgrade_reasons,
                "quality_score_claim_exceeds_valid_range",
            );
        }
        if sample_size > bundle.audit_evidence.len() as u64 {
            push_unique(
                &mut terminal_risk_flags,
                "quality_sample_exceeds_exported_evidence",
            );
            push_unique(
                &mut trust_downgrade_reasons,
                "quality_sample_claim_not_supported_by_exported_evidence",
            );
        }
        if score >= HIGH_QUALITY_SCORE_THRESHOLD && sample_size < MIN_HIGH_QUALITY_EVIDENCE_ROWS {
            push_unique(
                &mut terminal_risk_flags,
                "quality_score_high_with_low_evidence",
            );
            push_unique(
                &mut trust_downgrade_reasons,
                "high_quality_claim_requires_more_verified_evidence",
            );
        }
        if score >= HIGH_QUALITY_SCORE_THRESHOLD
            && (bundle.audit_evidence.len() as u64) < MIN_HIGH_QUALITY_EVIDENCE_ROWS
        {
            push_unique(
                &mut terminal_risk_flags,
                "quality_score_high_with_low_exported_evidence",
            );
            push_unique(
                &mut trust_downgrade_reasons,
                "high_quality_claim_requires_more_exported_evidence",
            );
        }
        if score > 0 && external_success_evidence_rows == 0 {
            push_unique(
                &mut advisory_risk_flags,
                "quality_score_without_success_evidence",
            );
            push_unique(
                &mut trust_downgrade_reasons,
                "quality_score_has_no_success_evidence_in_bundle",
            );
        }
    }

    if external_success_evidence_rows == 0 {
        push_unique(&mut advisory_risk_flags, "low_quality_no_success_evidence");
        push_unique(
            &mut trust_downgrade_reasons,
            "no_success_evidence_in_bundle",
        );
    }
    if registry_rows_read > external_success_evidence_rows.saturating_mul(25).max(25) {
        push_unique(
            &mut advisory_risk_flags,
            "registry_row_volume_exceeds_success_evidence",
        );
        push_unique(
            &mut trust_downgrade_reasons,
            "registry_row_volume_not_backed_by_success_evidence",
        );
    }

    if !terminal_risk_flags.is_empty() {
        terminal_risk_flags.extend(advisory_risk_flags);
        terminal_risk_flags.sort();
        terminal_risk_flags.dedup();
        trust_downgrade_reasons.sort();
        trust_downgrade_reasons.dedup();
        return ContributionReview::quarantined(
            registry_rows_read,
            external_success_evidence_rows,
            terminal_risk_flags,
            trust_downgrade_reasons,
        );
    }

    let mut review = ContributionReview::staged(registry_rows_read, external_success_evidence_rows);
    review.risk_flags = advisory_risk_flags;
    review.risk_flags.sort();
    review.risk_flags.dedup();
    review
        .trust_downgrade_reasons
        .extend(trust_downgrade_reasons);
    review.trust_downgrade_reasons.sort();
    review.trust_downgrade_reasons.dedup();
    review
}

fn contribution_row_is_global(row: &ProfileRegistryBundleRow) -> bool {
    let row_kind = row.value.get("row_kind").and_then(Value::as_str);
    matches!(
        row_kind,
        Some("registry_source" | "registry_head" | "registry_trust_root")
    ) || row.key.starts_with(SOURCE_PREFIX)
        || row.key.starts_with(HEAD_PREFIX)
        || row.key.starts_with(TRUST_ROOT_PREFIX)
}

fn contribution_value_has_poison_marker(value: &Value) -> bool {
    match value {
        Value::String(text) => {
            let normalized = text.to_ascii_lowercase();
            CONTRIBUTION_POISON_MARKERS
                .iter()
                .any(|marker| normalized.contains(marker))
        }
        Value::Array(items) => items.iter().any(contribution_value_has_poison_marker),
        Value::Object(object) => object.values().any(contribution_value_has_poison_marker),
        _ => false,
    }
}

fn contribution_value_requests_unsafe_permission(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (key == "remote_server_allowed" && value.as_bool() == Some(true))
                || (key == "local_only" && value.as_bool() == Some(false))
                || (key == "shell_allowed" && value.as_bool() == Some(true))
                || contribution_value_requests_unsafe_permission(value)
        }),
        Value::Array(items) => items
            .iter()
            .any(contribution_value_requests_unsafe_permission),
        _ => false,
    }
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|item| item == value) {
        values.push(value.to_owned());
    }
}

fn verify_optional_hash(
    field: &str,
    expected: Option<&str>,
    actual: &str,
) -> Result<(), ErrorData> {
    if let Some(expected) = expected
        && expected != actual
    {
        return Err(registry_error(
            "registry_bundle_hash_mismatch",
            format!("{field} mismatch: expected {expected}, actual {actual}"),
        ));
    }
    Ok(())
}

fn contribution_content_hash(
    bundle_kind: &str,
    profile_id: Option<&str>,
    merge_rules: &[String],
    rows: &[ProfileRegistryBundleRow],
    audit_evidence: &[ProfileRegistryAuditEvidenceRow],
    quality_summaries: &[ProfileRegistryQualitySummaryRow],
) -> Result<String, ErrorData> {
    #[derive(Serialize)]
    struct DeterministicContent<'a> {
        schema_version: u32,
        bundle_kind: &'a str,
        profile_id: Option<&'a str>,
        merge_rules: &'a [String],
        rows: &'a [ProfileRegistryBundleRow],
        audit_evidence: &'a [ProfileRegistryAuditEvidenceRow],
        quality_summaries: &'a [ProfileRegistryQualitySummaryRow],
    }
    let content = DeterministicContent {
        schema_version: SCHEMA_VERSION,
        bundle_kind,
        profile_id,
        merge_rules,
        rows,
        audit_evidence,
        quality_summaries,
    };
    hash_json(&content)
}

fn hash_json<T: Serialize>(value: &T) -> Result<String, ErrorData> {
    serde_json::to_vec(value)
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("profile registry hash encode failed: {error}"),
            )
        })
}

fn filter_duplicate_or_conflicting_rows(
    runtime: &MutexGuard<'_, ReflexRuntime>,
    profile_rows: &mut EncodedRows,
    kv_rows: &mut EncodedRows,
) -> Result<u64, ErrorData> {
    let profile_duplicates =
        filter_duplicate_or_conflicting_cf_rows(runtime, cf::CF_PROFILES, profile_rows)?;
    let kv_duplicates = filter_duplicate_or_conflicting_cf_rows(runtime, cf::CF_KV, kv_rows)?;
    Ok(profile_duplicates + kv_duplicates)
}

fn filter_duplicate_or_conflicting_cf_rows(
    runtime: &MutexGuard<'_, ReflexRuntime>,
    cf_name: &str,
    rows: &mut EncodedRows,
) -> Result<u64, ErrorData> {
    let mut duplicates = 0;
    let mut retained = Vec::new();
    for (key, value) in rows.drain(..) {
        let existing = if cf_name == cf::CF_PROFILES {
            runtime.storage_profile_row(&key).map_err(storage_error)?
        } else {
            runtime.storage_kv_row(&key).map_err(storage_error)?
        };
        match existing {
            Some(existing) if rows_are_duplicate(cf_name, &key, &existing, &value) => {
                duplicates += 1;
            }
            Some(existing) => {
                let existing_hash = sha256_hex(&existing);
                let incoming_hash = sha256_hex(&value);
                return Err(registry_error(
                    "registry_bundle_conflict",
                    format!(
                        "import row {} in {cf_name} conflicts with existing local row: existing {existing_hash}, incoming {incoming_hash}",
                        String::from_utf8_lossy(&key)
                    ),
                ));
            }
            None => retained.push((key, value)),
        }
    }
    *rows = retained;
    Ok(duplicates)
}

fn rows_are_duplicate(cf_name: &str, key: &[u8], existing: &[u8], incoming: &[u8]) -> bool {
    if existing == incoming {
        return true;
    }
    cf_name == cf::CF_PROFILES
        && key.starts_with(CONTRIBUTION_PREFIX.as_bytes())
        && contribution_rows_are_semantic_duplicates(existing, incoming)
}

fn contribution_rows_are_semantic_duplicates(existing: &[u8], incoming: &[u8]) -> bool {
    let Some(mut existing) = decode_json::<Value>(existing).ok() else {
        return false;
    };
    let Some(mut incoming) = decode_json::<Value>(incoming).ok() else {
        return false;
    };
    if existing.get("row_kind").and_then(Value::as_str) != Some("profile_contribution_bundle")
        || incoming.get("row_kind").and_then(Value::as_str) != Some("profile_contribution_bundle")
    {
        return false;
    }
    remove_object_field(&mut existing, "bundle_file_sha256");
    remove_object_field(&mut incoming, "bundle_file_sha256");
    remove_object_field(&mut existing, "created_at");
    remove_object_field(&mut incoming, "created_at");
    remove_object_field(&mut existing, "updated_at");
    remove_object_field(&mut incoming, "updated_at");
    remove_object_field(&mut existing, "reviewed_at");
    remove_object_field(&mut incoming, "reviewed_at");
    existing == incoming
}

fn contribution_import_row(
    bundle_path: &Path,
    bundle: &ProfileRegistryExportBundle,
    registry_rows_read: u64,
    review: Option<&ContributionReview>,
) -> Result<Value, ErrorData> {
    let bundle_file_sha256 = fs::read(bundle_path)
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "profile_registry_import could not re-read bundle {}: {error}",
                    bundle_path.display()
                ),
            )
        })?;
    let updated_at = Utc::now().to_rfc3339();
    let default_review = ContributionReview::staged(registry_rows_read, 0);
    let review = review.unwrap_or(&default_review);
    Ok(json!({
        "schema_version": SCHEMA_VERSION,
        "row_kind": "profile_contribution_bundle",
        "row_id": contribution_key(
            bundle.profile_id.as_deref().unwrap_or("unknown-profile"),
            bundle.deterministic_bundle_sha256.as_deref(),
        ),
        "created_at": updated_at,
        "updated_at": updated_at,
        "reviewed_at": updated_at,
        "source_id": "registry.contribution.local",
        "state": review.review_state,
        "activation_state": review.review_state,
        "bundle_kind": bundle.bundle_kind.clone(),
        "profile_id": bundle.profile_id.clone(),
        "deterministic_bundle_sha256": bundle.deterministic_bundle_sha256.clone(),
        "bundle_file_sha256": bundle_file_sha256,
        "registry_rows_read": registry_rows_read,
        "registry_rows_accepted": review.registry_rows_accepted,
        "registry_rows_rejected": review.registry_rows_rejected,
        "audit_evidence_rows": bundle.audit_evidence.len() as u64,
        "quality_summary_rows": bundle.quality_summaries.len() as u64,
        "registry_rows_sha256": bundle.registry_rows_sha256.clone(),
        "audit_evidence_sha256": bundle.audit_evidence_sha256.clone(),
        "quality_summary_sha256": bundle.quality_summary_sha256.clone(),
        "merge_rules": bundle.merge_rules.clone(),
        "review_state": review.review_state,
        "risk_flags": review.risk_flags.clone(),
        "trust_downgrade_reasons": review.trust_downgrade_reasons.clone(),
        "rank_eligible": review.rank_eligible,
        "quality_weight": review.quality_weight,
        "external_success_evidence_rows": review.external_success_evidence_rows,
        "local_success_evidence_rows": review.local_success_evidence_rows,
        "external_quality_claims_trusted": false,
        "audit_evidence": bundle.audit_evidence.clone(),
        "quality_summaries": bundle.quality_summaries.clone(),
        "external_sharing_allowed": false,
    }))
}

fn validate_bundle_row(row: &ProfileRegistryBundleRow) -> Result<(), ErrorData> {
    if row.cf_name != cf::CF_PROFILES && row.cf_name != cf::CF_KV {
        return Err(registry_error(
            "registry_bundle_cf_invalid",
            format!(
                "bundle row cf_name must be CF_PROFILES or CF_KV; got {}",
                row.cf_name
            ),
        ));
    }
    if !row.key.starts_with(REGISTRY_PREFIX) {
        return Err(registry_error(
            "registry_bundle_key_invalid",
            "bundle row key must start with profile_registry/v1/",
        ));
    }
    if row.cf_name == cf::CF_KV && !row.key.starts_with(HEAD_PREFIX) {
        return Err(registry_error(
            "registry_bundle_kv_key_invalid",
            "CF_KV registry import rows must use profile_registry/v1/head/",
        ));
    }
    if !row.value.is_object() {
        return Err(registry_error(
            "registry_bundle_value_invalid",
            "bundle row value must be a JSON object",
        ));
    }
    let schema_version = row
        .value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if schema_version != u64::from(SCHEMA_VERSION) {
        return Err(registry_error(
            "registry_bundle_row_schema_invalid",
            format!("bundle row schema_version must be {SCHEMA_VERSION}; got {schema_version}"),
        ));
    }
    Ok(())
}

fn resolve_package_file(manifest_path: &Path, package_path: &str) -> PathBuf {
    let raw = PathBuf::from(package_path);
    if raw.is_absolute() || raw.exists() {
        return raw;
    }
    manifest_path
        .parent()
        .map_or_else(|| raw.clone(), |parent| parent.join(&raw))
}

fn required_path(field: &str, value: &str) -> Result<PathBuf, ErrorData> {
    validate_non_empty(field, value)?;
    Ok(PathBuf::from(value))
}

fn validate_limit(limit: u32) -> Result<(), ErrorData> {
    if (1..=MAX_SEARCH_LIMIT).contains(&limit) {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("limit must be 1..={MAX_SEARCH_LIMIT}; got {limit}"),
    ))
}

fn validate_contribution_audit_rows(limit: u32) -> Result<(), ErrorData> {
    if (1..=MAX_CONTRIBUTION_AUDIT_ROWS).contains(&limit) {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("max_audit_rows must be 1..={MAX_CONTRIBUTION_AUDIT_ROWS}; got {limit}"),
    ))
}

fn normalized_bundle_kind(value: &str) -> Result<String, ErrorData> {
    let normalized = value.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        REGISTRY_BUNDLE_KIND | CONTRIBUTION_BUNDLE_KIND
    ) {
        return Ok(normalized);
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!(
            "bundle_kind must be {REGISTRY_BUNDLE_KIND:?} or {CONTRIBUTION_BUNDLE_KIND:?}; got {value:?}"
        ),
    ))
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), ErrorData> {
    if !value.trim().is_empty() {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("{field} must not be empty"),
    ))
}

fn validate_registry_id(field: &str, value: &str) -> Result<(), ErrorData> {
    validate_non_empty(field, value)?;
    if value.chars().all(|item| {
        item.is_ascii_lowercase() || item.is_ascii_digit() || matches!(item, '.' | '-' | '_')
    }) {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("{field} must use lowercase ascii letters, digits, '.', '-', or '_'"),
    ))
}

fn validate_disabled_state(value: &str) -> Result<(), ErrorData> {
    if matches!(value, "disabled" | "removed") {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("profile_registry_disable state must be disabled or removed; got {value:?}"),
    ))
}

fn validate_install_trust_policy(value: &str) -> Result<(), ErrorData> {
    if matches!(value, "local_first" | "signed_required") {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!(
            "profile_registry_install trust_policy must be local_first or signed_required; got {value:?}"
        ),
    ))
}

fn normalized_query(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
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

fn set_object_field(value: &mut Value, field: &str, next: Value) {
    if let Value::Object(object) = value {
        object.insert(field.to_owned(), next);
    }
}

fn remove_object_field(value: &mut Value, field: &str) {
    if let Value::Object(object) = value {
        object.remove(field);
    }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn string_array_field(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn u64_map_field(value: &Value, pointer: &str) -> BTreeMap<String, u64> {
    value
        .pointer(pointer)
        .and_then(Value::as_object)
        .map(|items| {
            items
                .iter()
                .filter_map(|(key, value)| value.as_u64().map(|count| (key.clone(), count)))
                .collect()
        })
        .unwrap_or_default()
}

fn increment(counts: &mut BTreeMap<String, u64>, key: String) {
    *counts.entry(key).or_default() += 1;
}

fn source_key(source_id: &str) -> String {
    format!("{SOURCE_PREFIX}{source_id}")
}

fn package_key(package_id: &str, package_version: &str) -> String {
    format!("{PACKAGE_PREFIX}{package_id}/{package_version}")
}

fn profile_key(profile_id: &str, profile_version: &str) -> String {
    format!("{PROFILE_PREFIX}{profile_id}/{profile_version}")
}

fn installed_key(profile_id: &str) -> String {
    format!("{INSTALLED_PREFIX}{profile_id}")
}

fn compat_key(target_id: &str, profile_id: &str, profile_version: &str) -> String {
    format!("{COMPAT_PREFIX}{target_id}/{profile_id}/{profile_version}")
}

fn curated_target_key(seed_set_id: &str, target_id: &str) -> String {
    format!("{CURATED_TARGET_PREFIX}{seed_set_id}/{target_id}")
}

fn quality_link_key(profile_id: &str, profile_version: &str) -> String {
    format!("{QUALITY_LINK_PREFIX}{profile_id}/{profile_version}")
}

fn trust_root_key(root: &TrustRoot) -> String {
    format!(
        "{TRUST_ROOT_PREFIX}{}/{}",
        root.signer_id,
        root.key_id.strip_prefix("sha256:").unwrap_or(root.key_id)
    )
}

fn quarantine_key(package_id: &str, package_version: &str, manifest_digest: &str) -> String {
    let digest = manifest_digest
        .strip_prefix("sha256:")
        .unwrap_or(manifest_digest)
        .chars()
        .take(16)
        .collect::<String>();
    format!("{QUARANTINE_PREFIX}{package_id}/{package_version}/{digest}")
}

fn rollback_key(profile_id: &str, updated_at: &str) -> String {
    let timestamp = updated_at
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("{ROLLBACK_PREFIX}{profile_id}/{timestamp}")
}

fn contribution_key(profile_id: &str, digest: Option<&str>) -> String {
    let digest = digest
        .and_then(|value| value.strip_prefix("sha256:").or(Some(value)))
        .unwrap_or("missing-digest");
    format!("{CONTRIBUTION_PREFIX}{profile_id}/{digest}")
}

fn head_key(source_id: &str) -> String {
    format!("{HEAD_PREFIX}{source_id}")
}

fn quality_key(profile_id: &str) -> String {
    format!("profile_quality/v1/{profile_id}")
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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_encode(&digest))
}

fn utf8_prefix(bytes: &[u8], max_chars: usize) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(max_chars)
        .collect()
}

const fn default_search_limit() -> u32 {
    DEFAULT_SEARCH_LIMIT
}

const fn default_report_audit_rows() -> u32 {
    DEFAULT_REPORT_AUDIT_ROWS
}

fn default_bundle_kind() -> String {
    DEFAULT_BUNDLE_KIND.to_owned()
}

const fn default_include_audit_evidence() -> bool {
    true
}

const fn default_include_quality_summary() -> bool {
    true
}

const fn default_contribution_audit_rows() -> u32 {
    DEFAULT_CONTRIBUTION_AUDIT_ROWS
}

fn default_source_id() -> String {
    DEFAULT_SOURCE_ID.to_owned()
}

fn merge_rules() -> Vec<String> {
    vec![
        "identical_existing_rows_are_skipped".to_owned(),
        "same_deterministic_contribution_rows_are_skipped_even_if_bundle_file_hash_differs"
            .to_owned(),
        "same_key_different_value_fails_closed".to_owned(),
        "contribution_evidence_is_staged_under_profile_registry_contribution_rows".to_owned(),
        "redacted_audit_evidence_is_not_imported_into_CF_ACTION_LOG".to_owned(),
        "contribution_import_runs_local_risk_review_before_active_writes".to_owned(),
        "hostile_contribution_writes_quarantine_row_only".to_owned(),
        "external_quality_claims_do_not_rank_without_local_success".to_owned(),
        "curated_target_rows_require_manifest_compatibility_metadata".to_owned(),
    ]
}

fn default_install_trust_policy() -> String {
    DEFAULT_INSTALL_TRUST_POLICY.to_owned()
}

fn default_disabled_state() -> String {
    "disabled".to_owned()
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
fn profile_error(error: ProfileError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err receives owned errors and this adapter preserves the simple call sites"
)]
fn decode_error(error: synapse_storage::StorageError) -> ErrorData {
    mcp_error(
        error_codes::STORAGE_CORRUPTED,
        format!("profile registry row decode failed: {error}"),
    )
}

fn registry_error(reason: &'static str, message: impl Into<String>) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "reason": reason,
        })),
    )
}

fn rollback_unavailable_error(reason: &'static str, message: impl Into<String>) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": error_codes::PROFILE_ROLLBACK_UNAVAILABLE,
            "reason": reason,
        })),
    )
}

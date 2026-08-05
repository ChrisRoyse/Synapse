//! Local prompt-injection hygiene scanner (#872).
//!
//! The scanner is deliberately detection-only: it writes queryable flag rows
//! that point back to physical storage rows, but it never blocks content.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use chrono::{DateTime, Utc};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use synapse_core::types::{
    RoutineGranularity, RoutineLifecycle, RoutineRecord, RoutineStateRecord, TimelineActor,
    TimelineRecord,
};
use synapse_core::{
    OcrResult, SCHEMA_VERSION, StoredObservation, SuspectedInjectionAnnotation,
    SuspectedInjectionSpan, error_codes,
};
use synapse_reflex::ReflexRuntime;
use synapse_storage::{
    Db, cf, decode_json, encode_json, episodes as episode_codec, routines as routine_codec,
    timeline as timeline_codec,
};

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    episodes::decode_episode_row,
    permissions::{Permission, RequiredPermissions, required},
    profile_authoring::ProfileAuthoringCandidate,
};

const FLAG_PREFIX: &str = "hygiene/flag/v1/";
const DEFAULT_MIN_SCORE: u32 = 50;
const MAX_SCORE: u32 = 100;
const MAX_SCAN_TEXT_BYTES: usize = 262_144;
const DEFAULT_STORAGE_ROW_LIMIT: u32 = 1_000;
const MAX_STORAGE_ROW_LIMIT: u32 = 10_000;
const DEFAULT_FLAG_LIMIT: u32 = 200;
const MAX_FLAG_LIMIT: u32 = 1_000;
const STORAGE_SCAN_CHUNK_ROWS: usize = 512;

/// One local day in nanoseconds. Episodes are split at every local-midnight
/// (`EpisodeBoundary::DayBoundary`), so no episode spans more than one local
/// day. That makes a 24h guard a *provable* lower bound for the earliest
/// episode that can still contain a flagged timeline timestamp: an episode
/// covering `ts` has `start_ts_ns > ts - DAY_NS`. `hygiene_report` uses it to
/// bound the `CF_EPISODES` scan instead of walking the whole store.
const DAY_NS: u64 = 86_400 * 1_000_000_000;

/// Upper bound on `CF_EPISODES` rows scanned in one `hygiene_report` call. A
/// truncated derivation would silently under-report poisoned learned state, so
/// exhaustion is a loud error, never a partial answer.
const MAX_REPORT_EPISODE_SCAN_ROWS: usize = 500_000;

/// Upper bound on `CF_ROUTINES` rows scanned in one `hygiene_report` call. The
/// routine store holds at most a few hundred rows; this is a runaway backstop.
const MAX_REPORT_ROUTINE_SCAN_ROWS: usize = 50_000;

/// Prefix for profile-authoring candidates in `CF_PROFILES`.
const PROFILE_AUTHORING_CANDIDATE_PREFIX: &str = "profile_authoring/v1/candidate/";

/// Upper bound on candidate rows scanned in one `hygiene_report` call. Candidate
/// rows are installable downstream artifacts; truncating this scan would hide
/// the highest-risk poisoned state, so exhaustion is a loud error.
const MAX_REPORT_AUTHORING_CANDIDATE_SCAN_ROWS: usize = 50_000;

const SOURCE_CF_OBSERVATIONS: &str = cf::CF_OBSERVATIONS;
const SOURCE_CF_TIMELINE: &str = cf::CF_TIMELINE;
const SOURCE_CF_OCR_CACHE: &str = cf::CF_OCR_CACHE;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneScanTextParams {
    /// Text to score. Empty text is accepted and returns zero matches.
    pub text: String,
    /// Minimum score to return (default 50, max 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<u32>,
    /// Persist matches as flag rows. Requires source_cf, source_key_hex, and
    /// source_field so the row links back to a physical Source of Truth.
    #[serde(default)]
    pub persist: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_field: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneScanStorageParams {
    /// Sources to scan: CF_OBSERVATIONS and/or CF_TIMELINE. Defaults to both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cfs: Option<Vec<String>>,
    /// Maximum source rows scanned this call (default 1000, max 10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_rows: Option<u32>,
    /// Maximum persisted flags this call (default 200, max 1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag_limit: Option<u32>,
    /// Minimum score to persist/return (default 50, max 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<u32>,
    /// Cursor returned by a previous call, formatted as source_cf:key_hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneFlagsParams {
    /// Optional source CF filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cf: Option<String>,
    /// Optional exact source key, hex-encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key_hex: Option<String>,
    /// Minimum score to return (default 0 for readback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<u32>,
    /// Maximum rows returned (default 100, max 1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Hex-encoded CF_KV key from a previous response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneTextMatch {
    /// Byte offset in the original source text.
    pub span_start: u32,
    /// Exclusive byte offset in the original source text.
    pub span_end: u32,
    pub span_text: String,
    pub span_text_sha256: String,
    pub score: u32,
    pub heuristics: Vec<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneFlagRecord {
    pub schema_version: u32,
    pub flag_id: String,
    pub detected_at: DateTime<Utc>,
    pub source_cf: String,
    /// Hex-encoded key in `source_cf`.
    pub source_key_hex: String,
    /// Field path within the decoded source row.
    pub source_field: String,
    pub source_text_sha256: String,
    pub span_start: u32,
    pub span_end: u32,
    pub span_text: String,
    pub span_text_sha256: String,
    pub score: u32,
    pub heuristics: Vec<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneStoredFlag {
    pub kv_key_hex: String,
    pub record: HygieneFlagRecord,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneScanTextResponse {
    pub matches: Vec<HygieneTextMatch>,
    pub flags_written: u64,
    pub persisted_flags: Vec<HygieneStoredFlag>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneScanStorageResponse {
    pub source_cfs: Vec<String>,
    pub scanned_rows: u64,
    pub invalid_rows: u64,
    pub flags_written: u64,
    pub persisted_flags: Vec<HygieneStoredFlag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub stopped_because: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneFlagsResponse {
    pub flags: Vec<HygieneStoredFlag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub scanned_rows: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneReportTimeRange {
    /// Inclusive lower bound on a flag's `detected_at`, unix nanoseconds.
    pub start_ns: u64,
    /// Exclusive upper bound on a flag's `detected_at`, unix nanoseconds.
    pub end_ns: u64,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneReportParams {
    /// Restrict to one source CF (`CF_TIMELINE`, `CF_OBSERVATIONS`,
    /// `CF_OCR_CACHE`). Omit to report flags across every source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cf: Option<String>,
    /// Restrict to one exact source row, hex-encoded. Requires `source_cf`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key_hex: Option<String>,
    /// Minimum flag score to include (default 0, max 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<u32>,
    /// Restrict to flags whose `detected_at` falls in `[start_ns, end_ns)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_range: Option<HygieneReportTimeRange>,
    /// Maximum flags returned (default 100, max 1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Hex-encoded `CF_KV` key cursor from a previous response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// One episode a flagged timeline row fed (time-window containment over
/// `CF_EPISODES`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneImpactedEpisode {
    pub episode_id: String,
    pub start_ts_ns: u64,
    pub end_ts_ns: u64,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// True when `hygiene/taint/v1/episode/<episode_id>` exists.
    pub tainted: bool,
    /// Exact taint ledger row for this episode, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<HygieneTaintRecord>,
}

/// One mined routine whose evidence references an episode a flagged row fed.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneImpactedRoutine {
    pub routine_id: String,
    pub schedule_label: String,
    pub granularity: String,
    pub support_days: u32,
    pub confidence: f64,
    /// Operator lifecycle from `CF_ROUTINE_STATE` when present. A flagged row
    /// feeding a `confirmed` routine is higher-stakes than one feeding an
    /// unreviewed `candidate` — this lets the consumer prioritize cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    /// Operator-assigned routine label from `CF_ROUTINE_STATE`, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Impacted episode ids that link this flag to this routine (the
    /// intersection of the flag's episodes with the routine's evidence).
    pub via_episode_ids: Vec<String>,
    /// True when `hygiene/taint/v1/routine/<routine_id>` exists.
    pub tainted: bool,
    /// Exact taint ledger row for this routine, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<HygieneTaintRecord>,
}

/// One profile-authoring candidate whose generated evidence/patch references
/// an impacted routine or episode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneImpactedAuthoringCandidate {
    pub candidate_id: String,
    pub profile_id: String,
    /// Review/install state from the candidate row: candidate/accepted/rejected.
    pub state: String,
    pub generated_at_ns: u64,
    pub updated_at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_at_ns: Option<u64>,
    /// Impacted routine ids referenced by the candidate JSON.
    pub via_routine_ids: Vec<String>,
    /// Impacted episode ids referenced by the candidate JSON.
    pub via_episode_ids: Vec<String>,
    /// True when `hygiene/taint/v1/authoring_candidate/<candidate_id>` exists.
    pub tainted: bool,
    /// Exact taint ledger row for this candidate, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<HygieneTaintRecord>,
}

/// One flagged row plus the derived state (episodes, routines, authoring
/// candidates) traceable to it.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneFlagImpact {
    pub flag: HygieneStoredFlag,
    /// Decoded source-row timestamp for `CF_TIMELINE` flags; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ts_ns: Option<u64>,
    pub derived_episodes: Vec<HygieneImpactedEpisode>,
    pub derived_routines: Vec<HygieneImpactedRoutine>,
    pub derived_authoring_candidates: Vec<HygieneImpactedAuthoringCandidate>,
    /// Honest, human-readable explanation of the derivation (including why it
    /// is empty).
    pub derivation_note: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneReportSummary {
    pub flags_total: u64,
    /// Flags that fed at least one episode (poisoned derived state).
    pub flags_with_downstream_impact: u64,
    pub impacted_episode_count: u64,
    pub impacted_routine_count: u64,
    /// Subset of impacted routines the operator has `confirmed`.
    pub impacted_confirmed_routine_count: u64,
    pub impacted_authoring_candidate_count: u64,
    /// Subset of impacted authoring candidates the operator has accepted.
    pub impacted_accepted_authoring_candidate_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneReportResponse {
    pub flags: Vec<HygieneFlagImpact>,
    pub summary: HygieneReportSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub scanned_flag_rows: u64,
    pub scanned_episode_rows: u64,
    pub scanned_routine_rows: u64,
    pub scanned_authoring_candidate_rows: u64,
}

#[must_use]
pub const fn hygiene_scan_text() -> M3ToolStub {
    M3ToolStub::new("hygiene_scan_text")
}

#[must_use]
pub const fn hygiene_scan_storage() -> M3ToolStub {
    M3ToolStub::new("hygiene_scan_storage")
}

#[must_use]
pub const fn hygiene_flags() -> M3ToolStub {
    M3ToolStub::new("hygiene_flags")
}

#[must_use]
pub fn required_permissions_scan_text(params: &HygieneScanTextParams) -> RequiredPermissions {
    if params.persist {
        required([Permission::ReadStorage, Permission::WriteStorage])
    } else {
        RequiredPermissions::new()
    }
}

#[must_use]
pub fn required_permissions_scan_storage(
    _params: &HygieneScanStorageParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_flags(_params: &HygieneFlagsParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_report(_params: &HygieneReportParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

/// Upper bound on records scanned in one Calyx-intelligence hygiene pass. Mirrors
/// `synapse_calyx::SYNAPSE_INTELLIGENCE_MAX_RECORDS`; the vault re-clamps.
const MAX_INTELLIGENCE_HYGIENE_RECORDS: u32 = 20_000;

fn clamp_intelligence_hygiene_records(requested: Option<u32>) -> usize {
    requested
        .unwrap_or(MAX_INTELLIGENCE_HYGIENE_RECORDS)
        .clamp(1, MAX_INTELLIGENCE_HYGIENE_RECORDS) as usize
}

/// Grounding-gap report request over one panel (domain) (#1670).
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGroundingGapParams {
    /// Panel/domain to report grounding coverage for.
    pub panel_version: u32,
    /// Optional cap on records scanned (defaults to the vault maximum).
    #[serde(default)]
    pub max_records: Option<u32>,
}

/// Per-anchor-kind grounded coverage row in the grounding-gap report.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneAnchorKindCoverage {
    pub anchor_kind: String,
    pub grounded_records: u64,
    pub coverage_fraction: f32,
}

/// Per-lens grounded coverage row in the grounding-gap report.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneSlotGroundingCoverage {
    pub slot: u32,
    /// Records where this lens produced a real measurement.
    pub records_present: u64,
    /// Records where this lens is stored but measured nothing — a sparse lane
    /// over a record with no text for it. Counted separately so coverage cannot
    /// report a text lane as fully measured on records that have no text
    /// (#1904); `calyx-search` excludes these same rows from BM25's `N`.
    pub records_empty_measurement: u64,
    /// Records where this lens *refused* the row and left `Absent{Error}`
    /// (#1924). A refusal is no longer allowed to abort the whole
    /// constellation, so it has to be countable here or the loss is invisible.
    /// Ordinary inapplicable absence is not counted: only an explicit refusal.
    pub records_slot_refused: u64,
    pub grounded_records: u64,
    pub ungrounded_records: u64,
    pub coverage_fraction: f32,
    /// True when this lens is under-anchored: results read from it are provisional.
    pub provisional: bool,
}

/// Grounding-gap report over one panel with the physical `Base` CF readback.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGroundingGapResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub records_scanned: u64,
    pub records_measured: u64,
    pub grounded_records: u64,
    pub ungrounded_records: u64,
    pub grounded_fraction: f32,
    pub coverage_floor: f32,
    /// The load-bearing control-doctrine marker: true when the domain is
    /// under-anchored, so any assay/oracle result over it must be tagged
    /// provisional (may advise, never control).
    pub provisional: bool,
    pub provisional_reason: Option<String>,
    /// **#1962.** The domain carries no anchor of any kind, as distinct from
    /// carrying some and falling under the floor. With no outcome axis every
    /// bits/sufficiency/synergy/kernel-groundedness result over the domain is
    /// undefined rather than provisional.
    pub no_outcome_axis: bool,
    pub distinct_anchor_kinds: u64,
    pub anchor_kind_coverage: Vec<HygieneAnchorKindCoverage>,
    pub slot_coverage: Vec<HygieneSlotGroundingCoverage>,
    pub largest_ungrounded_slots: Vec<HygieneSlotGroundingCoverage>,
    pub base_cf_rows: u64,
}

#[must_use]
pub fn required_permissions_grounding_gap(
    _params: &HygieneGroundingGapParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

fn map_slot_grounding(
    coverage: synapse_calyx::SynapseCalyxSlotGroundingCoverage,
) -> HygieneSlotGroundingCoverage {
    HygieneSlotGroundingCoverage {
        slot: u32::from(coverage.slot),
        records_present: coverage.records_present as u64,
        records_empty_measurement: coverage.records_empty_measurement as u64,
        records_slot_refused: coverage.records_slot_refused as u64,
        grounded_records: coverage.grounded_records as u64,
        ungrounded_records: coverage.ungrounded_records as u64,
        coverage_fraction: coverage.coverage_fraction,
        provisional: coverage.provisional,
    }
}

/// Reports grounding gaps for one panel: per-anchor-kind and per-lens grounded
/// coverage, the largest ungrounded regions, and the domain's provisional
/// verdict. Read-only physical `Base` CF readback.
///
/// # Errors
///
/// Returns a structured error when the `Base` CF cannot be scanned or a
/// constellation row fails to decode.
pub fn run_grounding_gap(
    db: &Db,
    params: &HygieneGroundingGapParams,
) -> Result<HygieneGroundingGapResponse, ErrorData> {
    let max_records = clamp_intelligence_hygiene_records(params.max_records);
    let report = db
        .grounding_gap_intelligence(params.panel_version, max_records)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(HygieneGroundingGapResponse {
        source_of_truth: "Calyx Base CF anchors + hydrated per-slot CF lens vectors",
        panel_version: report.panel_version,
        records_scanned: report.records_scanned as u64,
        records_measured: report.records_measured as u64,
        grounded_records: report.grounded_records as u64,
        ungrounded_records: report.ungrounded_records as u64,
        grounded_fraction: report.grounded_fraction,
        coverage_floor: report.coverage_floor,
        provisional: report.provisional,
        provisional_reason: report.provisional_reason,
        no_outcome_axis: report.no_outcome_axis,
        distinct_anchor_kinds: report.distinct_anchor_kinds as u64,
        anchor_kind_coverage: report
            .anchor_kind_coverage
            .into_iter()
            .map(|coverage| HygieneAnchorKindCoverage {
                anchor_kind: coverage.anchor_kind,
                grounded_records: coverage.grounded_records as u64,
                coverage_fraction: coverage.coverage_fraction,
            })
            .collect(),
        slot_coverage: report
            .slot_coverage
            .into_iter()
            .map(map_slot_grounding)
            .collect(),
        largest_ungrounded_slots: report
            .largest_ungrounded_slots
            .into_iter()
            .map(map_slot_grounding)
            .collect(),
        base_cf_rows: report.base_cf_rows as u64,
    })
}

// ---------------------------------------------------------------------------
// Blind-spot and MMD drift detection (#1674)
// ---------------------------------------------------------------------------

/// Blind-spot scan request over one panel (#1674).
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneBlindSpotParams {
    pub panel_version: u32,
    #[serde(default)]
    pub max_records: Option<u32>,
    /// Optional cap on alerts returned (defaults to the vault maximum).
    #[serde(default)]
    pub max_alerts: Option<u32>,
}

/// One flagged cross-lens blind-spot anomaly in the hygiene report.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneBlindSpotAlert {
    pub cx_id: String,
    pub slot_a: u32,
    pub slot_b: u32,
    pub lens_a_similarity: f32,
    pub lens_b_neighbor_mean: f32,
    pub delta: f32,
    pub severity: String,
    pub calibration_sample_count: u64,
    pub calibration_alpha: f32,
    pub calibration_p_value: f32,
    pub calibration_percentile: f32,
    pub threshold_delta: f32,
    /// Distinct delta values behind this pair's calibration (#1961 ask 2).
    pub calibration_distinct_deltas: u64,
    /// Whether that calibration resolves `alpha`. When false, the certificate
    /// proves only that the observation sits at the top of a nearly constant
    /// variable and the severity is capped at `low`.
    pub calibration_resolves_alpha: bool,
    pub lens_b_observed_min: f32,
    pub lens_b_observed_max: f32,
    /// Where `lens_b_neighbor_mean` sits inside lens B's own reached range:
    /// `0.0` at B's maximum, `1.0` at B's minimum (#1961 ask 3).
    pub lens_b_dissent_fraction: f32,
}

/// One slot-pair direction refused rather than evaluated, with the measured
/// evidence for the refusal (#1961 ask 1).
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneBlindSpotPairDiagnostic {
    pub slot_a: u32,
    pub slot_b: u32,
    pub code: String,
    pub detail: String,
    pub records: u64,
    pub lens_a_distinct_values: u64,
    pub lens_a_modal_value: f32,
    pub lens_a_modal_share: f32,
    pub lens_a_observed_min: f32,
    pub lens_a_observed_max: f32,
}

/// Blind-spot scan report over one panel.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneBlindSpotResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub records_scanned: u64,
    pub records_measured: u64,
    pub n_lenses: u64,
    /// Ordered `(A, B)` directions evaluated; both directions of a pair are
    /// candidates because the rule is directional.
    pub slot_pairs_evaluated: u64,
    pub slot_pairs_uncalibrated: u64,
    /// Directions refused because lens A's confidence is definitional.
    pub slot_pairs_nondiscriminative: u64,
    pub nondiscriminative_pairs: Vec<HygieneBlindSpotPairDiagnostic>,
    /// Distinct `(lens_a_similarity, lens_b_neighbor_mean)` tuples across every
    /// emitted alert — the alert set's own discriminative power (#1961 ask 2).
    pub alert_distinct_signatures: u64,
    pub alert_distinct_deltas: u64,
    pub alerts_total: u64,
    pub alerts: Vec<HygieneBlindSpotAlert>,
}

/// MMD lens-drift scan request over one panel (#1674).
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneDriftParams {
    pub panel_version: u32,
    #[serde(default)]
    pub max_records: Option<u32>,
    /// Fraction of the most-recent records forming the recent window (0.05..0.95).
    #[serde(default)]
    pub recent_fraction: Option<f32>,
    /// MMD permutation count for the null distribution.
    #[serde(default)]
    pub permutations: Option<u32>,
}

/// One lens's MMD reference-vs-recent drift measurement in the hygiene report.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneLensDrift {
    pub slot: u32,
    pub dimension: u64,
    pub reference_n: u64,
    pub recent_n: u64,
    pub mmd2: f64,
    pub p_value: f64,
    pub bandwidth: f64,
    pub significant: bool,
    pub persisted: bool,
}

/// One fired drift trigger, decoded from its committed Reactive CF row.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygienePersistedDriftFinding {
    pub panel_version: u32,
    pub slot: u32,
    pub dimension: u64,
    pub reference_n: u64,
    pub recent_n: u64,
    pub mmd2: f64,
    pub p_value: f64,
    pub bandwidth: f64,
    pub significant: bool,
    pub observed_seq: u64,
}

/// MMD lens-drift scan report over one panel with the `Reactive` CF readback.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneDriftResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub records_scanned: u64,
    pub records_measured: u64,
    pub recent_fraction: f32,
    pub permutations: u64,
    pub lenses_evaluated: u64,
    pub lenses_insufficient: u64,
    pub drifted_lenses: u64,
    pub lens_drift: Vec<HygieneLensDrift>,
    pub reactive_cf_rows_after: u64,
    pub drift_rows_persisted: u64,
    pub persisted_findings: Vec<HygienePersistedDriftFinding>,
    pub notifications_matched: u64,
    pub notifications_queued: u64,
    pub notifications_dropped: u64,
}

/// Kernel-health request for one persisted domain kernel (#1675).
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneKernelParams {
    pub panel_version: u32,
    /// Dense semantic-lens slot the kernel was selected on.
    pub content_slot: u32,
    /// Grounded outcome domain (anchor-kind label). Omit for the panel-default
    /// kernel written by a single `storage.intelligence kernel` build.
    #[serde(default)]
    pub anchor_kind: Option<String>,
}

/// Cold per-domain kernel rebuild request over one panel (#1675).
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneKernelRebuildParams {
    pub panel_version: u32,
    pub content_slot: u32,
    #[serde(default)]
    pub max_records: Option<u32>,
    /// Kernel-only recall gate ratio; an ungrounded kernel is refused, not served.
    #[serde(default)]
    pub min_recall_ratio: Option<f32>,
    #[serde(default)]
    pub max_domains: Option<u32>,
}

/// Kernel health assembled from the persisted artifact - never recomputed.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneKernelResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub content_slot: u32,
    pub anchor_kind: Option<String>,
    pub kernel_id: String,
    pub size: u64,
    pub kernel_graph_size: u64,
    pub recall_raw: f32,
    pub recall_ratio: f32,
    pub min_recall_ratio: f32,
    pub n_queries_tested: u64,
    pub recall_pass_mode: String,
    pub grounded_fraction: f32,
    pub unanchored_count: u64,
    pub approx_factor: f64,
    pub tau_star_estimate: u64,
    pub tau_star_exact: bool,
    pub built_at_millis: u64,
    pub corpus_shard_hash: String,
    pub trust: String,
    pub warnings: Vec<String>,
    pub artifact_bytes: u64,
    pub kernel_cf_rows: u64,
}

/// One domain's outcome in a kernel rebuild sweep.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneKernelDomainOutcome {
    pub anchor_kind: String,
    pub anchored_records: u64,
    pub built: bool,
    pub kernel_id: Option<String>,
    pub members: u64,
    pub corpus_size: u64,
    pub recall_kernel_only: f32,
    pub recall_ratio: f32,
    pub reached_anchor: f32,
    pub refusal_code: Option<String>,
    pub refusal: Option<String>,
}

/// Cold per-domain kernel rebuild report with the `Kernel` CF readback.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneKernelRebuildResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub content_slot: u32,
    pub domains_discovered: u64,
    pub domains_attempted: u64,
    pub domains_built: u64,
    pub domains_refused: u64,
    pub all_domains_grounded: bool,
    pub min_recall_ratio: f32,
    pub domains: Vec<HygieneKernelDomainOutcome>,
    pub artifacts_persisted: u64,
    pub kernel_cf_rows_after: u64,
}

/// Operator-asserted aspect of a guarded slot (#1677). Never inferred: it sets
/// the maximum permitted target FAR and is persisted as calibration provenance.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HygieneGuardAspect {
    Identity,
    Stylistic,
    Content,
}

/// One slot to calibrate with its asserted aspect.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGuardSlotSpec {
    pub slot: u32,
    pub aspect: HygieneGuardAspect,
}

/// Ward guard calibration request over one panel (#1677).
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGuardCalibrateParams {
    pub panel_version: u32,
    /// Dense active panel slots to guard, each with its asserted aspect.
    pub slots: Vec<HygieneGuardSlotSpec>,
    #[serde(default)]
    pub domain: Option<String>,
    /// Conformal miscoverage budget; the tau bounds the true FAR at `target_far`
    /// with confidence `1 - alpha`.
    #[serde(default)]
    pub alpha: Option<f32>,
    #[serde(default)]
    pub target_far: Option<f32>,
    #[serde(default)]
    pub max_records: Option<u32>,
    /// Dry run when false: the calibration is computed but the Guard CF is not
    /// written.
    #[serde(default)]
    pub persist: Option<bool>,
    /// OOD disposition persisted in the calibrated profile. Omitted keeps the
    /// existing fail-closed `reject_closed` policy.
    #[serde(default)]
    pub novelty_action: Option<HygieneGuardNoveltyAction>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HygieneGuardNoveltyAction {
    RejectClosed,
    NewRegion,
    Quarantine,
}

/// One slot's calibration evidence.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGuardSlotCalibration {
    pub slot: u32,
    pub aspect: String,
    pub target_far: f32,
    pub tau: f32,
    pub bad_accepts: u64,
    pub achieved_far: f64,
    pub achieved_frr: f64,
    pub good_scores: u64,
    pub bad_scores: u64,
    pub clopper_pearson_tail: f64,
    pub certifiable_min_bad_scores: u64,
}

/// Ward guard calibration report with the `Guard` CF readback.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGuardCalibrateResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub domain: String,
    pub guard_id: String,
    pub alpha: f32,
    pub novelty_action: String,
    pub records_scanned: u64,
    pub adjudicated_good: u64,
    pub adjudicated_bad: u64,
    pub unadjudicated: u64,
    pub conflicting: u64,
    pub estimator: String,
    pub slots: Vec<HygieneGuardSlotCalibration>,
    pub persisted: bool,
    pub guard_cf_profile_bytes: u64,
    pub guard_cf_rows_after: u64,
    pub readback_calibrated: bool,
}

/// Ward guard verification request for one record (#1677).
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGuardVerifyParams {
    pub panel_version: u32,
    pub query_cx_id: String,
    /// High-stakes verification refuses provisional/partially-calibrated profiles.
    #[serde(default)]
    pub high_stakes: Option<bool>,
    #[serde(default)]
    pub max_records: Option<u32>,
}

/// One slot's verdict inside a guard verification.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGuardSlotVerdict {
    pub slot: u32,
    pub cos: f32,
    pub tau: f32,
    pub pass: bool,
    pub matched_cx_id: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygienePersistedNoveltyFinding {
    pub panel_version: u32,
    pub query_cx_id: String,
    pub guard_id: String,
    pub action: String,
    pub failing_slots: Vec<u32>,
    pub ledger_seq: u64,
    pub ledger_hash: String,
}

/// A Ward `GuardVerdict` produced against the persisted profile.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneGuardVerifyResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub query_cx_id: String,
    pub guard_id: String,
    pub domain: String,
    pub high_stakes: bool,
    pub overall_pass: bool,
    pub provisional: bool,
    pub policy: String,
    pub required_slots: Vec<u32>,
    pub per_slot: Vec<HygieneGuardSlotVerdict>,
    pub failing_slots: Vec<u32>,
    pub action: Option<String>,
    pub calibration_far: Option<f32>,
    pub calibration_frr: Option<f32>,
    pub calibration_confidence: Option<f32>,
    pub trusted_exemplars: u64,
    pub ledger_seq: u64,
    pub ledger_hash: String,
    pub persisted_novelty: Option<HygienePersistedNoveltyFinding>,
    pub notifications_matched: u64,
    pub notifications_queued: u64,
    pub notifications_dropped: u64,
}

#[must_use]
pub fn required_permissions_kernel(_params: &HygieneKernelParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_kernel_rebuild(
    _params: &HygieneKernelRebuildParams,
) -> RequiredPermissions {
    // The rebuild persists Kernel artifacts and per-domain index rows.
    required([Permission::ReadStorage, Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_guard_calibrate(
    params: &HygieneGuardCalibrateParams,
) -> RequiredPermissions {
    if params.persist.unwrap_or(true) {
        required([Permission::ReadStorage, Permission::WriteStorage])
    } else {
        required([Permission::ReadStorage])
    }
}

#[must_use]
pub fn required_permissions_guard_verify(
    _params: &HygieneGuardVerifyParams,
) -> RequiredPermissions {
    // Ward verification always appends its security verdict to Ledger and a
    // configured novelty disposition also commits a Reactive outbox row.
    required([Permission::ReadStorage, Permission::WriteStorage])
}

/// Reports the health of one persisted domain kernel by reading its Kernel
/// artifact from the vault. Read-only; never re-measures recall.
///
/// # Errors
///
/// Returns a structured error when no kernel is persisted for the domain or the
/// artifact is missing/stale/undecodable.
pub fn run_kernel(
    db: &Db,
    params: &HygieneKernelParams,
) -> Result<HygieneKernelResponse, ErrorData> {
    let report = db
        .domain_kernel_health_intelligence(
            params.panel_version,
            clamp_slot(params.content_slot),
            params.anchor_kind.as_deref(),
        )
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(HygieneKernelResponse {
        source_of_truth: "Calyx Kernel CF persisted kernel artifact",
        panel_version: report.panel_version,
        content_slot: u32::from(report.content_slot),
        anchor_kind: report.anchor_kind,
        kernel_id: report.kernel_id,
        size: report.size as u64,
        kernel_graph_size: report.kernel_graph_size as u64,
        recall_raw: report.recall_raw,
        recall_ratio: report.recall_ratio,
        min_recall_ratio: report.min_recall_ratio,
        n_queries_tested: report.n_queries_tested as u64,
        recall_pass_mode: report.recall_pass_mode,
        grounded_fraction: report.grounded_fraction,
        unanchored_count: report.unanchored_count as u64,
        approx_factor: report.approx_factor,
        tau_star_estimate: report.tau_star_estimate as u64,
        tau_star_exact: report.tau_star_exact,
        built_at_millis: report.built_at_millis,
        corpus_shard_hash: report.corpus_shard_hash,
        trust: report.trust,
        warnings: report.warnings,
        artifact_bytes: report.artifact_bytes as u64,
        kernel_cf_rows: report.kernel_cf_rows as u64,
    })
}

/// Runs the COLD per-domain kernel rebuild sweep and reads the `Kernel` CF back.
///
/// # Errors
///
/// Returns a structured error when the panel has no grounded outcome domain,
/// when every domain refused the recall gate, or when the CF write/readback
/// fails.
pub fn run_kernel_rebuild(
    db: &Db,
    params: &HygieneKernelRebuildParams,
) -> Result<HygieneKernelRebuildResponse, ErrorData> {
    let mut spec = synapse_calyx::SynapseCalyxKernelRebuildParams::new(
        params.panel_version,
        clamp_slot(params.content_slot),
    );
    spec.max_records = clamp_intelligence_hygiene_records(params.max_records);
    if let Some(min_recall_ratio) = params.min_recall_ratio {
        spec.min_recall_ratio = min_recall_ratio;
    }
    if let Some(max_domains) = params.max_domains {
        spec.max_domains = max_domains.max(1) as usize;
    }
    let report = db
        .rebuild_domain_kernels_intelligence(&spec)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(HygieneKernelRebuildResponse {
        source_of_truth: "Calyx Kernel CF persisted kernel artifacts",
        panel_version: report.panel_version,
        content_slot: u32::from(report.content_slot),
        domains_discovered: report.domains_discovered as u64,
        domains_attempted: report.domains_attempted as u64,
        domains_built: report.domains_built as u64,
        domains_refused: report.domains_refused as u64,
        all_domains_grounded: report.all_domains_grounded,
        min_recall_ratio: report.min_recall_ratio,
        domains: report
            .domains
            .into_iter()
            .map(|domain| HygieneKernelDomainOutcome {
                anchor_kind: domain.anchor_kind,
                anchored_records: domain.anchored_records as u64,
                built: domain.built,
                kernel_id: domain.kernel_id,
                members: domain.members as u64,
                corpus_size: domain.corpus_size as u64,
                recall_kernel_only: domain.recall_kernel_only,
                recall_ratio: domain.recall_ratio,
                reached_anchor: domain.reached_anchor,
                refusal_code: domain.refusal_code,
                refusal: domain.refusal,
            })
            .collect(),
        artifacts_persisted: report.artifacts_persisted as u64,
        kernel_cf_rows_after: report.kernel_cf_rows_after as u64,
    })
}

/// Calibrates the Ward guard profile from the vault's adjudicated corpus and
/// persists it to the `Guard` CF the guarded-search consumer reads.
///
/// # Errors
///
/// Fails closed when the adjudicated bad-case corpus is absent, below ward's
/// minimum, or too small to certify the requested target FAR - never with a
/// fabricated corpus.
pub fn run_guard_calibrate(
    db: &Db,
    params: &HygieneGuardCalibrateParams,
) -> Result<HygieneGuardCalibrateResponse, ErrorData> {
    let slots = params
        .slots
        .iter()
        .map(|spec| synapse_calyx::SynapseCalyxGuardSlotSpec {
            slot: clamp_slot(spec.slot),
            aspect: match spec.aspect {
                HygieneGuardAspect::Identity => synapse_calyx::SynapseCalyxGuardAspect::Identity,
                HygieneGuardAspect::Stylistic => synapse_calyx::SynapseCalyxGuardAspect::Stylistic,
                HygieneGuardAspect::Content => synapse_calyx::SynapseCalyxGuardAspect::Content,
            },
        })
        .collect();
    let mut spec =
        synapse_calyx::SynapseCalyxGuardCalibrateParams::new(params.panel_version, slots);
    if let Some(domain) = &params.domain {
        spec.domain.clone_from(domain);
    }
    if let Some(alpha) = params.alpha {
        spec.alpha = alpha;
    }
    spec.target_far = params.target_far;
    spec.max_records = clamp_intelligence_hygiene_records(params.max_records);
    spec.persist = params.persist.unwrap_or(true);
    spec.novelty_action = match params
        .novelty_action
        .unwrap_or(HygieneGuardNoveltyAction::RejectClosed)
    {
        HygieneGuardNoveltyAction::RejectClosed => calyx_ward::NoveltyAction::RejectClosed,
        HygieneGuardNoveltyAction::NewRegion => calyx_ward::NoveltyAction::NewRegion,
        HygieneGuardNoveltyAction::Quarantine => calyx_ward::NoveltyAction::Quarantine,
    };
    let report = db
        .guard_calibrate_intelligence(&spec)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(HygieneGuardCalibrateResponse {
        source_of_truth: "Calyx Guard CF calibrated profile row",
        panel_version: report.panel_version,
        domain: report.domain,
        guard_id: report.guard_id,
        alpha: report.alpha,
        novelty_action: report.novelty_action,
        records_scanned: report.records_scanned as u64,
        adjudicated_good: report.adjudicated_good as u64,
        adjudicated_bad: report.adjudicated_bad as u64,
        unadjudicated: report.unadjudicated as u64,
        conflicting: report.conflicting as u64,
        estimator: report.estimator,
        slots: report
            .slots
            .into_iter()
            .map(|slot| HygieneGuardSlotCalibration {
                slot: u32::from(slot.slot),
                aspect: slot.aspect,
                target_far: slot.target_far,
                tau: slot.tau,
                bad_accepts: slot.bad_accepts as u64,
                achieved_far: slot.achieved_far,
                achieved_frr: slot.achieved_frr,
                good_scores: slot.good_scores as u64,
                bad_scores: slot.bad_scores as u64,
                clopper_pearson_tail: slot.clopper_pearson_tail,
                certifiable_min_bad_scores: slot.certifiable_min_bad_scores as u64,
            })
            .collect(),
        persisted: report.persisted,
        guard_cf_profile_bytes: report.guard_cf_profile_bytes as u64,
        guard_cf_rows_after: report.guard_cf_rows_after as u64,
        readback_calibrated: report.readback_calibrated,
    })
}

/// Verifies one record against the persisted Ward guard profile.
///
/// # Errors
///
/// Fails closed with `CALYX_GUARD_PROVISIONAL` when no calibrated profile is
/// persisted, and with a named error when the record or a trusted exemplar is
/// missing.
pub fn run_guard_verify(
    db: &Db,
    params: &HygieneGuardVerifyParams,
) -> Result<HygieneGuardVerifyResponse, ErrorData> {
    let spec = synapse_calyx::SynapseCalyxGuardVerifyParams {
        panel_version: params.panel_version,
        query_cx_id: params.query_cx_id.clone(),
        high_stakes: params.high_stakes.unwrap_or(false),
        max_records: clamp_intelligence_hygiene_records(params.max_records),
    };
    let report = db
        .guard_verify_intelligence(&spec)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let persisted_novelty = match report.action.as_deref() {
        Some(action @ ("new_region" | "quarantine")) => Some(
            db.persist_novelty_finding(&synapse_calyx::SynapseCalyxPersistedNoveltyFinding {
                panel_version: report.panel_version,
                query_cx_id: report.query_cx_id.clone(),
                guard_id: report.guard_id.clone(),
                action: action.to_owned(),
                failing_slots: report.failing_slots.clone(),
                ledger_seq: report.ledger_seq,
                ledger_hash: report.ledger_hash.clone(),
            })
            .map_err(|error| mcp_error(error.code(), error.to_string()))?,
        ),
        _ => None,
    };
    Ok(HygieneGuardVerifyResponse {
        source_of_truth: "Calyx Guard profile + Ledger verdict + optional Reactive novelty row",
        panel_version: report.panel_version,
        query_cx_id: report.query_cx_id,
        guard_id: report.guard_id,
        domain: report.domain,
        high_stakes: report.high_stakes,
        overall_pass: report.overall_pass,
        provisional: report.provisional,
        policy: report.policy,
        required_slots: report.required_slots.into_iter().map(u32::from).collect(),
        per_slot: report
            .per_slot
            .into_iter()
            .map(|slot| HygieneGuardSlotVerdict {
                slot: u32::from(slot.slot),
                cos: slot.cos,
                tau: slot.tau,
                pass: slot.pass,
                matched_cx_id: slot.matched_cx_id,
            })
            .collect(),
        failing_slots: report.failing_slots.into_iter().map(u32::from).collect(),
        action: report.action,
        calibration_far: report.calibration_far,
        calibration_frr: report.calibration_frr,
        calibration_confidence: report.calibration_confidence,
        trusted_exemplars: report.trusted_exemplars as u64,
        ledger_seq: report.ledger_seq,
        ledger_hash: report.ledger_hash,
        persisted_novelty: persisted_novelty.map(|finding| HygienePersistedNoveltyFinding {
            panel_version: finding.panel_version,
            query_cx_id: finding.query_cx_id,
            guard_id: finding.guard_id,
            action: finding.action,
            failing_slots: finding.failing_slots.into_iter().map(u32::from).collect(),
            ledger_seq: finding.ledger_seq,
            ledger_hash: finding.ledger_hash,
        }),
        notifications_matched: 0,
        notifications_queued: 0,
        notifications_dropped: 0,
    })
}

/// Narrows a wire slot id into the physical `u16` panel slot space. A value
/// above the ceiling is pinned to `u16::MAX`, which no published panel defines,
/// so the request fails closed downstream instead of silently guarding slot 0.
fn clamp_slot(slot: u32) -> u16 {
    u16::try_from(slot).unwrap_or(u16::MAX)
}

#[must_use]
pub fn required_permissions_blind_spot(_params: &HygieneBlindSpotParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_drift(_params: &HygieneDriftParams) -> RequiredPermissions {
    // Drift persists findings to the native Reactive CF for #1677/#1681.
    required([Permission::ReadStorage, Permission::WriteStorage])
}

/// Scans a panel for cross-lens blind spots and lists the flagged records with
/// the disagreeing lens pair and calibration evidence. Read-only.
///
/// # Errors
///
/// Returns a structured error when the corpus cannot be read, the math backend
/// is unavailable, or the calibrated detector fails closed.
pub fn run_blind_spot(
    db: &Db,
    params: &HygieneBlindSpotParams,
) -> Result<HygieneBlindSpotResponse, ErrorData> {
    let mut spec = synapse_calyx::SynapseCalyxBlindSpotParams::new(params.panel_version);
    spec.max_records = clamp_intelligence_hygiene_records(params.max_records);
    if let Some(max_alerts) = params.max_alerts {
        spec.max_alerts = max_alerts.max(1) as usize;
    }
    let report = db
        .blind_spot_intelligence(&spec)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(HygieneBlindSpotResponse {
        source_of_truth: "Calyx per-slot CF lens vectors (hydrated; the Base CF row carries only slot ids and hashes)",
        panel_version: report.panel_version,
        records_scanned: report.records_scanned as u64,
        records_measured: report.records_measured as u64,
        n_lenses: report.n_lenses as u64,
        slot_pairs_evaluated: report.slot_pairs_evaluated as u64,
        slot_pairs_uncalibrated: report.slot_pairs_uncalibrated as u64,
        slot_pairs_nondiscriminative: report.slot_pairs_nondiscriminative as u64,
        nondiscriminative_pairs: report
            .nondiscriminative_pairs
            .into_iter()
            .map(|pair| HygieneBlindSpotPairDiagnostic {
                slot_a: u32::from(pair.slot_a),
                slot_b: u32::from(pair.slot_b),
                code: pair.code,
                detail: pair.detail,
                records: pair.records as u64,
                lens_a_distinct_values: pair.lens_a_distinct_values as u64,
                lens_a_modal_value: pair.lens_a_modal_value,
                lens_a_modal_share: pair.lens_a_modal_share,
                lens_a_observed_min: pair.lens_a_observed_min,
                lens_a_observed_max: pair.lens_a_observed_max,
            })
            .collect(),
        alert_distinct_signatures: report.alert_distinct_signatures as u64,
        alert_distinct_deltas: report.alert_distinct_deltas as u64,
        alerts_total: report.alerts_total as u64,
        alerts: report
            .alerts
            .into_iter()
            .map(|alert| HygieneBlindSpotAlert {
                cx_id: alert.cx_id,
                slot_a: u32::from(alert.slot_a),
                slot_b: u32::from(alert.slot_b),
                lens_a_similarity: alert.lens_a_similarity,
                lens_b_neighbor_mean: alert.lens_b_neighbor_mean,
                delta: alert.delta,
                severity: alert.severity,
                calibration_sample_count: alert.calibration_sample_count as u64,
                calibration_alpha: alert.calibration_alpha,
                calibration_p_value: alert.calibration_p_value,
                calibration_percentile: alert.calibration_percentile,
                threshold_delta: alert.threshold_delta,
                calibration_distinct_deltas: alert.calibration_distinct_deltas as u64,
                calibration_resolves_alpha: alert.calibration_resolves_alpha,
                lens_b_observed_min: alert.lens_b_observed_min,
                lens_b_observed_max: alert.lens_b_observed_max,
                lens_b_dissent_fraction: alert.lens_b_dissent_fraction,
            })
            .collect(),
    })
}

/// Measures per-lens MMD distribution drift between a reference and a recent
/// window, persisting each finding to the native `Reactive` CF for downstream
/// consumers, then reads the CF back.
///
/// # Errors
///
/// Returns a structured error when the corpus cannot be read, the MMD estimator
/// hard-fails, or the `Reactive` CF write/readback fails.
pub fn run_drift(db: &Db, params: &HygieneDriftParams) -> Result<HygieneDriftResponse, ErrorData> {
    let mut spec = synapse_calyx::SynapseCalyxPanelDriftParams::new(params.panel_version);
    spec.max_records = clamp_intelligence_hygiene_records(params.max_records);
    if let Some(recent_fraction) = params.recent_fraction {
        spec.recent_fraction = recent_fraction;
    }
    if let Some(permutations) = params.permutations {
        spec.permutations = permutations.max(1) as usize;
    }
    let report = db
        .panel_drift_intelligence(&spec)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(HygieneDriftResponse {
        source_of_truth: "Calyx Reactive CF drift findings",
        panel_version: report.panel_version,
        records_scanned: report.records_scanned as u64,
        records_measured: report.records_measured as u64,
        recent_fraction: report.recent_fraction,
        permutations: report.permutations as u64,
        lenses_evaluated: report.lenses_evaluated as u64,
        lenses_insufficient: report.lenses_insufficient as u64,
        drifted_lenses: report.drifted_lenses as u64,
        lens_drift: report
            .lens_drift
            .into_iter()
            .map(|drift| HygieneLensDrift {
                slot: u32::from(drift.slot),
                dimension: drift.dimension as u64,
                reference_n: drift.reference_n as u64,
                recent_n: drift.recent_n as u64,
                mmd2: drift.mmd2,
                p_value: drift.p_value,
                bandwidth: drift.bandwidth,
                significant: drift.significant,
                persisted: drift.persisted,
            })
            .collect(),
        reactive_cf_rows_after: report.reactive_cf_rows_after as u64,
        drift_rows_persisted: report.drift_rows_persisted as u64,
        persisted_findings: report
            .persisted_findings
            .into_iter()
            .map(|finding| HygienePersistedDriftFinding {
                panel_version: finding.panel_version,
                slot: u32::from(finding.slot),
                dimension: finding.dimension as u64,
                reference_n: finding.reference_n as u64,
                recent_n: finding.recent_n as u64,
                mmd2: finding.mmd2,
                p_value: finding.p_value,
                bandwidth: finding.bandwidth,
                significant: finding.significant,
                observed_seq: finding.observed_seq,
            })
            .collect(),
        notifications_matched: 0,
        notifications_queued: 0,
        notifications_dropped: 0,
    })
}

pub fn scan_text_tool(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &HygieneScanTextParams,
) -> Result<HygieneScanTextResponse, ErrorData> {
    let started = Instant::now();
    let min_score = validate_min_score(params.min_score, DEFAULT_MIN_SCORE, "hygiene_scan_text")?;
    if params.text.len() > MAX_SCAN_TEXT_BYTES {
        return Err(invalid(format!(
            "hygiene_scan_text text must be <= {MAX_SCAN_TEXT_BYTES} bytes"
        )));
    }
    let matches = scan_text(&params.text, min_score);
    let (flags_written, persisted_flags) = if params.persist {
        let source_cf = params
            .source_cf
            .as_deref()
            .ok_or_else(|| invalid("hygiene_scan_text persist=true requires source_cf"))?;
        let source_cf = normalize_source_cf(source_cf, true)?;
        let source_key_hex = params
            .source_key_hex
            .as_deref()
            .ok_or_else(|| invalid("hygiene_scan_text persist=true requires source_key_hex"))?;
        validate_hex_text(source_key_hex, "source_key_hex")?;
        let source_key = hex_decode(source_key_hex)
            .ok_or_else(|| invalid("source_key_hex must be non-empty even-length hex"))?;
        let source_field = params
            .source_field
            .as_deref()
            .map(str::trim)
            .filter(|field| !field.is_empty())
            .ok_or_else(|| invalid("hygiene_scan_text persist=true requires source_field"))?;
        let records = records_for_matches(
            &source_cf,
            &source_key_hex.to_ascii_lowercase(),
            source_field,
            &params.text,
            &matches,
        );
        let runtime = lock_runtime(runtime)?;
        ensure_source_row_exists(&runtime, &source_cf, &source_key)?;
        write_flag_records(&runtime, records)?
    } else {
        (0, Vec::new())
    };
    Ok(HygieneScanTextResponse {
        matches,
        flags_written,
        persisted_flags,
        elapsed_ms: elapsed_ms(started),
    })
}

pub fn scan_storage(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &HygieneScanStorageParams,
) -> Result<HygieneScanStorageResponse, ErrorData> {
    let started = Instant::now();
    let source_cfs = validate_source_cfs(params.source_cfs.as_deref())?;
    let limit_rows = validate_limit(
        params.limit_rows.unwrap_or(DEFAULT_STORAGE_ROW_LIMIT),
        1,
        MAX_STORAGE_ROW_LIMIT,
        "hygiene_scan_storage limit_rows",
    )?;
    let flag_limit = validate_limit(
        params.flag_limit.unwrap_or(DEFAULT_FLAG_LIMIT),
        1,
        MAX_FLAG_LIMIT,
        "hygiene_scan_storage flag_limit",
    )?;
    let min_score =
        validate_min_score(params.min_score, DEFAULT_MIN_SCORE, "hygiene_scan_storage")?;
    let cursor = parse_storage_cursor(params.cursor.as_deref())?;
    let runtime = lock_runtime(runtime)?;

    let mut scanned_rows = 0_u64;
    let mut invalid_rows = 0_u64;
    let mut persisted_flags = Vec::new();
    let mut stopped_because = "end_of_sources".to_owned();
    let mut next_cursor = None;
    let mut start_seen = cursor.is_none();

    'sources: for source_cf in &source_cfs {
        let mut next_start = match cursor.as_ref() {
            Some(cursor) if cursor.source_cf == *source_cf => {
                start_seen = true;
                key_after(&cursor.key)
            }
            Some(_) if !start_seen => continue,
            _ => Vec::new(),
        };

        loop {
            if scanned_rows >= u64::from(limit_rows) {
                stopped_because = "row_limit_reached".to_owned();
                break 'sources;
            }
            if persisted_flags.len() >= flag_limit as usize {
                stopped_because = "flag_limit_reached".to_owned();
                break 'sources;
            }
            let remaining_rows =
                usize::try_from(u64::from(limit_rows) - scanned_rows).unwrap_or(usize::MAX);
            let chunk_rows = STORAGE_SCAN_CHUNK_ROWS.min(remaining_rows);
            let (rows, more) = runtime
                .storage_cf_rows_from(source_cf, &next_start, chunk_rows)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            if rows.is_empty() {
                break;
            }
            let mut last_key = Vec::new();
            for (key, value) in rows {
                scanned_rows += 1;
                last_key = key.clone();
                let candidates = match source_cf.as_str() {
                    SOURCE_CF_OBSERVATIONS => match decode_json::<StoredObservation>(&value) {
                        Ok(row) => observation_text_candidates(&row),
                        Err(error) => {
                            invalid_rows += 1;
                            tracing::warn!(
                                code = "HYGIENE_OBSERVATION_ROW_DECODE_FAILED",
                                key_hex = %hex_encode(&key),
                                %error,
                                "hygiene_scan_storage skipped undecodable CF_OBSERVATIONS row"
                            );
                            Vec::new()
                        }
                    },
                    SOURCE_CF_TIMELINE => match decode_json::<TimelineRecord>(&value) {
                        Ok(row) => timeline_text_candidates(&row),
                        Err(error) => {
                            invalid_rows += 1;
                            tracing::warn!(
                                code = "HYGIENE_TIMELINE_ROW_DECODE_FAILED",
                                key_hex = %hex_encode(&key),
                                %error,
                                "hygiene_scan_storage skipped undecodable CF_TIMELINE row"
                            );
                            Vec::new()
                        }
                    },
                    _ => Vec::new(),
                };
                for candidate in candidates {
                    if persisted_flags.len() >= flag_limit as usize {
                        stopped_because = "flag_limit_reached".to_owned();
                        next_cursor = Some(format!("{source_cf}:{}", hex_encode(&key)));
                        break 'sources;
                    }
                    let matches = scan_text(&candidate.text, min_score);
                    if matches.is_empty() {
                        continue;
                    }
                    let mut records = records_for_matches(
                        source_cf,
                        &hex_encode(&key),
                        &candidate.field,
                        &candidate.text,
                        &matches,
                    );
                    let remaining_flags = flag_limit as usize - persisted_flags.len();
                    if records.len() > remaining_flags {
                        records.truncate(remaining_flags);
                        stopped_because = "flag_limit_reached".to_owned();
                        next_cursor = Some(format!("{source_cf}:{}", hex_encode(&key)));
                    }
                    let (_written, mut readback) = write_flag_records(&runtime, records)?;
                    persisted_flags.append(&mut readback);
                    if persisted_flags.len() >= flag_limit as usize {
                        stopped_because = "flag_limit_reached".to_owned();
                        next_cursor = Some(format!("{source_cf}:{}", hex_encode(&key)));
                        break 'sources;
                    }
                }
            }
            if !more {
                break;
            }
            next_cursor = Some(format!("{source_cf}:{}", hex_encode(&last_key)));
            next_start = key_after(&last_key);
        }
    }
    let flags_written = persisted_flags.len() as u64;
    if stopped_because == "end_of_sources" {
        next_cursor = None;
    }
    Ok(HygieneScanStorageResponse {
        source_cfs,
        scanned_rows,
        invalid_rows,
        flags_written,
        persisted_flags,
        next_cursor,
        stopped_because,
        elapsed_ms: elapsed_ms(started),
    })
}

pub fn query_flags(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &HygieneFlagsParams,
) -> Result<HygieneFlagsResponse, ErrorData> {
    let source_cf = params
        .source_cf
        .as_deref()
        .map(|raw| normalize_source_cf(raw, true))
        .transpose()?;
    let source_key_hex = params
        .source_key_hex
        .as_deref()
        .map(|key| {
            validate_hex_text(key, "source_key_hex")?;
            Ok::<String, ErrorData>(key.to_ascii_lowercase())
        })
        .transpose()?;
    if source_key_hex.is_some() && source_cf.is_none() {
        return Err(invalid(
            "hygiene_flags source_key_hex requires source_cf so the prefix is exact",
        ));
    }
    let min_score = validate_min_score(params.min_score, 0, "hygiene_flags")?;
    let limit = validate_limit(
        params.limit.unwrap_or(100),
        1,
        MAX_FLAG_LIMIT,
        "hygiene_flags limit",
    )?;
    let prefix = flag_prefix(source_cf.as_deref(), source_key_hex.as_deref());
    let mut start_key = match params.cursor.as_deref() {
        Some(cursor) => key_after(
            &hex_decode(cursor).ok_or_else(|| invalid("hygiene_flags cursor is not valid hex"))?,
        ),
        None => prefix.as_bytes().to_vec(),
    };

    let runtime = lock_runtime(runtime)?;
    let mut flags = Vec::new();
    let mut scanned_rows = 0_u64;
    let mut next_cursor = None;
    let fetch_rows = STORAGE_SCAN_CHUNK_ROWS;
    loop {
        let rows = runtime
            .storage_cf_prefix_rows_from(cf::CF_KV, prefix.as_bytes(), &start_key, fetch_rows)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        let rows_len = rows.len();
        let mut last_key = None;
        for (key, value) in rows {
            scanned_rows += 1;
            last_key = Some(key.clone());
            match decode_json::<HygieneFlagRecord>(&value) {
                Ok(record) if record.score >= min_score => {
                    let kv_key_hex = hex_encode(&key);
                    flags.push(HygieneStoredFlag { kv_key_hex, record });
                    if flags.len() >= limit as usize {
                        next_cursor = Some(hex_encode(&key));
                        break;
                    }
                }
                Ok(_record) => {}
                Err(error) => {
                    tracing::warn!(
                        code = "HYGIENE_FLAG_ROW_DECODE_FAILED",
                        key_hex = %hex_encode(&key),
                        %error,
                        "hygiene_flags skipped undecodable flag row"
                    );
                }
            }
        }
        if next_cursor.is_some() || rows_len < fetch_rows {
            break;
        }
        let Some(last_key) = last_key else {
            break;
        };
        start_key = key_after(&last_key);
    }
    Ok(HygieneFlagsResponse {
        flags,
        next_cursor,
        scanned_rows,
    })
}

/// Reports flagged rows together with the derived state traceable to them
/// (#874/#968): which `CF_EPISODES` rows a flagged `CF_TIMELINE` row fed, which
/// `CF_ROUTINES` were mined from those episodes, and which generated
/// profile-authoring candidates reference those routines/episodes.
///
/// The derivation join is exact and physical:
/// - **flag → episode**: episodes are a deterministic function of `CF_TIMELINE`
///   rows over a time window, so a flagged row at `ts` fed every episode whose
///   `[start_ts_ns, end_ts_ns]` window contains `ts`. Containment is inclusive
///   on both ends *deliberately*: at an app-switch boundary a row's timestamp
///   equals both the closing episode's `end_ts_ns` and the opening episode's
///   `start_ts_ns`, and for a poisoning audit a false negative (missing a
///   poisoned routine) is far worse than naming an adjacent episode, so the
///   report errs toward over-inclusion at exact boundaries.
/// - **episode → routine**: a routine's persisted `evidence[].episode_ids` name
///   the exact `ep1-…` ids it was mined from; a routine is impacted iff that
///   set intersects the impacted episode ids.
/// - **routine/episode → authoring candidate**: a generated candidate is
///   impacted iff its persisted `CF_PROFILES` candidate row references an
///   impacted routine id or episode id anywhere in its evidence/patch JSON.
///
/// Only `CF_TIMELINE` flags carry episode/routine/candidate derivation —
/// episodes are segmented from `CF_TIMELINE` alone. Flags on
/// `CF_OBSERVATIONS`/`CF_OCR_CACHE` are reported with an honest empty
/// derivation and a note saying why, never silently dropped.
pub fn report(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &HygieneReportParams,
) -> Result<HygieneReportResponse, ErrorData> {
    let source_cf = params
        .source_cf
        .as_deref()
        .map(|raw| normalize_source_cf(raw, true))
        .transpose()?;
    let source_key_hex = params
        .source_key_hex
        .as_deref()
        .map(|key| {
            validate_hex_text(key, "source_key_hex")?;
            Ok::<String, ErrorData>(key.to_ascii_lowercase())
        })
        .transpose()?;
    if source_key_hex.is_some() && source_cf.is_none() {
        return Err(invalid(
            "hygiene_report source_key_hex requires source_cf so the prefix is exact",
        ));
    }
    let min_score = validate_min_score(params.min_score, 0, "hygiene_report")?;
    let limit = validate_limit(
        params.limit.unwrap_or(100),
        1,
        MAX_FLAG_LIMIT,
        "hygiene_report limit",
    )?;
    if let Some(range) = &params.time_range
        && range.start_ns >= range.end_ns
    {
        return Err(invalid(format!(
            "hygiene_report time_range.start_ns {} must be < end_ns {}",
            range.start_ns, range.end_ns
        )));
    }
    let prefix = flag_prefix(source_cf.as_deref(), source_key_hex.as_deref());
    let mut start_key = match params.cursor.as_deref() {
        Some(cursor) => key_after(
            &hex_decode(cursor).ok_or_else(|| invalid("hygiene_report cursor is not valid hex"))?,
        ),
        None => prefix.as_bytes().to_vec(),
    };

    let runtime = lock_runtime(runtime)?;

    // 1. Page over flag rows applying min_score + optional detected_at window.
    let mut page: Vec<HygieneStoredFlag> = Vec::new();
    let mut scanned_flag_rows = 0_u64;
    let mut next_cursor = None;
    let fetch_rows = STORAGE_SCAN_CHUNK_ROWS;
    loop {
        let rows = runtime
            .storage_cf_prefix_rows_from(cf::CF_KV, prefix.as_bytes(), &start_key, fetch_rows)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        let rows_len = rows.len();
        let mut last_key = None;
        for (key, value) in rows {
            scanned_flag_rows += 1;
            last_key = Some(key.clone());
            match decode_json::<HygieneFlagRecord>(&value) {
                Ok(record)
                    if record.score >= min_score
                        && flag_in_time_range(&record, params.time_range.as_ref()) =>
                {
                    page.push(HygieneStoredFlag {
                        kv_key_hex: hex_encode(&key),
                        record,
                    });
                    if page.len() >= limit as usize {
                        next_cursor = Some(hex_encode(&key));
                        break;
                    }
                }
                Ok(_record) => {}
                Err(error) => {
                    tracing::warn!(
                        code = "HYGIENE_FLAG_ROW_DECODE_FAILED",
                        key_hex = %hex_encode(&key),
                        %error,
                        "hygiene_report skipped undecodable flag row"
                    );
                }
            }
        }
        if next_cursor.is_some() || rows_len < fetch_rows {
            break;
        }
        let Some(last_key) = last_key else {
            break;
        };
        start_key = key_after(&last_key);
    }

    // 2. Decode each CF_TIMELINE flag's source key to its timeline timestamp.
    //    A timeline flag whose key the timeline codec cannot decode is data
    //    corruption — surfaced loudly, never silently skipped.
    let mut source_ts: Vec<Option<u64>> = Vec::with_capacity(page.len());
    let mut ts_set: BTreeSet<u64> = BTreeSet::new();
    for flag in &page {
        if flag.record.source_cf == SOURCE_CF_TIMELINE {
            let key = hex_decode(&flag.record.source_key_hex).ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "HYGIENE_REPORT_FLAG_KEY_INVALID: flag {} has non-hex source_key_hex {}",
                        flag.record.flag_id, flag.record.source_key_hex
                    ),
                )
            })?;
            let (ts_ns, _seq) = timeline_codec::decode_timeline_key(&key).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "HYGIENE_REPORT_TIMELINE_KEY_INVALID: flag {} source_key {} is not a \
                         CF_TIMELINE codec key: {error}",
                        flag.record.flag_id, flag.record.source_key_hex
                    ),
                )
            })?;
            source_ts.push(Some(ts_ns));
            ts_set.insert(ts_ns);
        } else {
            source_ts.push(None);
        }
    }

    // 3. Bounded CF_EPISODES scan shared with cleaning invalidation.
    let (ts_to_episode_ids, episodes_by_id, scanned_episode_rows) =
        scan_impacted_episodes(&runtime, &ts_set, ImpactScanCaller::Report)?;

    // 4. Bounded CF_ROUTINES scan shared with cleaning invalidation.
    let impacted_episode_ids: BTreeSet<String> = episodes_by_id.keys().cloned().collect();
    let episode_taints = read_taint_records(&runtime, "episode", &impacted_episode_ids)?;
    let (episode_to_routine_ids, routines_by_id, scanned_routine_rows) =
        scan_impacted_routines(&runtime, &impacted_episode_ids, ImpactScanCaller::Report)?;

    // 5. Point-lookup operator lifecycle for each impacted routine.
    let mut routine_state: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    for routine_id in routines_by_id.keys() {
        let key = routine_codec::routine_state_key(routine_id).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!("HYGIENE_REPORT_ROUTINE_STATE_KEY_INVALID for {routine_id}: {error}"),
            )
        })?;
        let rows = runtime
            .storage_cf_prefix_rows(cf::CF_ROUTINE_STATE, &key, 1)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if let Some((_key, value)) = rows.into_iter().find(|(row_key, _value)| row_key == &key) {
            let state = decode_json::<RoutineStateRecord>(&value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("HYGIENE_REPORT_ROUTINE_STATE_DECODE_FAILED for {routine_id}: {error}"),
                )
            })?;
            routine_state.insert(
                routine_id.clone(),
                (lifecycle_label(state.lifecycle), state.label),
            );
        }
    }

    // 6. Bounded profile-authoring candidate scan shared with cleaning
    //    invalidation.
    let impacted_routine_ids: BTreeSet<String> = routines_by_id.keys().cloned().collect();
    let routine_taints = read_taint_records(&runtime, "routine", &impacted_routine_ids)?;
    let (
        authoring_candidates_by_id,
        routine_to_authoring_candidate_ids,
        episode_to_authoring_candidate_ids,
        scanned_authoring_candidate_rows,
    ) = scan_impacted_candidates(
        &runtime,
        &impacted_routine_ids,
        &impacted_episode_ids,
        ImpactScanCaller::Report,
    )?;
    let impacted_candidate_ids: BTreeSet<String> =
        authoring_candidates_by_id.keys().cloned().collect();
    let authoring_candidate_taints =
        read_taint_records(&runtime, "authoring_candidate", &impacted_candidate_ids)?;

    // 7. Assemble each flag's impact from the maps.
    let mut flags_with_downstream_impact = 0_u64;
    let mut flag_impacts = Vec::with_capacity(page.len());
    for (flag, ts) in page.into_iter().zip(source_ts) {
        let (derived_episodes, derived_routines, derived_authoring_candidates, derivation_note) =
            match (ts, &flag) {
                (Some(ts_ns), _) => {
                    let episode_ids = ts_to_episode_ids.get(&ts_ns).cloned().unwrap_or_default();
                    let derived_episodes: Vec<HygieneImpactedEpisode> = episode_ids
                        .iter()
                        .filter_map(|episode_id| {
                            episodes_by_id.get(episode_id).cloned().map(|mut episode| {
                                episode.taint = episode_taints.get(episode_id).cloned();
                                episode.tainted = episode.taint.is_some();
                                episode
                            })
                        })
                        .collect();
                    // Routines reachable from this flag's episodes, with the exact
                    // linking episodes recorded per routine.
                    let mut routine_via: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
                    for episode_id in &episode_ids {
                        if let Some(routine_ids) = episode_to_routine_ids.get(episode_id) {
                            for routine_id in routine_ids {
                                routine_via
                                    .entry(routine_id.clone())
                                    .or_default()
                                    .insert(episode_id.clone());
                            }
                        }
                    }
                    let derived_routines: Vec<HygieneImpactedRoutine> = routine_via
                        .into_iter()
                        .filter_map(|(routine_id, via)| {
                            routines_by_id.get(&routine_id).map(|record| {
                                let (lifecycle, label) = routine_state
                                    .get(&routine_id)
                                    .map(|(lifecycle, label)| {
                                        (Some(lifecycle.clone()), label.clone())
                                    })
                                    .unwrap_or((None, None));
                                let taint = routine_taints.get(&routine_id).cloned();
                                let tainted = taint.is_some();
                                HygieneImpactedRoutine {
                                    routine_id: record.routine_id.clone(),
                                    schedule_label: record.schedule_label.clone(),
                                    granularity: granularity_label(record.granularity),
                                    support_days: record.support_days,
                                    confidence: record.confidence,
                                    lifecycle,
                                    label,
                                    via_episode_ids: via.into_iter().collect(),
                                    tainted,
                                    taint,
                                }
                            })
                        })
                        .collect();
                    let mut authoring_candidate_ids: BTreeSet<String> = BTreeSet::new();
                    for episode_id in &episode_ids {
                        if let Some(candidate_ids) =
                            episode_to_authoring_candidate_ids.get(episode_id)
                        {
                            authoring_candidate_ids.extend(candidate_ids.iter().cloned());
                        }
                    }
                    for routine in &derived_routines {
                        if let Some(candidate_ids) =
                            routine_to_authoring_candidate_ids.get(&routine.routine_id)
                        {
                            authoring_candidate_ids.extend(candidate_ids.iter().cloned());
                        }
                    }
                    let derived_authoring_candidates: Vec<HygieneImpactedAuthoringCandidate> =
                        authoring_candidate_ids
                            .into_iter()
                            .filter_map(|candidate_id| {
                                authoring_candidates_by_id.get(&candidate_id).cloned().map(
                                    |mut candidate| {
                                        candidate.taint =
                                            authoring_candidate_taints.get(&candidate_id).cloned();
                                        candidate.tainted = candidate.taint.is_some();
                                        candidate
                                    },
                                )
                            })
                            .collect();
                    let note = if derived_episodes.is_empty() {
                        format!(
                            "no CF_EPISODES row covers source ts {ts_ns}; the timeline row has not \
                         been segmented (run episode_segment) or is outside episode retention"
                        )
                    } else if derived_routines.is_empty() {
                        format!(
                            "fed {} episode(s); no mined routine references them (run routine_mine, \
                         or none qualified)",
                            derived_episodes.len()
                        )
                    } else if derived_authoring_candidates.is_empty() {
                        format!(
                            "fed {} episode(s) feeding {} mined routine(s); no profile-authoring \
                         candidate references those routines/episodes",
                            derived_episodes.len(),
                            derived_routines.len()
                        )
                    } else {
                        format!(
                            "fed {} episode(s) feeding {} mined routine(s) and {} \
                         profile-authoring candidate(s)",
                            derived_episodes.len(),
                            derived_routines.len(),
                            derived_authoring_candidates.len()
                        )
                    };
                    (
                        derived_episodes,
                        derived_routines,
                        derived_authoring_candidates,
                        note,
                    )
                }
                (None, flag) => (
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    format!(
                        "source CF {} is not an episode-segmentation input (episodes derive from \
                     CF_TIMELINE only); no episode/routine/candidate derivation",
                        flag.record.source_cf
                    ),
                ),
            };
        if !derived_episodes.is_empty() {
            flags_with_downstream_impact += 1;
        }
        flag_impacts.push(HygieneFlagImpact {
            flag,
            source_ts_ns: ts,
            derived_episodes,
            derived_routines,
            derived_authoring_candidates,
            derivation_note,
        });
    }

    let impacted_confirmed_routine_count = routine_state
        .values()
        .filter(|(lifecycle, _label)| lifecycle == "confirmed")
        .count() as u64;
    let impacted_accepted_authoring_candidate_count = authoring_candidates_by_id
        .values()
        .filter(|candidate| candidate.state == "accepted")
        .count() as u64;
    let summary = HygieneReportSummary {
        flags_total: flag_impacts.len() as u64,
        flags_with_downstream_impact,
        impacted_episode_count: episodes_by_id.len() as u64,
        impacted_routine_count: routines_by_id.len() as u64,
        impacted_confirmed_routine_count,
        impacted_authoring_candidate_count: authoring_candidates_by_id.len() as u64,
        impacted_accepted_authoring_candidate_count,
    };

    Ok(HygieneReportResponse {
        flags: flag_impacts,
        summary,
        next_cursor,
        scanned_flag_rows,
        scanned_episode_rows,
        scanned_routine_rows,
        scanned_authoring_candidate_rows,
    })
}

fn flag_in_time_range(record: &HygieneFlagRecord, range: Option<&HygieneReportTimeRange>) -> bool {
    let Some(range) = range else {
        return true;
    };
    let Some(ns) = record.detected_at.timestamp_nanos_opt() else {
        return false;
    };
    let ns = u64::try_from(ns).unwrap_or(0);
    ns >= range.start_ns && ns < range.end_ns
}

fn actor_label(actor: &TimelineActor) -> String {
    match actor {
        TimelineActor::Human => "human".to_owned(),
        TimelineActor::Agent { session_id } => format!("agent:{session_id}"),
    }
}

fn lifecycle_label(lifecycle: RoutineLifecycle) -> String {
    match lifecycle {
        RoutineLifecycle::Candidate => "candidate",
        RoutineLifecycle::Confirmed => "confirmed",
        RoutineLifecycle::Disabled => "disabled",
        RoutineLifecycle::Archived => "archived",
        RoutineLifecycle::Quarantined => "quarantined",
    }
    .to_owned()
}

fn granularity_label(granularity: RoutineGranularity) -> String {
    match granularity {
        RoutineGranularity::App => "app",
        RoutineGranularity::AppDocument => "app_document",
    }
    .to_owned()
}

fn candidate_reference_strings(
    candidate: &ProfileAuthoringCandidate,
) -> Result<BTreeSet<String>, ErrorData> {
    let value = serde_json::to_value(candidate).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "HYGIENE_REPORT_AUTHORING_CANDIDATE_ENCODE_FAILED for {}: {error}",
                candidate.candidate_id
            ),
        )
    })?;
    let mut references = BTreeSet::new();
    collect_json_strings(&value, &mut references);
    Ok(references)
}

fn collect_json_strings(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::String(text) => {
            let text = text.trim();
            if !text.is_empty() {
                output.insert(text.to_owned());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_json_strings(value, output);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if !key.trim().is_empty() {
                    output.insert(key.clone());
                }
                collect_json_strings(value, output);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

// ---------------------------------------------------------------------------
// Data-cleaning tools (#875): `timeline_redact` masks flagged spans in place
// (preserving row structure for mining continuity) and the purge path hard
// deletes by flag id. Both INVALIDATE derived state — every impacted routine,
// episode, and profile-authoring candidate gets a taint record in the
// `hygiene/taint/v1/` CF_KV ledger — and both are audit-logged with flag ids
// and counts only, never the cleaned content.
// ---------------------------------------------------------------------------

/// CF_KV prefix for derived-state taint records written when poisoned source
/// rows are cleaned. One record per impacted artifact; additive, so it never
/// mutates the operator-owned `CF_ROUTINE_STATE`/`CF_EPISODES` rows themselves.
const TAINT_PREFIX: &str = "hygiene/taint/v1/";
/// Default replacement for a redacted span. Deliberately benign so a re-scan of
/// the cleaned row never re-flags the marker itself.
const DEFAULT_REDACTION_MARKER: &str = "[REDACTED]";
/// Upper bound on flags one clean op resolves, so a `query` selector can never
/// fan out into an unbounded rewrite.
const MAX_CLEAN_FLAGS: usize = 5_000;

const fn default_true() -> bool {
    true
}

/// Operation tag recorded on taint + audit rows so a consumer can tell how a
/// row was cleaned.
pub const CLEAN_OP_REDACT: &str = "timeline_redact";
pub const CLEAN_OP_PURGE: &str = "timeline_purge_by_flags";

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneRedactParams {
    /// Explicit flag ids to redact. Mutually exclusive with the `source_*` /
    /// `min_score` query selector. Every id must resolve to a stored flag or
    /// the call fails loudly (a silently-skipped id would leave poison behind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag_ids: Option<Vec<String>>,
    /// Query selector: restrict to one source CF (`CF_TIMELINE`,
    /// `CF_OBSERVATIONS`, `CF_OCR_CACHE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cf: Option<String>,
    /// Query selector: restrict to one exact source row, hex-encoded. Requires
    /// `source_cf`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key_hex: Option<String>,
    /// Query selector: minimum flag score to redact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<u32>,
    /// Marker that replaces each flagged span (default `[REDACTED]`). Rejected
    /// if it itself trips the injection scanner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    /// Resolve, verify, and report outcomes without mutating any row, taint
    /// record, or audit row.
    #[serde(default)]
    pub dry_run: bool,
    /// Write derived-state taint records for impacted routines/episodes/
    /// candidates (default true).
    #[serde(default = "default_true")]
    pub invalidate: bool,
}

/// Per-flag redaction outcome — honest about every span the tool did NOT mask.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneFlagRedactOutcome {
    pub flag_id: String,
    pub source_cf: String,
    pub source_key_hex: String,
    pub source_field: String,
    /// `redacted` (span masked), `already_redacted` (span already absent and
    /// the marker present), `source_missing` (the physical row is gone),
    /// `field_missing` (the JSON pointer no longer resolves to a string), or
    /// `stale_source` (the flagged text is absent but the row was not cleaned
    /// by us — content changed out from under the flag).
    pub status: String,
    pub detail: String,
}

/// One derived artifact tainted because a poisoned source row feeding it was
/// cleaned. Persisted under [`TAINT_PREFIX`] and read back during manual
/// verification and regression readbacks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneTaintRecord {
    pub schema_version: u32,
    /// `routine`, `episode`, or `authoring_candidate`.
    pub artifact_kind: String,
    pub artifact_id: String,
    /// [`CLEAN_OP_REDACT`] or [`CLEAN_OP_PURGE`].
    pub cleaning_op: String,
    pub reason: String,
    /// Flag ids whose cleaning poisoned this artifact.
    pub source_flag_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaning_audit_key_hex: Option<String>,
    pub tainted_at_ns: u64,
    pub by_session: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneInvalidationSummary {
    pub tainted_routine_ids: Vec<String>,
    pub tainted_episode_ids: Vec<String>,
    pub tainted_authoring_candidate_ids: Vec<String>,
    pub taint_records_written: u64,
    pub scanned_episode_rows: u64,
    pub scanned_routine_rows: u64,
    pub scanned_authoring_candidate_rows: u64,
    /// Honest explanation when nothing was tainted (e.g. no timeline flags, or
    /// the rows were never segmented/mined).
    pub note: String,
}

impl HygieneInvalidationSummary {
    fn empty(note: impl Into<String>) -> Self {
        Self {
            tainted_routine_ids: Vec::new(),
            tainted_episode_ids: Vec::new(),
            tainted_authoring_candidate_ids: Vec::new(),
            taint_records_written: 0,
            scanned_episode_rows: 0,
            scanned_routine_rows: 0,
            scanned_authoring_candidate_rows: 0,
            note: note.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneRedactResponse {
    pub matched_flags: u64,
    pub redacted_flags: u64,
    /// Distinct physical rows rewritten.
    pub redacted_rows: u64,
    pub marker: String,
    pub dry_run: bool,
    pub outcomes: Vec<HygieneFlagRedactOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_key_hex: Option<String>,
    pub invalidation: HygieneInvalidationSummary,
    pub elapsed_ms: u64,
}

#[must_use]
pub fn required_permissions_redact(_params: &HygieneRedactParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

/// How a clean op selected its flags. Built from either explicit ids or the
/// `source_*`/`min_score` query, validated as mutually exclusive.
pub(crate) enum CleanFlagSelector {
    Ids(Vec<String>),
    Query {
        source_cf: Option<String>,
        source_key_hex: Option<String>,
        min_score: u32,
    },
}

/// Resolves a [`CleanFlagSelector`] to the exact stored flags it names. For ids
/// the whole flag store is scanned (bounded by [`MAX_CLEAN_FLAGS`] distinct
/// ids) and every requested id MUST be found — an unresolved id is a hard error
/// so a cleaning op can never silently leave a poisoned row untouched.
pub(crate) fn resolve_clean_flags(
    runtime: &ReflexRuntime,
    selector: &CleanFlagSelector,
) -> Result<Vec<HygieneStoredFlag>, ErrorData> {
    match selector {
        CleanFlagSelector::Ids(ids) => {
            let mut wanted: BTreeSet<&str> = BTreeSet::new();
            for id in ids {
                if id.trim().is_empty() {
                    return Err(invalid("flag_ids entries must not be empty"));
                }
                wanted.insert(id.as_str());
            }
            if wanted.is_empty() {
                return Err(invalid("flag_ids must name at least one flag"));
            }
            if wanted.len() > MAX_CLEAN_FLAGS {
                return Err(invalid(format!(
                    "flag_ids names {} flags; the per-call ceiling is {MAX_CLEAN_FLAGS}",
                    wanted.len()
                )));
            }
            let mut found: BTreeMap<String, HygieneStoredFlag> = BTreeMap::new();
            let prefix = FLAG_PREFIX.as_bytes();
            let mut start = prefix.to_vec();
            loop {
                let rows = runtime
                    .storage_cf_prefix_rows_from(cf::CF_KV, prefix, &start, STORAGE_SCAN_CHUNK_ROWS)
                    .map_err(|error| mcp_error(error.code(), error.to_string()))?;
                if rows.is_empty() {
                    break;
                }
                let rows_len = rows.len();
                let mut last = None;
                for (key, value) in rows {
                    last = Some(key.clone());
                    let record = decode_json::<HygieneFlagRecord>(&value).map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "HYGIENE_CLEAN_FLAG_ROW_DECODE_FAILED in CF_KV at {}: {error}",
                                hex_encode(&key)
                            ),
                        )
                    })?;
                    if wanted.contains(record.flag_id.as_str()) {
                        let kv_key_hex = hex_encode(&key);
                        found
                            .entry(record.flag_id.clone())
                            .or_insert(HygieneStoredFlag { kv_key_hex, record });
                    }
                }
                if found.len() == wanted.len() || rows_len < STORAGE_SCAN_CHUNK_ROWS {
                    break;
                }
                let Some(last) = last else { break };
                start = key_after(&last);
            }
            if found.len() != wanted.len() {
                let missing: Vec<&str> = wanted
                    .iter()
                    .copied()
                    .filter(|id| !found.contains_key(*id))
                    .collect();
                return Err(mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "HYGIENE_CLEAN_FLAG_IDS_UNRESOLVED: {} of {} flag id(s) not found: {:?}",
                        missing.len(),
                        wanted.len(),
                        missing
                    ),
                ));
            }
            // Preserve the caller's id order for stable, reviewable output.
            Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
        }
        CleanFlagSelector::Query {
            source_cf,
            source_key_hex,
            min_score,
        } => {
            let prefix = flag_prefix(source_cf.as_deref(), source_key_hex.as_deref());
            let mut start = prefix.as_bytes().to_vec();
            let mut flags = Vec::new();
            loop {
                let rows = runtime
                    .storage_cf_prefix_rows_from(
                        cf::CF_KV,
                        prefix.as_bytes(),
                        &start,
                        STORAGE_SCAN_CHUNK_ROWS,
                    )
                    .map_err(|error| mcp_error(error.code(), error.to_string()))?;
                if rows.is_empty() {
                    break;
                }
                let rows_len = rows.len();
                let mut last = None;
                for (key, value) in rows {
                    last = Some(key.clone());
                    let record = decode_json::<HygieneFlagRecord>(&value).map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "HYGIENE_CLEAN_FLAG_ROW_DECODE_FAILED in CF_KV at {}: {error}",
                                hex_encode(&key)
                            ),
                        )
                    })?;
                    if record.score >= *min_score {
                        flags.push(HygieneStoredFlag {
                            kv_key_hex: hex_encode(&key),
                            record,
                        });
                        if flags.len() > MAX_CLEAN_FLAGS {
                            return Err(invalid(format!(
                                "query selector matches more than {MAX_CLEAN_FLAGS} flags; narrow \
                                 source_cf/source_key_hex/min_score or clean by explicit flag_ids"
                            )));
                        }
                    }
                }
                if rows_len < STORAGE_SCAN_CHUNK_ROWS {
                    break;
                }
                let Some(last) = last else { break };
                start = key_after(&last);
            }
            Ok(flags)
        }
    }
}

/// Masks the flagged spans named by `params` in their physical source rows,
/// preserving every row's JSON structure (only the flagged substring inside a
/// string field is replaced), then invalidates derived state and writes one
/// audit row. Content-anchored: each span is located by its recorded text +
/// SHA-256, so the redaction is idempotent and resilient to offset drift from a
/// prior redaction of the same field.
pub fn redact(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &HygieneRedactParams,
    by_session: &str,
) -> Result<HygieneRedactResponse, ErrorData> {
    let started = Instant::now();
    let marker = params
        .marker
        .clone()
        .unwrap_or_else(|| DEFAULT_REDACTION_MARKER.to_owned());
    if marker.trim().is_empty() {
        return Err(invalid("timeline_redact marker must not be empty"));
    }
    if !scan_text(&marker, DEFAULT_MIN_SCORE).is_empty() {
        return Err(invalid(
            "timeline_redact marker itself trips the injection scanner; choose a benign marker",
        ));
    }
    let selector = redact_selector(params)?;

    let guard = lock_runtime(runtime)?;
    let flags = resolve_clean_flags(&guard, &selector)?;
    let matched_flags = flags.len() as u64;

    // Group flags by physical source row so each row is read, mutated, and
    // written exactly once even when it carries several flags.
    let mut by_row: BTreeMap<(String, String), Vec<HygieneStoredFlag>> = BTreeMap::new();
    for flag in flags {
        by_row
            .entry((
                flag.record.source_cf.clone(),
                flag.record.source_key_hex.clone(),
            ))
            .or_default()
            .push(flag);
    }

    let mut outcomes: Vec<HygieneFlagRedactOutcome> = Vec::new();
    let mut redacted_flags = 0_u64;
    let mut redacted_rows = 0_u64;
    let mut redacted_flag_records: Vec<HygieneStoredFlag> = Vec::new();

    for ((source_cf, source_key_hex), row_flags) in by_row {
        let source_cf = normalize_source_cf(&source_cf, true)?;
        let key = hex_decode(&source_key_hex).ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!("HYGIENE_REDACT_FLAG_KEY_INVALID: non-hex source_key_hex {source_key_hex}"),
            )
        })?;
        let existing = guard
            .storage_cf_prefix_rows(&source_cf, &key, 1)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let Some((_row_key, value)) = existing
            .into_iter()
            .find(|(row_key, _value)| row_key == &key)
        else {
            for flag in &row_flags {
                outcomes.push(outcome(
                    flag,
                    "source_missing",
                    format!("no row at {source_key_hex} in {source_cf}; nothing to redact"),
                ));
            }
            continue;
        };

        let mut document: Value = serde_json::from_slice(&value).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "HYGIENE_REDACT_ROW_DECODE_FAILED in {source_cf} at {source_key_hex}: {error}"
                ),
            )
        })?;

        let mut row_mutated = false;
        let mut row_redacted_flags: Vec<HygieneStoredFlag> = Vec::new();
        for flag in &row_flags {
            match redact_one_span(&mut document, &flag.record, &marker)? {
                SpanRedaction::Redacted => {
                    row_mutated = true;
                    redacted_flags += 1;
                    row_redacted_flags.push(flag.clone());
                    outcomes.push(outcome(flag, "redacted", "span masked".to_owned()));
                }
                SpanRedaction::AlreadyClean => outcomes.push(outcome(
                    flag,
                    "already_redacted",
                    "flagged text already absent and marker present; idempotent no-op".to_owned(),
                )),
                SpanRedaction::FieldMissing => outcomes.push(outcome(
                    flag,
                    "field_missing",
                    format!(
                        "JSON pointer {} no longer resolves to a string",
                        flag.record.source_field
                    ),
                )),
                SpanRedaction::Stale => outcomes.push(outcome(
                    flag,
                    "stale_source",
                    "flagged text absent without a redaction marker; row changed out from under \
                     the flag — left untouched, re-scan to refresh"
                        .to_owned(),
                )),
            }
        }

        if row_mutated && !params.dry_run {
            let encoded = serde_json::to_vec(&document).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("HYGIENE_REDACT_ROW_ENCODE_FAILED at {source_key_hex}: {error}"),
                )
            })?;
            guard
                .storage_replace_rows(&source_cf, Vec::new(), vec![(key.clone(), encoded)])
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("HYGIENE_REDACT_WRITE_FAILED at {source_key_hex}: {error}"),
                    )
                })?;
            verify_redacted_row(
                &guard,
                &source_cf,
                &key,
                &source_key_hex,
                &row_redacted_flags,
                &marker,
            )?;
        }
        if row_mutated {
            redacted_rows += 1;
            redacted_flag_records.extend(row_redacted_flags);
        }
    }

    // Invalidate derived state from the rows we actually cleaned (dry_run never
    // mutates anything, including taint).
    let (invalidation, audit_key_hex) = if params.dry_run {
        (
            HygieneInvalidationSummary::empty("dry_run: no rows, taint, or audit written"),
            None,
        )
    } else {
        let audit_payload = serde_json::json!({
            "op": CLEAN_OP_REDACT,
            "by_session": by_session,
            "matched_flags": matched_flags,
            "redacted_flags": redacted_flags,
            "redacted_rows": redacted_rows,
            "flag_ids": redacted_flag_records
                .iter()
                .map(|flag| flag.record.flag_id.clone())
                .collect::<Vec<_>>(),
            "marker": marker,
        });
        let audit_key_hex = if redacted_rows > 0 {
            Some(crate::m3::timeline::write_cleaning_audit_row(
                &guard,
                audit_payload,
            )?)
        } else {
            None
        };
        let invalidation = if params.invalidate {
            invalidate_cleaned_flags(
                &guard,
                &redacted_flag_records,
                CLEAN_OP_REDACT,
                audit_key_hex.as_deref(),
                by_session,
            )?
        } else {
            HygieneInvalidationSummary::empty("invalidate=false: derived-state taint skipped")
        };
        (invalidation, audit_key_hex)
    };
    drop(guard);

    tracing::info!(
        code = "HYGIENE_REDACT_COMPLETED",
        matched_flags,
        redacted_flags,
        redacted_rows,
        dry_run = params.dry_run,
        tainted_routines = invalidation.tainted_routine_ids.len(),
        by_session,
        "timeline_redact completed"
    );

    Ok(HygieneRedactResponse {
        matched_flags,
        redacted_flags,
        redacted_rows,
        marker,
        dry_run: params.dry_run,
        outcomes,
        audit_key_hex,
        invalidation,
        elapsed_ms: elapsed_ms(started),
    })
}

fn redact_selector(params: &HygieneRedactParams) -> Result<CleanFlagSelector, ErrorData> {
    let has_query =
        params.source_cf.is_some() || params.source_key_hex.is_some() || params.min_score.is_some();
    match (&params.flag_ids, has_query) {
        (Some(_), true) => Err(invalid(
            "timeline_redact flag_ids is mutually exclusive with the source_cf/source_key_hex/min_score query",
        )),
        (Some(ids), false) => Ok(CleanFlagSelector::Ids(ids.clone())),
        (None, true) => {
            let source_cf = params
                .source_cf
                .as_deref()
                .map(|raw| normalize_source_cf(raw, true))
                .transpose()?;
            let source_key_hex = params
                .source_key_hex
                .as_deref()
                .map(|key| {
                    validate_hex_text(key, "source_key_hex")?;
                    Ok::<String, ErrorData>(key.to_ascii_lowercase())
                })
                .transpose()?;
            if source_key_hex.is_some() && source_cf.is_none() {
                return Err(invalid(
                    "timeline_redact source_key_hex requires source_cf so the prefix is exact",
                ));
            }
            let min_score = validate_min_score(params.min_score, 0, "timeline_redact")?;
            Ok(CleanFlagSelector::Query {
                source_cf,
                source_key_hex,
                min_score,
            })
        }
        (None, false) => Err(invalid(
            "timeline_redact requires flag_ids or a query (source_cf/source_key_hex/min_score)",
        )),
    }
}

#[derive(Debug)]
enum SpanRedaction {
    Redacted,
    AlreadyClean,
    FieldMissing,
    Stale,
}

/// Masks a single flag's span in `document`, anchored on the recorded span text
/// (verified by SHA-256), located by the recorded byte offsets first and a
/// substring search as a fallback for offset drift.
fn redact_one_span(
    document: &mut Value,
    flag: &HygieneFlagRecord,
    marker: &str,
) -> Result<SpanRedaction, ErrorData> {
    // The stored flag must be internally consistent: span_text must hash to
    // span_text_sha256. A mismatch is a corrupt flag, surfaced loudly.
    if sha256_hex(flag.span_text.as_bytes()) != flag.span_text_sha256 {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "HYGIENE_REDACT_FLAG_SELF_INCONSISTENT: flag {} span_text does not match its \
                 span_text_sha256",
                flag.flag_id
            ),
        ));
    }
    let Some(target) = document.pointer_mut(&flag.source_field) else {
        return Ok(SpanRedaction::FieldMissing);
    };
    let Value::String(current) = target else {
        return Ok(SpanRedaction::FieldMissing);
    };

    let needle = &flag.span_text;
    // Prefer the exact recorded offsets when they still bracket the needle.
    let start = usize::try_from(flag.span_start).unwrap_or(usize::MAX);
    let end = usize::try_from(flag.span_end).unwrap_or(usize::MAX);
    let at_offset = start <= end
        && end <= current.len()
        && current.is_char_boundary(start)
        && current.is_char_boundary(end)
        && &current[start..end] == needle;
    if at_offset {
        let mut next = String::with_capacity(current.len() - (end - start) + marker.len());
        next.push_str(&current[..start]);
        next.push_str(marker);
        next.push_str(&current[end..]);
        *current = next;
        return Ok(SpanRedaction::Redacted);
    }
    // Offset drifted (e.g. an earlier redaction shortened the field): locate the
    // recorded text by content.
    if let Some(found) = current.find(needle.as_str()) {
        let next = current.replacen(needle.as_str(), marker, 1);
        let _ = found;
        *current = next;
        return Ok(SpanRedaction::Redacted);
    }
    // Needle absent: already cleaned if the marker is present, otherwise the row
    // changed for reasons we did not cause.
    if current.contains(marker) {
        Ok(SpanRedaction::AlreadyClean)
    } else {
        Ok(SpanRedaction::Stale)
    }
}

/// Regression readback: re-reads the row from storage and checks every redacted
/// flag's text is gone and the marker is present.
fn verify_redacted_row(
    runtime: &ReflexRuntime,
    source_cf: &str,
    key: &[u8],
    source_key_hex: &str,
    redacted_flags: &[HygieneStoredFlag],
    marker: &str,
) -> Result<(), ErrorData> {
    let rows = runtime
        .storage_cf_prefix_rows(source_cf, key, 1)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let (_row_key, value) = rows
        .into_iter()
        .find(|(row_key, _value)| row_key.as_slice() == key)
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("HYGIENE_REDACT_READBACK_ABSENT: redacted row {source_key_hex} vanished"),
            )
        })?;
    let document: Value = serde_json::from_slice(&value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("HYGIENE_REDACT_READBACK_DECODE_FAILED at {source_key_hex}: {error}"),
        )
    })?;
    for flag in redacted_flags {
        let field = document
            .pointer(&flag.record.source_field)
            .and_then(Value::as_str);
        let Some(field) = field else {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "HYGIENE_REDACT_READBACK_FIELD_LOST: {} missing after redacting flag {}",
                    flag.record.source_field, flag.record.flag_id
                ),
            ));
        };
        if field.contains(flag.record.span_text.as_str()) {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "HYGIENE_REDACT_READBACK_TEXT_PRESENT: flag {} text survived redaction in {}",
                    flag.record.flag_id, flag.record.source_field
                ),
            ));
        }
        if !field.contains(marker) {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "HYGIENE_REDACT_READBACK_MARKER_ABSENT: marker missing in {} after redacting \
                     flag {}",
                    flag.record.source_field, flag.record.flag_id
                ),
            ));
        }
    }
    Ok(())
}

fn outcome(flag: &HygieneStoredFlag, status: &str, detail: String) -> HygieneFlagRedactOutcome {
    HygieneFlagRedactOutcome {
        flag_id: flag.record.flag_id.clone(),
        source_cf: flag.record.source_cf.clone(),
        source_key_hex: flag.record.source_key_hex.clone(),
        source_field: flag.record.source_field.clone(),
        status: status.to_owned(),
        detail,
    }
}

/// Computes the derived artifacts (episodes → routines → authoring candidates)
/// reachable from a set of cleaned flags and writes one taint record per
/// artifact into the [`TAINT_PREFIX`] ledger. Only `CF_TIMELINE` flags carry
/// derivation (episodes are segmented from `CF_TIMELINE` alone), mirroring the
/// `hygiene_report` join exactly.
pub(crate) fn invalidate_cleaned_flags(
    runtime: &ReflexRuntime,
    flags: &[HygieneStoredFlag],
    cleaning_op: &str,
    audit_key_hex: Option<&str>,
    by_session: &str,
) -> Result<HygieneInvalidationSummary, ErrorData> {
    // 1. Timeline source timestamps of the cleaned flags.
    let mut ts_set: BTreeSet<u64> = BTreeSet::new();
    let mut ts_to_flag_ids: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for flag in flags {
        if flag.record.source_cf != SOURCE_CF_TIMELINE {
            continue;
        }
        let key = hex_decode(&flag.record.source_key_hex).ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "HYGIENE_INVALIDATE_FLAG_KEY_INVALID: flag {} has non-hex source_key_hex {}",
                    flag.record.flag_id, flag.record.source_key_hex
                ),
            )
        })?;
        let (ts_ns, _seq) = timeline_codec::decode_timeline_key(&key).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "HYGIENE_INVALIDATE_TIMELINE_KEY_INVALID: flag {} key is not a CF_TIMELINE \
                     codec key: {error}",
                    flag.record.flag_id
                ),
            )
        })?;
        ts_set.insert(ts_ns);
        ts_to_flag_ids
            .entry(ts_ns)
            .or_default()
            .insert(flag.record.flag_id.clone());
    }
    if ts_set.is_empty() {
        return Ok(HygieneInvalidationSummary::empty(
            "no CF_TIMELINE flags among the cleaned set; only timeline rows feed derived state",
        ));
    }

    // 2. Impacted episodes (time-window containment) — same join as hygiene_report.
    let (ts_to_episode_ids, episodes_by_id, scanned_episode_rows) =
        scan_impacted_episodes(runtime, &ts_set, ImpactScanCaller::Invalidate)?;
    let impacted_episode_ids: BTreeSet<String> = episodes_by_id.keys().cloned().collect();

    // 3. Impacted routines (evidence episode-id intersection).
    let (episode_to_routine_ids, routines_by_id, scanned_routine_rows) =
        scan_impacted_routines(runtime, &impacted_episode_ids, ImpactScanCaller::Invalidate)?;
    let impacted_routine_ids: BTreeSet<String> = routines_by_id.keys().cloned().collect();

    // 4. Impacted authoring candidates (reference impacted routine/episode ids).
    let (authoring_candidates_by_id, _, _, scanned_authoring_candidate_rows) =
        scan_impacted_candidates(
            runtime,
            &impacted_routine_ids,
            &impacted_episode_ids,
            ImpactScanCaller::Invalidate,
        )?;
    let impacted_candidate_ids: BTreeSet<String> =
        authoring_candidates_by_id.keys().cloned().collect();

    // Map each impacted artifact back to the flag ids that poisoned it so the
    // taint record carries an honest provenance trail.
    let mut episode_flag_ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (ts, episode_ids) in &ts_to_episode_ids {
        let Some(flag_ids) = ts_to_flag_ids.get(ts) else {
            continue;
        };
        for episode_id in episode_ids {
            episode_flag_ids
                .entry(episode_id.clone())
                .or_default()
                .extend(flag_ids.iter().cloned());
        }
    }
    let mut routine_flag_ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (episode_id, routine_ids) in &episode_to_routine_ids {
        let Some(flag_ids) = episode_flag_ids.get(episode_id) else {
            continue;
        };
        for routine_id in routine_ids {
            routine_flag_ids
                .entry(routine_id.clone())
                .or_default()
                .extend(flag_ids.iter().cloned());
        }
    }

    let all_flag_ids: Vec<String> = flags
        .iter()
        .map(|flag| flag.record.flag_id.clone())
        .collect();

    // 5. Write one taint record per impacted artifact, then read one back to
    //    prove the ledger landed.
    let tainted_at_ns = now_ns();
    let mut taint_rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut routine_ids: Vec<String> = impacted_routine_ids.iter().cloned().collect();
    routine_ids.sort();
    for routine_id in &routine_ids {
        let source_flag_ids = routine_flag_ids
            .get(routine_id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_else(|| all_flag_ids.clone());
        taint_rows.push(taint_row(
            "routine",
            routine_id,
            cleaning_op,
            "mined from cleaned poisoned timeline rows; re-mine before trusting",
            source_flag_ids,
            audit_key_hex,
            tainted_at_ns,
            by_session,
        )?);
    }
    let mut episode_ids: Vec<String> = impacted_episode_ids.iter().cloned().collect();
    episode_ids.sort();
    for episode_id in &episode_ids {
        let source_flag_ids = episode_flag_ids
            .get(episode_id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_else(|| all_flag_ids.clone());
        taint_rows.push(taint_row(
            "episode",
            episode_id,
            cleaning_op,
            "segmented from cleaned poisoned timeline rows; re-segment before trusting",
            source_flag_ids,
            audit_key_hex,
            tainted_at_ns,
            by_session,
        )?);
    }
    let mut candidate_ids: Vec<String> = impacted_candidate_ids.iter().cloned().collect();
    candidate_ids.sort();
    for candidate_id in &candidate_ids {
        taint_rows.push(taint_row(
            "authoring_candidate",
            candidate_id,
            cleaning_op,
            "references impacted routines/episodes; re-review before installing",
            all_flag_ids.clone(),
            audit_key_hex,
            tainted_at_ns,
            by_session,
        )?);
    }

    let taint_records_written = taint_rows.len() as u64;
    if !taint_rows.is_empty() {
        let sample_key = taint_rows[0].0.clone();
        runtime
            .storage_replace_rows(cf::CF_KV, Vec::new(), taint_rows)
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("HYGIENE_INVALIDATE_TAINT_WRITE_FAILED: {error}"),
                )
            })?;
        // Regression readback: the ledger must be physically present, not just
        // acked.
        let rows = runtime
            .storage_cf_prefix_rows(cf::CF_KV, &sample_key, 1)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.first().map(|(row_key, _value)| row_key.as_slice()) != Some(sample_key.as_slice()) {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "HYGIENE_INVALIDATE_TAINT_READBACK_ABSENT: taint record absent on readback",
            ));
        }
    }

    let note = if taint_records_written == 0 {
        format!(
            "{} timeline flag(s) cleaned but no episode/routine/candidate derives from them \
             (unsegmented, unmined, or outside retention)",
            ts_set.len()
        )
    } else {
        format!(
            "tainted {} routine(s), {} episode(s), {} authoring candidate(s) from {} timeline \
             flag(s)",
            routine_ids.len(),
            episode_ids.len(),
            candidate_ids.len(),
            ts_set.len()
        )
    };

    Ok(HygieneInvalidationSummary {
        tainted_routine_ids: routine_ids,
        tainted_episode_ids: episode_ids,
        tainted_authoring_candidate_ids: candidate_ids,
        taint_records_written,
        scanned_episode_rows,
        scanned_routine_rows,
        scanned_authoring_candidate_rows,
        note,
    })
}

#[allow(clippy::too_many_arguments, reason = "one flat taint-record builder")]
fn taint_row(
    artifact_kind: &str,
    artifact_id: &str,
    cleaning_op: &str,
    reason: &str,
    source_flag_ids: Vec<String>,
    audit_key_hex: Option<&str>,
    tainted_at_ns: u64,
    by_session: &str,
) -> Result<(Vec<u8>, Vec<u8>), ErrorData> {
    let record = HygieneTaintRecord {
        schema_version: SCHEMA_VERSION,
        artifact_kind: artifact_kind.to_owned(),
        artifact_id: artifact_id.to_owned(),
        cleaning_op: cleaning_op.to_owned(),
        reason: reason.to_owned(),
        source_flag_ids,
        cleaning_audit_key_hex: audit_key_hex.map(str::to_owned),
        tainted_at_ns,
        by_session: by_session.to_owned(),
    };
    let key = format!("{TAINT_PREFIX}{artifact_kind}/{artifact_id}").into_bytes();
    let value = encode_json(&record).map_err(|error| {
        mcp_error(
            error.code(),
            format!("HYGIENE_TAINT_ENCODE_FAILED for {artifact_kind}/{artifact_id}: {error}"),
        )
    })?;
    Ok((key, value))
}

/// Reads the taint record for one artifact, if present. Used by manual
/// verification evidence and by the upcoming taint-surfacing consumers deciding
/// whether learned state is poisoned (a routine_inspect / hygiene_report taint
/// column, #875 follow-up).
pub(crate) fn read_taint_record(
    runtime: &ReflexRuntime,
    artifact_kind: &str,
    artifact_id: &str,
) -> Result<Option<HygieneTaintRecord>, ErrorData> {
    let key = taint_key(artifact_kind, artifact_id);
    let rows = runtime
        .storage_cf_prefix_rows(cf::CF_KV, &key, 1)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let Some((_key, value)) = rows.into_iter().find(|(row_key, _value)| row_key == &key) else {
        return Ok(None);
    };
    decode_taint_record(artifact_kind, artifact_id, &value).map(Some)
}

pub(crate) fn read_taint_record_from_db(
    db: &Db,
    artifact_kind: &str,
    artifact_id: &str,
) -> Result<Option<HygieneTaintRecord>, ErrorData> {
    let key = taint_key(artifact_kind, artifact_id);
    let value = db.get_cf(cf::CF_KV, &key).map_err(|error| {
        mcp_error(
            error.code(),
            format!("HYGIENE_TAINT_READ_FAILED for {artifact_kind}/{artifact_id}: {error}"),
        )
    })?;
    let Some(value) = value else {
        return Ok(None);
    };
    decode_taint_record(artifact_kind, artifact_id, &value).map(Some)
}

fn read_taint_records(
    runtime: &ReflexRuntime,
    artifact_kind: &str,
    artifact_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, HygieneTaintRecord>, ErrorData> {
    let mut records = BTreeMap::new();
    for artifact_id in artifact_ids {
        if let Some(record) = read_taint_record(runtime, artifact_kind, artifact_id)? {
            records.insert(artifact_id.clone(), record);
        }
    }
    Ok(records)
}

fn taint_key(artifact_kind: &str, artifact_id: &str) -> Vec<u8> {
    format!("{TAINT_PREFIX}{artifact_kind}/{artifact_id}").into_bytes()
}

fn decode_taint_record(
    artifact_kind: &str,
    artifact_id: &str,
    value: &[u8],
) -> Result<HygieneTaintRecord, ErrorData> {
    decode_json::<HygieneTaintRecord>(value).map_err(|error| {
        mcp_error(
            error.code(),
            format!("HYGIENE_TAINT_DECODE_FAILED for {artifact_kind}/{artifact_id}: {error}"),
        )
    })
}

/// `(flag-ts → impacted episode ids, impacted episodes by id, rows scanned)`.
type EpisodeImpactIndex = (
    BTreeMap<u64, Vec<String>>,
    BTreeMap<String, HygieneImpactedEpisode>,
    u64,
);

/// `(impacted episode id → routine ids, impacted routines by id, rows scanned)`.
type RoutineImpactIndex = (
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, RoutineRecord>,
    u64,
);

/// `(impacted candidates by id, routine id → candidate ids, episode id →
/// candidate ids, rows scanned)`.
type CandidateImpactIndex = (
    BTreeMap<String, HygieneImpactedAuthoringCandidate>,
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
    u64,
);

#[derive(Clone, Copy)]
enum ImpactScanCaller {
    Report,
    Invalidate,
}

impl ImpactScanCaller {
    fn episode_budget_error(self) -> ErrorData {
        let message = match self {
            Self::Report => format!(
                "HYGIENE_REPORT_EPISODE_SCAN_BUDGET_EXHAUSTED after \
                 {MAX_REPORT_EPISODE_SCAN_ROWS} CF_EPISODES rows; narrow source_key_hex or \
                 time_range — a truncated derivation would under-report poisoned state"
            ),
            Self::Invalidate => format!(
                "HYGIENE_INVALIDATE_EPISODE_SCAN_BUDGET_EXHAUSTED after \
                 {MAX_REPORT_EPISODE_SCAN_ROWS} CF_EPISODES rows; a truncated derivation would \
                 under-taint poisoned state"
            ),
        };
        mcp_error(error_codes::STORAGE_READ_FAILED, message)
    }

    fn routine_budget_error(self) -> ErrorData {
        let message = match self {
            Self::Report => format!(
                "HYGIENE_REPORT_ROUTINE_SCAN_BUDGET_EXHAUSTED after \
                 {MAX_REPORT_ROUTINE_SCAN_ROWS} CF_ROUTINES rows; the routine store should \
                 hold at most a few hundred rows — inspect CF_ROUTINES"
            ),
            Self::Invalidate => format!(
                "HYGIENE_INVALIDATE_ROUTINE_SCAN_BUDGET_EXHAUSTED after \
                 {MAX_REPORT_ROUTINE_SCAN_ROWS} CF_ROUTINES rows"
            ),
        };
        mcp_error(error_codes::STORAGE_READ_FAILED, message)
    }

    fn routine_key_error(self, key: &[u8], error: impl std::fmt::Display) -> ErrorData {
        let message = match self {
            Self::Report => format!(
                "HYGIENE_REPORT_ROUTINE_KEY_INVALID in CF_ROUTINES at {}: {error}",
                hex_encode(key)
            ),
            Self::Invalidate => format!(
                "HYGIENE_INVALIDATE_ROUTINE_KEY_INVALID in CF_ROUTINES at {}: {error}",
                hex_encode(key)
            ),
        };
        mcp_error(error_codes::STORAGE_READ_FAILED, message)
    }

    fn routine_decode_error(
        self,
        key: &[u8],
        code: &'static str,
        error: impl std::fmt::Display,
    ) -> ErrorData {
        let message = match self {
            Self::Report => format!(
                "HYGIENE_REPORT_ROUTINE_ROW_DECODE_FAILED in CF_ROUTINES at {}: {error}",
                hex_encode(key)
            ),
            Self::Invalidate => format!(
                "HYGIENE_INVALIDATE_ROUTINE_ROW_DECODE_FAILED in CF_ROUTINES at {}: {error}",
                hex_encode(key)
            ),
        };
        mcp_error(code, message)
    }

    fn authoring_budget_error(self) -> ErrorData {
        let message = match self {
            Self::Report => format!(
                "HYGIENE_REPORT_AUTHORING_CANDIDATE_SCAN_BUDGET_EXHAUSTED after \
                 {MAX_REPORT_AUTHORING_CANDIDATE_SCAN_ROWS} CF_PROFILES candidate rows; \
                 a truncated derivation would hide poisoned installable artifacts"
            ),
            Self::Invalidate => format!(
                "HYGIENE_INVALIDATE_AUTHORING_CANDIDATE_SCAN_BUDGET_EXHAUSTED after \
                 {MAX_REPORT_AUTHORING_CANDIDATE_SCAN_ROWS} CF_PROFILES candidate rows"
            ),
        };
        mcp_error(error_codes::STORAGE_READ_FAILED, message)
    }

    fn authoring_decode_error(
        self,
        key: &[u8],
        code: &'static str,
        error: impl std::fmt::Display,
    ) -> ErrorData {
        let message = match self {
            Self::Report => format!(
                "HYGIENE_REPORT_AUTHORING_CANDIDATE_ROW_DECODE_FAILED in CF_PROFILES at {}: {error}",
                hex_encode(key)
            ),
            Self::Invalidate => format!(
                "HYGIENE_INVALIDATE_AUTHORING_CANDIDATE_ROW_DECODE_FAILED in CF_PROFILES at {}: {error}",
                hex_encode(key)
            ),
        };
        mcp_error(code, message)
    }
}

/// One bounded `CF_EPISODES` scan: an episode is impacted iff its inclusive time
/// window contains a flagged timestamp.
fn scan_impacted_episodes(
    runtime: &ReflexRuntime,
    ts_set: &BTreeSet<u64>,
    caller: ImpactScanCaller,
) -> Result<EpisodeImpactIndex, ErrorData> {
    let mut scanned = 0_u64;
    let mut ts_to_episode_ids: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    let mut episodes_by_id: BTreeMap<String, HygieneImpactedEpisode> = BTreeMap::new();
    let (Some(&min_ts), Some(&max_ts)) = (ts_set.iter().next(), ts_set.iter().next_back()) else {
        return Ok((ts_to_episode_ids, episodes_by_id, scanned));
    };
    let mut start = episode_codec::episode_scan_start(min_ts.saturating_sub(DAY_NS));
    'episodes: loop {
        if usize::try_from(scanned).unwrap_or(usize::MAX) >= MAX_REPORT_EPISODE_SCAN_ROWS {
            return Err(caller.episode_budget_error());
        }
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_EPISODES, &start, STORAGE_SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        let mut last = None;
        for (key, value) in &rows {
            scanned += 1;
            last = Some(key.clone());
            let (_key_ts_ns, _ordinal, record) = decode_episode_row(key, value)?;
            if record.start_ts_ns > max_ts {
                break 'episodes;
            }
            let contained: Vec<u64> = ts_set
                .range(record.start_ts_ns..=record.end_ts_ns)
                .copied()
                .collect();
            if contained.is_empty() {
                continue;
            }
            for ts in contained {
                ts_to_episode_ids
                    .entry(ts)
                    .or_default()
                    .push(record.episode_id.clone());
            }
            episodes_by_id
                .entry(record.episode_id.clone())
                .or_insert_with(|| HygieneImpactedEpisode {
                    episode_id: record.episode_id.clone(),
                    start_ts_ns: record.start_ts_ns,
                    end_ts_ns: record.end_ts_ns,
                    actor: actor_label(&record.actor),
                    app: record.app.clone(),
                    document: record.document.clone(),
                    tainted: false,
                    taint: None,
                });
        }
        if !more {
            break;
        }
        let Some(last) = last else { break };
        start = key_after(&last);
    }
    Ok((ts_to_episode_ids, episodes_by_id, scanned))
}

/// One bounded `CF_ROUTINES` scan: a routine is impacted iff its evidence
/// episode ids intersect the impacted episode ids.
fn scan_impacted_routines(
    runtime: &ReflexRuntime,
    impacted_episode_ids: &BTreeSet<String>,
    caller: ImpactScanCaller,
) -> Result<RoutineImpactIndex, ErrorData> {
    let mut scanned = 0_u64;
    let mut routines_by_id: BTreeMap<String, RoutineRecord> = BTreeMap::new();
    let mut episode_to_routine_ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if impacted_episode_ids.is_empty() {
        return Ok((episode_to_routine_ids, routines_by_id, scanned));
    }
    let mut start: Vec<u8> = Vec::new();
    loop {
        if usize::try_from(scanned).unwrap_or(usize::MAX) >= MAX_REPORT_ROUTINE_SCAN_ROWS {
            return Err(caller.routine_budget_error());
        }
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_ROUTINES, &start, STORAGE_SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        let mut last = None;
        for (key, value) in &rows {
            scanned += 1;
            last = Some(key.clone());
            routine_codec::decode_routine_key(key)
                .map_err(|error| caller.routine_key_error(key, error))?;
            let record = decode_json::<RoutineRecord>(value).map_err(|error| {
                let code = error.code();
                caller.routine_decode_error(key, code, error)
            })?;
            let mut linked: BTreeSet<String> = BTreeSet::new();
            for evidence in &record.evidence {
                for episode_id in &evidence.episode_ids {
                    if impacted_episode_ids.contains(episode_id) {
                        linked.insert(episode_id.clone());
                    }
                }
            }
            if linked.is_empty() {
                continue;
            }
            for episode_id in &linked {
                episode_to_routine_ids
                    .entry(episode_id.clone())
                    .or_default()
                    .insert(record.routine_id.clone());
            }
            routines_by_id.insert(record.routine_id.clone(), record);
        }
        if !more {
            break;
        }
        let Some(last) = last else { break };
        start = key_after(&last);
    }
    Ok((episode_to_routine_ids, routines_by_id, scanned))
}

/// One bounded `CF_PROFILES` candidate scan: a candidate is impacted iff its
/// JSON references an impacted routine or episode id.
fn scan_impacted_candidates(
    runtime: &ReflexRuntime,
    impacted_routine_ids: &BTreeSet<String>,
    impacted_episode_ids: &BTreeSet<String>,
    caller: ImpactScanCaller,
) -> Result<CandidateImpactIndex, ErrorData> {
    let mut scanned = 0_u64;
    let mut authoring_candidates_by_id: BTreeMap<String, HygieneImpactedAuthoringCandidate> =
        BTreeMap::new();
    let mut routine_to_authoring_candidate_ids: BTreeMap<String, BTreeSet<String>> =
        BTreeMap::new();
    let mut episode_to_authoring_candidate_ids: BTreeMap<String, BTreeSet<String>> =
        BTreeMap::new();
    if impacted_routine_ids.is_empty() && impacted_episode_ids.is_empty() {
        return Ok((
            authoring_candidates_by_id,
            routine_to_authoring_candidate_ids,
            episode_to_authoring_candidate_ids,
            scanned,
        ));
    }
    let prefix = PROFILE_AUTHORING_CANDIDATE_PREFIX.as_bytes();
    let mut start = prefix.to_vec();
    loop {
        if usize::try_from(scanned).unwrap_or(usize::MAX)
            >= MAX_REPORT_AUTHORING_CANDIDATE_SCAN_ROWS
        {
            return Err(caller.authoring_budget_error());
        }
        let rows = runtime
            .storage_cf_prefix_rows_from(cf::CF_PROFILES, prefix, &start, STORAGE_SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        let rows_len = rows.len();
        let mut last = None;
        for (key, value) in rows {
            scanned += 1;
            last = Some(key.clone());
            let candidate = decode_json::<ProfileAuthoringCandidate>(&value).map_err(|error| {
                let code = error.code();
                caller.authoring_decode_error(&key, code, error)
            })?;
            let references = candidate_reference_strings(&candidate)?;
            let via_routine_ids: Vec<String> = impacted_routine_ids
                .iter()
                .filter(|routine_id| references.contains(*routine_id))
                .cloned()
                .collect();
            let via_episode_ids: Vec<String> = impacted_episode_ids
                .iter()
                .filter(|episode_id| references.contains(*episode_id))
                .cloned()
                .collect();
            if via_routine_ids.is_empty() && via_episode_ids.is_empty() {
                continue;
            }
            let candidate_id = candidate.candidate_id.clone();
            for routine_id in &via_routine_ids {
                routine_to_authoring_candidate_ids
                    .entry(routine_id.clone())
                    .or_default()
                    .insert(candidate_id.clone());
            }
            for episode_id in &via_episode_ids {
                episode_to_authoring_candidate_ids
                    .entry(episode_id.clone())
                    .or_default()
                    .insert(candidate_id.clone());
            }
            authoring_candidates_by_id.insert(
                candidate_id,
                HygieneImpactedAuthoringCandidate {
                    candidate_id: candidate.candidate_id,
                    profile_id: candidate.profile_id,
                    state: candidate.state,
                    generated_at_ns: candidate.generated_at_ns,
                    updated_at_ns: candidate.updated_at_ns,
                    accepted_at_ns: candidate.accepted_at_ns,
                    rejected_at_ns: candidate.rejected_at_ns,
                    via_routine_ids,
                    via_episode_ids,
                    tainted: false,
                    taint: None,
                },
            );
        }
        if rows_len < STORAGE_SCAN_CHUNK_ROWS {
            break;
        }
        let Some(last) = last else { break };
        start = key_after(&last);
    }
    Ok((
        authoring_candidates_by_id,
        routine_to_authoring_candidate_ids,
        episode_to_authoring_candidate_ids,
        scanned,
    ))
}

fn now_ns() -> u64 {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    u64::try_from(nanos).unwrap_or(0)
}

#[must_use]
pub fn scan_perceived_text(
    source_path: impl Into<String>,
    text: &str,
) -> Vec<SuspectedInjectionAnnotation> {
    let source_path = source_path.into();
    scan_text(text, DEFAULT_MIN_SCORE)
        .into_iter()
        .map(|item| SuspectedInjectionAnnotation {
            source_path: source_path.clone(),
            span: SuspectedInjectionSpan {
                start: item.span_start,
                end: item.span_end,
                text: item.span_text,
                text_sha256: item.span_text_sha256,
            },
            score: item.score,
            heuristics: item.heuristics,
            evidence: item.evidence,
        })
        .collect()
}

pub fn scan_and_persist_observation(
    runtime: &ReflexRuntime,
    observation: &StoredObservation,
    source_key: &[u8],
) -> Result<u64, ErrorData> {
    let candidates = observation_text_candidates(observation);
    scan_and_persist_candidates(runtime, SOURCE_CF_OBSERVATIONS, source_key, candidates)
}

pub fn scan_and_persist_ocr_result(
    runtime: &ReflexRuntime,
    result: &OcrResult,
    source_key: &[u8],
) -> Result<u64, ErrorData> {
    let mut candidates = Vec::new();
    if !result.full_text.trim().is_empty() {
        candidates.push(TextCandidate {
            field: "/result/full_text".to_owned(),
            text: result.full_text.clone(),
        });
    }
    for (index, word) in result.words.iter().enumerate() {
        if !word.text.trim().is_empty() {
            candidates.push(TextCandidate {
                field: format!("/result/words/{index}/text"),
                text: word.text.clone(),
            });
        }
    }
    scan_and_persist_candidates(runtime, SOURCE_CF_OCR_CACHE, source_key, candidates)
}

fn scan_and_persist_candidates(
    runtime: &ReflexRuntime,
    source_cf: &str,
    source_key: &[u8],
    candidates: Vec<TextCandidate>,
) -> Result<u64, ErrorData> {
    let source_key_hex = hex_encode(source_key);
    let mut records = Vec::new();
    for candidate in candidates {
        let matches = scan_text(&candidate.text, DEFAULT_MIN_SCORE);
        records.extend(records_for_matches(
            source_cf,
            &source_key_hex,
            &candidate.field,
            &candidate.text,
            &matches,
        ));
    }
    let (written, _readback) = write_flag_records(runtime, records)?;
    Ok(written)
}

fn scan_text(text: &str, min_score: u32) -> Vec<HygieneTextMatch> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = NormalizedText::new(text);
    let mut matches = Vec::new();
    for pattern in SUSPICIOUS_PATTERNS {
        add_pattern_matches(text, &normalized, *pattern, &mut matches);
    }
    add_role_marker_matches(text, &normalized, &mut matches);
    add_zero_width_low_signal_matches(text, &mut matches);
    matches.retain(|item| item.score >= min_score);
    matches.sort_by(|left, right| {
        left.span_start
            .cmp(&right.span_start)
            .then(left.span_end.cmp(&right.span_end))
            .then(right.score.cmp(&left.score))
    });
    matches
}

fn add_pattern_matches(
    original: &str,
    normalized: &NormalizedText,
    pattern: SuspiciousPattern,
    matches: &mut Vec<HygieneTextMatch>,
) {
    let mut search_from = 0;
    while let Some(relative) = normalized.text[search_from..].find(pattern.needle) {
        let start = search_from + relative;
        let end = start + pattern.needle.len();
        if let Some((span_start, span_end)) = normalized.original_span(start, end) {
            let mut heuristics = vec![pattern.heuristic.to_owned()];
            let mut evidence = vec![format!("pattern:{}", pattern.needle)];
            let span = safe_slice(original, span_start, span_end);
            let mut score = pattern.score;
            if is_obfuscated_span(span) {
                heuristics.push("obfuscation".to_owned());
                evidence.push("normalized_match".to_owned());
                score = score.saturating_add(10).min(MAX_SCORE);
            }
            add_match(
                matches, original, span_start, span_end, score, heuristics, evidence,
            );
        }
        search_from = start.saturating_add(1);
    }
}

fn add_role_marker_matches(
    original: &str,
    normalized: &NormalizedText,
    matches: &mut Vec<HygieneTextMatch>,
) {
    let mut line_start = 0;
    for line in normalized.text.split_inclusive('\n') {
        let leading = line.len().saturating_sub(line.trim_start().len());
        let marker_start = line_start + leading;
        let tail = &normalized.text[marker_start..];
        for marker in ROLE_MARKERS {
            if tail.starts_with(marker) {
                let marker_end = marker_start + marker.len();
                if let Some((span_start, span_end)) =
                    normalized.original_span(marker_start, marker_end)
                {
                    add_match(
                        matches,
                        original,
                        span_start,
                        span_end,
                        60,
                        vec!["role_marker".to_owned()],
                        vec![format!("line_prefix:{marker}")],
                    );
                }
            }
        }
        line_start += line.len();
    }
}

fn add_zero_width_low_signal_matches(original: &str, matches: &mut Vec<HygieneTextMatch>) {
    for (index, ch) in original.char_indices() {
        if is_zero_width(ch) {
            add_match(
                matches,
                original,
                index,
                index + ch.len_utf8(),
                45,
                vec!["zero_width_obfuscation".to_owned()],
                vec![format!("codepoint:U+{:04X}", ch as u32)],
            );
        }
    }
}

fn add_match(
    matches: &mut Vec<HygieneTextMatch>,
    original: &str,
    span_start: usize,
    span_end: usize,
    score: u32,
    heuristics: Vec<String>,
    evidence: Vec<String>,
) {
    let span_start_u32 = u32::try_from(span_start).unwrap_or(u32::MAX);
    let span_end_u32 = u32::try_from(span_end).unwrap_or(u32::MAX);
    if let Some(existing) = matches
        .iter_mut()
        .find(|item| item.span_start == span_start_u32 && item.span_end == span_end_u32)
    {
        existing.score = existing.score.max(score);
        merge_unique(&mut existing.heuristics, heuristics);
        merge_unique(&mut existing.evidence, evidence);
        return;
    }
    let span_text = safe_slice(original, span_start, span_end).to_owned();
    matches.push(HygieneTextMatch {
        span_start: span_start_u32,
        span_end: span_end_u32,
        span_text_sha256: sha256_hex(span_text.as_bytes()),
        span_text,
        score,
        heuristics,
        evidence,
    });
}

fn merge_unique(target: &mut Vec<String>, additions: Vec<String>) {
    let mut seen = target.iter().cloned().collect::<BTreeSet<_>>();
    for addition in additions {
        if seen.insert(addition.clone()) {
            target.push(addition);
        }
    }
}

#[derive(Clone, Copy)]
struct SuspiciousPattern {
    needle: &'static str,
    heuristic: &'static str,
    score: u32,
}

const SUSPICIOUS_PATTERNS: &[SuspiciousPattern] = &[
    SuspiciousPattern {
        needle: "ignore previous instructions",
        heuristic: "instruction_override",
        score: 92,
    },
    SuspiciousPattern {
        needle: "ignore all previous instructions",
        heuristic: "instruction_override",
        score: 95,
    },
    SuspiciousPattern {
        needle: "disregard previous instructions",
        heuristic: "instruction_override",
        score: 92,
    },
    SuspiciousPattern {
        needle: "forget previous instructions",
        heuristic: "instruction_override",
        score: 88,
    },
    SuspiciousPattern {
        needle: "you are now",
        heuristic: "role_rewrite",
        score: 72,
    },
    SuspiciousPattern {
        needle: "new system prompt",
        heuristic: "role_rewrite",
        score: 78,
    },
    SuspiciousPattern {
        needle: "system prompt",
        heuristic: "system_prompt_reference",
        score: 45,
    },
    SuspiciousPattern {
        needle: "developer message",
        heuristic: "system_prompt_reference",
        score: 45,
    },
    SuspiciousPattern {
        needle: "what is your system prompt",
        heuristic: "system_prompt_exfiltration",
        score: 82,
    },
    SuspiciousPattern {
        needle: "show your system prompt",
        heuristic: "system_prompt_exfiltration",
        score: 82,
    },
    SuspiciousPattern {
        needle: "reveal your system prompt",
        heuristic: "system_prompt_exfiltration",
        score: 84,
    },
    SuspiciousPattern {
        needle: "reveal the system prompt",
        heuristic: "system_prompt_exfiltration",
        score: 84,
    },
    SuspiciousPattern {
        needle: "reveal your instructions",
        heuristic: "system_prompt_exfiltration",
        score: 84,
    },
    SuspiciousPattern {
        needle: "print your instructions",
        heuristic: "system_prompt_exfiltration",
        score: 82,
    },
    SuspiciousPattern {
        needle: "do anything now",
        heuristic: "jailbreak_shape",
        score: 88,
    },
    SuspiciousPattern {
        needle: "dan mode",
        heuristic: "jailbreak_shape",
        score: 82,
    },
    SuspiciousPattern {
        needle: "jailbreak",
        heuristic: "jailbreak_shape",
        score: 76,
    },
    SuspiciousPattern {
        needle: "bypass safety",
        heuristic: "jailbreak_shape",
        score: 78,
    },
    SuspiciousPattern {
        needle: "mcp__",
        heuristic: "tool_call_syntax",
        score: 78,
    },
    SuspiciousPattern {
        needle: "tools/call",
        heuristic: "tool_call_syntax",
        score: 80,
    },
    SuspiciousPattern {
        needle: "\"tool_calls\"",
        heuristic: "tool_call_syntax",
        score: 76,
    },
    SuspiciousPattern {
        needle: "\"function_call\"",
        heuristic: "tool_call_syntax",
        score: 74,
    },
    SuspiciousPattern {
        needle: "<tool_call",
        heuristic: "tool_call_syntax",
        score: 78,
    },
    SuspiciousPattern {
        needle: "assistant to=",
        heuristic: "tool_call_syntax",
        score: 72,
    },
    SuspiciousPattern {
        needle: "begin system prompt",
        heuristic: "role_rewrite",
        score: 82,
    },
    SuspiciousPattern {
        needle: "end system prompt",
        heuristic: "role_rewrite",
        score: 78,
    },
];

const ROLE_MARKERS: &[&str] = &[
    "system:",
    "developer:",
    "assistant:",
    "tool:",
    "[system]",
    "[developer]",
    "<system>",
    "<developer>",
    "### system",
    "### developer",
];

struct NormalizedText {
    text: String,
    start_map: Vec<usize>,
    end_map: Vec<usize>,
}

impl NormalizedText {
    fn new(original: &str) -> Self {
        let mut text = String::with_capacity(original.len());
        let mut start_map = Vec::with_capacity(original.len());
        let mut end_map = Vec::with_capacity(original.len());
        for (index, ch) in original.char_indices() {
            if is_zero_width(ch) {
                continue;
            }
            let end = index + ch.len_utf8();
            let mapped = normalize_char(ch);
            text.push(mapped);
            start_map.push(index);
            end_map.push(end);
        }
        Self {
            text,
            start_map,
            end_map,
        }
    }

    fn original_span(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        if start >= end || end > self.start_map.len() {
            return None;
        }
        Some((self.start_map[start], self.end_map[end - 1]))
    }
}

fn normalize_char(ch: char) -> char {
    if ch.is_ascii() {
        return ch.to_ascii_lowercase();
    }
    confusable_ascii(ch).unwrap_or_else(|| if ch.is_whitespace() { ' ' } else { '?' })
}

fn confusable_ascii(ch: char) -> Option<char> {
    match ch {
        '\u{0430}' | '\u{03B1}' | '\u{FF41}' => Some('a'),
        '\u{0435}' | '\u{03B5}' | '\u{FF45}' => Some('e'),
        '\u{0456}' | '\u{03B9}' | '\u{0131}' | '\u{FF49}' => Some('i'),
        '\u{043E}' | '\u{03BF}' | '\u{FF4F}' => Some('o'),
        '\u{0440}' | '\u{03C1}' | '\u{FF50}' => Some('p'),
        '\u{0441}' | '\u{03F2}' | '\u{FF43}' => Some('c'),
        '\u{0445}' | '\u{03C7}' | '\u{FF58}' => Some('x'),
        '\u{0443}' | '\u{FF59}' => Some('y'),
        '\u{0458}' | '\u{FF4A}' => Some('j'),
        '\u{0455}' | '\u{FF53}' => Some('s'),
        '\u{04CF}' | '\u{FF4C}' => Some('l'),
        _ => None,
    }
}

fn is_zero_width(ch: char) -> bool {
    matches!(
        ch,
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
    )
}

fn is_obfuscated_span(span: &str) -> bool {
    span.chars()
        .any(|ch| is_zero_width(ch) || (!ch.is_ascii() && confusable_ascii(ch).is_some()))
}

#[derive(Clone, Debug)]
struct TextCandidate {
    field: String,
    text: String,
}

fn observation_text_candidates(observation: &StoredObservation) -> Vec<TextCandidate> {
    let mut candidates = Vec::new();
    push_candidate(
        &mut candidates,
        "/foreground/window_title",
        &observation.foreground.window_title,
    );
    if let Some(focused) = &observation.focused {
        push_candidate(&mut candidates, "/focused/name", &focused.name);
        if let Some(value) = &focused.value {
            push_candidate(&mut candidates, "/focused/value", value);
        }
        if let Some(selected_text) = &focused.selected_text {
            push_candidate(&mut candidates, "/focused/selected_text", selected_text);
        }
    }
    for (index, element) in observation.elements.iter().enumerate() {
        push_candidate(
            &mut candidates,
            format!("/elements/{index}/name"),
            &element.name,
        );
        if let Some(value) = &element.value {
            push_candidate(&mut candidates, format!("/elements/{index}/value"), value);
        }
    }
    for (name, reading) in &observation.hud.by_name {
        push_candidate(
            &mut candidates,
            format!("/hud/by_name/{}/raw_text", escape_json_pointer(name)),
            &reading.raw_text,
        );
    }
    if let Some(clipboard) = &observation.clipboard_summary
        && let Some(text_excerpt) = &clipboard.text_excerpt
    {
        push_candidate(
            &mut candidates,
            "/clipboard_summary/text_excerpt",
            text_excerpt,
        );
    }
    for (index, fs_event) in observation.fs_recent.iter().enumerate() {
        push_candidate(
            &mut candidates,
            format!("/fs_recent/{index}/path"),
            &fs_event.path,
        );
    }
    for (index, event) in observation.recent_events.iter().enumerate() {
        collect_value_strings(
            &event.data_excerpt,
            &format!("/recent_events/{index}/data_excerpt"),
            &mut candidates,
        );
    }
    candidates
}

fn timeline_text_candidates(record: &TimelineRecord) -> Vec<TextCandidate> {
    let mut candidates = Vec::new();
    if let Some(app) = &record.app {
        push_candidate(&mut candidates, "/app", app);
    }
    if let TimelineActor::Agent { session_id } = &record.actor {
        push_candidate(&mut candidates, "/actor/session_id", session_id);
    }
    collect_value_strings(&record.payload, "/payload", &mut candidates);
    candidates
}

fn collect_value_strings(value: &Value, path: &str, candidates: &mut Vec<TextCandidate>) {
    match value {
        Value::String(text) => push_candidate(candidates, path, text),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_value_strings(item, &format!("{path}/{index}"), candidates);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                collect_value_strings(
                    item,
                    &format!("{path}/{}", escape_json_pointer(key)),
                    candidates,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn push_candidate(candidates: &mut Vec<TextCandidate>, field: impl Into<String>, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    candidates.push(TextCandidate {
        field: field.into(),
        text: text.to_owned(),
    });
}

fn escape_json_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn records_for_matches(
    source_cf: &str,
    source_key_hex: &str,
    source_field: &str,
    source_text: &str,
    matches: &[HygieneTextMatch],
) -> Vec<HygieneFlagRecord> {
    let source_text_sha256 = sha256_hex(source_text.as_bytes());
    matches
        .iter()
        .map(|item| {
            let flag_id = flag_id(
                source_cf,
                source_key_hex,
                source_field,
                item.span_start,
                item.span_end,
                &item.heuristics,
            );
            HygieneFlagRecord {
                schema_version: SCHEMA_VERSION,
                flag_id,
                detected_at: Utc::now(),
                source_cf: source_cf.to_owned(),
                source_key_hex: source_key_hex.to_owned(),
                source_field: source_field.to_owned(),
                source_text_sha256: source_text_sha256.clone(),
                span_start: item.span_start,
                span_end: item.span_end,
                span_text: item.span_text.clone(),
                span_text_sha256: item.span_text_sha256.clone(),
                score: item.score,
                heuristics: item.heuristics.clone(),
                evidence: item.evidence.clone(),
            }
        })
        .collect()
}

fn write_flag_records(
    runtime: &ReflexRuntime,
    records: Vec<HygieneFlagRecord>,
) -> Result<(u64, Vec<HygieneStoredFlag>), ErrorData> {
    if records.is_empty() {
        return Ok((0, Vec::new()));
    }
    if !runtime.storage_pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "hygiene flag write refused under disk pressure: cf_name={}",
                cf::CF_KV
            ),
        ));
    }
    let rows = records
        .iter()
        .map(|record| {
            let key = flag_key(record);
            let value = encode_json(record).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("hygiene flag encode failed for {}: {error}", record.flag_id),
                )
            })?;
            Ok::<(Vec<u8>, Vec<u8>), ErrorData>((key, value))
        })
        .collect::<Result<Vec<_>, _>>()?;
    runtime
        .storage_put_kv_rows(rows.clone())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let mut readbacks = Vec::new();
    for (key, _value) in rows {
        let readback_rows = runtime
            .storage_cf_prefix_rows(cf::CF_KV, &key, 1)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let Some((readback_key, readback_value)) = readback_rows
            .into_iter()
            .find(|(readback_key, _value)| readback_key == &key)
        else {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "hygiene flag write had no readback row: key_hex={}",
                    hex_encode(&key)
                ),
            ));
        };
        let record = decode_json::<HygieneFlagRecord>(&readback_value).map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "hygiene flag readback decode failed for key_hex={}: {error}",
                    hex_encode(&readback_key)
                ),
            )
        })?;
        readbacks.push(HygieneStoredFlag {
            kv_key_hex: hex_encode(&readback_key),
            record,
        });
    }
    Ok((readbacks.len() as u64, readbacks))
}

fn ensure_source_row_exists(
    runtime: &ReflexRuntime,
    source_cf: &str,
    source_key: &[u8],
) -> Result<(), ErrorData> {
    let rows = runtime
        .storage_cf_prefix_rows(source_cf, source_key, 1)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    if rows.into_iter().any(|(key, _value)| key == source_key) {
        return Ok(());
    }
    Err(invalid(format!(
        "hygiene_scan_text persist=true source row not found: source_cf={source_cf} source_key_hex={}",
        hex_encode(source_key)
    )))
}

fn flag_id(
    source_cf: &str,
    source_key_hex: &str,
    source_field: &str,
    span_start: u32,
    span_end: u32,
    heuristics: &[String],
) -> String {
    let mut material = String::new();
    material.push_str(source_cf);
    material.push('\0');
    material.push_str(source_key_hex);
    material.push('\0');
    material.push_str(source_field);
    material.push('\0');
    material.push_str(&span_start.to_string());
    material.push('\0');
    material.push_str(&span_end.to_string());
    material.push('\0');
    material.push_str(&heuristics.join(","));
    sha256_hex(material.as_bytes())
}

fn flag_key(record: &HygieneFlagRecord) -> Vec<u8> {
    format!(
        "{FLAG_PREFIX}{}/{}/{}",
        record.source_cf, record.source_key_hex, record.flag_id
    )
    .into_bytes()
}

fn flag_prefix(source_cf: Option<&str>, source_key_hex: Option<&str>) -> String {
    match (source_cf, source_key_hex) {
        (Some(source_cf), Some(source_key_hex)) => {
            format!("{FLAG_PREFIX}{source_cf}/{source_key_hex}/")
        }
        (Some(source_cf), None) => format!("{FLAG_PREFIX}{source_cf}/"),
        (None, None) => FLAG_PREFIX.to_owned(),
        (None, Some(_)) => FLAG_PREFIX.to_owned(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StorageCursor {
    source_cf: String,
    key: Vec<u8>,
}

fn parse_storage_cursor(raw: Option<&str>) -> Result<Option<StorageCursor>, ErrorData> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let Some((source_cf, key_hex)) = raw.split_once(':') else {
        return Err(invalid(
            "hygiene_scan_storage cursor must be formatted as source_cf:key_hex",
        ));
    };
    let source_cf = normalize_source_cf(source_cf, false)?;
    let key = hex_decode(key_hex)
        .ok_or_else(|| invalid("hygiene_scan_storage cursor key_hex is not valid hex"))?;
    Ok(Some(StorageCursor { source_cf, key }))
}

fn validate_source_cfs(raw: Option<&[String]>) -> Result<Vec<String>, ErrorData> {
    let mut output = Vec::new();
    match raw {
        None => {
            output.push(SOURCE_CF_OBSERVATIONS.to_owned());
            output.push(SOURCE_CF_TIMELINE.to_owned());
        }
        Some(values) => {
            if values.is_empty() {
                return Err(invalid("hygiene_scan_storage source_cfs must not be empty"));
            }
            for value in values {
                let cf = normalize_source_cf(value, false)?;
                if !output.contains(&cf) {
                    output.push(cf);
                }
            }
        }
    }
    Ok(output)
}

fn normalize_source_cf(raw: &str, include_ocr: bool) -> Result<String, ErrorData> {
    let normalized = raw.trim().to_ascii_uppercase();
    match normalized.as_str() {
        "OBSERVATIONS" | SOURCE_CF_OBSERVATIONS => Ok(SOURCE_CF_OBSERVATIONS.to_owned()),
        "TIMELINE" | SOURCE_CF_TIMELINE => Ok(SOURCE_CF_TIMELINE.to_owned()),
        "OCR" | "OCR_CACHE" | SOURCE_CF_OCR_CACHE if include_ocr => {
            Ok(SOURCE_CF_OCR_CACHE.to_owned())
        }
        _ => Err(invalid(format!(
            "unsupported hygiene source_cf {raw:?}; expected CF_OBSERVATIONS{} or CF_TIMELINE",
            if include_ocr { ", CF_OCR_CACHE," } else { "" }
        ))),
    }
}

fn validate_min_score(score: Option<u32>, default: u32, tool_name: &str) -> Result<u32, ErrorData> {
    let score = score.unwrap_or(default);
    if score > MAX_SCORE {
        return Err(invalid(format!(
            "{tool_name} min_score must be between 0 and {MAX_SCORE}; got {score}"
        )));
    }
    Ok(score)
}

fn validate_limit(value: u32, min: u32, max: u32, name: &str) -> Result<u32, ErrorData> {
    if value < min || value > max {
        return Err(invalid(format!(
            "{name} must be between {min} and {max}; got {value}"
        )));
    }
    Ok(value)
}

fn validate_hex_text(value: &str, field: &str) -> Result<(), ErrorData> {
    if value.is_empty()
        || !value.len().is_multiple_of(2)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid(format!(
            "{field} must be non-empty even-length hex"
        )));
    }
    Ok(())
}

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
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

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks_exact(2) {
        let hi = hex_value(pair[0])?;
        let lo = hex_value(pair[1])?;
        bytes.push((hi << 4) | lo);
    }
    Some(bytes)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

fn safe_slice(text: &str, start: usize, end: usize) -> &str {
    text.get(start..end).unwrap_or_default()
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn lock_runtime(
    runtime: &Arc<Mutex<ReflexRuntime>>,
) -> Result<MutexGuard<'_, ReflexRuntime>, ErrorData> {
    runtime.lock().map_err(|_error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "reflex runtime lock poisoned while running hygiene scanner",
        )
    })
}

fn invalid(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.into())
}

/// Scheduled whole-vault verification request (#1687 + #1679).
///
/// Both knobs default to the routine posture: verify the live vault with an
/// incremental tail scan of the provenance chain. A full re-hash of the whole
/// Ledger CF is available but must be asked for, because a routine check that is
/// too expensive to run on a schedule stops being run at all.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneVaultVerifyParams {
    /// Re-hash the entire Ledger CF instead of the recent window.
    #[serde(default)]
    pub full_chain: bool,
    /// Size of the incremental window, in ledger entries. Ignored when
    /// `full_chain` is set. Defaults to
    /// `synapse_calyx::VAULT_VERIFY_DEFAULT_TAIL_ENTRIES`.
    #[serde(default)]
    pub tail_entries: Option<u64>,
}

/// Physical verdict of one scheduled vault verification.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneVaultVerifyResponse {
    pub source_of_truth: &'static str,
    pub vault_dir: String,
    pub vault_id: String,
    /// True only when every checked surface verified.
    pub green: bool,
    /// `incremental_tail` | `full_chain`.
    pub scan_mode: String,
    pub tail_entries: u64,
    pub ledger_head_height: u64,
    pub verified_from_seq: u64,
    pub verified_to_seq: u64,
    pub chain_verdict: String,
    pub chain_entry_count: u64,
    pub ledger_tip_hash: String,
    pub restore_success: bool,
    pub chain_intact: bool,
    pub raw_commitments_intact: bool,
    /// False when the lineage journal that survives deleting the vault is gone.
    pub lineage_present: bool,
    pub lineage_path: String,
    pub vault_generation: u64,
    pub vault_reset_count: u64,
    pub chain_origin: String,
    /// Reported, never alarmed on: an intact chain that begins after a seeded
    /// journal or an acknowledged reset is intact and partial at once (#1884).
    /// `history_coverage` names which, and `attested_from_seq` names where.
    pub covers_full_history: bool,
    pub history_coverage: String,
    /// `latest_seq` when this generation was recorded, in its own numbering — 0
    /// for both a genesis vault and an emptied replacement, so it is not a
    /// coverage proxy. See `history_coverage`.
    pub attested_from_seq: u64,
    pub constellation_count: u64,
    pub anchor_count: u64,
    pub ledger_entry_count: u64,
    pub wal_bytes_present: u64,
    pub failure_reasons: Vec<String>,
}

/// Upper bound on the incremental window, so a "tail" request cannot silently
/// become a full-vault re-hash that was never authorised.
const MAX_VAULT_VERIFY_TAIL_ENTRIES: u64 = 1_000_000;

#[must_use]
pub fn required_permissions_vault_verify(
    _params: &HygieneVaultVerifyParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

pub fn required_permissions_anneal_status() -> RequiredPermissions {
    required([Permission::ReadStorage])
}

pub fn required_permissions_anneal_mutation() -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

/// Verifies the live vault and fails closed on any non-green surface.
///
/// Delegates to the existing `verify_vault_restore` and `verify_ledger_chain`
/// paths — nothing is re-implemented here — and converts a non-green verdict
/// into `SYNAPSE_HYGIENE_VAULT_VERIFY_FAILED` naming exactly what failed. A
/// scheduled verification that returned a cheerful report on a broken vault
/// would be worse than no verification at all.
///
/// # Errors
///
/// Returns a structured error when the vault cannot be read, when the vault
/// maintenance guard is already held by a backup/erase/compaction pass, or when
/// the verdict is not green.
pub fn run_vault_verify(
    db: &Db,
    params: &HygieneVaultVerifyParams,
) -> Result<HygieneVaultVerifyResponse, ErrorData> {
    let tail_entries = params
        .tail_entries
        .unwrap_or(synapse_calyx::VAULT_VERIFY_DEFAULT_TAIL_ENTRIES)
        .clamp(1, MAX_VAULT_VERIFY_TAIL_ENTRIES);
    let report = db
        .verify_calyx_vault(params.full_chain, tail_entries)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let response = HygieneVaultVerifyResponse {
        source_of_truth: "live Calyx vault SST/WAL bytes + physical CF_LEDGER hash chain + vault \
                          lineage journal",
        vault_dir: report.vault_dir.display().to_string(),
        vault_id: report.vault_id.clone(),
        green: report.green(),
        scan_mode: report.scan_mode.clone(),
        tail_entries: report.requested_tail_entries,
        ledger_head_height: report.ledger_head_height,
        verified_from_seq: report.chain.verified_from_seq,
        verified_to_seq: report.chain.verified_to_seq,
        chain_verdict: report.chain.verdict.clone(),
        chain_entry_count: report.chain.entry_count,
        ledger_tip_hash: report.chain.tip_hash.clone().unwrap_or_default(),
        restore_success: report.restore.success,
        chain_intact: report.chain.intact,
        raw_commitments_intact: report.chain.raw_commitments_intact,
        lineage_present: report.lineage_present,
        lineage_path: report.lineage_path.display().to_string(),
        vault_generation: report.chain.vault_generation,
        vault_reset_count: report.chain.vault_reset_count,
        chain_origin: report.chain.chain_origin.clone(),
        covers_full_history: report.chain.covers_full_history,
        history_coverage: report.chain.history_coverage.clone(),
        attested_from_seq: report.chain.attested_from_seq,
        constellation_count: report.restore.constellation_count,
        anchor_count: report.restore.anchor_count,
        ledger_entry_count: report.restore.ledger_entry_count,
        wal_bytes_present: report.restore.wal_bytes_present,
        failure_reasons: report.failure_reasons(),
    };
    if response.green {
        return Ok(response);
    }
    Err(mcp_error(
        error_codes::HYGIENE_VAULT_VERIFY_FAILED,
        format!(
            "scheduled vault verification failed for vault_id={} at {}: scan_mode={} \
             verified=[{}..{}) restore_success={} chain_intact={} raw_commitments_intact={} \
             lineage_present={} reasons=[{}]; remediation=stop writers, preserve the vault \
             directory and its lineage journal, and restore from a verified backup before \
             trusting any read from this vault",
            response.vault_id,
            response.vault_dir,
            response.scan_mode,
            response.verified_from_seq,
            response.verified_to_seq,
            response.restore_success,
            response.chain_intact,
            response.raw_commitments_intact,
            response.lineage_present,
            response.failure_reasons.join("; ")
        ),
    ))
}

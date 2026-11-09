//! Suggestion engine (#858, epic #832/#828).
//!
//! The decision layer between intent detection (#854/#855) and the human-facing
//! approval/assist surface (#833). Given the routines the operator appears to be
//! executing right now (the same engine `intent_current` uses), it decides
//! whether to surface a suggestion — and, crucially, when NOT to. The
//! anti-"Clippy" gates are the product, not polish:
//!
//! 1. confidence threshold (default high)
//! 2. feedback suppression / decline cooldown (#856)
//! 3. quiet hours
//! 4. dedup: at most one LIVE suggestion per routine
//! 5. per-routine frequency cap (one per routine per window)
//! 6. global frequency cap (N per rolling window)
//! 7. disabled/archived routines never surface
//!
//! Live suggestions terminate by timeout (→ `ignored_timeout` feedback) or by
//! the routine dropping out of the live intent set (→ `abandoned` feedback),
//! closing the loop back into #856. Accept/decline come from the execution /
//! approval path (#860/#833) and are out of this module's scope.
//!
//! Truth lives in `CF_KV` under `suggestion/v1/`, never daemon memory: a daemon
//! restart re-derives every cap and dedup decision from the persisted rows.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{Local, TimeZone, Timelike};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::error_codes;
use synapse_core::intent::IntentCandidate;
use synapse_core::types::{RoutineFeedbackOutcome, RoutineGranularity, RoutineLifecycle};
use synapse_core::{SCHEMA_VERSION, StoredEvent};
use synapse_storage::{Db, cf, decode_json, encode_json};

use crate::m1::mcp_error;

use super::episodes::{key_after, now_ts_ns};
use super::intent::{IntentCurrentParams, current_intents};
use super::permissions::{Permission, RequiredPermissions, required};
use super::plan::{PlanBackend, PlanDocument, PlanStep, Postcondition};
use super::plan_execution::PlanExecutionRecord;
use super::routines::{
    RoutineFeedbackParams, feedback_suppressed, load_state_row, record_routine_feedback,
};

/// `CF_KV` key prefix for suggestion rows.
const SUGGESTION_PREFIX: &str = "suggestion/v1/";
/// Schema version for [`SuggestionRecord`].
const SUGGESTION_RECORD_VERSION: u32 = 1;
/// `CF_KV` key prefix for suggestion-id to primary-row index entries.
const SUGGESTION_ID_INDEX_PREFIX: &str = "suggestion_id/v1/";
/// Schema version for [`SuggestionIdIndexRecord`].
const SUGGESTION_ID_INDEX_RECORD_VERSION: u32 = 1;
/// The engine actor recorded on feedback it generates.
const SUGGESTION_ACTOR: &str = "suggestion-engine";
const ASSIST_EVENT_KIND: &str = "assist.opportunity";
const ASSIST_ROUTINE_PREFIX: &str = "assist1-";
const DEFAULT_ASSIST_LOOKBACK_SECS: u64 = 900;
const MAX_ASSIST_LOOKBACK_SECS: u64 = 86_400;
const ASSIST_EVENT_SCAN_ROWS: usize = 4_096;
const ASSIST_PLAN_RECORD_VERSION: u32 = 1;

const ENGINE_VERSION_DEFAULTS: &str = "see SuggestionConfig::from_env";

/// Engine knobs. Defaults are deliberately conservative (anti-Clippy);
/// every one is overridable by env so supporting regression checks and manual
/// verification can use deterministic thresholds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SuggestionConfig {
    /// Minimum intent confidence to surface (default 0.6).
    pub min_confidence: f64,
    /// How long a live suggestion stays live before timing out (default 600s).
    pub expiry_secs: u64,
    /// Max suggestions created per rolling global window (default 5).
    pub global_max: u32,
    /// The global rolling window (default 3600s).
    pub global_window_secs: u64,
    /// Minimum spacing between suggestions for the SAME routine (default 4h).
    pub per_routine_window_secs: u64,
    /// Optional quiet-hours window as local minutes-of-day `[start, end)`.
    /// Wraps past midnight when start > end. `None` disables quiet hours.
    pub quiet_hours: Option<(u32, u32)>,
}

impl SuggestionConfig {
    fn env_f64(name: &str, default: f64) -> f64 {
        std::env::var(name)
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(default)
    }
    fn env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(default)
    }
    fn env_u32(name: &str, default: u32) -> u32 {
        std::env::var(name)
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(default)
    }

    #[must_use]
    pub fn from_env() -> Self {
        let quiet_start = std::env::var("SYNAPSE_SUGGEST_QUIET_START_MIN")
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok());
        let quiet_end = std::env::var("SYNAPSE_SUGGEST_QUIET_END_MIN")
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok());
        let quiet_hours = match (quiet_start, quiet_end) {
            (Some(start), Some(end)) if start < 1440 && end < 1440 => Some((start, end)),
            _ => None,
        };
        Self {
            min_confidence: Self::env_f64("SYNAPSE_SUGGEST_MIN_CONFIDENCE", 0.6),
            expiry_secs: Self::env_u64("SYNAPSE_SUGGEST_EXPIRY_SECS", 600),
            global_max: Self::env_u32("SYNAPSE_SUGGEST_GLOBAL_MAX", 5),
            global_window_secs: Self::env_u64("SYNAPSE_SUGGEST_GLOBAL_WINDOW_SECS", 3_600),
            per_routine_window_secs: Self::env_u64(
                "SYNAPSE_SUGGEST_PER_ROUTINE_WINDOW_SECS",
                14_400,
            ),
            quiet_hours,
        }
    }
}

/// Lifecycle of a surfaced suggestion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionStatus {
    /// Surfaced and awaiting the operator.
    Live,
    /// Operator accepted (set by the execution/approval path, #860/#833).
    Accepted,
    /// Operator declined (set by the approval path).
    Declined,
    /// Timed out unanswered.
    Expired,
    /// The routine dropped out of the live intent set before resolution.
    Abandoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionSource {
    RoutineIntent,
    AssistOpportunity,
}

const fn default_suggestion_source() -> SuggestionSource {
    SuggestionSource::RoutineIntent
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssistMitigationStrategy {
    InSessionCorrection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistMitigation {
    pub strategy: AssistMitigationStrategy,
    pub source_event_id: String,
    pub detector: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub target_window_hwnd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_origin: Option<String>,
    pub instruction: String,
    pub postcondition: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub evidence: Value,
}

/// One surfaced suggestion, persisted in `CF_KV`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionRecord {
    pub record_version: u32,
    pub suggestion_id: String,
    pub routine_id: String,
    #[serde(default = "default_suggestion_source")]
    pub source: SuggestionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitigation: Option<AssistMitigation>,
    pub created_ts_ns: u64,
    pub expiry_ts_ns: u64,
    pub status: SuggestionStatus,
    /// Intent confidence at creation (the value the threshold gate saw).
    pub confidence: f64,
    pub matched_prefix_len: u32,
    pub total_steps: u32,
    pub remaining_step_count: u32,
    /// Compiled plan reference (filled by #859 once a plan exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_plan_ref: Option<String>,
    /// When the suggestion left `Live` (expiry/abandon/accept/decline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ts_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_note: Option<String>,
}

/// Secondary index entry for O(1) `suggestion_id` lookups.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestionIdIndexRecord {
    record_version: u32,
    suggestion_id: String,
    routine_id: String,
    created_ts_ns: u64,
    primary_key_hex: String,
    primary_value_sha256: String,
}

/// Why a candidate did NOT surface (or that it did). Ordered by the gate's
/// short-circuit precedence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GateOutcome {
    Surface,
    DisabledRoutine,
    BelowThreshold,
    SuppressedCooldown,
    QuietHours,
    DuplicateLive,
    PerRoutineCap,
    GlobalCap,
}

/// Pre-computed aggregates over existing suggestions, so the gate stays a pure
/// function (unit-testable without storage).
#[derive(Clone, Debug, Default)]
pub struct SuggestionAggregates {
    pub live_routines: BTreeSet<String>,
    /// routine_id → most recent created_ts_ns across ALL statuses.
    pub last_created_by_routine: BTreeMap<String, u64>,
    /// created_ts_ns of every suggestion (any status), for the global window.
    pub created_ts: Vec<u64>,
}

/// Local minute-of-day for `now_ns`, or `None` if the clock is out of range.
#[must_use]
pub fn local_minute_of_day(now_ns: u64) -> Option<u32> {
    let secs = i64::try_from(now_ns / 1_000_000_000).ok()?;
    match Local.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(dt) => Some(dt.hour() * 60 + dt.minute()),
        _ => None,
    }
}

#[must_use]
fn in_quiet_hours(minute: u32, quiet: Option<(u32, u32)>) -> bool {
    match quiet {
        None => false,
        Some((start, end)) if start <= end => minute >= start && minute < end,
        // Wrapping window (e.g. 22:00–07:00).
        Some((start, end)) => minute >= start || minute < end,
    }
}

/// Pure gate: decide whether ONE candidate should surface. `suppressed` is the
/// #856 feedback cooldown verdict; the aggregates supply dedup/cap context.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn gate_decision(
    routine_id: &str,
    confidence: f64,
    lifecycle: RoutineLifecycle,
    suppressed: bool,
    now_ns: u64,
    now_minute: Option<u32>,
    aggregates: &SuggestionAggregates,
    config: &SuggestionConfig,
) -> GateOutcome {
    if matches!(
        lifecycle,
        RoutineLifecycle::Disabled | RoutineLifecycle::Archived
    ) {
        return GateOutcome::DisabledRoutine;
    }
    if confidence < config.min_confidence {
        return GateOutcome::BelowThreshold;
    }
    if suppressed {
        return GateOutcome::SuppressedCooldown;
    }
    if let Some(minute) = now_minute {
        if in_quiet_hours(minute, config.quiet_hours) {
            return GateOutcome::QuietHours;
        }
    }
    if aggregates.live_routines.contains(routine_id) {
        return GateOutcome::DuplicateLive;
    }
    if let Some(last) = aggregates.last_created_by_routine.get(routine_id) {
        if now_ns.saturating_sub(*last)
            < config.per_routine_window_secs.saturating_mul(1_000_000_000)
        {
            return GateOutcome::PerRoutineCap;
        }
    }
    let window_floor =
        now_ns.saturating_sub(config.global_window_secs.saturating_mul(1_000_000_000));
    let global_count = aggregates
        .created_ts
        .iter()
        .filter(|ts| **ts >= window_floor)
        .count();
    if u32::try_from(global_count).unwrap_or(u32::MAX) >= config.global_max {
        return GateOutcome::GlobalCap;
    }
    GateOutcome::Surface
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionTickParams {
    /// Evaluate as of this instant (replay/test). Defaults to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ts_ns: Option<u64>,
    /// Recent-activity lookback handed to the intent matcher (default 6h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_hours: Option<u32>,
    /// Compute the decision for every candidate but persist nothing.
    #[serde(default)]
    pub dry_run: bool,
    /// Include stored ASSIST_OPPORTUNITY detector events in the same gated pass.
    #[serde(default = "default_true")]
    pub include_assist_opportunities: bool,
    /// Recent assist-event lookback. Defaults to 15 minutes, capped at 24h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_lookback_secs: Option<u64>,
}

const fn default_true() -> bool {
    true
}

/// One per-candidate gate decision, echoed for auditability.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GateDecisionRow {
    pub routine_id: String,
    pub source: SuggestionSource,
    pub confidence: f64,
    pub outcome: GateOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionTickResponse {
    pub now_ts_ns: u64,
    pub dry_run: bool,
    pub candidates_evaluated: u32,
    pub created: Vec<String>,
    pub expired: Vec<String>,
    pub abandoned: Vec<String>,
    pub assist_events_scanned: u32,
    pub assist_events_evaluated: u32,
    /// Every candidate's gate decision (created or suppressed-with-reason).
    pub decisions: Vec<GateDecisionRow>,
    pub config: SuggestionConfigEcho,
}

/// Serializable echo of the active config (the opaque struct is not `JsonSchema`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionConfigEcho {
    pub min_confidence: f64,
    pub expiry_secs: u64,
    pub global_max: u32,
    pub global_window_secs: u64,
    pub per_routine_window_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<[u32; 2]>,
}

impl From<SuggestionConfig> for SuggestionConfigEcho {
    fn from(c: SuggestionConfig) -> Self {
        Self {
            min_confidence: c.min_confidence,
            expiry_secs: c.expiry_secs,
            global_max: c.global_max,
            global_window_secs: c.global_window_secs,
            per_routine_window_secs: c.per_routine_window_secs,
            quiet_hours: c.quiet_hours.map(|q| [q.0, q.1]),
        }
    }
}

pub fn required_permissions_tick(_params: &SuggestionTickParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

fn storage_error(error: impl std::fmt::Display) -> ErrorData {
    mcp_error(
        error_codes::STORAGE_READ_FAILED,
        format!("suggestion engine storage failure: {error}"),
    )
}

fn suggestion_key(routine_id: &str, created_ts_ns: u64) -> Vec<u8> {
    format!("{SUGGESTION_PREFIX}{routine_id}/{created_ts_ns:020}").into_bytes()
}

fn suggestion_id_index_key(suggestion_id: &str) -> Vec<u8> {
    let id_hex = hex_encode(suggestion_id.as_bytes());
    format!("{SUGGESTION_ID_INDEX_PREFIX}{id_hex}/primary").into_bytes()
}

fn event_scan_start_key(ts_ns: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&0_u32.to_be_bytes());
    key
}

fn event_key_ts_ns(key: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = key.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
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
    hex_encode(&digest[..])
}

fn sha256_short_hex(material: &str) -> String {
    let digest = Sha256::digest(material.as_bytes());
    hex_encode(&digest[..8])
}

#[derive(Clone, Debug)]
struct AssistOpportunityCandidate {
    routine_id: String,
    source_event_id: String,
    label: String,
    offer: String,
    confidence: f64,
    matched_prefix_len: u32,
    total_steps: u32,
    remaining_step_count: u32,
    mitigation: AssistMitigation,
}

fn assist_lookback_secs(params: &SuggestionTickParams) -> u64 {
    params
        .assist_lookback_secs
        .unwrap_or(DEFAULT_ASSIST_LOOKBACK_SECS)
        .clamp(1, MAX_ASSIST_LOOKBACK_SECS)
}

fn detector_label(detector: &str) -> &'static str {
    match detector {
        "undo_burst" => "undo loop",
        "retype_loop" => "retyping loop",
        "repeated_click_without_state_change" => "repeated click loop",
        "dialog_reopen_loop" => "reopening dialog",
        _ => "interaction struggle",
    }
}

fn detector_offer(detector: &str, process_name: Option<&str>) -> String {
    let label = detector_label(detector);
    let article = if label
        .chars()
        .next()
        .is_some_and(|ch| matches!(ch.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u'))
    {
        "an"
    } else {
        "a"
    };
    match process_name {
        Some(process) if !process.trim().is_empty() => {
            format!(
                "Stuck in {article} {label} in {process}? I can inspect the target and report what can be verified."
            )
        }
        _ => {
            format!(
                "Stuck in {article} {label}? I can inspect the target and report what can be verified."
            )
        }
    }
}

fn assist_candidate_key_material(event: &StoredEvent, detector: &str) -> String {
    let window = event.data.get("window").unwrap_or(&Value::Null);
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        detector,
        window
            .get("hwnd")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        window
            .get("pid")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        window
            .get("process_name")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        window
            .get("focused_element_sha256")
            .and_then(Value::as_str)
            .unwrap_or("window"),
        window
            .get("focused_role")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )
}

fn assist_candidate_from_event(
    event: &StoredEvent,
) -> Result<Option<AssistOpportunityCandidate>, ErrorData> {
    if event.kind != ASSIST_EVENT_KIND {
        return Ok(None);
    }
    let detector = event
        .data
        .get("detector")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ASSIST_OPPORTUNITY_EVENT_MISSING_DETECTOR: CF_EVENTS row {} lacks data.detector",
                    event.event_id
                ),
            )
        })?;
    let source_event_id = event
        .data
        .get("opportunity_id")
        .and_then(Value::as_str)
        .unwrap_or(&event.event_id)
        .to_owned();
    let confidence = event
        .data
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);
    let window = event.data.get("window").unwrap_or(&Value::Null);
    let target_window_hwnd = window.get("hwnd").and_then(Value::as_i64);
    if let Some(hwnd) = target_window_hwnd {
        validate_stored_assist_target_hwnd(
            &format!("CF_EVENTS assist row {}", event.event_id),
            hwnd,
        )?;
    }
    let target_pid = window
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok());
    let process_name = window
        .get("process_name")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let input_origin = event
        .data
        .pointer("/trigger/input_origin")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let key_material = assist_candidate_key_material(event, detector);
    let routine_id = format!("{ASSIST_ROUTINE_PREFIX}{}", sha256_short_hex(&key_material));
    let label = format!("Assist: {}", detector_label(detector));
    let offer = detector_offer(detector, process_name.as_deref());
    let instruction = format!(
        "Inspect the current target for assist opportunity {source_event_id} ({detector}); use the privacy-safe event evidence and fresh observation only. Report a scoped readback and precise blocker; do not claim a correction unless a desired state is known, mutation is attempted, and the postcondition is verified."
    );
    let mitigation = AssistMitigation {
        strategy: AssistMitigationStrategy::InSessionCorrection,
        source_event_id: source_event_id.clone(),
        detector: detector.to_owned(),
        target_window_hwnd,
        target_pid,
        process_name,
        input_origin,
        instruction,
        postcondition: "fresh target readback exists and the in-session assist report records whether a correction was verified, skipped as report-only, or failed".to_owned(),
        evidence: json!({
            "event_id": &event.event_id,
            "opportunity_id": &source_event_id,
            "detector": detector,
            "confidence": confidence,
            "trigger": event.data.get("trigger").cloned().unwrap_or(Value::Null),
            "window": window,
            "counts": event.data.get("counts").cloned().unwrap_or(Value::Null),
            "privacy": event.data.get("privacy").cloned().unwrap_or(Value::Null),
        }),
    };

    Ok(Some(AssistOpportunityCandidate {
        routine_id,
        source_event_id,
        label,
        offer,
        confidence,
        matched_prefix_len: 1,
        total_steps: 1,
        remaining_step_count: 1,
        mitigation,
    }))
}

pub(crate) fn validate_stored_assist_target_hwnd(
    record_ref: &str,
    hwnd: i64,
) -> Result<i64, ErrorData> {
    if crate::m1::window_hwnd_shape_is_canonical(hwnd) {
        return Ok(hwnd);
    }
    tracing::error!(
        code = error_codes::STORAGE_CORRUPTED,
        source_of_truth = record_ref,
        field = "target_window_hwnd",
        actual_value = hwnd,
        accepted_range = "1..=u32::MAX",
        remediation = "remove or repair the corrupt assist event/suggestion row and regenerate it from a live canonical window readback",
        "stored assist target contains a noncanonical HWND"
    );
    Err(mcp_error(
        error_codes::STORAGE_CORRUPTED,
        format!("{record_ref} has noncanonical target_window_hwnd={hwnd}; expected 1..=4294967295"),
    ))
}

fn load_recent_assist_opportunities(
    db: &Arc<Db>,
    now: u64,
    lookback_secs: u64,
) -> Result<(Vec<AssistOpportunityCandidate>, u32), ErrorData> {
    let start_ts_ns = now.saturating_sub(lookback_secs.saturating_mul(1_000_000_000));
    let mut start_key = event_scan_start_key(start_ts_ns);
    let mut scanned: u32 = 0;
    let mut candidates = Vec::new();
    'scan: loop {
        let (rows, more) = db
            .scan_cf_from(cf::CF_EVENTS, &start_key, ASSIST_EVENT_SCAN_ROWS)
            .map_err(storage_error)?;
        if rows.is_empty() {
            break;
        }
        let mut last_key: Option<Vec<u8>> = None;
        for (key, value) in rows {
            if let Some(key_ts_ns) = event_key_ts_ns(&key) {
                if key_ts_ns > now {
                    break 'scan;
                }
            }
            scanned = scanned.saturating_add(1);
            let event: StoredEvent = decode_json(&value).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "ASSIST_EVENT_ROW_DECODE_FAILED in CF_EVENTS at {}: {error}",
                        hex_encode(&key)
                    ),
                )
            })?;
            if event.schema_version != SCHEMA_VERSION {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "ASSIST_EVENT_SCHEMA_VERSION_UNSUPPORTED in CF_EVENTS at {}: expected {}, got {}",
                        hex_encode(&key),
                        SCHEMA_VERSION,
                        event.schema_version
                    ),
                ));
            }
            if event.ts_ns >= start_ts_ns
                && event.ts_ns <= now
                && let Some(candidate) = assist_candidate_from_event(&event)?
            {
                candidates.push(candidate);
            }
            last_key = Some(key);
        }
        if !more {
            break;
        }
        let Some(last_key) = last_key else {
            break;
        };
        start_key = key_after(&last_key);
    }
    Ok((candidates, scanned))
}

/// Loads every suggestion row, newest decode first is irrelevant (callers
/// aggregate). Loud on undecodable rows.
fn load_all_suggestions(db: &Arc<Db>) -> Result<Vec<(Vec<u8>, SuggestionRecord)>, ErrorData> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, SUGGESTION_PREFIX.as_bytes())
        .map_err(storage_error)?;
    let mut out = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let record: SuggestionRecord = decode_json(&value).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "SUGGESTION_ROW_DECODE_FAILED in CF_KV at {}: {error}",
                    String::from_utf8_lossy(&key)
                ),
            )
        })?;
        out.push((key, record));
    }
    Ok(out)
}

fn load_exact_kv_value(
    db: &Arc<Db>,
    key: &[u8],
    context: &'static str,
) -> Result<Option<Vec<u8>>, ErrorData> {
    let rows = db.scan_cf_prefix(cf::CF_KV, key).map_err(storage_error)?;
    let mut exact_values = rows
        .into_iter()
        .filter_map(|(row_key, value)| (row_key == key).then_some(value))
        .collect::<Vec<_>>();
    if exact_values.len() > 1 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_EXACT_KEY_DUPLICATE: {context} key {} appeared more than once in CF_KV",
                String::from_utf8_lossy(key)
            ),
        ));
    }
    Ok(exact_values.pop())
}

fn suggestion_id_index_record(
    record: &SuggestionRecord,
    primary_key: &[u8],
    primary_value: &[u8],
) -> SuggestionIdIndexRecord {
    SuggestionIdIndexRecord {
        record_version: SUGGESTION_ID_INDEX_RECORD_VERSION,
        suggestion_id: record.suggestion_id.clone(),
        routine_id: record.routine_id.clone(),
        created_ts_ns: record.created_ts_ns,
        primary_key_hex: hex_encode(primary_key),
        primary_value_sha256: sha256_hex(primary_value),
    }
}

fn decode_suggestion_id_index(
    expected_suggestion_id: &str,
    index_key: &[u8],
    value: &[u8],
) -> Result<SuggestionIdIndexRecord, ErrorData> {
    let index: SuggestionIdIndexRecord = decode_json(value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_DECODE_FAILED in CF_KV at {}: {error}",
                String::from_utf8_lossy(index_key)
            ),
        )
    })?;
    if index.record_version != SUGGESTION_ID_INDEX_RECORD_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_VERSION_UNSUPPORTED for {expected_suggestion_id}: expected {}, got {}",
                SUGGESTION_ID_INDEX_RECORD_VERSION, index.record_version
            ),
        ));
    }
    if index.suggestion_id != expected_suggestion_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_KEY_MISMATCH: key for {expected_suggestion_id} contains suggestion_id {}",
                index.suggestion_id
            ),
        ));
    }
    Ok(index)
}

fn load_suggestion_id_index(
    db: &Arc<Db>,
    suggestion_id: &str,
) -> Result<Option<SuggestionIdIndexRecord>, ErrorData> {
    let index_key = suggestion_id_index_key(suggestion_id);
    let Some(value) = load_exact_kv_value(db, &index_key, "suggestion id index")? else {
        return Ok(None);
    };
    decode_suggestion_id_index(suggestion_id, &index_key, &value).map(Some)
}

fn validate_suggestion_id_index(
    index: &SuggestionIdIndexRecord,
    record: &SuggestionRecord,
    primary_key: &[u8],
    primary_value: &[u8],
    context: &'static str,
) -> Result<(), ErrorData> {
    let expected_primary_key_hex = hex_encode(primary_key);
    let expected_primary_value_sha256 = sha256_hex(primary_value);
    if index.suggestion_id != record.suggestion_id
        || index.routine_id != record.routine_id
        || index.created_ts_ns != record.created_ts_ns
        || index.primary_key_hex != expected_primary_key_hex
        || index.primary_value_sha256 != expected_primary_value_sha256
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_MISMATCH during {context}: suggestion_id={}, index_routine={}, record_routine={}, index_created={}, record_created={}, index_key={}, expected_key={}, index_hash={}, expected_hash={}",
                record.suggestion_id,
                index.routine_id,
                record.routine_id,
                index.created_ts_ns,
                record.created_ts_ns,
                index.primary_key_hex,
                expected_primary_key_hex,
                index.primary_value_sha256,
                expected_primary_value_sha256
            ),
        ));
    }
    Ok(())
}

fn write_suggestion(db: &Arc<Db>, record: &SuggestionRecord) -> Result<(), ErrorData> {
    validate_suggestion_id("suggestion_write", &record.suggestion_id)?;
    let key = suggestion_key(&record.routine_id, record.created_ts_ns);
    let value = encode_json(record).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to encode suggestion {}: {error}",
                record.suggestion_id
            ),
        )
    })?;
    let index_key = suggestion_id_index_key(&record.suggestion_id);
    let index = suggestion_id_index_record(record, &key, &value);
    if let Some(existing_index) = load_suggestion_id_index(db, &record.suggestion_id)? {
        let expected_primary_key_hex = hex_encode(&key);
        if existing_index.primary_key_hex != expected_primary_key_hex
            || existing_index.routine_id != record.routine_id
            || existing_index.created_ts_ns != record.created_ts_ns
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "SUGGESTION_ID_COLLISION: suggestion_id {} is already indexed to routine_id={}, created_ts_ns={}, primary_key_hex={}; refusing to point it at routine_id={}, created_ts_ns={}, primary_key_hex={}",
                    record.suggestion_id,
                    existing_index.routine_id,
                    existing_index.created_ts_ns,
                    existing_index.primary_key_hex,
                    record.routine_id,
                    record.created_ts_ns,
                    expected_primary_key_hex
                ),
            ));
        }
    }
    let index_value = encode_json(&index).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to encode suggestion_id index for {}: {error}",
                record.suggestion_id
            ),
        )
    })?;
    db.mutate_batch_pressure_bypass(
        cf::CF_KV,
        Vec::<Vec<u8>>::new(),
        [(key.clone(), value), (index_key.clone(), index_value)],
    )
    .map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to persist suggestion {} and suggestion_id index atomically: {error}",
                record.suggestion_id
            ),
        )
    })?;
    let Some(primary_readback_value) = load_exact_kv_value(db, &key, "suggestion primary row")?
    else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_READBACK_MISSING: row for {} vanished immediately after write",
                record.suggestion_id
            ),
        ));
    };
    let primary_readback: SuggestionRecord =
        decode_json(&primary_readback_value).map_err(storage_error)?;
    if &primary_readback != record {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_READBACK_MISMATCH for {}: persisted row != value just written",
                record.suggestion_id
            ),
        ));
    }
    let Some(index_readback_value) =
        load_exact_kv_value(db, &index_key, "suggestion id index readback")?
    else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_READBACK_MISSING: index row for {} vanished immediately after write",
                record.suggestion_id
            ),
        ));
    };
    let index_readback =
        decode_suggestion_id_index(&record.suggestion_id, &index_key, &index_readback_value)?;
    if index_readback != index {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_READBACK_MISMATCH for {}: persisted index != value just written",
                record.suggestion_id
            ),
        ));
    }
    validate_suggestion_id_index(
        &index_readback,
        record,
        &key,
        &primary_readback_value,
        "write readback",
    )
}

fn load_suggestion_primary_from_index(
    db: &Arc<Db>,
    index: &SuggestionIdIndexRecord,
) -> Result<SuggestionRecord, ErrorData> {
    let primary_key = suggestion_key(&index.routine_id, index.created_ts_ns);
    let expected_primary_key_hex = hex_encode(&primary_key);
    if index.primary_key_hex != expected_primary_key_hex {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_PRIMARY_KEY_MISMATCH for {}: index_key={}, expected_key={}",
                index.suggestion_id, index.primary_key_hex, expected_primary_key_hex
            ),
        ));
    }
    let Some(primary_value) = load_exact_kv_value(db, &primary_key, "suggestion primary lookup")?
    else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_DANGLING: suggestion_id {} points at missing primary key {}",
                index.suggestion_id,
                String::from_utf8_lossy(&primary_key)
            ),
        ));
    };
    let actual_hash = sha256_hex(&primary_value);
    if index.primary_value_sha256 != actual_hash {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_HASH_MISMATCH for {}: index_hash={}, actual_hash={}",
                index.suggestion_id, index.primary_value_sha256, actual_hash
            ),
        ));
    }
    let record: SuggestionRecord = decode_json(&primary_value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_PRIMARY_ROW_DECODE_FAILED for {} at {}: {error}",
                index.suggestion_id,
                String::from_utf8_lossy(&primary_key)
            ),
        )
    })?;
    validate_suggestion_id_index(index, &record, &primary_key, &primary_value, "indexed load")?;
    if record.suggestion_id != index.suggestion_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ID_INDEX_RECORD_MISMATCH: index for {} points at primary row for {}",
                index.suggestion_id, record.suggestion_id
            ),
        ));
    }
    Ok(record)
}

pub fn load_suggestion_by_id(
    db: &Arc<Db>,
    suggestion_id: &str,
) -> Result<Option<SuggestionRecord>, ErrorData> {
    validate_suggestion_id("suggestion", suggestion_id)?;
    let Some(index) = load_suggestion_id_index(db, suggestion_id)? else {
        return Ok(None);
    };
    load_suggestion_primary_from_index(db, &index).map(Some)
}

pub fn accept_suggestion_for_execution(
    db: &Arc<Db>,
    suggestion_id: &str,
    now_ns: u64,
    plan_ref: &str,
    execution_id: &str,
    dry_run: bool,
) -> Result<SuggestionRecord, ErrorData> {
    validate_suggestion_id("suggestion_accept", suggestion_id)?;
    let Some(mut record) = load_suggestion_by_id(db, suggestion_id)? else {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!("SUGGESTION_NOT_FOUND: suggestion_id {suggestion_id} is not in CF_KV"),
        ));
    };
    if record.status != SuggestionStatus::Live {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "SUGGESTION_NOT_LIVE: suggestion_id {suggestion_id} has status {:?}; only live suggestions can be accepted for execution",
                record.status
            ),
        ));
    }
    record.status = SuggestionStatus::Accepted;
    record.proposed_plan_ref = Some(plan_ref.to_owned());
    record.resolved_ts_ns = Some(now_ns);
    record.resolution_note = Some(if dry_run {
        format!("dry-run accepted by suggestion_accept; execution_id={execution_id}")
    } else {
        format!("accepted by suggestion_accept; execution_id={execution_id}")
    });
    if !dry_run {
        if !db.pressure_permits_write(cf::CF_KV) {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "suggestion_accept refused under disk pressure: pressure_level={:?}",
                    db.pressure_level()
                ),
            ));
        }
        write_suggestion(db, &record)?;
    }
    Ok(record)
}

pub fn record_suggestion_execution_feedback(
    db: &Arc<Db>,
    routine_id: &str,
    outcome: RoutineFeedbackOutcome,
    now_ns: u64,
    note: &str,
) -> Result<(), ErrorData> {
    record_terminal_feedback(db, routine_id, outcome, now_ns, note)
}

fn validate_suggestion_id(tool: &str, suggestion_id: &str) -> Result<(), ErrorData> {
    let trimmed = suggestion_id.trim();
    if trimmed.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} suggestion_id must not be empty"),
        ));
    }
    if trimmed != suggestion_id {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} suggestion_id must not contain leading or trailing whitespace"),
        ));
    }
    if suggestion_id.chars().count() > 512 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} suggestion_id must be at most 512 Unicode scalar values"),
        ));
    }
    if suggestion_id.contains('\0') || suggestion_id.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} suggestion_id must not contain control characters"),
        ));
    }
    Ok(())
}

fn record_terminal_feedback(
    db: &Arc<Db>,
    routine_id: &str,
    outcome: RoutineFeedbackOutcome,
    now_ns: u64,
    note: &str,
) -> Result<(), ErrorData> {
    let params = RoutineFeedbackParams {
        routine_id: routine_id.to_owned(),
        outcome,
        note: Some(note.to_owned()),
        now_ts_ns: Some(now_ns),
    };
    record_routine_feedback(db, &params, SUGGESTION_ACTOR).map(|_| ())
}

/// One engine pass: expire timed-out suggestions, abandon ones whose routine
/// left the live set, then create suggestions for fresh candidates that pass
/// every gate. Each terminal transition records #856 feedback.
pub fn suggestion_tick(
    db: &Arc<Db>,
    params: &SuggestionTickParams,
) -> Result<SuggestionTickResponse, ErrorData> {
    let _ = ENGINE_VERSION_DEFAULTS;
    let now = params.now_ts_ns.unwrap_or_else(now_ts_ns);
    let config = SuggestionConfig::from_env();

    if !db.pressure_permits_write(cf::CF_KV) && !params.dry_run {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "suggestion_tick refused under disk pressure: pressure_level={:?}",
                db.pressure_level()
            ),
        ));
    }

    // Current live intents (the detection signal). Floor at the engine
    // threshold so the candidate set is exactly the surfacing-eligible ones.
    let intent = current_intents(
        db,
        &IntentCurrentParams {
            now_ts_ns: Some(now),
            lookback_hours: params.lookback_hours,
            min_confidence: Some(0.0),
            max_candidates: Some(50),
            include_agent_activity: false,
        },
    )?;
    let candidate_routines: BTreeSet<String> = intent
        .candidates
        .iter()
        .map(|c| c.routine_id.clone())
        .collect();
    let (assist_candidates, assist_events_scanned) = if params.include_assist_opportunities {
        load_recent_assist_opportunities(db, now, assist_lookback_secs(params))?
    } else {
        (Vec::new(), 0)
    };

    let mut suggestions = load_all_suggestions(db)?;
    let mut expired = Vec::new();
    let mut abandoned = Vec::new();

    // --- Expire / abandon pass over live suggestions ---
    for (_key, record) in &mut suggestions {
        if record.status != SuggestionStatus::Live {
            continue;
        }
        if now >= record.expiry_ts_ns {
            record.status = SuggestionStatus::Expired;
            record.resolved_ts_ns = Some(now);
            record.resolution_note = Some("timed out unanswered".to_owned());
            if !params.dry_run {
                write_suggestion(db, record)?;
                if record.source == SuggestionSource::RoutineIntent {
                    record_terminal_feedback(
                        db,
                        &record.routine_id,
                        RoutineFeedbackOutcome::IgnoredTimeout,
                        now,
                        "suggestion expired (timeout)",
                    )?;
                }
            }
            expired.push(record.suggestion_id.clone());
        } else if record.source == SuggestionSource::RoutineIntent
            && !candidate_routines.contains(&record.routine_id)
        {
            record.status = SuggestionStatus::Abandoned;
            record.resolved_ts_ns = Some(now);
            record.resolution_note = Some("routine left the live intent set".to_owned());
            if !params.dry_run {
                write_suggestion(db, record)?;
                record_terminal_feedback(
                    db,
                    &record.routine_id,
                    RoutineFeedbackOutcome::Abandoned,
                    now,
                    "suggestion abandoned (intent dropped)",
                )?;
            }
            abandoned.push(record.suggestion_id.clone());
        }
    }

    // --- Aggregates AFTER expiry/abandon (so a just-expired routine is no
    // longer "live" for dedup, and caps count history honestly). Mutated as the
    // creation pass adds suggestions, so a second candidate respects the caps. ---
    let mut live = build_aggregates(&suggestions);

    // --- Creation pass ---
    let mut created = Vec::new();
    let mut decisions = Vec::new();
    let now_minute = local_minute_of_day(now);
    for candidate in &intent.candidates {
        let suppressed = is_routine_suppressed(db, &candidate.routine_id, now)?;
        let outcome = gate_decision(
            &candidate.routine_id,
            candidate.confidence,
            candidate.lifecycle,
            suppressed,
            now,
            now_minute,
            &live,
            &config,
        );
        let mut created_id = None;
        if outcome == GateOutcome::Surface && !params.dry_run {
            let record = build_suggestion(candidate, now, &config);
            write_suggestion(db, &record)?;
            // Update in-tick aggregates so a second candidate respects the caps.
            live.live_routines.insert(record.routine_id.clone());
            live.last_created_by_routine
                .insert(record.routine_id.clone(), record.created_ts_ns);
            live.created_ts.push(record.created_ts_ns);
            created.push(record.suggestion_id.clone());
            created_id = Some(record.suggestion_id.clone());
        } else if outcome == GateOutcome::Surface && params.dry_run {
            created_id = Some(format!("(dry-run){}", candidate.routine_id));
        }
        decisions.push(GateDecisionRow {
            routine_id: candidate.routine_id.clone(),
            source: SuggestionSource::RoutineIntent,
            confidence: candidate.confidence,
            outcome,
            suggestion_id: created_id,
            source_event_id: None,
        });
    }

    for candidate in &assist_candidates {
        let outcome = gate_decision(
            &candidate.routine_id,
            candidate.confidence,
            RoutineLifecycle::Confirmed,
            false,
            now,
            now_minute,
            &live,
            &config,
        );
        let mut created_id = None;
        if outcome == GateOutcome::Surface && !params.dry_run {
            let record = build_assist_suggestion(candidate, now, &config);
            write_suggestion(db, &record)?;
            live.live_routines.insert(record.routine_id.clone());
            live.last_created_by_routine
                .insert(record.routine_id.clone(), record.created_ts_ns);
            live.created_ts.push(record.created_ts_ns);
            created.push(record.suggestion_id.clone());
            created_id = Some(record.suggestion_id.clone());
        } else if outcome == GateOutcome::Surface && params.dry_run {
            created_id = Some(format!("(dry-run){}", candidate.routine_id));
        }
        decisions.push(GateDecisionRow {
            routine_id: candidate.routine_id.clone(),
            source: SuggestionSource::AssistOpportunity,
            confidence: candidate.confidence,
            outcome,
            suggestion_id: created_id,
            source_event_id: Some(candidate.source_event_id.clone()),
        });
    }

    Ok(SuggestionTickResponse {
        now_ts_ns: now,
        dry_run: params.dry_run,
        candidates_evaluated: u32::try_from(intent.candidates.len()).unwrap_or(u32::MAX),
        created,
        expired,
        abandoned,
        assist_events_scanned,
        assist_events_evaluated: u32::try_from(assist_candidates.len()).unwrap_or(u32::MAX),
        decisions,
        config: config.into(),
    })
}

fn build_aggregates(suggestions: &[(Vec<u8>, SuggestionRecord)]) -> SuggestionAggregates {
    let mut agg = SuggestionAggregates::default();
    for (_key, record) in suggestions {
        if record.status == SuggestionStatus::Live {
            agg.live_routines.insert(record.routine_id.clone());
        }
        let entry = agg
            .last_created_by_routine
            .entry(record.routine_id.clone())
            .or_insert(0);
        *entry = (*entry).max(record.created_ts_ns);
        agg.created_ts.push(record.created_ts_ns);
    }
    agg
}

fn build_suggestion(
    candidate: &IntentCandidate,
    now: u64,
    config: &SuggestionConfig,
) -> SuggestionRecord {
    SuggestionRecord {
        record_version: SUGGESTION_RECORD_VERSION,
        suggestion_id: format!("sg1-{}-{now:020}", candidate.routine_id),
        routine_id: candidate.routine_id.clone(),
        source: SuggestionSource::RoutineIntent,
        source_event_id: None,
        label: candidate.label.clone(),
        offer: candidate
            .label
            .as_ref()
            .map(|label| format!("Continue {label}? I can run the remaining setup steps.")),
        mitigation: None,
        created_ts_ns: now,
        expiry_ts_ns: now.saturating_add(config.expiry_secs.saturating_mul(1_000_000_000)),
        status: SuggestionStatus::Live,
        confidence: candidate.confidence,
        matched_prefix_len: u32::try_from(candidate.matched_prefix_len).unwrap_or(u32::MAX),
        total_steps: u32::try_from(candidate.total_steps).unwrap_or(u32::MAX),
        remaining_step_count: u32::try_from(candidate.remaining_steps.len()).unwrap_or(u32::MAX),
        proposed_plan_ref: None,
        resolved_ts_ns: None,
        resolution_note: None,
    }
}

fn build_assist_suggestion(
    candidate: &AssistOpportunityCandidate,
    now: u64,
    config: &SuggestionConfig,
) -> SuggestionRecord {
    SuggestionRecord {
        record_version: SUGGESTION_RECORD_VERSION,
        suggestion_id: format!("sg1-{}-{now:020}", candidate.routine_id),
        routine_id: candidate.routine_id.clone(),
        source: SuggestionSource::AssistOpportunity,
        source_event_id: Some(candidate.source_event_id.clone()),
        label: Some(candidate.label.clone()),
        offer: Some(candidate.offer.clone()),
        mitigation: Some(candidate.mitigation.clone()),
        created_ts_ns: now,
        expiry_ts_ns: now.saturating_add(config.expiry_secs.saturating_mul(1_000_000_000)),
        status: SuggestionStatus::Live,
        confidence: candidate.confidence,
        matched_prefix_len: candidate.matched_prefix_len,
        total_steps: candidate.total_steps,
        remaining_step_count: candidate.remaining_step_count,
        proposed_plan_ref: None,
        resolved_ts_ns: None,
        resolution_note: None,
    }
}

pub fn assist_plan_for_suggestion(
    record: &SuggestionRecord,
    compiled_ts_ns: u64,
) -> Result<PlanDocument, ErrorData> {
    if record.source != SuggestionSource::AssistOpportunity {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "ASSIST_PLAN_SOURCE_MISMATCH: suggestion {} has source {:?}",
                record.suggestion_id, record.source
            ),
        ));
    }
    let Some(mitigation) = &record.mitigation else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ASSIST_SUGGESTION_MISSING_MITIGATION: suggestion {} has no mitigation payload",
                record.suggestion_id
            ),
        ));
    };
    let source_app = mitigation
        .process_name
        .clone()
        .unwrap_or_else(|| "assist-opportunity".to_owned());
    let action = format!(
        "in-session assist report for {} from {}",
        mitigation.detector, mitigation.source_event_id
    );
    Ok(PlanDocument {
        record_version: ASSIST_PLAN_RECORD_VERSION,
        routine_id: record.routine_id.clone(),
        compiled_ts_ns,
        granularity: RoutineGranularity::App,
        schedule_label: "assist opportunity".to_owned(),
        total_steps: 1,
        deterministic_steps: 0,
        agent_task_steps: 1,
        fully_deterministic: false,
        steps: vec![PlanStep {
            index: 0,
            source_app,
            source_document: Some(mitigation.detector.clone()),
            backend: PlanBackend::AgentTask,
            deterministic: false,
            action,
            postcondition: Postcondition::AgentReported,
            agent_task_reason: Some(mitigation.instruction.clone()),
        }],
    })
}

fn is_routine_suppressed(db: &Arc<Db>, routine_id: &str, now: u64) -> Result<bool, ErrorData> {
    Ok(match load_state_row(db, routine_id)? {
        Some(state) => feedback_suppressed(&state, now),
        None => false,
    })
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionListParams {
    /// Filter by status (live/accepted/declined/expired/abandoned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SuggestionStatus>,
    /// Filter to one routine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routine_id: Option<String>,
    /// Max rows (default 100, max 1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionListResponse {
    pub suggestions: Vec<SuggestionRecord>,
    pub total_rows: u64,
    pub returned: u64,
}

pub fn required_permissions_list(_params: &SuggestionListParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionAcceptParams {
    pub suggestion_id: String,
    /// Compute the plan and per-step routing report without mutating storage or
    /// launching/opening anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Browser HWND used by `cdp_open_tab` steps. If omitted, the executor may
    /// use the MCP session's existing CDP/window target; if neither exists, the
    /// step is refused with evidence instead of using the human foreground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub browser_window_hwnd: Option<i64>,
    /// Timeout applied to launch-window/postcondition waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionAcceptResponse {
    pub suggestion: SuggestionRecord,
    pub plan: PlanDocument,
    pub execution: PlanExecutionRecord,
}

pub fn required_permissions_accept(_params: &SuggestionAcceptParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

pub fn list_suggestions(
    db: &Arc<Db>,
    params: &SuggestionListParams,
) -> Result<SuggestionListResponse, ErrorData> {
    let limit = params.limit.unwrap_or(100).min(1000) as usize;
    let all = load_all_suggestions(db)?;
    let total_rows = all.len() as u64;
    let mut filtered: Vec<SuggestionRecord> = all
        .into_iter()
        .map(|(_key, record)| record)
        .filter(|record| params.status.is_none_or(|status| record.status == status))
        .filter(|record| {
            params
                .routine_id
                .as_ref()
                .is_none_or(|routine_id| &record.routine_id == routine_id)
        })
        .collect();
    // Newest first.
    filtered.sort_by_key(|record| std::cmp::Reverse(record.created_ts_ns));
    filtered.truncate(limit);
    let returned = filtered.len() as u64;
    Ok(SuggestionListResponse {
        suggestions: filtered,
        total_rows,
        returned,
    })
}
